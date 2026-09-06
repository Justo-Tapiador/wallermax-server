//! Request correlation ids.
//!
//! Every request gets a unique [`RequestId`] (a UUID v4) unless the client
//! already supplied a valid `X-Request-Id` header, in which case it is
//! reused (capped at 64 characters of `[A-Za-z0-9_-]`) so upstream gateways
//! can correlate traffic.
//!
//! The policy for client-supplied ids is configurable through
//! `[request_id] mode`:
//!
//! - `accept` (default): reuse a valid client-supplied id;
//! - `overwrite`: always generate a server-side id, so clients cannot
//!   forge or pollute correlation ids.
//!
//! The id is stored in the request extensions (readable by handlers and
//! inner middleware, e.g. to embed it in error bodies) and echoed back as
//! the `X-Request-Id` response header.

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::config::RequestIdMode;
use crate::state::AppState;

/// Name of the correlation id header.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Maximum accepted length of a client-supplied request id.
const MAX_CLIENT_ID_LEN: usize = 64;

/// Correlation id attached to every request.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

impl RequestId {
    /// Generates a fresh random id.
    fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Extracts a usable client-supplied id, if any.
    ///
    /// Returns `None` when the header is missing, empty, too long or
    /// contains characters outside `[A-Za-z0-9_-]`.
    fn from_client(value: Option<&HeaderValue>) -> Option<Self> {
        let value = value?.to_str().ok()?;

        let is_valid = !value.is_empty()
            && value.len() <= MAX_CLIENT_ID_LEN
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');

        if is_valid {
            Some(Self(value.to_owned()))
        } else {
            None
        }
    }
}

/// Middleware entry point (see [`crate::middleware`] for ordering).
pub async fn run(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    // In `overwrite` mode client values are ignored entirely.
    let client_id = if state.config().request_id.mode == RequestIdMode::Overwrite {
        None
    } else {
        RequestId::from_client(request.headers().get(&REQUEST_ID_HEADER))
    };

    let request_id = client_id.unwrap_or_else(RequestId::generate);

    // Make the id available to inner middleware and handlers.
    request.extensions_mut().insert(request_id.clone());

    let mut response = next.run(request).await;

    // Echo the id back to the client.
    if let Ok(value) = HeaderValue::from_str(&request_id.0) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_are_validated() {
        assert!(RequestId::from_client(None).is_none());

        let valid = RequestId::from_client(Some(&HeaderValue::from_static("abc-123")));
        assert_eq!(valid.expect("valid id").0, "abc-123");

        // Empty values are rejected.
        assert!(RequestId::from_client(Some(&HeaderValue::from_static(""))).is_none());

        // Invalid characters are rejected.
        assert!(RequestId::from_client(Some(&HeaderValue::from_static("bad id!"))).is_none());

        // Overly long values are rejected.
        let long = "a".repeat(MAX_CLIENT_ID_LEN + 1);
        let long = HeaderValue::from_str(&long).expect("header value");
        assert!(RequestId::from_client(Some(&long)).is_none());
    }

    #[test]
    fn generated_ids_are_uuids() {
        let id = RequestId::generate();
        assert!(Uuid::parse_str(&id.0).is_ok());
    }
}
