//! Application-wide error model.
//!
//! Every error produced by handlers and middleware is an [`AppError`],
//! converted into a consistent JSON error response:
//!
//! ```json
//! {
//!   "error": {
//!     "code": "NOT_FOUND",
//!     "message": "No route matches GET /nope",
//!     "request_id": "6f2c1e0a-9b0d-4d7a-a1c2-0123456789ab"
//!   }
//! }
//! ```
//!
//! The `request_id` field (when present) matches the `X-Request-Id`
//! response header so clients and logs can be correlated.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use std::time::Duration;

/// Errors that can be turned into an HTTP response.
#[derive(Debug)]
pub enum AppError {
    /// The requested resource does not exist (404).
    NotFound {
        /// Human-readable description of what was not found.
        message: String,
    },
    /// The request is malformed or invalid (400).
    BadRequest {
        /// Explanation of the invalid input.
        message: String,
    },
    /// The client has exceeded its rate limit (429).
    RateLimited {
        /// Explanation shown to the client.
        message: String,
        /// Seconds the client should wait before retrying
        /// (sent as the `Retry-After` header).
        retry_after_secs: u64,
    },
    /// The request body exceeds the configured limit (413).
    PayloadTooLarge {
        /// Explanation shown to the client.
        message: String,
    },
    /// The request took too long to process (408).
    RequestTimeout {
        /// Explanation shown to the client.
        message: String,
    },
    /// The route exists but not for this HTTP method (405).
    MethodNotAllowed {
        /// Explanation shown to the client.
        message: String,
    },
    /// The request lacks valid authentication credentials (401).
    Unauthorized {
        /// Explanation shown to the client (deliberately generic where
        /// details would leak information, e.g. on login).
        message: String,
    },
    /// Authenticated, but not allowed to perform the action (403).
    Forbidden {
        /// Explanation shown to the client.
        message: String,
    },
    /// The request conflicts with existing state, e.g. a duplicate
    /// username (409).
    Conflict {
        /// Explanation shown to the client.
        message: String,
    },
    /// An unexpected internal failure (500).
    Internal {
        /// Generic explanation that is safe to expose to the client.
        message: String,
    },
}

impl AppError {
    /// Builds a 404 error for an unmatched route.
    pub fn not_found(method: &str, path: &str) -> Self {
        Self::NotFound {
            message: format!("No route matches {method} {path}"),
        }
    }

    /// Builds a 400 error with the given explanation.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest {
            message: message.into(),
        }
    }

    /// Builds a 429 error with a `Retry-After` hint.
    pub fn rate_limited(retry_after_secs: u64) -> Self {
        Self::RateLimited {
            message: format!("Rate limit exceeded; retry after {retry_after_secs} second(s)"),
            retry_after_secs,
        }
    }

    /// Builds a 413 error for a known body size.
    pub fn payload_too_large(body_size: u64, limit: usize) -> Self {
        Self::PayloadTooLarge {
            message: format!("Request body of {body_size} bytes exceeds the {limit} byte limit"),
        }
    }

    /// Builds a 413 error for a stream of unknown size.
    pub fn payload_too_large_unknown_size(limit: usize) -> Self {
        Self::PayloadTooLarge {
            message: format!("Request body exceeds the {limit} byte limit"),
        }
    }

    /// Builds a 408 error for a request that exceeded the time budget.
    pub fn request_timeout(timeout: Duration) -> Self {
        Self::RequestTimeout {
            message: format!(
                "Request processing exceeded the {} second limit",
                timeout.as_secs()
            ),
        }
    }

    /// Builds a 405 error for a supported path with an unsupported method.
    pub fn method_not_allowed(method: &str, path: &str) -> Self {
        Self::MethodNotAllowed {
            message: format!("Method {method} is not allowed on {path}"),
        }
    }

    /// Builds a 401 error with the given explanation.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::Unauthorized {
            message: message.into(),
        }
    }

    /// Builds a 403 error with the given explanation.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Forbidden {
            message: message.into(),
        }
    }

    /// Builds a 409 error with the given explanation.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict {
            message: message.into(),
        }
    }

    /// Builds a 500 error with a safe, generic explanation.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// Converts the error into a response, attaching the request id
    /// (when known) to the body to ease log correlation.
    pub fn into_response_with_request_id(self, request_id: Option<&str>) -> Response {
        let (status, code, retry_after_secs) = match &self {
            AppError::NotFound { .. } => (StatusCode::NOT_FOUND, "NOT_FOUND", None),
            AppError::BadRequest { .. } => (StatusCode::BAD_REQUEST, "BAD_REQUEST", None),
            AppError::RateLimited {
                retry_after_secs, ..
            } => (
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                Some(*retry_after_secs),
            ),
            AppError::PayloadTooLarge { .. } => {
                (StatusCode::PAYLOAD_TOO_LARGE, "PAYLOAD_TOO_LARGE", None)
            }
            AppError::RequestTimeout { .. } => {
                (StatusCode::REQUEST_TIMEOUT, "REQUEST_TIMEOUT", None)
            }
            AppError::MethodNotAllowed { .. } => {
                (StatusCode::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED", None)
            }
            AppError::Unauthorized { .. } => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", None),
            AppError::Forbidden { .. } => (StatusCode::FORBIDDEN, "FORBIDDEN", None),
            AppError::Conflict { .. } => (StatusCode::CONFLICT, "CONFLICT", None),
            AppError::Internal { .. } => {
                (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", None)
            }
        };
        let message = match self {
            AppError::NotFound { message }
            | AppError::BadRequest { message }
            | AppError::RateLimited { message, .. }
            | AppError::PayloadTooLarge { message }
            | AppError::RequestTimeout { message }
            | AppError::MethodNotAllowed { message }
            | AppError::Unauthorized { message }
            | AppError::Forbidden { message }
            | AppError::Conflict { message }
            | AppError::Internal { message } => message,
        };

        let mut response = (
            status,
            Json(ErrorEnvelope {
                error: ErrorDetails {
                    code,
                    message: &message,
                    request_id,
                },
            }),
        )
            .into_response();

        if let Some(retry_after_secs) = retry_after_secs {
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }

        response
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        self.into_response_with_request_id(None)
    }
}

/// Top-level error envelope.
#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorDetails<'a>,
}

/// Error details payload.
#[derive(Serialize)]
struct ErrorDetails<'a> {
    code: &'a str,
    message: &'a str,
    request_id: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        serde_json::from_slice(&bytes).expect("body is valid JSON")
    }

    #[tokio::test]
    async fn not_found_renders_structured_json_404() {
        let response =
            AppError::not_found("GET", "/nope").into_response_with_request_id(Some("req-17"));

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get("retry-after").is_none());

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "NOT_FOUND");
        assert_eq!(json["error"]["request_id"], "req-17");
        assert!(json["error"]["message"]
            .as_str()
            .expect("message is a string")
            .contains("/nope"));
    }

    #[tokio::test]
    async fn request_id_is_omitted_when_unknown() {
        let response = AppError::bad_request("bad input").into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "BAD_REQUEST");
        assert!(json["error"]["request_id"].is_null());
    }

    #[tokio::test]
    async fn rate_limited_sets_retry_after() {
        let response = AppError::rate_limited(7).into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("7")
        );

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "RATE_LIMITED");
        assert!(json["error"]["message"]
            .as_str()
            .expect("message is a string")
            .contains("7 second"));
    }

    #[tokio::test]
    async fn payload_too_large_renders_json_413() {
        let response = AppError::payload_too_large(2048, 1024).into_response();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "PAYLOAD_TOO_LARGE");
        assert!(json["error"]["message"]
            .as_str()
            .expect("message is a string")
            .contains("2048"));
    }

    #[tokio::test]
    async fn request_timeout_renders_json_408() {
        let response = AppError::request_timeout(Duration::from_secs(15)).into_response();

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "REQUEST_TIMEOUT");
    }

    #[tokio::test]
    async fn method_not_allowed_renders_json_405() {
        let response = AppError::method_not_allowed("POST", "/health").into_response();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "METHOD_NOT_ALLOWED");
        let message = json["error"]["message"]
            .as_str()
            .expect("message is a string");
        assert!(message.contains("POST"));
        assert!(message.contains("/health"));
    }

    #[tokio::test]
    async fn unauthorized_renders_json_401() {
        let response = AppError::unauthorized("invalid or expired token").into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
        assert_eq!(json["error"]["message"], "invalid or expired token");
    }

    #[tokio::test]
    async fn forbidden_renders_json_403() {
        let response = AppError::forbidden("admin role required").into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "FORBIDDEN");
    }

    #[tokio::test]
    async fn conflict_renders_json_409() {
        let response = AppError::conflict("username is already taken").into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "CONFLICT");
        assert_eq!(json["error"]["message"], "username is already taken");
    }
}
