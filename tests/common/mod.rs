//! Shared test scaffolding for the integration suites.
//!
//! Each test binary declares `mod common;` and gets the same [`TestServer`]
//! used to boot the *real* server stack (routes + middleware, exactly as
//! the binary serves it) on an ephemeral port.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use wallermax_server::config::AppConfig;
use wallermax_server::server::{build_app, build_app_with_routes, build_state};
use wallermax_server::state::AppState;

/// A test server instance; its task is aborted when dropped.
pub struct TestServer {
    base_url: String,
    abort_handle: tokio::task::AbortHandle,
}

impl TestServer {
    /// Boots the default application stack on an ephemeral port.
    pub async fn start() -> Self {
        Self::start_with_config(AppConfig::default()).await
    }

    /// Boots the application with a custom configuration.
    pub async fn start_with_config(config: AppConfig) -> Self {
        let app = build_app(&config, AppState::new(config.clone()));
        Self::spawn(app).await
    }

    /// Boots the **full** application (database pool, migrations and
    /// authentication included) exactly as the binary would.
    ///
    /// Use with a config where `[database]` and `[auth]` are enabled; pair
    /// it with [`auth_config`] to get a fresh temporary database.
    pub async fn start_full(config: AppConfig) -> Self {
        let state = build_state(&config)
            .await
            .expect("application state builds");
        let app = build_app(&config, state);
        Self::spawn(app).await
    }

    /// Boots the application with a custom configuration **and route tree**
    /// (used to inject handlers with controlled behaviour).
    pub async fn start_with_router(config: AppConfig, router: Router<AppState>) -> Self {
        let app = build_app_with_routes(&config, AppState::new(config.clone()), router);
        Self::spawn(app).await
    }

    /// Serves the built application on an ephemeral port.
    async fn spawn(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port binds");
        let addr: SocketAddr = listener.local_addr().expect("local address");

        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("server runs until aborted");
        });

        Self {
            base_url: format!("http://{addr}"),
            abort_handle: task.abort_handle(),
        }
    }

    /// Builds a full URL for `path`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

/// SQLite URL for a fresh temporary file database.
///
/// The URL uses forward slashes so it stays valid on Windows as well; the
/// database files (`*.db`, `*-wal`, `*-shm`) are best-effort removed by
/// [`TempDbGuard`].
pub fn temp_sqlite_url() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "wallermax-auth-tests-{}-{unique}.db",
        std::process::id()
    ));
    format!(
        "sqlite://{}?mode=rwc",
        path.display().to_string().replace('\\', "/")
    )
}

/// Removes the files backing a [`temp_sqlite_url`] on drop.
pub struct TempDbGuard {
    url: String,
}

impl TempDbGuard {
    /// Registers a fresh temporary database and returns its URL.
    pub fn new() -> Self {
        Self {
            url: temp_sqlite_url(),
        }
    }

    /// The SQLite URL to configure.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Default for TempDbGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDbGuard {
    fn drop(&mut self) {
        let Some(file) = self
            .url
            .strip_prefix("sqlite://")
            .and_then(|rest| rest.split('?').next())
        else {
            return;
        };
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{file}{suffix}"));
        }
    }
}

/// A config with `[database]` and `[auth]` enabled, backed by a fresh
/// temporary database that is removed when dropped.
///
/// Panics (via the caller's own assertions) are irrelevant here: the guard
/// cleans up on any unwind.
pub fn auth_config() -> (AppConfig, TempDbGuard) {
    let db = TempDbGuard::new();
    let mut config = AppConfig::default();
    config.database.enabled = true;
    config.database.url = db.url().to_owned();
    config.database.max_connections = 2;
    config.auth.enabled = true;
    config.auth.jwt_secret = String::from("integration-test-secret-0123456789abcdef0123");
    (config, db)
}

/// A `reqwest` client that accepts the self-signed test certificates
/// (rustls backend, invalid certs and hostnames allowed).
pub fn tls_test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_rustls_tls()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .expect("test client builds")
}

/// A test server instance serving the application **over TLS** with a
/// freshly generated self-signed certificate.
///
/// Mirrors the production TLS path (`axum-server` + rustls over the full
/// application stack, connect info included); aborts on drop.
pub struct TlsTestServer {
    base_url: String,
    abort_handle: tokio::task::AbortHandle,
}

impl TlsTestServer {
    /// Boots the full application over HTTPS on an ephemeral port.
    pub async fn start_full(config: AppConfig) -> Self {
        let state = build_state(&config)
            .await
            .expect("application state builds");
        let app = build_app(&config, state);

        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert");
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
            certified.cert.pem().into_bytes(),
            certified.signing_key.serialize_pem().into_bytes(),
        )
        .await
        .expect("rustls config builds");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port binds");
        let addr: SocketAddr = listener.local_addr().expect("local address");

        let task = tokio::spawn(async move {
            let server = axum_server::tls_rustls::from_tcp_rustls(listener, rustls_config);
            if let Err(error) = server
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
            {
                panic!("tls server failed: {error}");
            }
        });

        Self {
            base_url: format!("https://{addr}"),
            abort_handle: task.abort_handle(),
        }
    }

    /// Builds a full URL for `path`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

impl Drop for TlsTestServer {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}
