//! Admin-only endpoints (require the `admin` role).
//!
//! - `GET /api/admin/users` — list registered accounts.
//!
//! Mounting follows authentication: the family is only present while
//! `[auth]` is enabled. The [`AdminUser`] extractor enforces the role and
//! renders 401/403 JSON envelopes for missing tokens or insufficient
//! privileges.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::db::{PublicUser, User};
use crate::error::AppError;
use crate::extractors::AdminUser;
use crate::state::AppState;

/// Maximum number of accounts returned by the listing.
///
/// A deliberate cap rather than full pagination; real paging is roadmap
/// material for the next phase.
const MAX_LISTED_USERS: i64 = 100;

/// User listing payload.
#[derive(Serialize)]
struct UsersResponse {
    users: Vec<PublicUser>,
    /// Total registered accounts, independent of the listing cap.
    total: i64,
}

/// `GET /api/admin/users`: lists accounts, newest first.
async fn list_users(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<UsersResponse>, AppError> {
    let auth = state
        .auth_context()
        .ok_or_else(|| AppError::internal("authentication is not initialized"))?;

    let users = auth
        .repository
        .list(MAX_LISTED_USERS)
        .await
        .map_err(AppError::from)?;
    let total = auth.repository.count().await.map_err(AppError::from)?;

    let users = users.iter().map(User::public).collect();

    Ok(Json(UsersResponse { users, total }))
}

/// Route fragment for this module.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/admin/users", get(list_users))
}
