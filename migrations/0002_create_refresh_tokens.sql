-- Refresh tokens backing the `/api/auth/refresh` endpoint.
--
-- Only the SHA-256 hash of each opaque token is stored, so a database
-- leak does not expose usable credentials. Tokens are grouped into
-- *families*: one family per login session. Refreshing rotates the
-- token (marking it `rotated_at`) and issues a successor inside the
-- same family; presenting an already-rotated or revoked token is
-- treated as theft and revokes the whole family.

CREATE TABLE IF NOT EXISTS refresh_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Root token id of the login session chain (equals `id` for the
    -- first token of a family).
    family_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Hex-encoded SHA-256 of the 32-byte random token given to the
    -- client. UNIQUE: lookup key on refresh.
    token_hash TEXT NOT NULL UNIQUE,
    -- Expiry, unix seconds (set from `[auth] refresh_token_ttl_secs`).
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    -- Best-effort audit fields captured at issue time.
    created_ip TEXT,
    user_agent TEXT,
    -- Set when the token was successfully exchanged for a successor.
    rotated_at INTEGER,
    -- Set on logout, family revocation or reuse detection.
    revoked_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_refresh_tokens_family ON refresh_tokens(family_id);
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_user ON refresh_tokens(user_id);
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_expires ON refresh_tokens(expires_at);
