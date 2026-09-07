-- CMS pages (v0.8.0).
--
-- Content managed through the admin panel and served dynamically at
-- `/p/<slug>`: the page body is `.jhs` template source rendered on the
-- fly with the standard globals (`user`, `path`, `query`, `pages`), so
-- CMS pages can personalise exactly like the views under `views/`.
--
-- `slug` is the public URL identifier and the uniqueness anchor; the
-- published flag gates public visibility (drafts are previewable by
-- editors/admins only). `created_by` is a soft audit link: deleting the
-- author keeps the page (ON DELETE SET NULL).

CREATE TABLE IF NOT EXISTS pages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    slug         TEXT    NOT NULL UNIQUE,
    title        TEXT    NOT NULL,
    -- `.jhs` template source; rendered by the sandboxed engine.
    content      TEXT    NOT NULL DEFAULT '',
    -- 1 = listed and served publicly, 0 = draft (editors only).
    is_published INTEGER NOT NULL DEFAULT 0,
    created_by   INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pages_published ON pages(is_published);
