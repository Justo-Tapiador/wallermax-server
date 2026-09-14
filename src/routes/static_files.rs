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
use axum::extract::Request;
use axum::routing::get_service;
use axum::Router;
use tower::Service as _;
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

/// Mounts the CMS host's shared-asset fallback (F14).
///
/// The CMS host borrows exactly what its pages and panel need from
/// the static root: the `/assets/*` subtree (the shared stylesheets
/// `wallermax.css`, `admin.css`, `error.css`) plus `/favicon.ico` and
/// `/robots.txt`, the two root files browsers ask of any origin.
/// Everything else — and every `.jhs` path, so a template source can
/// never be served raw — answers the standard JSON 404 (which the
/// templates middleware then auto-routes to `views/`).
///
/// Restricting the tree on purpose: the rest of `public/` is the
/// **main host's** content, and keeping it off the CMS host keeps the
/// two names' surfaces disjoint — no accidental duplicate content,
/// no cross-origin surprises under the CSP's `'self'` model.
pub fn mount_shared_fallback(router: Router<AppState>, config: &StaticConfig) -> Router<AppState> {
    // Nested router turning ServeDir misses into the standard JSON
    // 404 envelope — the same shape `mount_fallback` uses.
    let json_not_found = Router::new()
        .fallback(super::not_found)
        .into_service::<Body>();

    let serve = ServeDir::new(&config.root_dir)
        .call_fallback_on_method_not_allowed(true)
        .fallback(json_not_found.clone());

    router.fallback_service(tower::service_fn(move |request: Request| {
        let mut serve = serve.clone();
        let mut not_found = json_not_found.clone();
        async move {
            if is_shared_asset_path(request.uri().path()) {
                let served = serve
                    .call(request)
                    .await
                    .expect("ServeDir with a fallback set is infallible");
                // ServeDir answers with its own body type; axum's
                // `Body::new` adapts it (the parts and status stay).
                Ok::<_, std::convert::Infallible>(served.map(Body::new))
            } else {
                Ok(not_found
                    .call(request)
                    .await
                    .expect("the JSON 404 service is infallible"))
            }
        }
    }))
}

/// Paths the CMS host borrows from the static root: the stylesheet
/// subtree and the two root files browsers request by default. `.jhs`
/// paths never pass — public templates render on the main host only,
/// and their sources are never served raw anywhere.
fn is_shared_asset_path(path: &str) -> bool {
    !path.ends_with(".jhs")
        && (path == "/favicon.ico" || path == "/robots.txt" || path.starts_with("/assets/"))
}

#[cfg(test)]
mod tests {
    use super::is_shared_asset_path;

    #[test]
    fn shared_assets_cover_the_stylesheet_subtree_and_root_files() {
        assert!(is_shared_asset_path("/assets/wallermax.css"));
        assert!(is_shared_asset_path("/assets/admin.css"));
        assert!(is_shared_asset_path("/assets/"));
        assert!(is_shared_asset_path("/favicon.ico"));
        assert!(is_shared_asset_path("/robots.txt"));
    }

    #[test]
    fn other_public_content_stays_on_the_main_host() {
        assert!(!is_shared_asset_path("/"));
        assert!(!is_shared_asset_path("/index.html"));
        assert!(!is_shared_asset_path("/hello.jhs"));
        assert!(!is_shared_asset_path("/docs/guide.html"));
        assert!(!is_shared_asset_path("/favicon.png"));
    }

    #[test]
    fn jhs_sources_never_pass_even_under_assets() {
        assert!(!is_shared_asset_path("/assets/leak.jhs"));
        assert!(!is_shared_asset_path("/assets/sub/leak.jhs"));
    }
}
