// JHS sidecar service — the Node half of wallermax-server's
// `[templates] backend = "sidecar" | "auto"`.
//
// A tiny loopback-only HTTP service that renders `.jhs` templates with
// the ORIGINAL node-jhs2 engine (vendored in ./engine.js) inside
// worker threads:
//
//   POST /render         { "path": "<absolute .jhs path>", "data": {...} }
//   POST /render-string  { "source": "<?jhs ... ?>", "data": {...} }
//                         → { ok, html, console, redirect } |
//                           { ok: false, error: { kind, message, path } }
//   GET  /health         → { ok, pid, port, workers, renders, uptimeMs }
//   GET  /selftest       → { ok, checks: [{ name, ok, detail }] }
//
// Hardening:
//
//   * binds 127.0.0.1 only, port chosen by the OS unless
//     JHS_SIDECAR_PORT is set;
//   * every request must carry the `x-sidecar-token` header matching
//     JHS_SIDECAR_TOKEN (compared timing-safely); the token is minted
//     by the Rust parent for every spawn;
//   * a per-render wall-clock budget (JHS_SIDECAR_RENDER_BUDGET_MS,
//     default 5000) hard-kills runaway renders by TERMINATING the
//     worker thread — the fix for node-jhs2's ineffective vm timeout —
//     and respawns it;
//   * request bodies are capped at 16 MiB;
//   * stdout carries exactly ONE line — the READY handshake — and all
//     logging goes to stderr, so the Rust parent can parse it blindly;
//   * the service exits when its stdin pipe closes (parent death) or on
//     SIGTERM/SIGINT, so no orphaned sidecar survives the server.
//
// Configuration (set by the Rust parent via environment):
//
//   JHS_SIDECAR_TOKEN           shared secret (required)
//   JHS_SIDECAR_PORT            listen port (default 0 = OS-assigned)
//   JHS_SIDECAR_VIEWS_DIR       views directory (absolute)
//   JHS_SIDECAR_MODULES_DIR     modules directory (absolute)
//   JHS_SIDECAR_FORBIDDEN       JSON array: [templates] forbidden_modules
//   JHS_SIDECAR_AUTO_ESCAPE     "true" | "false" (default true)
//   JHS_SIDECAR_WORKERS         render worker count (default 2)
//   JHS_SIDECAR_RENDER_BUDGET_MS  per-render hard-kill budget (default 5000)

import http from 'node:http';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { Worker } from 'node:worker_threads';

const ENV = process.env;

function fail(message) {
  process.stderr.write(`[jhs-sidecar] fatal: ${message}\n`);
  process.exit(1);
}

// ── Configuration ─────────────────────────────────────────────────────
const TOKEN = ENV.JHS_SIDECAR_TOKEN;
if (!TOKEN || TOKEN.length < 16) {
  fail('JHS_SIDECAR_TOKEN (>= 16 chars) is required — start via wallermax-server');
}
const TOKEN_HASH = crypto.createHash('sha256').update(TOKEN).digest();

const PORT = Number.parseInt(ENV.JHS_SIDECAR_PORT || '0', 10) || 0;
const VIEWS_DIR = path.resolve(
  ENV.JHS_SIDECAR_VIEWS_DIR || path.join(process.cwd(), 'views'),
);
const MODULES_DIR = path.resolve(
  ENV.JHS_SIDECAR_MODULES_DIR || path.join(process.cwd(), 'modules'),
);
// The port's DEFAULT_FORBIDDEN_MODULES — used when the parent does not
// pass JHS_SIDECAR_FORBIDDEN (manual runs). The Rust parent always sends
// the configured [templates] forbidden_modules list.
const DEFAULT_FORBIDDEN = [
  'child_process', 'cluster', 'dgram', 'dns', 'fs', 'http', 'https',
  'inspector', 'jhs', 'mv', 'net', 'os', 'process', 'repl', 'tls',
  'tty', 'v8', 'vm', 'worker_threads',
];

let FORBIDDEN = DEFAULT_FORBIDDEN;
try {
  const parsed = JSON.parse(ENV.JHS_SIDECAR_FORBIDDEN || 'null');
  if (Array.isArray(parsed) && parsed.every((name) => typeof name === 'string')) {
    FORBIDDEN = parsed;
  }
} catch {
  /* keep the default list */
}
const AUTO_ESCAPE = ENV.JHS_SIDECAR_AUTO_ESCAPE !== 'false';
// While the parent disables require() entirely ([templates]
// require_enabled = false), the sandbox must not expose a callable
// require — the worker undefines the engine's require_filter so
// `typeof require` reads "undefined" exactly like the boa sandbox.
const REQUIRE_ENABLED = ENV.JHS_SIDECAR_REQUIRE_ENABLED !== 'false';
const WORKERS = Math.min(
  16,
  Math.max(1, Number.parseInt(ENV.JHS_SIDECAR_WORKERS || '2', 10) || 2),
);
const RENDER_BUDGET_MS = Math.max(
  100,
  Number.parseInt(ENV.JHS_SIDECAR_RENDER_BUDGET_MS || '5000', 10) || 5000,
);

const BODY_LIMIT = 16 * 1024 * 1024;
const STARTED_AT = Date.now();
let rendersServed = 0;
let jobIdCounter = 0;

function log(level, message) {
  process.stderr.write(`[jhs-sidecar] ${level}: ${message}\n`);
}

// ── Worker pool ───────────────────────────────────────────────────────
// Fixed worker slots. Each slot runs one render at a time; jobs queue
// FIFO when every slot is busy. A render that exceeds its budget gets
// its worker TERMINATED (hard-kill: vm code cannot be interrupted any
// other way) and a fresh worker takes the slot.

const workerUrl = new URL('./render-worker.mjs', import.meta.url);
const slots = [];
const queue = [];
const pending = new Map(); // jobId -> { slot, timer, resolve, label }

function spawnInto(slot) {
  slot.busy = false;
  slot.alive = false;
  slot.worker = new Worker(workerUrl, { stdout: true, stderr: true });

  slot.worker.on('message', (message) => {
    if (message.event === 'ready') {
      slot.alive = true;
      drainQueue();
      return;
    }
    finishJob(message);
  });

  slot.worker.on('error', (error) => {
    log('error', `worker thread error: ${error.message}`);
    replaceSlot(slot);
  });

  slot.worker.on('exit', () => {
    if (slot.alive) {
      log('warn', 'worker exited unexpectedly; respawning');
      replaceSlot(slot);
    }
  });

  // Worker diagnostics (console fallback) stay out of our stdout.
  slot.worker.stdout.on('data', () => {});
  slot.worker.stderr.on('data', (chunk) => {
    process.stderr.write(chunk);
  });

  slot.worker.postMessage({
    op: 'init',
    viewsDir: VIEWS_DIR,
    modulesDir: MODULES_DIR,
    forbidden: FORBIDDEN,
    autoEscape: AUTO_ESCAPE,
    requireEnabled: REQUIRE_ENABLED,
  });
}

function replaceSlot(slot) {
  for (const [id, entry] of pending) {
    if (entry.slot === slot) {
      clearTimeout(entry.timer);
      pending.delete(id);
      entry.resolve({
        ok: false,
        error: {
          kind: 'worker',
          message:
            `Template execution error (${entry.label}): the render worker died ` +
            'before answering; it has been respawned',
          path: entry.label,
        },
      });
    }
  }
  try {
    slot.alive = false;
    slot.worker.terminate().catch(() => {});
  } catch {
    /* already gone */
  }
  spawnInto(slot);
}

function drainQueue() {
  while (queue.length > 0) {
    const slot = slots.find((candidate) => candidate.alive && !candidate.busy);
    if (!slot) return;
    const job = queue.shift();
    dispatch(slot, job);
  }
}

function dispatch(slot, job) {
  slot.busy = true;
  const budget = job.budgetMs ?? RENDER_BUDGET_MS;
  const entry = {
    slot,
    label: job.label,
    resolve: job.resolve,
  };
  entry.timer = setTimeout(() => hardKill(job.jobId, budget), budget);
  pending.set(job.jobId, entry);
  slot.worker.postMessage({
    jobId: job.jobId,
    op: job.op,
    template: job.template,
    source: job.source,
    data: job.data,
  });
}

function hardKill(jobId, budget) {
  const entry = pending.get(jobId);
  if (!entry) return;
  pending.delete(jobId);
  const slot = entry.slot;
  const label = entry.label;
  // Hard-kill: terminate the runaway worker, respawn a fresh one.
  slot.alive = false;
  slot.worker
    .terminate()
    .then(() => spawnInto(slot))
    .catch(() => spawnInto(slot));
  entry.resolve({
    ok: false,
    error: {
      kind: 'timeout',
      message:
        `Template execution error (${label}): the render exceeded the ` +
        `${budget} ms sidecar budget and its worker was terminated`,
      path: label,
    },
  });
}

function finishJob(message) {
  const entry = pending.get(message.jobId);
  if (!entry) return; // late answer for an already-terminated job
  pending.delete(message.jobId);
  clearTimeout(entry.timer);
  entry.slot.busy = false;
  entry.resolve({
    ok: message.ok,
    html: message.html,
    console: message.console,
    redirect: message.redirect,
    error: message.error,
  });
  drainQueue();
}

/** Submits a render job; resolves with the worker's result. */
function submit(op, { template, source, data, label, budgetMs }) {
  return new Promise((resolve) => {
    const jobId = ++jobIdCounter;
    const job = { jobId, op, template, source, data, label, budgetMs, resolve };
    queue.push(job);
    drainQueue();
  });
}

for (let i = 0; i < WORKERS; i += 1) {
  const slot = {};
  slots.push(slot);
  spawnInto(slot);
}

// ── Selftest battery ──────────────────────────────────────────────────
// Runs through the real dispatch path (worker pool + hard-kill) so it
// validates exactly what production renders exercise. The Rust parent
// calls GET /selftest at startup and gates the backend on `ok`.

async function runSelftest() {
  const checks = [];
  const check = (name, ok, detail = '') => {
    checks.push({ name, ok, detail: String(detail) });
  };

  const renderString = (source, data = {}, budgetMs) =>
    submit('renderString', { source, data, label: '<selftest>', budgetMs });

  // 1. auto-escape.
  let result = await renderString('<?jhs echo("<b>") ?>');
  check(
    'auto-escape',
    result.ok && result.html === '&lt;b&gt;',
    `html=${JSON.stringify(result.html)} error=${JSON.stringify(result.error?.message)}`,
  );

  // 2. raw() sentinel through <?= ?>.
  result = await renderString('<?= raw("<b>") ?>');
  check('raw-sentinel', result.ok && result.html === '<b>', JSON.stringify(result.html));

  // 3. echo(raw()) — the 2.1.0 parity fix.
  result = await renderString('<?jhs echo(raw("<b>")) ?>');
  check('echo-raw', result.ok && result.html === '<b>', JSON.stringify(result.html));

  // 4. console capture (level + message).
  result = await renderString(
    '<?jhs console.log("sidecar-line"); console.warn("sidecar-warn") ?>',
  );
  check(
    'console-capture',
    result.ok &&
      result.console.length === 2 &&
      result.console[0].level === 'log' &&
      result.console[0].message === 'sidecar-line' &&
      result.console[1].level === 'warn',
    JSON.stringify(result.console),
  );

  // 5. data injection stays escaped.
  result = await renderString('<?= user ?>', { user: '<u>' });
  check(
    'data-escape',
    result.ok && result.html === '&lt;u&gt;',
    JSON.stringify(result.html),
  );

  // 6. res.redirect() records a local intent.
  result = await renderString('<?jhs res.redirect("/login") ?>');
  check(
    'res-redirect',
    result.ok &&
      result.redirect &&
      result.redirect.location === '/login' &&
      result.redirect.status === 302,
    JSON.stringify(result.redirect),
  );

  // 7. res.redirect() rejects external targets (open-redirect guard).
  result = await renderString('<?jhs res.redirect("https://evil.example/x") ?>');
  check(
    'res-redirect-local-only',
    !result.ok && /open-redirect protection/.test(result.error.message),
    JSON.stringify(result.error?.message),
  );

  // 8. real Node built-in behind require().
  result = await renderString(
    '<?jhs const u = require("url"); echo(typeof u.parse) ?>',
  );
  check(
    'require-builtin',
    result.ok && result.html === 'function',
    JSON.stringify(result.error?.message || result.html),
  );

  // 9. forbidden module banner (port wording).
  result = await renderString('<?jhs require("fs") ?>');
  check(
    'require-forbidden',
    !result.ok &&
      /is forbidden: the module 'fs' is listed/.test(result.error.message),
    JSON.stringify(result.error?.message),
  );

  // 10. missing local module message (port wording).
  result = await renderString(
    '<?jhs require("wallermax-selftest-missing") ?>',
  );
  check(
    'require-local-miss',
    !result.ok && /Cannot find module 'wallermax-selftest-missing'/.test(result.error.message),
    JSON.stringify(result.error?.message),
  );

  // 11. file render + mtime hot reload (temp fixture, never touches the
  // configured views tree; utimesSync guarantees an mtime change).
  try {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'jhs-sidecar-selftest-'));
    const file = path.join(dir, 'main.jhs');
    fs.writeFileSync(file, 'selftest-v1<?= 6 * 7 ?>');
    result = await submit('render', {
      template: file,
      data: {},
      label: file,
    });
    const first = result.ok && result.html === 'selftest-v142';
    fs.writeFileSync(file, 'selftest-v2<?= 6 * 7 ?>');
    const stamp = new Date(Date.now() + 5000);
    fs.utimesSync(file, stamp, stamp);
    result = await submit('render', { template: file, data: {}, label: file });
    const second = result.ok && result.html === 'selftest-v242';
    check(
      'file-render-mtime-hot-reload',
      first && second,
      `first=${first} second=${second} html=${JSON.stringify(result.html)}`,
    );
    fs.rmSync(dir, { recursive: true, force: true });
  } catch (error) {
    check('file-render-mtime-hot-reload', false, error.message);
  }

  // 12. runaway render hard-kill (short budget on purpose).
  result = await renderString('<?jhs while (true) { } ?>', {}, 300);
  check(
    'hard-kill-bounded',
    !result.ok && /exceeded the 300 ms sidecar budget/.test(result.error.message),
    JSON.stringify(result.error?.message),
  );

  // 13. the killed worker respawns and serves again.
  result = await renderString('respawn-ok');
  check(
    'worker-respawn',
    result.ok && result.html === 'respawn-ok',
    JSON.stringify(result.error?.message || result.html),
  );

  return { ok: checks.every((entry) => entry.ok), checks };
}

// ── HTTP service ──────────────────────────────────────────────────────

function sendJson(response, status, payload) {
  const body = JSON.stringify(payload);
  response.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(body),
    connection: 'close',
  });
  response.end(body);
}

function tokenMatches(header) {
  if (typeof header !== 'string') return false;
  const givenHash = crypto.createHash('sha256').update(header).digest();
  return crypto.timingSafeEqual(TOKEN_HASH, givenHash);
}

function readBody(request) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    request.on('data', (chunk) => {
      size += chunk.length;
      if (size > BODY_LIMIT) {
        reject(Object.assign(new Error('request body too large'), { statusCode: 413 }));
        request.destroy();
        return;
      }
      chunks.push(chunk);
    });
    request.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    request.on('error', reject);
  });
}

const server = http.createServer(async (request, response) => {
  try {
    if (!tokenMatches(request.headers['x-sidecar-token'])) {
      sendJson(response, 403, { ok: false, error: 'invalid sidecar token' });
      return;
    }

    if (request.method === 'GET' && request.url === '/health') {
      sendJson(response, 200, {
        ok: true,
        pid: process.pid,
        port: server.address().port,
        workers: slots.filter((slot) => slot.alive).length,
        renders: rendersServed,
        uptimeMs: Date.now() - STARTED_AT,
        engine: 'node-jhs2 2.1.0',
      });
      return;
    }

    if (request.method === 'GET' && request.url === '/selftest') {
      const report = await runSelftest();
      sendJson(response, 200, report);
      return;
    }

    if (request.method === 'POST' && (request.url === '/render' || request.url === '/render-string')) {
      const body = await readBody(request);
      let parsed;
      try {
        parsed = JSON.parse(body);
      } catch {
        sendJson(response, 400, {
          ok: false,
          error: { kind: 'protocol', message: 'request body is not valid JSON' },
        });
        return;
      }

      const isFile = request.url === '/render';
      const template = isFile ? parsed.path : undefined;
      const source = isFile ? undefined : parsed.source;
      const data = parsed.data && typeof parsed.data === 'object' ? parsed.data : {};

      if (isFile && (typeof template !== 'string' || template.length === 0)) {
        sendJson(response, 400, {
          ok: false,
          error: { kind: 'protocol', message: 'render expects a non-empty "path" string' },
        });
        return;
      }
      if (!isFile && typeof source !== 'string') {
        sendJson(response, 400, {
          ok: false,
          error: { kind: 'protocol', message: 'render-string expects a "source" string' },
        });
        return;
      }

      const label = isFile ? template : '<string>';
      const budgetMs =
        typeof parsed.budgetMs === 'number' && parsed.budgetMs >= 100
          ? parsed.budgetMs
          : undefined;

      const result = await submit(isFile ? 'render' : 'renderString', {
        template,
        source,
        data,
        label,
        budgetMs,
      });
      rendersServed += 1;
      sendJson(response, 200, result);
      return;
    }

    sendJson(response, 404, { ok: false, error: `no route for ${request.method} ${request.url}` });
  } catch (error) {
    const status = error.statusCode || 500;
    sendJson(response, status, {
      ok: false,
      error: { kind: 'protocol', message: error.message },
    });
  }
});

// ── Lifecycle ─────────────────────────────────────────────────────────

let shuttingDown = false;
function shutdown(signal) {
  if (shuttingDown) return;
  shuttingDown = true;
  log('info', `shutting down (${signal})`);
  for (const slot of slots) {
    try {
      slot.alive = false;
      slot.worker.terminate().catch(() => {});
    } catch {
      /* already gone */
    }
  }
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 500).unref();
}

process.on('SIGTERM', () => shutdown('SIGTERM'));
process.on('SIGINT', () => shutdown('SIGINT'));

// Parent-death watch: the Rust parent pipes stdin; when it dies the
// pipe closes and the sidecar exits instead of orphaning. TTY stdin
// (manual runs) never fires 'end', so the watch is a no-op there.
if (!process.stdin.isTTY) {
  process.stdin.resume();
  process.stdin.on('end', () => shutdown('stdin-closed'));
  process.stdin.on('error', () => shutdown('stdin-error'));
}

server.listen(PORT, '127.0.0.1', () => {
  const { port } = server.address();
  // The ONE stdout line the Rust parent parses; everything else logs to
  // stderr so the handshake stays unambiguous.
  process.stdout.write(
    `${JSON.stringify({
      event: 'ready',
      port,
      pid: process.pid,
      workers: WORKERS,
      engine: 'node-jhs2 2.1.0',
    })}\n`,
  );
  log(
    'info',
    `jhs-sidecar listening on 127.0.0.1:${port} ` +
      `(workers=${WORKERS}, budget=${RENDER_BUDGET_MS}ms, ` +
      `views=${VIEWS_DIR}, modules=${MODULES_DIR}, forbidden=${FORBIDDEN.length})`,
  );
});
