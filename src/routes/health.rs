//! Liveness probe (`GET /health`).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

/// Health check payload.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    /// The live `.jhs` rendering backend (v0.10.1): `"boa"` or
    /// `"sidecar"` (`"auto"` resolves to whichever half is currently
    /// serving), `null` while `[templates]` is disabled. Metadata, not
    /// a health verdict: the probe itself stays liveness-shaped and
    /// dependency-free.
    template_backend: Option<&'static str>,
}

/// `GET /health`: lightweight liveness probe; always fast and
/// dependency-free so it can back container orchestration checks. The
/// `template_backend` field is a pure in-memory read — no I/O, no
/// sidecar round trip.
async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        template_backend: state.template_backend(),
    })
}

/// Route fragment for this module.
pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}
