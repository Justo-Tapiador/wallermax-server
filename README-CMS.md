# The wallermax CMS — the corporate content model (F7), the editor (F8) & the media library (F9)

This is the companion deep-dive for the CMS feature line: the main
[README.md](README.md) keeps the essentials and links here, so the
front door of the repository stays short while the content features
get the room they deserve.

**F7** adds four corporate-site capabilities, **F8** the editor,
**F9** the media library — all of them **zero-JavaScript** (the CSP
keeps blocking scripts and every new screen works through plain HTML
forms). Later phases (search, history, the F12 panel redesign and
F13's English-everywhere pass) build on the same spine — see the
[roadmap](#the-cms-roadmap).

| Capability | One line |
|---|---|
| [Page hierarchy](#page-hierarchy) | Parents, sibling ordering, breadcrumbs and a cycle-proof move guard. |
| [Menus](#menus) | Named navigation menus rendered by any template through the `menus` global. |
| [SEO metadata](#seo-metadata) | Per-page `meta_title`, `meta_description` and `og_image` in the wrapper's `<head>`. |
| [Sitemap](#sitemap) | `GET /sitemap.xml` with every published page, automatically. |
| [The editor (F8)](#the-editor-two-body-modes) | Markdown bodies with a server-side preview — and the `.jhs` mode kept as-is. |
| [The media library (F9)](#the-media-library-f9) | Image uploads validated at the byte level, immutable-cached serving, alt text and copy-paste snippets. |
| [English everywhere + the shared error page (F13)](#english-everywhere--the-shared-error-page-f13) | Content-negotiated HTML error pages, the self-healing 429, the anglicized public site and the English routes. |

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
| globals `query`, `user`, `path`, `pages`, `menus`, `cms_origin` | yes | yes |
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

A relative `media_dir` resolves against the working directory, so a
bare `wallermax-server` run keeps its media next to itself. In the
container image the story is different: the app user cannot write under
`/app`, so the image pins `WALLERMAX_CMS__MEDIA_DIR=/data/media` —
uploads share the `/data` volume with the SQLite database (site
content, not image content) and survive container replacement the same
way the database does. A startup failure to create the media root is
deliberately fatal: the server refuses to run with a media library it
cannot write to.

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
reference a file is one search away now: the admin filter
(`GET /admin/pages?q=<the 32-hex stem>`) matches the URL inside page
bodies (F10). The grid is paginated, 24 thumbnails per page.

## Findability: search, pagination, feeds (F10)

Content nobody can find is content that does not exist. F10 gives the
published tree three discovery paths, all server-rendered, all
JavaScript-free:

| Route | Who | What |
|---|---|---|
| `GET /search?q=…` | public | The search page: ranked hits with highlighted snippets. |
| `GET /p?page=N` | public | The pages index, one window at a time (`[cms] index_page_size` rows). |
| `GET /feed.xml` | public | RSS 2.0 with the newest published pages. |
| `GET /atom.xml` | public | The same entries as Atom 1.0. |
| `GET /admin/pages?q=…` | editor/admin | The flat, ranked filter over every page, drafts included. |

### Search: FTS5, with the visitor's words defanged

The index is a SQLite FTS5 table over the pages' `title` and
`content` — the **stored source**, not a rendered projection. That is
deliberate: the admin filter's job includes finding *references* (a
media URL inside a body, an include), which a rendered-text index
would hide. A classic external-content table with insert/update/delete
triggers keeps index and rows in lockstep, and a one-command
`rebuild` in the migration makes every pre-F10 page searchable the
moment it applies.

The visitor's query never reaches `MATCH` as typed: the repository
(`fts_match_query` in `src/db.rs`) quotes every whitespace token into
an inert phrase, so FTS5's own operators (`OR`, `NOT`, `*`, column
filters) can only act as literals, and embedded quotes are stripped
rather than escaped. Operators are words here, not syntax.

Ranking is `bm25`. Snippets come from SQL `snippet()` with hits
wrapped in `⟦ ⟧` markers — the handler splits those into
`{ texto, hit }` segments and the view prints each one through the
auto-escaping `<?= ?>`, wrapping the hits in `<mark>`. No `raw()`, no
pre-escaped HTML: a page body cannot reach a browser as markup
through a search result, and a faked marker can at worst split a
segment (cosmetic).

The public search sees **published pages only** — drafts are the
admin filter's business. An empty or quotes-only query renders the
page with an invitation, never an error. The shared header carries
the search form on every page (`action="/search"`, plain GET).

### Paginated listings

`GET /p` is a real route now (it was an auto-routed view): same
`views/p.jhs`, but fed one window of the listing plus a `paginacion`
block — current page, totals, prev/next links. Out-of-range and
garbage `?page=` values clamp to the nearest real page instead of
erroring. Search results paginate with the same block, carrying the
query along in the links (`/search?q=…&page=2`), and the media grid
follows at 24 thumbnails per page.

One key sizes the public lists: `[cms] index_page_size` (default 10,
validated 1–100). The admin grids size themselves — a panel is not a
public listing.

### The feeds

`/feed.xml` (RSS 2.0) and `/atom.xml` (Atom 1.0) carry the newest
**published** pages, 20 at most — a feed is a window over the site,
not an archive. Item titles and absolute links follow
`[cms] site_url` (or the request's `Host` header, the sitemap's exact
fallback rule); dates are RFC 822 and RFC 3339 respectively, always
UTC. Descriptions come from each page's SEO `meta_description` —
unset simply omits the element. Both are public information like the
`/p` index, so the switch defaults to on: `[cms] feed = false` turns
them into 404s.

## History: revisions and scheduled publishing (F11)

Edits are cheap to make and expensive to regret. F11 records every
save and lets time do the publishing — both server-side, both
JavaScript-free:

| Route | Who | What |
|---|---|---|
| `GET /admin/pages/{id}/history` | editor/admin | The revision list: newest first, authors and notes included. |
| `GET /admin/pages/{id}/history/{revision}` | editor/admin | One snapshot in full — the body as escaped source. |
| `POST /admin/pages/{id}/history/{revision}/restore` | editor/admin | Copy the snapshot back onto the page, as a new revision. |

### Every save is a snapshot

Creating, editing, restoring — each lands the page row **and** its
revision in one SQLite transaction, so a page without history (or
history without a page) cannot exist. The snapshot carries every
editable field (title, slug, body, format, parent, position, SEO)
plus the state as saved, the editor who saved, and their optional
one-line note — the «qué cambió» the form now offers. Deleting a page
deletes its revisions; deleting an editor keeps them
(`ON DELETE SET NULL`, like the page's own author link). The
snapshot's `parent_id` deliberately carries no foreign key: it
records where the page hung *at the time*, and the parent it names
may legitimately be gone years later — the restore route
re-validates it against today's tree instead.

`[cms] max_revisions` (25 by default, `0` = unlimited) prunes the
oldest snapshots **inside the same write** — history never exceeds
the cap, not even for a moment.

### Restoring is append-only

Restoring copies an old snapshot onto the page as a **new** revision,
noted «Restaurada desde la revisión N.» — the restore itself is
recorded, and reversible like everything else. Content only, never
state: the live `is_published` flag and schedule are the editor's
current call, so a restore never publishes or unpublishes anything.
Today's world is re-validated before anything moves — a slug another
page has taken since, or a parent that no longer exists (or now
hangs below the restored page), bounce back as inline form errors
without appending a revision.

The revision detail page shows the body as **escaped source**: a
revision is never executed, not even for the editor reading it.
Restoring is what puts content back into the normal, previewable
pipeline.

### Scheduled publishing: the reads decide

A draft with a future `publish_at` becomes publicly visible the
moment the clock passes it — there is no background task, no cron,
nothing to keep running: every public read (the `/p` index,
`/p/{slug}`, the FTS5 search, both feeds, the sitemap, the menu
resolution) evaluates «published = the flag is set **or** the
schedule has elapsed» at query time. A page goes live exactly on the
second and behaves identically after a restart — the flag stored in
the database never has to flip.

The panel speaks the same truth: while pending, the page badges
«programada» and editors see a banner with the exact moment; once
elapsed, the badges and the edit form's checkbox say «publicada»,
because that is what a visitor sees. Saving normalizes the column: a
published page carries no schedule (the flag is the whole truth),
and a date already in the past is spent and dropped — unchecking
«Publicada» on a formerly scheduled page unpublishes for real
instead of resurrecting the old date. The field is UTC
(`datetime-local`), the panel clock every `*_h` field already
displays.

## The admin panel (F12): app shell, tokens and the no-JS theme

Until v0.15.0 the panel shared the public site's chrome — the same
header, the same stylesheet, the same Spanish voice. F12 gives it its
own **app shell**: a fixed sidebar (collapsing to a 72px icon rail on
small screens), a sticky topbar with a real pages filter, cards,
tables, status pills, a two-column editor layout and a media grid —
`public/assets/admin.css`, a design system independent from the
site's `wallermax.css`. The panel speaks English now — and F13 later
carried that English voice over to the public site, the feeds and
the shared error pages.

The redesign is **zero new keys, zero JavaScript and one new route**:

- **Partials**: `views/partials/admin/` — `head` (document head +
  `data-theme`), `sidebar` (navigation, user mini, sign-out), `topbar`
  (search + theme toggle), `foot`, and `pagination` (numbered page
  buttons with `aria-current`, a window around the current page).
  Views define `titulo` before including `head` — the compile-time
  `include()` runs in one program, so the variable is in scope.
- **`GET /admin/theme?to=dark|light&back=<path>`**: the no-JS dark
  mode. The link pins an HttpOnly, one-year, `SameSite=Lax`
  `wm_theme` cookie and bounces straight back; `base_data` turns the
  cookie into the `theme` global (only the two literal values survive
  — anything else reads as "no preference"). While no cookie exists
  the stylesheet follows the OS via `prefers-color-scheme`; `light`
  pins the light tokens explicitly. `back` is honored only as a
  printable `/admin` path without `..` — the toggle can never become
  an open redirect or a header-injection sink, and anonymous visitors
  hit the editor gate as always.
- **The dashboard grew up**: the counters (now including media files
  and scheduled pages) are stat cards, and two new cards read the
  database — *Recent activity* (the newest saves across every page:
  editor, note and time — the cross-page view of the F11 history) and
  *Needs attention* (drafts with a pending schedule, the soonest
  first, plus the drafts waiting to be published).
- **CSP-clean by construction**: the shipped policy is
  `style-src 'self'` without `'unsafe-inline'`, which silently
  **dropped** the old tree's `style="padding-left:…"` indentation in
  browsers. The pages tree now indents through `.depth-N` classes —
  the whole panel renders with zero inline style attributes (an
  assertion in the test battery keeps it that way).
- **Anglicized server messages**: every form-error and flash message
  the panel renders — page/menu/user validation, uploads, imports,
  restores — is English now, and `human_bytes` formats sizes with a
  decimal point. The auto-restore note reads "Restored from revision
  N." The state codes behind the pills are `published` / `scheduled`
  / `draft`.

What deliberately did **not** change in F12: the form field names,
the routes, the `?ok=` flash codes, the role gates and the profile/login
pages — every POST flow from v0.15.0 keeps working unchanged. (F13
later anglicized the public surface and the flash codes; the forms
themselves kept their field names.)

## English everywhere + the shared error page (F13)

F12 gave the panel an English voice; F13 finishes the job for every
other surface a visitor can see, and — more importantly — it stops
the server from answering **people** with **JSON**.

### The shared error page: content negotiation

Until F13, every 4xx/5xx was the JSON envelope — perfect for API
clients, alien for a browser. The trigger was mundane: a fast review
of the panel can trip the per-IP burst limiter, and the theme
toggle's 303 had already pinned the cookie when the 429 JSON
appeared (which is why a manual refresh "fixed" the panel).

The fix is a new middleware, `src/middleware/error_pages.rs`, second
in the pipeline (inside the security-headers layer, outside CORS,
the request id and the limiter, so it sees every envelope the server
produces). On a response that is 4xx/5xx **and** carries the JSON
error envelope **and** was asked for with an `Accept` that prefers
`text/html`, it swaps the body for the shared English error page and
keeps everything else — status, `Retry-After`, `X-Request-Id`, the
rate-limit fields. The negotiation model is deliberately simple
because the site is script-free: every HTML-preferring request **is**
a human navigation. Stylesheets, images and `Accept: */*` (curl,
health checks, Prometheus) keep the envelope byte-for-byte, so every
API contract and test written before F13 still holds.

The page itself — `HtmlErrorPage` in `src/error.rs`, styled by
`public/assets/error.css` — is script-free and inline-style-free
(the CSP serves it untouched), shows the status, the code, the
message, the request id (for log correlation) and the way-out links
(401s lead with *Sign in*), carries `noindex`, and **honors the
pinned panel theme**: the `wm_theme` cookie is read in Rust, so an
editor with a dark panel gets dark error pages. Two hand-built
pre-F13 error pages (`html_error_page` in cms.rs, `form_error_response`
in auth.rs) were deleted; their call sites now raise standard
envelopes negotiated by the same middleware — one error system, not
three.

### The 429 self-heal

A 429 page carries `<meta http-equiv="refresh" content="N">` with `N`
taken from `Retry-After` (capped at 5 s): the browser reloads on its
own once the token bucket has refilled, no F5 needed. The toggle
flow that surfaced the bug now finishes by itself.

### The anglicized public surface

Every shipped view speaks English now: the shared `partials/header`
(the `#register` modal id — it was `#registrar` —, the sign-in and
sign-up messages), `footer`, `paginacion` (Previous/Next, "Page N of
M"), `index`, `p`, `search`, `cms_page` (the draft banner, the
breadcrumbs, "updated …"), `404`, `login`, `register`, `profile` and
`contact`. The public demo `public/hello.jhs` is English too. Feeds
carry `<title>Pages — wallermax</title>`, the English description and
`<language>en</language>`. The flash codes in URLs are English —
`?login_error=invalid|post_register`,
`?register_error=taken|username|password|closed|error`,
`?pw_error=current|new|form|session|error`, `?ok=password` (public)
and `?ok=created|saved|deleted|restored|item-*` (panel) — the views'
dictionaries match, and old links with Spanish codes simply fall
back to the generic message. The CMS role errors and the media 404
message are English as well.

### English URLs, Spanish aliases

| Now | Was | Behavior |
|---|---|---|
| `/search?q=…` | `/buscar?q=…` | `301`, query preserved (route alias). |
| `/register` | `/registro` | `301` via a one-line `res.redirect` stub view. |
| `/profile` | `/perfil` | `301` via a stub view. |
| `/contact` | `/contacto` | `301` via a stub view. |
| `POST /profile/password` | `POST /perfil/password` | `307` — the POST replays verbatim at the new path. |

The old spellings are kept working on purpose: bookmarks, embedded
forms and muscle memory survive the rename. Data contracts under
the hood (the `paginacion` block keys, `resultados`, `fragmento`…)
keep their names — they are template-internal, documented, and
renaming them would break every view already written against them.

## Virtual hosts (F14): one server, two names

`public/` and `views/` were always two roots pretending to be one
site. F14 makes the split real: while `[cms] hosts` names hostnames,
the server becomes a **name-based virtual host** — the pattern Apache
calls vhosts and Nginx calls server blocks — serving two (or more)
sites from the same IP and port, routing by the request's `Host`
header.

```toml
[cms]
hosts = ["cms.localhost"]           # or your own domain(s)
```

The **main host** (every name not in the list, unknown or missing
included — fail safe) serves what an operator runs: the static site
under `[static] root_dir`, the `.jhs` files living there rendered on
the fly, and the machinery — `/api`, `/api/auth/*`,
`/api/admin/users`, `/health`, `/metrics`, the external proxy. Its
homepage is its own chain: a `public/index.jhs` rendered when one
exists, else the static `public/index_file`.

The **CMS host** serves the visitor surface: the public pages, the
`views/` auto-routing, the panel, the media library, search, feeds
and the sitemap. `/api/auth/*` rides along for one reason: the
no-JS login modal and login page POST to `/api/auth/login`, and an
HTML form needs its endpoint on the same origin — and the session
cookie that POST pins is host-only, so the editor session never
leaks to the main host. The CMS host also borrows exactly three
things from the static root: `/assets/*` (the shared stylesheets),
`/favicon.ico` and `/robots.txt` — same-origin under the CSP's
`style-src 'self'`, and nothing else: `public/`'s content never
duplicates onto the CMS host, and `.jhs` sources never render or
serve there (their raw bytes are never served anywhere either).

### The homepage chain, host by host

Both hosts resolve `/` through an explicit chain, and the main
host's can be dynamic: drop an `index.jhs` beside the static
`index.html` and it takes the homepage, rendered through the
sandboxed engine with the usual globals (`user`, `path`, `query`,
`pages`, `menus`, `req`, `cms_origin` — the main site can list the
CMS's published pages and link the CMS host too). The order, most
specific first:

- **main host**: `public/index.jhs` (rendered) → `public/index_file`
  (static, `index.html` by default) → JSON 404;
- **CMS host**: `cms.default_page` (the `/p/{slug}` pipeline) →
  `views/index.jhs` (auto-route) → JSON 404;
- **single host** (empty `hosts`): `cms.default_page` →
  `public/index.jhs` → `public/index_file` → `views/index.jhs` →
  JSON 404 — the explicit configuration beats every filesystem
  convention, and the static root beats the views root.

The same `index.jhs`-first rule covers every **directory** on the
main host: `/docs/` renders `public/docs/index.jhs` when present,
else serves `public/docs/index.html` — and `/docs` (no trailing
slash) reaches the same place through the static layer's own
add-the-slash redirect. Rendered indexes answer `no-store` with the
full middleware pipeline around them (security headers, the F13
error page on failure); the source bytes are never served, exactly
like every other `.jhs` under the static root. No new keys: the
chain is a convention, like the `.jhs` extension itself.

### Cross-host links: the `cms_origin` global

The split has a consequence a navigation bar meets immediately:
the CMS surface — `/login`, `/admin`, the `/p` pages — answers on
the CMS host only, and the main host's 404s stay 404s by design. A
`public/` template cannot just write `href="/login"`. Hardcoding
`https://cms.example.com` works until the domain changes; the
`cms_origin` global keeps templates environment-agnostic:

```jhs
<nav>
  <a href="<?= cms_origin ?>/login">Sign in</a>
  <a href="<?= cms_origin ?>/admin">CMS panel</a>
</nav>
```

The contract is one sentence: **the CMS surface's origin, or the
empty string while every surface shares one host** — so
`<?= cms_origin ?>/login` is the relative `/login` before the split
and the absolute `https://cms.example.com/login` after it, from
the same template, with zero new configuration. While `cms.hosts`
is set the origin is the **first** entry (the canonical name) with
the scheme from `[tls] enabled` and a non-default `[server] port`
tagging along; `cms.site_url` overrides the derivation — the same
key the sitemap and feeds already trust, so a reverse-proxy
deployment fixes the links and the feeds with one value.

Two things deliberately do **not** need the origin:

- **`/api/auth/*` rides both hosts**, so forms post same-origin.
  The logout control is the panel's own pattern — a POST form,
  because the endpoint is POST-only (a bare link answers 405):

  ```jhs
  <form method="post" action="/api/auth/logout">
    <input type="hidden" name="redirect" value="<?= path ?>">
    <button type="submit">Sign out</button>
  </form>
  ```

- **The session cookie is host-only** (`HttpOnly`, `SameSite=Strict`,
  no `Domain`): signing in on the CMS host does not identify you on
  the main host. A public site that greets signed-in users needs
  its own no-JS login form on the main host — the login modal
  pattern of `views/partials/header.jhs`, posting to the
  same-origin `/api/auth/login`. Sharing one session across hosts
  would mean a `Domain` cookie — a deliberate non-feature so far:
  the editor session never leaks to the static site.

The origin is public information by construction — a name the
server publishes anyway — so the global reaches every render
(`base_data`), CMS pages included: both hosts' templates can never
disagree about it.

### The mechanics, deliberately boring

Classification is `src/vhosts.rs`, pure functions: the host is
the URI authority (HTTP/2) or the single `Host` header (HTTP/1.1),
compared case-insensitively after stripping the port. A missing
header (an HTTP/1.0 relic) or a **duplicated** one (a request shaped
like header smuggling) classifies as the main host. The same pure
function answers in the dispatcher (`routes::vhost_routes`, a
`service_fn` over two route trees) and in the templates middleware,
so the two layers agree on every request by construction; the
middleware pipeline wraps the dispatcher from the outside, so the
security headers, the F13 error pages, rate limiting and friends
serve both hosts identically.

Everything else stays put: the whole pipeline, the panel's forms,
the role gates, the theme cookie. Empty `hosts` (the default) keeps
the single-host server of every phase before F14 — behaviour the
entire pre-F14 test suite pins down.

### Details that matter

- **Feeds and the sitemap follow the serving host.** They already
  built absolute URLs from `cms.site_url` or, absent that, the
  request's `Host` header (F7) — so on the CMS host they emit
  `http://cms.example.com/…` (or the configured `site_url`) with no
  changes at all. Behind TLS or a reverse proxy, `site_url` is the
  right key, as it always was.
- **TLS needs one certificate for every name** (SAN entries —
  mkcert covers `localhost` + `cms.localhost` locally). The
  plain-HTTP redirect listener keeps working; per-name SNI
certificates are out of scope.
- **Local testing**: modern browsers resolve `*.localhost` to
  127.0.0.1 already — `http://cms.localhost:8080` just works; for
curl, add an entry to the hosts file. A real deployment needs a DNS
  record pointing the (sub)domain at the same IP — same port, same
  binary.
- **Validation**: entries must be bare hostnames (no scheme, port,
  path or whitespace — IDN names go in punycode); `hosts` requires
  `cms.enabled` and `static.enabled`, because the CMS host borrows
  its styles from the static root. Entries are normalized at load:
  trimmed, lowercased, one trailing DNS dot tolerated.

## Organizations and memberships (F15)

F14 split the server by `Host` header — two sites, one binary — but
the split had no teeth: the only thing keeping the main host's
operator out of the CMS panel (and vice versa) was the cookie being
host-only, an accident of scope, not authorization. A token claiming
`role: admin` was admin **everywhere**. F15 gives the tenants an
identity in the database and makes membership, not the global role,
the thing the CMS guards read:

```text
organizations          memberships
───────────────        ─────────────────────────────
key: main      ◄────►  user ─┐
  root: public/               ├─ organization ─ role
key: cms        ◄────►  user ─┘
  root: views/
```

Two tables, one migration (`0009`), zero new configuration:

- **`organizations`** — one row per tenant, seeded at every boot
  from the configuration the server already has: `main` (the static
  site, `[static] root_dir`) and `cms` (the CMS, `[templates]
  views_dir`). The `key` is the stable identifier the code addresses
  tenants by; name and document root are display data the seed keeps
  in sync, so the rows always describe what `wallermax.toml` says.
- **`memberships`** — user x organization x role (`admin` or
  `editor`), the enforcement point. `CmsEditor` and `CmsAdmin` (the
  guards behind `/admin/*`, the media library and the panel forms)
  now answer this question: *is the authenticated user a member of
  the CMS organization, and at what rank?* The platform role in the
  token no longer opens the panel by itself.

### The F15 invariant: membership mirrors the platform role

On purpose, F15 ships with **zero visible behaviour change**: every
`admin`/`editor` account keeps exactly the access it had, and every
panel form keeps working the same way. The repository maintains the
mirror on every write — `create`, `update_role` and `delete` move
the membership with the role — and a startup pass
(`mirror_cms_memberships`) upgrades databases that predate F15 (and
heals any drift a role changed by hand in SQL leaves behind):

```text
role admin  ──► membership admin  of cms
role editor ──► membership editor of cms
role user   ──► (no membership)
```

The panel's role dropdown is still the operator-facing control; the
membership is the enforcement it drives. The later phases (the
`domains` table, per-organization roles in the panel) will retire the
mirror and let the two planes diverge on purpose — the table is
already general.

### What changed where

- **The guards** (`src/routes/cms.rs`): `CmsEditor` and `CmsAdmin`
  run one indexed SQLite read per protected request —
  `membership_role(user_id, "cms")`. Anonymous visitors still get
  the `303` to `/login`; authenticated non-members get the same HTML
  403 page as before, with a membership-shaped message.
- **The token stays identity-only** — `sub`, `username`, `role`, no
  organization. That is the point: because the membership is read
  per request on protected surfaces, a demotion (or a hand-edited
  membership) closes the door **immediately**, without waiting for
  access tokens to expire. The integration battery pins this down:
  the same unexpired `editor` token gets `/admin/pages` → 403 the
  moment the membership is gone.
- **`users.role` stays** as the platform-level role: the bootstrap
  rule (first registered account becomes admin), the last-admin
  guard of the user management, and the main host's
  `GET /api/admin/users` (`AdminUser`) all keep reading it. Platform
  machinery vs CMS content — two planes, both real.
- **Draft previews** (`/p/<slug>`, `cms.default_page`) still read the
  token's role for the viewer-is-editor check: under the mirror the
  two are equivalent by construction, and the public page path
  stays free of per-request membership reads. A demoted editor's
  unexpired token can still *preview* a draft until it expires — it
  can no longer *manage* anything. The data-driven phases will
  revisit this together with per-request organization resolution.
- **Registration posture** (recommended, matches the multi-tenant
  model): register the first account, then set
  `auth.registration_enabled = false` — from then on, accounts are
  created by an administrator in `/admin/users` with exactly the
  access they need. Membership, not open sign-up.

### The two-tenant picture today

```text
        localhost                     cms.localhost
            │                              │
     organization main            organization cms
     (root: public/)              (root: views/)
     public static site           public pages + panel
     no protected surface         gated by memberships
     (platform machinery:         (admin/editor members)
      /api, /health, /metrics)
```

The `main` organization exists as a row with no gates yet — its
administrator becomes meaningful the moment a protected surface
appears on the main host (the later phases). What F15 buys today is
the enforcement spine: **one user table, one session layer, and a
membership table deciding who may enter which tenant** — so the
follow-up phases can add real second administrators, per-organization
roles and the `domains` table without touching the auth core again.

## The domains table (F16)

F14 split the server by `Host` header and F15 gave the tenants a
database identity, but the split's truth still lived in
`wallermax.toml`: the `[cms] hosts` list, read at load time, and
nothing else. F16 moves that truth into the database — one table,
`domains` (migration `0010`), one row per hostname:

```text
domains
───────────────────────────────────────────────────
hostname  UNIQUE    e.g. 'cms.example.com'
organization_id      -> organizations.id ('cms', ...)
source    'config' | 'manual'   (default: 'manual')
```

At every boot the server seeds the `[cms] hosts` list into the table
and then **serves the CMS tree for exactly the hostnames the table
maps to the CMS organization** — dispatcher and templates middleware
read the same loaded list, so the two layers keep agreeing on every
request by construction. Every other host (unknown, missing, or
mapped to a different organization) gets the main tree, exactly as
before.

### Two sources, one provenance rule

The `source` column is what keeps the transition surprise-free:

- **`config`** — the seeder's own rows. They follow the `[cms] hosts`
  list one-for-one: a hostname added to the list is upserted (and a
  hand-made row with that hostname is *reclaimed* as the seeder's), a
  hostname removed from the list has its row deleted. Config edits
  behave exactly as they did in F14.
- **`manual`** — everything else, and the default for a hand-written
  `INSERT`. Data. The seeder never reads these rows: they survive
  every boot, whatever the configuration says.

The result is the honest reading of "configuration as bootstrap":
the list still works exactly as F14 promised, **and** the table is
real — a row you create by hand serves the CMS with the list empty:

```sql
-- vhosts with zero configuration (then restart; the table loads at boot):
INSERT INTO domains (hostname, organization_id, created_at)
SELECT 'cms2.example.com', id, strftime('%s','now')
FROM organizations WHERE key = 'cms';

-- and to retire one:
DELETE FROM domains WHERE hostname = 'cms2.example.com';
```

### What to know

- **Restarts apply.** The host list loads once at boot, after the
  seed — like every other startup decision. SQL edits need a restart
  to take effect; the panel UI for domains (a later phase) will make
  that a form away.
- **The F14 static-root rule extends to the data plane.** A CMS host
  borrows its shared stylesheets (`/assets/*`) from the static root,
  so CMS domains require `static.enabled` — `validate_cms` enforces
  it for the `[cms] hosts` list at load time, and the boot refuses to
  start when the *table* maps CMS hosts with static serving off.
- **`cms_origin` stays configuration-derived for now** (`cms.site_url`
  > the first `[cms] hosts` entry > empty). Operators running purely
  data-driven vhosts should set `cms.site_url` — that is the public
  truth the feeds already trust — until the per-organization phases
  give domains a canonical-order concept.
- **Other organizations' domains are data, not tenants yet.** A row
  pointing at a third organization survives every boot, but nothing
  serves it: F16's dispatch is still binary (the CMS tree or the main
  tree). The per-organization phases build trees from the document
  roots, which is when `organizations.document_root` (kept in sync
  with the configuration since F15) becomes the serving truth.
- **One hostname, one organization** — `hostname` is `UNIQUE`, and
  listing a hostname in `[cms] hosts` reclaims its row for the CMS
  organization. Use clean lowercase names (no scheme, port or path);
  the loader normalizes case and trailing dots anyway.

## Per-organization serving (F17)

F16 made the host names data; F17 makes the trees data too. The
boot loads the `domains` table joined to the `organizations` rows
and serves each mapped Host name from **its organization's
`document_root`** — the dispatch generalizes from F14's two-way
split to one tree per organization:

- the `cms` organization keeps the visitor surface: public pages,
  `views/` auto-routing, the panel, the media library, search,
  feeds, the auth forms and the shared stylesheets;
- the `main` organization — and every unmapped host, unknown or
  missing included, fail-safe — keeps the static site plus the
  operator machinery (`/api`, `/health`, `/metrics`, the proxy);
- **any other organization** gets a self-contained static site from
  its own `document_root`: the file tree, the directory indexes,
  the on-the-fly `.jhs` rendering (its own `index.jhs`, its own
  `*.jhs` files), the standard JSON 404s — and nothing else.

The dispatcher and the templates middleware classify every request
with the same pure function against the same boot-time table, so
the two layers keep agreeing by construction — the F14 invariant,
generalized from two classes to one per organization.

### A third tenant, in SQL

The phase's promise: a real third tenant without touching
`wallermax.toml`:

```sql
-- 1. the organization, with its content root (relative paths
--    resolve against the working directory, like [static] root_dir):
INSERT INTO organizations (key, name, document_root, created_at)
VALUES ('acme', 'Acme site', 'sites/acme', strftime('%s','now'));

-- 2. its host names (restart; the bindings load at boot):
INSERT INTO domains (hostname, organization_id, created_at)
SELECT 'acme.example.com', id, strftime('%s','now')
FROM organizations WHERE key = 'acme';
```

`GET /` on `acme.example.com` answers `sites/acme/index.html` (the
`[static] index_file` — a server-wide convention; the row carries
only the root), `sites/acme/docs/` follows the same
`index.jhs`-then-`index.html` chain as the main host, and
`sites/acme/anything.jhs` renders on the fly. To retire the tenant,
delete its rows and restart.

### The provenance rule, one level up

F16 gave the `domains` rows a `source` column; F17 applies the same
rule at the organization level, without a schema change:

- **`main` and `cms` are the seeder's organizations.** The startup
  seed keeps their `document_root` in step with `[static] root_dir`
  and `[templates] views_dir` — config edits keep working exactly
  as they always did, and the serving path (which now reads the
  rows) changes nothing for existing setups.
- **Every other organization is data.** Created by hand (or, later,
  by the panel), its `document_root` is its truth; the seeders
  never read or touch it.

`organizations.document_root` is therefore the serving truth:
static roots and views directories alike resolve through it on
every database boot.

### What to know

- **Tenant trees are self-contained.** No `/api`, no `/health`, no
  `/metrics`, no panel, and no borrowed `/assets/*` from the main
  root — a tenant's host serves exactly its own files (the way F14
  kept the two names' surfaces disjoint). A tenant that wants the
  wallermax look copies the stylesheets in; one that wants styled
  HTML error pages puts its own `assets/error.css` beside its
  content (the shared error page links `/assets/error.css`, and on
  a tenant host that URL is the tenant's own).
- **Tenant trees do not need `[static] enabled`.** That switch
  governs the main organization's static surface; the rows are
  data, and tying them to the switch would re-couple tenants to
  `wallermax.toml`. (CMS domains still require it — F14's borrowed
  stylesheets rule, unchanged.)
- **A missing tenant root is a warning, not a boot failure.** The
  organization's host names answer 404s until the directory
  appears; no restart needed once it does, because serving hits
  the filesystem per request.
- **`cms_origin` derives from the domains now** (`cms.site_url` >
  the first mapped CMS host — the table's insertion order >
  empty), closing F16's documented gap: data-driven vhosts no
  longer need `cms.site_url` to publish sane cross-host links. The
  first row you insert is the canonical name.
- **Several host names may serve one organization** — they share
  its tree. One hostname still maps to exactly one organization
  (`UNIQUE`), and listing a hostname in `[cms] hosts` reclaims its
  row for the CMS organization.
- **Restarts apply.** The bindings and the document roots load once
  at boot, after the seeders — SQL edits need a restart, exactly
  like F16's host names.

## Configuration

Two keys from F7, two from F9, two from F10, one from F11 (all
optional) — and **F8 adds zero keys**: the body mode is per-page
state in the database, not server configuration, so the
`wallermax.toml` boundary stayed untouched through it.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `cms.sitemap` | bool | `true` | Serve `GET /sitemap.xml` with the published pages. |
| `cms.site_url` | string | unset | Absolute origin for the sitemap's `<loc>` URLs (and the feeds' links); without it the request's `Host` header is used with `http://`. |
| `cms.media_dir` | path | `"media"` | Directory the media library stores uploads in (created at startup; its own directory — never `public/`). |
| `cms.media_max_bytes` | bytes | `524288` | Cap on one uploaded file; the upload is streamed and refused past the cap. |
| `cms.index_page_size` | int | `10` | Rows per page of the public listings (`/p` and `/buscar`); validated 1–100 (F10). |
| `cms.feed` | bool | `true` | Serve `GET /feed.xml` and `GET /atom.xml` with the published pages (F10). |
| `cms.max_revisions` | int | `25` | Snapshots kept per page (F11): every save appends one and prunes the oldest beyond the cap, in the same write. `0` = unlimited; validated 0–1000. |

Environment overrides: `WALLERMAX_CMS__SITEMAP`,
`WALLERMAX_CMS__SITE_URL`, `WALLERMAX_CMS__MEDIA_DIR`,
`WALLERMAX_CMS__MEDIA_MAX_BYTES`, `WALLERMAX_CMS__INDEX_PAGE_SIZE`,
`WALLERMAX_CMS__FEED`, `WALLERMAX_CMS__MAX_REVISIONS`. The keys are
validated while the CMS is off too — a typo'd `site_url`, an
out-of-range `media_max_bytes` (1 KiB .. 64 MiB), an out-of-range
`index_page_size` (1–100) or an out-of-range `max_revisions`
(0–1000) is a startup error regardless of the switch.

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
- **Search input never becomes query syntax** (F10): the visitor's
  words are quoted into inert FTS5 phrases before `MATCH`, so
  operators, column filters and embedded quotes act as literals —
  there is no `MATCH` injection surface at all.
- **Snippets are escaped by construction** (F10): highlighting is a
  list of text segments printed through `<?= ?>` with `<mark>`
  wrapped by the template — never a pre-escaped HTML string, never
  `raw()`, so page bodies cannot ride into the browser as markup.
- **Feeds are built like the sitemap** (F10): every value crosses
  `xml_escape`, only published rows are ever listed, and the origin
  is the validated `site_url` or the Host header — no
  request-controlled path or query reaches the XML.
- **Revisions are inert by construction** (F11): a snapshot's body is
  only ever rendered as escaped source — the detail view prints it
  through `<?= ?>`, never `raw()`, never the engine. A revision
  becomes live content again only by being restored onto the page,
  which feeds it back through the normal, validated, previewable
  editor pipeline — the trust domain of page bodies is unchanged.
- **The restore re-validates today's tree** (F11): the slug must be
  free, the snapshot's parent must exist and not create a cycle — a
  stale snapshot cannot steal a URL or wedge the hierarchy, and a
  failed restore appends nothing.
- **Scheduling adds no moving part** (F11): visibility is computed at
  read time inside the very SQL the public routes already ran; there
  is no task to kill, no race with the writer, and the public queries
  gained no new parameters. The column is normalized on save, so a
  spent schedule cannot silently resurrect a page.

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
- **F10 — findability** (done): search over pages (SQLite FTS5, the
  visitor's words defanged into inert phrases), paginated listings
  (`/p`, search results, the media grid) and RSS/Atom feeds — this
  document.
- **F11 — history** (done): page revisions with restore, scheduled
  publishing — this document.
- **F12 — the admin panel redesign** (done): the app shell (sidebar,
  topbar, cards, tables, pills), `admin.css` with light/dark tokens,
  the no-JS theme toggle, the enriched dashboard, and the panel's
  English voice — this document.
- **F13 — English everywhere + the shared error page** (done):
  content-negotiated HTML error pages for browsers (the JSON envelope
  stays for API clients), the self-healing 429, the anglicized public
  site, feeds, login/register/profile flows and flash codes, and the
  English public routes (`/search`, `/register`, `/profile`,
  `/contact`) with permanent redirects from the Spanish spellings —
  this document.
- **F14 — virtual hosts** (done): `cms.hosts` splits the server into
  a main host (the static site plus the API machinery) and a CMS
  host (public pages, panel, media, feeds, search, the auth forms
  and the shared assets) by the `Host` header — one IP, one port —
  with the follow-up homepage chain: `index.jhs` renders before the
  static index, `/` and every directory alike, and the `cms_origin`
  global giving templates the cross-host link origin — this
  document.
- **F15 — organizations and memberships** (done): the multi-tenant
  authorization model — `organizations` (seeded from the
  configuration, one row per tenant) and `memberships` (user x
  organization x role, the enforcement the CMS guards read instead
  of the global role), with the zero-behavior-change mirror that
  keeps memberships in step with the platform roles until the
  data-driven phases let them diverge on purpose — this document.
- **F16 — the domains table** (done): Host names as data — the
  virtual-host split reads the `domains` table (seeded from
  `[cms] hosts`, which keeps working exactly as in F14), hand-made
  rows are data that survive every boot, and the F14 static-root
  rule extends to the data plane — this document.
- **F17 — per-organization serving** (done):
  `organizations.document_root` becomes the serving truth — static
  roots and views directories from data, one tree per mapped
  organization (a third tenant is two INSERTs and a restart), and
  `cms_origin` derived from the domains — this document.
- **F18 — the tenant pages in the panel** (next): the management
  surface for the model F15–F17 built — organizations, domains and
  memberships as forms instead of SQL (the first sanctioned
  divergence of the membership mirror, and the end of
  "restarting to move a host").

Each phase ships as one patch with tests and this document updated;
nothing lands half-featured.
