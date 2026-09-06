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
    /// Full administrative access (first registered account).
    Admin,
    /// Standard authenticated access.
    User,
}

impl UserRole {
    /// Stable string representation persisted in the database and inside
    /// JWT claims.
    pub fn as_str(self) -> &'static str {
        match self {
            UserRole::Admin => "admin",
            UserRole::User => "user",
        }
    }

    /// Parses a persisted role value; unknown values yield `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "admin" => Some(UserRole::Admin),
            "user" => Some(UserRole::User),
            _ => None,
        }
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

    /// Lists users, newest first, up to `limit` rows.
    async fn list(&self, limit: i64) -> Result<Vec<User>, RepositoryError>;

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

    async fn list(&self, limit: i64) -> Result<Vec<User>, RepositoryError> {
        sqlx::query_as(&format!(
            "SELECT {USER_COLUMNS} FROM users ORDER BY id DESC LIMIT ?1"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
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
}
