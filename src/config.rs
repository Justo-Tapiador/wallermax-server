//! Layered application configuration.
//!
//! Configuration sources are applied in order; later sources win:
//!
//! 1. Built-in defaults (see the `Default` implementations below).
//! 2. `wallermax.toml` — the versioned project configuration.
//! 3. `wallermax.local.toml` — optional personal overrides (git-ignored).
//! 4. Environment variables prefixed with `WALLERMAX_`, using `__` as the
//!    nested-key separator, e.g. `WALLERMAX_SERVER__PORT=9000`.
//!
//! The base file name (and path) can be changed with the `WALLERMAX_CONFIG`
//! environment variable, with or without the `.toml` extension.
//!
//! ## Conventions
//!
//! - `[middleware]` holds **boolean switches** for the pipeline layers.
//! - Each middleware's **tuning values** live in their own section
//!   (`[rate_limit]`, `[cors]`, `[security_headers]`, `[request_id]`).
//! - Feature areas own their section plus an `enabled` switch
//!   (`[database]`, `[auth]`).

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderName, HeaderValue};
use config::{Config, ConfigError, Environment, File};
use serde::{Deserialize, Serialize};

use crate::auth::{MAX_PASSWORD_LEN, MIN_JWT_SECRET_LEN};
use crate::proxy::parse_cidr;

/// Base name of the main configuration file (without extension).
const DEFAULT_CONFIG_FILE: &str = "wallermax";

/// Environment variable holding an alternative configuration file path.
const CONFIG_FILE_ENV: &str = "WALLERMAX_CONFIG";

/// Environment variable prefix for overrides (`WALLERMAX_SERVER__PORT`, ...).
const ENV_PREFIX: &str = "WALLERMAX";

/// Nested-key separator for environment overrides.
const ENV_SEPARATOR: &str = "__";

/// Maximum accepted request body size: 1 MiB.
const DEFAULT_MAX_BODY_SIZE_BYTES: usize = 1_048_576;

/// Root application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// TCP and network settings.
    pub server: ServerConfig,
    /// Log output settings.
    pub logging: LoggingConfig,
    /// Feature switches for the middleware pipeline.
    pub middleware: MiddlewareConfig,
    /// Correlation id policy.
    pub request_id: RequestIdConfig,
    /// Per-client-IP rate limiting.
    pub rate_limit: RateLimitConfig,
    /// Cross-origin resource sharing.
    pub cors: CorsConfig,
    /// Security response header values.
    pub security_headers: SecurityHeadersConfig,
    /// SQLite persistence layer.
    pub database: DatabaseConfig,
    /// JWT authentication and user accounts.
    pub auth: AuthConfig,
    /// Static file serving (the `[static]` section).
    #[serde(rename = "static")]
    pub static_files: StaticConfig,
    /// Prometheus metrics exposition (the `[metrics]` section).
    pub metrics: MetricsConfig,
    /// TLS (HTTPS) serving (the `[tls]` section).
    pub tls: TlsConfig,
}

/// Network and server behaviour settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address the server binds to.
    pub host: IpAddr,
    /// Port the server binds to (`0` lets the OS pick a free port).
    pub port: u16,
    /// Maximum time, in seconds, a single request may take.
    pub request_timeout_secs: u64,
    /// Maximum time, in seconds, to wait for in-flight requests during
    /// graceful shutdown before forcing the server to stop.
    pub shutdown_timeout_secs: u64,
    /// Maximum accepted request body size, in bytes
    /// (enforced by the `body_limit` middleware switch).
    pub max_body_size_bytes: usize,
    /// Reverse proxies whose `X-Forwarded-For` header is trusted.
    ///
    /// Entries are exact IPs (`"10.0.0.4"`) or CIDR blocks
    /// (`"10.0.0.0/8"`, `"fd00::/8"`). While empty (the default) the
    /// client IP used by rate limiting and audit logs is always the TCP
    /// peer address, so spoofed `X-Forwarded-For` values are ignored.
    pub trusted_proxies: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::from([127, 0, 0, 1]),
            port: 8080,
            request_timeout_secs: 15,
            shutdown_timeout_secs: 15,
            max_body_size_bytes: DEFAULT_MAX_BODY_SIZE_BYTES,
            trusted_proxies: Vec::new(),
        }
    }
}

impl ServerConfig {
    /// Returns the address the server should bind to.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

/// Log output settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Minimum log level: `trace`, `debug`, `info`, `warn` or `error`.
    /// The `RUST_LOG` environment variable takes precedence when set.
    pub level: String,
    /// Output format: `pretty`, `json` or `compact`.
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: String::from("info"),
            format: LogFormat::default(),
        }
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-friendly multi-line output (default).
    #[default]
    Pretty,
    /// One JSON object per line, for structured log collectors.
    Json,
    /// One compact single-line entry per event.
    Compact,
}

/// Middleware pipeline feature switches (see the module docs for the
/// execution order each switch controls).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct MiddlewareConfig {
    /// Attach a unique `X-Request-Id` to every request and response.
    pub request_id: bool,
    /// Structured request logging and the `X-Response-Time` header.
    pub logging: bool,
    /// Security response headers (values in `[security_headers]`).
    pub security_headers: bool,
    /// Per-request timeout (`server.request_timeout_secs`).
    pub timeout: bool,
    /// Per-client-IP rate limiting (`[rate_limit]`).
    pub rate_limit: bool,
    /// Cross-origin resource sharing (`[cors]`).
    pub cors: bool,
    /// Request body size limit (`server.max_body_size_bytes`).
    pub body_limit: bool,
}

impl Default for MiddlewareConfig {
    fn default() -> Self {
        Self {
            request_id: true,
            logging: true,
            security_headers: true,
            timeout: true,
            // Off by default in the built-in defaults so test suites are not
            // throttled; the versioned `wallermax.toml` enables it.
            rate_limit: false,
            // Off by default: only needed for browser cross-origin usage.
            cors: false,
            // On by default: rejecting oversized bodies is cheap and safe.
            body_limit: true,
        }
    }
}

/// Correlation id policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestIdConfig {
    /// How client-supplied `X-Request-Id` values are treated:
    /// - `accept`: reuse a valid client-supplied id (default);
    /// - `overwrite`: always generate a server-side id, ignoring the client.
    pub mode: RequestIdMode,
}

/// Client request-id policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestIdMode {
    /// Reuse a valid client-supplied `X-Request-Id` (default).
    #[default]
    Accept,
    /// Always generate a server-side id and ignore the client value.
    Overwrite,
}

/// Per-client-IP rate limiting (token bucket).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Bucket capacity: the maximum burst size before requests are rejected.
    pub capacity: u64,
    /// Token refill rate, in tokens per second.
    pub refill_per_second: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            capacity: 60,
            refill_per_second: 10.0,
        }
    }
}

/// Cross-origin resource sharing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Allowed origins, e.g. `["https://app.example.com"]`, or `["*"]` to
    /// allow every origin (wildcard cannot be combined with specific
    /// origins). An empty list rejects all cross-origin browser requests.
    pub allowed_origins: Vec<String>,
    /// How long, in seconds, browsers may cache preflight responses.
    pub max_age_secs: u64,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            max_age_secs: 3600,
        }
    }
}

/// Security response header values.
///
/// Every value is a plain string for maximum configurability. An **empty
/// string omits that header** from responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityHeadersConfig {
    /// Value for `X-Content-Type-Options` (empty omits the header).
    pub x_content_type_options: String,
    /// Value for `X-Frame-Options` (empty omits the header).
    pub x_frame_options: String,
    /// Value for `Referrer-Policy` (empty omits the header).
    pub referrer_policy: String,
    /// Value for `Content-Security-Policy` (empty omits the header).
    pub content_security_policy: String,
    /// Value for `Strict-Transport-Security` (empty omits the header).
    pub strict_transport_security: String,
}

impl Default for SecurityHeadersConfig {
    fn default() -> Self {
        Self {
            x_content_type_options: String::from("nosniff"),
            x_frame_options: String::from("DENY"),
            referrer_policy: String::from("no-referrer"),
            content_security_policy: String::from("default-src 'none'; frame-ancestors 'none'"),
            strict_transport_security: String::from("max-age=31536000; includeSubDomains"),
        }
    }
}

/// SQLite persistence settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Enables the SQLite persistence layer (required by `[auth]`).
    pub enabled: bool,
    /// SQLite connection URL, e.g. `sqlite://wallermax.db?mode=rwc`
    /// (the `mode=rwc` flag creates the file when missing) or
    /// `sqlite::memory:` for an ephemeral in-process database.
    pub url: String,
    /// Maximum number of pool connections. SQLite serializes writes, so
    /// small pools are usually enough.
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::from("sqlite://wallermax.db?mode=rwc"),
            max_connections: 5,
        }
    }
}

/// JWT authentication settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Enables the `/api/auth/*` and `/api/admin/*` route families.
    /// Requires `database.enabled = true`.
    pub enabled: bool,
    /// Secret used to sign and verify access tokens (HMAC-SHA256).
    ///
    /// Must be at least 32 characters. In production, provide it through
    /// the `WALLERMAX_AUTH__JWT_SECRET` environment variable or the
    /// git-ignored `wallermax.local.toml` — never commit a real secret.
    pub jwt_secret: String,
    /// Access token lifetime, in seconds.
    pub token_ttl_secs: u64,
    /// Expected value of the token `iss` (issuer) claim.
    pub issuer: String,
    /// Whether `POST /api/auth/register` accepts new users.
    ///
    /// The **first** registered account bootstraps the `admin` role;
    /// later accounts are regular `user` accounts.
    pub registration_enabled: bool,
    /// Minimum accepted password length.
    pub min_password_len: usize,
    /// Enables long-lived refresh tokens: login and refresh responses
    /// carry one, `POST /api/auth/refresh` rotates it and
    /// `POST /api/auth/logout` / `logout_all` revoke.
    pub refresh_tokens_enabled: bool,
    /// Refresh token lifetime, in seconds (default: 30 days).
    pub refresh_token_ttl_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            jwt_secret: String::new(),
            token_ttl_secs: 3600,
            issuer: String::from("wallermax-server"),
            registration_enabled: true,
            min_password_len: 8,
            refresh_tokens_enabled: true,
            // 30 days, the common default for web sessions.
            refresh_token_ttl_secs: 2_592_000,
        }
    }
}

/// Static file serving settings.
///
/// While `enabled`, `GET /` answers with `root_dir/index_file` and any
/// request path that matches a file under `root_dir` is served from disk
/// (see `src/routes/static_files.rs`). The JSON service index remains
/// available at `GET /api`, and unmatched paths keep answering the
/// standard JSON 404 envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StaticConfig {
    /// Enables static file serving for otherwise unmatched paths.
    pub enabled: bool,
    /// Directory holding the static assets, relative to the current
    /// working directory (absolute paths are allowed too). The directory
    /// must exist at startup when static serving is enabled.
    pub root_dir: String,
    /// File served for `GET /`, looked up inside `root_dir`. Directory
    /// requests (e.g. `/docs/`) always resolve to `index.html` inside
    /// that directory.
    pub index_file: String,
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            root_dir: String::from("public"),
            index_file: String::from("index.html"),
        }
    }
}

/// Prometheus metrics settings.
///
/// While enabled, `GET <path>` (default `/metrics`) serves all collected
/// metrics in the Prometheus text exposition format. The request/response
/// counters are maintained by the request logging middleware, so they are
/// only recorded while `[middleware] logging = true` (the same caveat as
/// `GET /api/stats`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Enables the Prometheus exposition endpoint.
    pub enabled: bool,
    /// Path of the exposition endpoint (must start with `/` and contain no
    /// `..` segments).
    pub path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: String::from("/metrics"),
        }
    }
}

/// TLS (HTTPS) serving settings.
///
/// While enabled, the server binds `[server] host:port` with TLS using
/// the PEM certificate and key at `cert_path` / `key_path` (rustls,
/// ring provider; TLS 1.2 and 1.3). The optional `http_listen` address
/// runs a plain-HTTP listener in parallel that answers every request
/// with a `308` redirect to the HTTPS equivalent — handy when terminating
/// nothing in front and moving users from `http://` to `https://`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// Enables TLS serving on `[server] host:port`.
    pub enabled: bool,
    /// PEM certificate chain path (required when enabled).
    pub cert_path: String,
    /// PEM private key path (required when enabled).
    pub key_path: String,
    /// Optional `host:port` plain-HTTP listener that redirects every
    /// request to its HTTPS equivalent with `308 Permanent Redirect`.
    pub http_listen: Option<String>,
}

impl SecurityHeadersConfig {
    /// Builds the validated header pairs applied to every response.
    ///
    /// Empty values are skipped; invalid values cannot appear here because
    /// [`AppConfig::validate`] rejects them at load time.
    pub fn header_pairs(&self) -> Vec<(HeaderName, HeaderValue)> {
        let entries = [
            (
                "x-content-type-options",
                self.x_content_type_options.as_str(),
            ),
            ("x-frame-options", self.x_frame_options.as_str()),
            ("referrer-policy", self.referrer_policy.as_str()),
            (
                "content-security-policy",
                self.content_security_policy.as_str(),
            ),
            (
                "strict-transport-security",
                self.strict_transport_security.as_str(),
            ),
        ];

        entries
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .filter_map(|(name, value)| {
                let name = HeaderName::from_static(name);
                let value = HeaderValue::from_str(value).ok()?;
                Some((name, value))
            })
            .collect()
    }
}

impl AppConfig {
    /// Loads the configuration from all sources and validates it.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] if a source is malformed or the final
    /// values do not pass validation.
    pub fn load() -> Result<Self, ConfigError> {
        let config_file =
            std::env::var(CONFIG_FILE_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_FILE.to_owned());

        let config = Config::builder()
            .add_source(Config::try_from(&Self::default())?)
            .add_source(File::with_name(&config_file).required(false))
            .add_source(File::with_name("wallermax.local").required(false))
            .add_source(
                Environment::with_prefix(ENV_PREFIX)
                    .prefix_separator("_")
                    .separator(ENV_SEPARATOR)
                    .try_parsing(true),
            )
            .build()?;

        let config = config.try_deserialize::<AppConfig>()?;
        config.validate()?;
        Ok(config)
    }

    /// Validates invariants that cannot be expressed in the type system.
    fn validate(&self) -> Result<(), ConfigError> {
        const LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

        if !LEVELS.contains(&self.logging.level.as_str()) {
            return Err(ConfigError::Message(format!(
                "invalid `logging.level` value `{}`; expected one of: {}",
                self.logging.level,
                LEVELS.join(", ")
            )));
        }
        if self.server.request_timeout_secs == 0 {
            return Err(ConfigError::Message(
                "`server.request_timeout_secs` must be greater than zero".to_owned(),
            ));
        }
        if self.server.shutdown_timeout_secs == 0 {
            return Err(ConfigError::Message(
                "`server.shutdown_timeout_secs` must be greater than zero".to_owned(),
            ));
        }
        if self.server.max_body_size_bytes == 0 {
            return Err(ConfigError::Message(
                "`server.max_body_size_bytes` must be greater than zero".to_owned(),
            ));
        }
        self.validate_rate_limit()?;
        self.validate_cors()?;
        self.validate_security_headers()?;
        self.validate_database()?;
        self.validate_auth()?;
        self.validate_static()?;
        self.validate_metrics()?;
        self.validate_tls()?;
        self.validate_trusted_proxies()?;
        Ok(())
    }

    /// Validates the `[rate_limit]` section.
    fn validate_rate_limit(&self) -> Result<(), ConfigError> {
        if self.rate_limit.capacity == 0 {
            return Err(ConfigError::Message(
                "`rate_limit.capacity` must be greater than zero".to_owned(),
            ));
        }
        if !self.rate_limit.refill_per_second.is_finite()
            || self.rate_limit.refill_per_second <= 0.0
        {
            return Err(ConfigError::Message(
                "`rate_limit.refill_per_second` must be a positive finite number".to_owned(),
            ));
        }
        Ok(())
    }

    /// Validates the `[cors]` section.
    fn validate_cors(&self) -> Result<(), ConfigError> {
        let origins = &self.cors.allowed_origins;

        for origin in origins {
            if origin.is_empty() {
                return Err(ConfigError::Message(
                    "`cors.allowed_origins` must not contain empty strings".to_owned(),
                ));
            }
            if origin == "*" {
                if origins.len() > 1 {
                    return Err(ConfigError::Message(
                        "`cors.allowed_origins` wildcard `*` cannot be combined with specific \
                         origins"
                            .to_owned(),
                    ));
                }
            } else if !origin.starts_with("http://") && !origin.starts_with("https://") {
                return Err(ConfigError::Message(format!(
                    "invalid origin `{origin}` in `cors.allowed_origins`; expected \
                     `http://host[:port]`, `https://host[:port]` or `*`"
                )));
            } else if origin.ends_with('/') {
                return Err(ConfigError::Message(format!(
                    "invalid origin `{origin}` in `cors.allowed_origins`; origins must not end \
                     with a path separator"
                )));
            }
        }
        Ok(())
    }

    /// Validates the `[database]` section.
    fn validate_database(&self) -> Result<(), ConfigError> {
        if !self.database.enabled {
            return Ok(());
        }
        if self.database.max_connections == 0 {
            return Err(ConfigError::Message(
                "`database.max_connections` must be greater than zero".to_owned(),
            ));
        }
        let url = self.database.url.trim();
        if !url.starts_with("sqlite:") || url.len() <= "sqlite:".len() {
            return Err(ConfigError::Message(format!(
                "invalid `database.url` `{url}`; expected a SQLite URL such as \
                 `sqlite://wallermax.db?mode=rwc` or `sqlite::memory:`"
            )));
        }
        Ok(())
    }

    /// Validates the `[auth]` section.
    fn validate_auth(&self) -> Result<(), ConfigError> {
        if !self.auth.enabled {
            return Ok(());
        }
        if !self.database.enabled {
            return Err(ConfigError::Message(
                "`auth.enabled` requires `database.enabled`".to_owned(),
            ));
        }
        if self.auth.jwt_secret.len() < MIN_JWT_SECRET_LEN {
            return Err(ConfigError::Message(format!(
                "`auth.jwt_secret` must be at least {MIN_JWT_SECRET_LEN} characters; provide it \
                 via the WALLERMAX_AUTH__JWT_SECRET environment variable or \
                 wallermax.local.toml in production"
            )));
        }
        if self.auth.token_ttl_secs == 0 {
            return Err(ConfigError::Message(
                "`auth.token_ttl_secs` must be greater than zero".to_owned(),
            ));
        }
        if self.auth.min_password_len == 0 || self.auth.min_password_len > MAX_PASSWORD_LEN {
            return Err(ConfigError::Message(format!(
                "`auth.min_password_len` must be between 1 and {MAX_PASSWORD_LEN}"
            )));
        }
        if self.auth.issuer.trim().is_empty() {
            return Err(ConfigError::Message(
                "`auth.issuer` must not be empty".to_owned(),
            ));
        }
        if self.auth.refresh_token_ttl_secs == 0 {
            return Err(ConfigError::Message(
                "`auth.refresh_token_ttl_secs` must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }

    /// Validates the `[metrics]` section.
    fn validate_metrics(&self) -> Result<(), ConfigError> {
        let path = self.metrics.path.trim();
        if path.is_empty() || !path.starts_with('/') {
            return Err(ConfigError::Message(format!(
                "invalid `metrics.path` `{}`; expected an absolute path such as `/metrics`",
                self.metrics.path
            )));
        }
        if has_parent_segment(path) {
            return Err(ConfigError::Message(format!(
                "`metrics.path` must not contain `..` path segments: {path:?}"
            )));
        }
        Ok(())
    }

    /// Validates the `[tls]` section.
    fn validate_tls(&self) -> Result<(), ConfigError> {
        // The redirect address is validated regardless of `enabled` so
        // typos surface at load time, not the day TLS is switched on.
        if let Some(listen) = self.tls.http_listen.as_deref() {
            if listen.parse::<SocketAddr>().is_err() {
                return Err(ConfigError::Message(format!(
                    "invalid `tls.http_listen` `{listen}`; expected `host:port` such as \
                     `127.0.0.1:8080`"
                )));
            }
        }
        if !self.tls.enabled {
            return Ok(());
        }
        for (key, value) in [
            ("cert_path", &self.tls.cert_path),
            ("key_path", &self.tls.key_path),
        ] {
            if value.trim().is_empty() {
                return Err(ConfigError::Message(format!(
                    "`tls.{key}` must point at a PEM file while `tls.enabled = true`"
                )));
            }
        }
        Ok(())
    }

    /// Validates `server.trusted_proxies` entries (exact IPs or CIDRs).
    fn validate_trusted_proxies(&self) -> Result<(), ConfigError> {
        for entry in &self.server.trusted_proxies {
            if parse_cidr(entry).is_none() {
                return Err(ConfigError::Message(format!(
                    "invalid `server.trusted_proxies` entry `{entry}`; expected an IP address \
                     or CIDR block such as `10.0.0.4`, `10.0.0.0/8` or `fd00::/8`"
                )));
            }
        }
        Ok(())
    }

    /// Validates the `[static]` section.
    fn validate_static(&self) -> Result<(), ConfigError> {
        let static_files = &self.static_files;

        if static_files.root_dir.is_empty() || static_files.root_dir.contains('\0') {
            return Err(ConfigError::Message(
                "`static.root_dir` must be a non-empty path".to_owned(),
            ));
        }
        if has_parent_segment(&static_files.root_dir) {
            return Err(ConfigError::Message(format!(
                "`static.root_dir` must not contain `..` path segments: {:?}",
                static_files.root_dir
            )));
        }

        let index_file = &static_files.index_file;
        if index_file.is_empty()
            || index_file.contains('\0')
            || index_file.contains('/')
            || index_file.contains('\\')
            || index_file == "."
            || index_file == ".."
        {
            return Err(ConfigError::Message(
                "`static.index_file` must be a plain file name (no path separators, no `..`)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Validates the `[security_headers]` section.
    fn validate_security_headers(&self) -> Result<(), ConfigError> {
        let values = [
            (
                "x_content_type_options",
                &self.security_headers.x_content_type_options,
            ),
            ("x_frame_options", &self.security_headers.x_frame_options),
            ("referrer_policy", &self.security_headers.referrer_policy),
            (
                "content_security_policy",
                &self.security_headers.content_security_policy,
            ),
            (
                "strict_transport_security",
                &self.security_headers.strict_transport_security,
            ),
        ];

        for (key, value) in values {
            // Empty values are allowed (they omit the header).
            if !value.is_empty() && HeaderValue::from_str(value).is_err() {
                return Err(ConfigError::Message(format!(
                    "invalid header value for `security_headers.{key}`: {value:?}"
                )));
            }
        }
        Ok(())
    }
}

/// Returns `true` when `path` contains a `..` path segment (separated by
/// `/` or `\`), which would let requests escape the static root.
fn has_parent_segment(path: &str) -> bool {
    path.split(['/', '\\']).any(|segment| segment == "..")
}

impl StaticConfig {
    /// Path of the file served for `GET /`: `root_dir` joined with
    /// `index_file`.
    pub fn index_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.root_dir).join(&self.index_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::FileFormat;

    /// Builds a configuration from defaults plus a TOML string.
    fn from_toml(toml: &str) -> AppConfig {
        Config::builder()
            .add_source(Config::try_from(&AppConfig::default()).expect("default config"))
            .add_source(File::from_str(toml, FileFormat::Toml))
            .build()
            .expect("config builds")
            .try_deserialize()
            .expect("config deserializes")
    }

    #[test]
    fn defaults_are_sane() {
        let config = AppConfig::default();

        assert_eq!(
            config.server.socket_addr(),
            "127.0.0.1:8080".parse().expect("valid address")
        );
        assert_eq!(config.server.request_timeout_secs, 15);
        assert_eq!(
            config.server.max_body_size_bytes,
            DEFAULT_MAX_BODY_SIZE_BYTES
        );
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, LogFormat::Pretty);
        assert_eq!(config.request_id.mode, RequestIdMode::Accept);

        assert!(!config.middleware.rate_limit);
        assert!(!config.middleware.cors);
        assert!(config.middleware.body_limit);
        assert!(config.middleware.request_id);
        assert!(config.middleware.logging);
        assert!(config.middleware.security_headers);
        assert!(config.middleware.timeout);

        assert_eq!(config.rate_limit.capacity, 60);
        assert_eq!(config.rate_limit.refill_per_second, 10.0);
        assert!(config.cors.allowed_origins.is_empty());
        assert_eq!(config.cors.max_age_secs, 3600);

        // Phase 3 features are off in the built-in defaults so the server
        // behaves exactly like previous phases until opted in.
        assert!(!config.database.enabled);
        assert_eq!(config.database.max_connections, 5);
        assert_eq!(config.database.url, "sqlite://wallermax.db?mode=rwc");
        assert!(!config.auth.enabled);
        assert!(config.auth.jwt_secret.is_empty());
        assert_eq!(config.auth.token_ttl_secs, 3600);
        assert_eq!(config.auth.issuer, "wallermax-server");
        assert!(config.auth.registration_enabled);
        assert_eq!(config.auth.min_password_len, 8);
        assert!(config.auth.refresh_tokens_enabled);
        assert_eq!(config.auth.refresh_token_ttl_secs, 2_592_000);

        // Phase 4 features are off in the built-in defaults; the
        // versioned wallermax.toml enables the ones meant for real use.
        assert!(!config.metrics.enabled);
        assert_eq!(config.metrics.path, "/metrics");
        assert!(!config.tls.enabled);
        assert!(config.server.trusted_proxies.is_empty());
    }

    #[test]
    fn toml_overrides_defaults() {
        let config = from_toml(
            r#"
            [server]
            port = 9000
            request_timeout_secs = 5

            [logging]
            format = "json"

            [middleware]
            timeout = false
            rate_limit = true

            [request_id]
            mode = "overwrite"

            [rate_limit]
            capacity = 100
            refill_per_second = 2.5

            [cors]
            allowed_origins = ["https://app.example.com"]

            [security_headers]
            x_frame_options = "SAMEORIGIN"
            referrer_policy = ""
            "#,
        );

        assert_eq!(config.server.port, 9000);
        assert_eq!(config.server.request_timeout_secs, 5);
        // Untouched keys keep their defaults.
        assert_eq!(config.server.host.to_string(), "127.0.0.1");
        assert_eq!(config.logging.format, LogFormat::Json);
        assert!(!config.middleware.timeout);
        assert!(config.middleware.rate_limit);
        assert!(config.middleware.security_headers);

        assert_eq!(config.request_id.mode, RequestIdMode::Overwrite);
        assert_eq!(config.rate_limit.capacity, 100);
        assert_eq!(config.rate_limit.refill_per_second, 2.5);
        assert_eq!(
            config.cors.allowed_origins,
            vec!["https://app.example.com".to_owned()]
        );
        assert_eq!(config.security_headers.x_frame_options, "SAMEORIGIN");
        assert_eq!(config.security_headers.referrer_policy, "");
    }

    #[test]
    fn partial_files_are_accepted() {
        let config = from_toml("[server]\nport = 9100\n");

        assert_eq!(config.server.port, 9100);
        assert_eq!(config.logging.level, "info");
    }

    #[test]
    fn invalid_level_is_rejected() {
        let mut config = AppConfig::default();
        config.logging.level = String::from("verbose");

        assert!(config.validate().is_err());
    }

    #[test]
    fn zero_body_size_is_rejected() {
        let mut config = AppConfig::default();
        config.server.max_body_size_bytes = 0;

        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_rate_limit_values_are_rejected() {
        let mut config = AppConfig::default();
        config.rate_limit.capacity = 0;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.rate_limit.refill_per_second = 0.0;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.rate_limit.refill_per_second = f64::NAN;
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_origins_are_rejected() {
        let mut config = AppConfig::default();
        config.cors.allowed_origins = vec![String::from("ftp://example.com")];
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.cors.allowed_origins = vec![String::from("*"), String::from("https://example.com")];
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.cors.allowed_origins = vec![String::from("https://example.com/")];
        assert!(config.validate().is_err());
    }

    #[test]
    fn wildcard_alone_is_valid() {
        let mut config = AppConfig::default();
        config.cors.allowed_origins = vec![String::from("*")];

        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_security_header_values_are_rejected() {
        let mut config = AppConfig::default();
        config.security_headers.x_frame_options = String::from("bad\nvalue");

        assert!(config.validate().is_err());
    }

    #[test]
    fn phase3_sections_are_parsed_from_toml() {
        let config = from_toml(
            r#"
            [database]
            enabled = true
            url = "sqlite://custom.db?mode=rwc"
            max_connections = 3

            [auth]
            enabled = true
            jwt_secret = "a-fully-qualified-secret-of-at-least-32-chars"
            token_ttl_secs = 7200
            issuer = "custom-issuer"
            registration_enabled = false
            min_password_len = 12
            "#,
        );

        assert!(config.database.enabled);
        assert_eq!(config.database.url, "sqlite://custom.db?mode=rwc");
        assert_eq!(config.database.max_connections, 3);
        assert!(config.auth.enabled);
        assert_eq!(config.auth.token_ttl_secs, 7200);
        assert_eq!(config.auth.issuer, "custom-issuer");
        assert!(!config.auth.registration_enabled);
        assert_eq!(config.auth.min_password_len, 12);
    }

    #[test]
    fn auth_requires_database() {
        let mut config = AppConfig::default();
        config.auth.enabled = true;
        config.auth.jwt_secret = "a-fully-qualified-secret-of-at-least-32-chars".to_owned();
        config.database.enabled = false;

        assert!(config.validate().is_err());
    }

    #[test]
    fn short_jwt_secret_is_rejected() {
        let mut config = AppConfig::default();
        config.database.enabled = true;
        config.auth.enabled = true;
        config.auth.jwt_secret = "too-short".to_owned();

        assert!(config.validate().is_err());
    }

    #[test]
    fn zero_token_ttl_is_rejected() {
        let mut config = AppConfig::default();
        config.database.enabled = true;
        config.auth.enabled = true;
        config.auth.jwt_secret = "a-fully-qualified-secret-of-at-least-32-chars".to_owned();
        config.auth.token_ttl_secs = 0;

        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_database_values_are_rejected() {
        let mut config = AppConfig::default();
        config.database.enabled = true;
        config.database.url = String::from("postgres://host/db");
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.database.enabled = true;
        config.database.max_connections = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn header_pairs_skip_empty_values() {
        let config = SecurityHeadersConfig {
            referrer_policy: String::new(),
            x_frame_options: String::from("SAMEORIGIN"),
            ..SecurityHeadersConfig::default()
        };

        let pairs = config.header_pairs();

        let names: Vec<String> = pairs
            .iter()
            .map(|(name, _)| name.as_str().to_owned())
            .collect();
        assert!(!names.contains(&"referrer-policy".to_owned()));

        let frame = pairs
            .iter()
            .find(|(name, _)| name.as_str() == "x-frame-options")
            .expect("frame options header present");
        assert_eq!(frame.1.to_str().expect("ascii value"), "SAMEORIGIN");
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn static_defaults_are_sane() {
        let config = AppConfig::default();

        assert!(!config.static_files.enabled);
        assert_eq!(config.static_files.root_dir, "public");
        assert_eq!(config.static_files.index_file, "index.html");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn static_section_is_parsed_from_toml() {
        let config = from_toml(
            r#"
            [static]
            enabled = true
            root_dir = "site"
            index_file = "home.html"
            "#,
        );

        assert!(config.static_files.enabled);
        assert_eq!(config.static_files.root_dir, "site");
        assert_eq!(config.static_files.index_file, "home.html");
        assert!(config.validate().is_ok());
        assert_eq!(
            config.static_files.index_path(),
            std::path::Path::new("site").join("home.html")
        );
    }

    #[test]
    fn static_parent_segments_are_rejected() {
        for root_dir in ["..", "public/..", "a/../b", "a\\..\\b"] {
            let mut config = AppConfig::default();
            config.static_files.enabled = true;
            config.static_files.root_dir = root_dir.to_owned();
            assert!(config.validate().is_err(), "root_dir {root_dir} rejected");
        }
    }

    #[test]
    fn static_index_file_must_be_a_plain_name() {
        for index_file in ["", "docs/index.html", "a\\b.html", "..", "."] {
            let mut config = AppConfig::default();
            config.static_files.enabled = true;
            config.static_files.index_file = index_file.to_owned();
            assert!(
                config.validate().is_err(),
                "index_file {index_file:?} rejected"
            );
        }
    }

    #[test]
    fn static_disabled_skips_validation_of_paths_but_defaults_pass() {
        // A nonsensical root_dir is still rejected while the section is
        // disabled: validation of values (not existence) always runs.
        let mut config = AppConfig::default();
        config.static_files.root_dir = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn phase4_sections_are_parsed_from_toml() {
        let config = from_toml(
            r#"
            [server]
            trusted_proxies = ["127.0.0.1", "10.0.0.0/8", "fd00::/8"]

            [auth]
            refresh_tokens_enabled = false
            refresh_token_ttl_secs = 86400

            [metrics]
            enabled = true
            path = "/internal/metrics"

            [tls]
            enabled = true
            cert_path = "certs/cert.pem"
            key_path = "certs/key.pem"
            http_listen = "127.0.0.1:8080"
            "#,
        );

        assert_eq!(
            config.server.trusted_proxies,
            vec![
                "127.0.0.1".to_owned(),
                "10.0.0.0/8".to_owned(),
                "fd00::/8".to_owned()
            ]
        );
        assert!(!config.auth.refresh_tokens_enabled);
        assert_eq!(config.auth.refresh_token_ttl_secs, 86400);
        assert!(config.metrics.enabled);
        assert_eq!(config.metrics.path, "/internal/metrics");
        assert!(config.tls.enabled);
        assert_eq!(config.tls.cert_path, "certs/cert.pem");
        assert_eq!(config.tls.key_path, "certs/key.pem");
        assert_eq!(config.tls.http_listen.as_deref(), Some("127.0.0.1:8080"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn zero_refresh_token_ttl_is_rejected() {
        let mut config = AppConfig::default();
        config.database.enabled = true;
        config.auth.enabled = true;
        config.auth.jwt_secret = "a-fully-qualified-secret-of-at-least-32-chars".to_owned();
        config.auth.refresh_token_ttl_secs = 0;

        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_metrics_paths_are_rejected() {
        let mut config = AppConfig::default();
        config.metrics.path = String::from("metrics");
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.metrics.path = String::from("/a/../b");
        assert!(config.validate().is_err());
    }

    #[test]
    fn tls_requires_pem_paths_while_enabled() {
        let mut config = AppConfig::default();
        config.tls.enabled = true;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.tls.enabled = true;
        config.tls.cert_path = String::from("certs/cert.pem");
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.tls.enabled = true;
        config.tls.cert_path = String::from("certs/cert.pem");
        config.tls.key_path = String::from("certs/key.pem");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_tls_http_listen_is_rejected() {
        let mut config = AppConfig::default();
        config.tls.http_listen = Some(String::from("not-an-address"));

        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_trusted_proxy_entries_are_rejected() {
        let mut config = AppConfig::default();
        config.server.trusted_proxies = vec![String::from("10.0.0.0/8")];
        assert!(config.validate().is_ok());

        let mut config = AppConfig::default();
        config.server.trusted_proxies = vec![String::from("proxy.example.com")];
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.server.trusted_proxies = vec![String::from("10.0.0.0/33")];
        assert!(config.validate().is_err());
    }
}
