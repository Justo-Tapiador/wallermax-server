//! Request body size limiting.
//!
//! Two cooperating pieces enforce `server.max_body_size_bytes`:
//!
//! 1. This middleware (`from_fn`): an **early check** of the
//!    `Content-Length` header that rejects oversized bodies before any
//!    processing, plus a **normalization** pass that rewrites any inner
//!    plain-text 413 (produced by the tower-http layer) into the standard
//!    JSON error envelope.
//! 2. tower-http's `RequestBodyLimitLayer` (applied in
//!    [`crate::middleware::apply`] closer to the routes): stream-level
//!    enforcement that also covers chunked bodies without a known length.
//!
//! Exceeding the limit yields `413 Payload Too Large` with the JSON error
//! envelope (see [`crate::error::AppError`]).

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

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
    let max = state.config().server.max_body_size_bytes;
    let request_id = request_id_of(&request);

    // Early rejection when the declared length already exceeds the limit.
    if let Some(declared) = declared_content_length(&request) {
        if declared > max as u64 {
            tracing::warn!(
                declared_bytes = declared,
                limit_bytes = max,
                request_id = request_id.as_deref().unwrap_or("-"),
                "request rejected: body exceeds the configured limit"
            );
            return AppError::payload_too_large(declared, max)
                .into_response_with_request_id(request_id.as_deref());
        }
    }

    let response = next.run(request).await;

    // Normalize inner 413 responses (tower-http's plain-text rejection)
    // into the JSON error envelope.
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        let is_json = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"));

        if !is_json {
            return AppError::payload_too_large_unknown_size(max)
                .into_response_with_request_id(request_id.as_deref());
        }
    }

    response
}

/// Returns the declared `Content-Length`, when present and parseable.
fn declared_content_length(request: &Request) -> Option<u64> {
    request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_request(content_length: Option<&str>) -> Request {
        let mut builder = Request::builder();
        if let Some(length) = content_length {
            builder = builder.header(header::CONTENT_LENGTH, length);
        }
        builder
            .body(axum::body::Body::empty())
            .expect("request builds")
    }

    #[test]
    fn parses_declared_content_length() {
        let request = build_request(Some("2048"));

        assert_eq!(declared_content_length(&request), Some(2048));
    }

    #[test]
    fn missing_content_length_returns_none() {
        let request = build_request(None);

        assert_eq!(declared_content_length(&request), None);
    }
}
