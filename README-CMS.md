# The wallermax CMS — the corporate content model (F7) & the editor (F8)

This is the companion deep-dive for the CMS feature line: the main
[README.md](README.md) keeps the essentials and links here, so the
front door of the repository stays short while the content features
get the room they deserve.

**F7** adds four corporate-site capabilities and **F8** the editor —
all of them **zero-JavaScript** (the CSP keeps blocking scripts and
every new screen works through plain HTML forms):

| Capability | One line |
|---|---|
| [Page hierarchy](#page-hierarchy) | Parents, sibling ordering, breadcrumbs and a cycle-proof move guard. |
| [Menus](#menus) | Named navigation menus rendered by any template through the `menus` global. |
| [SEO metadata](#seo-metadata) | Per-page `meta_title`, `meta_description` and `og_image` in the wrapper's `<head>`. |
| [Sitemap](#sitemap) | `GET /sitemap.xml` with every published page, automatically. |
| [The editor (F8)](#the-editor-two-body-modes) | Markdown bodies with a server-side preview — and the `.jhs` mode kept as-is. |

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

## Configuration

Two keys from F7 (both optional) — and **F8 adds zero keys**: the
body mode is per-page state in the database, not server
configuration, so the `wallermax.toml` boundary is untouched.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `cms.sitemap` | bool | `true` | Serve `GET /sitemap.xml` with the published pages. |
| `cms.site_url` | string | unset | Absolute origin for the sitemap's `<loc>` URLs; without it the request's `Host` header is used with `http://`. |

Environment overrides: `WALLERMAX_CMS__SITEMAP`,
`WALLERMAX_CMS__SITE_URL`. The keys are validated while the CMS is off
too — a typo'd `site_url` is a startup error regardless of the switch.

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

## The CMS roadmap

The line towards "a basic WordPress, without the weight", scoped with
the same discipline as the phases before it:

- **F7** (done): hierarchy, menus, SEO, sitemap.
- **F8 — the editor** (done): Markdown bodies with the safe subset
  and the server-side previsualización, stored alongside the `.jhs`
  body mode — this document.
- **F9 — the media library**: uploads with magic-byte validation,
  size caps, thumbnails, alt text — served from a dedicated directory,
  never writing into `public/`.
- **F10 — findability**: search over pages (SQLite FTS5), paginated
  listings, RSS/Atom for dated content.
- **F11 — history**: page revisions with restore, scheduled
  publishing.

Each phase ships as one patch with tests and this document updated;
nothing lands half-featured.
