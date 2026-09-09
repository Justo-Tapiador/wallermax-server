//! Prometheus exposition endpoint (`GET /metrics` by default).
//!
//! Renders every metric family gathered in [`crate::metrics::Metrics`]
//! in the Prometheus text exposition format. The route is only mounted
//! while `[metrics] enabled = true`, so disabled servers answer the
//! path with the standard JSON 404 envelope.
//!
//! The endpoint is intentionally unauthenticated (scrapers usually live
//! on an internal network) but still flows through the middleware
//! pipeline: timeouts, security headers and logging all apply. Scrapes
//! are exempt from rate limiting so a throttled server can always be
//! observed (see `crate::middleware::rate_limit`). Restrict access at
//! the network layer (bind address, firewall or reverse proxy rules)
//! when the server is exposed.

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::error::AppError;
use crate::state::AppState;
use crate::util::round3;

/// `GET /metrics` (or the configured path): renders the exposition.
async fn metrics(State(state): State<AppState>) -> Result<Response, AppError> {
    let Some(metrics) = state.metrics() else {
        // Enabled in configuration but the registry failed to build at
        // startup (already logged); fail loudly for scrapers.
        return Err(AppError::internal("metrics are unavailable"));
    };

    // Refresh gauges at scrape time, mirroring `GET /api/stats`.
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

    let body = metrics
        .render(
            round3(state.uptime().as_secs_f64()),
            registered_users,
            state.template_backend(),
        )
        .map_err(|message| {
            tracing::error!(%message, "metrics encoding failed");
            AppError::internal("metrics are unavailable")
        })?;

    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(crate::metrics::CONTENT_TYPE),
    );
    Ok(response)
}

/// Route fragment for this module, mounted at the configured path.
pub fn routes(path: &str) -> Router<AppState> {
    Router::new().route(path, get(metrics))
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_type_constant_is_exposed() {
        // The constant backs the `CONTENT_TYPE` name used above.
        assert_eq!(crate::metrics::CONTENT_TYPE, "text/plain; version=0.0.4");
    }
}
