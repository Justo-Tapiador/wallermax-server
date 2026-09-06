# The JHS template engine (`.jhs`)

[![Ported from node-jhs2](https://img.shields.io/badge/ported%20from-node--jhs2-9cf)](https://github.com/Justo-Tapiador/node-jhs2)
[![Sandbox: boa_engine](https://img.shields.io/badge/sandbox-boa_engine%200.21-yellowgreen)](https://github.com/boa-dev/boa)

> Dynamic `.jhs` templates — HTML with embedded JavaScript, PHP-style —
> rendered on the fly inside a hardened sandbox.

This document covers the `[templates]` feature: what the template
middleware is, what it does, how it fits in the request pipeline, and a
complete guide to writing `.jhs` files. For the general server overview
see the [main README](README.md).

**Table of contents**

- [What it is](#what-it-is)
- [Quick tour](#quick-tour)
- [The request pipeline](#the-request-pipeline)
- [Where template files live](#where-template-files-live)
- [Configuration](#configuration)
- [Writing `.jhs` templates](#writing-jhs-templates)
- [Escaping: `<?= ?>`, `echo()` and `raw()`](#escaping--echo-and-raw)
- [The sandbox](#the-sandbox)
- [Caching and hot reload](#caching-and-hot-reload)
- [Error handling](#error-handling)
- [`console.*` output](#console-output)
- [Deliberate divergences from node-jhs2](#deliberate-divergences-from-node-jhs2)
- [Testing](#testing)
- [Troubleshooting](#troubleshooting)
- [Credits](#credits)

## What it is

The template engine is a faithful Rust port of
[node-jhs2](https://github.com/Justo-Tapiador/node-jhs2), a Node.js
template engine with PHP-style delimiters. A `.jhs` file is plain
markup with two kinds of interpolations:

```text
<?jhs  ...javascript code...  ?>
<?=   ...javascript expression...  ?>
```

The engine compiles the template into a single JavaScript program,
executes it, and returns the produced HTML. In this port the JavaScript
runs inside [boa_engine](https://github.com/boa-dev/boa) — a JavaScript
engine written entirely in Rust — wrapped in a strict sandbox: no
`require`, no `Buffer`, no `include`, no file system, no network, no
process access. Templates can compute, format and loop; they cannot
touch the host.

The feature is split into two layers:

| Layer | Module | Responsibility |
|---|---|---|
| Engine | `src/template_engine/` | Parsing (tags → one JS program), sandbox execution, caching, error model |
| Middleware | `src/middleware/templates.rs` | HTTP contract: which requests render, view auto-routing, JSON error envelopes, `tracing` routing |

The engine is deliberately engine-only (usable as a library, as
`tests/template_fidelity.rs` does); all HTTP policy lives in the
middleware.

## Quick tour

The default configuration ships with the engine enabled and two demo
trees, so `cargo run` is already a template demo:

```console
$ cargo run
INFO ...: dynamic template rendering enabled, views_dir: views, auto_escape: true, cache: true
INFO ...: wallermax-server listening, address: 127.0.0.1:8080, version: "0.5.0"
```

```console
$ curl http://127.0.0.1:8080/hello.jhs          # public/hello.jhs, rendered on the fly
<h1>Hola desde una plantilla .jhs</h1>
&lt;li&gt;elemento 1: uno&lt;/li&gt;              # echo() output is HTML-escaped
<p>Resultado de una expresión: 42</p>
<p>Sin escape: <b>sí en negrita</b></p>         # raw() bypasses the escape

$ curl http://127.0.0.1:8080/contacto           # auto-routed to views/contacto.jhs
<h1>Contacto</h1>
<ul><li>Email: hola@ejemplo.com</li>...</ul>
```

Delete or edit `public/hello.jhs`, `views/index.jhs` and
`views/contacto.jhs` freely — they are ordinary templates and ordinary
documentation.

## The request pipeline

The template middleware sits between the timeout middleware and the
route tree, so every rendered response still carries the security
headers, the request id and the access-log entry — and a runaway render
is bounded by the request timeout *on top of* the sandbox's loop limit.

For each request, while `[templates] enabled = true`:

1. **Only `GET` and `HEAD`** are ever rendered. Rendering is read-only
   by definition, so other methods pass through untouched and get the
   standard JSON 404/405 envelopes for file paths.
2. **Traversal is never intercepted.** Paths containing `..` segments
   (checked *after* percent-decoding) or backslashes are passed to the
   static pipeline, which rejects them as usual.
3. **A `*.jhs` file under the static root is rendered on the fly.**
   `GET /hello.jhs` finds `public/hello.jhs`, renders it, and answers
   the HTML. The template source is **never** served raw.
4. **Everything else runs the normal pipeline**: API routes first, then
   static files.
5. **A pipeline 404 auto-routes to the views directory** before the
   JSON envelope is returned: `GET /contacto` renders
   `views/contacto.jhs`; `GET /blog` renders `views/blog.jhs` or
   `views/blog/index.jhs`; `GET /` falls back to `views/index.jhs`
   when the static index file is missing.
6. **Unresolved paths answer the standard JSON 404 envelope.**

Two consequences worth internalising:

- **API routes always win.** If `/health` is an API route, a
  `views/health.jhs` can never shadow it — the view only renders for
  paths nothing else claims.
- **The static index wins at `/`.** While `public/index.html` exists,
  `GET /` serves it; the root view only takes over when the file is
  removed. This makes `/` predictable and the view a pure fallback.

Rendered responses carry `Content-Type: text/html; charset=utf-8` and
`Cache-Control: no-store` (dynamic output must not be cached by
intermediaries), plus every security header from `[security_headers]`.

Rendering itself runs on the blocking pool (`spawn_blocking`): the JS
engine is CPU-bound and the fresh-sandbox-per-render design keeps it
off the async workers.

## Where template files live

Two independent roots, both resolved at startup (relative paths resolve
against the working directory, exactly like `[static] root_dir`):

| Location | Served at | Rule |
|---|---|---|
| `[static] root_dir` (e.g. `public/`) | `GET /<name>.jhs` | Rendered on the fly, before routing |
| `[templates] views_dir` (e.g. `views/`) | `GET /<name>` | Fallback after a 404: `<name>.jhs`, then `<name>/index.jhs` |

A `.jhs` path missing from the static root still falls back to the
views directory (`GET /onlyview.jhs` → `views/onlyview.jhs`), so the
views tree can serve both shapes. With `[templates] enabled = false`
the middleware is not mounted at all and `.jhs` files are served as
plain static assets (source visible) — the escape hatch for debugging.

## Configuration

The `[templates]` section of `wallermax.toml`:

```toml
[templates]
enabled = true
views_dir = "views"
cache = true
auto_escape = true
loop_iteration_limit = 10000000
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Mounts the middleware and the engine. `false` = plain static serving of `.jhs`. |
| `views_dir` | `"views"` | Views root. Must exist at startup while enabled; `..` segments rejected. |
| `cache` | `true` | Cache compiled templates, recompiling when a file's mtime changes. |
| `auto_escape` | `true` | HTML-escape all dynamic output (`<?= ?>` and `echo()`). `raw()` always bypasses it. |
| `loop_iteration_limit` | `10000000` | Upper bound on loop iterations per render; exceeding it throws. Must be > 0. |

Like every section it participates in the layered configuration:
environment overrides use the `WALLERMAX_TEMPLATES__` prefix
(e.g. `WALLERMAX_TEMPLATES__AUTO_ESCAPE=false`).

## Writing `.jhs` templates

A template is a HTML document interleaved with JavaScript. Everything
outside the tags is literal output; everything inside runs top to
bottom in one shared scope.

### The two tags

```html
<!-- code block: statements, no output of its own -->
<?jhs
  var items = ["uno", "dos", "tres"];
  var total = items.length;
?>

<!-- output expression: evaluated, escaped, printed -->
<p><?= total ?> elementos</p>
```

### Loops interleaved with markup

Code blocks may open in one tag and close in a later one, so plain HTML
can live *inside* the loop body:

```html
<ul>
<?jhs canales.forEach(function (canal) { ?>
  <li><?= canal.nombre ?>: <?= canal.valor ?></li>
<?jhs }); ?>
</ul>
```

(This is exactly how `views/contacto.jhs` is written.) The engine
re-joins the pieces into one program, so braces opened inside a code
block must close in a later block — classic PHP discipline applies.

### Conditionals

```html
<?jhs if (user) { ?>
  <p>Hola, <?= user ?>.</p>
<?jhs } else { ?>
  <p>Hola, anónimo.</p>
<?jhs } ?>
```

### Building output programmatically

```html
<?jhs
  var rows = "";
  for (var i = 1; i <= 3; i++) {
    rows += "<tr><td>fila " + i + "</td></tr>";
  }
?>
<table><?= rows ?></table>
```

Note that `rows` printed through `<?= ?>` is escaped — the `<tr>` tags
would show literally. For markup built in code, print it with
`raw()`:

```html
<table><?jhs echo(raw("<tr><td>fila 1</td></tr>")); ?></table>
```

### Plain JavaScript is available

The sandbox implements ECMAScript: strings and template literals,
arrays and methods, objects, `JSON`, `Math`, `Date`, regular
expressions, `try/catch`, functions:

```html
<p>Año: <?= new Date().getFullYear() ?></p>
<?jhs
  var config = JSON.parse('{"lang":"es"}');
  echo("idioma: " + config.lang);
?>
```

### Undeclared variables throw

There are no implicit globals: `<?= title ?>` with no `title` in scope
throws a `ReferenceError` and the request answers a 500 envelope with
the engine's message. This matches the original engine (and JavaScript
proper) — declare what you print.

## Escaping: `<?= ?>`, `echo()` and `raw()`

With `auto_escape = true` (the default):

| Output path | Escaped? |
|---|---|
| `<?= expr ?>` | Yes |
| `echo(value)` | Yes |
| `echo(raw(value))` | No — trusted markup |
| Literal template text | Never (it is your file) |

The escape covers `&`, `<`, `>`, `"`, `'` — enough for HTML text,
attribute values and inline script contexts to stay inert. Escaping
happens at the boundary (output time), the node-jhs2 semantics.

Use `raw()` only for markup you generated yourself or fully trust;
never for user-supplied input.

## The sandbox

Every render builds a **fresh JavaScript context** — no state leaks
between requests, workers or templates. Inside it:

**Available**: the ECMAScript language core plus `echo`, `raw`,
`escapeHtml`, `console` and `JSON`.

**Not available, by construction**:

- `require` / module loading (there is no module system at all)
- `include` (the original engine's file-embedding helper — see
  [divergences](#deliberate-divergences-from-node-jhs2))
- `Buffer`, `process`, timers, `fetch`, any network or file I/O
- The host: Rust objects, configuration, secrets, request internals

**Loop bound**: `loop_iteration_limit` caps total loop iterations per
render. `<?jhs while (true) { } ?>` throws instead of hanging a worker
(in the original Node engine the 5-second `vm` timeout is ineffective
on modern V8 and the process hangs — here the limit is deterministic).
The request timeout still applies on top of it as a second belt.

**Data injection**: the engine API accepts a JSON data map injected as
global variables (via `Object.defineProperty` with
`CreateDataProperty` semantics — data keys cannot reach `Object.prototype`
or shadow the sandbox helpers). The HTTP middleware currently passes an
empty map, i.e. templates render self-contained; the injection seam
exists for future route-to-template data flows.

## Caching and hot reload

With `cache = true` compiled templates are cached per file and
**recompiled automatically when the file's modification time changes**:
edit a template, save, refresh the browser — no restart needed
(`GET /reload`-style iteration). The cache is bounded by the number of
template files; there is no TTL because the mtime is the truth.

## Error handling

Template failures never leak the template source and never crash the
server:

| Situation | Response |
|---|---|
| Template throws (`throw`, `ReferenceError`, ...) | `500` JSON envelope, `code: "INTERNAL_ERROR"`, message = `Template execution error (<file>): <engine message>` |
| Template file unreadable | `500` JSON envelope with the I/O error text |
| Loop limit exceeded | `500` JSON envelope (the sandbox throws) |
| No template matches, no route, no file | Standard JSON 404 envelope |

The error messages are template-author-facing diagnostics (which line
threw), which is the point: they mention *your* template's path and the
JavaScript error, not server internals. Access logs and the JSON
envelope carry the request id for correlation.

## `console.*` output

`console.log/info/warn/error/debug/trace` inside templates are captured
and routed to the server's structured logger (`tracing`) at the
matching level — `console.log` maps to `info`, mirroring Node's
stdout/stderr split. Nothing is written to the process stdout and
nothing reaches the HTTP response:

```text
2026-09-06T15:01:36Z  INFO wallermax::templates: línea de depuración desde la plantilla
    at src/middleware/templates.rs:150
```

## Deliberate divergences from node-jhs2

The port is behaviour-faithful (a 20-case fidelity battery asserts
identical output against the original engine's semantics) with four
deliberate exceptions, all in the security direction:

1. **No `include`, no `require`, no `Buffer`** — the sandbox exposes no
   host I/O at all, so template code cannot read the file system, spawn
   processes or load modules.
2. **Loop iteration limit** replaces the original's ineffective 5-second
   `vm` timeout: runaway loops throw deterministically.
3. **mtime cache invalidation** — the original caches compiled templates
   forever; this engine recompiles when the file changes.
4. **`console.*` is captured** and routed to `tracing` instead of the
   process stdout.

The custom-tag options of the original (`openTag`, `closeTag`,
`echoTag`) are supported by the engine API (`TagOptions`) with the same
defaults; the HTTP layer always uses the standard tags.

## Testing

```console
$ cargo test --test template_fidelity   # 20-case battery vs the original engine
$ cargo test --test templates           # 20 HTTP integration tests (real server)
$ cargo test                            # full suite: 280 tests
```

The HTTP battery covers the whole contract: on-the-fly rendering under
the static root, source never served raw, view auto-routing (flat,
directory index, trailing slash, root fallback), static index
precedence, API precedence, HEAD rendering, non-GET bypass, traversal
rejection, JSON 404/500 envelopes, mtime reload, coexistence with plain
static files, security headers on rendered responses and the disabled
configuration.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `500 Template execution error ... ReferenceError: x is not defined` | The variable is not declared in the template — declare it or check the typo. No data is injected over HTTP today. |
| `GET /x` answers JSON 404 but the file exists | Is it `views/x.jhs`? The views tree only serves extensionless paths after a 404; `public/` serves `/x.jhs` directly. |
| Template changes are not picked up | `cache = true` recompiles on mtime change; ensure the editor really changes the mtime (some tools preserve it). |
| Escaped markup shows as text (`&lt;li&gt;`) | Expected with `auto_escape` — print trusted markup with `raw()`. |
| Raw `.jhs` source is being served | `[templates] enabled = false`; set it to `true`. |
| `while (true) {}` answered 500 | The loop iteration limit did its job — that is the contract. |

## Credits

- [node-jhs2](https://github.com/Justo-Tapiador/node-jhs2) (MIT,
  Justo-Tapiador) — the original engine this port preserves.
- [boa_engine](https://github.com/boa-dev/boa) (MIT) — the pure-Rust
  JavaScript interpreter powering the sandbox.

Licensed under the server's [MIT license](LICENSE).
