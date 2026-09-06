//! The composable middleware pipeline.
//!
//! Layers are applied with [`axum::Router::layer`], which wraps the existing
//! stack on every call: the **last** layer applied becomes the **outermost**
//! one, i.e. the first to see a request and the last to touch the response.
//! [`apply`] therefore applies the layers in reverse execution order so the
//! pipeline below reads naturally from top to bottom:
//!
//! ```text
//! request  ->  security headers -> cors -> request id -> logging
//!              -> rate limit -> body limit -> timeout -> routes
//! response <-  security headers <- cors <- request id <- logging
//!              <- rate limit <- body limit <- timeout <- routes
//! ```
//!
//! Ordering rationale:
//!
//! - **Security headers** is outermost so every response — including 404
//!   fallbacks, timeouts and rate-limit rejections — carries them.
//! - **CORS** sits outside logging: preflight requests are answered
//!   directly without polluting logs or stats.
//! - **Request id** runs before the other feature middlewares so error
//!   responses (429/413/408) can embed the correlation id.
//! - **Logging** sees rejected and timed-out requests too, so access logs
//!   account for every arrival.
//! - **Rate limit** rejects floods before any body processing happens.
//! - **Body limit** combines an early `Content-Length` check with
//!   tower-http's stream enforcement.
//! - **Timeout** bounds the total handling time of whatever remains.
//!
//! Each middleware is toggled independently through the `[middleware]`
//! section of the configuration (see [`crate::config::MiddlewareConfig`]).
//! To add a new middleware in a future phase: create a module here with a
//! `pub async fn run(...)` entry point and register it in [`apply`].

pub mod body_limit;
pub mod cors;
pub mod logging;
pub mod rate_limit;
pub mod request_id;
pub mod security_headers;
pub mod timeout;

use axum::middleware;
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::AppConfig;
use crate::state::AppState;

/// Assembles the route tree with the middleware pipeline from `config`.
///
/// This is the heart of the server's modularity: behaviour is changed by
/// toggling layers in configuration, and it is extended by adding new
/// modules and registering them here.
pub fn apply(config: &AppConfig, state: AppState, router: Router<AppState>) -> Router<AppState> {
    // NOTE: `Router::layer` wraps the existing stack, so layers are applied
    // in REVERSE execution order (the last applied is outermost, runs first).

    // 8th in execution order: stream-level enforcement of the body limit.
    let router = if config.middleware.body_limit {
        router.layer(RequestBodyLimitLayer::new(
            config.server.max_body_size_bytes,
        ))
    } else {
        router
    };

    // 7th in execution order: bound the total request handling time.
    let router = if config.middleware.timeout {
        router.layer(middleware::from_fn_with_state(state.clone(), timeout::run))
    } else {
        router
    };

    // 6th in execution order: reject oversized request bodies early.
    let router = if config.middleware.body_limit {
        router.layer(middleware::from_fn_with_state(
            state.clone(),
            body_limit::run,
        ))
    } else {
        router
    };

    // 5th in execution order: per-client-IP rate limiting.
    let router = if config.middleware.rate_limit {
        router.layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit::run,
        ))
    } else {
        router
    };

    // 4th in execution order: structured logging, latency header, metrics.
    let router = if config.middleware.logging {
        router.layer(middleware::from_fn_with_state(state.clone(), logging::run))
    } else {
        router
    };

    // 3rd in execution order: correlation ids.
    let router = if config.middleware.request_id {
        router.layer(middleware::from_fn_with_state(
            state.clone(),
            request_id::run,
        ))
    } else {
        router
    };

    // 2nd in execution order: cross-origin resource sharing.
    let router = if config.middleware.cors {
        router.layer(cors::layer(&config.cors))
    } else {
        router
    };

    // 1st (outermost): security headers on every response.
    if config.middleware.security_headers {
        router.layer(middleware::from_fn_with_state(state, security_headers::run))
    } else {
        router
    }
}
