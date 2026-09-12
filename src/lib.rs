//! # wallermax-server
//!
//! A modular, secure and high-performance web server written in Rust.
//!
//! This is the **v0.8.0 release**: on top of the Phase 4
//! production-readiness stack — layered configuration (built-in
//! defaults, `TOML` files and environment variables), a composable
//! middleware pipeline (security headers, CORS, request-id, logging,
//! rate limiting, body limits, timeouts), a JSON-first HTTP API, SQLite
//! user storage behind the [`UserRepository`](db::UserRepository)
//! abstraction, JWT authentication with roles and rotating refresh
//! tokens, static file serving, a Prometheus metrics endpoint,
//! proxy-aware client IPs and optional TLS (rustls) — and the Phase 5
//! **dynamic `.jhs` template engine** ([`template_engine`]) — it adds
//! the **small built-in CMS** ([`routes::cms`]): database-backed pages
//! rendered as sandboxed templates at `/p/{slug}`, a JavaScript-free
//! admin panel for content and accounts, login/registration modals and
//! the browser session layer of v0.7.0. See `README.md` for the full
//! feature list and the phase-by-phase roadmap.
//!
//! ## Module map
//!
//! | Module               | Responsibility                                            |
//! |----------------------|-----------------------------------------------------------|
//! | [`config`]           | Layered configuration (defaults, `TOML`, environment)     |
//! | [`state`]            | Shared application state, config, limiter, auth, CMS      |
//! | [`error`]            | Application-wide error model with JSON responses           |
//! | [`rate_limit`]       | Token-bucket rate limiter (per client IP)                  |
//! | [`proxy`]            | Trusted proxies, CIDR matching, client IP resolution       |
//! | [`auth`]             | Argon2id hashing, JWT access tokens, refresh tokens        |
//! | [`db`]               | SQLite pool, migrations, user and page repositories        |
//! | [`session`]          | Browser session cookie (`wallermax_session`)               |
//! | [`metrics`]          | Prometheus registry and exposition                        |
//! | [`extractors`]       | `AuthUser` / `AdminUser` / `JsonBody` extractors           |
//! | [`external_api`]     | `[external_api]` proxy state, `${ENV}` header resolution   |
//! | [`template_engine`]  | Sandboxed `.jhs` template rendering (`[templates]`)         |
//! | [`routes`]           | Route modules (`/`, `/api`, `/health`, auth, admin, CMS)   |
//! | [`middleware`]       | Composable request/response middleware                     |
//! | [`logging`]          | `tracing` subscriber setup                                 |
//! | [`server`]           | Server bootstrap, TLS, graceful shutdown                   |
//!
//! Every module is public so that integration tests (and future phases) can
//! compose the building blocks exactly like the binary does.

//! The crate stays `unsafe`-free with one deliberate, contained
//! exception: [`template_engine::require_bridge`] (v0.9.0) uses
//! boa's `NativeFunction::from_closure`, whose contract is argued in
//! `SAFETY` comments next to each call. The exception is scoped to
//! that module through `deny` + a module-local allow; everywhere else
//! `unsafe` remains a hard error.
#![deny(unsafe_code)]

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod external_api;
pub mod extractors;
pub mod logging;
pub mod metrics;
pub mod middleware;
pub mod proxy;
pub mod rate_limit;
pub mod routes;
pub mod server;
pub mod session;
pub mod state;
pub mod template_engine;

mod markdown;
mod media;
mod util;

/// Loads the configuration, initializes logging and runs the server until
/// a shutdown signal is received.
///
/// This is the entry point used by the `wallermax-server` binary.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded, the address
/// cannot be bound, or the serving loop fails unexpectedly.
pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = config::AppConfig::load()?;
    logging::init(&config);
    server::serve(config).await
}
