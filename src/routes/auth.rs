//! Authentication endpoints: registration, login, token refresh and
//! logout.
//!
//! - `POST /api/auth/register` — create an account (first one becomes the
//!   admin; toggle with `[auth] registration_enabled`).
//! - `POST /api/auth/login` — exchange credentials for a Bearer access
//!   token (plus a refresh token while
//!   `[auth] refresh_tokens_enabled`).
//! - `POST /api/auth/refresh` — rotate a refresh token: a new access
//!   token **and** a new refresh token are issued and the presented one
//!   is retired.
//! - `POST /api/auth/logout` — revoke the session family of the given
//!   refresh token (requires a valid access token).
//! - `POST /api/auth/logout_all` — revoke every refresh token of the
//!   caller.
//! - `GET  /api/auth/me` — the caller's profile (requires a valid token).
//!
//! Refresh tokens are opaque 32-byte values; only their SHA-256 hash is
//! stored. Tokens issued by one login form a **family**: refreshing
//! retires the old token and mints a successor inside the family.
//! Presenting a retired (already rotated) token is treated as theft —
//! the whole family is revoked so a stolen successor dies with it.
//!
//! The route family is only mounted while authentication is enabled, so
//! disabled servers answer these paths with the standard JSON 404.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{
    generate_refresh_token, hash_password, hash_refresh_token, validate_password,
    validate_username, verify_password, MAX_REFRESH_TOKEN_LEN,
};
use crate::db::{NewRefreshToken, PublicUser, RepositoryError, UserRole};
use crate::error::AppError;
use crate::extractors::{AuthUser, JsonBody};
use crate::proxy;
use crate::state::{AppState, AuthContext};
use crate::util::unix_now;

/// Credentials accepted by registration and login.
#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

/// Token payloads accepted by refresh and logout.
#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

/// Login and refresh response payload.
#[derive(Serialize)]
struct LoginResponse {
    access_token: String,
    token_type: &'static str,
    /// Access token lifetime in seconds, mirroring `[auth] token_ttl_secs`.
    expires_in: u64,
    user: PublicUser,
    /// Refresh token (only while refresh tokens are enabled). The value
    /// is shown once; only its SHA-256 hash is stored server-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// Refresh token lifetime in seconds, mirroring
    /// `[auth] refresh_token_ttl_secs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_expires_in: Option<u64>,
}

/// `POST /api/auth/register`: creates a new user account.
///
/// The **first** account on a fresh database bootstraps the `admin` role;
/// every later account is a regular `user`. Role administration beyond
/// that bootstrap is a repository-level concern (see `README.md`).
async fn register(
    State(state): State<AppState>,
    JsonBody(credentials): JsonBody<Credentials>,
) -> Result<(StatusCode, Json<PublicUser>), AppError> {
    let auth = auth_context(&state)?;

    if !auth.registration_enabled {
        return Err(AppError::forbidden(
            "registration is disabled on this server",
        ));
    }

    validate_username(&credentials.username).map_err(AppError::bad_request)?;
    validate_password(&credentials.password, auth.min_password_len)
        .map_err(AppError::bad_request)?;

    let total = auth.repository.count().await.map_err(AppError::from)?;
    let role = if total == 0 {
        UserRole::Admin
    } else {
        UserRole::User
    };

    let password_hash = hash_password(&credentials.password).map_err(|error| {
        tracing::error!(%error, "password hashing failed");
        AppError::internal("password hashing failed")
    })?;

    let user = match auth
        .repository
        .create(&credentials.username, &password_hash, role)
        .await
    {
        Ok(user) => user,
        Err(RepositoryError::Duplicate) => {
            return Err(AppError::conflict("username is already taken"));
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "user storage failure");
            return Err(AppError::internal("storage failure"));
        }
    };

    tracing::info!(
        user_id = user.id,
        role = user.role.as_str(),
        "user registered"
    );

    Ok((StatusCode::CREATED, Json(user.public())))
}

/// `POST /api/auth/login`: exchanges credentials for an access token
/// (and a refresh token while enabled).
///
/// Unknown usernames and wrong passwords produce the **same** error (no
/// user enumeration); the unknown-username path also burns a comparable
/// Argon2 verification so response timing cannot distinguish the cases.
async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    JsonBody(credentials): JsonBody<Credentials>,
) -> Result<Json<LoginResponse>, AppError> {
    let auth = auth_context(&state)?;

    let Some(user) = auth
        .repository
        .find_by_username(&credentials.username)
        .await
        .map_err(AppError::from)?
    else {
        let _ = verify_password(&credentials.password, dummy_hash());
        return Err(AppError::unauthorized("invalid username or password"));
    };

    if !verify_password(&credentials.password, &user.password_hash) {
        return Err(AppError::unauthorized("invalid username or password"));
    }

    let access_token = auth.jwt.issue_token(&user).map_err(|error| {
        tracing::error!(%error, "token signing failed");
        AppError::internal("token signing failed")
    })?;

    // Best-effort bookkeeping: a failed stamp must not fail the login.
    if let Err(error) = auth.repository.record_login(user.id).await {
        tracing::warn!(user_id = user.id, %error, "failed to record last login");
    }

    let (refresh_token, refresh_expires_in) = if auth.refresh_tokens_enabled {
        let (value, _) = issue_refresh_token(&state, auth, &user, None, &peer, &headers).await?;
        (Some(value), Some(auth.refresh_token_ttl_secs))
    } else {
        (None, None)
    };

    tracing::info!(user_id = user.id, "user logged in");

    Ok(Json(LoginResponse {
        access_token,
        token_type: "Bearer",
        expires_in: state.config().auth.token_ttl_secs,
        user: user.public(),
        refresh_token,
        refresh_expires_in,
    }))
}

/// `POST /api/auth/refresh`: rotates a refresh token.
///
/// The presented token is retired (single use) and a successor is minted
/// inside the same family, together with a fresh access token. Retired,
/// revoked, expired, unknown or malformed tokens all answer the same
/// generic 401 — except that a **retired** token additionally revokes
/// its whole family, on the assumption that reuse means the token was
/// stolen.
async fn refresh(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    JsonBody(request): JsonBody<RefreshRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    let auth = auth_context(&state)?;
    let now = unix_now();

    // Housekeeping piggy-backed on every refresh attempt; failures are
    // irrelevant to the caller.
    if let Err(error) = auth.repository.delete_expired_refresh_tokens(now).await {
        tracing::debug!(%error, "refresh token pruning failed");
    }

    let value = sanitize_refresh_token(&request.refresh_token)?;
    let token_hash = hash_refresh_token(value);
    let record = auth
        .repository
        .find_refresh_token_by_hash(&token_hash)
        .await
        .map_err(AppError::from)?;

    let Some(record) = record else {
        return Err(AppError::unauthorized("invalid refresh token"));
    };
    if record.revoked_at.is_some() {
        return Err(AppError::unauthorized("invalid refresh token"));
    }
    if record.rotated_at.is_some() {
        // Reuse of a retired token: assume theft, kill the family.
        tracing::warn!(
            family_id = record.family_id,
            user_id = record.user_id,
            "retired refresh token replayed; revoking the family"
        );
        if let Err(error) = auth
            .repository
            .revoke_refresh_token_family(record.family_id, now)
            .await
        {
            tracing::error!(%error, "family revocation failed");
        }
        return Err(AppError::unauthorized("invalid refresh token"));
    }
    if record.expires_at <= now {
        return Err(AppError::unauthorized("invalid refresh token"));
    }

    // The guarded update is the concurrency anchor: whichever worker
    // rotates first wins, the loser sees `false` and lands in the
    // reuse branch above.
    let rotated = auth
        .repository
        .rotate_refresh_token(record.id, now)
        .await
        .map_err(AppError::from)?;
    if !rotated {
        tracing::warn!(
            family_id = record.family_id,
            user_id = record.user_id,
            "concurrent refresh token rotation detected; revoking the family"
        );
        if let Err(error) = auth
            .repository
            .revoke_refresh_token_family(record.family_id, now)
            .await
        {
            tracing::error!(%error, "family revocation failed");
        }
        return Err(AppError::unauthorized("invalid refresh token"));
    }

    // The account must still exist: a deleted user kills the session.
    let Some(user) = auth
        .repository
        .find_by_id(record.user_id)
        .await
        .map_err(AppError::from)?
    else {
        tracing::warn!(
            family_id = record.family_id,
            user_id = record.user_id,
            "refresh token of a deleted account; revoking the family"
        );
        if let Err(error) = auth
            .repository
            .revoke_refresh_token_family(record.family_id, now)
            .await
        {
            tracing::error!(%error, "family revocation failed");
        }
        return Err(AppError::unauthorized("invalid refresh token"));
    };

    let access_token = auth.jwt.issue_token(&user).map_err(|error| {
        tracing::error!(%error, "token signing failed");
        AppError::internal("token signing failed")
    })?;
    let (new_value, _) =
        issue_refresh_token(&state, auth, &user, Some(record.family_id), &peer, &headers).await?;

    tracing::info!(
        user_id = user.id,
        family_id = record.family_id,
        "refresh token rotated"
    );

    Ok(Json(LoginResponse {
        access_token,
        token_type: "Bearer",
        expires_in: state.config().auth.token_ttl_secs,
        user: user.public(),
        refresh_token: Some(new_value),
        refresh_expires_in: Some(auth.refresh_token_ttl_secs),
    }))
}

/// `POST /api/auth/logout`: revokes the family of the given refresh
/// token.
///
/// Requires a valid access token; the answer is always `204 No Content`
/// so the endpoint reveals nothing about token validity (an unknown,
/// foreign or already revoked token is indistinguishable from success).
async fn logout(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(request): JsonBody<RefreshRequest>,
) -> Result<StatusCode, AppError> {
    let auth = auth_context(&state)?;
    let now = unix_now();

    if let Some(value) = sanitize_refresh_token_opt(&request.refresh_token) {
        let token_hash = hash_refresh_token(value);
        if let Ok(Some(record)) = auth
            .repository
            .find_refresh_token_by_hash(&token_hash)
            .await
        {
            if record.user_id == user.user_id {
                if let Err(error) = auth
                    .repository
                    .revoke_refresh_token_family(record.family_id, now)
                    .await
                {
                    tracing::error!(%error, "logout revocation failed");
                    return Err(AppError::internal("storage failure"));
                }
                tracing::info!(
                    user_id = user.user_id,
                    family_id = record.family_id,
                    "session logged out"
                );
            }
            // A foreign token: silently ignore (still 204).
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/auth/logout_all`: revokes every refresh token of the
/// caller, ending all sessions.
async fn logout_all(State(state): State<AppState>, user: AuthUser) -> Result<StatusCode, AppError> {
    let auth = auth_context(&state)?;

    if let Err(error) = auth
        .repository
        .revoke_all_refresh_tokens(user.user_id, unix_now())
        .await
    {
        tracing::error!(%error, "logout_all revocation failed");
        return Err(AppError::internal("storage failure"));
    }

    tracing::info!(user_id = user.user_id, "all sessions logged out");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/auth/me`: the caller's profile, fresh from the repository.
///
/// Tokens stay valid after account deletion (they are stateless), so this
/// endpoint re-checks existence and answers 401 when the account is gone.
async fn me(State(state): State<AppState>, user: AuthUser) -> Result<Json<PublicUser>, AppError> {
    let auth = auth_context(&state)?;

    let user = auth
        .repository
        .find_by_id(user.user_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::unauthorized("user no longer exists"))?;

    Ok(Json(user.public()))
}

/// Mints and persists a refresh token for `user`, returning the value
/// (shown to the client exactly once) and the stored record.
async fn issue_refresh_token(
    state: &AppState,
    auth: &AuthContext,
    user: &crate::db::User,
    family_id: Option<i64>,
    peer: &SocketAddr,
    headers: &HeaderMap,
) -> Result<(String, crate::db::RefreshTokenRecord), AppError> {
    let now = unix_now();
    let value = generate_refresh_token();
    let token = NewRefreshToken {
        user_id: user.id,
        family_id,
        token_hash: hash_refresh_token(&value),
        expires_at: now + auth.refresh_token_ttl_secs as i64,
        created_at: now,
        created_ip: Some(client_ip_of(state, peer, headers).to_string()),
        user_agent: user_agent_of(headers),
    };

    let record = auth
        .repository
        .save_refresh_token(&token)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to persist the refresh token");
            AppError::internal("storage failure")
        })?;

    Ok((value, record))
}

/// Length-checks a client-supplied refresh token value, rejecting
/// oversized inputs with a 400 before any database work.
///
/// Empty values are **not** rejected here: they are simply unknown
/// tokens (the hash lookup misses and the caller answers the same
/// generic 401 as any other garbage, leaking no validation details).
fn sanitize_refresh_token(value: &str) -> Result<&str, AppError> {
    if value.len() > MAX_REFRESH_TOKEN_LEN {
        return Err(AppError::bad_request(format!(
            "`refresh_token` must be at most {MAX_REFRESH_TOKEN_LEN} characters"
        )));
    }
    Ok(value)
}

/// `logout`-variant of [`sanitize_refresh_token`]: garbage simply means
/// "nothing to revoke" (the answer stays 204).
fn sanitize_refresh_token_opt(value: &str) -> Option<&str> {
    if value.is_empty() || value.len() > MAX_REFRESH_TOKEN_LEN {
        None
    } else {
        Some(value)
    }
}

/// Resolves the client IP of the request, honouring trusted proxies.
fn client_ip_of(state: &AppState, peer: &SocketAddr, headers: &HeaderMap) -> IpAddr {
    let xff = proxy::forwarded_for_values(headers);
    proxy::resolve_client_ip(peer.ip(), &xff, state.trusted_proxies())
}

/// Extracts a bounded `User-Agent` string for the audit columns.
fn user_agent_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|agent| agent.chars().take(200).collect())
}

/// Returns the auth context for a mounted route (always present: the route
/// family is only merged while auth is enabled).
fn auth_context(state: &AppState) -> Result<&AuthContext, AppError> {
    state
        .auth_context()
        .ok_or_else(|| AppError::internal("authentication is not initialized"))
}

/// Argon2 hash of a throwaway value, computed once.
///
/// Login attempts for unknown usernames verify against it, spending the
/// same CPU time as a real check so timing cannot enumerate users.
fn dummy_hash() -> &'static str {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("wallermax-timing-equalizer").unwrap_or_default())
}

/// Route fragment for this module.
///
/// The refresh/logout endpoints are only mounted while
/// `[auth] refresh_tokens_enabled` is set.
pub fn routes(refresh_enabled: bool) -> Router<AppState> {
    let mut router = Router::new()
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/me", get(me));

    if refresh_enabled {
        router = router
            .route("/api/auth/refresh", post(refresh))
            .route("/api/auth/logout", post(logout))
            .route("/api/auth/logout_all", post(logout_all));
    }

    router
}
