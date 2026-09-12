-- F11 — history: page revisions with restore, and scheduled
-- publishing.
--
-- Two additions to the F10 pages, both server-rendered and
-- JavaScript-free like the rest of the panel:
--
-- 1. `page_revisions`: a snapshot of every editable field, taken on
--    every save (create, edit, restore). Each row is the state *as
--    saved* — the revision list is the page's own history, and
--    restoring copies an old snapshot onto the page as a NEW revision
--    (history is append-only: a restore never destroys anything).
--    Deleting a page removes its revisions (`ON DELETE CASCADE`);
--    `parent_id` in a snapshot carries no foreign key on purpose —
--    it records where the page *hung at the time*, and the parent it
--    names may legitimately be gone years later. The restore route
--    re-validates existence and cycles before applying it.
-- 2. `pages.publish_at`: scheduled publishing. A draft with a future
--    `publish_at` becomes publicly visible the moment the clock
--    passes it — no background task, no cron: every public read
--    (listing, page, search, feeds, sitemap, menus) evaluates
--    "published = the flag is set OR the schedule has elapsed" at
--    query time, so the page goes live exactly on time even after a
--    restart, and stays a draft for anonymous visitors until then.
--    The forms normalise the column on every save: `is_published = 1`
--    clears it (a live page has no schedule), and a schedule already
--    in the past is dropped (it is spent — unchecking «Publicada»
--    unpublishes for real).

ALTER TABLE pages ADD COLUMN publish_at INTEGER;

CREATE TABLE IF NOT EXISTS page_revisions (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    page_id          INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    -- Per-page sequence, 1..N in save order.
    revision         INTEGER NOT NULL,
    -- The editable-field snapshot, as saved.
    slug             TEXT    NOT NULL,
    title            TEXT    NOT NULL,
    content          TEXT    NOT NULL DEFAULT '',
    body_format      TEXT    NOT NULL DEFAULT 'jhs'
                             CHECK (body_format IN ('jhs', 'markdown')),
    -- Historical parent (see above: deliberately no REFERENCES).
    parent_id        INTEGER,
    position         INTEGER NOT NULL DEFAULT 0,
    meta_title       TEXT,
    meta_description TEXT,
    og_image         TEXT,
    -- State snapshot at save time: informational (the history list
    -- shows "guardada como borrador programada"); a restore applies
    -- content only, never the state.
    is_published     INTEGER NOT NULL DEFAULT 0,
    publish_at       INTEGER,
    -- Soft audit link, like `pages.created_by`: deleting the editor
    -- keeps the revision.
    edited_by        INTEGER REFERENCES users(id) ON DELETE SET NULL,
    -- Optional editor note ("qué cambió"), capped by the forms.
    note             TEXT,
    created_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_page_revisions_page
    ON page_revisions(page_id, revision);
