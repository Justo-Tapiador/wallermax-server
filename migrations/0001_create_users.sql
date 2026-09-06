-- Phase 3: user accounts backing JWT authentication.
CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT    NOT NULL,
    role          TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    last_login_at INTEGER
);
