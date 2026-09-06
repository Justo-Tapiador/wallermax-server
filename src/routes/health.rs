//! Liveness probe (`GET /health`).

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

/// Health check payload.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

/// `GET /health`: lightweight liveness probe; always fast and
/// dependency-free so it can back container orchestration checks.
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Route fragment for this module.
pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}
