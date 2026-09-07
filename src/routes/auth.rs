//! Authentication endpoints: registration, login, token refresh and
//! logout.
//!
//! - `POST /api/auth/register` — create an account (first one becomes the
//!   admin; toggle with `[auth] registration_enabled`). Accepts JSON **and**
//!   browser form posts: the JSON path answers the API envelope, the form
//!   path logs the fresh account in directly (session cookie included) and
//!   redirects.
//! - `POST /api/auth/login` — exchange credentials for a Bearer access
//!   token (plus a refresh token while
//!   `[auth] refresh_tokens_enabled`). Accepts JSON **and** browser form
//!   posts (the `/login` view and the site-wide modal), and always attaches
//!   the access token as the `wallermax_session` cookie.
//! - `POST /api/auth/refresh` — rotate a refresh token: a new access
//!   token **and** a new refresh token are issued and the presented one
//!   is retired (the session cookie is refreshed alongside).
//! - `POST /api/auth/logout` — revoke the session family of the given
//!   refresh token (requires a valid access token) and clear the session
//!   cookie. JSON clients keep the idempotent `204`; **browser forms**
//!   land on a `303` redirect back to the site (no JavaScript, no blank
//!   `204` page) and authenticate leniently: an already-expired session
//!   just clears the cookie.
//! - `POST /api/auth/logout_all` — revoke every refresh token of the
//!   caller and clear the session cookie.
//! - `GET  /api/auth/me` — the caller's profile (requires a valid token
//!   or session cookie).
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

use axum::body::Body;
use axum::extract::{ConnectInfo, FromRequest, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{
    generate_refresh_token, hash_password, hash_refresh_token, validate_password,
    validate_username, verify_password, MAX_REFRESH_TOKEN_LEN,
};
use crate::db::{NewRefreshToken, PublicUser, RepositoryError, UserRole};
use crate::error::AppError;
use crate::extractors::{bearer_token, AuthUser, JsonBody};
use crate::middleware::request_id::RequestId;
use crate::proxy;
use crate::session;
use crate::state::{AppState, AuthContext};
use crate::util::{read_form, unix_now};
use axum::extract::FromRequestParts;

/// Credentials accepted by registration and login.
#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

/// Login form fields (`application/x-www-form-urlencoded`, posted by
/// the `/login` view and the site-wide modal): the credentials plus the
/// optional local `redirect` target the browser is sent to after the
/// `303`.
#[derive(Deserialize, Default)]
struct FormCredentials {
    username: String,
    password: String,
    redirect: Option<String>,
}

/// Registration form fields: the credentials plus the optional local
/// `redirect` target (the modal posts the page it was opened from).
#[derive(Deserialize, Default)]
struct FormRegister {
    username: String,
    password: String,
    redirect: Option<String>,
}

/// Logout form fields: a browser form has no refresh token to offer
/// (the cookie alone identified the session), so everything is
/// optional — an absent token simply revokes nothing while the cookie
/// still gets cleared, and `redirect` decides where the browser lands
/// after the `303` (the site root by default).
#[derive(Deserialize, Default)]
struct FormLogout {
    refresh_token: Option<String>,
    redirect: Option<String>,
}

/// Verified login outcome shared by the JSON and form input shapes.
struct LoginSuccess {
    access_token: String,
    user: PublicUser,
    refresh_token: Option<String>,
    refresh_expires_in: Option<u64>,
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
/// that bootstrap lives in the CMS user management (`/admin/users`).
///
/// Two input shapes share the endpoint, dispatched on the
/// `Content-Type` (mirroring `login`):
///
/// - **JSON** — the API contract: a `201` with the public user.
/// - **Form** (`application/x-www-form-urlencoded`) — the browser path
///   used by the registration modal: on success the fresh account is
///   logged in directly (session cookie attached) and the browser gets
///   a `303` to the same-origin `redirect` field (default `/`);
///   failures bounce back with `?register_error=<code>#registrar` so
///   the modal re-opens with the message.
async fn register(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());
    let headers = request.headers().clone();
    let max_body = state.config().server.max_body_size_bytes;
    let is_form = content_type_is_form(&request);

    let (credentials, redirect) = if is_form {
        match read_form::<FormRegister>(request, max_body).await {
            Ok(form) => {
                let redirect = safe_redirect(form.redirect.as_deref()).to_owned();
                (
                    Credentials {
                        username: form.username,
                        password: form.password,
                    },
                    redirect,
                )
            }
            Err(message) => return form_error_response(StatusCode::BAD_REQUEST, &message),
        }
    } else {
        match <JsonBody<Credentials> as FromRequest<()>>::from_request(request, &()).await {
            Ok(JsonBody(credentials)) => (credentials, String::new()),
            Err(rejection) => return rejection.into_response(),
        }
    };

    let auth = match auth_context(&state) {
        Ok(auth) => auth,
        Err(error) => return register_failure(&state, error, is_form, &redirect, request_id),
    };

    if !auth.registration_enabled {
        let error = AppError::forbidden("registration is disabled on this server");
        return register_failure(&state, error, is_form, &redirect, request_id);
    }

    if let Err(error) = validate_username(&credentials.username) {
        return register_failure(
            &state,
            AppError::bad_request(error),
            is_form,
            &redirect,
            request_id,
        );
    }
    if let Err(error) = validate_password(&credentials.password, auth.min_password_len) {
        return register_failure(
            &state,
            AppError::bad_request(error),
            is_form,
            &redirect,
            request_id,
        );
    }

    let total = match auth.repository.count().await {
        Ok(total) => total,
        Err(error) => {
            return register_failure(
                &state,
                AppError::from(error),
                is_form,
                &redirect,
                request_id,
            )
        }
    };
    let role = if total == 0 {
        UserRole::Admin
    } else {
        UserRole::User
    };

    let password_hash = match hash_password(&credentials.password) {
        Ok(hash) => hash,
        Err(error) => {
            tracing::error!(%error, "password hashing failed");
            return register_failure(
                &state,
                AppError::internal("password hashing failed"),
                is_form,
                &redirect,
                request_id,
            );
        }
    };

    let user = match auth
        .repository
        .create(&credentials.username, &password_hash, role)
        .await
    {
        Ok(user) => user,
        Err(RepositoryError::Duplicate) => {
            let error = AppError::conflict("username is already taken");
            return register_failure(&state, error, is_form, &redirect, request_id);
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "user storage failure");
            let error = AppError::internal("storage failure");
            return register_failure(&state, error, is_form, &redirect, request_id);
        }
    };

    tracing::info!(
        user_id = user.id,
        role = user.role.as_str(),
        "user registered"
    );

    if !is_form {
        return (StatusCode::CREATED, Json(user.public())).into_response();
    }

    // Browser path: log the fresh account in straight away.
    match issue_login(&state, &peer, &headers, &credentials).await {
        Ok(success) => respond_login(&state, success, true, &redirect),
        Err(error) => {
            // The account exists but the login failed (storage hiccup,
            // signing failure): send the browser to the login page
            // instead of leaving it stranded on a redirect-less page.
            tracing::warn!(message = error.message(), "post-registration login failed");
            let redirect = redirect_with_error("/login", "login_error", "postregistro", "");
            see_other(&redirect)
        }
    }
}

/// Answers a failed registration: the JSON envelope for API clients,
/// a `303` back to the form's page with `?register_error=<code>` for
/// browsers.
fn register_failure(
    state: &AppState,
    error: AppError,
    is_form: bool,
    redirect: &str,
    request_id: Option<String>,
) -> Response {
    if !is_form {
        return error.into_response_with_request_id(request_id.as_deref());
    }

    let code = match error.status_code() {
        StatusCode::CONFLICT => "tomado",
        StatusCode::FORBIDDEN => "cerrado",
        StatusCode::BAD_REQUEST => {
            if error.message().contains("username") {
                "usuario"
            } else {
                "contrasena"
            }
        }
        _ => "error",
    };

    let secure = secure_cookies(state);
    let target = redirect_with_error(redirect, "register_error", code, "#registrar");
    redirect_response(&target, Some(session::clear_cookie_value(secure)))
}

/// `POST /api/auth/login`: exchanges credentials for an access token
/// (and a refresh token while enabled).
///
/// Two input shapes share the endpoint, dispatched on the
/// `Content-Type`:
///
/// - **JSON** — the API contract: a `LoginResponse` body, exactly as in
///   earlier phases (curl, SDKs, scripts).
/// - **Form** (`application/x-www-form-urlencoded`) — the browser path
///   used by the `/login` view: on success the answer is a
///   `303 See Other` to the same-origin `redirect` field (default `/`)
///   instead of JSON, and on failure a minimal HTML error page — a
///   plain HTML form needs no JavaScript, which the default CSP
///   (`default-src 'none'`) would block anyway.
///
/// Both attach the fresh access token as the `wallermax_session`
/// cookie (`HttpOnly`, `Secure`, `SameSite=Strict`, `Path=/`): browsers
/// then stay authenticated across normal navigation, and
/// `<?jhs if (user) ?>` pages personalise. Header-based clients can
/// keep ignoring the cookie entirely.
///
/// Unknown usernames and wrong passwords produce the **same** error (no
/// user enumeration); the unknown-username path also burns a comparable
/// Argon2 verification so response timing cannot distinguish the cases.
async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());
    let headers = request.headers().clone();
    let max_body = state.config().server.max_body_size_bytes;
    let is_form = content_type_is_form(&request);

    let (credentials, redirect) = if is_form {
        match read_form::<FormCredentials>(request, max_body).await {
            Ok(form) => {
                let redirect = safe_redirect(form.redirect.as_deref()).to_owned();
                (
                    Credentials {
                        username: form.username,
                        password: form.password,
                    },
                    redirect,
                )
            }
            Err(message) => return form_error_response(StatusCode::BAD_REQUEST, &message),
        }
    } else {
        match <JsonBody<Credentials> as FromRequest<()>>::from_request(request, &()).await {
            Ok(JsonBody(credentials)) => (credentials, String::new()),
            Err(rejection) => return rejection.into_response(),
        }
    };

    match issue_login(&state, &peer, &headers, &credentials).await {
        Ok(success) => respond_login(&state, success, is_form, &redirect),
        Err(error) => {
            if is_form {
                // Browsers bounce back to the page they came from with
                // `?login_error=<code>#login`, which re-opens the modal
                // and shows the message (every page carries the shared
                // header). Storage/internal failures keep the plain HTML
                // error page: they are not retryable from the form.
                if error.status_code() == StatusCode::UNAUTHORIZED {
                    let secure = secure_cookies(&state);
                    let target =
                        redirect_with_error(&redirect, "login_error", "credenciales", "#login");
                    redirect_response(&target, Some(session::clear_cookie_value(secure)))
                } else {
                    form_error_response(error.status_code(), error.message())
                }
            } else {
                error.into_response_with_request_id(request_id.as_deref())
            }
        }
    }
}

/// Login core shared by both input shapes: verify the credentials,
/// mint the access token (plus the refresh token while enabled) and
/// stamp the last-login timestamp.
async fn issue_login(
    state: &AppState,
    peer: &SocketAddr,
    headers: &HeaderMap,
    credentials: &Credentials,
) -> Result<LoginSuccess, AppError> {
    let auth = auth_context(state)?;

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
        let (value, _) = issue_refresh_token(state, auth, &user, None, peer, headers).await?;
        (Some(value), Some(auth.refresh_token_ttl_secs))
    } else {
        (None, None)
    };

    tracing::info!(user_id = user.id, "user logged in");

    Ok(LoginSuccess {
        access_token,
        user: user.public(),
        refresh_token,
        refresh_expires_in,
    })
}

/// Builds the login response: JSON for API clients, a `303` redirect
/// for browser forms — both carrying the session cookie.
fn respond_login(
    state: &AppState,
    success: LoginSuccess,
    is_form: bool,
    redirect: &str,
) -> Response {
    let ttl = state.config().auth.token_ttl_secs;
    let cookie = session::set_cookie_value(&success.access_token, ttl, secure_cookies(state));

    if is_form {
        return redirect_response(redirect, Some(cookie));
    }

    let mut response = Json(LoginResponse {
        access_token: success.access_token,
        token_type: "Bearer",
        expires_in: ttl,
        user: success.user,
        refresh_token: success.refresh_token,
        refresh_expires_in: success.refresh_expires_in,
    })
    .into_response();
    set_header(&mut response, header::SET_COOKIE, &cookie);
    response
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
) -> Result<Response, AppError> {
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

    // The browser session follows the rotation: a new cookie value for
    // the new access token (curl and friends can keep ignoring it).
    let ttl = state.config().auth.token_ttl_secs;
    let cookie = session::set_cookie_value(&access_token, ttl, secure_cookies(&state));

    let mut response = Json(LoginResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ttl,
        user: user.public(),
        refresh_token: Some(new_value),
        refresh_expires_in: Some(auth.refresh_token_ttl_secs),
    })
    .into_response();
    set_header(&mut response, header::SET_COOKIE, &cookie);
    Ok(response)
}

/// `POST /api/auth/logout`: revokes the family of the given refresh
/// token and clears the session cookie.
///
/// Two shapes, dispatched on the `Content-Type`:
///
/// - **JSON** (API clients) — requires a valid access token (Bearer
///   header or session cookie); the answer is always `204 No Content`
///   so the endpoint reveals nothing about token validity. An unknown,
///   foreign or already-revoked token is indistinguishable from
///   success.
/// - **Form** (browsers) — lenient and idempotent: the cookie is
///   cleared and the browser lands on a `303` to the (local) redirect
///   field, defaulting to the site root. An absent or expired session
///   clears the cookie without erroring — a `Salir` click must never
///   show a blank `204` page or a JSON envelope. Revocation is still
///   attempted through the optional `refresh_token` field, exactly
///   like the JSON path.
async fn logout(State(state): State<AppState>, request: Request) -> Response {
    let max_body = state.config().server.max_body_size_bytes;

    if content_type_is_form(&request) {
        let headers = request.headers().clone();
        let (refresh_token, redirect) = match read_form::<FormLogout>(request, max_body).await {
            Ok(form) => (
                form.refresh_token,
                safe_redirect(form.redirect.as_deref()).to_owned(),
            ),
            Err(message) => return form_error_response(StatusCode::BAD_REQUEST, &message),
        };

        // Lenient identity: whatever the cookie (or header) says, a
        // foreign or expired token simply revokes nothing.
        let token = bearer_token(&headers).or_else(|| session::session_token(&headers));
        if let (Some(auth), Some(token)) = (state.auth_context(), token) {
            if let Ok(claims) = auth.jwt.verify_token(token) {
                if let Ok(user) = AuthUser::from_claims(&claims) {
                    if let Err(error) =
                        revoke_logout_family(&state, &user, refresh_token.as_deref()).await
                    {
                        tracing::warn!(message = error.message(), "form logout revocation failed");
                    }
                }
            }
        }

        let secure = secure_cookies(&state);
        return redirect_response(&redirect, Some(session::clear_cookie_value(secure)));
    }

    // JSON path: the extractor's rejection envelope is preserved by
    // running it manually on the request parts.
    let (mut parts, body) = request.into_parts();
    let user = match AuthUser::from_request_parts(&mut parts, &state).await {
        Ok(user) => user,
        Err(rejection) => return rejection.into_response(),
    };
    let request = Request::from_parts(parts, body);

    let refresh_token =
        match <JsonBody<RefreshRequest> as FromRequest<()>>::from_request(request, &()).await {
            Ok(JsonBody(body)) => Some(body.refresh_token),
            Err(rejection) => return rejection.into_response(),
        };

    match revoke_logout_family(&state, &user, refresh_token.as_deref()).await {
        Ok(()) => cleared_session_response(&state),
        Err(error) => error.into_response(),
    }
}

/// Logout core: revoke the family of the given refresh token when it
/// belongs to the caller. Foreign or unknown tokens are silently
/// ignored (the answer stays 204 regardless).
async fn revoke_logout_family(
    state: &AppState,
    user: &AuthUser,
    refresh_token: Option<&str>,
) -> Result<(), AppError> {
    let auth = auth_context(state)?;
    let now = unix_now();

    if let Some(value) = refresh_token.and_then(sanitize_refresh_token_opt) {
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

    Ok(())
}

/// `POST /api/auth/logout_all`: revokes every refresh token of the
/// caller, ending all sessions, and clears the session cookie.
async fn logout_all(State(state): State<AppState>, user: AuthUser) -> Result<Response, AppError> {
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
    Ok(cleared_session_response(&state))
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

/// Whether the request body is a browser form post.
fn content_type_is_form(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
}

/// Where form logins land after the `303`: the posted `redirect` field
/// when it is a local path, `/` otherwise.
///
/// Only same-origin paths pass (see [`is_local_redirect`]) so a hostile
/// `redirect` field cannot turn the login form into an open redirect.
fn safe_redirect(target: Option<&str>) -> &str {
    target.filter(|path| is_local_redirect(path)).unwrap_or("/")
}

/// A redirect target is local when it is site-rooted and cannot escape
/// the origin: it starts with a single `/` (never the protocol-relative
/// `//host`), and carries neither backslashes (Windows-separator
/// tricks) nor control characters (header splitting).
fn is_local_redirect(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
}

/// Minimal HTML error page for form submissions (browsers; the API
/// keeps answering the JSON envelope). Deliberately script-free so the
/// default CSP (`default-src 'none'`) allows it untouched.
fn form_error_response(status: StatusCode, message: &str) -> Response {
    let html = format!(
        "<!DOCTYPE html>\n<html lang=\"es\">\n<head>\n<meta charset=\"utf-8\">\n<title>{title}</title>\n</head>\n<body>\n<h1>{title}</h1>\n<p>{message}</p>\n<p><a href=\"/login\">Volver a intentarlo</a></p>\n</body>\n</html>\n",
        title = "No se pudo iniciar sesión",
        message = escape_html(message),
    );

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .expect("valid error page")
}

/// HTML-escapes a text so server messages stay inert inside the error
/// page.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

/// A `204` response that expires the `wallermax_session` cookie.
fn cleared_session_response(state: &AppState) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("valid response");
    set_header(
        &mut response,
        header::SET_COOKIE,
        &session::clear_cookie_value(secure_cookies(state)),
    );
    response
}

/// Resolves the session cookie's `Secure` flag from `[auth]
/// secure_cookies` and the TLS state (see [`SecureCookieMode`]).
fn secure_cookies(state: &AppState) -> bool {
    state
        .config()
        .auth
        .secure_cookies
        .resolve(state.config().tls.enabled)
}

/// Builds `{redirect}{?|&}{param}={code}{fragment}`.
///
/// The separator picks `?` or `&` so the parameter composes with targets
/// that already carry a query string; the (optional) fragment re-opens
/// the modal on the page the browser came from.
fn redirect_with_error(redirect: &str, param: &str, code: &str, fragment: &str) -> String {
    let separator = if redirect.contains('?') { '&' } else { '?' };
    format!("{redirect}{separator}{param}={code}{fragment}")
}

/// A `303 See Other` with an optional `Set-Cookie`.
fn redirect_response(location: &str, cookie: Option<String>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location);
    if let Some(cookie) = cookie {
        builder = builder.header(header::SET_COOKIE, cookie);
    }
    builder
        .body(Body::empty())
        .expect("valid redirect response")
}

/// A bare `303 See Other`.
fn see_other(location: &str) -> Response {
    redirect_response(location, None)
}

/// Inserts an ASCII header value, logging instead of failing on the
/// (impossible for these server-built values) rejection.
fn set_header(response: &mut Response, name: header::HeaderName, value: &str) {
    match HeaderValue::from_str(value) {
        Ok(value) => {
            response.headers_mut().insert(name, value);
        }
        Err(error) => {
            tracing::error!(%error, "invalid session cookie header value");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_redirects_pass_validation() {
        for path in ["/", "/perfil", "/hello.jhs", "/blog/post?x=1", "/a%20b"] {
            assert!(is_local_redirect(path), "{path} should be a local redirect");
        }
    }

    #[test]
    fn off_site_redirect_targets_are_rejected() {
        for path in [
            "",
            "perfil",
            "https://evil.example",
            "http://evil.example",
            "//evil.example",
            "/\\evil.example",
            "/a\rb",
        ] {
            assert!(!is_local_redirect(path), "{path:?} should be rejected");
        }
    }

    #[test]
    fn unsafe_redirect_targets_fall_back_to_the_site_root() {
        assert_eq!(safe_redirect(Some("/perfil")), "/perfil");
        assert_eq!(safe_redirect(Some("https://evil.example")), "/");
        assert_eq!(safe_redirect(Some("//evil.example")), "/");
        assert_eq!(safe_redirect(None), "/");
    }

    #[test]
    fn error_redirects_compose_query_and_fragment() {
        assert_eq!(
            redirect_with_error("/", "login_error", "credenciales", "#login"),
            "/?login_error=credenciales#login"
        );
        assert_eq!(
            redirect_with_error("/p/x?a=b", "register_error", "tomado", ""),
            "/p/x?a=b&register_error=tomado"
        );
    }

    #[test]
    fn html_escaping_neutralizes_markup() {
        assert_eq!(
            escape_html("<script>alert('x')</script>&"),
            "&lt;script&gt;alert(&#039;x&#039;)&lt;/script&gt;&amp;"
        );
    }
}
