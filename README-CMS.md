# The wallermax CMS — the corporate content model (F7)

This is the companion deep-dive for the CMS feature line: the main
[README.md](README.md) keeps the essentials and links here, so the
front door of the repository stays short while the content features
get the room they deserve.

**F7** (this document) adds four corporate-site capabilities on top of
the v0.8.0/v0.11.0 CMS, all of them **zero-JavaScript** — the CSP keeps
blocking scripts and every new screen works through plain HTML forms:

| Capability | One line |
|---|---|
| [Page hierarchy](#page-hierarchy) | Parents, sibling ordering, breadcrumbs and a cycle-proof move guard. |
| [Menus](#menus) | Named navigation menus rendered by any template through the `menus` global. |
| [SEO metadata](#seo-metadata) | Per-page `meta_title`, `meta_description` and `og_image` in the wrapper's `<head>`. |
| [Sitemap](#sitemap) | `GET /sitemap.xml` with every published page, automatically. |

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

## Configuration

Two new keys in `[cms]` (both optional; everything else about F7 needs
no configuration):

| Key | Type | Default | Purpose |
|---|---|---|---|
| `cms.sitemap` | bool | `true` | Serve `GET /sitemap.xml` with the published pages. |
| `cms.site_url` | string | unset | Absolute origin for the sitemap's `<loc>` URLs; without it the request's `Host` header is used with `http://`. |

Environment overrides: `WALLERMAX_CMS__SITEMAP`,
`WALLERMAX_CMS__SITE_URL`. The keys are validated while the CMS is off
too — a typo'd `site_url` is a startup error regardless of the switch.

## Security notes

- **No new client-side surface.** Zero JavaScript, zero CSP changes,
  zero new external requests. The four capabilities are server-rendered
  forms and templates like the rest of the panel.
- **Auto-escape still guards every print.** Menu labels, page titles
  and metadata reach HTML through `<?= ?>` (escaped); the resolved
  `menus` global is plain data for templates.
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

- **F7 — this document** (done): hierarchy, menus, SEO, sitemap.
- **F8 — the editor**: Markdown with **server-side preview** (the
  chosen philosophy: a textarea, a preview button, rendering on the
  server — no editor JavaScript, CSP untouched), stored alongside the
  `.jhs` body mode.
- **F9 — the media library**: uploads with magic-byte validation,
  size caps, thumbnails, alt text — served from a dedicated directory,
  never writing into `public/`.
- **F10 — findability**: search over pages (SQLite FTS5), paginated
  listings, RSS/Atom for dated content.
- **F11 — history**: page revisions with restore, scheduled
  publishing.

Each phase ships as one patch with tests and this document updated;
nothing lands half-featured.
