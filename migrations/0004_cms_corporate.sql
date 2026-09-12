-- Corporate content model (F7, on the v0.12.0 line).
--
-- Three additions to the v0.8.0 pages, all server-rendered and
-- JavaScript-free like the rest of the panel:
--
-- 1. Hierarchy: `parent_id` places a page under another page (menus,
--    breadcrumbs and the admin tree render from it). Deleting a parent
--    reparents its children to the top level (ON DELETE SET NULL) --
--    content is never lost with a branch. Cycles are prevented at the
--    handler layer (the ancestors chain is checked before every parent
--    change); SQLite's single writer makes that check-then-write safe
--    in practice. `position` orders siblings.
-- 2. SEO: optional `meta_title`, `meta_description` and `og_image` per
--    page, rendered into the <head> by views/cms_page.jhs.
-- 3. Menus: named collections of items pointing at a page or a custom
--    URL, exposed to every template as the `menus` global. Deleting a
--    menu removes its items; deleting a page removes its menu items
--    (the navigation never keeps dead links by construction).

ALTER TABLE pages ADD COLUMN parent_id INTEGER REFERENCES pages(id) ON DELETE SET NULL;
ALTER TABLE pages ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pages ADD COLUMN meta_title TEXT;
ALTER TABLE pages ADD COLUMN meta_description TEXT;
ALTER TABLE pages ADD COLUMN og_image TEXT;

CREATE INDEX IF NOT EXISTS idx_pages_parent ON pages(parent_id);

CREATE TABLE IF NOT EXISTS menus (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Machine name (slug-shaped, immutable): the `menus` template
    -- global is keyed by it -- menus.main, menus.footer...
    name       TEXT    NOT NULL UNIQUE,
    title      TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS menu_items (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    menu_id  INTEGER NOT NULL REFERENCES menus(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    -- Label override; NULL falls back to the linked page's title.
    label    TEXT,
    -- Exactly one of page_id / url is set (checked by the forms and
    -- by the CHECK below as defense in depth).
    page_id  INTEGER REFERENCES pages(id) ON DELETE CASCADE,
    url      TEXT,
    CHECK ((page_id IS NULL) <> (url IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_menu_items_menu ON menu_items(menu_id);
