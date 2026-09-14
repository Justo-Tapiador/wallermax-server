-- Domains (F16): which Host name serves which organization.
--
-- F14 split the server by Host header, but the split's truth lived in
-- wallermax.toml: the [cms] hosts list, read at load time, and nothing
-- else. F16 moves that truth into the database, one row per hostname:
--
--   domains        hostname -> organization. The boot reads this
--                  table (joined to organizations) to decide which
--                  Host names get the CMS tree; every other host --
--                  unknown, missing, or mapped to a different
--                  organization -- gets the main tree, exactly as
--                  before.
--
-- The [cms] hosts list becomes the table's bootstrap: at every boot
-- the seeder upserts each configured hostname as a row of the CMS
-- organization and prunes the rows it itself created that are no
-- longer configured -- config edits keep working exactly as they did
-- in F14. Rows the seeder did NOT create are data: they survive every
-- boot, whatever the configuration says. The `source` column is that
-- provenance:
--
--   'config'  the seeder's own row. Follows the [cms] hosts list
--             one-for-one: added when the hostname is configured,
--             reclaimed when a hand-made row's hostname is configured,
--             removed when the hostname stops being configured.
--   'manual'  a row created by hand (or, later, by the panel). The
--             default: an INSERT that does not name `source` is
--             data, and data is never pruned.
--
-- A hostname is UNIQUE: one name, one organization (the seeder
-- reclaims a hostname by re-pointing its row at the CMS
-- organization). No foreign key on organization_id, matching the
-- existing schema style: organizations are seed rows with no delete
-- path, and deletes elsewhere are explicit.
--
-- The document roots stay configuration-driven in F16 (the F15 seed
-- keeps organizations.document_root in sync with wallermax.toml);
-- the later data-driven serving phases promote them.

CREATE TABLE IF NOT EXISTS domains (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    hostname TEXT NOT NULL UNIQUE,
    organization_id INTEGER NOT NULL,
    source TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('config', 'manual')),
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_domains_organization ON domains(organization_id);
