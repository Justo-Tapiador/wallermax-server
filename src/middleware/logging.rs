//! Structured request logging.
//!
//! Emits one `INFO` record per request with the method, path, status code,
//! latency, the resolved client IP and the correlation id (see
//! [`super::request_id`]), and exposes the measured latency in the
//! `X-Response-Time` response header (milliseconds).
//!
//! This middleware also maintains the request counter surfaced by
//! `GET /api/stats` and feeds the Prometheus counters
//! (`wallermax_requests_total`, `wallermax_request_duration_seconds`),
//! so both are only collected while `[middleware] logging = true`.

use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

use crate::middleware::request_id::RequestId;
use crate::proxy;
use crate::state::AppState;
use crate::util::round3;

/// Name of the response latency header, in milliseconds.
const RESPONSE_TIME_HEADER: HeaderName = HeaderName::from_static("x-response-time");

/// Middleware entry point (see [`crate::middleware`] for ordering).
pub async fn run(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let target = request
        .uri()
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());

    // Client IP honouring trusted proxies (peer address by default).
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip())
        .map(|peer| {
            let xff = proxy::forwarded_for_values(request.headers());
            proxy::resolve_client_ip(peer, &xff, state.trusted_proxies())
        });

    let started = Instant::now();

    state.record_request();
    let response = next.run(request).await;

    let elapsed = started.elapsed();
    let latency_ms = elapsed.as_secs_f64() * 1000.0;
    let status = response.status().as_u16();

    tracing::info!(
        method = method.as_str(),
        target = %target,
        status = status,
        latency_ms = round3(latency_ms),
        client_ip = client_ip
            .map(|ip| ip.to_string())
            .as_deref()
            .unwrap_or("-"),
        request_id = request_id.as_deref().unwrap_or("-"),
        "request completed"
    );

    // Feed the Prometheus families (method label values are HTTP
    // tokens, always valid).
    if let Some(metrics) = state.metrics() {
        metrics.record_request(method.as_str(), status, elapsed.as_secs_f64());
    }

    let mut response = response;

    // Expose the latency as a response header (milliseconds, 3 decimals).
    if let Ok(value) = HeaderValue::from_str(&format!("{latency_ms:.3}")) {
        response.headers_mut().insert(RESPONSE_TIME_HEADER, value);
    }

    response
}
