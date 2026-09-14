//! Server bootstrap, serving loop and graceful shutdown.
//!
//! Two serving modes share the same application stack
//! ([`build_state`] + [`build_app`]):
//!
//! - **plain HTTP** ([`serve_plain`]) — `axum::serve` with the graceful
//!   shutdown watchdog;
//! - **HTTPS** ([`serve_tls`], `[tls] enabled = true`) — the same router
//!   served through `axum-server` with rustls, plus an optional
//!   plain-HTTP listener that answers every request with a `308`
//!   redirect to its HTTPS equivalent.

use std::error::Error;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tokio::sync::watch;

use crate::auth::JwtService;
use crate::config::AppConfig;
use crate::db::{
    self, SqliteMediaRepository, SqliteMenuRepository, SqlitePageRepository, SqliteUserRepository,
};
use crate::routes;
use crate::state::{self, AppState, AuthContext, CmsContext};

/// Startup/serving error type (kept simple on purpose; a dedicated error
/// enum can be introduced in a later phase if the surface grows).
pub type ServerError = Box<dyn Error + Send + Sync + 'static>;

/// Builds the complete application: standard route tree + middleware
/// pipeline.
///
/// The authentication and admin route families are mounted only when the
/// application state carries the auth services (see [`build_state`]).
///
/// Exposed so integration tests boot the exact same stack the binary
/// serves, and so future phases (or embedders) can reuse it.
pub fn build_app(config: &AppConfig, state: AppState) -> Router {
    let refresh_enabled = state.refresh_enabled();
    // F17: the main organization's document root is the static
    // tree's serving truth. The row equals `[static] root_dir` by
    // construction (the startup seed keeps them in step), so this
    // changes nothing for existing setups — it only routes the
    // serving path through the organizations table, the way F16
    // routed the host list through `domains`. The rest of the
    // section (the switch, the index file — a server-wide
    // convention) stays configuration.
    let mut static_files = config.static_files.clone();
    static_files.root_dir = state.main_document_root().to_owned();
    let router = if state.host_bindings().is_empty() {
        routes::routes(
            state.auth_enabled(),
            refresh_enabled,
            &static_files,
            &config.metrics,
            state.cms_enabled(),
            state.external_api_enabled(),
        )
    } else {
        // F14: name-based virtual hosting — one listener, as many
        // trees as there are organizations. F17 generalizes the
        // split: the boot-time bindings (the `domains` table on a
        // database boot, seeded from `[cms] hosts`) decide which
        // organization's tree each Host name serves — the CMS tree,
        // the main tree, or a tenant organization's self-contained
        // static site from its document root.
        routes::vhost_routes(
            state.auth_enabled(),
            refresh_enabled,
            &static_files,
            &config.metrics,
            state.cms_enabled(),
            state.external_api_enabled(),
            &state,
        )
    };
    build_app_with_routes(config, state, router)
}

/// Builds the application around a **custom route tree**.
///
/// Integration tests use this to inject routes with controlled behaviour
/// (e.g. handlers that sleep to exercise the timeout middleware); embedders
/// can reuse the middleware pipeline over their own routes.
pub fn build_app_with_routes(
    config: &AppConfig,
    state: AppState,
    router: Router<AppState>,
) -> Router {
    crate::middleware::apply(config, state.clone(), router).with_state(state)
}

/// Initializes the shared application state, including the SQLite pool,
/// the embedded migrations and the authentication services when the
/// corresponding features are enabled in the configuration.
///
/// Exposed so integration tests and embedders can build the exact state
/// the binary serves without going through the serving loop.
///
/// # Errors
///
/// Returns an error when the enabled database cannot be opened or
/// migrated (reported to the operator, never to clients).
pub async fn build_state(config: &AppConfig) -> Result<AppState, ServerError> {
    if config.static_files.enabled {
        let root = Path::new(&config.static_files.root_dir);
        if !root.is_dir() {
            return Err(format!(
                "static file serving is enabled but the root directory `{}` does not exist; \
                 create it or point `static.root_dir` at an existing directory",
                config.static_files.root_dir
            )
            .into());
        }
        if !root.join(&config.static_files.index_file).is_file() {
            tracing::warn!(
                index_path = %config.static_files.index_path().display(),
                "static index file missing; `GET /` will answer 404 until it exists"
            );
        }
        tracing::info!(
            root_dir = %config.static_files.root_dir,
            index_file = %config.static_files.index_file,
            "static file serving enabled"
        );
    }

    if let Err(error) = validate_templates(config) {
        return Err(error.into());
    }

    if !config.database.enabled {
        let state = AppState::new(config.clone());
        ensure_external_api_resolved(config, &state)?;
        return Ok(state);
    }

    let pool = db::connect(&config.database).await.map_err(|error| {
        format!(
            "failed to open the SQLite database `{}`: {error}",
            config.database.url
        )
    })?;
    db::run_migrations(&pool).await.map_err(|message| {
        format!(
            "failed to apply the database migrations for `{}`: {message}",
            config.database.url
        )
    })?;

    // F15: give the tenants their rows and mirror the platform roles
    // into the CMS organization's memberships — a no-op on a fresh
    // database, the upgrade path for one that predates F15.
    db::seed_organizations(
        &pool,
        &config.static_files.root_dir,
        &config.templates.views_dir,
    )
    .await
    .map_err(|message| {
        format!(
            "failed to seed the organizations for `{}`: {message}",
            config.database.url
        )
    })?;
    db::mirror_cms_memberships(&pool).await.map_err(|message| {
        format!(
            "failed to mirror the CMS memberships for `{}`: {message}",
            config.database.url
        )
    })?;

    // F16: the domains table decides which Host names serve the CMS
    // tree. `[cms] hosts` is the bootstrap: the seeder keeps its own
    // rows in step with the list (added when added, removed when
    // removed), while rows created by hand — or, later, by the panel
    // — are data and survive every boot untouched.
    db::seed_domains(&pool, &config.cms.hosts)
        .await
        .map_err(|message| {
            format!(
                "failed to seed the domains for `{}`: {message}",
                config.database.url
            )
        })?;
    // F17: the serving truth loads with the split — the
    // organizations' document roots and every mapped hostname, each
    // carrying the organization (and therefore the tree) it routes
    // to.
    let document_roots = db::load_document_roots(&pool).await.map_err(|message| {
        format!(
            "failed to load the document roots for `{}`: {message}",
            config.database.url
        )
    })?;
    let bindings = db::load_host_bindings(&pool).await.map_err(|message| {
        format!(
            "failed to load the host bindings for `{}`: {message}",
            config.database.url
        )
    })?;
    let cms_hosts: Vec<String> = bindings
        .iter()
        .filter(|binding| binding.organization == db::CMS_ORGANIZATION_KEY)
        .map(|binding| binding.hostname.clone())
        .collect();
    tracing::info!(
        url = %config.database.url,
        max_connections = config.database.max_connections,
        "sqlite pool ready (migrations applied, organizations and domains seeded)"
    );

    // The F14 rule, extended to the data plane: a CMS host borrows its
    // shared stylesheets (`/assets/*`) from the static root, so
    // domains mapping CMS hosts require static serving. `validate_cms`
    // enforces it for the `[cms] hosts` list at load time; this is
    // the same check for rows that live only in the table (F16's
    // manual domains — the configuration list is empty, so load-time
    // validation sees nothing to reject).
    if config.cms.enabled && !cms_hosts.is_empty() && !config.static_files.enabled {
        return Err(
            "the domains table maps CMS hosts but `static.enabled` is off: the CMS host \
             borrows its shared stylesheets (`/assets/*`) from the static root — enable \
             static serving or clear the domains table"
                .into(),
        );
    }

    // A tenant organization's document root that does not exist
    // (yet) is a warning, not a boot failure: the row is data, and
    // the tree simply answers 404s until the directory appears — no
    // restart needed once it does, because serving hits the
    // filesystem per request. Only the root's absolute shape is
    // frozen at boot, exactly like every other serving root. The
    // bootstrap organizations (`main`, `cms`) are excluded: their
    // roots are the configuration's, and the checks above already
    // refused a missing static root / views directory.
    let mut warned: Vec<&str> = Vec::new();
    for binding in &bindings {
        if binding.organization != db::MAIN_ORGANIZATION_KEY
            && binding.organization != db::CMS_ORGANIZATION_KEY
            && !warned.contains(&binding.organization.as_str())
            && !state::absolutize(&binding.document_root).is_dir()
        {
            tracing::warn!(
                organization = %binding.organization,
                document_root = %binding.document_root,
                "the organization's document root does not exist; its host names answer \
                 404 until the directory is created"
            );
            warned.push(binding.organization.as_str());
        }
    }

    let auth = if config.auth.enabled {
        let registration_enabled = config.auth.registration_enabled;
        Some(AuthContext {
            repository: Arc::new(SqliteUserRepository::new(pool.clone())),
            jwt: JwtService::new(
                &config.auth.jwt_secret,
                &config.auth.issuer,
                config.auth.token_ttl_secs,
            ),
            registration_enabled,
            min_password_len: config.auth.min_password_len,
            refresh_tokens_enabled: config.auth.refresh_tokens_enabled,
            refresh_token_ttl_secs: config.auth.refresh_token_ttl_secs,
        })
    } else {
        None
    };

    // The CMS mounts when enabled AND its prerequisites (database,
    // auth, templates) are on — `validate_cms` enforces the same rule
    // at load time, so the silent skip here only guards embedders that
    // build states directly.
    //
    // The media directory (F9) is resolved and created right here:
    // request handling only ever joins flat server-generated names onto
    // the frozen absolute path, and a missing directory is a startup
    // problem (fail fast), never a per-upload one.
    let cms = match (auth.is_some(), config.cms.enabled, config.templates.enabled) {
        (true, true, true) => {
            let media_root = state::absolutize(&config.cms.media_dir);
            if let Err(error) = std::fs::create_dir_all(&media_root) {
                return Err(format!(
                    "failed to create the media directory `{}`: {error}",
                    media_root.display()
                )
                .into());
            }
            Some(CmsContext {
                // F11: the repository carries the revision cap the
                // history prunes to on every save.
                pages: Arc::new(SqlitePageRepository::new(
                    pool.clone(),
                    config.cms.max_revisions,
                )),
                menus: Arc::new(SqliteMenuRepository::new(pool.clone())),
                media: Arc::new(SqliteMediaRepository::new(pool)),
                media_root,
            })
        }
        _ => None,
    };
    if config.cms.enabled && cms.is_none() {
        tracing::warn!(
            "cms.enabled is set but database/auth/templates are off; the CMS stays unmounted"
        );
    }

    // Uploads are multipart bodies: the request-body limit has to make
    // room for the file plus the framing, or the server answers 413
    // before the friendly form error can fire (F9).
    if cms.is_some()
        && config.cms.media_max_bytes + 1_024 > config.server.max_body_size_bytes as u64
    {
        tracing::warn!(
            media_max_bytes = config.cms.media_max_bytes,
            max_body_size_bytes = config.server.max_body_size_bytes,
            "cms media_max_bytes is at or above the request body limit; uploads near the cap \
             will be rejected with 413 — raise server.max_body_size_bytes alongside"
        );
    }

    // A default page without a mounted CMS is a configuration smell:
    // accepted (validate_cms only enforces the slug shape) but called
    // out loudly, because the homepage quietly keeps its normal
    // behaviour otherwise.
    if config.cms.default_page.is_some() && cms.is_none() {
        tracing::warn!(
            "cms.default_page is set but the CMS is not mounted; GET / keeps its normal \
             behaviour"
        );
    }

    if config.auth.enabled {
        tracing::info!(
            registration_enabled = config.auth.registration_enabled,
            token_ttl_secs = config.auth.token_ttl_secs,
            refresh_tokens_enabled = config.auth.refresh_tokens_enabled,
            refresh_token_ttl_secs = config.auth.refresh_token_ttl_secs,
            "authentication enabled (the first registered user becomes the admin)"
        );
    }

    if cms.is_some() {
        tracing::info!(
            "cms enabled (public pages at /p, admin panel at /admin, menus at /admin/menus, \
             sitemap at /sitemap.xml while [cms] sitemap; media library at /admin/media \
             serving /media/{{id}}/{{name}}, uploads capped by [cms] media_max_bytes; search \
             at /search and the admin page filter, feeds at /feed.xml and /atom.xml while \
             [cms] feed, listings paginated with [cms] index_page_size; content, media and \
             users — server configuration stays in wallermax.toml)"
        );
        if let Some(slug) = config.cms.default_page.as_deref() {
            tracing::info!(
                slug,
                "cms default page takes over GET / (a missing slug warns and falls back)"
            );
        }
    }

    if !bindings.is_empty() {
        let resolved: Vec<String> = bindings
            .iter()
            .map(|binding| {
                format!(
                    "{} -> {} ({})",
                    binding.hostname, binding.organization, binding.document_root
                )
            })
            .collect();
        tracing::info!(
            bindings = ?resolved,
            "virtual hosts active (F17): each mapped Host name serves its organization's \
             tree — the domains table joined to the organizations' document roots — and \
             every other host (unknown or missing included) gets the main tree: same IP, \
             same port"
        );
    }

    if config.metrics.enabled {
        tracing::info!(
            path = %config.metrics.path,
            "prometheus metrics enabled"
        );
    }

    let state = AppState::with_vhosts(
        config.clone(),
        auth,
        cms,
        state::VhostData::from_database(config, &document_roots, bindings),
    );

    // The strict sidecar backend fails fast: a server configured for
    // Node-side rendering must not start without the sidecar (the
    // `auto` backend has already fallen back to boa with a warning).
    if let Some(templates) = state.templates() {
        if let Err(error) = templates.ensure_ready() {
            return Err(error.into());
        }
    }

    // Endpoints that failed to resolve (a `${ENV}` variable that is not
    // set, a header value the HTTP layer rejects) disable the proxy in
    // `AppState::build` with an error log — for the real binary that
    // silent degradation is turned into a hard startup failure instead:
    // a proxy running without its secrets is worse than no proxy.
    ensure_external_api_resolved(config, &state)?;

    if state.external_api_enabled() {
        let names: Vec<&str> = config
            .external_api
            .endpoints
            .iter()
            .map(|endpoint| endpoint.name.as_str())
            .collect();
        tracing::info!(
            endpoints = %names.join(", "),
            "external API proxy enabled (GET/POST /api/ext/<name>[/<subpath>]; secrets stay server-side)"
        );
    }

    Ok(state)
}

/// Turns a resolved-away `[external_api]` section into a hard startup
/// failure (see `build_state`): `AppState::build` already logged the
/// exact cause, so the error only points at the log.
fn ensure_external_api_resolved(config: &AppConfig, state: &AppState) -> Result<(), ServerError> {
    if !config.external_api.endpoints.is_empty() && !state.external_api_enabled() {
        return Err(String::from(
            "external_api endpoints are configured but failed to resolve; check the \
             startup log for the exact header or ${ENV} variable at fault",
        )
        .into());
    }
    Ok(())
}

/// Runs the server until a shutdown signal is received.
///
/// On `SIGINT` (Ctrl-C) or `SIGTERM` the server stops accepting new
/// connections and waits up to `server.shutdown_timeout_secs` for in-flight
/// requests to complete; any connections still open after that are aborted.
///
/// The peer address of each connection is captured and made available to
/// middleware and handlers (used by the rate limiter to key buckets).
///
/// With `[tls] enabled = true` the socket speaks HTTPS (rustls) and an
/// optional `[tls] http_listen` address serves plain HTTP that redirects
/// to it.
///
/// # Errors
///
/// Returns an error if the state cannot be built (database, static root,
/// TLS material) or the address cannot be bound.
pub async fn serve(config: AppConfig) -> Result<(), ServerError> {
    let addr = config.server.socket_addr();
    let state = build_state(&config).await?;
    let app = build_app(&config, state);

    if config.tls.enabled {
        serve_tls(config, app, addr).await
    } else {
        serve_plain(config, app, addr).await
    }
}

/// Checks the `[templates]` startup requirements: while enabled, the
/// views directory must exist (mirroring the `[static]` root check).
fn validate_templates(config: &AppConfig) -> Result<(), String> {
    if !config.templates.enabled {
        return Ok(());
    }
    let views = Path::new(&config.templates.views_dir);
    if !views.is_dir() {
        return Err(format!(
            "template rendering is enabled but the views directory `{}` does not exist; \
             create it or point `templates.views_dir` at an existing directory",
            config.templates.views_dir
        ));
    }
    tracing::info!(
        views_dir = %config.templates.views_dir,
        backend = %config.templates.backend,
        auto_escape = config.templates.auto_escape,
        cache = config.templates.cache,
        require = config.templates.require_enabled,
        modules_dir = %config.templates.modules_dir,
        "dynamic template rendering enabled"
    );
    Ok(())
}

/// Log line describing the mounted route families.
fn route_map(config: &AppConfig, auth_enabled: bool, tls: bool) -> String {
    let vhosts = !config.cms.hosts.is_empty();
    let mut routes = String::new();
    if config.cms.enabled && config.cms.default_page.is_some() && !vhosts {
        routes.push_str("GET / (cms)  |  GET /api  |  ");
    } else if config.static_files.enabled {
        routes.push_str("GET / (static)  |  GET /api  |  ");
    } else {
        routes.push_str("GET /  |  GET /api  |  ");
    }
    routes.push_str("GET /health  |  GET /api/stats  |  POST /api/echo");
    if config.metrics.enabled {
        routes.push_str("  |  GET /metrics");
    }
    if auth_enabled {
        routes
            .push_str("  |  POST /api/auth/register  |  POST /api/auth/login  |  GET /api/auth/me");
        if config.auth.refresh_tokens_enabled {
            routes.push_str(
                "  |  POST /api/auth/refresh  |  POST /api/auth/logout  |  \
                 POST /api/auth/logout_all",
            );
        }
        routes.push_str("  |  GET /api/admin/users");
    }
    if config.static_files.enabled {
        routes.push_str("  |  + static files");
    }
    if config.templates.enabled {
        routes.push_str("  |  + .jhs templates");
    }
    if vhosts {
        routes.push_str(&format!(
            "  |  + CMS vhost{} on {}",
            if config.cms.hosts.len() == 1 { "" } else { "s" },
            config.cms.hosts.join(", ")
        ));
    }
    if let Some(listen) = config.tls.http_listen.as_deref() {
        if tls {
            routes.push_str(&format!("  |  HTTP {listen} redirects to HTTPS"));
        }
    }
    routes
}

/// Plain-HTTP serving loop (see [`serve`]).
async fn serve_plain(config: AppConfig, app: Router, addr: SocketAddr) -> Result<(), ServerError> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    tracing::info!(
        address = %local_addr,
        version = env!("CARGO_PKG_VERSION"),
        "wallermax-server listening"
    );
    tracing::info!(routes = route_map(&config, true, false), "route map ready");

    // Becomes `true` once the shutdown signal has been received; the
    // hard-timeout watchdog below watches this channel.
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    let server = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, service)
            .with_graceful_shutdown(shutdown_signal(shutdown_tx))
            .await
        {
            tracing::error!(%error, "server task failed");
        }
    });
    let abort_handle = server.abort_handle();

    // Graceful completion wins; otherwise the hard timeout aborts the task.
    tokio::select! {
        result = server => {
            match result {
                Ok(()) => {}
                Err(join_error) if join_error.is_panic() => {
                    tracing::error!("server task panicked");
                }
                Err(_) => {
                    tracing::debug!("server task was cancelled");
                }
            }
            tracing::info!("server stopped");
        }
        _ = forced_shutdown(&mut shutdown_rx, config.server.shutdown_timeout_secs) => {
            tracing::warn!(
                timeout_secs = config.server.shutdown_timeout_secs,
                "graceful shutdown timed out; aborting remaining connections"
            );
            abort_handle.abort();
        }
    }

    Ok(())
}

/// HTTPS serving loop (see [`serve`]).
///
/// Binds the TLS listener eagerly (so bind errors surface at startup and
/// port `0` resolves to a usable address), serves the application through
/// `axum-server` + rustls, and optionally runs a parallel plain-HTTP
/// listener that redirects every request to its HTTPS equivalent.
async fn serve_tls(config: AppConfig, app: Router, addr: SocketAddr) -> Result<(), ServerError> {
    for (key, path) in [
        ("cert_path", &config.tls.cert_path),
        ("key_path", &config.tls.key_path),
    ] {
        if !Path::new(path.as_str()).is_file() {
            return Err(format!("tls is enabled but {key} `{path}` is not a readable file").into());
        }
    }

    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        &config.tls.cert_path,
        &config.tls.key_path,
    )
    .await
    .map_err(|error| {
        format!(
            "failed to load the TLS material from `{}` / `{}`: {error}",
            config.tls.cert_path, config.tls.key_path
        )
    })?;

    let listener = std::net::TcpListener::bind(addr)?;
    let tls_addr = listener.local_addr()?;

    tracing::info!(
        address = %tls_addr,
        version = env!("CARGO_PKG_VERSION"),
        "wallermax-server listening (HTTPS, TLS 1.2/1.3)"
    );
    tracing::info!(routes = route_map(&config, true, true), "route map ready");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut shutdown_rx_graceful = shutdown_rx.clone();
    let shutdown_rx_redirect = shutdown_rx.clone();
    let mut shutdown_rx_forced = shutdown_rx;

    // Signal listener: flips the channel on CTRL-C / SIGTERM.
    tokio::spawn(shutdown_signal(shutdown_tx));

    // Axum-server graceful shutdown watcher: once the signal fires, let
    // in-flight connections finish within the configured timeout.
    let handle = axum_server::Handle::new();
    let graceful_handle = handle.clone();
    let graceful_timeout = Duration::from_secs(config.server.shutdown_timeout_secs);
    tokio::spawn(async move {
        wait_for_shutdown(&mut shutdown_rx_graceful).await;
        tracing::info!("starting graceful shutdown");
        graceful_handle.graceful_shutdown(Some(graceful_timeout));
    });

    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    let serving_handle = handle.clone();
    let tls_task = tokio::spawn(async move {
        let server =
            axum_server::tls_rustls::from_tcp_rustls(listener, tls_config).handle(serving_handle);
        if let Err(error) = server.serve(service).await {
            tracing::error!(%error, "https server task failed");
        }
    });
    let abort_handle = tls_task.abort_handle();

    // Optional plain-HTTP listener redirecting to the HTTPS port.
    let mut redirect_task = None;
    if let Some(listen) = config.tls.http_listen.as_deref() {
        let http_addr: SocketAddr = listen.parse().expect("validated at configuration load");
        let redirect = redirect_router(tls_addr);
        match tokio::net::TcpListener::bind(http_addr).await {
            Ok(listener) => {
                tracing::info!(
                    address = %http_addr,
                    https = %tls_addr,
                    "plain HTTP listener redirecting to HTTPS"
                );
                let mut rx = shutdown_rx_redirect.clone();
                redirect_task = Some(tokio::spawn(async move {
                    if let Err(error) = axum::serve(listener, redirect.into_make_service())
                        .with_graceful_shutdown(async move {
                            // Owns the receiver: `with_graceful_shutdown`
                            // needs a 'static future.
                            while !*rx.borrow_and_update() {
                                if rx.changed().await.is_err() {
                                    return;
                                }
                            }
                        })
                        .await
                    {
                        tracing::error!(%error, "http redirect task failed");
                    }
                }));
            }
            Err(error) => {
                return Err(format!(
                    "failed to bind the HTTP redirect listener `{http_addr}`: {error}"
                )
                .into());
            }
        }
    }

    // Graceful completion wins; otherwise the hard timeout forces the
    // axum-server handle to drop every connection.
    tokio::select! {
        result = tls_task => {
            match result {
                Ok(()) => {}
                Err(join_error) if join_error.is_panic() => {
                    tracing::error!("https server task panicked");
                }
                Err(_) => {
                    tracing::debug!("https server task was cancelled");
                }
            }
            tracing::info!("server stopped");
        }
        _ = forced_shutdown(&mut shutdown_rx_forced, config.server.shutdown_timeout_secs) => {
            tracing::warn!(
                timeout_secs = config.server.shutdown_timeout_secs,
                "graceful shutdown timed out; aborting remaining connections"
            );
            handle.shutdown();
            abort_handle.abort();
        }
    }
    if let Some(task) = redirect_task.take() {
        task.abort();
    }

    Ok(())
}

/// Builds the router of the redirect listener: every request and method
/// answers `308 Permanent Redirect` pointing at the HTTPS equivalent.
///
/// Public so integration tests (and embedders running their own
/// redirect listener) can reuse the exact production behaviour.
pub fn redirect_router(tls_addr: SocketAddr) -> Router {
    Router::new().fallback(move |request: Request| async move {
        let target = redirect_target(&request, &tls_addr);
        Response::builder()
            .status(StatusCode::PERMANENT_REDIRECT)
            .header(header::LOCATION, target)
            .body(axum::body::Body::empty())
            .unwrap_or_else(|error| {
                tracing::error!(%error, "failed to build the redirect response");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })
    })
}

/// Absolute HTTPS target of the redirect: `https://<host>:<tls port><path>`.
///
/// The host comes from the request's `Host` header (any explicit port is
/// replaced with the TLS port, since the request arrived on the plain
/// listener); without a `Host` header the TLS socket address is used.
pub fn redirect_target(request: &Request, tls_addr: &SocketAddr) -> String {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let authority = https_authority(host, tls_addr);
    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    format!("https://{authority}{path}")
}

/// Builds the `host:port` authority of the HTTPS target, dropping any
/// port the `Host` header carried and appending the TLS port (omitted
/// on the default 443).
fn https_authority(host_header: Option<&str>, tls_addr: &SocketAddr) -> String {
    let host = host_header
        .map(strip_port)
        .unwrap_or_else(|| tls_addr.ip().to_string());

    if tls_addr.port() == 443 {
        host
    } else if host.contains(':') {
        // Bare IPv6 literal: needs brackets before the port.
        format!("[{host}]:{}", tls_addr.port())
    } else {
        format!("{host}:{}", tls_addr.port())
    }
}

/// Strips an explicit `:port` (or the bracketed-IPv6 port) from a
/// `Host` value, keeping the host part.
fn strip_port(host: &str) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8080` or `[::1]`
        if let Some(end) = rest.find(']') {
            return rest[..end].to_owned();
        }
        return host.to_owned();
    }
    if host.matches(':').count() == 1 {
        // `example.com:8080`
        if let Some((host_part, _)) = host.rsplit_once(':') {
            return host_part.to_owned();
        }
    }
    host.to_owned()
}

/// Resolves when the process receives `SIGINT` (Ctrl-C) or `SIGTERM`.
///
/// Notifies `tx` so the hard-timeout watchdog in [`serve`] can engage.
async fn shutdown_signal(tx: watch::Sender<bool>) {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install the CTRL-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};

        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to install the SIGTERM handler");
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("CTRL-C received, starting graceful shutdown"),
        _ = terminate => tracing::info!("SIGTERM received, starting graceful shutdown"),
    }

    let _ = tx.send(true);
}

/// Resolves once the shutdown channel flips to `true`.
async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Waits for the shutdown signal, then for the hard timeout to expire.
async fn forced_shutdown(rx: &mut watch::Receiver<bool>, timeout_secs: u64) {
    wait_for_shutdown(rx).await;
    tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_port_handles_the_common_host_shapes() {
        assert_eq!(strip_port("example.com"), "example.com");
        assert_eq!(strip_port("example.com:8080"), "example.com");
        assert_eq!(strip_port("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(strip_port("[::1]:8080"), "::1");
        assert_eq!(strip_port("[::1]"), "::1");
        // Bare IPv6 (invalid in Host, but harmless).
        assert_eq!(strip_port("::1"), "::1");
    }

    #[test]
    fn https_authority_appends_the_tls_port() {
        let tls_addr: SocketAddr = "127.0.0.1:8443".parse().expect("valid address");

        assert_eq!(
            https_authority(Some("127.0.0.1:8080"), &tls_addr),
            "127.0.0.1:8443"
        );
        assert_eq!(
            https_authority(Some("example.com"), &tls_addr),
            "example.com:8443"
        );
        assert_eq!(https_authority(Some("[::1]:80"), &tls_addr), "[::1]:8443");
        assert_eq!(https_authority(None, &tls_addr), "127.0.0.1:8443");
    }

    #[test]
    fn https_authority_omits_the_default_port() {
        let tls_addr: SocketAddr = "203.0.113.9:443".parse().expect("valid address");

        assert_eq!(
            https_authority(Some("example.com:80"), &tls_addr),
            "example.com"
        );
    }
}
