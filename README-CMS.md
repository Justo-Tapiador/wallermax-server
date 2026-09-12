# The wallermax CMS — the corporate content model (F7), the editor (F8) & the media library (F9)

This is the companion deep-dive for the CMS feature line: the main
[README.md](README.md) keeps the essentials and links here, so the
front door of the repository stays short while the content features
get the room they deserve.

**F7** adds four corporate-site capabilities, **F8** the editor and
**F9** the media library — all of them **zero-JavaScript** (the CSP
keeps blocking scripts and every new screen works through plain HTML
forms):

| Capability | One line |
|---|---|
| [Page hierarchy](#page-hierarchy) | Parents, sibling ordering, breadcrumbs and a cycle-proof move guard. |
| [Menus](#menus) | Named navigation menus rendered by any template through the `menus` global. |
| [SEO metadata](#seo-metadata) | Per-page `meta_title`, `meta_description` and `og_image` in the wrapper's `<head>`. |
| [Sitemap](#sitemap) | `GET /sitemap.xml` with every published page, automatically. |
| [The editor (F8)](#the-editor-two-body-modes) | Markdown bodies with a server-side preview — and the `.jhs` mode kept as-is. |
| [The media library (F9)](#the-media-library-f9) | Image uploads validated at the byte level, immutable-cached serving, alt text and copy-paste snippets. |

Planned next phases live in [the roadmap](#the-cms-roadmap) at the
bottom.

## Page hierarchy

A page may name another page as its **parent** and carry a `position`
(its order among its siblings — menus, subpage listings and the admin
tree all respect it; lower comes first, ties break by creation order).

Three deliberate rules:

- **Flat, globally-unique URLs.** A child keeps its one-piece slug and
  serves at `/p/{slug}` — the same URL regardless of depth. WordPress
  allows the same slug under different parents; this CMS does not,
  because a slug that means exactly one page is unambiguous in menus,
  sitemaps, logs and conversations.
- **A page can never hang under its own branch.** The edit form's
  parent list excludes the page and its whole subtree, and the server
  re-checks the move (a parent must exist, differ from the page, and
  not be its own descendant) before touching the database — a cycle
  answers the form with a friendly Spanish error, never a corrupted
  tree. The ancestor walk itself is hop-bounded, so even a hand-edited
  database cannot hang a request.
- **Deleting a parent reparents its children to the top level.**
  Content is never lost with a branch: the children keep their slugs,
  their URLs and their own subtrees.

The admin listing (`/admin/pages`) renders the whole tree indented by
depth; the parent `<select>` in the page form indents the same way, so
the shape of the site reads inside the dropdown.

### What the public render sees

`GET /p/{slug}` passes the page to `views/cms_page.jhs` wrapped with
its hierarchy:

| Field | Content |
|---|---|
| `page.parent` | `{ id, slug, title }` or `null` |
| `page.breadcrumbs` | `[{ slug, title }]` — root first, immediate parent last, the page itself excluded |
| `page.children` | `[{ slug, title, position }]` — direct children (drafts included only for editors) |
| `page.position` | The sibling ordering number |

The shipped wrapper renders the breadcrumbs above the title (a
`pagina-meta migas` paragraph: `Inicio · Servicios · …`) whenever the
chain is non-empty; a custom wrapper can do anything else with the same
fields.

## Menus

Navigation is data, not template hardcoding: named menus hold ordered
items, and **every template render** receives them resolved through the
`menus` global.

```toml
# nothing to configure — menus live in the database, managed at /admin/menus
```

A menu has an immutable **name** (the template key) and an editable
title. Items are ordered and come in exactly one of two shapes:

- a **page link** — optionally with a label override; without one, the
  linked page's title is the label and the href resolves to
  `/p/{slug}`;
- a **custom URL** — a site path (`/aviso-legal`), an external link
  (`https://…`) or an anchor (`#seccion`), with a mandatory label.

**Drafts never leak into the public navigation**: an item pointing at
a draft page is stored (editors see it in the panel) but skipped by the
public `menus` global until the page is published, so the site
navigation never links a 404. Deleting a page removes its menu items;
deleting a menu removes its items — the navigation never keeps dead
links by construction.

### The `menus` global in templates

```jhs
<?jhs if (menus && menus.principal) { ?>
<nav>
<?jhs menus.principal.forEach(function (item) { ?>
  <a href="<?= item.href ?>"><?= item.label ?></a>
<?jhs }); ?>
</nav>
<?jhs } ?>
```

Each item is `{ label, href }` — resolved server-side, ready to print.
The global is `{}` while the CMS is off or the menu table is empty, so
templates degrade gracefully. Like `pages`, it costs one query per
render (the [LRU caching](#the-cms-roadmap) phase will deduplicate
both).

### Admin routes (F7)

| Route | Purpose |
|---|---|
| `GET/POST /admin/menus` | List menus; create (`name` + `title`). |
| `GET/POST /admin/menus/{id}` | Detail (items + rename form); rename the title. |
| `POST /admin/menus/{id}/delete` | Delete the menu and its items. |
| `GET /admin/menus/{id}/items/new`, `POST /admin/menus/{id}/items` | The add-item form; create the item. |
| `GET /admin/menus/{id}/items/{item}/edit`, `POST /admin/menus/{id}/items/{item}` | Edit one item. |
| `POST /admin/menus/{id}/items/{item}/delete` | Delete one item. |

The `editor` and `admin` roles manage menus (the same privilege split
as pages); plain users get the friendly 403, anonymous visitors are
redirected to the login view.

## SEO metadata

Every page carries three optional fields, edited in the same form
below the content (an own "SEO" card):

| Field | Renders as | Notes |
|---|---|---|
| `meta_title` | `<title>` | Falls back to the page title; ~60 characters shown by engines. |
| `meta_description` | `<meta name="description">` | Omitted entirely when empty; ~160 characters shown. |
| `og_image` | `og:title` + `og:image` (+ `og:type`) | A site path (`/assets/…`) or an absolute URL — validated, so no `javascript:` nonsense. |

`views/cms_page.jhs` renders them into the `<head>`; empty means
absent, and a page without metadata renders exactly like before F7
(there is a test pinning that).

## Sitemap

`GET /sitemap.xml` (while `[cms] sitemap = true`, the default, and the
CMS is enabled) lists every **published** page as
`<loc>…/p/{slug}</loc>` with a `<lastmod>` date (the page's last edit,
as `YYYY-MM-DD`). The homepage appears as the bare origin **only**
while `default_page` names a published page — otherwise `GET /` is not
CMS content and stays out of the map.

The sitemap protocol requires absolute URLs, and the server does not
know its public origin (TLS? a proxy? a domain?). Two sources, in
order:

1. `[cms] site_url = "https://www.example.com"` — the explicit,
   recommended way. Validated at startup: absolute, no trailing slash,
   no whitespace.
2. The request's `Host` header under plain `http://` — correct for
   direct-HTTP setups (development, LAN), wrong behind TLS or a reverse
   proxy. If you run behind either, set `site_url`.

The response is `application/xml`, cacheable for an hour (public
information, unlike every template render). Drafts never appear;
`robots.txt` stays a static file under `public/` — reference the
sitemap from it if you want it discovered (`Sitemap: https://…/sitemap.xml`).

## The editor: two body modes

Every page now states **how its body is interpreted**, in the same
form, right above the textarea:

| Mode | Body is… | For |
|---|---|---|
| `jhs` (default) | `.jhs` template source, rendered by the engine with the standard globals — exactly the pre-F8 pipeline. | Editors who want markup, personalization, `include()`. |
| `markdown` | Markdown source, rendered by the safe in-process renderer. | Plain writing: prose, headings, lists, tables, links. |

The mode is a per-page column (`migrations/0005`, `CHECK`-constrained);
existing pages inherit `jhs` and render exactly as before (there is a
test pinning that), and switching modes on an existing page is a
normal edit — the public render re-routes through the new mode.

### The Markdown subset — safe by construction

The renderer (`src/markdown.rs`, `pulldown-cmark`) is deliberately
narrow, and **nothing an editor writes can become markup the server
did not generate itself**:

- **Raw HTML is dropped, not rendered.** pulldown-cmark 0.13 parses
  raw HTML unconditionally (there is no off switch), so the filter
  drops those events on the floor: `<script>`, `<iframe>`, `onerror=`
  payloads simply vanish. Inline tags drop while their inner text
  stays as inert prose; whole HTML blocks drop entirely. The preview
  makes that visible immediately — the editor previews, sees the
  markup go, and knows. Need real HTML? That is what the `.jhs` mode
  is for, and its power is already gated behind the editor role.
- **URL schemes are filtered.** Link and image destinations may be
  `http`, `https`, `mailto`, `ftp`, or any relative form (`/`, `#`,
  `?`, plain paths). Anything else — `javascript:`, `data:`,
  `vbscript:` — degrades to `#`: the anchor still renders, the
  payload does not.
- **Tables and strikethrough** are on (the useful GFM extras);
  footnotes, task lists and the other exotica stay off to keep the
  rendered surface small and predictable.
- **Text is escaped** everywhere by the writer, including code blocks.

Bodies cap at the same 600 000 characters as before, and the render
runs in `spawn_blocking` like the `.jhs` pipeline — an editor-sized
document never blocks the async runtime.

### The previsualización — server-side, zero JavaScript

The chosen philosophy (the alternative was a JavaScript live-preview
editor; it was rejected): the form carries **two submit buttons**, and
the second one is pure HTML5:

```html
<button type="submit" formaction="/admin/pages/preview"
        formmethod="post">Previsualizar</button>
```

`POST /admin/pages/preview` (editor/admin) reads the same fields the
save routes read, renders the body with the mode it states, and
re-renders **the same form** — values kept, so the editor keeps
writing — with the rendered body above it in a «Vista previa» card.
Nothing is written: no row, no draft, no updated_at bump; the page
count cannot change from a preview.

Details that matter:

- A `.jhs` preview runs through the real engine with the live
  `base_data` globals (`user`, `query`, `path`, …) — the same render
  the public page would get, so template **errors bounce back as an
  inline form error instead of a published 500**. That is the whole
  point of previewing.
- Editing an existing page round-trips: the form posts a hidden
  `page_id`, the preview re-renders the form pointed at the edit
  route, and the editor's next «Guardar» saves normally. `page_id` is
  round-trip data, never a command — the preview never persists.
- Re-submitting the preview on refresh is harmless (the render is
  idempotent); saves keep the PRG pattern as always.

### `.jhs` body patterns: what the engine actually supports

Verified live against both backends (the probe below ships as
`public/sidecar-check.jhs` semantics — the Docker smoke test asserts
it on every build):

```jhs
<?jhs
  require("url");                       // Node built-in — see the table
  function myfunc(p) {
    return { a: "hi", b: "bye", c: 234 };
  }
  echo(JSON.stringify(myfunc(query.probe)))   // query: the request's
?>                                             // query string, object
```

`GET /probe.jhs?probe=7` → `{"a":"hi","b":"bye","c":234,"probe":"7"}`
(auto-escaped on `echo`; `raw()` for trusted HTML, as everywhere).

| Capability | `backend = "boa"` | `backend = "sidecar"`/`"auto"` + Node |
|---|---|---|
| globals `query`, `user`, `path`, `pages`, `menus` | yes | yes |
| `echo(…)`, `<?= ?>`, `raw()` | yes | yes |
| `require("./module")` from `modules/` | yes | yes |
| `require("url")` (Node built-ins) | **no — 500 with an explanatory banner** | yes |

The shipped default is `backend = "auto"`: the Node sidecar whenever
Node is on `PATH` (always inside the Docker image), boa as the
fallback. Plain JS modules under `modules/` resolve in both; **Node
built-ins like `url` only exist through the sidecar** — the boa
sandbox answers with a clear error, not a silence.

One clarification that saves a trap: **there is no "fetch without
JavaScript"** — a JS-free page cannot make requests on its own. The
no-JS pattern is the one this whole panel uses: a form `GET`/`POST`s,
the server re-renders. A `.jhs` template *can* read its own query
string (like the probe above) — but composing another endpoint's
output into a page is the proxy's job (`[external_api]`), not a
template's.

## The media library (F9)

Pages need images. `/admin/media` is where they live: editors upload
through a plain `multipart/form-data` form (the only place the panel
uses multipart — everything else stays urlencoded), the server
validates, stores and serves, and page authors copy a ready-made
snippet. Still zero JavaScript: no drop zones, no progress bars, no
crop widgets — a form, a redirect, a page.

### The three validation gates

Every upload passes all three before a single byte touches the disk:

1. **The sniff.** The format is derived from the file's leading bytes
   (`image::guess_format` — real magic-byte detection), never from the
   client's `Content-Type` or the file name. The sniffed format is also
   what gets stored and later served, so a served file can never lie
   about what it is.
2. **The whitelist.** PNG, JPEG, GIF and WebP — the raster formats
   every browser renders natively. SVG is deliberately absent: it is
   text, it can carry scripts, and the CMS's scripts-blocked story
   should not depend on sanitizing an attacker-controlled one. A
   sniffed-but-disallowed format (BMP, TIFF, …) bounces back with
   «Formato no admitido».
3. **The decode.** The file is fully decoded under
   `image::Limits` — at most 8192 × 8192 pixels and 512 MiB of decoder
   allocation (the decompression-bomb guard) — which also proves the
   file is well-formed: a truncated or corrupt "PNG" never reaches
   the disk. Decode and thumbnailing run on the blocking thread pool,
   not the async runtime.

The `media_max_bytes` cap is enforced while *streaming* the upload —
the file is never buffered past the limit, so the memory cost of a
malicious upload is bounded by the cap itself.

### Storage: flat, server-generated, never in `public/`

Files land in `[cms] media_dir` (default `media/`, created at
startup) under names the server generates: `<32-hex>.<ext>` for the
file and `<32-hex>_t.png` for its 320-pixel PNG thumbnail. Nothing the
client sent — not the name, not the path, not the extension —
influences the on-disk name, so the serving routes can never be talked
into a path traversal; schema `CHECK`s back the flat shape and the
four-mime whitelist in the database. The library **never writes into
`public/`**, and media is served by its own routes (with row lookups
and cache headers) rather than the static file family.

The uploader's original file name is kept for display only (last
path segment, control characters stripped, 200 characters), and the
`alt` text travels with the upload.

### Serving: immutable by construction

| Route | Who | What |
|---|---|---|
| `GET /media/{id}/{name}` | public | The file, byte-for-byte, with the sniffed `Content-Type`. |
| `GET /media/thumb/{id}` | public | The 320-pixel PNG thumbnail (falls back to the full file if missing). |
| `GET /admin/media` | editor/admin | The grid + the upload form. |
| `GET /admin/media/{id}` | editor/admin | Detail: full image, metadata, alt form, snippets, delete. |
| `POST /admin/media` | editor/admin | The upload (multipart: `file` + `alt`). |
| `POST /admin/media/{id}/alt` | editor/admin | Replace the alt text. |
| `POST /admin/media/{id}/delete` | editor/admin | Delete the row and the files. |

The URL name must match the row exactly — a wrong name, an unknown id
or a traversal attempt all land in the same 404 as `/p/{slug}` does.
Because every upload gets a fresh id and a fresh name, the bytes under
a given URL never change: the responses carry
`Cache-Control: public, max-age=31536000, immutable`, and a browser
fetches a media URL at most once. (Deleting a file breaks that URL by
design — see below.)

### The editor workflow

Upload → the detail page shows the image, its metadata, and two
snippets ready to copy-paste — the Markdown one carrying the alt text:

```markdown
![Logo del sitio](/media/3/8f14e45fceea167a5a36dedd4bea2543.png)
```

The alt text is editable on the same page (500 characters, the SEO
budget), and the snippets regenerate with it — the same
accessibility-first loop the Markdown renderer's `![alt](url)`
syntax expects.

### Deleting, and the limits of the library

Deleting removes the row and both files; the public URL then answers
404, and any page still referencing it shows a hole. The detail page
warns about exactly that before the button — finding *which* pages
reference a file is search work (F10). The listing shows the latest
200 items (full pagination is F10's business too).

## Configuration

Two keys from F7, two from F9 (all optional) — and **F8 adds zero
keys**: the body mode is per-page state in the database, not server
configuration, so the `wallermax.toml` boundary stayed untouched
through it.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `cms.sitemap` | bool | `true` | Serve `GET /sitemap.xml` with the published pages. |
| `cms.site_url` | string | unset | Absolute origin for the sitemap's `<loc>` URLs; without it the request's `Host` header is used with `http://`. |
| `cms.media_dir` | path | `"media"` | Directory the media library stores uploads in (created at startup; its own directory — never `public/`). |
| `cms.media_max_bytes` | bytes | `524288` | Cap on one uploaded file; the upload is streamed and refused past the cap. |

Environment overrides: `WALLERMAX_CMS__SITEMAP`,
`WALLERMAX_CMS__SITE_URL`, `WALLERMAX_CMS__MEDIA_DIR`,
`WALLERMAX_CMS__MEDIA_MAX_BYTES`. The keys are validated while the CMS
is off too — a typo'd `site_url` or an out-of-range `media_max_bytes`
(1 KiB .. 64 MiB) is a startup error regardless of the switch.

**The one interplay worth knowing** (F9): the default `512 KiB`
`media_max_bytes` fits under the default 1 MiB
`server.max_body_size_bytes` with multipart framing headroom, so
uploads work out of the box. Raise `media_max_bytes` for real
photography and raise `server.max_body_size_bytes` with it — the
request body limit rejects oversized uploads with a 413 *before* the
friendly form error can fire, and a startup warning tells you when
the keys are out of step.

## Security notes

- **No new client-side surface.** Zero JavaScript, zero CSP changes,
  zero new external requests. F7's capabilities and F8's editor are
  server-rendered forms and templates like the rest of the panel.
- **Auto-escape still guards every print.** Menu labels, page titles
  and metadata reach HTML through `<?= ?>` (escaped); the resolved
  `menus` global is plain data for templates.
- **Markdown bodies are safe by construction** (F8): raw HTML is
  dropped, link/image schemes are filtered to `http(s)`/`mailto`/`ftp`
  and relatives, text is escaped by the writer — the fragment the
  wrapper injects with `raw()` is trusted **because the renderer
  produced it**, never because the editor wrote it. The mode is
  stored behind a schema `CHECK`.
- **The preview never writes.** `POST /admin/pages/preview` is
  editor-only, renders, and re-renders the form — no row, no draft,
  no `updated_at` bump; `page_id` is round-trip data, never a command.
- **Shape validation everywhere**: positions are bounded integers,
  `og_image` and menu URLs must be paths/`http(s)`/anchors, menu names
  follow the slug rule, and the page-xor-url menu item shape is
  enforced by the forms *and* by a `CHECK` in the schema.
- **The privilege split is untouched**: menus are content (`editor`),
  not server configuration; nothing new crossed the
  `wallermax.toml`-only boundary.
- **Media is validated at the byte level** (F9): the sniffed magic
  bytes (not the client's `Content-Type`) decide the format and the
  whitelist (PNG/JPEG/GIF/WebP — no SVG, ever), and the full decode
  under dimension/allocation limits rejects truncation and
  decompression bombs before anything is written. The upload is
  streamed with the cap enforced mid-read, never buffered unchecked.
- **Media serving cannot traverse**: on-disk names are flat,
  server-generated hex stems (`CHECK`-constrained in the schema), the
  lookup key is the id, and the URL name must match the row exactly —
  wrong names, unknown ids and `../` attempts all land in the same
  404. `Cache-Control: immutable` is safe by construction: every
  upload is a new id and a new name, so the bytes under a URL never
  change.

## The CMS roadmap

The line towards "a basic WordPress, without the weight", scoped with
the same discipline as the phases before it:

- **F7** (done): hierarchy, menus, SEO, sitemap.
- **F8 — the editor** (done): Markdown bodies with the safe subset
  and the server-side previsualización, stored alongside the `.jhs`
  body mode — this document.
- **F9 — the media library** (done): uploads with magic-byte
  validation, size caps, thumbnails, alt text — served from a dedicated
  directory, never writing into `public/` — this document.
- **F10 — findability**: search over pages (SQLite FTS5), paginated
  listings, RSS/Atom for dated content.
- **F11 — history**: page revisions with restore, scheduled
  publishing.

Each phase ships as one patch with tests and this document updated;
nothing lands half-featured.
