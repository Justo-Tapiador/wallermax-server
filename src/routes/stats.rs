//! Runtime metrics (`GET /api/stats`).

use std::time::Duration;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;
use crate::util::round3;

/// Runtime statistics payload.
#[derive(Serialize)]
struct StatsResponse {
    service: &'static str,
    version: &'static str,
    uptime_seconds: f64,
    total_requests: u64,
    requests_per_second: f64,
    rate_limited_requests: u64,
    /// Registered accounts; present only while authentication is enabled
    /// (absent from the JSON otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    registered_users: Option<i64>,
}

/// `GET /api/stats`: lightweight runtime metrics (uptime, request and
/// rejection counters, and the registered-user count when the auth feature
/// is on).
///
/// `total_requests` counts every arrival seen by the logging middleware,
/// including requests later rejected by the rate limiter, which are
/// additionally counted in `rate_limited_requests`.
async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let uptime = state.uptime();
    let total_requests = state.total_requests();

    // A repository hiccup must not break the stats endpoint; the metric is
    // simply omitted for that call.
    let registered_users = match state.auth_context() {
        Some(auth) => match auth.repository.count().await {
            Ok(count) => Some(count),
            Err(error) => {
                tracing::warn!(%error, "failed to count registered users");
                None
            }
        },
        None => None,
    };

    Json(StatsResponse {
        service: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: round3(uptime.as_secs_f64()),
        total_requests,
        requests_per_second: per_second(total_requests, uptime),
        rate_limited_requests: state.rate_limited_requests(),
        registered_users,
    })
}

/// Computes a requests-per-second rate, guarded against division by zero.
fn per_second(total: u64, uptime: Duration) -> f64 {
    let seconds = uptime.as_secs_f64();
    if seconds > 0.0 {
        round3(total as f64 / seconds)
    } else {
        0.0
    }
}

/// Route fragment for this module.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/stats", get(stats))
}
