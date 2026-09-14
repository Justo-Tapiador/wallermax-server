-- Organizations and memberships (F15): the multi-tenant
-- authorization model.
--
-- One server can host several sites; F14 split them by Host header
-- (the static tree on the main host, the CMS on its own names). F15
-- gives those surfaces an identity in the database, so access can be
-- authorized per organization instead of trusting the one global role
-- column:
--
--   organizations  one row per tenant. `key` is the stable identifier
--                  the code addresses organizations by ('main' for the
--                  static site's tenant, 'cms' for the CMS tenant) --
--                  names and document roots are display/config data
--                  the startup seed keeps in sync with wallermax.toml.
--   memberships    user x organization x role. THE enforcement point:
--                  the CMS guards (CmsEditor / CmsAdmin) read this
--                  table, not users.role.
--
-- Roles reuse the users.role vocabulary ('admin', 'editor'); a
-- membership row with any other value is rejected by the CHECK below
-- -- 'user' is the absence of a membership, not a membership rank.
--
-- F15 transitional invariant (see README-CMS.md): the CMS
-- organization's memberships MIRROR the platform role -- every boot
-- and every role write upserts/removes rows so an upgrade changes
-- nothing. Divergence tools (per-organization roles in the panel)
-- arrive with the later phases; the table is already general.
--
-- No foreign keys on purpose, matching the existing schema style
-- (users, refresh_tokens, pages): the repository deletes a user's
-- memberships explicitly, exactly like it deletes their refresh
-- tokens.

CREATE TABLE IF NOT EXISTS organizations (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Stable code identifier: 'main' or 'cms' (F15).
    key           TEXT    NOT NULL UNIQUE,
    -- Human-readable name (display data for the later panel pages).
    name          TEXT    NOT NULL,
    -- The tenant's content root, seeded from the configuration
    -- ([static] root_dir for 'main', [templates] views_dir for 'cms').
    document_root TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS memberships (
    user_id         INTEGER NOT NULL,
    organization_id INTEGER NOT NULL,
    role            TEXT    NOT NULL CHECK (role IN ('admin', 'editor')),
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (user_id, organization_id)
);

-- The guards look memberships up by user; the (future) member lists
-- look them up by organization.
CREATE INDEX IF NOT EXISTS idx_memberships_organization
    ON memberships(organization_id);
