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
pub mod external_api;
pub mod health;
pub mod index;
pub mod media;
pub mod metrics;
pub mod search;
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
/// `/profile/password`), which additionally requires the template
/// rendering services in the application state. The media library
/// routes (`/admin/media` and the public `/media/{id}/{name}`
/// family, F9) ride the exact same mount: they live in the same
/// [`CmsContext`].
///
/// `external_api_enabled` mounts the external API proxy
/// (`GET/POST /api/ext/{name}` and `GET/POST /api/ext/{name}/{subpath}`),
/// which is enabled by configuring at least one
/// `[[external_api.endpoints]]` entry.
///
/// `static_files` mounts the static file family (see
/// [`static_files`]): while enabled, `GET /` serves the index file and
/// unmatched paths resolve against the static root instead of the JSON
/// 404 handler; the service index stays at `GET /api`.
///
/// This is the **single-host** tree, used while no hostname is
/// mapped at all — every surface on one host, the pre-F14
/// behaviour. While the boot-time bindings map any hostname
/// (the `domains` table on a database boot, `[cms] hosts`
/// otherwise), [`vhost_routes`] builds the per-organization split
/// instead.
pub fn routes(
    auth_enabled: bool,
    refresh_enabled: bool,
    static_files: &StaticConfig,
    metrics: &MetricsConfig,
    cms_enabled: bool,
    external_api_enabled: bool,
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
        router = router
            .merge(cms::routes())
            .merge(media::routes())
            .merge(search::routes());
    }

    if external_api_enabled {
        router = router.merge(external_api::routes());
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

/// Assembles the **per-organization** route trees for name-based
/// virtual hosting (F14, generalized by F17), wrapped in one
/// dispatcher: one IP, one port, and the request's `Host` header
/// decides which organization's tree serves it.
///
/// The **main tree** (every host the bindings do not map — unknown
/// or missing ones included — plus hostnames mapped to the `main`
/// organization on purpose) gets what an operator runs: the static
/// site under the main organization's document root (the `[static]`
/// root, which the seed keeps in step with it), the `.jhs` files
/// living there rendered on the fly, and the machinery — `/api`,
/// `/api/auth/*`, `/api/admin/users`, `/health`, `/metrics`, the
/// external proxy.
///
/// The **CMS tree** (hostnames mapped to the `cms` organization)
/// gets the visitor surface: the public pages, `views/`
/// auto-routing, the panel, the media library, search, feeds and
/// the sitemap — plus the `/api/auth/*` family, because the no-JS
/// login modal and login page POST to `/api/auth/login` (an HTML
/// form needs its endpoint on the same origin), and the shared
/// `/assets/*` subtree so the panel keeps its styles under the CSP's
/// `style-src 'self'`. Everything else on the CMS host answers the
/// standard JSON 404 (the templates middleware auto-routes views
/// from there).
///
/// A **tenant tree** (hostnames mapped to any other organization)
/// gets that organization's own content: a self-contained static
/// site from its `document_root` — the file tree, the directory
/// indexes, the on-the-fly `.jhs` rendering — and nothing else. See
/// [`static_files::tenant_routes`].
///
/// Classification is [`crate::vhosts::classify`] — the same pure
/// function the templates middleware consults, so the two layers
/// agree on every request by construction. Both read the boot-time
/// bindings from the application state (F17): the `domains` table
/// joined to the `organizations` rows on a database boot, the
/// `[cms] hosts` bootstrap otherwise. The middleware pipeline wraps
/// the dispatcher from the outside (see [`crate::middleware::apply`]),
/// so security headers, error pages, rate limiting and friends serve
/// every tree identically.
///
/// Validation guarantees `hosts` non-empty requires `cms.enabled`
/// and `static.enabled`, so the fallback shapes below are
/// belt-and-braces.
pub fn vhost_routes(
    auth_enabled: bool,
    refresh_enabled: bool,
    static_files: &StaticConfig,
    metrics: &MetricsConfig,
    cms_enabled: bool,
    external_api_enabled: bool,
    state: &AppState,
) -> Router<AppState> {
    use tower::Service as _;

    // The main host: the static site and the operator machinery.
    let mut main = Router::new()
        .merge(index::routes())
        .merge(health::routes())
        .merge(stats::routes())
        .merge(echo::routes());

    if auth_enabled {
        main = main
            .merge(auth::routes(refresh_enabled))
            .merge(admin::routes());
    }

    if external_api_enabled {
        main = main.merge(external_api::routes());
    }

    if metrics.enabled {
        main = main.merge(metrics::routes(&metrics.path));
    }

    if static_files.enabled {
        main = main.merge(static_files::routes(static_files));
        main = static_files::mount_fallback(main, static_files);
    } else {
        // Without static serving, `GET /` is the JSON service index.
        main = main.merge(index::root_routes()).fallback(not_found);
    }

    let main = main
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state.clone());

    // The CMS host: the visitor surface. `/api/auth/*` rides along
    // for the no-JS forms (the login modal's action is
    // `/api/auth/login`); `/api`, `/health`, `/metrics` and the proxy
    // deliberately do not — the CMS host exposes exactly what its
    // pages need.
    let mut cms_tree = Router::new();

    if cms_enabled {
        cms_tree = cms_tree
            .merge(cms::routes())
            .merge(media::routes())
            .merge(search::routes());
    }

    if auth_enabled {
        cms_tree = cms_tree.merge(auth::routes(refresh_enabled));
    }

    if static_files.enabled {
        // The shared stylesheets and root files browsers ask for;
        // everything else falls to the JSON 404 the templates
        // middleware auto-routes from.
        cms_tree = static_files::mount_shared_fallback(cms_tree, static_files);
    } else {
        cms_tree = cms_tree.fallback(not_found);
    }

    let cms_tree = cms_tree
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state.clone());

    // Tenant trees (F17): one self-contained static site per
    // organization the bindings map besides the bootstrap two.
    // Several host names may route to the same organization — they
    // share its tree — so the trees are built once per organization,
    // in first-seen order. A missing document root is fine here: the
    // boot already warned, and `ServeDir` simply answers 404s until
    // the directory appears.
    let mut tenants: Vec<(String, Router)> = Vec::new();
    for binding in state.host_bindings() {
        if binding.organization != crate::db::MAIN_ORGANIZATION_KEY
            && binding.organization != crate::db::CMS_ORGANIZATION_KEY
            && !tenants.iter().any(|(key, _)| key == &binding.organization)
        {
            tenants.push((
                binding.organization.clone(),
                static_files::tenant_routes(&binding.document_root, &static_files.index_file)
                    .method_not_allowed_fallback(method_not_allowed)
                    .with_state(state.clone()),
            ));
        }
    }

    // The dispatcher: one service, one tree per organization, the
    // Host header decides. Router implements
    // `Service<Request, Error = Infallible>`, so the boxed future
    // simply forwards the result.
    let bindings = state.host_bindings().to_vec();
    let dispatch = tower::service_fn(move |request: Request| {
        let main = main.clone();
        let cms_tree = cms_tree.clone();
        let tenants = tenants.clone();
        let bindings = bindings.clone();
        async move {
            let mut router =
                match crate::vhosts::classify(request.uri(), request.headers(), &bindings) {
                    crate::vhosts::HostClass::Cms => cms_tree,
                    crate::vhosts::HostClass::Main => main,
                    crate::vhosts::HostClass::Tenant(binding) => tenants
                        .iter()
                        .find(|(key, _)| *key == binding.organization)
                        .expect("every mapped organization has its tree")
                        .1
                        .clone(),
                };
            // `Router` implements `Service<Request, Error = Infallible>`,
            // so its own `Result` is exactly the dispatcher's answer.
            router.call(request).await
        }
    });

    let outer: Router<AppState> = Router::new().fallback_service(dispatch);
    outer
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
