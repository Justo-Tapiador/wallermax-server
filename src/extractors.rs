//! Request extractors for authenticated identities and JSON bodies.
//!
//! Handlers declare their requirements as argument types and axum runs the
//! corresponding extractor before the handler body:
//!
//! - [`AuthUser`] — any request carrying a valid `Authorization: Bearer`
//!   token **or session cookie** (`wallermax_session`); yields the id,
//!   username and role.
//! - [`AdminUser`] — like [`AuthUser`], additionally requiring the `admin`
//!   role (403 otherwise).
//! - [`JsonBody`] — a JSON request body decoded with the application's
//!   JSON error envelope instead of axum's plain-text rejections.
//!
//! All rejections render the structured
//! `{ "error": { code, message, request_id } }` envelope. Extractor
//! failures happen before handlers run, so the [`Rejection`] type captures
//! the correlation id from the request extensions — the same convention
//! the middleware and the JSON fallbacks follow.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::header;
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use crate::auth::Claims;
use crate::db::UserRole;
use crate::error::AppError;
use crate::middleware::request_id::RequestId;
use crate::session;
use crate::state::AppState;

/// Extractor rejection that embeds the request's correlation id.
#[derive(Debug)]
pub struct Rejection {
    error: AppError,
    request_id: Option<String>,
}

impl Rejection {
    /// Captures the correlation id (when present) alongside the error.
    fn new(error: AppError, extensions: &Extensions) -> Self {
        let request_id = extensions.get::<RequestId>().map(|id| id.0.clone());
        Self { error, request_id }
    }

    /// The wrapped error (cms browser guards read the status code to
    /// decide between a login redirect and a privilege page).
    pub(crate) fn error(&self) -> &AppError {
        &self.error
    }
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        self.error
            .into_response_with_request_id(self.request_id.as_deref())
    }
}

/// The authenticated identity extracted from a valid Bearer token.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// Id of the authenticated user (the token `sub` claim).
    pub user_id: i64,
    /// Username carried by the token.
    pub username: String,
    /// Role granted by the token.
    pub role: UserRole,
}

impl AuthUser {
    /// Rebuilds the identity from verified claims.
    ///
    /// Crate-visible so the template middleware can turn a verified
    /// Bearer token into template data without duplicating the parsing
    /// rules (and their failure handling) here.
    pub(crate) fn from_claims(claims: &Claims) -> Result<Self, AppError> {
        let user_id = claims
            .sub
            .parse()
            .map_err(|_| AppError::unauthorized("invalid token subject"))?;
        let role = UserRole::parse(&claims.role)
            .ok_or_else(|| AppError::unauthorized("invalid token role"))?;

        Ok(Self {
            user_id,
            username: claims.username.clone(),
            role,
        })
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = Rejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(auth) = state.auth_context() else {
            return Err(Rejection::new(
                AppError::unauthorized("authentication is not enabled"),
                &parts.extensions,
            ));
        };

        // The Bearer header, when present, always wins; the session
        // cookie is the browser-side fallback.
        let Some(token) =
            bearer_token(&parts.headers).or_else(|| session::session_token(&parts.headers))
        else {
            return Err(Rejection::new(
                AppError::unauthorized(
                    "missing `Authorization: Bearer <token>` header or session cookie",
                ),
                &parts.extensions,
            ));
        };

        // The exact verification failure is irrelevant to the client.
        let claims = match auth.jwt.verify_token(token) {
            Ok(claims) => claims,
            Err(_) => {
                return Err(Rejection::new(
                    AppError::unauthorized("invalid or expired token"),
                    &parts.extensions,
                ));
            }
        };

        AuthUser::from_claims(&claims).map_err(|error| Rejection::new(error, &parts.extensions))
    }
}

/// An [`AuthUser`] additionally verified to hold the `admin` role.
#[derive(Debug)]
pub struct AdminUser {
    /// The underlying authenticated identity.
    pub user: AuthUser,
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = Rejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if user.role != UserRole::Admin {
            return Err(Rejection::new(
                AppError::forbidden("this endpoint requires the admin role"),
                &parts.extensions,
            ));
        }
        Ok(Self { user })
    }
}

/// JSON body extractor producing the application error envelope on
/// rejections (malformed JSON, missing content type) instead of axum's
/// plain-text defaults.
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Rejection;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        // The request is consumed by the inner extractor; grab the
        // correlation id first.
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .map(|id| id.0.clone());

        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(JsonBody(value)),
            Err(rejection) => Err(Rejection {
                error: AppError::bad_request(rejection.body_text()),
                request_id,
            }),
        }
    }
}

/// Extracts the raw token from an `Authorization: Bearer <token>` header.
///
/// Crate-visible so middleware that optionally authenticates (the
/// template renderer) shares the exact same parsing rules as the
/// request extractors.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
