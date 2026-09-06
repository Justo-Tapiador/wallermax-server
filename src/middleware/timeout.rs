//! Per-request timeout middleware.
//!
//! Bounds the total handling time of a request to
//! `server.request_timeout_secs`. When the budget is exhausted the inner
//! service future is dropped and the client receives a `408 Request
//! Timeout` with the standard JSON error envelope and the correlation id.
//!
//! Implemented directly on top of [`tokio::time::timeout`] so the rejection
//! integrates with the application's error model (a plain tower layer would
//! produce a non-JSON body).

use std::time::Duration;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tokio::time::timeout;

use crate::error::AppError;
use crate::middleware::request_id::RequestId;
use crate::state::AppState;

/// Extracts the request id as a plain string, when present.
fn request_id_of(request: &Request) -> Option<String> {
    request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone())
}

/// Middleware entry point (see [`crate::middleware`] for ordering).
pub async fn run(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let budget = Duration::from_secs(state.config().server.request_timeout_secs);
    let request_id = request_id_of(&request);

    match timeout(budget, next.run(request)).await {
        Ok(response) => response,
        Err(_elapsed) => {
            tracing::warn!(
                budget_secs = budget.as_secs(),
                request_id = request_id.as_deref().unwrap_or("-"),
                "request timed out"
            );
            AppError::request_timeout(budget).into_response_with_request_id(request_id.as_deref())
        }
    }
}
