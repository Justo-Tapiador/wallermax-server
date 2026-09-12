-- The media library (F9, on the v0.13.0 line).
--
-- One row per upload: the server-generated on-disk names (flat,
-- unique, never user-influenced), the provenance the panel displays
-- (original file name, size, dimensions, uploader) and the alt text
-- the detail page edits. The mime type is the SNIFFED one (see
-- src/media.rs) -- the client's Content-Type is never trusted, so a
-- served file can never lie about what it is.
--
-- The CHECK backstops keep a bug from ever storing a path separator
-- (the serving route joins these names onto the media directory) or
-- a format outside the accepted four.

CREATE TABLE IF NOT EXISTS media (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    -- <32-hex>.<ext>: flat, server-generated (a UUID v4 stem).
    stored_name   TEXT    NOT NULL UNIQUE,
    -- <32-hex>_t.png: the admin-listing thumbnail.
    thumb_name    TEXT    NOT NULL,
    -- The uploader's file name, display-only (the views escape it).
    original_name TEXT    NOT NULL,
    -- The sniffed mime type, served verbatim by GET /media/{id}/{name}.
    mime_type     TEXT    NOT NULL,
    bytes         INTEGER NOT NULL,
    width         INTEGER NOT NULL,
    height        INTEGER NOT NULL,
    alt_text      TEXT    NOT NULL DEFAULT '',
    created_at    INTEGER NOT NULL,
    created_by    INTEGER,
    CHECK (stored_name NOT GLOB '*/*' AND stored_name NOT GLOB '*\*'),
    CHECK (thumb_name  NOT GLOB '*/*' AND thumb_name  NOT GLOB '*\*'),
    CHECK (mime_type IN ('image/png', 'image/jpeg', 'image/gif', 'image/webp'))
);

CREATE INDEX IF NOT EXISTS idx_media_id ON media(id DESC);
