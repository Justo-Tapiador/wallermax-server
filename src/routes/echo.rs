//! Request echo (`POST /api/echo`).
//!
//! A small utility endpoint for debugging and integration testing: it
//! reads the request body (bounded by `server.max_body_size_bytes`) and
//! reports what was received. It is also the natural endpoint for testing
//! the body-limit middleware.

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::header;
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use crate::error::AppError;
use crate::state::AppState;

/// Echo response payload.
#[derive(Serialize)]
struct EchoResponse {
    received_bytes: usize,
    content_type: Option<String>,
    body: String,
}

/// `POST /api/echo`: reads and describes the request body.
async fn echo(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<EchoResponse>, AppError> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // Defense in depth: the body-limit middleware normally rejects
    // oversized bodies first; this bound still holds when the middleware
    // is disabled or a chunked stream lies about its length.
    let max = state.config().server.max_body_size_bytes;
    let bytes = to_bytes(request.into_body(), max)
        .await
        .map_err(|_| AppError::payload_too_large_unknown_size(max))?;

    Ok(Json(EchoResponse {
        received_bytes: bytes.len(),
        content_type,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }))
}

/// Route fragment for this module.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/echo", post(echo))
}
