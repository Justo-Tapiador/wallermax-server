# wallermax-server

[![Rust](https://img.shields.io/badge/Rust-1.88%2B-orange?logo=rust)](https://www.rust-lang.org) [![Built with Axum](https://img.shields.io/badge/Built%20with-Axum%200.8-blueviolet)](https://github.com/tokio-rs/axum) [![License: MIT](https://img.shields.io/badge/License-MIT-blue)](LICENSE) [![Roadmap](https://img.shields.io/badge/Roadmap-All%208%20phases%20done-green)](#roadmap)

<div align="left">
<p><img src="ws.png" width="482" alt="IA-SO BROODER"></p>
</div>

> A modular, secure and high-performance web server written in Rust.

**Status: v0.11.0 — the roadmap phases done, a dynamic template engine with require() running on the original node-jhs2 engine, 
browser sessions, and a small built-in CMS that owns the homepage.** Phase 4 added rotating
refresh tokens with family revocation, a Prometheus `/metrics` endpoint, HTTPS via
rustls (plus an HTTP-to-HTTPS redirect listener), trusted-proxy `X-Forwarded-For`
parsing, a multi-stage Docker image with a compose example and a GitHub Actions CI
pipeline. v0.5.0 added the sandboxed `.jhs` template engine, v0.6.0 injects the
authenticated identity into every render as the `user` global, v0.7.0 keeps
browsers logged in with the `wallermax_session` cookie and the no-JavaScript
`/login` page, v0.8.0 adds the CMS: database-backed pages at `/p/<slug>`,
login/registration modals, a JavaScript-free admin panel for content and accounts,
and v0.9.0 brings `require()` back to the templates — a native module bridge
with a configurable forbidden-modules banner, the `crypto` polyfill,
CommonJS modules under `modules/`, plus `res.redirect()` and the `req`
global. v0.10.0 moves every render onto a supervised **Node sidecar**
running the **original node-jhs2 engine** (`[templates] backend =
auto | boa | sidecar`), and v0.10.1 exposes the live backend through
`GET /health` and the `wallermax_template_backend` metric. v0.11.0 gives
the CMS the homepage: `[cms] default_page` renders a database-backed page
at `GET /` — ahead of the static index file, with the `/p/<slug>` draft
gating and a graceful fallback. v0.12.0 adds the server-side external
API proxy: `GET/POST /api/ext/{name}` forwards to operator-named
upstreams, injecting API keys the browser never sees (and sidestepping
upstream CORS entirely). See
[README-jhs-engine.md](README-jhs-engine.md),
[Template backends](#template-backends-boa-and-the-node-sidecar-v0100),
[The CMS](#the-cms-v080),
[External API proxy](#external-api-proxy-v0120) and the
[roadmap](#roadmap).

## Table of contents

- [Features](#features)
- [Requirements](#requirements)
- [Getting started](#getting-started)
- [Configuration](#configuration)
- [Security headers and the CSP cookbook](#security-headers-and-the-csp-cookbook)
- [HTTP API](#http-api)
- [Dynamic templates (.jhs)](#dynamic-templates-jhs)
- [Template backends: boa and the Node sidecar (v0.10.0)](#template-backends-boa-and-the-node-sidecar-v0100)
- [Refresh tokens](#refresh-tokens)
- [Browser sessions (v0.7.0)](#browser-sessions-v070)
- [The CMS (v0.8.0)](#the-cms-v080)
- [External API proxy (v0.12.0)](#external-api-proxy-v0120)
- [Prometheus metrics](#prometheus-metrics)
- [TLS (HTTPS)](#tls-https)
- [Trusted proxies and client IPs](#trusted-proxies-and-client-ips)
- [Docker & CI](#docker--ci)
- [Architecture](#architecture)
- [Project structure](#project-structure)
- [Testing](#testing)
- [Security notes](#security-notes)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)

## Features

- **Fast and efficient** — fully asynchronous runtime (Tokio) with the Axum/Hyper
  HTTP stack; the release profile enables thin LTO and symbol stripping.
- **Memory safe** — the whole crate is compiled with `#![forbid(unsafe_code)]`.
- **Modular** — route *modules* and middleware *modules*; new features are added
  by adding a module and one registration line, nothing else changes.
- **Highly configurable** — built-in defaults → `wallermax.toml` →
  `wallermax.local.toml` → environment variables; every layer overrides the
  previous one.
- **Composable middleware pipeline** — security headers, CORS, request-id,
  logging/timing, rate limiting, body limits and request timeout; each one
  can be toggled independently at runtime configuration.
- **Rate limiting** — per-client-IP token buckets (`429` + `Retry-After` +
  `X-RateLimit-*` headers), bounded memory with stale-bucket sweeping.
- **CORS** — exact-origin allowlist or wildcard, configurable preflight
  caching; disabled by default (browsers then deny all cross-origin reads).
- **External API proxy** — `GET/POST /api/ext/{name}` forwards to
  operator-configured upstreams with `${ENV}`-expanded secret headers, so
  API keys never reach the browser and upstream CORS stops mattering
  (v0.12.0).
- **Request body limits** — early `Content-Length` rejection plus
  stream-level enforcement; `413` with the JSON error envelope.
- **JSON-first** — consistent JSON success bodies and a consistent JSON error
  envelope (`{ "error": { code, message, request_id } }`), including 404, 405,
  408, 413 and 429 responses.
- **Operable** — pretty/JSON/compact log formats, `GET /api/stats` runtime
  metrics (including rate-limited rejections), graceful shutdown on
  `SIGINT`/`SIGTERM` with a hard timeout.
- **Persistent** — SQLite storage (WAL mode) with migrations **embedded in
  the binary** (single-file deployment); user accounts survive restarts.
- **Authenticated** — JWT access tokens (HS256) with Argon2id password
  hashing; role-based access (`admin` / `editor` / `user`) enforced by typed
  extractors.
- **Static files** — serve a website from a directory (`GET /` answers
  `public/index.html`); conditional requests (`304`), range requests
  (`206`), correct content types, path-traversal rejection and the JSON
  404 envelope for missing files.
- **Dynamic `.jhs` templates** — a PHP-style JavaScript template engine
  (a faithful port of [node-jhs2](https://github.com/Justo-Tapiador/node-jhs2))
  running in a [boa_engine](https://github.com/boa-dev/boa) sandbox:
  `require()` since v0.9.0 (native bridge: `forbidden_modules` banner,
  `crypto` polyfill, CommonJS modules jailed to `modules/`), no `Buffer`,
  no network, escaped output by default, `res.redirect()` and the `req`
  global, loop-iteration bounds, mtime-based recompilation, view
  auto-routing and `console.*` routed to the structured logs. See
  [README-jhs-engine.md](README-jhs-engine.md).
- **Pluggable template backends** (v0.10.0) — every render in the process
  (public `.jhs`, auto-routed views, CMS page bodies) can flow through a
  supervised **Node sidecar** running the original node-jhs2 engine
  (vendored in `sidecar/`, no npm install): `[templates] backend =
  auto` (the default — sidecar with transparent boa fallback and
  automatic recovery), `boa` (zero Node, the hardened sandbox) or
  `sidecar` (strict — the server refuses to start without it). Real
  `require()` built-ins behind the `forbidden_modules` banner, a
  wall-clock hard-kill budget for runaway renders, a 13-check startup
  selftest, and the live backend visible in `GET /health` plus the
  `wallermax_template_backend` metric (v0.10.1). See
  [Template backends](#template-backends-boa-and-the-node-sidecar-v0100).
- **Refresh tokens** — long-lived opaque sessions (256-bit, stored only as
  SHA-256 hashes) with rotation on every refresh and automatic family
  revocation when a retired token is replayed; `POST /api/auth/logout`
  and `logout_all` end one or all sessions.
- **Browser sessions** — login also sets the access token as an
  `HttpOnly` + `SameSite=Strict` cookie (`Secure` follows `[auth]`
  `secure_cookies`, `auto` by default), so plain page navigation stays
  authenticated and `.jhs` templates personalise; the shipped `/login`
  view and the site-wide login/registration modals are JavaScript-free
  HTML forms (form posts get a `303` redirect, JSON clients keep the
  exact same API).
- **Built-in CMS** — pages live in SQLite and render as `.jhs` templates
  at `/p/<slug>` (with the `user`/`path`/`query`/`pages` globals and the
  shared partials via `include()`); a `/admin` panel manages pages
  (create, edit, publish, import from `public/`) and accounts (admin
  only), plus a self-service password change — every bit of it plain
  HTML forms, no JavaScript, CSP keeps scripts blocked. Since v0.11.0
  `[cms] default_page` mounts any page as the homepage: `GET /` renders
  it through the exact `/p/<slug>` pipeline, ahead of the static index
  file, with a graceful fallback when the slug stops existing. The
  panel manages content and accounts only: server configuration stays
  in `wallermax.toml`.
- **Prometheus metrics** — `GET /metrics` (configurable path) exposing
  request counters, latency histograms, rate-limit rejections, uptime and
  registered users in the text exposition format; scrapes are exempt
  from rate limiting so a throttled server stays observable.
- **TLS (HTTPS)** — rustls with the ring provider (TLS 1.2/1.3, no
  OpenSSL linkage), plus an optional plain-HTTP listener that answers
  every request with a `308` redirect to its HTTPS equivalent.
- **Proxy-aware** — `X-Forwarded-For` is honoured only from explicitly
  trusted proxies (exact IPs or CIDR blocks), so clients cannot spoof
  their rate-limit bucket identity; untrusted headers are ignored.
- **Container-ready** — multi-stage Dockerfile (non-root user, `/data`
  volume for SQLite, env-var configuration) plus a compose example, and
  a GitHub Actions CI pipeline (fmt, clippy, Linux + Windows test
  matrix, Docker build with in-container smoke test).
- **Storage-agnostic** — handlers depend on the `UserRepository` trait, not
  on SQLite; the engine can be swapped without touching HTTP code.
- **Tested** — 453 tests: unit tests per module plus end-to-end integration
  tests that boot the *real* server (plain HTTP and HTTPS) and speak HTTP
  to it — including a cookie-jar "browser" battery for the CMS and the
  Node sidecar battery (spawn, selftest, parity, hard-kill, respawn)
  whenever Node is on `PATH`.

## Requirements

- Rust **1.88 or newer** (stable toolchain) — the resolved dependency tree
  in `Cargo.lock` requires it; `rust-version` in `Cargo.toml` documents
  it and the Dockerfile pins the same version. On Windows, the
  `x86_64-pc-windows-msvc` target with Visual Studio 2022 works out of
  the box (which is the standard `rustup` setup).
- A C compiler (the SQLite bundled with sqlx, and ring for rustls, are
  compiled from source). Visual Studio 2022's MSVC on Windows, or
  `gcc`/`clang` elsewhere.
- Docker (optional) — only to build/run the container image.
- Node.js **18 or newer** (optional) — only while `[templates] backend =
  "sidecar"` or `"auto"` (the default): the sidecar runs the original
  node-jhs2 engine as a supervised child process. `backend = "boa"`
  needs no Node at all, and the Docker runtime image already ships
  Node.js, so containers are covered either way.
- No other runtime requirements.

## Getting started

```console
$ cargo run
   Compiling wallermax-server v0.11.0
    Finished dev [unoptimized + debuginfo] target(s)
     Running `target/debug/wallermax-server`

INFO wallermax_server::server: static file serving enabled root_dir=public index_file=index.html
INFO wallermax_server::server: dynamic template rendering enabled views_dir=views auto_escape=true cache=true
INFO wallermax_server::state: template backend: auto (Node sidecar, boa fallback)
INFO wallermax_server::template_engine::sidecar: the JHS sidecar is up: templates render on the original node-jhs2 engine, addr: 127.0.0.1:45145, workers: 2, budget_ms: 5000
INFO wallermax_server::server: sqlite pool ready (migrations applied) url=sqlite://wallermax.db?mode=rwc max_connections=5
INFO wallermax_server::server: authentication enabled (the first registered user becomes the admin) registration_enabled=true token_ttl_secs=3600 refresh_tokens_enabled=true refresh_token_ttl_secs=2592000
INFO wallermax_server::server: cms enabled (public pages at /p, admin panel at /admin; content and users — server configuration stays in wallermax.toml)
INFO wallermax_server::server: prometheus metrics enabled path=/metrics
INFO wallermax_server::server: wallermax-server listening address=127.0.0.1:8080 version=0.11.0
INFO wallermax_server::server: route map ready routes="GET / (static) | GET /api | GET /health | GET /api/stats | POST /api/echo | GET /metrics | POST /api/auth/register | POST /api/auth/login | GET /api/auth/me | POST /api/auth/refresh | POST /api/auth/logout | POST /api/auth/logout_all | GET /api/admin/users | GET /p/{slug} | GET /admin | POST /perfil/password | + static files | + .jhs templates"
```

The default `wallermax.toml` ships with static files, the database,
authentication and the CMS enabled, so the very first registration becomes
the admin. Open `http://127.0.0.1:8080/` in a browser: the site home is the
dynamic `views/index.jhs` (the stock `public/index.html` was removed in
v0.8.0 — drop your own back in to pin a static home), with login/registration
modals in the header and the published CMS pages listed:

```console
$ curl http://127.0.0.1:8080/
<!DOCTYPE html>
<html lang="es">
...
<h1>Un sitio que administra su contenido a sí mismo</h1>
...
<a class="btn btn-primario" href="#registrar">Registrarse</a>   # the modals
...

$ curl http://127.0.0.1:8080/p                # the published CMS pages
...

$ curl http://127.0.0.1:8080/api
{"service":"wallermax-server",...}

$ curl http://127.0.0.1:8080/health
{"status":"ok","version":"0.11.0","template_backend":"sidecar"}

$ curl http://127.0.0.1:8080/hello.jhs     # .jhs template, rendered on the fly
<h1>Hola desde una plantilla .jhs</h1>
...                                       # demo; see README-jhs-engine.md

$ curl "http://127.0.0.1:8080/sidecar-check.jhs?probe=7"   # sidecar probe (v0.10.0)
sidecar-ok:7

$ curl http://127.0.0.1:8080/metrics | head -4
# HELP wallermax_requests_total Requests served, by HTTP method and response status code.
# TYPE wallermax_requests_total counter
wallermax_requests_total{code="200",method="GET"} 3
...
$ curl http://127.0.0.1:8080/metrics | grep template_backend   # which engine is live (v0.10.1)
wallermax_template_backend{backend="sidecar"} 1
wallermax_template_backend{backend="boa"} 0

$ curl -X POST http://127.0.0.1:8080/api/auth/register \
    -H "Content-Type: application/json" \
    -d '{"username":"admin","password":"correct-horse-battery"}'
{"id":1,"username":"admin","role":"admin","created_at":1788646761,"last_login_at":null}   # 201 Created

$ curl -X POST http://127.0.0.1:8080/api/auth/login \
    -H "Content-Type: application/json" \
    -d '{"username":"admin","password":"correct-horse-battery"}'
{"access_token":"eyJhbGciOi...","token_type":"Bearer","expires_in":3600,"user":{...},"refresh_token":"cU9tY1...","refresh_expires_in":2592000}

# The same login also sets the session cookie (see "Browser sessions");
# curl keeps it in a jar with -c/-b:
$ curl -c cookies.txt -X POST http://127.0.0.1:8080/api/auth/login \
    -H "Content-Type: application/json" \
    -d '{"username":"admin","password":"correct-horse-battery"}' > /dev/null
$ curl -b cookies.txt http://127.0.0.1:8080/perfil
<h1>Bienvenido, administrador admin</h1>  # the user global, via cookie

$ curl http://127.0.0.1:8080/api/auth/me \
    -H "Authorization: Bearer eyJhbGciOi..."
{"id":1,"username":"admin","role":"admin","created_at":1788646761,"last_login_at":1788646780}

$ curl -X POST http://127.0.0.1:8080/api/auth/refresh \
    -H "Content-Type: application/json" \
    -d '{"refresh_token":"cU9tY1..."}'
{"access_token":"eyJhbGciOi...","token_type":"Bearer","expires_in":3600,"user":{...},"refresh_token":"NEW-OPAQUE-TOKEN","refresh_expires_in":2592000}
```

Stop the server with `Ctrl-C`: it stops accepting new connections, waits for
in-flight requests (up to `server.shutdown_timeout_secs`) and exits cleanly.
Restarting picks the database back up: registered users and their passwords
persist, no re-migration runs (sqlx tracks applied migrations). Note that
`POST /api/auth/refresh` also performs best-effort housekeeping on every
call (expired refresh tokens are pruned), so no cron job is needed.

Run the full test suite:

```console
$ cargo test
```

Run an optimized production build:

```console
$ cargo run --release
```

> **Windows note.** If `cargo run` fails with `failed to remove file ...
wallermax-server.exe` (`os error 5`), a previous instance is still
> running and Windows locks executables of live processes. Stop it first:
> `Stop-Process -Name wallermax-server -Force -ErrorAction SilentlyContinue`
> (PowerShell) or `taskkill /F /IM wallermax-server.exe` (cmd), then re-run.
> A compile that fails right after a successful run can also be Windows
> Defender scanning the fresh binary — waiting a few seconds and retrying
> is usually enough.

## Configuration

Configuration sources are applied in order; **later sources win**:

1. Built-in defaults (sensible for local development).
2. `wallermax.toml` — versioned project configuration (this file ships with
   the repository and is expected to be committed).
3. `wallermax.local.toml` — optional personal overrides, git-ignored.
4. Environment variables prefixed with `WALLERMAX_`, with `__` as the
   nested-key separator.

The base config file path can be changed with `WALLERMAX_CONFIG`
(e.g. `WALLERMAX_CONFIG=/etc/wallermax/production.toml`).

Conventions: `[middleware]` holds the boolean switches for the pipeline;
each middleware's tuning values live in their own section.

### Keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `server.host` | IP address | `127.0.0.1` | Bind address (`0.0.0.0` to expose on the network). |
| `server.port` | integer | `8080` | Bind port (`0` = OS-assigned). |
| `server.request_timeout_secs` | integer | `15` | Max seconds per request before a `408` is returned. |
| `server.shutdown_timeout_secs` | integer | `15` | Max seconds to wait for in-flight requests on shutdown. |
| `server.max_body_size_bytes` | integer | `1048576` | Max accepted request body size (1 MiB). |
| `server.trusted_proxies` | list | `[]` | IPs/CIDRs whose `X-Forwarded-For` is trusted (e.g. `["10.0.0.0/8"]`). Empty = always use the TCP peer address. |
| `logging.level` | string | `info` | `trace` / `debug` / `info` / `warn` / `error`. `RUST_LOG` wins when set. |
| `logging.format` | string | `pretty` | `pretty` / `json` / `compact`. |
| `middleware.request_id` | bool | `true` | Unique `X-Request-Id` per request, echoed on responses. |
| `middleware.logging` | bool | `true` | Structured request logging + `X-Response-Time` + stats counter. |
| `middleware.security_headers` | bool | `true` | Security headers on every response. |
| `middleware.timeout` | bool | `true` | Enforce `server.request_timeout_secs`. |
| `middleware.rate_limit` | bool | `false` (defaults) / `true` (wallermax.toml) | Per-client-IP rate limiting. |
| `middleware.cors` | bool | `false` | Cross-origin resource sharing. |
| `middleware.body_limit` | bool | `true` | Enforce `server.max_body_size_bytes`. |
| `request_id.mode` | string | `accept` | `accept` reuses valid client ids; `overwrite` always generates server-side ids. |
| `rate_limit.capacity` | integer | `60` | Burst size: requests allowed before throttling. |
| `rate_limit.refill_per_second` | float | `10.0` | Steady token refill rate per second. |
| `cors.allowed_origins` | list | `[]` | Allowed origins, e.g. `["https://app.example.com"]` or `["*"]`. |
| `cors.max_age_secs` | integer | `3600` | Preflight response caching time. |
| `security_headers.*` | string | see file | One key per header (`nosniff`, `DENY`, ...); empty value omits the header. |
| `external_api.timeout_secs` | integer | `8` | Per-call upstream timeout; must be 1-60 and below `server.request_timeout_secs`. |
| `external_api.response_limit_bytes` | integer | `262144` | Cap on forwarded upstream bodies (1-16777216); oversized answers become `502`. |
| `external_api.endpoints` | list of tables | `[]` | Named upstreams; one entry = one route. Empty (the default) keeps the whole family unmounted. |
| `external_api.endpoints.name` | string | — | Route segment `/api/ext/{name}`; slug rules (lowercase, digits, single dashes), unique. |
| `external_api.endpoints.url` | string | — | Absolute upstream `http`/`https` URL; the incoming query string is appended per call. |
| `external_api.endpoints.auth_required` | bool | `false` | When `true`, calls need a Bearer token or session cookie; the rate limiter applies either way. |
| `external_api.endpoints.headers` | table | `{}` | Headers attached to every upstream call; values may use `${VAR}` env references. |
| `database.enabled` | bool | `false` (defaults) / `true` (wallermax.toml) | Mount the SQLite persistence layer (required by `[auth]`). |
| `database.url` | string | `sqlite://wallermax.db?mode=rwc` | SQLite URL; `mode=rwc` creates the file, `sqlite::memory:` is ephemeral. |
| `database.max_connections` | integer | `5` | Pool size (SQLite serializes writes; small is fine). |
| `auth.enabled` | bool | `false` (defaults) / `true` (wallermax.toml) | Mount the auth + admin route families. |
| `auth.jwt_secret` | string | dev value in file | HMAC-SHA256 secret, at least 32 chars. **Set via env in production.** |
| `auth.token_ttl_secs` | integer | `3600` | Access token lifetime. |
| `auth.issuer` | string | `wallermax-server` | Expected `iss` claim. |
| `auth.registration_enabled` | bool | `true` | Whether new users may self-register. |
| `auth.min_password_len` | integer | `8` | Minimum accepted password length. |
| `auth.refresh_tokens_enabled` | bool | `true` | Mount `/api/auth/refresh`, `/logout`, `/logout_all`; login returns a refresh token. |
| `auth.refresh_token_ttl_secs` | integer | `2592000` | Refresh token lifetime (30 days). |
| `cms.enabled` | bool | `false` (defaults) / `true` (wallermax.toml) | Mount the CMS route family (requires `[database]`, `[auth]` and `[templates]`). |
| `cms.default_page` | string | unset | Slug of the CMS page that takes over `GET /` (v0.11.0) — beats the static index file; a missing slug warns and falls back, a draft stays editor-only. |
| `metrics.enabled` | bool | `false` (defaults) / `true` (wallermax.toml) | Serve the Prometheus exposition endpoint. |
| `metrics.path` | string | `/metrics` | Path of the exposition endpoint. |
| `tls.enabled` | bool | `false` | Serve HTTPS (rustls) on `server.host:port` instead of plain HTTP. |
| `tls.cert_path` | string | — | PEM certificate chain (required when TLS is on). |
| `tls.key_path` | string | — | PEM private key (required when TLS is on). |
| `tls.http_listen` | string | — | Optional `host:port` plain-HTTP listener answering `308` redirects to HTTPS. |
| `static.enabled` | bool | `false` (defaults) / `true` (wallermax.toml) | Serve static files for otherwise unmatched paths. |
| `static.root_dir` | string | `public` | Directory holding the assets; must exist when enabled. |
| `static.index_file` | string | `index.html` | File served for `GET /` (inside `root_dir`). |
| `templates.backend` | string | `auto` | Which engine renders `.jhs`: `auto` / `boa` / `sidecar` — see [Template backends](#template-backends-boa-and-the-node-sidecar-v0100). |
| `templates.sidecar.node_command` | string | `node` | Node.js binary the sidecar spawns. |
| `templates.sidecar.script` | string | `sidecar/jhs-sidecar.mjs` | Sidecar entry point (the loopback HTTP service). |
| `templates.sidecar.workers` | integer | `2` | Render worker pool size (one worker = one render). |
| `templates.sidecar.startup_timeout_ms` | integer | `8000` | Budget for spawn + READY handshake + the selftest. |
| `templates.sidecar.request_timeout_ms` | integer | `10000` | Client-side timeout per render (must exceed `render_budget_ms`). |
| `templates.sidecar.render_budget_ms` | integer | `5000` | Wall-clock hard-kill per render — runaway loops die with the worker, not the server. |

### Environment variable overrides

Nested keys map to `WALLERMAX_` + sections joined by `__`:

```console
$ WALLERMAX_SERVER__PORT=9000 \
  WALLERMAX_RATE_LIMIT__CAPACITY=5 \
  WALLERMAX_RATE_LIMIT__REFILL_PER_SECOND=1 \
  WALLERMAX_LOGGING__FORMAT=json \
  WALLERMAX_AUTH__JWT_SECRET='production-secret-at-least-32-chars' \
  cargo run
```

Values are parsed automatically (`"9000"` → integer, `"5.5"` → float,
`"false"` → boolean), which makes this pattern ideal for containers and CI.
Providing the JWT secret through the environment (rather than a file) is the
recommended production setup.

## Security headers and the CSP cookbook

The `[security_headers]` middleware is the outermost layer of the pipeline:
its five headers ride on **every** response — pages, API envelopes, static
files, even the 404/408/413/429 answers. Four of them (`nosniff`,
`X-Frame-Options`, `Referrer-Policy`, HSTS) are set-and-forget. The fifth,
`content_security_policy`, decides what visiting **browsers** may load and
execute on your pages — and it is the one a site owner will tune.

The one thing to internalize before debugging a "broken" page: the server
always ships the markup intact. When a `<script>` or a `style="…"`
attribute seems to do nothing, it is the browser that refused it —
silently — because of this header. Open the developer console (F12) and it
will tell you verbatim.

### What the shipped policy allows and blocks

`wallermax.toml` ships:

```toml
[security_headers]
content_security_policy = "default-src 'none'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
```

which keeps the whole site pure HTML+CSS: JavaScript runs **server-side**
inside the `.jhs` engine, never in the visitor's browser. Directive by
directive (anything not named inherits `default-src`):

| Directive | What it governs in the browser | Shipped value | Net effect |
|-----------|-------------------------------|---------------|------------|
| `default-src` | Fallback for every directive not named below | `'none'` | Scripts, iframes, fonts, media, `fetch`/XHR and workers are all blocked |
| `style-src` | Stylesheets, `<style>` blocks, `style="…"` attributes | `'self'` | Only same-origin CSS (`/assets/wallermax.css`) applies — inline styles and `<style>` blocks are refused |
| `img-src` | `<img>`, CSS background images | `'self' data:` | Same-origin images and `data:` URIs; external hotlinks refused |
| `form-action` | Where `<form>` may submit | `'self'` | Forms post back to this origin only |
| `frame-ancestors` | Who may embed **your** pages in an iframe | `'none'` | Nobody (clickjacking shield) |
| `base-uri` | `<base href>` targets | `'none'` | An injected `<base>` cannot rewrite relative URLs |

(`form-action`, `frame-ancestors` and `base-uri` never inherit from
`default-src` — that is why the shipped value names them explicitly.)

When something is blocked, the console says so:

```console
Refused to apply inline style because it violates the following Content
Security Policy directive: "style-src 'self'". Either the 'unsafe-inline'
keyword, a hash, or a nonce is required to enable inline execution.

Refused to execute inline script because it violates the following Content
Security Policy directive: "default-src 'none'".

Refused to display 'https://localhost/p/home' in a frame because it set
'X-Frame-Options' to 'deny'.
```

### Applying and verifying a policy

Three places, later layers winning: `wallermax.toml` (the committed product
default) → the git-ignored `wallermax.local.toml` (personal overrides) →
`WALLERMAX_SECURITY_HEADERS__CONTENT_SECURITY_POLICY` (environment
variables, e.g. containers). The configuration is read at startup —
restart after changing it. An **empty string omits the header entirely**
(see the last recipe). Verify what you are actually sending:

```console
$ curl -sD - -o /dev/null https://localhost/p/<slug> | grep -iE 'content-security|x-frame'
```

(Windows PowerShell: `curl.exe -k -sD - -o NUL https://localhost/p/<slug>`
and look for the headers in the output.)

### The recipes

Every recipe is a complete drop-in value for
`security_headers.content_security_policy` — the shipped policy plus
exactly one concern. Start from the one you need and combine by adding
directives together (the last recipe shows a realistic combination).

#### Inline styles: `style="…"` and `<style>` blocks

```toml
content_security_policy = "default-src 'none'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
```

Unlocks `style="color:Gold"` in CMS page bodies and `<style>` blocks in
your own views; the same-origin stylesheet keeps loading. The trade-off:
inline CSS enables overlay-based UI redressing (a fake login form drawn
with `position:fixed`), so relax it only when page authors are trusted.
CSS cannot read cookies or storage, and external `url()` loads still hit
the `img-src` wall, so classic CSS exfiltration stays contained.

#### Client-side JavaScript

```toml
content_security_policy = "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
```

Unlocks `<script src="/assets/app.js">` served from `public/`. The
`connect-src 'self'` part is what lets that script talk to the HTTP API
(`fetch('/api/…')`) — without it the script loads but every request is
refused. To load from a CDN, extend the source list:
`script-src 'self' https://cdn.jsdelivr.net` — specific hosts, never `*`.
Inline `<script>` and `onclick="…"` attributes stay blocked: keep the code
in files. From this recipe on, the no-JavaScript guarantee is gone —
`.jhs` auto-escaping still stands, but the CSP no longer backstops it.

#### Embedded content: YouTube, Vimeo, Google Maps

```toml
content_security_policy = "default-src 'none'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; frame-src https://www.youtube-nocookie.com https://player.vimeo.com https://www.google.com"
```

Unlocks `<iframe>` embeds from the listed hosts
(`www.youtube-nocookie.com` is the privacy-enhanced YouTube player).
`frame-src` inherits `'none'` from `default-src`, which is why unlisted
embeds render as empty rectangles. For self-hosted `<video>`/`<audio>` add
`media-src 'self'` (or the hosting origin). Note this recipe embeds OTHER
sites in YOUR pages — the reverse situation has its own recipe below.

#### Web fonts, hosted media and WebSockets

```toml
content_security_policy = "default-src 'none'; style-src 'self' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; connect-src 'self' wss://api.example.com"
```

Google Fonts needs **two** permissions: the stylesheet comes from
`fonts.googleapis.com` (a `style-src` concern) while the font files
themselves come from `fonts.gstatic.com` (`font-src`). `connect-src`
governs `fetch`, XHR and WebSockets — list `wss://` endpoints explicitly
when your client-side JavaScript talks to a live service.

#### Letting other sites embed yours

```toml
[security_headers]
x_frame_options = ""
content_security_policy = "default-src 'none'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors https://partner.example; base-uri 'none'"
```

Two switches, not one: `frame-ancestors` names who may embed your pages —
and `x_frame_options` (a **separate** header, still `DENY` from
`wallermax.toml`) keeps blocking every frame regardless of what the CSP
allows. Empty the one and narrow the other, or the iframe stays blank with
a console refusal naming `X-Frame-Options`. `SAMEORIGIN` is the middle
ground when only your own pages may embed each other.

#### Everything open (last resort)

```toml
[security_headers]
x_frame_options = ""
content_security_policy = ""
```

An empty value **omits the header** — the browser falls back to its
defaults and everything loads, which is what most of the web effectively
runs. The explicit equivalent, for when you want the header present but
unrestricted:

```toml
content_security_policy = "default-src * data: blob: 'unsafe-inline' 'unsafe-eval'; form-action *; frame-ancestors *; base-uri *"
```

(`*` does not cover the `data:`/`blob:` schemes, and the last three
directives never inherit from `default-src`.) What you lose, concretely:
any script an editor pastes into a CMS page then executes in every
visitor's browser — the second XSS barrier is gone (`.jhs` auto-escaping
still stands, and the `HttpOnly` session cookie keeps scripts from
stealing tokens, but in-page impersonation is possible). Use this to
experiment locally, not as a resting posture.

#### Putting it together — a realistic policy

A site with its own `/assets/app.js` calling the API, inline styles in CMS
pages, a YouTube embed and Google Fonts:

```toml
content_security_policy = "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; img-src 'self' data:; media-src 'self'; frame-src https://www.youtube-nocookie.com; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"
```

Scripts still limited to your own origin, styles inline but fonts pinned,
embeds whitelisted — nothing open to the whole internet.

## HTTP API

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/` | — | The CMS default page while `cms.default_page` is set (v0.11.0); otherwise the static index file while `[static]` is on, or the JSON service index. |
| `GET` | `/api` | — | Service index: name, version and endpoint discovery. |
| `GET` | `/health` | — | Liveness probe (version + the live `template_backend`). |
| `GET` | `/api/stats` | — | Runtime metrics (+ `registered_users` while auth is on). |
| `POST` | `/api/echo` | — | Debug utility: reads and describes the request body. |
| `GET`/`POST` | `/api/ext/{name}` | per endpoint | External API proxy (v0.12.0): forward to the named configured upstream — see [External API proxy](#external-api-proxy-v0120). |
| `POST` | `/api/auth/register` | — | Create an account; the **first** one becomes the admin. JSON **and** form bodies: JSON answers `201`, the form path logs the fresh account in (cookie + `303`). |
| `POST` | `/api/auth/login` | — | Exchange credentials for a Bearer token (+ refresh token while enabled). JSON **and** `x-www-form-urlencoded` bodies; both set the session cookie (form posts answer `303`). |
| `GET` | `/api/auth/me` | Bearer / cookie | The caller's profile (fresh from the repository). |
| `POST` | `/api/auth/refresh` | — | Rotate a refresh token: new access + refresh token, same session (refreshes the cookie). |
| `POST` | `/api/auth/logout` | Bearer / cookie | Revoke one refresh token's family (`204` for JSON clients, idempotent); browser forms get a lenient `303` back to the site. Both clear the session cookie. |
| `POST` | `/api/auth/logout_all` | Bearer / cookie | Revoke every refresh token of the caller (`204`); clears the session cookie. |
| `GET` | `/api/admin/users` | Bearer (admin) | List accounts, newest first. |
| `GET` | `/p` | — | The published CMS pages index (auto-routed view). |
| `GET` | `/p/{slug}` | — | One CMS page, rendered as `.jhs` (drafts answer `404` for the public). |
| `GET` | `/admin` (+ `/admin/pages`, `/admin/users`, forms) | cookie (editor / admin) | The CMS panel: pages CRUD, import from `public/`, account management (admin only). |
| `POST` | `/perfil/password` | Bearer / cookie | Self-service password change (requires the current one). |
| `GET` | `/metrics` | — | Prometheus text exposition (while `[metrics]` is on; path configurable). |
| `GET`/`HEAD` | `/<file>` | — | While `[static]` is on: files under `static.root_dir` (conditional + range requests supported). |

The auth and admin families are mounted only while `[auth]` is enabled;
the CMS family additionally requires `[database]`, `[auth]` and
`[templates]`; the external API proxy mounts while at least one
`[[external_api.endpoints]]` entry is configured and resolves; otherwise
those paths answer with the standard JSON `404`. The same applies
to the static family (`[static]`), the metrics endpoint (`[metrics]`) and
the refresh/logout endpoints (`auth.refresh_tokens_enabled`).

Example responses:

```json
// GET /api
{"service":"wallermax-server","version":"0.11.0","description":"...","endpoints":["GET / (static index + files)","GET /api","GET /health","GET /api/stats","POST /api/echo","GET /metrics","POST /api/auth/register","POST /api/auth/login","GET /api/auth/me","GET /api/admin/users","POST /api/auth/refresh","POST /api/auth/logout","POST /api/auth/logout_all"]}

// GET /health
{"status":"ok","version":"0.11.0","template_backend":"sidecar"}

// GET /api/stats
{"service":"wallermax-server","version":"0.11.0","uptime_seconds":10.244,"total_requests":12,"requests_per_second":1.171,"rate_limited_requests":0,"registered_users":2}

// POST /api/echo  (Content-Type: text/plain, body "hello wallermax")
{"received_bytes":15,"content_type":"text/plain","body":"hello wallermax"}

// POST /api/auth/register  -> 201
{"id":1,"username":"admin","role":"admin","created_at":1788646761,"last_login_at":null}

// POST /api/auth/login
{"access_token":"eyJhbGciOi...","token_type":"Bearer","expires_in":3600,"user":{"id":1,"username":"admin","role":"admin",...},"refresh_token":"cU9tY1...","refresh_expires_in":2592000}

// POST /api/auth/refresh  (same shape as login)
{"access_token":"eyJhbGciOi...","token_type":"Bearer","expires_in":3600,"user":{"id":1,...},"refresh_token":"dGhpcy...","refresh_expires_in":2592000}
```

Every failure mode answers with the structured JSON error envelope,
including the correlation id:

```json
// GET /nope -> 404
{"error":{"code":"NOT_FOUND","message":"No route matches GET /nope","request_id":"a132e585-f086-4bc3-b0b1-ab73bee23377"}}

// POST /health -> 405
{"error":{"code":"METHOD_NOT_ALLOWED","message":"Method POST is not allowed on /health","request_id":"..."}}

// GET /api/auth/me without a token -> 401
{"error":{"code":"UNAUTHORIZED","message":"missing `Authorization: Bearer <token>` header or session cookie","request_id":"..."}}

// GET /api/admin/users as a regular user -> 403
{"error":{"code":"FORBIDDEN","message":"this endpoint requires the admin role","request_id":"..."}}

// POST /api/auth/register with a taken username -> 409
{"error":{"code":"CONFLICT","message":"username is already taken","request_id":"..."}}

// slow handler -> 408
{"error":{"code":"REQUEST_TIMEOUT","message":"Request processing exceeded the 15 second limit","request_id":"..."}}

// oversized body -> 413
{"error":{"code":"PAYLOAD_TOO_LARGE","message":"Request body of 2000000 bytes exceeds the 1048576 byte limit","request_id":"..."}}

// over the rate limit -> 429 (+ Retry-After / X-RateLimit-* headers)
{"error":{"code":"RATE_LIMITED","message":"Rate limit exceeded; retry after 1 second(s)","request_id":"..."}}

// POST /api/auth/refresh with an unknown, expired, revoked or replayed
// token -> 401 (generic: no detail is leaked)
{"error":{"code":"UNAUTHORIZED","message":"invalid refresh token","request_id":"..."}}

// POST /api/auth/refresh with an oversized value -> 400
{"error":{"code":"BAD_REQUEST","message":"`refresh_token` must be at most 512 characters","request_id":"..."}}
```

## Dynamic templates (.jhs)

While `[templates] enabled = true` the server renders `.jhs` templates
(PHP-style JavaScript, ported from
[node-jhs2](https://github.com/Justo-Tapiador/node-jhs2)) inside a
[boa_engine](https://github.com/boa-dev/boa) sandbox. Since v0.9.0
templates can `require()` the `crypto` polyfill and pure-JS CommonJS
modules under `modules/` (guarded by the `forbidden_modules` banner and
jailed to that directory), call `res.redirect('/path')` for real HTTP
redirects, and read the sanitized `req` global — everything else stays
sandboxed with no host access: `GET /hello.jhs` renders
`public/hello.jhs` on the fly
(the template source is never served raw) and otherwise-unmatched
paths auto-route to the views directory (`GET /contacto` →
`views/contacto.jhs`, `GET /blog` → `views/blog/index.jhs`). API
routes keep their precedence, unresolved paths answer the JSON 404
envelope, template errors answer the JSON 500 envelope, and
`console.*` calls inside templates are routed to the structured logs.

The syntax, escaping rules, sandbox contract, configuration keys and
a template-writing guide live in
**[README-jhs-engine.md](README-jhs-engine.md)**.

Since v0.6.0 every render also receives the authenticated identity as
the `user` global: a valid `Authorization: Bearer` token **or
`wallermax_session` cookie** (verified — signature, expiry, issuer)
injects `user = { id, username, role }`, while anonymous visitors and
failed verifications render with `user = null`, so role-gated markup is
a plain `<?jhs if (user && user.role == 'admin') { ?>` block.
`[templates] expose_user = false` turns the injection off. Since v0.8.0
renders additionally receive `path` (the request path), `query` (the
query parameters) and `pages` (the published CMS pages), and templates
embed shared partials with `<?jhs include("partials/header") ?>` —
see [README-jhs-engine.md](README-jhs-engine.md).

## Template backends: boa and the Node sidecar (v0.10.0)

The `.jhs` language is stable; the *engine* behind it is a choice.
Since v0.10.0 every render in the process — public `.jhs` files,
auto-routed views and CMS page bodies — flows through one
`TemplateRenderer` seam with two implementations:

| | **boa** (the sandbox since v0.5.0) | **sidecar** (Node) |
|---|---|---|
| Runtime | in-process, `#![forbid(unsafe_code)]` | a supervised child process, `node` |
| Engine | the faithful Rust port on [boa_engine](https://github.com/boa-dev/boa) | **the original node-jhs2 2.1.0**, vendored in `sidecar/engine.js` (pinned, no npm install) |
| `require('url')`, `require('crypto')`, `require('path')` | polyfill / module banner | **real Node built-ins** (behind the same `forbidden_modules` banner) |
| `Buffer` | absent (sandboxed) | absent (documented divergence — see `sidecar/README.md`) |
| Fidelity | hardened port, documented divergences | byte-for-byte upstream semantics |
| Node.js needed | no | yes (18+; the Docker image ships it) |

Choose with `[templates] backend`:

| Value | Behaviour |
|-------|-----------|
| `auto` (the default) | the sidecar while Node is available; a **transparent fallback to boa** when the child dies, with an automatic recovery probe (~10 s cooldown) bringing it back. |
| `boa` | the in-process sandbox, zero Node — the hardened behaviour of v0.5.0–v0.9.0. |
| `sidecar` | strict: the server **refuses to start** without a live, selftested sidecar — a loud boot failure instead of mysterious 500s. |

```toml
[templates]
backend = "auto"        # auto (default) | boa | sidecar

[templates.sidecar]
node_command = "node"           # Node.js binary
script = "sidecar/jhs-sidecar.mjs"
workers = 2                     # render workers (one worker = one render)
startup_timeout_ms = 8000       # spawn + READY handshake + selftest
request_timeout_ms = 10000      # client-side timeout (> render_budget)
render_budget_ms = 5000         # hard-kill: runaway renders die here
```

What the sidecar adds on top of the engine itself:

- **Real `require()` built-ins** — `url`, `crypto`, `path`… resolve to
  actual Node modules behind the same `forbidden_modules` banner (the
  port's polyfills are not in play), and local CommonJS modules under
  `modules/` keep working.
- **Upstream `include()` semantics**, resolved at compile time exactly
  the way node-jhs2 resolves them.
- **Per-render isolation and capture** — a fresh worker context per
  render, `console.*` captured into the structured logs, and
  `res.redirect()` honoured as a real HTTP redirect.
- **A wall-clock hard-kill budget** (`render_budget_ms`): an infinite
  loop costs one worker, which is respawned — never the request
  handler, never the server.
- **A 13-check selftest at startup** (automatic escaping, the `raw()`
  sentinel, `console` capture, data escaping, `res.redirect` and the
  open-redirect rejection, `require` built-ins, the forbid banner, hot
  reload by mtime, the hard-kill, the worker respawn…): the backend is
  only accepted when everything passes.

**Is the sidecar actually serving?** Two one-liners (v0.10.1):

```console
$ curl http://127.0.0.1:8080/health
{"status":"ok","version":"0.11.0","template_backend":"sidecar"}

$ curl http://127.0.0.1:8080/metrics | grep template_backend
wallermax_template_backend{backend="sidecar"} 1
wallermax_template_backend{backend="boa"} 0
```

And one public page that only renders green through Node:

```console
$ curl "http://127.0.0.1:8080/sidecar-check.jhs?probe=7"
sidecar-ok:7            # boa would answer the module banner as a 500
```

Operational details — the loopback protocol, the timing-safe token
handshake, worker respawn, hardening notes and the documented
divergences — live in **[sidecar/README.md](sidecar/README.md)**; the
template language itself is
[README-jhs-engine.md](README-jhs-engine.md).

## Refresh tokens

Access tokens are short-lived and stateless; refresh tokens are how a
session survives past `token_ttl_secs` without weakening the access
tokens themselves. While `[auth] refresh_tokens_enabled = true` (the
default):

- **Login returns a refresh token** — a 43-character opaque base64url
  string encoding 256 bits of CSPRNG output. Only its **SHA-256 hash**
  is stored (in the `refresh_tokens` table, with the session `family_id`,
  `user_id`, expiry and best-effort audit fields `created_ip` /
  `user_agent`), so a database leak exposes no usable credentials.
- **`POST /api/auth/refresh` rotates it** — the presented token is
  retired (`rotated_at`), a successor is minted in the same family and a
  fresh access token is returned in the login response shape. Rotation is
  guarded by a compare-and-set update, so two concurrent refreshes with
  the same token cannot both win.
- **Reuse means theft: the family dies.** Presenting a token that was
  already rotated (or belongs to a revoked family) is the classic
  stolen-token signature, so it revokes *every* token in that family —
  the attacker and the legitimate user both lose the session, and the
  legitimate client falls back to a fresh login.
- **`POST /api/auth/logout`** (Bearer + `refresh_token` in the body)
  revokes that token's family; **`POST /api/auth/logout_all`** (Bearer)
  revokes every family of the caller. Both answer `204` and are
  idempotent — unknown or foreign tokens still answer `204`.
- **Housekeeping is free.** Every refresh call prunes expired tokens
  (best-effort), so the table does not grow without bound.

```text
login ──▶ refresh_token #1 (family F, hashed in SQLite)
              │
   POST /api/auth/refresh
              │ rotate (CAS): #1 retired, #2 issued ──▶ same session
              │
   replay #1  ──▶ 401 + revoke family F ──▶ #2 dead too (theft assumed)
```

Unknown, expired, revoked, empty or malformed tokens all answer the same
generic `401 invalid refresh token` (no detail leaks); only values longer
than 512 characters are rejected as `400` before touching the database.
Disable the whole mechanism with `refresh_tokens_enabled = false` to get
the access-token-only behaviour of earlier phases.

## Browser sessions (v0.7.0)

The Bearer header is perfect for API clients and impossible for a
browser: links, address bars and reloads cannot attach
`Authorization` headers. The session cookie closes that gap without
adding a second credential:

- **`POST /api/auth/login` and `/api/auth/refresh` also set the cookie.**
  `Set-Cookie: wallermax_session=<access_token>; Path=/;
  Max-Age=token_ttl_secs; HttpOnly; SameSite=Strict`. The cookie
  is a *mirror* of the access token, verified by the exact same
  `verify_token` path (signature, expiry, issuer).
- **`Secure` follows `[auth] secure_cookies` (v0.8.0).** `auto` (the
  default) attaches it only while `[tls]` is enabled: browsers refuse
  to store `Secure` cookies on plain-HTTP origins, so an unconditional
  `Secure` silently broke browser logins on HTTP-only dev servers
  (curl and PowerShell were lax about it, which made the failure look
  like a server bug — that was the v0.7.0 gotcha). `always` pins it for
  TLS-terminating proxies; `never` for HTTP-only local testing.
- **The cookie is accepted wherever the header is** — the
  `AuthUser`/`AdminUser` extractors, `/api/auth/me` and the `user`
  template global. When both are present the **Bearer header wins**, so
  existing clients are unaffected. Bad cookies degrade exactly like bad
  headers: `401` on API routes, anonymous render on templates.
- **`logout` / `logout_all` clear it** (`Max-Age=0`). Like every JWT
  logout, this ends the *browser's* session; an already-issued access
  token stays mathematically valid until `exp` — the reason
  `token_ttl_secs` should stay modest and refresh tokens carry the
  session length.

### The `/login` view, the modals and the browser forms

`GET /login` renders the shipped `views/login.jhs`: a plain HTML form
posting `username` / `password` / `redirect` to `/api/auth/login` as
`application/x-www-form-urlencoded` — **no JavaScript**, so the default
CSP (scripts stay blocked) allows it untouched. Since v0.8.0 every page
embedding the shared `views/partials/header.jhs` carries the same
forms inside CSS-only modals (`#login` / `#registrar`, opened with the
`:target` trick) plus the logout button. Form posts answer:

- **login success** → `303 See Other` + the session cookie, landing on
  the `redirect` field when it is a local path (`/…`, never `//host` or
  an absolute URL — off-site values fall back to `/`);
- **login failure** → `303` back to the same page with
  `?login_error=credenciales#login`, which re-opens the modal and shows
  the message (server-rendered; the code is fixed, never reflected);
- **registration (form)** → the account is created **and logged in
  directly** (cookie attached, `303`); failures bounce back with
  `?register_error=<code>#registrar`;
- **logout (form)** → lenient and idempotent: the cookie is cleared and
  the browser lands on a `303` — an absent or expired session clears
  the cookie without erroring, because a `Salir` click must never show
  a blank `204` page or a JSON envelope.

Cookie-authenticated visitors see the logged-in state instead: who they
are, links to their pages and the logout form in the header.

```text
browser                          server
   │ GET /                         │  renders views/index.jhs (user = null)
   │        [abre #registrar]      │  CSS :target modal, no JS
   │ POST /api/auth/register ────▶│  creates the account + logs it in
   │ ◀──── 303 / + Set-Cookie      │
   │ GET /  (cookie) ─────────────▶│  user = { id, username, role }
   │ ◀──── «admin · Salir»         │
   │ POST /api/auth/logout ──────▶│  clears the cookie
   │ ◀──── 303 /                   │
```

### CSRF posture

The cookie authenticates requests, so state-changing endpoints
(`logout`, `logout_all`, the admin API) become reachable from browsers —
and therefore CSRF-relevant. `SameSite=Strict` is the defence: modern
browsers never attach the cookie to **cross-site** requests, so a
hostile page cannot logout or act as the visitor. Remaining caveats,
deliberately accepted for this scope: a cross-site form can still
*login* the victim as the attacker (login-CSRF — nuisance-level here,
no sensitive actions exist), and `Secure`/`HttpOnly` protect the value
from scripts and plain-text networks. If you build browser-facing
mutations beyond logout, add per-request CSRF tokens rather than
relying on `SameSite` alone.

## The CMS (v0.8.0)

A small, deliberately boring content layer on top of the sessions:
the browser speaks plain HTML forms, the server is the only party that
touches SQLite, and the session cookie stays server-managed
(`HttpOnly`) — the "server as proxy" model.

### Roles

| Role | Sees | Manages |
|------|------|---------|
| `user` | published pages | nothing (own password via `/perfil`) |
| `editor` | + drafts (preview) | pages: create, edit, publish, delete, import |
| `admin` | everything | pages **and** accounts: create, roles, password resets, delete |

The **first** registered account bootstraps as `admin` (unchanged since
v0.4); registration forms always create plain `user` accounts.

### The public site

- `GET /p` — published pages index (auto-routed `views/p.jhs`, rendering
  the `pages` global);
- `GET /p/{slug}` — one page. The stored body is `.jhs` template source
  rendered through the same sandbox with the standard globals, so a page
  can personalise (`Hola <?= user ? user.username : "visitante" ?>`) and
  reuse the shared partials (`<?jhs include("partials/header") ?>`).
  Drafts answer `404` for the public and render with a preview banner
  for editors.

### The homepage: `default_page` (v0.11.0)

`[cms] default_page = "slug"` (env: `WALLERMAX_CMS__DEFAULT_PAGE`) makes
`GET /` render that CMS page through the **exact** `/p/{slug}` pipeline
— same sandboxed body render, same `views/cms_page.jhs` wrapper, same
globals; one shared renderer serves both routes — with three
deliberate properties:

- **Priority**: the explicit configuration beats `public/index.html`
  and the `views/index.jhs` auto-routing, and rendering directly (no
  redirect to `/p/{slug}`) keeps `/` itself the canonical URL.
- **Gating**: a draft default page follows the `/p/{slug}` rules — `404`
  for the public, preview banner for editors — so an unpublished
  homepage can never leak.
- **Graceful degradation**: a slug that stops existing (page deleted,
  or not created yet) logs a warning and the homepage falls back to the
  normal chain — static index file, then views auto-routing — instead
  of hard-failing. The takeover is live: create the page and `/` serves
  it, no restart.

The slug shape is validated at startup with the exact rule the panel
forms use (`1-64` characters of lowercase, digits and single dashes).
Setting `default_page` while `cms.enabled = false` is accepted (the
shape is still validated) but ignored, with a startup warning.

### The admin panel (`/admin`)

Pure HTML forms — **no JavaScript anywhere**, the CSP keeps scripts
blocked. Guards are browser-friendly: anonymous visitors get a `303` to
`/login?redirect=<panel path>`; privileged failures re-render the form
**with the submitted values and the error**, so nothing typed is ever
lost. Successes follow the post/redirect/get pattern.

- `/admin` — dashboard (counters and quick links);
- `/admin/pages` — every page, drafts included; create, edit
  (title/slug/body/publish), delete;
- `/admin/pages/import` — copy a `.html`/`.jhs` file from `public/` into
  a new **draft** (read-only on the static tree: the CMS never writes to
  `public/`; the original file keeps being served at its own path);
- `/admin/users` (admin only) — create accounts with a role, change
  roles, reset passwords, delete. Two guards keep the panel safe from
  itself: you cannot edit your **own** account there (use `/perfil`),
  and the **last admin** can never be demoted or deleted — even by a
  stale admin token whose role claim predates the demotion;
- `/perfil` — any session changes its own password (the current one
  must be presented).

### The privilege split

The panel manages **content and CMS accounts, nothing else**. Server
configuration (ports, TLS, secrets, middleware, the `[cms]` switch
itself) lives in `wallermax.toml` + environment variables, writable only
with local repository access to the machine that runs the server. That
is the deliberate separation between the *CMS administrator* and the
*server operator*.

## External API proxy (v0.12.0)

Browsers cannot keep a secret, and cross-origin `fetch` calls are bound
by the *target's* CORS policy. Both problems bite the moment a page
needs a third-party API: embedding the API key in the page publishes it
to every visitor, and the upstream may answer no CORS headers at all
(the browser then refuses to even read the response). The
`[external_api]` proxy solves both at once — the page calls a **named**
endpoint on this server and the server forwards the request upstream
with the secrets attached:

```toml
[external_api]
timeout_secs = 8
response_limit_bytes = 262144

[[external_api.endpoints]]
name = "weather"
url = "https://api.weather.example.com/v1/current"
auth_required = false

[external_api.endpoints.headers]
X-Api-Key = "${WEATHER_API_KEY}"
Accept = "application/json"
```

```js
// any page (a .jhs view, a CMS page body):
fetch('/api/ext/weather?city=Madrid')
  .then(r => r.json())
  .then(data => /* the upstream JSON, verbatim */);
```

The browser only ever talks to this origin, so upstream CORS stops
mattering entirely; the `X-Api-Key` header travels server-to-server and
never appears in any markup, script or dev-tools panel.

The family mounts only while at least one
`[[external_api.endpoints]]` entry is configured — the default
configuration has none, and `/api/ext/...` then answers the standard
JSON `404`. One entry is one route: `name` becomes the path segment
(`GET/POST /api/ext/{name}`), validated with the same slug rules the
CMS enforces (lowercase letters, digits, single dashes; unique). The
`url` must be an absolute `http`/`https` address and is **operator
territory**: the client picks a *name*, never a URL, so there is no
SSRF surface — no request can be steered towards an address that was
not explicitly configured.

### Secrets via `${ENV}` references

Header values may reference the environment with `${VAR_NAME}`,
expanded once at startup. A missing variable, an unterminated `${`, an
invalid name or an expansion containing control characters are all
**startup errors** — a proxy that would silently run without its
secrets is worse than no proxy, so the server refuses to boot instead
(see the startup log for the exact endpoint and variable at fault).
The committed `wallermax.toml` can therefore hold placeholders while
real keys live in the git-ignored `wallermax.local.toml` or in the
process environment:

```toml
# wallermax.local.toml (git-ignored) — real values, never committed
[external_api.endpoints.headers]      # merged over wallermax.toml
X-Api-Key = "real-key-from-the-dashboard"
```

Reserved header names (`host`, `content-length`, `connection`,
`transfer-encoding`, `cookie`) are rejected at validation — the HTTP
client owns them. Nothing from the incoming request is forwarded
upstream either: no cookies, no `Authorization` header, no arbitrary
client headers — only the configured headers, `User-Agent`
(`wallermax-server/<version>`) and `Accept` travel, so a session cookie
can never leak to a third party through the proxy.

### The request contract

| Concern | Behaviour |
|---------|-----------|
| Methods | `GET` and `POST` pass through (the upstream sees the same method). |
| Query string | Appended to the configured URL (`?city=Madrid` → upstream `...?city=Madrid`; `&` joins when the URL already has one). |
| `POST` bodies | Forwarded as-is while bounded by `server.max_body_size_bytes`, and only for text media types (`application/json`, `text/*`); anything else answers `400`. |
| Response status | Passes through verbatim — an upstream `429` reaches the browser as `429`. |
| Response body | Capped by `external_api.response_limit_bytes`; an oversized answer becomes a `502` envelope (reading stops at the cap, so a huge upstream costs bounded memory). |
| Media types | Only `application/json`, `application/*+json` and `text/*` are forwarded (parameters like `; charset=utf-8` included); binary answers become `502` — this is a JSON proxy, not a media one. |
| Auth | `auth_required = true` endpoints demand a valid Bearer token or session cookie (`401` otherwise); public endpoints still go through the global rate limiter and every middleware. |
| Failures | Transport errors and timeouts render as `502` envelopes with generic messages; the endpoint name and exact cause are logged server-side only. |

The usual pipeline still wraps every call: rate limiting (per client
IP, `429` + `Retry-After`), the request timeout, the request id, and
the security headers — the CSP of the page that made the call governs
what the browser may do with the answer, exactly as with any same-origin
response.

### Operating notes

Keep `external_api.timeout_secs` below
`server.request_timeout_secs` (the default 8 vs 15) so the proxy
answers the browser with its own `502` before the global timeout
truncates the request. Each call holds one connection from the pooled
`reqwest` client while waiting upstream, so `endpoints * workers` is
the rough worst case for concurrent upstream calls. The proxy is
stateless: no retry logic, no caching, no circuit breaker — if an
upstream flaps, the browser sees the flapping, which is usually the
honest answer for a dashboard-style consumer. Enabling `[cors]` for
other origins is *not* needed for the proxy itself (the calls are
same-origin by design); it only matters when a *different* site must
call this server directly.

## Prometheus metrics

Flip `[metrics] enabled = true` (on in the shipped `wallermax.toml`) and
`GET /metrics` (path configurable) serves the text exposition format:

| Metric                                   | Type      | Labels          |
|------------------------------------------|-----------|-----------------|
| `wallermax_requests_total`               | counter   | `method`, `code`|
| `wallermax_request_duration_seconds`     | histogram | `method`        |
| `wallermax_rate_limited_requests_total`  | counter   | —               |
| `wallermax_uptime_seconds`               | gauge     | —               |
| `wallermax_registered_users`             | gauge     | — (auth only)   |
| `wallermax_template_backend`             | gauge     | `backend`       |

`wallermax_template_backend{backend="sidecar"|"boa"}` (v0.10.1) is a
0/1 pair refreshed at scrape time: the backend currently serving
renders is `1`, the other `0` (both `0` while `[templates]` is
disabled). In `auto` mode a sidecar failure flips it to `boa` — if
Node-side rendering is a requirement for you, alert on
`wallermax_template_backend{backend="sidecar"} == 0`. It pairs with
the `template_backend` field of `GET /health`.

On Linux the standard `process_*` collectors (CPU, memory, file
descriptors) are registered as well. The request counters and the latency
histogram are fed by the logging middleware — keep
`[middleware] logging = true` (the default) or they will stay at zero.

The endpoint is unauthenticated by design (scrapers live on internal
networks): restrict it at the network layer when the server is exposed.
Scrapes are **exempt from rate limiting**, so a saturated server can
always be observed. Prometheus scrape config:

```yaml
scrape_configs:
  - job_name: wallermax
    scrape_interval: 15s
    metrics_path: /metrics
    static_configs:
      - targets: ["wallermax.example.com:8080"]
```

## TLS (HTTPS)

Enable `[tls]` and the same router is served through **rustls** (ring
provider, TLS 1.2 and 1.3 — no OpenSSL linkage) on `server.host:port`,
using the PEM certificate and key at `cert_path` / `key_path`:

```toml
[tls]
enabled = true
cert_path = "certs/cert.pem"
key_path = "certs/key.pem"

# Optional: keep an HTTP listener that redirects to HTTPS (308).
http_listen = "127.0.0.1:8080"
```

For development, generate a self-signed pair:

```console
$ openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
    -keyout certs/key.pem -out certs/cert.pem -subj "/CN=localhost"
```

(or use [mkcert](https://github.com/FiloSottile/mkcert) for
locally-trusted certificates). `certs/` is git-ignored and excluded from
Docker build contexts on purpose — mount certificate material at runtime
instead of baking it into images.

The optional `http_listen` address runs a second, plain-HTTP listener
that answers **every** request with `308 Permanent Redirect` to the HTTPS
equivalent (path and query preserved, so POSTs survive the hop). This
mirrors the production pattern of terminating nothing in front of the
server while still migrating users from `http://` to `https://`. HSTS is
already part of the default security headers.

## Trusted proxies and client IPs

The rate limiter and the refresh-token audit columns key on the *client*
IP, which is the TCP peer address by default. Behind a reverse proxy
that is wrong (every peer is the proxy), so `server.trusted_proxies`
teaches the server which peers may speak `X-Forwarded-For`:

```toml
[server]
trusted_proxies = ["10.0.0.4", "10.0.0.0/8", "fd00::/8"]
```

Entries are exact IPs or CIDR blocks (host bits rejected at validation).
Resolution follows the de-facto standard: walk `X-Forwarded-For` **right
to left**, skip trusted proxies, stop at the first untrusted address.
With an empty list (the default) the header is ignored entirely — an
untrusted client cannot spoof its bucket identity with a fake
`X-Forwarded-For`, and behind no proxy that is exactly what you want.

## Docker & CI

The repository ships a multi-stage `Dockerfile`:

- **Builder stage** — `rust:1.88-slim` (the pinned MSRV) compiles the
  release binary with `--locked`; dependencies are cached in their own
  layer so source-only changes rebuild in seconds.
- **Runtime stage** — `debian:bookworm-slim` with only the binary, the
  default configuration, `public/` and `sidecar/` — plus **Node.js**
  (from Debian), so the sidecar backend works in containers. It runs
  as an **unprivileged user** (`wallermax`, uid 10001); SQLite lives on
  the **`/data` volume**; the binary is PID 1 and handles `SIGTERM`
  gracefully (so `docker stop` drains in-flight requests).

```console
$ docker build -t wallermax-server .
$ docker run -p 8080:8080 -v wallermax-data:/data wallermax-server

# production: override the dev secret
$ docker run -p 8080:8080 -v wallermax-data:/data \
    -e WALLERMAX_AUTH__JWT_SECRET="$(openssl rand -hex 32)" \
    wallermax-server
```

The image sets `WALLERMAX_SERVER__HOST=0.0.0.0` and
`WALLERMAX_DATABASE__URL=sqlite:///data/wallermax.db?mode=rwc` as
container-friendly defaults; every other value is overridden with
ordinary `WALLERMAX_*` environment variables. There is no built-in
`HEALTHCHECK` (the image has no curl): probe `GET /health` from your
orchestrator. A `compose.yaml` example with a named volume is included.

`.github/workflows/ci.yml` runs on every push/PR:

- **lint** — `cargo fmt --check` + `cargo clippy --all-targets -D warnings`
  (Linux);
- **test** — `cargo test --locked` on **Linux and Windows** (the two
  platforms the project is developed against), with cargo caching;
- **docker** — builds the image (which doubles as the MSRV check, since
  the Dockerfile pins 1.88) and smoke-tests `/health`, `/metrics`, the
  static index **and the live Node sidecar** inside the running
  container: `sidecar-check.jhs` must answer `sidecar-ok:7` and the
  `wallermax_template_backend{backend="sidecar"} 1` gauge must be up
  (the boa sandbox cannot answer either — `sidecar-check.jhs` calls
  `require('url')`).

## Architecture

### Middleware pipeline

```text
request   ->  [security headers] -> [cors] -> [request id] -> [logging]
              -> [rate limit] -> [body limit] -> [timeout] -> routes / handlers
response  <-  [security headers] <- [cors] <- [request id] <- [logging]
              <- [rate limit] <- [body limit] <- [timeout] <- routes / handlers
```

- **Security headers** is the outermost layer on purpose: every response —
  including 404/405/408/413/429 — carries the configured security headers.
- **CORS** sits outside logging, so preflight requests are answered directly
  without polluting logs or stats.
- **Request id** attaches a correlation id before the feature middlewares so
  error bodies can embed it.
- **Logging** emits one structured `INFO` line per request (method, path,
  status, latency, request id), adds `X-Response-Time` and feeds the
  `/api/stats` counters.
- **Rate limit** enforces per-client-IP token buckets (proxy-aware via
  `server.trusted_proxies`; rejections add `Retry-After` and
  `X-RateLimit-*` headers and are counted in stats; Prometheus scrapes
  are exempt).
- **Body limit** combines an early `Content-Length` check with tower-http's
  stream enforcement (chunked bodies included).
- **Timeout** bounds total handling time per request (`408` on expiry).

Each layer is registered in `src/middleware/mod.rs::apply` and toggled
through `[middleware]` in the configuration.

### Module map

| Module | Responsibility |
|--------|----------------|
| `src/config.rs` | Layered configuration + validation. |
| `src/state.rs` | Cheap-clone shared state (config, limiter, auth, metrics, proxies). |
| `src/error.rs` | `AppError` model → consistent JSON error envelope. |
| `src/rate_limit.rs` | Token-bucket rate limiter (per client IP). |
| `src/proxy.rs` | Trusted proxies, CIDR matching, `X-Forwarded-For` resolution. |
| `src/external_api.rs` | `[external_api]` proxy state: endpoint table, `${ENV}` header resolution, pooled upstream client. |
| `src/auth.rs` | Argon2id hashing, JWT access tokens, refresh token primitives. |
| `src/db.rs` | SQLite pool, embedded migrations, `UserRepository` trait + impl. |
| `src/metrics.rs` | Prometheus registry and exposition. |
| `src/template_engine/` | `.jhs` parsing, the boa sandbox, the `TemplateRenderer` seam and the Node sidecar client. |
| `src/extractors.rs` | `AuthUser` / `AdminUser` / `JsonBody` request extractors. |
| `src/routes/*` | One module per feature area, merged in `routes/mod.rs`. |
| `src/middleware/*` | One module per middleware, composed in `middleware/mod.rs`. |
| `src/logging.rs` | `tracing` subscriber (level + format). |
| `src/server.rs` | `build_state()` + `build_app()` assembly, HTTP/HTTPS loops, shutdown. |

### The Repository pattern

Handlers never talk to SQLite. They depend on the `UserRepository` trait
(`create`, `find_by_id`, `find_by_username`, `count`, `list`,
`record_login` plus the refresh-token store: `save_refresh_token`,
`find_refresh_token_by_hash`, `rotate_refresh_token`,
`revoke_refresh_token_family`, `revoke_all_refresh_tokens`,
`delete_expired_refresh_tokens`), which keeps the HTTP layer
storage-agnostic:

```text
routes/auth.rs ──▶ UserRepository (trait) ◀── SqliteUserRepository (sqlx)
extractors.rs          │                        migrations/0001_create_users.sql
                       └── test stubs / future engines (Postgres, ...)
```

State initialization happens once, in `server::build_state`: connect the
pool → run the embedded migrations → assemble the `AuthContext`
(repository + JWT service + policy flags) → build the app with the auth
routes mounted. Integration tests use the exact same function.

### Static file serving

While `[static]` is enabled, route resolution works in three tiers:

```text
1. API routes keep precedence: /health, /api/* always win.
2. GET / is an explicit route to <root_dir>/<index_file> (ServeFile:
   conditional requests, ranges, Last-Modified).
3. Everything else falls through to ServeDir over root_dir:
   /style.css -> root_dir/style.css, /docs/ -> root_dir/docs/index.html;
   misses (any method) -> the standard JSON 404 envelope.
```

`ServeDir` resolves paths itself and rejects `..` segments and
percent-encoded traversals before touching the filesystem, so requests
staying inside the root is enforced by the file service, not by convention.
The `GET /` route being explicit means `POST /` answers the JSON `405`
envelope like any other known route. There is no directory listing:
`/docs/` only resolves to `docs/index.html`, never to an index of files.

### Request authentication flow

```text
Authorization: Bearer <token>   ── or (browsers) ──  Cookie: wallermax_session=<token>
        │                                              │
        └────────────────────┬─────────────────────────┘
                             ▼
AuthUser extractor: verify signature (HS256) → check exp (+30s leeway)
        → check issuer → parse sub/role → typed identity
        │                                       │
        ▼                                       ▼
 handler receives AuthUser            401/403 JSON envelope
 (or AdminUser, which also            with the request_id
  enforces the admin role)
```

Tokens are stateless: any worker verifies them without a database round
trip. Handlers that must react to deleted accounts (like `/api/auth/me`)
re-query the repository. The header takes precedence when both it and
the session cookie are present.

### Adding a route module

```rust
// src/routes/ping.rs
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
struct PingResponse { pong: bool }

async fn ping() -> Json<PingResponse> {
    Json(PingResponse { pong: true })
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/ping", get(ping))
}
```

```rust
// src/routes/mod.rs — two lines and nothing else changes
pub mod ping;
// ...
    .merge(ping::routes())
```

### Adding a middleware

```rust
// src/middleware/api_version.rs
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::AppState;

pub async fn run(_state: State<AppState>, request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        axum::http::HeaderName::from_static("x-api-version"),
        axum::http::HeaderValue::from_static("1"),
    );
    response
}
```

Register it in `middleware::apply` (respecting the reverse-application order
documented there), optionally adding a boolean switch to
`config::MiddlewareConfig` and `wallermax.toml`.

## Project structure

```text
wallermax-server/
├── Cargo.toml            # dependencies + release profile (MSRV 1.88)
├── Cargo.lock            # resolved, committed, CI-protected
├── wallermax.toml        # main configuration (versioned)
├── Dockerfile            # multi-stage build (builder + minimal runtime)
├── compose.yaml          # minimal deployment example
├── .dockerignore         # keeps build contexts lean and secrets out
├── .github/workflows/ci.yml  # fmt + clippy + test matrix + docker smoke
├── migrations/           # embedded SQLite migrations
│   ├── 0001_create_users.sql
│   └── 0002_create_refresh_tokens.sql
├── public/               # static site root ([static] root_dir)
│   ├── index.html        #   the page served at GET / ("Hello, world!")
│   ├── hello.jhs         #   demo template, rendered at GET /hello.jhs
│   └── sidecar-check.jhs #   public sidecar probe: green only through Node
├── views/                # view templates ([templates] views_dir)
│   ├── index.jhs         #   root fallback when public/index.html is absent
│   ├── contacto.jhs      #   auto-routed at GET /contacto
│   ├── perfil.jhs        #   GET /perfil — the `user` global demo
│   └── login.jhs         #   GET /login — the no-JavaScript login form
├── sidecar/              # the Node sidecar ([templates] backend = sidecar | auto)
│   ├── jhs-sidecar.mjs   #   loopback HTTP service: token, worker pool, respawn
│   ├── render-worker.mjs #   one worker = one render (require bridge, capture)
│   ├── engine.js         #   vendored node-jhs2 2.1.0 (pinned, no npm install)
│   └── README.md         #   sidecar operations guide
├── LICENSE               # MIT
├── README.md
├── README-jhs-engine.md  # the .jhs template engine guide
├── src/
│   ├── main.rs           # thin binary entry point
│   ├── lib.rs            # module map + run()
│   ├── config.rs         # layered configuration
│   ├── state.rs          # shared application state (+ AuthContext)
│   ├── error.rs          # JSON error model
│   ├── rate_limit.rs     # token-bucket limiter
│   ├── proxy.rs          # trusted proxies + X-Forwarded-For resolution
│   ├── auth.rs           # Argon2id + JWT + refresh token primitives
│   ├── db.rs             # SQLite pool, migrations, UserRepository
│   ├── session.rs        # the wallermax_session cookie helpers
│   ├── metrics.rs        # Prometheus registry + exposition rendering
│   ├── extractors.rs     # AuthUser / AdminUser / JsonBody
│   ├── logging.rs        # tracing setup
│   ├── server.rs         # bootstrap, TLS, graceful shutdown
│   ├── util.rs           # tiny shared helpers
│   ├── template_engine/  # .jhs engine: parser + boa sandbox + renderer trait + sidecar client
│   ├── routes/           # one file per feature area (index, health, stats,
│   │                     #   echo, auth, admin, static_files, metrics)
│   └── middleware/       # one file per middleware (incl. templates.rs)
└── tests/
    ├── common/mod.rs     # shared TestServer scaffolding (+ temp DB + TLS
    │                     #   test server with generated certificates)
    ├── http_api.rs       # end-to-end HTTP integration tests (core)
    ├── security.rs       # phase 2 security integration tests
    ├── auth.rs           # phase 3 auth integration tests
    ├── static_files.rs   # static file serving integration tests
    ├── refresh_tokens.rs # rotation, reuse detection, logout flows
    ├── metrics.rs        # Prometheus exposition integration tests
    ├── proxy.rs          # X-Forwarded-For + rate-limit interaction tests
    ├── tls.rs            # HTTPS serving + redirect listener tests
    ├── templates.rs       # .jhs HTTP contract integration tests
    ├── template_fidelity.rs  # engine fidelity vs the original node-jhs2
    ├── sidecar.rs        # Node sidecar battery — spawn, selftest, parity,
    │                      #   hard-kill, respawn (skipped without node)
    └── config_env.rs     # environment override semantics
```

## Testing

```console
$ cargo test
running 252 tests ... ok      # unit tests (config, state, error, limiter,
                               #   proxy CIDRs, auth + refresh primitives,
                               #   repository incl. refresh token store and
                               #   the page repository, metrics registry
                               #   incl. the template backend gauge,
                               #   server helpers, middleware, template
                               #   engine + parser + include() + session
                               #   cookie helpers, login helpers, CMS
                               #   validation helpers incl. the
                               #   default_page slug rule, sidecar client,
                               #   external_api config validation +
                               #   ${ENV} expansion)
running 5 tests ... ok          # config_env: environment override semantics
                               #   (incl. cms.default_page)
running 30 tests ... ok         # auth: register/login/profile/admin flows,
                               #   the session cookie (set/authenticate/
                               #   rotate/clear, Bearer precedence) and the
                               #   browser form flows (303, open-redirect
                               #   rejection, error redirects, idempotent
                               #   form logout)
running 29 tests ... ok         # cms: the cookie-jar "browser" battery —
                               #   guards (anon/user/editor/admin), page
                               #   lifecycle over forms, drafts, imports,
                               #   .jhs page bodies (globals + include),
                               #   account management, last-admin lockout,
                               #   modals, password change, cookie flags,
                               #   and the v0.11.0 default_page homepage
                               #   (takeover, priority over the static
                               #   index, draft gating, fallback, off mode)
running 12 tests ... ok         # refresh_tokens: rotation, reuse detection,
                               #   family revocation, logout/logout_all,
                               #   expiry, disabled mode, multi-rotation
running 11 tests ... ok         # http_api: real server + real HTTP requests
                               #   (+ the health probe's template_backend)
running 14 tests ... ok         # external_api: the proxy battery — a stub
                               #   upstream on an ephemeral port: JSON/text
                               #   pass-through with statuses and media
                               #   types, configured headers travel while
                               #   client headers do not, query and POST
                               #   forwarding, ${ENV} secrets, auth_required
                               #   gating (401 vs session), oversized and
                               #   binary answers becoming 502, upstream
                               #   timeouts, the unmounted default
running 15 tests ... ok         # security: rate limit, CORS, body limit, ...
running 8 tests ... ok          # metrics: exposition format, headers, path,
                               #   scrape exemption, registered users gauge,
                               #   the template backend gauge vs /health
running 6 tests ... ok          # proxy: XFF parsing, trust boundaries,
                               #   per-client buckets via forwarded IPs
running 16 tests ... ok         # static_files: index, assets, 404/405,
                               #   traversal, conditional + range requests
running 6 tests ... ok          # tls: HTTPS serving, auth over TLS,
                               #   strict-client rejection, 308 redirects
running 1 test ... ok           # template_fidelity: 20-case battery vs the
                               #   original node-jhs2 engine
running 37 tests ... ok         # templates: rendering, view auto-routing,
                               #   precedence, error envelopes, mtime
                               #   reload, loop bounds, disabled mode,
                               #   cookie personalisation + /login view
running 11 tests ... ok         # sidecar: the Node sidecar battery — spawn
                               #   + selftest, renderer semantics, require
                               #   built-ins, the hard-kill budget, worker
                               #   respawn, strict/auto behaviour and the
                               #   boa/sidecar HTTP parity (skipped without
                               #   node on PATH)
```

**453 tests total**, all of them plain `cargo test` (no docker, no
network). The sidecar battery needs `node` on `PATH` and skips
gracefully otherwise — mirroring the `auto` backend's fallback. The
integration tests boot the exact same application the
binary serves (`server::build_state` + `server::build_app`, with
`build_app_with_routes` available for injecting custom routes) on an
ephemeral port and assert on real HTTP responses: status codes, JSON
bodies, security headers, rate-limit headers, CORS behaviour, request-id
policy, the JSON error envelope, middleware toggles, registration/login/
profile/admin flows, token forgery/expiry rejections, refresh rotation
and reuse detection, static file serving (index, nested assets, directory
indexes, conditional and range requests, traversal rejection), Prometheus
exposition semantics, trusted-proxy resolution and persistence across
restarts. The CMS battery drives a cookie-storing `reqwest` client
(Set-Cookie in, Cookie out — the closest thing to a browser a test can
get) through the whole panel. The TLS tests generate a fresh self-signed
certificate per run
(rcgen) and boot the production `axum-server` + rustls path over HTTPS.
Each auth test gets a fresh temporary SQLite file, and each static test a
fresh temporary asset directory, cleaned up afterwards.

## Security notes

- **Passwords are Argon2id-hashed** (PHC format, fresh random salt, default
  parameters: 19 MiB / 2 iterations). Verification is constant-time.
- **JWT secrets must be at least 32 characters**; the configuration layer
  rejects shorter ones at load time. The `wallermax.toml` value is a
  committed **development** secret — in production provide
  `WALLERMAX_AUTH__JWT_SECRET` (or `wallermax.local.toml`) instead.
- **Login failures are indistinguishable.** Unknown usernames and wrong
  passwords produce the same error code and message, and the
  unknown-username path burns a comparable Argon2 verification so timing
  cannot enumerate users either.
- **The session cookie is defence-in-depth wrapped.** It is `HttpOnly`
  (invisible to `document.cookie`), `Secure` (HTTPS-only storage —
  browsers reject it from plain HTTP, and the project serves TLS by
  design), `SameSite=Strict` (never attached to cross-site requests,
  which is the CSRF posture for cookie-authenticated `POST`s) and
  short-lived (`Max-Age = token_ttl_secs`). It mirrors the access token
  and is verified identically; the Bearer header always wins when both
  are present. See [Browser sessions](#browser-sessions-v070) for the
  remaining caveats (login-CSRF, logout vs unexpired tokens).
- **Token lifecycle is deliberate and layered.** Access tokens are HS256,
  carry `sub`/`username`/`role`/`iat`/`exp`/`iss`, are verified with a 30
  second expiry leeway, and are rejected with the wrong issuer or a bad
  signature. Refresh tokens are 256-bit CSPRNG values shown to the client
  once and stored **only as SHA-256 hashes**; they rotate on every use,
  and replaying a rotated token revokes the entire session family
  (stolen-token containment). Expired refresh tokens are pruned on every
  refresh call. Keep `token_ttl_secs` modest (minutes) and let the
  refresh token carry the session length.
- **The first registered account becomes the admin.** On a fresh database,
  register your own admin immediately, then (optionally) disable
  self-registration with `auth.registration_enabled = false`.
- **Rate limiting keys on the client IP.** Direct connections use the TCP
  peer address; behind a reverse proxy, list it in
  `server.trusted_proxies` (IP or CIDR) and the client address is
  recovered from `X-Forwarded-For` with a right-to-left trust walk. The
  list is empty by default, so spoofed `X-Forwarded-For` values from
  untrusted clients are ignored and cannot evade throttling by rotating
  fake IPs.
- **CORS is disabled by default.** Browsers then deny all cross-origin reads,
  which is the safe posture for an API. Enable it only with an explicit
  allowlist, and prefer specific origins over the `*` wildcard.
- **Body limits protect memory.** `413` responses are issued before a body
  is buffered (declared length) and during streaming (chunked bodies).
  Passwords are additionally capped at 128 bytes so clients cannot waste
  Argon2 CPU on giant inputs.
- **Security headers are strict by default.** The default CSP (`default-src
  'none'; frame-ancestors 'none'`) is meant for pure JSON APIs and also
  applies to static pages: the shipped `public/index.html` is plain HTML on
  purpose (no scripts, styles or images). If you serve a real site, relax
  `security_headers.content_security_policy` to allow exactly the assets
  you use (e.g. `default-src 'self'`).
- **Static file requests cannot escape the root.** `..` segments and
  percent-encoded traversals are rejected by the file service before any
  filesystem access; the root directory must exist at startup (a missing
  one is a hard startup error, a missing index file only logs a warning).
  There is no directory listing.
- **HSTS is sent even over plain HTTP** so the policy is in place the moment
  TLS is enabled; browsers only honour it on secure connections.
- **TLS is rustls-only** (ring provider): no OpenSSL linkage, no
  certificate verification callbacks to get wrong, TLS 1.2/1.3 only.
  Certificate files are read at startup and a missing/unreadable pair is
  a hard startup error; `certs/` is git-ignored and excluded from Docker
  build contexts.
- **The metrics endpoint is unauthenticated by design** (scrapers live on
  internal networks). When the server is exposed, restrict `/metrics` at
  the network layer — bind address, firewall rules or reverse-proxy
  allowlists. Scrapes bypass rate limiting on purpose so monitoring
  survives abuse.
- **The container runs as an unprivileged user.** The Docker image drops
  to `wallermax` (uid 10001), keeps SQLite on the `/data` volume owned by
  that user, and expects secrets through `WALLERMAX_*` environment
  variables rather than baked-in files.

## Roadmap

### Phase 1 — Core (done)

- [x] Layered configuration (defaults, `TOML`, env) with validation.
- [x] Modular route registry with JSON fallback for 404s.
- [x] Middleware pipeline: request-id, logging/timing, security headers, timeout.
- [x] Runtime metrics endpoint (`GET /api/stats`).
- [x] Graceful shutdown with hard timeout.
- [x] Unit + end-to-end integration tests, clippy-clean, `#![forbid(unsafe_code)]`.

### Phase 2 — Security hardening (done)

- [x] Rate limiting per client IP (token bucket, configurable limits).
- [x] CORS with configurable allowlist.
- [x] Request body size limits (declared + streamed).
- [x] Strict client request-id policy (`accept` / `overwrite`).
- [x] Configurable security header values.
- [x] Consistent JSON error envelope across 404/405/408/413/429.

### Phase 3 — Authentication & persistence (done)

- [x] JWT authentication (`/api/auth/register`, `/api/auth/login`,
      `/api/auth/me`) with role-based admin endpoints.
- [x] Users and roles (`admin` / `user`, first user bootstraps the admin).
- [x] SQLite storage via `sqlx` behind the `UserRepository` trait, so other
  engines can be swapped in without touching business logic.
- [x] Embedded migrations (single-binary deployment, WAL mode, busy
  timeout).

### Phase 4 — Production extras (done)

- [x] Static file serving (`[static]` section, `public/` root, conditional
      and range requests, traversal rejection, JSON 404 for missing files).
- [x] Refresh tokens with rotation and family revocation
      (`/api/auth/refresh`, `/api/auth/logout`, `/api/auth/logout_all`,
      hashed storage, reuse detection).
- [x] Prometheus metrics (`/metrics`, configurable path, request/latency/
      rate-limit/uptime/user families, scrape exemption).
- [x] TLS via `rustls` (`[tls]` section, optional HTTP→HTTPS `308`
      redirect listener).
- [x] Trusted proxy support (`server.trusted_proxies`, CIDR matching,
      right-to-left `X-Forwarded-For` resolution).
- [x] Docker image (multi-stage, non-root, `/data` volume) + compose
      example + `.dockerignore`.
- [x] GitHub Actions CI (fmt, clippy, Linux + Windows test matrix, Docker
      build with in-container smoke test).

### Phase 5 — Dynamic templates (done)

- [x] `.jhs` template engine: a faithful port of
      [node-jhs2](https://github.com/Justo-Tapiador/node-jhs2) to Rust
      on top of [boa_engine](https://github.com/boa-dev/boa) — see
      [README-jhs-engine.md](README-jhs-engine.md).
- [x] Sandbox hardening: `require()` as a native bridge (banner,
      polyfills, modules jailed to `modules/`), no
      `Buffer`/fs-outside-modules/network, fresh context per
      render, loop iteration limits, captured `console.*` → tracing.
- [x] HTTP integration: on-the-fly rendering under `[static]`, views
      auto-routing with API precedence, JSON error envelopes, mtime
      cache invalidation, `spawn_blocking` renders.
- [x] 20-case fidelity battery against the original engine plus 25
      HTTP integration tests (354 total across the suite).
- [x] v0.6.0: template data — the verified identity injected as the
      `user` global (`{ id, username, role }`, `null` for anonymous),
      `expose_user` switch, bad tokens degrade to anonymous renders.
- [x] v0.7.0: browser sessions — the `wallermax_session` cookie
      (`HttpOnly`, `SameSite=Strict`, `Secure` per `secure_cookies`)
      set by login/refresh,
      accepted by the extractors and the `user` global, cleared by
      logout; the JavaScript-free `/login` view (form posts → `303`
      with open-redirect validation); the shipped `hello.jhs` demo
      fixed (`req` → the real `user` global).

### Phase 6 — The CMS (done, v0.8.0)

- [x] Content in SQLite behind the `PageRepository` trait (migration
      0003: slug/title/body/published/author/audit timestamps).
- [x] Public site: `GET /p` (published index) and `GET /p/{slug}` —
      page bodies are `.jhs` templates rendered with the standard
      globals, drafts are editors-only.
- [x] Admin panel (`/admin`) as plain HTML forms — pages CRUD,
      import-from-`public/` (read-only on the static tree), account
      management with the self-edit and last-admin guards.
- [x] Roles: `editor` (content) and `admin` (content + accounts) on top
      of `user`.
- [x] Self-service password change (`/perfil/password`, requires the
      current one).
- [x] Engine additions: compile-time `include()` for shared partials
      (path-validated, depth/size-bounded, mtime-invalidated), the
      `path`/`query`/`pages` template globals, `echo(raw())` fixed to
      honour the sentinel.
- [x] Browser UX: login/registration modals (CSS `:target`, zero
      JavaScript), form registration with auto-login, error redirects
      that re-open the modal, idempotent form logout, `secure_cookies`
      (auto/always/never) fixing the plain-HTTP `Secure` gotcha.

### Phase 7 — Template backends (done, v0.10.0)

- [x] The `TemplateRenderer` trait: one seam for public `.jhs` files,
      auto-routed views and CMS page bodies alike; `JhsEngine` (boa),
      `SidecarRenderer` (Node) and `AutoRenderer` (sidecar with
      transparent fallback) behind it, selected by
      `[templates] backend = auto | boa | sidecar`.
- [x] The Node sidecar: a supervised child with a READY handshake and a
      timing-safe token over loopback HTTP (std-only client — zero new
      Rust dependencies), a worker pool with wall-clock hard-kill and
      respawn, running the **original node-jhs2 2.1.0** vendored in
      `sidecar/engine.js` (pinned, no npm install), real `require()`
      built-ins behind the `forbidden_modules` banner, per-render
      `console.*` capture and `res.redirect()`.
- [x] A 13-check startup selftest gates backend acceptance; strict
      mode refuses to boot without a live sidecar; `auto` falls back to
      boa and probes for recovery.
- [x] v0.10.1: observability — the live backend in `GET /health` and
      the `wallermax_template_backend` Prometheus gauge; the Docker
      runtime image ships Node.js and CI's container smoke expects the
      sidecar answering.

### Phase 8 — The CMS homepage (done, v0.11.0)

- [x] `[cms] default_page`: `GET /` renders the named CMS page through
      the exact `GET /p/{slug}` pipeline (one shared renderer behind
      both routes), ahead of `public/index.html` and the views
      auto-routing; rendered directly, so `/` stays the canonical URL.
- [x] Same draft gating as `/p/{slug}` (`404` for the public, preview
      banner for editors); a missing slug warns and falls back to the
      normal homepage chain — the takeover is live, no restart needed.
- [x] The slug shape is validated at startup with the panel forms' exact
      rule; `WALLERMAX_CMS__DEFAULT_PAGE` env override; ignored (with a
      startup warning) while the CMS is off.

### Beyond the roadmap (ideas)

- [ ] CMS page revisions (history + restore) and scheduled publishing.
- [ ] Account deactivation (soft disable) next to deletion.
- [ ] CSRF tokens for the admin forms on top of `SameSite=Strict`.
- [ ] Request tracing spans and OpenTelemetry export.
- [ ] Graceful config reload (SIGHUP) and admin API.
- [ ] Alternative repositories (Postgres) behind `UserRepository`.

## Contributing

Pull requests are welcome. Please keep the codebase `cargo fmt` clean,
`cargo clippy` warning-free, and add tests for any new behaviour.

## License

[MIT](LICENSE)
