-- F10 — findability: the FTS5 index over the pages table.
--
-- External-content table: `pages` stays the single source of truth
-- (title + content columns read by name), and `pages_fts` carries only
-- the inverted index, kept in lockstep by the three triggers below —
-- the classic FTS5 pattern. The `rebuild` command at the end backfills
-- every pre-F10 database in one statement, so existing content becomes
-- searchable the moment the migration applies (fresh databases rebuild
-- an empty table, a no-op).
--
-- What lands in the index is the raw source: the title plus the body
-- exactly as stored (`.jhs` or Markdown). That is deliberate — the
-- admin filter's job includes finding *references* (a media URL inside
-- a body, a partial include), which a rendered-text index would hide.
--
-- User input never reaches MATCH unsanitized: the repository quotes
-- every whitespace token into a phrase (src/db.rs, `fts_match_query`),
-- so FTS5 operators typed by a visitor become inert literals.

CREATE VIRTUAL TABLE pages_fts USING fts5(
    title,
    content,
    content='pages',
    content_rowid='id'
);

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

INSERT INTO pages_fts(pages_fts) VALUES ('rebuild');
