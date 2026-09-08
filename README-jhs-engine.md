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
- [Template data: the globals](#template-data-the-globals)
- [Shared partials: `include()`](#shared-partials-include)
- [Importing modules: `require()`](#importing-modules-require)
- [Redirecting from a template: `res.redirect()`](#redirecting-from-a-template-resredirect)
- [Escaping: `<?= ?>`, `echo()` and `raw()`](#escaping--echo-and-raw)
- [Cookbook: markup from loops](#cookbook-markup-from-loops)
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
engine written entirely in Rust — wrapped in a strict sandbox. Since
v0.9.0 the original engine's `require()` is back, rebuilt as a **native
bridge**: a configurable module **banner** (`forbidden_modules` — the
hardened descendant of node-jhs2's `banned_require`), the `crypto`
polyfill implemented in Rust, and CommonJS loading of pure-JS modules
from the `modules/` directory. There is still no Node.js behind the
sandbox: no `Buffer`, no `process`, no native addons, and the file
system is only reachable for modules under `modules/`. Templates can
compute, format, loop, import and redirect; they cannot touch the host.

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
expose_user = true
loop_iteration_limit = 10000000
require_enabled = true
modules_dir = "modules"
forbidden_modules = [
  "child_process", "cluster", "dgram", "dns", "fs", "http", "https",
  "inspector", "jhs", "mv", "net", "os", "process", "repl", "tls",
  "tty", "v8", "vm", "worker_threads",
]
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Mounts the middleware and the engine. `false` = plain static serving of `.jhs`. |
| `views_dir` | `"views"` | Views root. Must exist at startup while enabled; `..` segments rejected. |
| `cache` | `true` | Cache compiled templates, recompiling when a file's mtime changes. |
| `auto_escape` | `true` | HTML-escape all dynamic output (`<?= ?>` and `echo()`). `raw()` always bypasses it. |
| `expose_user` | `true` | Inject the authenticated identity as the `user` template global (`null` when anonymous or unverifiable). |
| `loop_iteration_limit` | `10000000` | Upper bound on loop iterations per render; exceeding it throws. Must be > 0. |
| `require_enabled` | `true` | Installs the `require()` bridge (see [below](#importing-modules-require)). `false` = the v0.8.x sandbox, `require` undefined. |
| `modules_dir` | `"modules"` | Root local JS modules resolve under. May be absent at startup (requiring a local module then answers a descriptive miss); `..` segments rejected. |
| `forbidden_modules` | the 19 dangerous built-ins + `jhs` + `mv` | The banner: module names `require()` rejects outright. Checked before polyfills and files. |

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

The [cookbook](#cookbook-markup-from-loops) develops this into the full
toolbox: why `echo()` escapes, the three ways to print markup from a
loop, and how to choose between them.

### Conditionals

```html
<?jhs if (user) { ?>
  <p>Hola, <?= user.username ?>.</p>
<?jhs } else { ?>
  <p>Hola, anónimo.</p>
<?jhs } ?>
```

(`user` is one of the globals the server injects into every render —
see [the next section](#template-data-the-user-object).)

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
proper) — declare what you print. The server-injected globals (`user`,
`path`, `query`, `pages`) are always defined (`user` is `null` for
anonymous visitors) — see below.

## Template data: the globals

The original node-jhs2 accepted an extra data object next to the file
path (`render(templatePath, data)`), whose keys became template
globals. The HTTP middleware uses that seam; since v0.9.0 every render
receives **five** globals:

| Global | Value |
|---|---|
| `user` | `{ id, username, role }` with a valid `Authorization: Bearer` token or `wallermax_session` cookie (verified), `null` otherwise |
| `path` | The request path — the login/register modals post it back as their `redirect` field so users land where they were |
| `query` | The query parameters as an object of first-value strings (`?login_error=credenciales` → `query.login_error`), how the flash-style error codes reach the templates |
| `pages` | The published CMS pages (`[{ id, slug, title, updated_at, updated_at_h }]`, newest first, capped at 50) while the CMS is enabled; an empty array otherwise |
| `req` | An Express-shaped request object: `{ method, url, path, query, headers }`. Only a fixed allowlist of harmless headers is exposed (see the section on [`req`](#redirecting-from-a-template-resredirect)) |

| Request | `user` value |
|---|---|
| Valid `Authorization: Bearer <token>` (verified signature, expiry and issuer) | `{ id: 1, username: "justo", role: "admin" }` |
| Valid `wallermax_session` cookie (v0.7.0; the login page sets it) | `{ id: 1, username: "justo", role: "admin" }` |
| No header and no cookie, `[auth]` disabled, or `expose_user = false` | `null` |
| Malformed, expired or foreign-signed token | `null` (the page renders anonymously — a public page never becomes an error because of a bad token) |

A role-gated fragment looks exactly like you would expect:

```html
<?jhs if (user && user.role == 'admin') { ?>
   <p>Bienvenido, administrador <?= user.username ?></p>
<?jhs } else if (user) { ?>
   <p>Hola, <?= user.username ?> (<?= user.role ?>)</p>
<?jhs } else { ?>
   <p>por favor, inicia sesión</p>
<?jhs } ?>
```

Properties and guarantees:

- **`user.role` is server-issued.** It comes from the signed JWT, not
  from the request, so a client cannot forge the `admin` role by
  editing a cookie or a header. A token that fails verification (bad
  signature, expired, wrong issuer) degrades to `user = null`, never
  to a partial identity.
- **`<?= user.username ?>` is auto-escaped.** The username is
  user-chosen input; `auto_escape` (on by default) HTML-escapes it, so
  personalisation cannot turn into stored XSS. Only `raw()` bypasses
  the escaper — use it for trusted markup, never for identity fields.
- **`user` is always *defined*** (unlike undeclared variables): null
  for anonymous visitors. Guard with `if (user && ...)` — a truthiness
  check is the portable form. `pages` is always an array; `query` and
  `path` are always defined too.
- Turn the injection off with `[templates] expose_user = false` — for
  example when rendered pages must stay identity-free so a CDN can
  cache them.

The injection mechanism is generic (a JSON map whose keys become
globals, with `__proto__`-style keys blocked and sandbox helper names
non-shadowable), so future server-side data can ride the same seam
without engine changes.

## Shared partials: `include()`

Since v0.8.0 a code block whose **entire** body is a single
`include("name")` call is resolved at **compile time** into the
referenced partial, read from the views directory:

```html
<body>
<?jhs include("partials/header") ?>
  <main>…</main>
<?jhs include("partials/footer") ?>
</body>
```

The rules:

- **Names are views-root-relative** (`partials/header` →
  `views/partials/header.jhs`), regardless of the file that embeds
  them; the `.jhs` extension is optional and normalised.
- **Valid names are relative paths of `[A-Za-z0-9_-]` segments joined
  by `/`** — `..`, `.`, backslashes, absolute paths and non-ASCII are
  rejected (a 500 envelope with the author-facing message).
- **Nesting is bounded** (8 levels) and the total embedded size is
  capped (1 MiB) — the include-bomb guard.
- **Editing a partial recompiles its includers**: the cache tracks the
  partials' mtimes exactly like the main file's.
- The embedded partial is compiled into the **same program**, so it
  sees the same globals (`user`, `pages`, `query`, `path` …) and shares
  the same loop-iteration budget.
- Any other use — `echo(include("x"))`, `var h = include("x")` — is
  **not** resolved and fails at execution time on a sandbox stub whose
  message explains the standalone-tag rule.

The shipped `views/partials/header.jhs` (site header + login/register
modals) and `views/partials/footer.jhs` are the reference example, and
CMS page bodies may embed them too.

## Importing modules: `require()`

Since v0.9.0 templates can import modules exactly like the original
node-jhs2 did — with the crucial difference that there is **no Node.js
behind the sandbox** (boa_engine is a pure ECMAScript interpreter), so
the loader is rebuilt as a native bridge with the original's *banner*
concept kept and hardened:

```html
<?jhs
  const { randomBytes, randomUUID } = require('crypto');
  const greeting = require('greeting');          // modules/greeting.js
  const tag = randomBytes(16).toString('hex');   // cache-busting
?>
<link rel="stylesheet" href="style.css?v=<?= tag ?>">
<p><?= greeting.hello('mundo') ?> <?= randomUUID() ?></p>
```

Three sources feed `require`, checked in this order:

1. **The banner** (`[templates] forbidden_modules`). The administrator's
   list of module names rejected outright — `fs`, `mv`, `child_process`,
   … — checked against the *package name* (the first path segment), so
   `mv`, `mv/sub` and `node:mv` are all covered by the entry `mv`. The
   banner wins over everything, including polyfills: listing `crypto`
   there bans the polyfill too.
2. **Builtins as native polyfills.** `crypto` is implemented in Rust
   (`randomBytes(size).toString('hex' | 'base64' | 'utf8' |
   'latin1')` with `length`, and `randomUUID()`), backed by the OS
   CSPRNG — never `Math.random`. Every other Node builtin (`fs`, `net`,
   `path`, `events`, …) answers a descriptive error: there is no Node
   runtime behind the sandbox, so it cannot exist here.
3. **Local CommonJS modules** under `modules_dir` (default `modules/`):
   your own files, or npm packages copied in. The loader follows Node's
   lookup ladder — exact file, `<name>.js`, `<name>/index.js`,
   `<name>/package.json`'s `main` — evaluates the source with the
   standard wrapper (`exports`, `require`, `module`, `__filename`,
   `__dirname`), and caches instances **per render** (the module object
   is registered before the body runs, so circular requires return
   partial exports exactly like Node's).

Rules worth internalising:

- **Bare names** (`require('lodash')`) resolve under `modules/`, then
  under `modules/node_modules/`. **Relative names** (`./x`, `../x`)
  resolve against the *requiring file's* directory — in template code
  that is the modules root itself.
- **Every resolved path must stay inside `modules_dir`** (symlinks
  resolved): `require('../../etc/passwd')`, absolute paths and any
  traversal answer a sandbox-escape error, not a file read. The modules
  directory is the only file system the sandbox can ever see.
- **A module body is compiled as a `new Function` parameter**, never
  concatenated into evaluable source — a crafted body cannot break out
  of its wrapper. This design point is also what keeps exported
  closures working: a nested `Context::eval` inside the native call
  reenters boa's run loop and leaves freshly created closures
  un-callable.
- **Pure-JS modules only.** A module may use the ECMAScript language
  and the sandbox globals (`JSON`, `Math`, `Date`, `console`, …). It
  cannot use `Buffer`, `process`, `fs`, streams or native addons —
  those do not exist here, and packages that hard-depend on them fail
  with the descriptive errors above. CommonJS only (`module.exports`);
  ES module syntax (`import`/`export`) is not compiled.
- **Guards**: the total module source bytes per render are capped
  (8 MiB), module nesting is bounded by the runtime recursion limit,
  and module code shares the render's loop-iteration limit. Editing a
  module file takes effect on the next request (mtime-based source
  cache, exactly like templates).
- **State never leaks between requests**: every render builds a fresh
  sandbox, so module-level state (`var count = 0` at the top of a
  module) resets per request. Node caches module instances for the
  process lifetime — here that would be a cross-request leak.
- Set `require_enabled = false` to remove `require` entirely and get
  the v0.8.x sandbox back (with `res.redirect()` still available).

Extending the polyfill registry (`src/template_engine/require_bridge.rs`)
is deliberately simple: one match arm in `resolve_impl` plus a builder
function — `crypto` is ~80 lines including its tests.

## Redirecting from a template: `res.redirect()`

The original engine's host app passed the Express `res` object into
render data, so templates could do `res.redirect('/login')`. That is
rebuilt as a safe shim: every template (views, `public/*.jhs`, CMS page
bodies) gets a non-shadowable `res` object with one method —

```html
<?jhs if (!user) { res.redirect('/login'); return; } ?>
área privada
```

- `res.redirect(location)` records a redirect **intent**; when the
  render finishes, the route layer answers it with the real HTTP
  redirect (302 by default) instead of the HTML. The pattern above —
  redirect then a bare top-level `return;` — behaves exactly like the
  original engine.
- `res.redirect(location, status)` accepts 301, 302, 303, 307 and 308.
  The first call wins (the response is committed, like Express).
- **Local paths only** (`/...`): `http://`, `https://` and
  protocol-relative `//evil.example` targets throw — the same
  anti-open-redirect posture as the auth forms. A CMS editor cannot
  turn your domain into a phishing redirect.
- `res` and `require` are protected globals: render data can never
  shadow them.

### The `req` global

The mirror object arrives as render data: `req = { method, url, path,
query, headers }` — `url` is path + query, `query` the first-value
object (same shape as the `query` global). **Headers are a fixed
allowlist** (`accept`, `accept-language`, `content-type`, `host`,
`referer`, `user-agent`): everything else is simply *absent* —
`req.headers.cookie` is `undefined` even when the browser sent a
cookie. The reason is the CMS's multi-editor model: page **editors**
author template code, so a page echoing the *viewer's* session cookie
would leak it to the page author. The allowlist keeps `req` useful for
rendering (language negotiation, UA-based markup) without that class
of leak. `typeof req.headers.cookie === 'undefined'` — a template
annot even detect that credentials were sent.

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

## Cookbook: markup from loops

The one rule that dissolves the first surprise — printing `<li>` from
a loop and getting `&lt;li&gt;` on the page:

> **Outside the tags** you are writing the template file itself: the
> literal text between code blocks reaches the response **verbatim**,
> never escaped. **Through `echo()` or `<?= ?>`** you are printing
> **data**: it is HTML-escaped on output.

Both engines behave identically here — node-jhs2's `echo()` is
`args.map(arg => __escape(String(arg)))` with `autoEscape` on by
default — and the reason is XSS: values a user can influence (a
username, a movie title, a query parameter) must arrive at the browser
inert. Escaping is the default; trust is what you declare explicitly.
Loops are where the two worlds meet, and three patterns cover it all.

### Pattern 1 — interleave the markup (canonical, both engines)

Keep the static tags in the template — between code blocks — and let
the loop body span several blocks:

```html
<ul>
<?jhs
  var items = ["uno", "dos", "tres"];
  items.forEach(function (item, i) { ?>
  <li><?jhs echo("elemento " + (i + 1) + ": " + item); ?></li>
<?jhs }); ?>
</ul>
```

To the compiler, the text between blocks is literal output and block
bodies are verbatim JavaScript, so the loop above becomes:

```js
items.forEach(function (item, i) {
__output += "\n  <li>";                          // your markup, verbatim
    echo("elemento " + (i + 1) + ": " + item);   // your data, escaped
__output += "</li>\n";                           // your markup, verbatim
});
```

The tags come out as real HTML; the interpolated data is still
escaped. Braces may open in one block and close in a later one —
classic PHP discipline — and this is how every view in `views/admin/`
is written. It is also the safest form, because static and dynamic
content never travel through the same expression: there is nothing to
reason about. (The data can cross the boundary through `<?= item ?>`
instead of `echo()` — the form shown
[earlier](#loops-interleaved-with-markup); the guarantees are the same.)

### Pattern 2 — build the string, print it with `raw()`

When interleaving is impractical (deeply nested rows, fragments
assembled in code), build the markup in a variable and mark the
**finished** string as trusted:

```html
<?jhs
  var rows = "";
  items.forEach(function (item) {
    rows += "<tr><td>" + escapeHtml(item) + "</td></tr>\n";
  });
?>
<table><?jhs echo(raw(rows)) ?></table>
```

Two details matter. First, `raw()` must wrap the **whole concatenated
value**: a `raw()` result is a sentinel object, so `raw(a) + b` does
not concatenate — it produces `[object Object]b`. Concatenate first,
wrap last. Second, notice the `escapeHtml(item)` inside the loop:
`raw()` marks the *markup you wrote* as trusted, not the *data* —
escape the data yourself on the way in. `escapeHtml` is a sandbox
global for exactly this.

### Pattern 3 — `echo(raw(...))` inside the loop

The compact one-liner, same rules as pattern 2:

```html
<?jhs
  items.forEach(function (item, i) {
    echo(raw("<li>elemento " + (i + 1) + ": " + escapeHtml(item) + "</li>\n"));
  });
?>
```

Patterns 2 and 3 are **port-only**. In node-jhs2, `raw` is an identity
function that suppresses nothing, so the same template prints escaped
markup there. This port's `raw()` is a real sentinel — the one place
where the port is *more* capable than the original (see
[Deliberate divergences](#deliberate-divergences-from-node-jhs2),
item 6).

### Choosing

| Situation | Pattern |
|---|---|
| Loop with static surrounding markup (lists, tables, cards) | 1 — interleave |
| Markup assembled in code (rows, fragments, deep nesting) | 2 — build + `raw()` |
| Compact loop, port-only template | 3 — `echo(raw(...))` |
| Values a user can influence | any — but they cross the boundary via `<?= ?>` or `escapeHtml()` |

### Why the default is worth it

```html
<?jhs var bad = "<script>alert(1)</script>"; ?>
<p><?= bad ?></p>
```

renders as `<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>` — inert text
on the page. That is the property the default buys you: a template
author (or a CMS page editor — the same reasoning as the [`req` header
allowlist](#redirecting-from-a-template-resredirect)) cannot smuggle
active markup into a visitor's session by accident. `raw()` is the
explicit opt-out for markup you control.

### Gotchas

- **A `?>` inside a JavaScript string closes the block early.**
  `echo("a ?> b")` splits the block at the `?>` — inherited from the
  original's regex compiler. Keep `?>` out of string literals in code
  blocks.
- **The text between tags shapes the output.** Pattern 1's newlines and
  indentation are literal and end up in the response. Cosmetic — but if
  each element needs its own line, put the newline in the static text
  rather than inside the `echo()`.
- **`<?= echo(x) ?>` prints nothing, in both engines.** It compiles to
  `__output += __escape(echo(x))`, and the `+=` reads `__output` before
  `echo()` writes it. Print expressions with `<?= x ?>`, print from
  code with `<?jhs echo(x); ?>` — never nest one inside the other.

## The sandbox

Every render builds a **fresh JavaScript context** — no state leaks
between requests, workers or templates. Inside it:

**Available**: the ECMAScript language core plus `echo`, `raw`,
`escapeHtml`, `console`, `JSON`, `res` (with `redirect`), and — while
`require_enabled = true` — `require()` backed by the module bridge
(banner + `crypto` polyfill + CommonJS modules under `modules_dir`; see
[Importing modules](#importing-modules-require)). `include("name")`
works only as a standalone tag — it is resolved at compile time by the
host, never executed inside the sandbox.

**Not available, by construction**:

- `Buffer`, `process`, timers, `fetch`, any network or native addons
  (there is no Node.js behind boa_engine, period)
- `require` of anything outside the three sanctioned sources: the
  banner rejects listed names, non-polyfilled built-ins throw, and file
  resolution is jailed to `modules_dir`
- `include` **at execution time** (the original engine's runtime
  file-embedding helper — see
  [divergences](#deliberate-divergences-from-node-jhs2): here include
  is a compile-time, host-side, path-validated tag)
- The host: Rust objects, configuration, secrets, request internals
  (beyond the `req` data global's sanitized allowlist)

**Loop bound**: `loop_iteration_limit` caps total loop iterations per
render. `<?jhs while (true) { } ?>` throws instead of hanging a worker
(in the original Node engine the 5-second `vm` timeout is ineffective
on modern V8 and the process hangs — here the limit is deterministic).
The request timeout still applies on top of it as a second belt.

**Data injection**: the engine API accepts a JSON data map injected as
global variables (via `Object.defineProperty` with
`CreateDataProperty` semantics — data keys cannot reach `Object.prototype`
or shadow the sandbox helpers). The HTTP middleware passes the `user`
object through this seam (see
[Template data](#template-data-the-user-object)); the mechanism stays
generic for future route-to-template data flows.

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
| An `include()` name is invalid or the partial is missing | `500` JSON envelope, message = `Template include error: …` (author-facing) |
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
identical output against the original engine's semantics) with
deliberate exceptions, all in the security direction:

1. **`require()` is a native bridge, not Node's.** The original wrapped
   the real Node `require` with a small `banned_require` blocklist —
   `fs` and `child_process` were loadable there. This port keeps the
   banner posture (`[templates] forbidden_modules`, defaulting to the
   dangerous built-ins) but modules come from a Rust polyfill registry
   (`crypto`) and from pure-JS CommonJS files under `modules_dir`,
   jailed to that directory. There is no Node.js behind boa_engine, so
   native addons and host-touching built-ins cannot exist here.
2. **Loop iteration limit** replaces the original's ineffective 5-second
   `vm` timeout: runaway loops throw deterministically (and the limit
   covers module code too).
3. **mtime cache invalidation** — the original caches compiled templates
   and module instances forever; this engine recompiles when a file (or
   an embedded partial, or a required module) changes, and module
   instances live per render so state never leaks across requests.
4. **`console.*` is captured** and routed to `tracing` instead of the
   process stdout.
5. **`res` and `req` are rebuilt as safe shims.** The original received
   the host app's Express objects as render data — full headers, open
   redirects included. Here `res.redirect()` accepts local paths only,
   and `req` exposes a fixed header allowlist: the CMS's editor role
   authors template code, and an editor page echoing a viewer's cookie
   (or redirecting to an attacker's site) must be impossible by
   construction.
6. **`raw()` actually suppresses the escape.** In node-jhs2 `raw` is an
   identity function that suppresses nothing — `<?= raw("<b>") ?>` and
   `echo(raw("<b>"))` print `&lt;b&gt;` there. Here `raw()` wraps the
   value in a hidden sentinel that passes untouched through the
   escaper (and through `echo()`), so the documented escape hatch is
   real. The safe default is unchanged; only the opt-out works now.

The custom-tag options of the original (`openTag`, `closeTag`,
`echoTag`) are supported by the engine API (`TagOptions`) with the same
defaults; the HTTP layer always uses the standard tags.

## Testing

```console
$ cargo test --test template_fidelity   # 20-case battery vs the original engine
$ cargo test --test templates           # 37 HTTP integration tests (real server)
$ cargo test                            # full suite: 396 tests
```

The HTTP battery covers the whole contract: on-the-fly rendering under
the static root, source never served raw, view auto-routing (flat,
directory index, trailing slash, root fallback), static index
precedence, API precedence, HEAD rendering, non-GET bypass, traversal
rejection, JSON 404/500 envelopes, mtime reload, coexistence with plain
static files, security headers on rendered responses and the disabled
configuration — and, since v0.9.0, `require()` from views (crypto and
local modules), the configurable banner, `require_enabled = false`,
`res.redirect()` as a real 302/301, and the `req` global with its
header allowlist.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `500 ... require('fs') is forbidden` | The banner did its job: `fs` ships in the default `forbidden_modules`. Removing it from the list changes nothing on its own — there is no Node runtime behind the sandbox anyway. |
| `500 ... Cannot find module 'x': ... looked under the modules directory` | Put the module under `modules/` (`modules/x.js`, `modules/x/index.js`, or `modules/x/package.json` with a `main`), or point `modules_dir` at the right directory. |
| `500 ... 'net' is a Node.js built-in ... only as native polyfills` | There is no Node.js behind boa_engine — that builtin cannot be required. Use the `crypto` polyfill, a local module, or extend the polyfill registry in Rust. |
| `500 ... escapes the modules directory` | A module tried to reach outside `modules/` (a `../../` climb, an absolute path, a symlink). Modules must live inside the modules directory. |
| An npm package fails on `Buffer` / `process` / `fs` | Pure-JS packages only: anything hard-depending on Node APIs cannot run inside the sandbox. |
| `require is not defined` | `[templates] require_enabled = false`; flip it to `true`. |
| `res.redirect('https://…')` threw | Local paths only — open-redirect protection. |
| `500 Template execution error ... ReferenceError: x is not defined` | The variable is not declared in the template — declare it or check the typo. The injected `user` global is always defined (`null` when anonymous); anything else must be declared. |
| `GET /x` answers JSON 404 but the file exists | Is it `views/x.jhs`? The views tree only serves extensionless paths after a 404; `public/` serves `/x.jhs` directly. |
| Template changes are not picked up | `cache = true` recompiles on mtime change; ensure the editor really changes the mtime (some tools preserve it). |
| Escaped markup shows as text (`&lt;li&gt;`) | Expected with `auto_escape` — print trusted markup with `raw()`, or move the tags outside the code blocks ([cookbook](#cookbook-markup-from-loops)). |
| Raw `.jhs` source is being served | `[templates] enabled = false`; set it to `true`. |
| `while (true) {}` answered 500 | The loop iteration limit did its job — that is the contract. |

## Credits

- [node-jhs2](https://github.com/Justo-Tapiador/node-jhs2) (MIT,
  Justo-Tapiador) — the original engine this port preserves.
- [boa_engine](https://github.com/boa-dev/boa) (MIT) — the pure-Rust
  JavaScript interpreter powering the sandbox.

Licensed under the server's [MIT license](LICENSE).
