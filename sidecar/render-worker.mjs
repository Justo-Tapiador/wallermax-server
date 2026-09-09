// JHS sidecar render worker.
//
// One worker = one template render at a time. The worker owns:
//
//   * the vendored node-jhs2 engine (sidecar/engine.js) configured from
//     the parent's `init` message;
//   * an mtime-aware cache of compiled template programs keyed by file
//     path — mirroring wallermax-server's JhsEngine cache (main file
//     mtime + every embedded partial's mtime must be unchanged for a
//     hit), so template edits hot-reload without a restart;
//   * the compile-time `include()` resolution with the port's
//     semantics: only a *standalone* `<?jhs include("name") ?>` block
//     is resolved (embedded verbatim, sharing the render data); depth
//     and total-size budgets bound the resolution;
//   * the `require()` wrapper: the [templates] forbidden_modules banner
//     (checked first, with the port's message wording), real Node
//     built-ins for everything else, and local CommonJS modules from
//     the modules directory evaluated through `new Function` with the
//     banner enforced recursively and module instances per render —
//     the same per-render isolation wallermax-server's sandbox has;
//   * a patched `console` whose output is captured per render and
//     returned with the render result instead of leaking to stdout;
//   * the Express-shaped `res` shim (`res.redirect()`) injected into
//     the render data with the port's local-path-only validation.
//
// The parent (jhs-sidecar.mjs) enforces the wall-clock render budget:
// if a render runs longer, the whole worker is terminated and respawned
// — the hard-kill node-jhs2's ineffective `vm` timeout never had.

import { parentPort } from 'node:worker_threads';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import JSTemplateEngine from './engine.js';

// ── Budgets: identical to the port (engine.rs) ────────────────────────
const INCLUDE_MAX_DEPTH = 8;
const INCLUDE_MAX_BYTES = 1_048_576;
const INCLUDE_MAX_NAME_LEN = 64;

// Data keys that may not shadow sandbox helpers — the port's
// PROTECTED_GLOBALS. Incoming render data carrying these keys is
// dropped: in the port the helpers are defined after the data spread
// and always win, so a data key named `res`/`console`/`require`… never
// reaches the sandbox either way.
const PROTECTED_GLOBALS = new Set([
  '__escape',
  'escapeHtml',
  'raw',
  'console',
  'JSON',
  '__jhsConsoleLines',
  'include',
  '__jhsEchoPart',
  'require',
  'res',
  '__jhsResolve',
  '__jhsModuleLoad',
  '__jhsModuleCache',
]);

// ── Per-render console capture ───────────────────────────────────────
// The engine passes the worker's own `console` object into the sandbox
// (console: console), so replacing the console METHODS here redirects
// template console.* calls to the active collector. Exactly one render
// runs per worker at a time (the job chain below), so a module-level
// collector pointer is race-free. While no render is active the
// fallback writes to stderr — stdout stays reserved for the parent's
// READY handshake line.
let collector = null;
for (const level of ['log', 'info', 'warn', 'error', 'debug', 'trace']) {
  console[level] = (...args) => {
    if (collector) {
      const message = args.map((arg) => String(arg)).join(' ');
      collector.push({ level, message });
    } else {
      process.stderr.write(`[jhs-worker] ${level}: ${args.join(' ')}\n`);
    }
  };
}

// ── Worker state (initialised by the `init` message) ─────────────────
let engine = null;
let viewsDir = null;
let modulesDir = null;
let forbidden = new Set();
let autoEscape = true;
let requireEnabled = true;
const builtinRequire = createRequire(import.meta.url);

// Compiled-program cache: path -> { code, mtime, includes }.
// `includes` carries (path, mtime) of every embedded partial so an
// edit to ANY partial invalidates the cached program exactly like the
// port's CachedTemplate.
const compiledCache = new Map();

// ── Compile-time include() resolution (port semantics) ───────────────

/**
 * Resolves standalone `<?jhs include("name") ?>` blocks in `source`,
 * embedding the partials' source in their place, recursively.
 * Mirrors JhsEngine::resolve_includes including the error wording.
 */
function resolveIncludes(source, depth, budget, tracked) {
  const openTag = '<?jhs';
  const closeTag = '?>';
  let out = '';
  let cursor = 0;

  while (true) {
    const openAt = source.indexOf(openTag, cursor);
    if (openAt === -1) break;
    const bodyStart = openAt + openTag.length;
    const closeOffset = source.indexOf(closeTag, bodyStart);
    if (closeOffset === -1) {
      // Unclosed code tag: keep the remainder verbatim, exactly like
      // the parser would.
      out += source.slice(cursor);
      return out;
    }
    // indexOf returns an ABSOLUTE offset — no bodyStart to add.
    const closeAt = closeOffset;
    const body = source.slice(bodyStart, closeAt);

    if (openAt > cursor) {
      out += source.slice(cursor, openAt);
    }

    const name = standaloneInclude(body);
    if (name !== null) {
      out += readInclude(name, depth, budget, tracked);
    } else {
      out += source.slice(openAt, closeAt + closeTag.length);
    }

    cursor = closeAt + closeTag.length;
  }

  out += source.slice(cursor);
  return out;
}

// `budget` is a shared accumulator object (the port's `&mut usize`):
// every embedded partial counts against the same 1 MiB budget, across
// siblings and depth levels alike.
function newBudget() {
  return { bytes: 0 };
}

/** Parses the include target of a standalone code block, or null. */
function standaloneInclude(body) {
  const trimmed = body.trim();
  if (!trimmed.startsWith('include')) return null;
  let rest = trimmed.slice('include'.length).trimStart();
  if (!rest.startsWith('(')) return null;
  rest = rest.slice(1).trimStart();
  const quote = rest.charAt(0);
  if (quote !== '"' && quote !== "'") return null;
  const end = rest.indexOf(quote, 1);
  if (end === -1) return null;
  const name = rest.slice(1, end);
  const tail = rest.slice(end + 1).trim();
  if (tail !== ')' || name.length === 0) return null;
  return name;
}

/** Reads and recursively resolves one partial (JhsEngine::read_include). */
function readInclude(name, depth, budget, tracked) {
  if (depth >= INCLUDE_MAX_DEPTH) {
    throw new IncludeError(
      `\`${name}\` exceeds the maximum include nesting depth (${INCLUDE_MAX_DEPTH})`,
    );
  }
  const file = includePath(name);
  let mtime = null;
  let source;
  try {
    const stats = fs.statSync(file);
    mtime = stats.mtimeMs;
    source = fs.readFileSync(file, 'utf8');
  } catch (error) {
    throw new IncludeError(
      `\`${name}\` could not be read from the views directory (${error.message})`,
    );
  }
  budget.bytes += source.length;
  if (budget.bytes > INCLUDE_MAX_BYTES) {
    throw new IncludeError(
      `embedding \`${name}\` exceeds the total include size budget ` +
        `(${INCLUDE_MAX_BYTES} bytes)`,
    );
  }
  tracked.push({ path: file, mtime });
  return resolveIncludes(source, depth + 1, budget, tracked);
}

/**
 * Builds the partial's path inside viewsDir, rejecting every shape that
 * could escape it — the port's include_path validation verbatim.
 */
function includePath(name) {
  const base = name.endsWith('.jhs') ? name.slice(0, -4) : name;
  const invalid = (reason) =>
    new IncludeError(
      `\`${name}\` is not a valid include name (${reason})`,
    );

  if (base.length > INCLUDE_MAX_NAME_LEN) throw invalid('too long');
  if (base.length === 0) throw invalid('empty name');
  if (
    base.includes('\\') ||
    base.startsWith('/') ||
    base.endsWith('/') ||
    base.includes('//') ||
    !/^[A-Za-z0-9_\-/]+$/.test(base)
  ) {
    throw invalid('expected relative `[A-Za-z0-9_-]` segments joined by `/`');
  }
  if (base.split('/').some((segment) => segment === '' || segment === '.' || segment === '..')) {
    throw invalid('`..` and `.` segments are not allowed');
  }
  return path.join(viewsDir, `${base}.jhs`);
}

/** Include-resolution failure (maps to JhsError::Include upstream). */
class IncludeError extends Error {
  constructor(message) {
    super(message);
    this.kind = 'include';
  }
}

// ── The require() wrapper (banner + built-ins + local modules) ───────

/**
 * Node's builtin module list, resolved without a filesystem roundtrip.
 * `module.isBuiltin` exists since Node 14; the fallback list covers the
 * few names the port banners by default anyway.
 */
const nodeModule = builtinRequire('node:module');
const isBuiltin = (name) =>
  typeof nodeModule.isBuiltin === 'function'
    ? nodeModule.isBuiltin(name)
    : ['assert', 'buffer', 'child_process', 'cluster', 'console', 'crypto',
       'dns', 'domain', 'events', 'fs', 'http', 'https', 'inspector', 'net',
       'os', 'path', 'perf_hooks', 'process', 'punycode', 'querystring',
       'readline', 'repl', 'stream', 'string_decoder', 'timers', 'tls',
       'tty', 'url', 'util', 'v8', 'vm', 'worker_threads', 'zlib'].includes(name);

/**
 * Builds the per-render `require()` handed to templates. The banner uses
 * the port's exact message wording (node-jhs2's own banned_require error
 * is a JSON blob, which the port never adopted); built-ins resolve
 * against the REAL Node require — the whole point of the sidecar
 * backend; local modules load from the modules directory with the
 * port's probe order, containment check and per-render instance cache.
 *
 * The wrapper is installed by assigning `engine.require_filter`: the
 * engine reads that property when it builds the sandbox context, so the
 * assignment wins over the constructor's bound original.
 */
function makeRequire(moduleCache) {
  function jhsRequire(spec) {
    if (typeof spec !== 'string') {
      // The port's boa error renders as "TypeError: <message>"; bake the
      // prefix in so the 500 envelope reads identically.
      throw new Error(
        `TypeError: require() expects a module name string (got ${typeof spec})`,
      );
    }
    if (spec.includes('\0')) {
      throw new Error(
        `TypeError: require() module name cannot contain NUL characters: "${spec}"`,
      );
    }
    if (spec.includes('\\')) {
      throw new Error(
        `require('${spec}') is invalid: module names must use forward slashes`,
      );
    }
    const name = spec.trim().replace(/^node:/, '');
    if (name.length === 0) {
      throw new Error('TypeError: require() expects a non-empty module name');
    }

    // 1. The banner wins over everything (built-ins and files).
    const pkg = name.split('/')[0];
    if (forbidden.has(pkg)) {
      throw new Error(
        `require('${spec}') is forbidden: the module '${pkg}' is listed in ` +
          `[templates] forbidden_modules`,
      );
    }

    // 2. Real Node built-ins — the sidecar's superpower: require('url'),
    //    require('crypto') (the full module, not the port's polyfill),
    //    require('path')… all resolve natively.
    if (isBuiltin(name)) {
      return builtinRequire(name);
    }

    // 3. Local CommonJS modules under the modules directory.
    return loadLocalModule(spec, name, '', moduleCache);
  }
  return jhsRequire;
}

/**
 * Local module resolution — the port's resolve_local_module: relative
 * specs resolve against the requiring module's directory, bare names
 * against the modules root, both also probe `<root>/node_modules`;
 * the probe order is exact file, `<name>.js`, `<name>/index.js`,
 * `<name>/package.json`'s `main`; the canonicalised path must stay
 * inside the modules directory (symlinks included).
 */
function loadLocalModule(spec, name, base, moduleCache) {
  let rootCanonical;
  try {
    rootCanonical = fs.realpathSync(modulesDir);
  } catch {
    throw new Error(
      `Cannot find module '${spec}': the modules directory ${modulesDir} does ` +
        `not exist (see [templates] modules_dir)`,
    );
  }

  const baseDir = base ? path.join(modulesDir, base) : modulesDir;
  const relative = name.startsWith('./') || name.startsWith('../');
  const searchRoots = relative
    ? [baseDir, path.join(modulesDir, 'node_modules')]
    : [modulesDir, path.join(modulesDir, 'node_modules')];

  const tried = [];
  for (const searchRoot of searchRoots) {
    const target = path.join(searchRoot, name);
    const candidate = resolveModuleFile(target);
    if (candidate) {
      let canonical;
      try {
        canonical = fs.realpathSync(candidate);
      } catch {
        throw new Error(
          `require('${spec}'): the module file ${candidate} could not be resolved`,
        );
      }
      if (
        canonical !== rootCanonical &&
        !canonical.startsWith(rootCanonical + path.sep)
      ) {
        throw new Error(
          `require('${spec}') escapes the modules directory: modules must live ` +
            `under ${modulesDir} (see [templates] modules_dir)`,
        );
      }
      return evaluateModule(spec, canonical, moduleCache);
    }
    tried.push(`${searchRoot}/${name}(, .js, /index.js)`);
  }

  throw new Error(
    `Cannot find module '${spec}': looked under the modules directory ` +
      `${modulesDir} (tried ${tried.join('; ')})`,
  );
}

/** The probe order: exact, `.js`, `index.js`, `package.json` main. */
function resolveModuleFile(target) {
  let stats;
  try {
    stats = fs.statSync(target);
  } catch {
    // Fall through to the `.js` probe.
  }
  if (stats && stats.isFile()) return target;

  const asJs = `${target}.js`;
  try {
    if (fs.statSync(asJs).isFile()) return asJs;
  } catch {
    /* keep probing */
  }

  if (stats && stats.isDirectory()) {
    const index = path.join(target, 'index.js');
    try {
      if (fs.statSync(index).isFile()) return index;
    } catch {
      /* keep probing */
    }
    const manifest = path.join(target, 'package.json');
    try {
      const main = JSON.parse(fs.readFileSync(manifest, 'utf8')).main;
      if (typeof main === 'string' && main.length > 0) {
        const mainPath = path.join(target, main);
        if (fs.statSync(mainPath).isFile()) return mainPath;
      }
    } catch {
      /* not a package, or unreadable manifest */
    }
  }
  return null;
}

/**
 * Evaluates a local module with `new Function` — the body is a
 * *parameter*, so a module cannot break out of its wrapper — with the
 * inner `require` pointing back through the banner. Module instances
 * live in the per-render cache: state can never leak between renders,
 * exactly like the port's per-render module semantics.
 */
function evaluateModule(spec, canonical, moduleCache) {
  if (moduleCache.has(canonical)) {
    return moduleCache.get(canonical).exports;
  }
  let source;
  try {
    source = fs.readFileSync(canonical, 'utf8');
  } catch (error) {
    throw new Error(`module '${canonical}' could not be read (${error.message})`);
  }
  const module = { exports: {} };
  moduleCache.set(canonical, module);
  const base = path.relative(modulesDir, path.dirname(canonical));
  let wrapper;
  try {
    wrapper = new Function(
      'exports',
      'require',
      'module',
      '__filename',
      '__dirname',
      source,
    );
  } catch (error) {
    moduleCache.delete(canonical);
    throw new Error(
      `module '${canonical}' failed to load: ${error.message}`,
    );
  }
  wrapper(
    module.exports,
    (innerSpec) => loadLocalModule(innerSpec, innerSpec.trim().replace(/^node:/, ''), base, moduleCache),
    module,
    canonical,
    path.dirname(canonical),
  );
  return module.exports;
}

// ── The res shim (port's prelude, verbatim semantics) ────────────────

/**
 * Builds the Express-shaped `res` object injected into the render data.
 * `res.redirect(location[, status])` records a local-path-only redirect
 * intent (first call wins) with the port's validation and messages.
 */
function makeResShim(intent) {
  const shim = {
    redirect(location, status) {
      if (typeof location !== 'string' || location.length === 0) {
        throw new Error(
          'TypeError: res.redirect(location) expects a target path string',
        );
      }
      if (location.charAt(0) !== '/' || location.indexOf('//') === 0) {
        throw new Error(
          "res.redirect() only accepts local paths starting with '/' " +
            '(open-redirect protection)',
        );
      }
      const code = status === undefined ? 302 : status;
      if (![301, 302, 303, 307, 308].includes(code)) {
        throw new Error(
          'res.redirect() status must be one of 301, 302, 303, 307 or 308',
        );
      }
      if (intent.value === null) {
        intent.value = { location, status: code };
      }
    },
  };
  return Object.freeze(shim);
}

// ── Render pipeline ──────────────────────────────────────────────────

/**
 * Loads (and caches) the compiled program for `file`, mirroring
 * JhsEngine::load_program: a cache hit requires the main file's mtime
 * AND every embedded partial's mtime to be unchanged.
 */
function loadProgram(file) {
  let mtime = null;
  try {
    mtime = fs.statSync(file).mtimeMs;
  } catch {
    /* missing file: fall through to the read below for the IO error */
  }

  const cached = compiledCache.get(file);
  if (cached && cached.mtime === mtime) {
    const partialsUnchanged = cached.includes.every(
      (partial) => statMtime(partial.path) === partial.mtime,
    );
    if (partialsUnchanged) return cached.code;
  }

  let source;
  try {
    source = fs.readFileSync(file, 'utf8');
  } catch (error) {
    const failure = new Error(error.message);
    failure.kind = 'io';
    throw failure;
  }
  const tracked = [];
  const resolved = resolveIncludes(source, 0, newBudget(), tracked);
  const code = engine._compile(resolved);
  compiledCache.set(file, { code, mtime, includes: tracked });
  return code;
}

function statMtime(file) {
  try {
    return fs.statSync(file).mtimeMs;
  } catch {
    return null;
  }
}

/**
 * Runs a compiled program: installs the per-render require, console
 * collector and res shim, executes through the engine, and collects the
 * port-shaped result (html + console lines + redirect intent).
 */
async function executeProgram(code, data, label) {
  const consoleLines = [];
  const intent = { value: null };
  const moduleCache = new Map();

  // The engine reads this property when it builds the sandbox context;
  // assigning undefined removes the callable — `typeof require` then
  // reads "undefined", matching the boa sandbox with require disabled
  // (a call fails with a TypeError instead of the port's ReferenceError:
  // a documented wording-only divergence).
  engine.require_filter = requireEnabled ? makeRequire(moduleCache) : undefined;

  const sandboxData = {};
  for (const [key, value] of Object.entries(data || {})) {
    if (!PROTECTED_GLOBALS.has(key)) sandboxData[key] = value;
  }
  sandboxData.res = makeResShim(intent);

  collector = consoleLines;
  try {
    const html = await engine._executeTemplate(code, sandboxData, label);
    return {
      ok: true,
      html: html === undefined || html === null ? '' : String(html),
      console: consoleLines,
      redirect: intent.value,
    };
  } catch (error) {
    // The engine already wraps execution errors in the port's wording:
    // `Template execution error (<label>): <message>`.
    const failure = { kind: 'execution', message: String(error.message), path: label };
    return { ok: false, error: failure };
  } finally {
    collector = null;
  }
}

// ── Job loop: one render at a time per worker ────────────────────────

let chain = Promise.resolve();

parentPort.on('message', (message) => {
  if (message.op === 'init') {
    viewsDir = message.viewsDir;
    modulesDir = message.modulesDir;
    forbidden = new Set(message.forbidden || []);
    autoEscape = message.autoEscape !== false;
    requireEnabled = message.requireEnabled !== false;
    engine = new JSTemplateEngine({
      viewsPath: viewsDir,
      cache: false, // the worker owns its own mtime-aware cache
      autoEscape,
    });
    parentPort.postMessage({ event: 'ready', threadId: message.threadId });
    return;
  }

  const job = message;
  chain = chain
    .then(async () => {
      if (job.op === 'render') {
        const code = loadProgram(job.template);
        return executeProgram(code, job.data, job.template);
      }
      if (job.op === 'renderString') {
        // The port's render_string resolves includes too (CMS bodies may
        // embed the shared partials) and never caches.
        const resolved = resolveIncludes(job.source, 0, newBudget(), []);
        const code = engine._compile(resolved);
        return executeProgram(code, job.data, '<string>');
      }
      return {
        ok: false,
        error: { kind: 'protocol', message: `unknown worker op '${job.op}'` },
      };
    })
    .then((result) => {
      parentPort.postMessage({ jobId: job.jobId, ...result });
    })
    .catch((error) => {
      // Include-resolution and IO failures carry a `kind`; anything else
      // is a worker bug and maps to the include/io default kinds.
      const kind = error.kind === 'include' || error.kind === 'io' ? error.kind : 'worker';
      const message = kind === 'worker' ? String(error.message || error) : String(error.message);
      parentPort.postMessage({
        jobId: job.jobId,
        ok: false,
        error: { kind, message },
      });
    });
});
