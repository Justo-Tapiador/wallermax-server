# wallermax-server

[![Rust](https://img.shields.io/badge/Rust-1.88%2B-orange?logo=rust)](https://www.rust-lang.org)
[![Built with Axum](https://img.shields.io/badge/Built%20with-Axum%200.8-blueviolet)](https://github.com/tokio-rs/axum)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue)](LICENSE)
[![Roadmap](https://img.shields.io/badge/Roadmap-All%205%20phases%20done-green)](#roadmap)

<div align="left">
<p><img src="ws.png" width="482" alt="IA-SO BROODER"></p>
</div>

> A modular, secure and high-performance web server written in Rust.

**Status: v0.6.0 — the four roadmap phases plus a dynamic template engine.** Phase 4 added rotating
refresh tokens with family revocation, a Prometheus `/metrics` endpoint, HTTPS via
rustls (plus an HTTP-to-HTTPS redirect listener), trusted-proxy `X-Forwarded-For`
parsing, a multi-stage Docker image with a compose example and a GitHub Actions CI
pipeline. v0.5.0 added the sandboxed `.jhs` template engine, and v0.6.0 injects the
authenticated identity into every render as the `user` global — see
[README-jhs-engine.md](README-jhs-engine.md) and the [roadmap](#roadmap).

## Table of contents

- [Features](#features)
- [Requirements](#requirements)
- [Getting started](#getting-started)
- [Configuration](#configuration)
- [HTTP API](#http-api)
- [Dynamic templates (.jhs)](#dynamic-templates-jhs)
- [Refresh tokens](#refresh-tokens)
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
  hashing; role-based access (`admin` / `user`) enforced by typed extractors.
- **Static files** — serve a website from a directory (`GET /` answers
  `public/index.html`); conditional requests (`304`), range requests
  (`206`), correct content types, path-traversal rejection and the JSON
  404 envelope for missing files.
- **Dynamic `.jhs` templates** — a PHP-style JavaScript template engine
  (a faithful port of [node-jhs2](https://github.com/Justo-Tapiador/node-jhs2))
  running in a [boa_engine](https://github.com/boa-dev/boa) sandbox:
  no `require`, no file system, no network; escaped output by default,
  loop-iteration bounds, mtime-based recompilation, view auto-routing
  and `console.*` routed to the structured logs. See
  [README-jhs-engine.md](README-jhs-engine.md).
- **Refresh tokens** — long-lived opaque sessions (256-bit, stored only as
  SHA-256 hashes) with rotation on every refresh and automatic family
  revocation when a retired token is replayed; `POST /api/auth/logout`
  and `logout_all` end one or all sessions.
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
- **Tested** — 202 tests: unit tests per module plus end-to-end integration
  tests that boot the *real* server (plain HTTP and HTTPS) and speak HTTP
  to it.

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
- No other runtime requirements.

## Getting started

```console
$ cargo run
   Compiling wallermax-server v0.6.0
    Finished dev [unoptimized + debuginfo] target(s)
     Running `target/debug/wallermax-server`

INFO wallermax_server::server: static file serving enabled root_dir=public index_file=index.html
INFO wallermax_server::server: dynamic template rendering enabled views_dir=views auto_escape=true cache=true
INFO wallermax_server::server: sqlite pool ready (migrations applied) url=sqlite://wallermax.db?mode=rwc max_connections=5
INFO wallermax_server::server: authentication enabled (the first registered user becomes the admin) registration_enabled=true token_ttl_secs=3600 refresh_tokens_enabled=true refresh_token_ttl_secs=2592000
INFO wallermax_server::server: prometheus metrics enabled path=/metrics
INFO wallermax_server::server: wallermax-server listening address=127.0.0.1:8080 version=0.5.0
INFO wallermax_server::server: route map ready routes="GET / (static) | GET /api | GET /health | GET /api/stats | POST /api/echo | GET /metrics | POST /api/auth/register | POST /api/auth/login | GET /api/auth/me | POST /api/auth/refresh | POST /api/auth/logout | POST /api/auth/logout_all | GET /api/admin/users | + static files | + .jhs templates"
```

The default `wallermax.toml` ships with static files, the database and
authentication enabled, so the very first registration becomes the admin.
Open `http://127.0.0.1:8080/` in a browser to see the served page
(`public/index.html` — edit or replace it freely):

```console
$ curl http://127.0.0.1:8080/
<!DOCTYPE html>
<html lang="en">
...
<h1>Hello, world!</h1>
...

$ curl http://127.0.0.1:8080/api
{"service":"wallermax-server",...,"endpoints":["GET / (static index + files)","GET /api","GET /health","GET /api/stats","POST /api/echo","GET /metrics","POST /api/auth/register","POST /api/auth/login","GET /api/auth/me","GET /api/admin/users","POST /api/auth/refresh","POST /api/auth/logout","POST /api/auth/logout_all"]}

$ curl http://127.0.0.1:8080/health
{"status":"ok","version":"0.5.0"}

$ curl http://127.0.0.1:8080/hello.jhs     # .jhs template, rendered on the fly
<h1>Hola desde una plantilla .jhs</h1>
...                                       # demo; see README-jhs-engine.md

$ curl http://127.0.0.1:8080/metrics | head -4
# HELP wallermax_requests_total Requests served, by HTTP method and response status code.
# TYPE wallermax_requests_total counter
wallermax_requests_total{code="200",method="GET"} 3
...

$ curl -X POST http://127.0.0.1:8080/api/auth/register \
    -H "Content-Type: application/json" \
    -d '{"username":"admin","password":"correct-horse-battery"}'
{"id":1,"username":"admin","role":"admin","created_at":1788646761,"last_login_at":null}   # 201 Created

$ curl -X POST http://127.0.0.1:8080/api/auth/login \
    -H "Content-Type: application/json" \
    -d '{"username":"admin","password":"correct-horse-battery"}'
{"access_token":"eyJhbGciOi...","token_type":"Bearer","expires_in":3600,"user":{...},"refresh_token":"cU9tY1...","refresh_expires_in":2592000}

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
| `metrics.enabled` | bool | `false` (defaults) / `true` (wallermax.toml) | Serve the Prometheus exposition endpoint. |
| `metrics.path` | string | `/metrics` | Path of the exposition endpoint. |
| `tls.enabled` | bool | `false` | Serve HTTPS (rustls) on `server.host:port` instead of plain HTTP. |
| `tls.cert_path` | string | — | PEM certificate chain (required when TLS is on). |
| `tls.key_path` | string | — | PEM private key (required when TLS is on). |
| `tls.http_listen` | string | — | Optional `host:port` plain-HTTP listener answering `308` redirects to HTTPS. |
| `static.enabled` | bool | `false` (defaults) / `true` (wallermax.toml) | Serve static files for otherwise unmatched paths. |
| `static.root_dir` | string | `public` | Directory holding the assets; must exist when enabled. |
| `static.index_file` | string | `index.html` | File served for `GET /` (inside `root_dir`). |

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

## HTTP API

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/` | — | Static index file while `[static]` is on; JSON service index otherwise. |
| `GET` | `/api` | — | Service index: name, version and endpoint discovery. |
| `GET` | `/health` | — | Liveness probe. |
| `GET` | `/api/stats` | — | Runtime metrics (+ `registered_users` while auth is on). |
| `POST` | `/api/echo` | — | Debug utility: reads and describes the request body. |
| `POST` | `/api/auth/register` | — | Create an account; the **first** one becomes the admin. |
| `POST` | `/api/auth/login` | — | Exchange credentials for a Bearer token (+ refresh token while enabled). |
| `GET` | `/api/auth/me` | Bearer | The caller's profile (fresh from the repository). |
| `POST` | `/api/auth/refresh` | — | Rotate a refresh token: new access + refresh token, same session. |
| `POST` | `/api/auth/logout` | Bearer | Revoke one refresh token's family (`204`, idempotent). |
| `POST` | `/api/auth/logout_all` | Bearer | Revoke every refresh token of the caller (`204`). |
| `GET` | `/api/admin/users` | Bearer (admin) | List accounts, newest first. |
| `GET` | `/metrics` | — | Prometheus text exposition (while `[metrics]` is on; path configurable). |
| `GET`/`HEAD` | `/<file>` | — | While `[static]` is on: files under `static.root_dir` (conditional + range requests supported). |

The auth and admin families are mounted only while `[auth]` is enabled;
otherwise those paths answer with the standard JSON `404`. The same applies
to the static family (`[static]`), the metrics endpoint (`[metrics]`) and
the refresh/logout endpoints (`auth.refresh_tokens_enabled`).

Example responses:

```json
// GET /api
{"service":"wallermax-server","version":"0.5.0","description":"...","endpoints":["GET / (static index + files)","GET /api","GET /health","GET /api/stats","POST /api/echo","GET /metrics","POST /api/auth/register","POST /api/auth/login","GET /api/auth/me","GET /api/admin/users","POST /api/auth/refresh","POST /api/auth/logout","POST /api/auth/logout_all"]}

// GET /health
{"status":"ok","version":"0.5.0"}

// GET /api/stats
{"service":"wallermax-server","version":"0.5.0","uptime_seconds":10.244,"total_requests":12,"requests_per_second":1.171,"rate_limited_requests":0,"registered_users":2}

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
{"error":{"code":"UNAUTHORIZED","message":"missing `Authorization: Bearer <token>` header","request_id":"..."}}

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
[boa_engine](https://github.com/boa-dev/boa) sandbox with no host
access at all: `GET /hello.jhs` renders `public/hello.jhs` on the fly
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
the `user` global: a valid `Authorization: Bearer` token (verified —
signature, expiry, issuer) injects `user = { id, username, role }`,
while anonymous visitors and failed verifications render with
`user = null`, so role-gated markup is a plain `<?jhs if (user &&
user.role == 'admin') { ?>` block. `[templates] expose_user = false`
turns the injection off.

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
  default configuration and `public/`; runs as an **unprivileged user**
  (`wallermax`, uid 10001); SQLite lives on the **`/data` volume**; the
  binary is PID 1 and handles `SIGTERM` gracefully (so `docker stop`
  drains in-flight requests).

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
  the Dockerfile pins 1.88) and smoke-tests `/health`, `/metrics` and
  the static index inside the running container.

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
| `src/auth.rs` | Argon2id hashing, JWT access tokens, refresh token primitives. |
| `src/db.rs` | SQLite pool, embedded migrations, `UserRepository` trait + impl. |
| `src/metrics.rs` | Prometheus registry and exposition. |
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
Authorization: Bearer <token>
        │
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
re-query the repository.

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
│   └── hello.jhs         #   demo template, rendered at GET /hello.jhs
├── views/                # view templates ([templates] views_dir)
│   ├── index.jhs         #   root fallback when public/index.html is absent
│   └── contacto.jhs      #   auto-routed at GET /contacto
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
│   ├── metrics.rs        # Prometheus registry + exposition rendering
│   ├── extractors.rs     # AuthUser / AdminUser / JsonBody
│   ├── logging.rs        # tracing setup
│   ├── server.rs         # bootstrap, TLS, graceful shutdown
│   ├── util.rs           # tiny shared helpers
│   ├── template_engine/  # .jhs engine: parser + boa sandbox + cache
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
    └── config_env.rs     # environment override semantics
```

## Testing

```console
$ cargo test
running 165 tests ... ok       # unit tests (config, state, error, limiter,
                               #   proxy CIDRs, auth + refresh primitives,
                               #   repository incl. refresh token store,
                               #   metrics registry, server helpers,
                               #   middleware, template engine + parser)
running 4 tests ... ok          # config_env: environment override semantics
running 17 tests ... ok         # auth: register/login/profile/admin flows
running 12 tests ... ok         # refresh_tokens: rotation, reuse detection,
                               #   family revocation, logout/logout_all,
                               #   expiry, disabled mode, multi-rotation
running 11 tests ... ok         # http_api: real server + real HTTP requests
running 15 tests ... ok         # security: rate limit, CORS, body limit, ...
running 7 tests ... ok          # metrics: exposition format, headers, path,
                               #   scrape exemption, registered users gauge
running 6 tests ... ok          # proxy: XFF parsing, trust boundaries,
                               #   per-client buckets via forwarded IPs
running 16 tests ... ok         # static_files: index, assets, 404/405,
                               #   traversal, conditional + range requests
running 6 tests ... ok          # tls: HTTPS serving, auth over TLS,
                               #   strict-client rejection, 308 redirects
running 1 test ... ok           # template_fidelity: 20-case battery vs the
                               #   original node-jhs2 engine
running 20 tests ... ok         # templates: rendering, view auto-routing,
                               #   precedence, error envelopes, mtime
                               #   reload, loop bounds, disabled mode
```

**286 tests total**, all of them plain `cargo test` (no docker, no
network). The integration tests boot the exact same application the
binary serves (`server::build_state` + `server::build_app`, with
`build_app_with_routes` available for injecting custom routes) on an
ephemeral port and assert on real HTTP responses: status codes, JSON
bodies, security headers, rate-limit headers, CORS behaviour, request-id
policy, the JSON error envelope, middleware toggles, registration/login/
profile/admin flows, token forgery/expiry rejections, refresh rotation
and reuse detection, static file serving (index, nested assets, directory
indexes, conditional and range requests, traversal rejection), Prometheus
exposition semantics, trusted-proxy resolution and persistence across
restarts. The TLS tests generate a fresh self-signed certificate per run
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
- [x] Sandbox hardening: no `require`/fs/network, fresh context per
      render, loop iteration limits, captured `console.*` → tracing.
- [x] HTTP integration: on-the-fly rendering under `[static]`, views
      auto-routing with API precedence, JSON error envelopes, mtime
      cache invalidation, `spawn_blocking` renders.
- [x] 20-case fidelity battery against the original engine plus 25
      HTTP integration tests (286 total across the suite).
- [x] v0.6.0: template data — the verified identity injected as the
      `user` global (`{ id, username, role }`, `null` for anonymous),
      `expose_user` switch, bad tokens degrade to anonymous renders.

### Beyond the roadmap (ideas)

- [ ] Role administration endpoints (promote/demote, disable accounts).
- [ ] POST `application/x-www-form-urlencoded` login for OAuth2 flows.
- [ ] Request tracing spans and OpenTelemetry export.
- [ ] Graceful config reload (SIGHUP) and admin API.
- [ ] Alternative repositories (Postgres) behind `UserRepository`.

## Contributing

Pull requests are welcome. Please keep the codebase `cargo fmt` clean,
`cargo clippy` warning-free, and add tests for any new behaviour.

## License

[MIT](LICENSE)
