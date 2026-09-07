//! Route modules.
//!
//! Each feature area lives in its own module exposing a `routes()` function
//! that returns a [`Router`] fragment; [`routes`] merges them all into the
//! final route tree.
//!
//! Adding a new feature area (in this or a future phase) takes three steps:
//!
//! 1. Create `src/routes/<name>.rs` with handlers and
//!    `pub fn routes() -> Router<AppState>`.
//! 2. Declare the module here: `pub mod <name>;`
//! 3. Merge it below: `.merge(<name>::routes())`

pub mod admin;
pub mod auth;
pub mod cms;
pub mod echo;
pub mod health;
pub mod index;
pub mod metrics;
pub mod static_files;
pub mod stats;

use axum::extract::Request;
use axum::response::Response;
use axum::Router;

use crate::config::{MetricsConfig, StaticConfig};
use crate::error::AppError;
use crate::middleware::request_id::RequestId;
use crate::state::AppState;

/// Assembles the complete route tree, including the JSON fallbacks.
///
/// `auth_enabled` mounts the `/api/auth/*` and `/api/admin/*` families,
/// which depend on the database-backed authentication services being
/// initialized in the application state; `refresh_enabled` (a subset of
/// `auth_enabled`) mounts the token refresh and logout endpoints.
///
/// `metrics` mounts the Prometheus exposition endpoint at its configured
/// path while enabled.
///
/// `cms_enabled` mounts the CMS family (`/p/{slug}`, `/admin/*` and
/// `/perfil/password`), which additionally requires the template
/// rendering services in the application state.
///
/// `static_files` mounts the static file family (see
/// [`static_files`]): while enabled, `GET /` serves the index file and
/// unmatched paths resolve against the static root instead of the JSON
/// 404 handler; the service index stays at `GET /api`.
pub fn routes(
    auth_enabled: bool,
    refresh_enabled: bool,
    static_files: &StaticConfig,
    metrics: &MetricsConfig,
    cms_enabled: bool,
) -> Router<AppState> {
    let mut router = Router::new()
        .merge(index::routes())
        .merge(health::routes())
        .merge(stats::routes())
        .merge(echo::routes());

    if auth_enabled {
        router = router
            .merge(auth::routes(refresh_enabled))
            .merge(admin::routes());
    }

    if cms_enabled {
        router = router.merge(cms::routes());
    }

    if metrics.enabled {
        router = router.merge(metrics::routes(&metrics.path));
    }

    if static_files.enabled {
        router = router.merge(static_files::routes(static_files));
        router = static_files::mount_fallback(router, static_files);
    } else {
        // Without static serving, `GET /` is the JSON service index.
        router = router.merge(index::root_routes()).fallback(not_found);
    }

    router.method_not_allowed_fallback(method_not_allowed)
}

/// JSON 404 handler that echoes the correlation id when available.
pub(crate) async fn not_found(request: Request) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());

    AppError::not_found(&method, &path).into_response_with_request_id(request_id.as_deref())
}

/// JSON 405 handler for known paths with unsupported methods.
async fn method_not_allowed(request: Request) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());

    AppError::method_not_allowed(&method, &path)
        .into_response_with_request_id(request_id.as_deref())
}
