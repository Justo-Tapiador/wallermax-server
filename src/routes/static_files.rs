//! Static file serving (`[static]` section).
//!
//! While enabled, two pieces are mounted:
//!
//! - `GET /` answers with `root_dir/index_file` (via [`ServeFile`], so
//!   conditional requests, `Range` and `ETag`/`Last-Modified` all work);
//! - every other otherwise-unmatched path is served from `root_dir` by
//!   [`ServeDir`] (e.g. `/style.css` -> `root_dir/style.css`, `/docs/` ->
//!   `root_dir/docs/index.html`).
//!
//! Requests that do not map to a file — and non-`GET`/`HEAD` requests to
//! file paths — fall back to the standard JSON 404 envelope used by the
//! rest of the server, so error responses stay uniform and carry the
//! `request_id` correlation id. [`ServeDir`] rejects `..` segments and
//! encoded traversals (they never touch the filesystem), keeping requests
//! inside the configured root.
//!
//! The JSON service index is always available at `GET /api`, and the API
//! routes keep precedence over static paths in all cases.

use axum::body::Body;
use axum::routing::get_service;
use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::StaticConfig;
use crate::state::AppState;

/// Route fragment for this module: `GET /` -> the configured index file.
pub fn routes(config: &StaticConfig) -> Router<AppState> {
    Router::new().route("/", get_service(ServeFile::new(config.index_path())))
}

/// Mounts the static file fallback on `router`.
///
/// Any request that reaches the router fallback (no matching API route) is
/// resolved against `root_dir`:
///
/// - `GET`/`HEAD` for an existing file (or `dir/index.html` for
///   directories) -> the file;
/// - anything else -> the JSON 404 envelope via the nested router (the
///   same handler used when static serving is disabled).
pub fn mount_fallback(router: Router<AppState>, config: &StaticConfig) -> Router<AppState> {
    // Nested router turning ServeDir misses into the standard JSON 404
    // envelope (with the request's correlation id). Serving it for every
    // non-file hit also mirrors the API behaviour of answering unmatched
    // paths with 404 regardless of method.
    let json_not_found = Router::new()
        .fallback(super::not_found)
        .into_service::<Body>();

    router.fallback_service(
        ServeDir::new(&config.root_dir)
            .call_fallback_on_method_not_allowed(true)
            .fallback(json_not_found),
    )
}
