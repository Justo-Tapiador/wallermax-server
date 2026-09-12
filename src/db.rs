//! SQLite persistence behind the `UserRepository` abstraction.
//!
//! The HTTP layer depends only on the [`UserRepository`] trait, never on
//! SQLite directly, so the storage engine can be swapped — a different
//! database, a cloud service or a test double — without touching handlers
//! or middleware. [`SqliteUserRepository`] is the reference implementation,
//! backed by a shared [`SqlitePool`] and the embedded migrations under
//! `migrations/`.
//!
//! Pool tuning (applied by [`connect`]):
//!
//! - **WAL journaling + `NORMAL` sync**: the recommended SQLite
//!   combination for servers — readers never block the writer and the
//!   write-ahead log is checkpointed efficiently.
//! - **Foreign keys on**, matching what the schema declares.
//! - **5 second busy timeout**, so concurrent pool connections waiting on
//!   SQLite's single writer lock degrade gracefully instead of failing
//!   with `SQLITE_BUSY`.

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Serialize;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow,
    SqliteSynchronous,
};
use sqlx::{Error as SqlxError, FromRow, Row};

use crate::config::DatabaseConfig;

/// User roles used by the authorization layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    /// Full administrative access (first registered account): CMS pages,
    /// CMS users, own password.
    Admin,
    /// CMS content access: create, edit, delete and publish pages, but
    /// no user administration (v0.8.0).
    Editor,
    /// Standard authenticated access.
    User,
}

impl UserRole {
    /// Stable string representation persisted in the database and inside
    /// JWT claims.
    pub fn as_str(self) -> &'static str {
        match self {
            UserRole::Admin => "admin",
            UserRole::Editor => "editor",
            UserRole::User => "user",
        }
    }

    /// Parses a persisted role value; unknown values yield `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "admin" => Some(UserRole::Admin),
            "editor" => Some(UserRole::Editor),
            "user" => Some(UserRole::User),
            _ => None,
        }
    }

    /// Whether this role may manage CMS pages (create, edit, delete,
    /// publish) — the `admin` and `editor` roles.
    pub fn is_editor(self) -> bool {
        matches!(self, UserRole::Admin | UserRole::Editor)
    }
}

/// A registered user account.
#[derive(Debug, Clone)]
pub struct User {
    /// Database id (also used as the JWT `sub` claim).
    pub id: i64,
    /// Unique, case-insensitively unique username.
    pub username: String,
    /// Argon2id PHC string. Never leaves this module through the API.
    pub password_hash: String,
    /// Authorization role.
    pub role: UserRole,
    /// Registration time, unix seconds.
    pub created_at: i64,
    /// Last successful login, unix seconds (`None` until first login).
    pub last_login_at: Option<i64>,
}

impl User {
    /// Projects the user onto its public shape (no password hash).
    pub fn public(&self) -> PublicUser {
        PublicUser {
            id: self.id,
            username: self.username.clone(),
            role: self.role,
            created_at: self.created_at,
            last_login_at: self.last_login_at,
        }
    }
}

/// Public projection of a user, safe to serialize into API responses.
#[derive(Debug, Serialize)]
pub struct PublicUser {
    pub id: i64,
    pub username: String,
    pub role: UserRole,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

/// Decodes `SqliteRow`s into [`User`]s, rejecting unknown role values.
impl<'r> FromRow<'r, SqliteRow> for User {
    fn from_row(row: &'r SqliteRow) -> Result<Self, SqlxError> {
        let role: String = row.try_get("role")?;
        let role = UserRole::parse(&role).ok_or_else(|| SqlxError::ColumnDecode {
            index: "role".to_owned(),
            source: format!("unknown role value `{role}`").into(),
        })?;

        Ok(Self {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            password_hash: row.try_get("password_hash")?,
            role,
            created_at: row.try_get("created_at")?,
            last_login_at: row.try_get("last_login_at")?,
        })
    }
}

/// Values needed to insert a refresh token row. Times are unix seconds
/// supplied by the caller (handlers use the wall clock; tests inject
/// deterministic values).
#[derive(Debug, Clone)]
pub struct NewRefreshToken {
    /// Owning user.
    pub user_id: i64,
    /// Family of the token: `None` starts a new family (login), `Some`
    /// continues one (refresh rotation).
    pub family_id: Option<i64>,
    /// Hex-encoded SHA-256 of the opaque token handed to the client.
    pub token_hash: String,
    /// Expiry, unix seconds.
    pub expires_at: i64,
    /// Creation time, unix seconds.
    pub created_at: i64,
    /// Best-effort audit fields.
    pub created_ip: Option<String>,
    pub user_agent: Option<String>,
}

/// A persisted refresh token row (see `migrations/0002_*`).
#[derive(Debug, Clone)]
pub struct RefreshTokenRecord {
    pub id: i64,
    pub family_id: i64,
    pub user_id: i64,
    pub token_hash: String,
    pub expires_at: i64,
    pub created_at: i64,
    pub created_ip: Option<String>,
    pub user_agent: Option<String>,
    /// Set once the token was exchanged for a successor.
    pub rotated_at: Option<i64>,
    /// Set on logout, family revocation or reuse detection.
    pub revoked_at: Option<i64>,
}

impl<'r> FromRow<'r, SqliteRow> for RefreshTokenRecord {
    fn from_row(row: &'r SqliteRow) -> Result<Self, SqlxError> {
        Ok(Self {
            id: row.try_get("id")?,
            family_id: row.try_get("family_id")?,
            user_id: row.try_get("user_id")?,
            token_hash: row.try_get("token_hash")?,
            expires_at: row.try_get("expires_at")?,
            created_at: row.try_get("created_at")?,
            created_ip: row.try_get("created_ip")?,
            user_agent: row.try_get("user_agent")?,
            rotated_at: row.try_get("rotated_at")?,
            revoked_at: row.try_get("revoked_at")?,
        })
    }
}

/// Errors surfaced by [`UserRepository`] implementations.
#[derive(Debug)]
pub enum RepositoryError {
    /// A uniqueness constraint was violated (the username is taken).
    Duplicate,
    /// Any other storage failure; the message is logged server-side only.
    Internal(String),
}

impl std::fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepositoryError::Duplicate => {
                write!(formatter, "a uniqueness constraint was violated")
            }
            RepositoryError::Internal(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for RepositoryError {}

impl RepositoryError {
    /// Maps a raw sqlx error to a repository error, recognising
    /// unique-constraint violations.
    fn from_sqlx(error: SqlxError) -> Self {
        if error
            .as_database_error()
            .is_some_and(|database| database.is_unique_violation())
        {
            Self::Duplicate
        } else {
            Self::Internal(error.to_string())
        }
    }
}

impl From<RepositoryError> for crate::error::AppError {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::Duplicate => {
                crate::error::AppError::conflict("resource already exists")
            }
            RepositoryError::Internal(message) => {
                tracing::error!(%message, "user storage failure");
                crate::error::AppError::internal("storage failure")
            }
        }
    }
}

/// Storage abstraction for user accounts.
///
/// Handlers depend on this trait — not on SQLite — so the storage engine
/// can be replaced without touching HTTP code. The trait is
/// `Send + Sync + 'static` so implementations can be shared through
/// [`std::sync::Arc`] inside the application state.
#[async_trait]
pub trait UserRepository: Send + Sync + 'static {
    /// Inserts a new user. Fails with [`RepositoryError::Duplicate`] when
    /// the username is already taken (case-insensitively).
    async fn create(
        &self,
        username: &str,
        password_hash: &str,
        role: UserRole,
    ) -> Result<User, RepositoryError>;

    /// Looks up a user by id.
    async fn find_by_id(&self, id: i64) -> Result<Option<User>, RepositoryError>;

    /// Looks up a user by username (case-insensitive).
    async fn find_by_username(&self, username: &str) -> Result<Option<User>, RepositoryError>;

    /// Counts registered users.
    async fn count(&self) -> Result<i64, RepositoryError>;

    /// Counts users holding a specific role (used by the last-admin
    /// guard of the CMS user management).
    async fn count_with_role(&self, role: UserRole) -> Result<i64, RepositoryError>;

    /// Lists users, newest first, up to `limit` rows.
    async fn list(&self, limit: i64) -> Result<Vec<User>, RepositoryError>;

    /// Replaces the password hash of `id` (the CMS "reset password"
    /// and the self-service change on `/perfil`).
    async fn update_password(&self, id: i64, password_hash: &str) -> Result<(), RepositoryError>;

    /// Grants `role` to `id` (admin-only CMS action).
    async fn update_role(&self, id: i64, role: UserRole) -> Result<(), RepositoryError>;

    /// Deletes the account (refresh tokens cascade in SQL).
    async fn delete(&self, id: i64) -> Result<(), RepositoryError>;

    /// Stamps `last_login_at` for a successful login.
    async fn record_login(&self, id: i64) -> Result<(), RepositoryError>;

    /// Persists a refresh token; `family_id: None` starts a new family.
    async fn save_refresh_token(
        &self,
        token: &NewRefreshToken,
    ) -> Result<RefreshTokenRecord, RepositoryError>;

    /// Looks up a refresh token by the SHA-256 hash of its value.
    async fn find_refresh_token_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<RefreshTokenRecord>, RepositoryError>;

    /// Marks a token as rotated at `now`.
    ///
    /// Returns `false` when the row was already rotated or revoked —
    /// the caller treats that as reuse and revokes the family.
    async fn rotate_refresh_token(&self, id: i64, now: i64) -> Result<bool, RepositoryError>;

    /// Revokes every active token of `family_id` (logout, reuse
    /// detection, deleted account).
    async fn revoke_refresh_token_family(
        &self,
        family_id: i64,
        now: i64,
    ) -> Result<(), RepositoryError>;

    /// Revokes every active token owned by `user_id`
    /// (`POST /api/auth/logout_all`).
    async fn revoke_all_refresh_tokens(
        &self,
        user_id: i64,
        now: i64,
    ) -> Result<(), RepositoryError>;

    /// Deletes rows that are **both expired and revoked** — they can no
    /// longer be redeemed and no longer serve reuse detection, while
    /// rotated-but-unrevoked rows are kept so replays keep triggering
    /// family revocation. Returns the number of removed rows.
    async fn delete_expired_refresh_tokens(&self, now: i64) -> Result<u64, RepositoryError>;
}

/// SQLite-backed [`UserRepository`] over a shared pool.
#[derive(Clone)]
pub struct SqliteUserRepository {
    pool: SqlitePool,
}

impl SqliteUserRepository {
    /// Wraps an already-migrated pool into a repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Column list shared by every `SELECT` on the `users` table.
const USER_COLUMNS: &str = "id, username, password_hash, role, created_at, last_login_at";

/// Column list shared by every `SELECT` on the `refresh_tokens` table.
const REFRESH_TOKEN_COLUMNS: &str = "id, family_id, user_id, token_hash, expires_at, \
                                    created_at, created_ip, user_agent, rotated_at, \
                                    revoked_at";

#[async_trait]
impl UserRepository for SqliteUserRepository {
    async fn create(
        &self,
        username: &str,
        password_hash: &str,
        role: UserRole,
    ) -> Result<User, RepositoryError> {
        let created_at = unix_now();

        let result = sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(role.as_str())
        .bind(created_at)
        .execute(&self.pool)
        .await;

        match result {
            Ok(done) => Ok(User {
                id: done.last_insert_rowid(),
                username: username.to_owned(),
                password_hash: password_hash.to_owned(),
                role,
                created_at,
                last_login_at: None,
            }),
            Err(error) => Err(RepositoryError::from_sqlx(error)),
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<User>, RepositoryError> {
        sqlx::query_as(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<User>, RepositoryError> {
        sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users WHERE username = ?1"
        ))
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn count(&self) -> Result<i64, RepositoryError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn count_with_role(&self, role: UserRole) -> Result<i64, RepositoryError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = ?1")
            .bind(role.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn list(&self, limit: i64) -> Result<Vec<User>, RepositoryError> {
        sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users ORDER BY id DESC LIMIT ?1"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn update_password(&self, id: i64, password_hash: &str) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE users SET password_hash = ?2 WHERE id = ?1")
            .bind(id)
            .bind(password_hash)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(RepositoryError::from_sqlx)
    }

    async fn update_role(&self, id: i64, role: UserRole) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE users SET role = ?2 WHERE id = ?1")
            .bind(id)
            .bind(role.as_str())
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(RepositoryError::from_sqlx)
    }

    async fn delete(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM users WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(RepositoryError::from_sqlx)
    }

    async fn record_login(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE users SET last_login_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(unix_now())
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(RepositoryError::from_sqlx)
    }

    async fn save_refresh_token(
        &self,
        token: &NewRefreshToken,
    ) -> Result<RefreshTokenRecord, RepositoryError> {
        // `family_id` is NOT NULL, but a new family only knows its id
        // after the insert; 0 is the placeholder and is rewritten with
        // the row's own id right below.
        let placeholder_family = token.family_id.unwrap_or(0);

        let inserted = sqlx::query(
            "INSERT INTO refresh_tokens \
             (family_id, user_id, token_hash, expires_at, created_at, created_ip, \
              user_agent) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(placeholder_family)
        .bind(token.user_id)
        .bind(&token.token_hash)
        .bind(token.expires_at)
        .bind(token.created_at)
        .bind(&token.created_ip)
        .bind(&token.user_agent)
        .execute(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)?;

        let id = inserted.last_insert_rowid();
        let family_id = match token.family_id {
            Some(family_id) => family_id,
            None => {
                sqlx::query("UPDATE refresh_tokens SET family_id = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(id)
                    .execute(&self.pool)
                    .await
                    .map_err(RepositoryError::from_sqlx)?;
                id
            }
        };

        Ok(RefreshTokenRecord {
            id,
            family_id,
            user_id: token.user_id,
            token_hash: token.token_hash.clone(),
            expires_at: token.expires_at,
            created_at: token.created_at,
            created_ip: token.created_ip.clone(),
            user_agent: token.user_agent.clone(),
            rotated_at: None,
            revoked_at: None,
        })
    }

    async fn find_refresh_token_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<RefreshTokenRecord>, RepositoryError> {
        sqlx::query_as(&format!(
            "SELECT {REFRESH_TOKEN_COLUMNS} FROM refresh_tokens WHERE token_hash = ?1"
        ))
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn rotate_refresh_token(&self, id: i64, now: i64) -> Result<bool, RepositoryError> {
        // The WHERE clause doubles as the concurrency guard: if another
        // worker rotated (or revoked) the row first, rows_affected is 0
        // and the caller treats the token as reused.
        let result = sqlx::query(
            "UPDATE refresh_tokens SET rotated_at = ?2 \
             WHERE id = ?1 AND rotated_at IS NULL AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)?;

        Ok(result.rows_affected() == 1)
    }

    async fn revoke_refresh_token_family(
        &self,
        family_id: i64,
        now: i64,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = ?2 \
             WHERE family_id = ?1 AND revoked_at IS NULL",
        )
        .bind(family_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(RepositoryError::from_sqlx)
    }

    async fn revoke_all_refresh_tokens(
        &self,
        user_id: i64,
        now: i64,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = ?2 \
             WHERE user_id = ?1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(RepositoryError::from_sqlx)
    }

    async fn delete_expired_refresh_tokens(&self, now: i64) -> Result<u64, RepositoryError> {
        // Only expired AND revoked rows are safe to remove: rotated rows
        // must survive so replaying them still triggers family
        // revocation while a live successor exists.
        let result = sqlx::query(
            "DELETE FROM refresh_tokens WHERE expires_at < ?1 AND revoked_at IS NOT NULL",
        )
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)?;

        Ok(result.rows_affected())
    }
}

// ─── CMS pages (v0.8.0) ───────────────────────────────────────────────

/// How a page body is interpreted (F8, `pages.body_format`).
///
/// `Jhs` is the pre-F8 default: the body is template source rendered
/// by the engine with the standard globals. `Markdown` renders through
/// the safe subset in [`crate::markdown`] — no raw HTML, filtered URL
/// schemes — the plain-writing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFormat {
    Jhs,
    Markdown,
}

impl BodyFormat {
    /// The two values the admin form and the database agree on.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "jhs" => Some(Self::Jhs),
            "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    /// The stored spelling (also the `<option value>` in the form).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jhs => "jhs",
            Self::Markdown => "markdown",
        }
    }
}

impl std::fmt::Display for BodyFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Values needed to insert a CMS page.
#[derive(Debug, Clone)]
pub struct NewPage {
    /// Public URL identifier (`/p/<slug>`); uniqueness anchor.
    pub slug: String,
    /// Human title (rendered as the page heading and in listings).
    pub title: String,
    /// `.jhs` template source rendered on the fly.
    pub content: String,
    /// How `content` is interpreted (F8).
    pub body_format: BodyFormat,
    /// Whether the page is publicly visible (drafts are editors-only).
    pub is_published: bool,
    /// Author id (soft audit link; `NULL` keeps the page after deletion).
    pub created_by: Option<i64>,
    /// Parent page id; `None` places the page at the top level (F7).
    pub parent_id: Option<i64>,
    /// Sibling ordering inside the parent — menus, subpage listings and
    /// the admin tree, lower first (F7).
    pub position: i64,
    /// Optional `<title>` override for search engines (F7).
    pub meta_title: Option<String>,
    /// Optional `<meta name="description">` content (F7).
    pub meta_description: Option<String>,
    /// Optional social/`og:image` URL — a site path or an absolute URL
    /// (F7; the media library lands in a later phase).
    pub og_image: Option<String>,
}

/// Field updates for an existing CMS page, addressed by id. A `None`
/// slug keeps the current one.
#[derive(Debug, Clone)]
pub struct PageUpdate {
    /// New slug, or `None` to keep the current one.
    pub slug: Option<String>,
    pub title: String,
    pub content: String,
    /// The new body interpretation — the form always states it, like
    /// `title` (F8).
    pub body_format: BodyFormat,
    pub is_published: bool,
    /// The new parent. **Unlike `slug`, `None` means "top level"**, not
    /// "keep the current one": the admin form always carries the field
    /// (empty select = move to the top), so every update states the
    /// intended parent explicitly (F7).
    pub parent_id: Option<i64>,
    /// Sibling ordering inside the (new) parent.
    pub position: i64,
    /// New `<title>` override, or `None` to clear it (empty form field).
    pub meta_title: Option<String>,
    /// New `<meta name="description">`, or `None` to clear it.
    pub meta_description: Option<String>,
    /// New `og:image`, or `None` to clear it.
    pub og_image: Option<String>,
}

/// A persisted CMS page row (see `migrations/0003_*` and `0004_*`).
#[derive(Debug, Clone)]
pub struct PageRecord {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub content: String,
    /// How `content` is interpreted (F8).
    pub body_format: BodyFormat,
    pub is_published: bool,
    pub created_by: Option<i64>,
    /// Creation time, unix seconds.
    pub created_at: i64,
    /// Last edit time, unix seconds.
    pub updated_at: i64,
    /// Parent page id; `None` = top level (F7).
    pub parent_id: Option<i64>,
    /// Sibling ordering inside the parent (F7).
    pub position: i64,
    pub meta_title: Option<String>,
    pub meta_description: Option<String>,
    pub og_image: Option<String>,
}

/// Listing projection of a page (no `content`: listings stay small).
#[derive(Debug, Clone)]
pub struct PageSummary {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub is_published: bool,
    pub updated_at: i64,
    /// Parent page id; `None` = top level (F7).
    pub parent_id: Option<i64>,
    /// Sibling ordering inside the parent (F7).
    pub position: i64,
}

/// One full-text search result (F10): the page fields the results
/// pages need plus a body snippet, with the hits wrapped in the `⟦`/
/// `⟧` markers the handlers split into escaped `<mark>` segments —
/// the fragment is raw SQL output, never template-bound as-is.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SearchHit {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub is_published: bool,
    pub updated_at: i64,
    /// Body snippet around the hits, markers included.
    pub fragment: String,
}

/// One published page as a feed entry (F10): the newest-first
/// projection `/feed.xml` and `/atom.xml` render, with the SEO
/// description riders the `<description>`/`<summary>` elements use
/// when set.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedEntry {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub updated_at: i64,
    /// Per-page SEO description; feeds omit the element when unset.
    pub meta_description: Option<String>,
}

/// Sanitizes a visitor query into an FTS5 `MATCH` expression (F10):
/// every whitespace token becomes a quoted phrase, so FTS5's own
/// operators (`OR`, `NOT`, `*`, column filters…) typed by a visitor can
/// only act as inert literals, and embedded quotes are stripped
/// rather than escaped. Returns `None` when nothing quotable remains —
/// the caller skips the search instead of matching everything.
pub(crate) fn fts_match_query(terms: &str) -> Option<String> {
    let phrases: Vec<String> = terms
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "")))
        .filter(|phrase| phrase != "\"\"")
        .collect();
    (!phrases.is_empty()).then(|| phrases.join(" "))
}

impl PageRecord {
    /// The listing projection of the record.
    pub fn summary(&self) -> PageSummary {
        PageSummary {
            id: self.id,
            slug: self.slug.clone(),
            title: self.title.clone(),
            is_published: self.is_published,
            updated_at: self.updated_at,
            parent_id: self.parent_id,
            position: self.position,
        }
    }
}

impl<'r> FromRow<'r, SqliteRow> for PageRecord {
    fn from_row(row: &'r SqliteRow) -> Result<Self, SqlxError> {
        Ok(Self {
            id: row.try_get("id")?,
            slug: row.try_get("slug")?,
            title: row.try_get("title")?,
            content: row.try_get("content")?,
            // The CHECK constraint keeps the column honest; the Jhs
            // fallback covers a pre-F8 database mid-migration.
            body_format: row
                .try_get::<String, _>("body_format")
                .ok()
                .and_then(|raw| BodyFormat::parse(&raw))
                .unwrap_or(BodyFormat::Jhs),
            is_published: row.try_get::<i64, _>("is_published")? != 0,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            parent_id: row.try_get("parent_id")?,
            position: row.try_get("position")?,
            meta_title: row.try_get("meta_title")?,
            meta_description: row.try_get("meta_description")?,
            og_image: row.try_get("og_image")?,
        })
    }
}

impl<'r> FromRow<'r, SqliteRow> for PageSummary {
    fn from_row(row: &'r SqliteRow) -> Result<Self, SqlxError> {
        Ok(Self {
            id: row.try_get("id")?,
            slug: row.try_get("slug")?,
            title: row.try_get("title")?,
            is_published: row.try_get::<i64, _>("is_published")? != 0,
            updated_at: row.try_get("updated_at")?,
            parent_id: row.try_get("parent_id")?,
            position: row.try_get("position")?,
        })
    }
}

/// Storage abstraction for CMS pages.
///
/// Same shape as [`UserRepository`]: handlers depend on the trait, not
/// on SQLite, so the storage can be swapped in tests or future phases.
#[async_trait]
pub trait PageRepository: Send + Sync + 'static {
    /// Inserts a new page. Fails with [`RepositoryError::Duplicate`] when
    /// the slug is already taken.
    async fn create(&self, page: &NewPage) -> Result<PageRecord, RepositoryError>;

    /// Looks up a page by id.
    async fn find_by_id(&self, id: i64) -> Result<Option<PageRecord>, RepositoryError>;

    /// Looks up a page by slug.
    async fn find_by_slug(&self, slug: &str) -> Result<Option<PageRecord>, RepositoryError>;

    /// Lists pages, newest first, up to `limit` rows. Drafts are only
    /// included while `include_drafts` (the admin listing); the public
    /// listing and the `pages` template global see published pages only.
    async fn list(
        &self,
        include_drafts: bool,
        limit: i64,
    ) -> Result<Vec<PageSummary>, RepositoryError>;

    /// The same listing, one window of it (F10): `limit` rows from
    /// `offset`, newest first — the paginated `GET /p` index.
    async fn list_paged(
        &self,
        include_drafts: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PageSummary>, RepositoryError>;

    /// Full-text search over title + content (F10). `terms` is the raw
    /// visitor query — the implementation sanitizes it into
    /// `MATCH`-safe quoted phrases ([`fts_match_query`]) before it
    /// reaches FTS5, and results are ranked by `bm25` with a snippet
    /// fragment of the body (hits wrapped in `⟦ ⟧` markers). Drafts
    /// ride along only while `include_drafts` (the admin filter); the
    /// public search page always passes `false`.
    async fn search(
        &self,
        terms: &str,
        include_drafts: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<SearchHit>, RepositoryError>;

    /// How many rows [`PageRepository::search`] can return — the count
    /// the results pagination needs.
    async fn search_count(&self, terms: &str, include_drafts: bool)
        -> Result<i64, RepositoryError>;

    /// The published pages as feed entries (F10): newest first, up to
    /// `limit`, with the SEO description riders.
    async fn feed_entries(&self, limit: i64) -> Result<Vec<FeedEntry>, RepositoryError>;

    /// Applies `update` to the page with `id`. Returns `None` when the
    /// page does not exist, and fails with
    /// [`RepositoryError::Duplicate`] when the new slug is taken.
    async fn update(
        &self,
        id: i64,
        update: &PageUpdate,
    ) -> Result<Option<PageRecord>, RepositoryError>;

    /// Deletes the page with `id`. Returns whether a row was removed.
    ///
    /// Children survive: the schema reparents them to the top level
    /// (`ON DELETE SET NULL`), and menu items pointing at the page are
    /// removed (`ON DELETE CASCADE`) — the navigation never keeps dead
    /// links by construction (F7).
    async fn delete(&self, id: i64) -> Result<bool, RepositoryError>;

    /// Counts all pages.
    async fn count(&self) -> Result<i64, RepositoryError>;

    /// Counts published pages.
    async fn count_published(&self) -> Result<i64, RepositoryError>;

    /// The ancestor chain of the page with `id`: root first, immediate
    /// parent last, the page itself excluded. The walk is bounded so a
    /// corrupted cycle can never hang it (F7).
    async fn ancestors(&self, id: i64) -> Result<Vec<PageSummary>, RepositoryError>;

    /// The direct children of a page (`None` = top level), ordered by
    /// `position` then id. Drafts are only included while
    /// `include_drafts` (the admin tree); the public subpage listings
    /// see published children only (F7).
    async fn children(
        &self,
        parent_id: Option<i64>,
        include_drafts: bool,
    ) -> Result<Vec<PageSummary>, RepositoryError>;
}

/// Column list shared by every `SELECT` on the `pages` table.
const PAGE_COLUMNS: &str = "id, slug, title, content, is_published, created_by, \
                           created_at, updated_at, parent_id, position, meta_title, \
                           meta_description, og_image, body_format";

/// Column list of the listing projection (no `content`).
const PAGE_SUMMARY_COLUMNS: &str = "id, slug, title, is_published, updated_at, \
                                   parent_id, position";

/// How many ancestor hops [`PageRepository::ancestors`] walks before
/// giving up: deeper than any sane site tree, but a corrupted cycle
/// answers an error instead of hanging the walk.
const MAX_ANCESTOR_HOPS: usize = 128;

/// SQLite-backed [`PageRepository`] over a shared pool.
#[derive(Clone)]
pub struct SqlitePageRepository {
    pool: SqlitePool,
}

impl SqlitePageRepository {
    /// Wraps an already-migrated pool into a repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PageRepository for SqlitePageRepository {
    async fn create(&self, page: &NewPage) -> Result<PageRecord, RepositoryError> {
        let created_at = unix_now();
        let updated_at = created_at;

        let result = sqlx::query(
            "INSERT INTO pages (slug, title, content, is_published, created_by, \
             created_at, updated_at, parent_id, position, meta_title, meta_description, \
             og_image, body_format) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
             ?11, ?12, ?13)",
        )
        .bind(&page.slug)
        .bind(&page.title)
        .bind(&page.content)
        .bind(page.is_published)
        .bind(page.created_by)
        .bind(created_at)
        .bind(updated_at)
        .bind(page.parent_id)
        .bind(page.position)
        .bind(&page.meta_title)
        .bind(&page.meta_description)
        .bind(&page.og_image)
        .bind(page.body_format.as_str())
        .execute(&self.pool)
        .await;

        match result {
            Ok(done) => Ok(PageRecord {
                id: done.last_insert_rowid(),
                slug: page.slug.clone(),
                title: page.title.clone(),
                content: page.content.clone(),
                body_format: page.body_format,
                is_published: page.is_published,
                created_by: page.created_by,
                created_at,
                updated_at,
                parent_id: page.parent_id,
                position: page.position,
                meta_title: page.meta_title.clone(),
                meta_description: page.meta_description.clone(),
                og_image: page.og_image.clone(),
            }),
            Err(error) => Err(RepositoryError::from_sqlx(error)),
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<PageRecord>, RepositoryError> {
        sqlx::query_as(&format!("SELECT {PAGE_COLUMNS} FROM pages WHERE id = ?1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn find_by_slug(&self, slug: &str) -> Result<Option<PageRecord>, RepositoryError> {
        sqlx::query_as(&format!("SELECT {PAGE_COLUMNS} FROM pages WHERE slug = ?1"))
            .bind(slug)
            .fetch_optional(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn list(
        &self,
        include_drafts: bool,
        limit: i64,
    ) -> Result<Vec<PageSummary>, RepositoryError> {
        let sql = if include_drafts {
            format!(
                "SELECT {PAGE_SUMMARY_COLUMNS} FROM pages \
                 ORDER BY updated_at DESC, id DESC LIMIT ?1"
            )
        } else {
            format!(
                "SELECT {PAGE_SUMMARY_COLUMNS} FROM pages WHERE is_published = 1 \
                 ORDER BY updated_at DESC, id DESC LIMIT ?1"
            )
        };

        sqlx::query_as(&sql)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn list_paged(
        &self,
        include_drafts: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PageSummary>, RepositoryError> {
        let sql = if include_drafts {
            format!(
                "SELECT {PAGE_SUMMARY_COLUMNS} FROM pages \
                 ORDER BY updated_at DESC, id DESC LIMIT ?1 OFFSET ?2"
            )
        } else {
            format!(
                "SELECT {PAGE_SUMMARY_COLUMNS} FROM pages WHERE is_published = 1 \
                 ORDER BY updated_at DESC, id DESC LIMIT ?1 OFFSET ?2"
            )
        };

        sqlx::query_as(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn search(
        &self,
        terms: &str,
        include_drafts: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<SearchHit>, RepositoryError> {
        // An unsanitizable query is not an error — it is no query.
        let Some(match_query) = fts_match_query(terms) else {
            return Ok(Vec::new());
        };
        // `snippet` reads column 1 (`content`; column 0 is `title`) and
        // wraps the hits in the markers the handlers split on. The
        // draft gate lives in the JOIN's WHERE, decided by the caller.
        sqlx::query_as(
            "SELECT p.id AS id, p.slug AS slug, p.title AS title, \
             p.is_published AS is_published, p.updated_at AS updated_at, \
             snippet(pages_fts, 1, '⟦', '⟧', '…', 12) AS fragment \
             FROM pages_fts JOIN pages p ON p.id = pages_fts.rowid \
             WHERE pages_fts MATCH ?1 AND (p.is_published = 1 OR ?2) \
             ORDER BY bm25(pages_fts), p.id LIMIT ?3 OFFSET ?4",
        )
        .bind(match_query)
        .bind(include_drafts)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn search_count(
        &self,
        terms: &str,
        include_drafts: bool,
    ) -> Result<i64, RepositoryError> {
        let Some(match_query) = fts_match_query(terms) else {
            return Ok(0);
        };
        sqlx::query_scalar(
            "SELECT count(*) FROM pages_fts JOIN pages p ON p.id = pages_fts.rowid \
             WHERE pages_fts MATCH ?1 AND (p.is_published = 1 OR ?2)",
        )
        .bind(match_query)
        .bind(include_drafts)
        .fetch_one(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn feed_entries(&self, limit: i64) -> Result<Vec<FeedEntry>, RepositoryError> {
        sqlx::query_as(
            "SELECT id, slug, title, updated_at, meta_description FROM pages \
             WHERE is_published = 1 ORDER BY updated_at DESC, id DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn update(
        &self,
        id: i64,
        update: &PageUpdate,
    ) -> Result<Option<PageRecord>, RepositoryError> {
        let updated_at = unix_now();

        // `COALESCE` keeps the current slug when the update carries none
        // (a `None` binding is SQL `NULL`). `parent_id` binds plainly:
        // the form always states the intended parent, and `NULL` means
        // top level (see `PageUpdate`).
        let result = sqlx::query(
            "UPDATE pages SET slug = COALESCE(?2, slug), title = ?3, content = ?4, \
             is_published = ?5, updated_at = ?6, parent_id = ?7, position = ?8, \
             meta_title = ?9, meta_description = ?10, og_image = ?11, body_format = ?12 \
             WHERE id = ?1",
        )
        .bind(id)
        .bind(update.slug.as_deref())
        .bind(&update.title)
        .bind(&update.content)
        .bind(update.is_published)
        .bind(updated_at)
        .bind(update.parent_id)
        .bind(update.position)
        .bind(&update.meta_title)
        .bind(&update.meta_description)
        .bind(&update.og_image)
        .bind(update.body_format.as_str())
        .execute(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.find_by_id(id).await
    }

    async fn delete(&self, id: i64) -> Result<bool, RepositoryError> {
        let result = sqlx::query("DELETE FROM pages WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)?;

        Ok(result.rows_affected() > 0)
    }

    async fn count(&self) -> Result<i64, RepositoryError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM pages")
            .fetch_one(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn count_published(&self) -> Result<i64, RepositoryError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM pages WHERE is_published = 1")
            .fetch_one(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn ancestors(&self, id: i64) -> Result<Vec<PageSummary>, RepositoryError> {
        let mut chain = Vec::new();
        let mut current = id;
        for _ in 0..MAX_ANCESTOR_HOPS {
            let parent = sqlx::query_as::<_, PageSummary>(&format!(
                "SELECT {PAGE_SUMMARY_COLUMNS} FROM pages WHERE id = ?1"
            ))
            .bind(current)
            .fetch_optional(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)?;

            let Some(page) = parent else {
                // A parent disappeared mid-walk: the chain simply stops.
                break;
            };
            // The walk starts at the page itself, which is NOT its own
            // ancestor — only the chain above it is collected.
            if page.id != id {
                chain.insert(0, page.clone());
            }
            match page.parent_id {
                // A corrupted self-parent loop would hang the walk; the
                // hop budget bounds it and this guard exits early.
                Some(parent_id) if parent_id != page.id => current = parent_id,
                _ => break,
            }
        }
        Ok(chain)
    }

    async fn children(
        &self,
        parent_id: Option<i64>,
        include_drafts: bool,
    ) -> Result<Vec<PageSummary>, RepositoryError> {
        // `parent_id IS ?1` matches both a concrete id and `NULL`
        // (top level) without string-building the condition.
        let sql = format!(
            "SELECT {PAGE_SUMMARY_COLUMNS} FROM pages WHERE parent_id IS ?1 \
             AND (is_published = 1 OR ?2) ORDER BY position, id"
        );
        sqlx::query_as(&sql)
            .bind(parent_id)
            .bind(include_drafts)
            .fetch_all(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }
}

// ─── Menus (F7, corporate content model) ─────────────────────────────

/// Values needed to insert a named navigation menu.
#[derive(Debug, Clone)]
pub struct NewMenu {
    /// Machine name (slug-shaped, immutable after creation): the
    /// `menus` template global is keyed by it — `menus.main`.
    pub name: String,
    /// Human title for the admin listing.
    pub title: String,
}

/// A persisted menu row (see `migrations/0004_*`).
#[derive(Debug, Clone)]
pub struct MenuRecord {
    pub id: i64,
    pub name: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl<'r> FromRow<'r, SqliteRow> for MenuRecord {
    fn from_row(row: &'r SqliteRow) -> Result<Self, SqlxError> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            title: row.try_get("title")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// Values needed to insert (or replace) a menu item: a link to a page
/// or a custom URL — exactly one of the two.
#[derive(Debug, Clone)]
pub struct NewMenuItem {
    pub menu_id: i64,
    /// Sibling ordering, lower first.
    pub position: i64,
    /// Label override; `None` falls back to the linked page's title
    /// (custom-URL items must carry a label — validated by the forms).
    pub label: Option<String>,
    /// The linked page (draft pages are skipped in the public `menus`
    /// global until published).
    pub page_id: Option<i64>,
    /// A custom or external URL: `/aviso-legal`, `https://…`.
    pub url: Option<String>,
}

/// A persisted menu item row.
#[derive(Debug, Clone)]
pub struct MenuItemRecord {
    pub id: i64,
    pub menu_id: i64,
    pub position: i64,
    pub label: Option<String>,
    pub page_id: Option<i64>,
    pub url: Option<String>,
}

impl<'r> FromRow<'r, SqliteRow> for MenuItemRecord {
    fn from_row(row: &'r SqliteRow) -> Result<Self, SqlxError> {
        Ok(Self {
            id: row.try_get("id")?,
            menu_id: row.try_get("menu_id")?,
            position: row.try_get("position")?,
            label: row.try_get("label")?,
            page_id: row.try_get("page_id")?,
            url: row.try_get("url")?,
        })
    }
}

/// The page projection joined into menu item listings (the admin
/// detail view needs slug/title/published state without the content).
#[derive(Debug, Clone)]
pub struct MenuPageProjection {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub is_published: bool,
}

/// A menu item with the page half-resolved: the item row plus the
/// linked page's slug/title when it exists (drafts included — this is
/// the admin view; the public `menus` global filters them).
#[derive(Debug, Clone)]
pub struct MenuItemWithPage {
    pub item: MenuItemRecord,
    pub page: Option<MenuPageProjection>,
}

/// A menu item fully resolved for rendering — what the `menus`
/// template global consumes.
#[derive(Debug, Clone)]
pub struct ResolvedMenuItem {
    /// Sibling ordering (kept so templates can restyle ordered lists).
    pub position: i64,
    pub label: String,
    /// `/p/<slug>` for page links, the custom URL as stored otherwise.
    pub url: String,
}

/// Storage abstraction for the named navigation menus.
///
/// Same shape as [`PageRepository`]: handlers and the template globals
/// depend on the trait, not on SQLite.
#[async_trait]
pub trait MenuRepository: Send + Sync + 'static {
    /// Inserts a menu. Fails with [`RepositoryError::Duplicate`] when
    /// the name is already taken.
    async fn create(&self, menu: &NewMenu) -> Result<MenuRecord, RepositoryError>;

    /// Looks up a menu by id.
    async fn find_by_id(&self, id: i64) -> Result<Option<MenuRecord>, RepositoryError>;

    /// Every menu ordered by name (the admin listing).
    async fn list(&self) -> Result<Vec<MenuRecord>, RepositoryError>;

    /// Counts menus.
    async fn count(&self) -> Result<i64, RepositoryError>;

    /// Counts the items of one menu (the admin listing badge).
    async fn count_items(&self, menu_id: i64) -> Result<i64, RepositoryError>;

    /// Renames a menu's **title**; the name is the template key and
    /// stays immutable. Returns `None` when the menu does not exist.
    async fn update_title(
        &self,
        id: i64,
        title: &str,
    ) -> Result<Option<MenuRecord>, RepositoryError>;

    /// Deletes the menu and (by the schema) its items.
    async fn delete(&self, id: i64) -> Result<bool, RepositoryError>;

    /// The items of one menu with their page projection, ordered by
    /// `position` then id — the admin detail view (drafts included).
    async fn items_with_pages(
        &self,
        menu_id: i64,
    ) -> Result<Vec<MenuItemWithPage>, RepositoryError>;

    /// Looks up one item by id.
    async fn find_item(&self, item_id: i64) -> Result<Option<MenuItemRecord>, RepositoryError>;

    /// Adds an item to its menu. The caller validates the
    /// page-xor-url shape first; the schema `CHECK` is the backstop.
    async fn add_item(&self, item: &NewMenuItem) -> Result<MenuItemRecord, RepositoryError>;

    /// Replaces an item. Returns `None` when it does not exist.
    async fn update_item(
        &self,
        item_id: i64,
        item: &NewMenuItem,
    ) -> Result<Option<MenuItemRecord>, RepositoryError>;

    /// Deletes one item.
    async fn delete_item(&self, item_id: i64) -> Result<bool, RepositoryError>;

    /// Every menu with its items resolved to label + href — **published
    /// pages only**: draft and deleted-page links are skipped, so the
    /// public navigation never 404s by construction. Menus ordered by
    /// name (the `menus` template global).
    async fn resolved(&self) -> Result<Vec<(MenuRecord, Vec<ResolvedMenuItem>)>, RepositoryError>;
}

/// SQLite-backed [`MenuRepository`] over a shared pool.
#[derive(Clone)]
pub struct SqliteMenuRepository {
    pool: SqlitePool,
}

impl SqliteMenuRepository {
    /// Wraps an already-migrated pool into a repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MenuRepository for SqliteMenuRepository {
    async fn create(&self, menu: &NewMenu) -> Result<MenuRecord, RepositoryError> {
        let now = unix_now();
        let result = sqlx::query(
            "INSERT INTO menus (name, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&menu.name)
        .bind(&menu.title)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;

        match result {
            Ok(done) => Ok(MenuRecord {
                id: done.last_insert_rowid(),
                name: menu.name.clone(),
                title: menu.title.clone(),
                created_at: now,
                updated_at: now,
            }),
            Err(error) => Err(RepositoryError::from_sqlx(error)),
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<MenuRecord>, RepositoryError> {
        sqlx::query_as("SELECT id, name, title, created_at, updated_at FROM menus WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn list(&self) -> Result<Vec<MenuRecord>, RepositoryError> {
        sqlx::query_as("SELECT id, name, title, created_at, updated_at FROM menus ORDER BY name")
            .fetch_all(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn count(&self) -> Result<i64, RepositoryError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM menus")
            .fetch_one(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn count_items(&self, menu_id: i64) -> Result<i64, RepositoryError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM menu_items WHERE menu_id = ?1")
            .bind(menu_id)
            .fetch_one(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn update_title(
        &self,
        id: i64,
        title: &str,
    ) -> Result<Option<MenuRecord>, RepositoryError> {
        let result = sqlx::query("UPDATE menus SET title = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(id)
            .bind(title)
            .bind(unix_now())
            .execute(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.find_by_id(id).await
    }

    async fn delete(&self, id: i64) -> Result<bool, RepositoryError> {
        let result = sqlx::query("DELETE FROM menus WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)?;

        Ok(result.rows_affected() > 0)
    }

    async fn items_with_pages(
        &self,
        menu_id: i64,
    ) -> Result<Vec<MenuItemWithPage>, RepositoryError> {
        sqlx::query(
            "SELECT mi.id, mi.menu_id, mi.position, mi.label, mi.page_id, mi.url, \
             p.id AS page_id2, p.slug AS page_slug, p.title AS page_title, \
             p.is_published AS page_is_published \
             FROM menu_items mi LEFT JOIN pages p ON p.id = mi.page_id \
             WHERE mi.menu_id = ?1 ORDER BY mi.position, mi.id",
        )
        .bind(menu_id)
        .fetch_all(&self.pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|row| {
                    let item = MenuItemRecord {
                        id: row.try_get("id").expect("mi.id is always present"),
                        menu_id: row.try_get("menu_id").expect("mi.menu_id is present"),
                        position: row.try_get("position").expect("mi.position is present"),
                        label: row.try_get("label").expect("mi.label column"),
                        page_id: row.try_get("page_id").expect("mi.page_id column"),
                        url: row.try_get("url").expect("mi.url column"),
                    };
                    let page = row
                        .try_get::<Option<i64>, _>("page_id2")
                        .expect("joined page id")
                        .map(|_| MenuPageProjection {
                            id: row.try_get("page_id2").expect("joined page id"),
                            slug: row.try_get("page_slug").expect("joined slug"),
                            title: row.try_get("page_title").expect("joined title"),
                            is_published: row
                                .try_get::<i64, _>("page_is_published")
                                .expect("joined published flag")
                                != 0,
                        });
                    MenuItemWithPage { item, page }
                })
                .collect()
        })
        .map_err(RepositoryError::from_sqlx)
    }

    async fn find_item(&self, item_id: i64) -> Result<Option<MenuItemRecord>, RepositoryError> {
        sqlx::query_as(
            "SELECT id, menu_id, position, label, page_id, url FROM menu_items WHERE id = ?1",
        )
        .bind(item_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn add_item(&self, item: &NewMenuItem) -> Result<MenuItemRecord, RepositoryError> {
        let result = sqlx::query(
            "INSERT INTO menu_items (menu_id, position, label, page_id, url) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(item.menu_id)
        .bind(item.position)
        .bind(&item.label)
        .bind(item.page_id)
        .bind(&item.url)
        .execute(&self.pool)
        .await;

        match result {
            Ok(done) => Ok(MenuItemRecord {
                id: done.last_insert_rowid(),
                menu_id: item.menu_id,
                position: item.position,
                label: item.label.clone(),
                page_id: item.page_id,
                url: item.url.clone(),
            }),
            Err(error) => Err(RepositoryError::from_sqlx(error)),
        }
    }

    async fn update_item(
        &self,
        item_id: i64,
        item: &NewMenuItem,
    ) -> Result<Option<MenuItemRecord>, RepositoryError> {
        let result = sqlx::query(
            "UPDATE menu_items SET menu_id = ?2, position = ?3, label = ?4, page_id = ?5, \
             url = ?6 WHERE id = ?1",
        )
        .bind(item_id)
        .bind(item.menu_id)
        .bind(item.position)
        .bind(&item.label)
        .bind(item.page_id)
        .bind(&item.url)
        .execute(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.find_item(item_id).await
    }

    async fn delete_item(&self, item_id: i64) -> Result<bool, RepositoryError> {
        let result = sqlx::query("DELETE FROM menu_items WHERE id = ?1")
            .bind(item_id)
            .execute(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)?;

        Ok(result.rows_affected() > 0)
    }

    async fn resolved(&self) -> Result<Vec<(MenuRecord, Vec<ResolvedMenuItem>)>, RepositoryError> {
        // One joined pass: menus LEFT JOIN items LEFT JOIN pages. Items
        // pointing at draft or missing pages are filtered in Rust so
        // the public navigation never links a 404.
        let rows = sqlx::query(
            "SELECT m.id, m.name, m.title, m.created_at, m.updated_at, \
             mi.id AS item_id, mi.position, mi.label, mi.page_id, mi.url, \
             p.slug AS page_slug, p.title AS page_title, p.is_published AS page_is_published \
             FROM menus m \
             LEFT JOIN menu_items mi ON mi.menu_id = m.id \
             LEFT JOIN pages p ON p.id = mi.page_id \
             ORDER BY m.name, mi.position, mi.id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)?;

        let mut ordered: Vec<(MenuRecord, Vec<ResolvedMenuItem>)> = Vec::new();
        let mut by_name: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for row in rows {
            let menu = MenuRecord {
                id: row.try_get("id").expect("menus.id is always present"),
                name: row.try_get("name").expect("menus.name is always present"),
                title: row.try_get("title").expect("menus.title is always present"),
                created_at: row.try_get("created_at").expect("menus.created_at"),
                updated_at: row.try_get("updated_at").expect("menus.updated_at"),
            };
            let index = *by_name.entry(menu.name.clone()).or_insert_with(|| {
                ordered.push((menu, Vec::new()));
                ordered.len() - 1
            });

            // A menu with no items: the LEFT JOIN produced one row with
            // item_id NULL — nothing to append.
            if row
                .try_get::<Option<i64>, _>("item_id")
                .expect("item id")
                .is_none()
            {
                continue;
            }

            let page_id: Option<i64> = row.try_get("page_id").expect("mi.page_id column");
            let url: Option<String> = row.try_get("url").expect("mi.url column");
            let page_slug: Option<String> = row.try_get("page_slug").expect("joined slug");
            let page_published: Option<i64> = row
                .try_get("page_is_published")
                .expect("joined published flag");

            let resolved = match (page_id, page_slug, page_published) {
                // Page link, published: resolve to /p/<slug>.
                (Some(_), Some(slug), Some(1)) => {
                    let title: String = row.try_get("page_title").expect("joined title");
                    ResolvedMenuItem {
                        position: row.try_get("position").expect("mi.position"),
                        label: row
                            .try_get::<Option<String>, _>("label")
                            .expect("mi.label column")
                            .unwrap_or(title),
                        url: format!("/p/{slug}"),
                    }
                }
                // Draft or deleted page: skip the item publicly.
                (Some(_), _, _) => continue,
                // Custom URL: label is mandatory (form-validated).
                (None, _, _) => ResolvedMenuItem {
                    position: row.try_get("position").expect("mi.position"),
                    label: row
                        .try_get::<Option<String>, _>("label")
                        .expect("mi.label column")
                        .unwrap_or_default(),
                    url: url.unwrap_or_default(),
                },
            };

            ordered[index].1.push(resolved);
        }

        Ok(ordered)
    }
}

// ─── The media library (F9) ──────────────────────────────────────────

/// Values needed to insert a media row. The file bytes themselves live
/// on disk under `media_dir`; the row carries the names and the
/// provenance.
#[derive(Debug, Clone)]
pub struct NewMedia {
    /// Flat server-generated file name (`<32-hex>.<ext>`).
    pub stored_name: String,
    /// Flat server-generated thumbnail name (`<32-hex>_t.png`).
    pub thumb_name: String,
    /// The uploader's file name, display-only.
    pub original_name: String,
    /// The sniffed mime type (see `src/media.rs`).
    pub mime_type: String,
    pub bytes: i64,
    pub width: i64,
    pub height: i64,
    pub alt_text: String,
    pub created_by: Option<i64>,
}

/// A persisted media row (see `migrations/0006_*`).
#[derive(Debug, Clone)]
pub struct MediaRecord {
    pub id: i64,
    pub stored_name: String,
    pub thumb_name: String,
    pub original_name: String,
    pub mime_type: String,
    pub bytes: i64,
    pub width: i64,
    pub height: i64,
    pub alt_text: String,
    /// Creation time, unix seconds.
    pub created_at: i64,
    pub created_by: Option<i64>,
}

impl<'r> FromRow<'r, SqliteRow> for MediaRecord {
    fn from_row(row: &'r SqliteRow) -> Result<Self, SqlxError> {
        Ok(Self {
            id: row.try_get("id")?,
            stored_name: row.try_get("stored_name")?,
            thumb_name: row.try_get("thumb_name")?,
            original_name: row.try_get("original_name")?,
            mime_type: row.try_get("mime_type")?,
            bytes: row.try_get("bytes")?,
            width: row.try_get("width")?,
            height: row.try_get("height")?,
            alt_text: row.try_get("alt_text")?,
            created_at: row.try_get("created_at")?,
            created_by: row.try_get("created_by")?,
        })
    }
}

/// Column list shared by every `SELECT` on the `media` table.
const MEDIA_COLUMNS: &str = "id, stored_name, thumb_name, original_name, mime_type, \
                           bytes, width, height, alt_text, created_at, created_by";

/// Storage abstraction for the media library (F9).
///
/// Same shape as [`PageRepository`]: handlers depend on the trait, not
/// on SQLite. The files themselves are written by the upload route
/// around the row insert; this layer only owns the metadata.
#[async_trait]
pub trait MediaRepository: Send + Sync + 'static {
    /// Inserts a media row. The caller has already written the files;
    /// on failure it removes them again (the row and the disk never
    /// disagree for long).
    async fn create(&self, media: &NewMedia) -> Result<MediaRecord, RepositoryError>;

    /// Looks up a media row by id (the serving and detail routes).
    async fn find_by_id(&self, id: i64) -> Result<Option<MediaRecord>, RepositoryError>;

    /// The listing window (F10): `limit` newest rows from `offset` —
    /// the paginated admin grid. `count` reports the total.
    async fn list_paged(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MediaRecord>, RepositoryError>;

    /// How many media rows exist in total (the pagination's other
    /// half).
    async fn count(&self) -> Result<i64, RepositoryError>;

    /// Replaces the alt text. Returns the updated row, or `None` when
    /// the id does not exist.
    async fn update_alt(
        &self,
        id: i64,
        alt_text: &str,
    ) -> Result<Option<MediaRecord>, RepositoryError>;

    /// Deletes the row. The caller removes the files afterwards.
    /// Returns whether a row was removed.
    async fn delete(&self, id: i64) -> Result<bool, RepositoryError>;
}

/// SQLite-backed [`MediaRepository`] over a shared pool.
#[derive(Clone)]
pub struct SqliteMediaRepository {
    pool: SqlitePool,
}

impl SqliteMediaRepository {
    /// Wraps an already-migrated pool into a repository.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MediaRepository for SqliteMediaRepository {
    async fn create(&self, media: &NewMedia) -> Result<MediaRecord, RepositoryError> {
        let now = unix_now();
        let result = sqlx::query(
            "INSERT INTO media (stored_name, thumb_name, original_name, mime_type, \
             bytes, width, height, alt_text, created_at, created_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&media.stored_name)
        .bind(&media.thumb_name)
        .bind(&media.original_name)
        .bind(&media.mime_type)
        .bind(media.bytes)
        .bind(media.width)
        .bind(media.height)
        .bind(&media.alt_text)
        .bind(now)
        .bind(media.created_by)
        .execute(&self.pool)
        .await;

        match result {
            Ok(done) => Ok(MediaRecord {
                id: done.last_insert_rowid(),
                stored_name: media.stored_name.clone(),
                thumb_name: media.thumb_name.clone(),
                original_name: media.original_name.clone(),
                mime_type: media.mime_type.clone(),
                bytes: media.bytes,
                width: media.width,
                height: media.height,
                alt_text: media.alt_text.clone(),
                created_at: now,
                created_by: media.created_by,
            }),
            Err(error) => Err(RepositoryError::from_sqlx(error)),
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<MediaRecord>, RepositoryError> {
        sqlx::query_as(&format!("SELECT {MEDIA_COLUMNS} FROM media WHERE id = ?1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn list_paged(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MediaRecord>, RepositoryError> {
        sqlx::query_as(&format!(
            "SELECT {MEDIA_COLUMNS} FROM media ORDER BY id DESC LIMIT ?1 OFFSET ?2"
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(RepositoryError::from_sqlx)
    }

    async fn count(&self) -> Result<i64, RepositoryError> {
        sqlx::query_scalar("SELECT count(*) FROM media")
            .fetch_one(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)
    }

    async fn update_alt(
        &self,
        id: i64,
        alt_text: &str,
    ) -> Result<Option<MediaRecord>, RepositoryError> {
        let updated = sqlx::query("UPDATE media SET alt_text = ?2 WHERE id = ?1")
            .bind(id)
            .bind(alt_text)
            .execute(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)?;

        if updated.rows_affected() == 0 {
            return Ok(None);
        }
        self.find_by_id(id).await
    }

    async fn delete(&self, id: i64) -> Result<bool, RepositoryError> {
        let removed = sqlx::query("DELETE FROM media WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(RepositoryError::from_sqlx)?;

        Ok(removed.rows_affected() > 0)
    }
}

/// Opens a SQLite pool with the project's recommended settings.
///
/// # Errors
///
/// Returns a [`SqlxError`] when the URL is malformed or the database
/// cannot be opened.
pub async fn connect(config: &DatabaseConfig) -> Result<SqlitePool, SqlxError> {
    let options = SqliteConnectOptions::from_str(&config.url)?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .connect_with(options)
        .await
}

/// Runs the embedded migrations (from the `migrations/` directory)
/// against the pool.
///
/// Already-applied migrations are skipped; the SQL is validated and
/// checksummed by sqlx itself.
///
/// # Errors
///
/// Returns the migration failure as a displayable message (startup-only
/// path, reported straight to the operator).
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), String> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|error| error.to_string())
}

/// Current unix time in seconds (never panics, saturates at zero).
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh temporary file database, removed (best-effort) on drop.
    struct TempDb {
        url: String,
    }

    impl TempDb {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wallermax-db-tests-{}-{unique}.db",
                std::process::id()
            ));
            // Forward slashes keep the URL valid on Windows too.
            Self {
                url: format!(
                    "sqlite://{}?mode=rwc",
                    path.display().to_string().replace('\\', "/")
                ),
            }
        }

        /// Filesystem path backing the URL.
        fn path(&self) -> std::path::PathBuf {
            let file = self
                .url
                .strip_prefix("sqlite://")
                .and_then(|rest| rest.split('?').next())
                .expect("url shape is produced by this helper");
            std::path::PathBuf::from(file)
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let base = self.path();
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", base.display()));
            }
        }
    }

    /// Database config pointing at the temporary file.
    fn config_for(db: &TempDb) -> DatabaseConfig {
        DatabaseConfig {
            enabled: true,
            url: db.url.clone(),
            max_connections: 2,
        }
    }

    /// Connects and migrates a fresh temporary database.
    async fn repository(db: &TempDb) -> SqliteUserRepository {
        let pool = connect(&config_for(db)).await.expect("pool connects");
        run_migrations(&pool).await.expect("migrations apply");
        SqliteUserRepository::new(pool)
    }

    /// Registers a user directly in the repository (helper for the tests
    /// below; the hash is irrelevant for storage-level tests).
    async fn seed(repo: &SqliteUserRepository, username: &str) -> User {
        repo.create(username, "irrelevant-hash", UserRole::User)
            .await
            .expect("seed user created")
    }

    #[tokio::test]
    async fn create_and_find_roundtrip() {
        let db = TempDb::new();
        let repo = repository(&db).await;

        let created = repo
            .create("alice", "some-hash", UserRole::Admin)
            .await
            .expect("user created");

        assert!(created.id > 0);
        assert_eq!(created.username, "alice");
        assert_eq!(created.role, UserRole::Admin);
        assert!(created.created_at > 0);
        assert_eq!(created.last_login_at, None);

        let found = repo
            .find_by_username("alice")
            .await
            .expect("lookup succeeds")
            .expect("user exists");
        assert_eq!(found.id, created.id);
        assert_eq!(found.username, "alice");
        assert_eq!(found.role, UserRole::Admin);
        assert_eq!(found.password_hash, "some-hash");

        let by_id = repo
            .find_by_id(created.id)
            .await
            .expect("lookup succeeds")
            .expect("user exists");
        assert_eq!(by_id.username, "alice");
    }

    #[tokio::test]
    async fn usernames_are_unique_case_insensitively() {
        let db = TempDb::new();
        let repo = repository(&db).await;

        seed(&repo, "Alice").await;

        match repo.create("alice", "other-hash", UserRole::User).await {
            Err(RepositoryError::Duplicate) => {}
            other => panic!("expected Duplicate, got {other:?}"),
        }

        // Lookup stays case-insensitive as well.
        let found = repo
            .find_by_username("ALICE")
            .await
            .expect("lookup succeeds")
            .expect("case-insensitive match");
        assert_eq!(found.username, "Alice");
    }

    #[tokio::test]
    async fn missing_lookups_return_none() {
        let db = TempDb::new();
        let repo = repository(&db).await;

        assert!(repo
            .find_by_username("ghost")
            .await
            .expect("query ok")
            .is_none());
        assert!(repo.find_by_id(9999).await.expect("query ok").is_none());
    }

    #[tokio::test]
    async fn count_and_list_track_creates() {
        let db = TempDb::new();
        let repo = repository(&db).await;

        seed(&repo, "first").await;
        seed(&repo, "second").await;
        seed(&repo, "third").await;

        assert_eq!(repo.count().await.expect("count ok"), 3);

        let listed = repo.list(2).await.expect("list ok");
        assert_eq!(listed.len(), 2);
        // Newest first.
        assert_eq!(listed[0].username, "third");
        assert_eq!(listed[1].username, "second");
    }

    #[tokio::test]
    async fn record_login_stamps_last_login() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        let user = seed(&repo, "alice").await;

        assert_eq!(user.last_login_at, None);

        repo.record_login(user.id).await.expect("stamp ok");

        let reloaded = repo
            .find_by_id(user.id)
            .await
            .expect("query ok")
            .expect("user exists");
        let stamped = reloaded.last_login_at.expect("last_login_at stamped");
        assert!(stamped >= user.created_at);
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        seed(&repo, "persisted").await;

        // A second connection to the same file must see the same schema and
        // data, and re-running migrations must succeed without changes.
        let pool = connect(&config_for(&db)).await.expect("pool reconnects");
        run_migrations(&pool).await.expect("migrations re-apply");

        let still_there = repo
            .find_by_username("persisted")
            .await
            .expect("query ok")
            .expect("data survived");
        assert_eq!(still_there.username, "persisted");
    }

    #[tokio::test]
    async fn unknown_role_values_fail_decoding() {
        let db = TempDb::new();
        let repo = repository(&db).await;

        sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at) \
             VALUES ('bogus', 'hash', 'superuser', 0)",
        )
        .execute(&repo.pool)
        .await
        .expect("raw row inserted");

        match repo.find_by_username("bogus").await {
            Err(RepositoryError::Internal(_)) => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    /// Builds a token payload with deterministic times.
    fn new_token(user_id: i64, family_id: Option<i64>, token_hash: &str) -> NewRefreshToken {
        NewRefreshToken {
            user_id,
            family_id,
            token_hash: token_hash.to_owned(),
            expires_at: 1_000_000,
            created_at: 900_000,
            created_ip: Some("127.0.0.1".to_owned()),
            user_agent: Some("test-agent/1.0".to_owned()),
        }
    }

    #[tokio::test]
    async fn refresh_token_roundtrip_and_families() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        let user = seed(&repo, "alice").await;

        let root = repo
            .save_refresh_token(&new_token(user.id, None, "hash-a"))
            .await
            .expect("root token saved");
        assert_eq!(root.family_id, root.id);

        let successor = repo
            .save_refresh_token(&new_token(user.id, Some(root.family_id), "hash-b"))
            .await
            .expect("successor saved");
        assert_eq!(successor.family_id, root.family_id);
        assert_eq!(successor.rotated_at, None);
        assert_eq!(successor.revoked_at, None);

        let found = repo
            .find_refresh_token_by_hash("hash-b")
            .await
            .expect("query ok")
            .expect("row exists");
        assert_eq!(found.id, successor.id);
        assert_eq!(found.created_ip.as_deref(), Some("127.0.0.1"));

        assert!(repo
            .find_refresh_token_by_hash("missing")
            .await
            .expect("query ok")
            .is_none());
    }

    #[tokio::test]
    async fn token_hashes_are_unique() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        let user = seed(&repo, "alice").await;

        repo.save_refresh_token(&new_token(user.id, None, "same"))
            .await
            .expect("first saved");

        match repo
            .save_refresh_token(&new_token(user.id, None, "same"))
            .await
        {
            Err(RepositoryError::Duplicate) => {}
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rotation_is_single_shot() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        let user = seed(&repo, "alice").await;

        let token = repo
            .save_refresh_token(&new_token(user.id, None, "hash-a"))
            .await
            .expect("token saved");

        assert!(repo
            .rotate_refresh_token(token.id, 950_000)
            .await
            .expect("rotate ok"));
        // Second rotation (the reuse case) reports false.
        assert!(!repo
            .rotate_refresh_token(token.id, 960_000)
            .await
            .expect("rotate ok"));

        let found = repo
            .find_refresh_token_by_hash("hash-a")
            .await
            .expect("query ok")
            .expect("row exists");
        assert_eq!(found.rotated_at, Some(950_000));
    }

    #[tokio::test]
    async fn family_revocation_kills_the_chain() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        let user = seed(&repo, "alice").await;

        let root = repo
            .save_refresh_token(&new_token(user.id, None, "hash-a"))
            .await
            .expect("root saved");
        let successor = repo
            .save_refresh_token(&new_token(user.id, Some(root.family_id), "hash-b"))
            .await
            .expect("successor saved");

        repo.revoke_refresh_token_family(root.family_id, 970_000)
            .await
            .expect("family revoked");

        for hash in ["hash-a", "hash-b"] {
            let found = repo
                .find_refresh_token_by_hash(hash)
                .await
                .expect("query ok")
                .expect("row exists");
            assert_eq!(found.revoked_at, Some(970_000));
        }

        // Revocation beats rotation attempts.
        assert!(!repo
            .rotate_refresh_token(successor.id, 980_000)
            .await
            .expect("rotate ok"));
    }

    #[tokio::test]
    async fn revoke_all_covers_every_family_of_the_user() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        let alice = seed(&repo, "alice").await;
        let bob = seed(&repo, "bob").await;

        repo.save_refresh_token(&new_token(alice.id, None, "alice-1"))
            .await
            .expect("saved");
        repo.save_refresh_token(&new_token(alice.id, None, "alice-2"))
            .await
            .expect("saved");
        let bob_token = repo
            .save_refresh_token(&new_token(bob.id, None, "bob-1"))
            .await
            .expect("saved");

        repo.revoke_all_refresh_tokens(alice.id, 990_000)
            .await
            .expect("revoked");

        for hash in ["alice-1", "alice-2"] {
            let found = repo
                .find_refresh_token_by_hash(hash)
                .await
                .expect("query ok")
                .expect("row exists");
            assert_eq!(found.revoked_at, Some(990_000));
        }

        let untouched = repo
            .find_refresh_token_by_hash("bob-1")
            .await
            .expect("query ok")
            .expect("row exists");
        assert_eq!(untouched.id, bob_token.id);
        assert_eq!(untouched.revoked_at, None);
    }

    #[tokio::test]
    async fn expired_revoked_rows_are_pruned() {
        let db = TempDb::new();
        let repo = repository(&db).await;
        let user = seed(&repo, "alice").await;

        // Expired + revoked: pruned.
        let mut stale = new_token(user.id, None, "stale");
        stale.expires_at = 1_000;
        let stale = repo.save_refresh_token(&stale).await.expect("saved");
        repo.revoke_refresh_token_family(stale.family_id, 2_000)
            .await
            .expect("revoked");

        // Expired but still active: kept (needed for reuse detection).
        let mut rotated = new_token(user.id, None, "rotated");
        rotated.expires_at = 1_000;
        let rotated = repo.save_refresh_token(&rotated).await.expect("saved");
        repo.rotate_refresh_token(rotated.id, 1_500)
            .await
            .expect("rotated");

        let now = 1_000_000;
        let removed = repo
            .delete_expired_refresh_tokens(now)
            .await
            .expect("prune ok");
        assert_eq!(removed, 1);

        assert!(repo
            .find_refresh_token_by_hash("stale")
            .await
            .expect("query ok")
            .is_none());
        assert!(repo
            .find_refresh_token_by_hash("rotated")
            .await
            .expect("query ok")
            .is_some());
    }

    #[test]
    fn fts_match_query_quotes_every_token_inertly() {
        // Plain words become quoted phrases joined by implicit AND.
        assert_eq!(
            fts_match_query("hola mundo").as_deref(),
            Some("\"hola\" \"mundo\"")
        );
        // FTS5 operators can only act as literals inside the quotes.
        assert_eq!(
            fts_match_query("OR NOT *").as_deref(),
            Some("\"OR\" \"NOT\" \"*\"")
        );
        assert_eq!(
            fts_match_query("title:guia NEAR(x)").as_deref(),
            Some("\"title:guia\" \"NEAR(x)\"")
        );
    }

    #[test]
    fn fts_match_query_strips_embedded_quotes() {
        // An embedded quote is dropped, never escaped into the phrase —
        // the phrase stays a phrase no matter what the visitor typed.
        assert_eq!(
            fts_match_query("el \"menu\" del sitio").as_deref(),
            Some("\"el\" \"menu\" \"del\" \"sitio\"")
        );
    }

    #[test]
    fn fts_match_query_returns_none_when_nothing_quotable_remains() {
        assert_eq!(fts_match_query(""), None);
        assert_eq!(fts_match_query("   "), None);
        assert_eq!(fts_match_query(" \" \" \"\""), None);
    }
}
