# The JHS sidecar (v0.10.0)

The Node half of wallermax-server's `[templates] backend = "sidecar" |
"auto"`: a supervised child process that renders every `.jhs` template —
the public tree, the auto-routed views and CMS page bodies alike — with
the **original `node-jhs2` engine**, so templates get the real Node
runtime behind `require()` (minus the `[templates] forbidden_modules`
banner).

```
wallermax-server (Rust)                 jhs-sidecar.mjs (Node)
┌────────────────────────────┐  loopback HTTP + token  ┌──────────────────────┐
│ middleware ─► TemplateRenderer trait ── POST /render ─► worker pool (2×)   │
│ CMS body ────┘            │  POST /render-string    │  └ node-jhs2 2.1.0  │
│        └─── boa fallback (auto)                      │     (vendored)      │
└────────────────────────────┘                         └──────────────────────┘
```

## Why

The Rust port (boa engine) is hardened but has **no Node.js behind
`require()`**: built-ins exist only as polyfills (`crypto`). Templates
porting over from a node-jhs2 deployment — `require('url')`,
`require('path')`, real Node `crypto` — fail with the module banner.
The sidecar closes that gap without giving up the security posture:

| Property | boa backend | sidecar backend |
|---|---|---|
| `require('fs')`/bannered modules | rejected (banner) | rejected (banner, same wording) |
| `require('url')`, `crypto`, `path`… | banner error | **real Node modules** |
| Runaway `<?jhs while(true){} ?>` | loop-iteration limit | **wall-clock hard-kill** (worker terminated) |
| Compile cache + hot reload | mtime-based | mtime-based (same invalidation rules) |
| `console.*` in templates | captured → tracing | captured → returned → tracing |
| `res.redirect()` | local paths only | local paths only (same validation) |
| `echo(raw())`, `<?= raw() ?>` | sentinel | sentinel (node-jhs2 2.1.0) |

## Layout

| File | Role |
|---|---|
| `jhs-sidecar.mjs` | the service: loopback HTTP, token check, worker pool, per-render budget, `/selftest` |
| `render-worker.mjs` | one render at a time: include resolution, `require()` banner + local modules, console capture, `res` shim, mtime cache |
| `engine.js` | **vendored `node-jhs2` v2.1.0** (`Justo-Tapiador/node-jhs2`, MIT, same author), pinned and byte-identical under the provenance header |

No `package.json`, no npm install, no network at build or run time: the
engine is vendored and everything else is Node built-ins. Node 18+ is
enough (worker_threads, `node:http`, ESM/CJS interop).

## Configuration

Set from `wallermax.toml` (see `[templates] backend` and
`[templates.sidecar]` there); the values reach the sidecar as
environment variables:

| Env (set by the server) | Meaning |
|---|---|
| `JHS_SIDECAR_TOKEN` | random per-spawn shared secret (required) |
| `JHS_SIDECAR_PORT` | `0` = OS-assigned, reported on the handshake line |
| `JHS_SIDECAR_VIEWS_DIR` / `JHS_SIDECAR_MODULES_DIR` | absolute roots, same as the boa engine's |
| `JHS_SIDECAR_FORBIDDEN` | JSON array: `[templates] forbidden_modules` |
| `JHS_SIDECAR_AUTO_ESCAPE` | `[templates] auto_escape` |
| `JHS_SIDECAR_REQUIRE_ENABLED` | `false` removes the callable `require` |
| `JHS_SIDECAR_WORKERS` / `JHS_SIDECAR_RENDER_BUDGET_MS` | pool size / hard-kill budget |

## Protocol

- `POST /render` `{"path": "<abs .jhs>", "data": {...}}`
- `POST /render-string` `{"source": "<?jhs ... ?>", "data": {...}}`
  → `{"ok": true, "html", "console": [{level, message}], "redirect": null | {location, status}}`
  or `{"ok": false, "error": {"kind": "io" | "include" | "execution" | "timeout" | "worker" | "protocol", "message", "path"}}`
  (kinds map onto the Rust port's `JhsError` variants — including the
  `Template execution error (<path>): …` wording).
- `GET /selftest` — 13 checks through the real dispatch path (escaping,
  raw sentinel, `echo(raw())`, console capture, data injection,
  `res.redirect` + open-redirect rejection, built-in require, banner
  wording, local-module miss, file render + mtime hot reload, hard-kill,
  worker respawn). The server gates the backend on `ok`.
- `GET /health` — liveness.

Every request must carry `x-sidecar-token` (timing-safe compare); the
service binds `127.0.0.1` only, answers `connection: close` with
`content-length` framing, and caps bodies at 16 MiB.

## Lifecycle

The server spawns the sidecar at startup, parses the single stdout
handshake line (`{"event":"ready","port":…}` — stdout is reserved for
it, logs go to stderr), runs the selftest, then supervises the child:
it is killed on renderer drop, and it also exits by itself when its
stdin pipe closes (parent death) or on `SIGTERM`/`SIGINT` — no orphan
survives the server. With `backend = "auto"`, a mid-render transport
failure falls back to boa for that render and a rate-limited health
probe brings the sidecar back automatically.

## Known divergences from the boa backend

- **`Buffer` exists in the sandbox** (the original engine passes it);
  harmless byte utilities, no `fs` behind it unless you un-ban `fs`.
- **Non-standalone `include()`** (inside an expression) behaves like the
  original: it returns a promise that renders nothing. Standalone
  `<?jhs include("x") ?>` blocks are compile-time resolved exactly like
  the port. The port throws a descriptive error instead.
- **Disabled `require()`** fails calls with `TypeError: require is not
  a function` instead of the port's `ReferenceError` (wording only;
  `typeof require` reads `undefined` in both).
- **Module instances are per render** (matching the port) but the
  compiled-template cache is **per worker** (with the default two
  workers a template compiles at most twice).
- The runaway-render bound is **time** (default 5 s hard-kill), not the
  port's loop-iteration count.

## Vendoring note

`engine.js` is `node-jhs2` v2.1.0 with a provenance header. The service
and worker call two engine methods node-jhs2 documents as internal
(`_compile`, `_executeTemplate`) — safe because the copy is pinned in
this repository. To upgrade: re-vendor the new `index.js`, update the
header, then run `node sidecar/jhs-sidecar.mjs` (the selftest needs a
token: `JHS_SIDECAR_TOKEN=dev-token-0123456789abcdef JHS_SIDECAR_PORT=0`
plus a views dir) and `cargo test --test sidecar`.

Manual smoke against a running server:

```
curl 'http://127.0.0.1:8080/sidecar-check.jhs?probe=7'   # → sidecar-ok:7
```

(a 500 there means the boa backend answered instead — the sidecar is
down or `backend = "boa"`).
