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
use crate::db::{self, SqliteUserRepository};
use crate::routes;
use crate::state::{AppState, AuthContext};

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
    let router = routes::routes(
        state.auth_enabled(),
        refresh_enabled,
        &config.static_files,
        &config.metrics,
    );
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

    if !config.database.enabled {
        return Ok(AppState::new(config.clone()));
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
    tracing::info!(
        url = %config.database.url,
        max_connections = config.database.max_connections,
        "sqlite pool ready (migrations applied)"
    );

    let auth = if config.auth.enabled {
        let registration_enabled = config.auth.registration_enabled;
        Some(AuthContext {
            repository: Arc::new(SqliteUserRepository::new(pool)),
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

    if config.auth.enabled {
        tracing::info!(
            registration_enabled = config.auth.registration_enabled,
            token_ttl_secs = config.auth.token_ttl_secs,
            refresh_tokens_enabled = config.auth.refresh_tokens_enabled,
            refresh_token_ttl_secs = config.auth.refresh_token_ttl_secs,
            "authentication enabled (the first registered user becomes the admin)"
        );
    }

    if config.metrics.enabled {
        tracing::info!(
            path = %config.metrics.path,
            "prometheus metrics enabled"
        );
    }

    let state = match auth {
        Some(auth) => AppState::with_auth(config.clone(), auth),
        None => AppState::new(config.clone()),
    };

    Ok(state)
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

/// Log line describing the mounted route families.
fn route_map(config: &AppConfig, auth_enabled: bool, tls: bool) -> String {
    let mut routes = String::new();
    if config.static_files.enabled {
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
