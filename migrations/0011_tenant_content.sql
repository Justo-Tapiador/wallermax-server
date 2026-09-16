-- no-transaction
--
-- Per-tenant content (F20): the content plane learns its
-- organization. (The marker above is sqlx's: this script must run
-- OUTSIDE a transaction, and it is why — see the PRAGMA below.)
--
-- F15 gave the surfaces an identity (organizations); F16-F18 made the
-- serving and the management data-driven; F19 shared the session
-- across the host names. None of that touched the content itself:
-- pages, menus and media lived in one global plane, served only on
-- the CMS organization's hosts. F20 graduates every organization to
-- a full CMS tenant: content rows gain `organization_id`, every
-- existing row backfills to the CMS organization (the only tenant
-- with content before F20 — an upgrade changes nothing), and the two
-- natural keys stop being global:
--
--   pages:  UNIQUE(slug)  ->  UNIQUE(organization_id, slug)
--   menus:  UNIQUE(name)  ->  UNIQUE(organization_id, name)
--   media:  `stored_name` stays globally UNIQUE — the names are
--           server-generated (UUID v4 stems), so cross-organization
--           collisions cannot happen by construction, and the flat
--           on-disk layout (one shared media directory) is untouched.
--
-- `menu_items` needs no column: it reaches its organization through
-- its menu. `page_revisions` needs none either: its rows reach their
-- organization through their page.
--
-- SQLite cannot drop a column constraint, so `pages` and `menus` are
-- rebuilt (the documented SQLite procedure): create the new shape,
-- copy the rows preserving ids, drop the old table, rename. The
-- script is marked `-- no-transaction` ON PURPOSE: it must run
-- OUTSIDE a transaction so the two PRAGMA statements are real. With foreign
-- keys enforced, dropping the old `pages` would run the implicit
-- delete that `page_revisions`' ON DELETE CASCADE foreign key answers
-- — taking the whole history with it; with them off, the drop is
-- just a drop. Ids are preserved, so every child row keeps pointing
-- at its page and menu; the FTS5 triggers die with the old table and
-- are recreated verbatim; the index is rebuilt from the content
-- table in one statement.

PRAGMA foreign_keys = OFF;

-- ─── pages ───────────────────────────────────────────────────────────
-- The accumulated shape of 0003 + 0004 + 0005 + 0008, with
-- `organization_id` and the per-organization slug uniqueness.

CREATE TABLE IF NOT EXISTS pages_f20 (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    slug             TEXT    NOT NULL,
    title            TEXT    NOT NULL,
    content          TEXT    NOT NULL DEFAULT '',
    is_published     INTEGER NOT NULL DEFAULT 0,
    created_by       INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    parent_id        INTEGER REFERENCES pages(id) ON DELETE SET NULL,
    position         INTEGER NOT NULL DEFAULT 0,
    meta_title       TEXT,
    meta_description TEXT,
    og_image         TEXT,
    body_format      TEXT    NOT NULL DEFAULT 'jhs'
                             CHECK (body_format IN ('jhs', 'markdown')),
    publish_at       INTEGER,
    -- The owning organization, by the same no-foreign-key convention
    -- as the memberships: organizations are seed rows with no delete
    -- path, and the panel refuses to delete one that still owns
    -- content.
    organization_id  INTEGER NOT NULL,
    UNIQUE (organization_id, slug)
);

-- The backfill: every pre-F20 row belongs to the CMS organization.
-- On a fresh database both tables are empty, so the subselect never
-- even runs; on an existing one the organizations table has been
-- seeded since F15. INSERT OR IGNORE makes a crashed-then-rerun
-- migration resume instead of tripping over its own copied ids.
INSERT OR IGNORE INTO pages_f20 (
    id, slug, title, content, is_published, created_by, created_at,
    updated_at, parent_id, position, meta_title, meta_description,
    og_image, body_format, publish_at, organization_id
)
SELECT
    id, slug, title, content, is_published, created_by, created_at,
    updated_at, parent_id, position, meta_title, meta_description,
    og_image, body_format, publish_at,
    (SELECT id FROM organizations WHERE key = 'cms')
FROM pages;

DROP TABLE IF EXISTS pages;
ALTER TABLE pages_f20 RENAME TO pages;

CREATE INDEX IF NOT EXISTS idx_pages_published ON pages(is_published);
CREATE INDEX IF NOT EXISTS idx_pages_parent ON pages(parent_id);

-- The FTS5 maintenance triggers, verbatim from 0007 — the originals
-- were attached to the old table and died with it.
CREATE TRIGGER pages_fts_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, title, content)
    VALUES (new.id, new.title, new.content);
END;

CREATE TRIGGER pages_fts_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, content)
    VALUES ('delete', old.id, old.title, old.content);
END;

CREATE TRIGGER pages_fts_au AFTER UPDATE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, content)
    VALUES ('delete', old.id, old.title, old.content);
    INSERT INTO pages_fts(rowid, title, content)
    VALUES (new.id, new.title, new.content);
END;

-- One statement, the whole index, from the table of record. The
-- external-content shape of 0007 is what makes this possible.
INSERT INTO pages_fts(pages_fts) VALUES ('rebuild');

-- ─── menus ───────────────────────────────────────────────────────────
-- The 0004 shape with `organization_id` and the per-organization
-- name uniqueness. `menu_items` keeps pointing at `menus(id)` — the
-- rebuild preserves the ids, so nothing else moves.

CREATE TABLE IF NOT EXISTS menus_f20 (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Machine name (slug-shaped, immutable): the `menus` template
    -- global is keyed by it — unique within the organization now.
    name             TEXT    NOT NULL,
    title            TEXT    NOT NULL,
    organization_id  INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    UNIQUE (organization_id, name)
);

INSERT OR IGNORE INTO menus_f20 (
    id, name, title, organization_id, created_at, updated_at
)
SELECT
    id, name, title,
    (SELECT id FROM organizations WHERE key = 'cms'),
    created_at, updated_at
FROM menus;

DROP TABLE IF EXISTS menus;
ALTER TABLE menus_f20 RENAME TO menus;

CREATE INDEX IF NOT EXISTS idx_menus_organization ON menus(organization_id);

-- ─── media ────────────────────────────────────────────────────────────
-- The 0006 shape with `organization_id`. No foreign children, no
-- triggers — but the same rebuild, so the column lands NOT NULL the
-- clean way instead of a nullable ALTER.

CREATE TABLE IF NOT EXISTS media_f20 (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    -- <32-hex>.<ext>: flat, server-generated (a UUID v4 stem).
    stored_name      TEXT    NOT NULL UNIQUE,
    -- <32-hex>_t.png: the admin-listing thumbnail.
    thumb_name       TEXT    NOT NULL,
    -- The uploader's file name, display-only (the views escape it).
    original_name    TEXT    NOT NULL,
    -- The sniffed mime type, served verbatim by GET /media/{id}/{name}.
    mime_type        TEXT    NOT NULL,
    bytes            INTEGER NOT NULL,
    width            INTEGER NOT NULL,
    height           INTEGER NOT NULL,
    alt_text         TEXT    NOT NULL DEFAULT '',
    organization_id  INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    created_by       INTEGER,
    CHECK (stored_name NOT GLOB '*/*' AND stored_name NOT GLOB '*\*'),
    CHECK (thumb_name  NOT GLOB '*/*' AND thumb_name  NOT GLOB '*\*'),
    CHECK (mime_type IN ('image/png', 'image/jpeg', 'image/gif', 'image/webp'))
);

INSERT OR IGNORE INTO media_f20 (
    id, stored_name, thumb_name, original_name, mime_type, bytes,
    width, height, alt_text, organization_id, created_at, created_by
)
SELECT
    id, stored_name, thumb_name, original_name, mime_type, bytes,
    width, height, alt_text,
    (SELECT id FROM organizations WHERE key = 'cms'),
    created_at, created_by
FROM media;

DROP TABLE IF EXISTS media;
ALTER TABLE media_f20 RENAME TO media;

CREATE INDEX IF NOT EXISTS idx_media_organization ON media(organization_id);

-- Restore the enforcement the connection arrived with (the pool
-- configures `foreign_keys(true)`; this migration borrowed the
-- connection, so it puts it back).
PRAGMA foreign_keys = ON;
