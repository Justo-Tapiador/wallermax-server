//! Per-client-IP rate limiting middleware.
//!
//! Backed by the token-bucket [`RateLimiter`](crate::rate_limit::RateLimiter)
//! shared through the application state. Allowed responses carry
//! `X-RateLimit-Limit` and `X-RateLimit-Remaining`; rejected requests get a
//! `429 Too Many Requests` with the standard JSON error envelope, a
//! `Retry-After` header and the correlation id.
//!
//! Scrapes of the Prometheus exposition endpoint (the `[metrics] path`,
//! while metrics are enabled) are exempt from the limiter: monitoring
//! must keep working while clients are being throttled, so a saturated
//! application can always be observed.
//!
//! The client IP honours `server.trusted_proxies`: while the list is empty
//! (the default) it is the TCP peer address; behind a trusted reverse
//! proxy it is recovered from `X-Forwarded-For` (see
//! [`crate::proxy`]). Untrusted peers cannot influence their bucket
//! identity with a spoofed header.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::AppError;
use crate::middleware::request_id::RequestId;
use crate::proxy;
use crate::rate_limit::Decision;
use crate::state::AppState;

/// Extracts the request id as a plain string, when present.
fn request_id_of(request: &Request) -> Option<String> {
    request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone())
}

/// Name of the rate limit capacity response header.
const RATE_LIMIT_LIMIT_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-limit");

/// Name of the rate limit remaining tokens response header.
const RATE_LIMIT_REMAINING_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-remaining");

/// Address used when the TCP peer address is unavailable.
fn unknown_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

/// Extracts the client IP from the connection info in the request extensions.
fn client_ip(request: &Request) -> IpAddr {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
        .unwrap_or_else(unknown_ip)
}

/// Formats a token count as a header value.
fn remaining_value(remaining: f64) -> HeaderValue {
    // Floor the token count: a remaining budget of 2.7 means two full
    // requests are guaranteed.
    HeaderValue::from_str(&format!("{}", remaining.floor() as u64))
        .expect("token counts are valid header values")
}

/// Middleware entry point (see [`crate::middleware`] for ordering).
pub async fn run(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let peer = client_ip(&request);
    let xff = proxy::forwarded_for_values(request.headers());
    let ip = proxy::resolve_client_ip(peer, &xff, state.trusted_proxies());
    let request_id = request_id_of(&request);
    let capacity = state.rate_limiter().capacity();

    // Prometheus scrapes are exempt: the exposition must stay reachable
    // while clients are throttled (the path only matches a real route
    // while metrics are enabled; otherwise this is a normal 404 path and
    // remains subject to the limiter).
    if state.metrics().is_some() && state.config().metrics.path == request.uri().path() {
        return next.run(request).await;
    }

    match state.rate_limiter().try_acquire(ip) {
        Decision::Allowed { remaining } => {
            let mut response = next.run(request).await;

            let headers = response.headers_mut();
            if let Ok(value) = HeaderValue::from_str(&capacity.to_string()) {
                headers.insert(RATE_LIMIT_LIMIT_HEADER, value);
            }
            headers.insert(RATE_LIMIT_REMAINING_HEADER, remaining_value(remaining));

            response
        }
        Decision::Rejected { retry_after_secs } => {
            state.record_rate_limited();
            tracing::warn!(
                client_ip = %ip,
                retry_after_secs,
                request_id = request_id.as_deref().unwrap_or("-"),
                "request rejected by rate limiter"
            );

            let mut response = AppError::rate_limited(retry_after_secs)
                .into_response_with_request_id(request_id.as_deref());

            let headers = response.headers_mut();
            if let Ok(value) = HeaderValue::from_str(&capacity.to_string()) {
                headers.insert(RATE_LIMIT_LIMIT_HEADER, value);
            }
            headers.insert(RATE_LIMIT_REMAINING_HEADER, remaining_value(0.0_f64));

            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_value_floors_tokens() {
        assert_eq!(remaining_value(2.7).to_str().expect("ascii"), "2");
        assert_eq!(remaining_value(0.9).to_str().expect("ascii"), "0");
    }
}
