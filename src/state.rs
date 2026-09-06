//! Shared application state.
//!
//! [`AppState`] is a cheap, cloneable handle (internally an `Arc`) shared by
//! all request handlers and middleware. It carries:
//!
//! - the validated [`AppConfig`], so middleware and handlers can read their
//!   tuning values;
//! - the [`RateLimiter`] shared by all workers;
//! - the precomputed security header pairs (built once from the
//!   configuration, so applying them per response only clones small values);
//! - the precomputed trusted proxy networks (see [`crate::proxy`]);
//! - the [`AuthContext`] (user repository + token service) when the `[auth]`
//!   feature is enabled;
//! - the [`Metrics`] registry when the `[metrics]` feature is enabled;
//! - runtime metrics (uptime, request and rejection counters).
//!
//! Future phases can extend the inner struct with caches and other
//! services without changing any call sites.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{HeaderName, HeaderValue};

use crate::auth::JwtService;
use crate::config::AppConfig;
use crate::db::UserRepository;
use crate::metrics::Metrics;
use crate::proxy::{self, Cidr};
use crate::rate_limit::RateLimiter;

/// Authentication services shared by handlers when `[auth]` is enabled.
///
/// Built once at startup (see [`crate::server::build_state`]) and immutable
/// afterwards.
pub struct AuthContext {
    /// User storage behind the [`UserRepository`] abstraction.
    pub repository: Arc<dyn UserRepository>,
    /// Token signing and verification service.
    pub jwt: JwtService,
    /// Whether `POST /api/auth/register` accepts new users.
    pub registration_enabled: bool,
    /// Minimum accepted password length (from `[auth]`).
    pub min_password_len: usize,
    /// Whether login responses carry refresh tokens and the
    /// `/api/auth/refresh`, `/api/auth/logout` and `/api/auth/logout_all`
    /// endpoints are mounted (from `[auth] refresh_tokens_enabled`).
    pub refresh_tokens_enabled: bool,
    /// Refresh token lifetime in seconds (from `[auth]`).
    pub refresh_token_ttl_secs: u64,
}

/// Cheap-to-clone shared application state.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<StateInner>,
}

struct StateInner {
    config: AppConfig,
    started_at: Instant,
    requests_served: AtomicU64,
    rate_limited_requests: AtomicU64,
    rate_limiter: RateLimiter,
    security_headers: Vec<(HeaderName, HeaderValue)>,
    trusted_proxies: Vec<Cidr>,
    auth: Option<AuthContext>,
    metrics: Option<Metrics>,
}

impl AppState {
    /// Creates a fresh application state from a validated configuration,
    /// **without** authentication services (previous-phase behaviour).
    pub fn new(config: AppConfig) -> Self {
        Self::build(config, None)
    }

    /// Creates a fresh application state with the authentication services
    /// attached (see [`AuthContext`]).
    pub fn with_auth(config: AppConfig, auth: AuthContext) -> Self {
        Self::build(config, Some(auth))
    }

    /// Shared constructor for the public builders.
    fn build(config: AppConfig, auth: Option<AuthContext>) -> Self {
        let security_headers = config.security_headers.header_pairs();
        let rate_limiter = RateLimiter::new(
            config.rate_limit.capacity,
            config.rate_limit.refill_per_second,
        );
        let trusted_proxies = proxy::Cidr::parse_all(&config.server.trusted_proxies);
        if !config.server.trusted_proxies.is_empty() && trusted_proxies.is_empty() {
            // Cannot happen: validation rejects unparsable entries. The
            // guard keeps a future regression from silently ignoring the
            // configuration.
            tracing::warn!(
                "server.trusted_proxies produced no usable networks; falling back to peer IPs"
            );
        }

        let metrics = if config.metrics.enabled {
            match Metrics::new(auth.is_some()) {
                Ok(metrics) => Some(metrics),
                Err(error) => {
                    // Programming error (duplicate registration); metrics
                    // are disabled rather than aborting the process.
                    tracing::error!(%error, "failed to build the metrics registry");
                    None
                }
            }
        } else {
            None
        };

        Self {
            inner: Arc::new(StateInner {
                config,
                started_at: Instant::now(),
                requests_served: AtomicU64::new(0),
                rate_limited_requests: AtomicU64::new(0),
                rate_limiter,
                security_headers,
                trusted_proxies,
                auth,
                metrics,
            }),
        }
    }

    /// Returns the application configuration.
    pub fn config(&self) -> &AppConfig {
        &self.inner.config
    }

    /// Returns the shared rate limiter.
    pub fn rate_limiter(&self) -> &RateLimiter {
        &self.inner.rate_limiter
    }

    /// Returns the security header pairs applied to every response.
    pub fn security_headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.inner.security_headers
    }

    /// Returns the trusted proxy networks used for client IP resolution.
    pub fn trusted_proxies(&self) -> &[Cidr] {
        &self.inner.trusted_proxies
    }

    /// Returns the authentication services when the `[auth]` feature is
    /// enabled.
    pub fn auth_context(&self) -> Option<&AuthContext> {
        self.inner.auth.as_ref()
    }

    /// Whether the authentication and admin routes are mounted.
    pub fn auth_enabled(&self) -> bool {
        self.inner.auth.is_some()
    }

    /// Whether the refresh token endpoints are mounted (auth enabled and
    /// `[auth] refresh_tokens_enabled`).
    pub fn refresh_enabled(&self) -> bool {
        self.inner
            .auth
            .as_ref()
            .is_some_and(|auth| auth.refresh_tokens_enabled)
    }

    /// Returns the Prometheus metrics registry when the `[metrics]`
    /// feature is enabled (and the registry could be built).
    pub fn metrics(&self) -> Option<&Metrics> {
        self.inner.metrics.as_ref()
    }

    /// Records that a request has been served.
    pub fn record_request(&self) {
        self.inner.requests_served.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the total number of requests served since startup.
    ///
    /// The counter is maintained by the request logging middleware, so it
    /// reports zero while `[middleware] logging = false`.
    pub fn total_requests(&self) -> u64 {
        self.inner.requests_served.load(Ordering::Relaxed)
    }

    /// Records that a request was rejected by the rate limiter.
    pub fn record_rate_limited(&self) {
        self.inner
            .rate_limited_requests
            .fetch_add(1, Ordering::Relaxed);
        if let Some(metrics) = &self.inner.metrics {
            metrics.record_rate_limited();
        }
    }

    /// Returns the total number of requests rejected by the rate limiter.
    pub fn rate_limited_requests(&self) -> u64 {
        self.inner.rate_limited_requests.load(Ordering::Relaxed)
    }

    /// Returns the time elapsed since the server started.
    pub fn uptime(&self) -> Duration {
        self.inner.started_at.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(AppConfig::default())
    }

    #[test]
    fn counts_requests() {
        let state = state();
        assert_eq!(state.total_requests(), 0);

        state.record_request();
        state.record_request();

        assert_eq!(state.total_requests(), 2);
    }

    #[test]
    fn counts_rate_limited_requests() {
        let state = state();
        assert_eq!(state.rate_limited_requests(), 0);

        state.record_rate_limited();
        state.record_rate_limited();

        assert_eq!(state.rate_limited_requests(), 2);
    }

    #[test]
    fn exposes_config_and_headers() {
        let mut config = AppConfig::default();
        config.security_headers.x_frame_options = String::from("SAMEORIGIN");
        let state = AppState::new(config);

        assert_eq!(
            state.config().security_headers.x_frame_options,
            "SAMEORIGIN"
        );
        assert_eq!(state.config().server.port, 8080);

        let names: Vec<String> = state
            .security_headers()
            .iter()
            .map(|(name, _)| name.as_str().to_owned())
            .collect();
        assert!(names.contains(&"x-frame-options".to_owned()));
        assert_eq!(state.security_headers().len(), 5);
    }

    #[test]
    fn clones_share_state() {
        let state = state();
        let clone = state.clone();

        clone.record_request();

        assert_eq!(state.total_requests(), 1);
    }

    #[test]
    fn uptime_is_monotonic() {
        let state = state();
        let first = state.uptime();

        std::thread::sleep(Duration::from_millis(5));

        assert!(state.uptime() >= first);
    }

    /// Minimal repository stub so state-level tests need no database.
    struct StubRepository;

    #[async_trait::async_trait]
    impl crate::db::UserRepository for StubRepository {
        async fn create(
            &self,
            _username: &str,
            _password_hash: &str,
            _role: crate::db::UserRole,
        ) -> Result<crate::db::User, crate::db::RepositoryError> {
            Err(crate::db::RepositoryError::Internal("stub".to_owned()))
        }

        async fn find_by_id(
            &self,
            _id: i64,
        ) -> Result<Option<crate::db::User>, crate::db::RepositoryError> {
            Ok(None)
        }

        async fn find_by_username(
            &self,
            _username: &str,
        ) -> Result<Option<crate::db::User>, crate::db::RepositoryError> {
            Ok(None)
        }

        async fn count(&self) -> Result<i64, crate::db::RepositoryError> {
            Ok(0)
        }

        async fn list(
            &self,
            _limit: i64,
        ) -> Result<Vec<crate::db::User>, crate::db::RepositoryError> {
            Ok(Vec::new())
        }

        async fn record_login(&self, _id: i64) -> Result<(), crate::db::RepositoryError> {
            Ok(())
        }

        async fn save_refresh_token(
            &self,
            _token: &crate::db::NewRefreshToken,
        ) -> Result<crate::db::RefreshTokenRecord, crate::db::RepositoryError> {
            Err(crate::db::RepositoryError::Internal("stub".to_owned()))
        }

        async fn find_refresh_token_by_hash(
            &self,
            _token_hash: &str,
        ) -> Result<Option<crate::db::RefreshTokenRecord>, crate::db::RepositoryError> {
            Ok(None)
        }

        async fn rotate_refresh_token(
            &self,
            _id: i64,
            _now: i64,
        ) -> Result<bool, crate::db::RepositoryError> {
            Ok(false)
        }

        async fn revoke_refresh_token_family(
            &self,
            _family_id: i64,
            _now: i64,
        ) -> Result<(), crate::db::RepositoryError> {
            Ok(())
        }

        async fn revoke_all_refresh_tokens(
            &self,
            _user_id: i64,
            _now: i64,
        ) -> Result<(), crate::db::RepositoryError> {
            Ok(())
        }

        async fn delete_expired_refresh_tokens(
            &self,
            _now: i64,
        ) -> Result<u64, crate::db::RepositoryError> {
            Ok(0)
        }
    }

    #[test]
    fn auth_is_absent_by_default() {
        let state = state();

        assert!(!state.auth_enabled());
        assert!(state.auth_context().is_none());
    }

    fn stub_auth() -> AuthContext {
        AuthContext {
            repository: Arc::new(StubRepository),
            jwt: crate::auth::JwtService::new(
                "state-test-secret-0123456789abcdef0123456789",
                "wallermax-server",
                60,
            ),
            registration_enabled: true,
            min_password_len: 8,
            refresh_tokens_enabled: true,
            refresh_token_ttl_secs: 3600,
        }
    }

    #[test]
    fn with_auth_exposes_the_context() {
        let state = AppState::with_auth(AppConfig::default(), stub_auth());

        assert!(state.auth_enabled());
        let auth = state.auth_context().expect("auth context present");
        assert!(auth.registration_enabled);
        assert_eq!(auth.min_password_len, 8);
        assert!(auth.refresh_tokens_enabled);
        assert_eq!(auth.refresh_token_ttl_secs, 3600);
    }

    #[test]
    fn refresh_flag_follows_the_auth_context() {
        let mut auth = stub_auth();
        auth.refresh_tokens_enabled = false;
        let state = AppState::with_auth(AppConfig::default(), auth);

        assert!(state.auth_enabled());
        assert!(!state.refresh_enabled());
    }

    #[test]
    fn metrics_are_absent_by_default() {
        let state = state();

        assert!(state.metrics().is_none());
    }

    #[test]
    fn metrics_attach_when_enabled() {
        let mut config = AppConfig::default();
        config.metrics.enabled = true;
        let state = AppState::new(config);

        let metrics = state.metrics().expect("metrics present");
        // Vec families only appear once a labelled child exists.
        metrics.record_request("GET", 200, 0.001);
        let body = metrics.render(1.0, None).expect("renders");
        assert!(body.contains("wallermax_requests_total"));
    }

    #[test]
    fn trusted_proxies_are_parsed_into_state() {
        let mut config = AppConfig::default();
        config.server.trusted_proxies = vec!["127.0.0.1".to_owned(), "10.0.0.0/8".to_owned()];
        let state = AppState::new(config);

        assert_eq!(state.trusted_proxies().len(), 2);
    }
}
