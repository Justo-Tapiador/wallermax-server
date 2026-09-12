-- CMS page body format (F8): the editor ships with two body modes.
--
-- `jhs` (the default, and the only mode before F8): `content` is
-- `.jhs` template source rendered on the fly with the standard
-- globals (`user`, `path`, `query`, `pages`) — the full-featured mode
-- an editor or admin already trusts.
--
-- `markdown`: `content` is Markdown rendered by the safe in-process
-- renderer (`src/markdown.rs`): no raw HTML survives, link schemes
-- are filtered. The mode for plain writing — prose, headings, lists,
-- tables — without template power.
--
-- The CHECK constraint keeps the column honest; existing rows inherit
-- `jhs` so every pre-F8 page keeps rendering exactly as before.

ALTER TABLE pages
    ADD COLUMN body_format TEXT NOT NULL DEFAULT 'jhs'
        CHECK (body_format IN ('jhs', 'markdown'));
