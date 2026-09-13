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
    /// An upstream service failed or answered unusable data while being
    /// proxied through the `[external_api]` family (502). Messages stay
    /// generic on purpose: upstream URLs, header values and transport
    /// error details are logged server-side, never shipped to clients.
    BadGateway {
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

    /// Builds a 404 error with a custom explanation — for resources
    /// that match a route but not a row (e.g. a missing media file).
    pub fn not_found_message(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
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

    /// Builds a 502 error for an `[external_api]` upstream failure.
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::BadGateway {
            message: message.into(),
        }
    }

    /// The HTTP status this error renders as (browsers get the same
    /// status the JSON envelope would answer with).
    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::NotFound { .. } => StatusCode::NOT_FOUND,
            AppError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            AppError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AppError::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::RequestTimeout { .. } => StatusCode::REQUEST_TIMEOUT,
            AppError::MethodNotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            AppError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            AppError::Forbidden { .. } => StatusCode::FORBIDDEN,
            AppError::Conflict { .. } => StatusCode::CONFLICT,
            AppError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::BadGateway { .. } => StatusCode::BAD_GATEWAY,
        }
    }

    /// The human-readable message (safe to show to clients).
    pub fn message(&self) -> &str {
        match self {
            AppError::NotFound { message }
            | AppError::BadRequest { message }
            | AppError::RateLimited { message, .. }
            | AppError::PayloadTooLarge { message }
            | AppError::RequestTimeout { message }
            | AppError::MethodNotAllowed { message }
            | AppError::Unauthorized { message }
            | AppError::Forbidden { message }
            | AppError::Conflict { message }
            | AppError::Internal { message }
            | AppError::BadGateway { message } => message,
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
            AppError::BadGateway { .. } => (StatusCode::BAD_GATEWAY, "BAD_GATEWAY", None),
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
            | AppError::Internal { message }
            | AppError::BadGateway { message } => message,
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

// ─── The shared HTML error page (F13) ────────────────────────────────

/// The facts one shared HTML error page renders.
///
/// Every 4xx/5xx the server answers to a *browser* lands here: the
/// error middleware negotiates on `Accept` (HTML navigations get this
/// page, API clients keep the JSON envelope), and the two hand-built
/// error pages the auth and CMS routes raise for form posts delegate
/// to the same builder so the whole surface stays uniform. The page
/// is deliberately script-free and carries no inline styles — the
/// default CSP (`style-src 'self'`) serves it untouched with its
/// stylesheet at `/assets/error.css`.
pub struct HtmlErrorPage<'a> {
    /// The HTTP status (kept verbatim on the response).
    pub status: StatusCode,
    /// The envelope code (`NOT_FOUND`, `RATE_LIMITED`, …) shown as a
    /// small tag next to the number.
    pub code: &'a str,
    /// The engine/server message (escaped into the body).
    pub message: &'a str,
    /// Correlation id, printed for log matching.
    pub request_id: Option<&'a str>,
    /// Request method + target for the diagnostics block, when the
    /// raiser knows them.
    pub method: Option<&'a str>,
    pub target: Option<&'a str>,
    /// `Retry-After` seconds — present on 429s. The page then reloads
    /// itself via `<meta http-equiv="refresh">` once the budget
    /// refills, which is how the no-JavaScript panel survives its own
    /// burst limiter without the visitor pressing F5.
    pub retry_after_secs: Option<u64>,
    /// The pinned panel theme (`wm_theme` cookie, "dark"/"light") so
    /// editors keep their dark panel even on error pages; `None`
    /// follows the OS via `prefers-color-scheme`.
    pub theme: Option<&'a str>,
}

impl HtmlErrorPage<'_> {
    /// Renders the complete HTML document.
    pub fn render(&self) -> String {
        let status = self.status.as_u16();
        let title = status_title(self.status);
        let message = escape_html_text(self.message);
        let code = escape_html_text(self.code);
        let request_id = self.request_id.map(escape_html_text);
        let method = self.method.map(escape_html_text);
        let target = self.target.map(escape_html_text);
        let theme_attr = match self.theme {
            Some("dark") => " data-theme=\"dark\"",
            Some("light") => " data-theme=\"light\"",
            _ => "",
        };

        // The self-heal: only for 429, delay copied from Retry-After
        // and capped (a limiter that asks for minutes should not turn
        // the browser into a metronome).
        let (meta_refresh, retry_note) = match (self.status, self.retry_after_secs) {
            (StatusCode::TOO_MANY_REQUESTS, Some(secs)) => {
                let delay = secs.clamp(1, 5);
                (
                    format!("\n  <meta http-equiv=\"refresh\" content=\"{delay}\">"),
                    format!(
                        "\n    <p class=\"error-retry\">This page reloads itself in \
                         {delay} second{} — the request budget refills continuously.</p>",
                        if delay == 1 { "" } else { "s" }
                    ),
                )
            }
            _ => (String::new(), String::new()),
        };

        // 401s point at the sign-in page first; everything else offers
        // the homepage as the primary way out.
        let actions = if self.status == StatusCode::UNAUTHORIZED {
            "<a class=\"primary\" href=\"/login\">Sign in</a>\n      \
             <a href=\"/\">Go to the homepage</a>\n      \
             <a href=\"/p\">Published pages</a>"
        } else {
            "<a class=\"primary\" href=\"/\">Go to the homepage</a>\n      \
             <a href=\"/p\">Published pages</a>\n      \
             <a href=\"/admin\">CMS panel</a>"
        };

        let request_line = match (&method, &target) {
            (Some(method), Some(target)) => {
                format!("\n    <dt>Request</dt><dd>{} {}</dd>", method, target)
            }
            (Some(method), None) => format!("\n    <dt>Request</dt><dd>{}</dd>", method),
            (None, Some(target)) => format!("\n    <dt>Request</dt><dd>{}</dd>", target),
            (None, None) => String::new(),
        };
        let request_id_line = request_id
            .as_deref()
            .map(|id| format!("\n    <dt>Request id</dt><dd>{}</dd>", id))
            .unwrap_or_default();

        format!(
            "<!DOCTYPE html>\n<html lang=\"en\"{theme_attr}>\n<head>\n  \
             <meta charset=\"utf-8\">\n  \
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  \
             <meta name=\"robots\" content=\"noindex\">\n  \
             <title>{status} {title} — wallermax</title>\n  \
             <link rel=\"stylesheet\" href=\"/assets/error.css\">{meta_refresh}\n\
             </head>\n<body>\n  \
             <main class=\"error-card\" role=\"main\">\n    \
             <div class=\"error-status\">\n      \
             <span class=\"error-code\">{status}</span>\n      \
             <span class=\"error-code-name\">{code}</span>\n    \
             </div>\n    \
             <h1>{title}</h1>\n    \
             <p class=\"error-message\">{message}</p>\n    \
             <dl class=\"error-detail\">{request_line}{request_id_line}</dl>\n    \
             <div class=\"error-actions\">\n      {actions}\n    </div>{retry_note}\n    \
             <p class=\"error-brand\">wallermax-server — served without a line of \
             JavaScript.</p>\n  \
             </main>\n</body>\n</html>\n"
        )
    }
}

/// The friendly English headline per status.
pub fn status_title(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "Bad request",
        StatusCode::UNAUTHORIZED => "Sign-in required",
        StatusCode::FORBIDDEN => "Access denied",
        StatusCode::NOT_FOUND => "Page not found",
        StatusCode::METHOD_NOT_ALLOWED => "Method not allowed",
        StatusCode::CONFLICT => "Conflict",
        StatusCode::REQUEST_TIMEOUT => "Request timeout",
        StatusCode::PAYLOAD_TOO_LARGE => "Upload too large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "Unsupported media type",
        StatusCode::TOO_MANY_REQUESTS => "Slow down for a moment",
        StatusCode::INTERNAL_SERVER_ERROR => "Something went wrong",
        StatusCode::BAD_GATEWAY => "Bad gateway",
        StatusCode::SERVICE_UNAVAILABLE => "Service unavailable",
        StatusCode::GATEWAY_TIMEOUT => "Gateway timeout",
        _ => "Request failed",
    }
}

/// HTML-escapes a text so server messages stay inert inside the page.
pub fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
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

    #[tokio::test]
    async fn bad_gateway_renders_json_502() {
        let response =
            AppError::bad_gateway("upstream did not answer within the configured timeout")
                .into_response();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let json = body_json(response).await;
        assert_eq!(json["error"]["code"], "BAD_GATEWAY");
        assert_eq!(
            json["error"]["message"],
            "upstream did not answer within the configured timeout"
        );
    }
}
