//! Cross-origin resource sharing.
//!
//! Builds a tower-http [`CorsLayer`] from the `[cors]` configuration:
//!
//! - `allowed_origins`: exact-origin allowlist (or `["*"]` for a wildcard);
//! - methods: `GET`, `POST`, `PUT`, `PATCH`, `DELETE`;
//! - headers: `Content-Type`, `Authorization`, `X-Request-Id`;
//! - `max_age_secs`: how long browsers may cache preflight responses.
//!
//! Preflight (`OPTIONS`) requests are answered by the layer directly,
//! before the request-id/logging middlewares, so they never pollute logs
//! or stats. With an empty allowlist the layer is effectively inert and
//! browsers deny all cross-origin reads (the safe default).
//!
//! The layer is only applied while `[middleware] cors = true`; the origin
//! syntax is validated at configuration load time
//! (see [`crate::config::CorsConfig`]).

use std::time::Duration;

use axum::http::{header, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::config::CorsConfig;

/// Builds the CORS layer from the configuration.
///
/// The configuration must have been validated already (origins are valid
/// header values by construction).
pub fn layer(config: &CorsConfig) -> CorsLayer {
    let mut cors = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::HeaderName::from_static("x-request-id"),
        ])
        .max_age(Duration::from_secs(config.max_age_secs));

    if config.allowed_origins == ["*"] {
        // Wildcard: reflect nothing, allow everyone.
        cors = cors.allow_origin(Any);
    } else {
        let origins: Vec<HeaderValue> = config
            .allowed_origins
            .iter()
            .map(|origin| {
                HeaderValue::from_str(origin)
                    .expect("origins are validated at configuration load time")
            })
            .collect();
        cors = cors.allow_origin(AllowOrigin::list(origins));
    }

    cors
}
