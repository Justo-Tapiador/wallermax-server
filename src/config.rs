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

/// Upper bound for `external_api.response_limit_bytes`: 16 MiB.
const MAX_EXTERNAL_API_RESPONSE_BYTES: usize = 16 * 1_048_576;

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
    /// Server-side proxy for external APIs (the `[external_api]` section).
    pub external_api: ExternalApiConfig,
    /// SQLite persistence layer.
    pub database: DatabaseConfig,
    /// JWT authentication and user accounts.
    pub auth: AuthConfig,
    /// The small built-in CMS (the `[cms]` section).
    pub cms: CmsConfig,
    /// Static file serving (the `[static]` section).
    #[serde(rename = "static")]
    pub static_files: StaticConfig,
    /// Dynamic `.jhs` template rendering (the `[templates]` section).
    pub templates: TemplatesConfig,
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
            // v0.8.0: styles and same-origin images are allowed for the
            // CMS UI (stylesheet under `public/assets/`); scripts stay
            // blocked — the whole site works without JavaScript.
            // `form-action 'self'` pins form posts to this origin.
            content_security_policy: String::from(
                "default-src 'none'; style-src 'self'; img-src 'self' data:; \
                 form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
            ),
            strict_transport_security: String::from("max-age=31536000; includeSubDomains"),
        }
    }
}

/// Server-side proxy for external APIs (v0.12.0): browsers call named
/// endpoints on this server and it forwards upstream, keeping secret
/// headers out of the browser (and sidestepping the upstream's CORS
/// policy, which does not apply server-to-server).
///
/// Enabled implicitly by configuring at least one
/// `[[external_api.endpoints]]` entry; with no endpoints the route
/// family is not mounted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalApiConfig {
    /// Per-call upstream timeout in seconds. Keep it below
    /// `server.request_timeout_secs` so the proxy answers first.
    pub timeout_secs: u64,
    /// Maximum forwarded upstream body size in bytes (protects the
    /// server from an upstream that answers with something enormous).
    pub response_limit_bytes: usize,
    /// Named upstreams; the browser-visible path segment is the `name`.
    pub endpoints: Vec<ExternalEndpointConfig>,
}

impl Default for ExternalApiConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 8,
            response_limit_bytes: 262_144,
            endpoints: Vec::new(),
        }
    }
}

/// One named upstream of the `[external_api]` proxy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalEndpointConfig {
    /// Endpoint name; becomes the path segment `/api/ext/{name}`. The
    /// same slug shape the CMS enforces (lowercase letters, digits,
    /// single dashes).
    pub name: String,
    /// Absolute upstream URL (scheme `http` or `https`). The incoming
    /// query string is forwarded on top of it.
    pub url: String,
    /// Whether calls require an authenticated user (Bearer token or
    /// browser session). `false` lets public pages use the endpoint;
    /// the global rate limiter still applies either way.
    pub auth_required: bool,
    /// Headers attached to every upstream call. Values may reference the
    /// environment with `${VAR_NAME}`, expanded once at startup — the
    /// committed `wallermax.toml` can hold placeholders while real keys
    /// live in `wallermax.local.toml` or the process environment.
    pub headers: std::collections::BTreeMap<String, String>,
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
    /// When the `wallermax_session` cookie carries the `Secure`
    /// attribute (v0.8.0).
    ///
    /// `auto` (the default) sets `Secure` only while `[tls]` is enabled:
    /// browsers refuse to store `Secure` cookies on plain-HTTP origins, so
    /// an unconditional `Secure` broke browser logins on HTTP-only dev
    /// servers (curl and PowerShell were lax about it, which made the
    /// failure look mysterious). `always` and `never` pin the attribute
    /// explicitly — `always` for TLS-terminating reverse proxies where the
    /// server itself speaks HTTP.
    pub secure_cookies: SecureCookieMode,
}

/// Policy for the `Secure` attribute of the session cookie.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecureCookieMode {
    /// `Secure` while `[tls] enabled = true` (the default).
    #[default]
    Auto,
    /// Always `Secure`, even when the server itself speaks HTTP (for
    /// TLS-terminating reverse proxies).
    Always,
    /// Never `Secure` (local-only HTTP testing).
    Never,
}

impl SecureCookieMode {
    /// Resolves the effective `Secure` flag for a server whose TLS state
    /// is `tls_enabled`.
    pub fn resolve(self, tls_enabled: bool) -> bool {
        match self {
            SecureCookieMode::Auto => tls_enabled,
            SecureCookieMode::Always => true,
            SecureCookieMode::Never => false,
        }
    }
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
            secure_cookies: SecureCookieMode::default(),
        }
    }
}

/// The small built-in CMS (the `[cms]` section).
///
/// While enabled — and it additionally requires `[database]`, `[auth]`
/// and `[templates]` to be enabled — the server mounts a database-backed
/// content layer on top of the v0.7.0 session groundwork:
///
/// - **Public site**: `GET /p` lists published pages; `GET /p/<slug>`
///   renders one (page bodies are `.jhs` template source, so they see
///   the `user` global and can embed the shared partials through
///   `<?jhs include("partials/header") ?>`);
/// - **Admin panel**: `/admin` (pages, import-from-`public/`, users) for
///   the `admin` and `editor` roles — pure HTML forms, no JavaScript,
///   everything audited on the server side.
///
/// The panel manages **content and CMS users only**: server configuration
/// (ports, TLS, secrets, middleware) stays in `wallermax.toml`, which is
/// only writable with local repository access — the deliberate
/// privilege split between the CMS administrator and the server
/// operator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CmsConfig {
    /// Enables the CMS routes and services.
    ///
    /// The `Default` is `false` — opt-in like `[database]`, `[auth]` and
    /// `[templates]`: the lean server keeps the section off until the
    /// configuration file (the shipped `wallermax.toml`) turns it on.
    pub enabled: bool,
    /// The slug of the CMS page that takes over `GET /` (v0.11.0).
    ///
    /// While set (and the CMS is enabled), the homepage renders that
    /// page through the exact `GET /p/{slug}` pipeline — sandboxed body
    /// render, `views/cms_page.jhs` wrapper, draft gating — BEFORE the
    /// static index file, the views auto-routing or the JSON 404 are
    /// considered: the explicit configuration always beats
    /// `public/index.html`. Rendering directly (instead of redirecting
    /// to `/p/{slug}`) keeps `/` itself the canonical URL.
    ///
    /// A slug that no longer exists at request time (page deleted after
    /// startup) logs a warning and falls back to the normal homepage
    /// chain; a **draft** default page follows the `/p/{slug}` gating
    /// (404 for the public, preview banner for editors). The slug shape
    /// is validated at startup exactly like the panel forms validate
    /// it.
    ///
    /// `WALLERMAX_CMS__DEFAULT_PAGE` overrides the file value. Setting
    /// it while `cms.enabled = false` is accepted (the shape is still
    /// validated) but ignored, with a startup warning in `build_state`.
    pub default_page: Option<String>,
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

/// Dynamic `.jhs` template rendering (the `[templates]` section).
///
/// Templates are HTML with embedded JavaScript using `<?jhs ... ?>`
/// code blocks and `<?= ... ?>` output expressions (the `node-jhs2`
/// syntax), rendered inside a hardened sandbox (a native `require()`
/// bridge with a configurable module banner since v0.9.0, but no
/// `Buffer`, no host file system outside the modules directory and no
/// network; runaway loops are bounded by `loop_iteration_limit`).
///
/// While enabled:
///
/// - `GET`/`HEAD` requests for an existing `*.jhs` file under the
///   `[static]` root are **rendered** (the source is never served
///   raw);
/// - otherwise-unmatched paths auto-route to views: `GET /contacto`
///   renders `views_dir/contacto.jhs`, `GET /blog` renders
///   `views_dir/blog.jhs` or `views_dir/blog/index.jhs`, and `GET /`
///   falls back to `views_dir/index.jhs` when the static index file
///   is missing;
/// - API routes keep precedence and requests that resolve to nothing
///   answer the standard JSON 404 envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplatesConfig {
    /// Enables dynamic template rendering.
    pub enabled: bool,
    /// The rendering backend (v0.10.0):
    ///
    /// - `"boa"` — the in-process sandboxed engine (hardened default);
    /// - `"sidecar"` — the Node sidecar running the original node-jhs2
    ///   engine; startup fails fast when Node cannot be launched;
    /// - `"auto"` — the sidecar when it starts and stays healthy, with a
    ///   transparent fallback to `"boa"` renders.
    pub backend: String,
    /// Sidecar tuning while `backend` is `"sidecar"` or `"auto"`
    /// (v0.10.0); ignored by the `"boa"` backend.
    pub sidecar: SidecarConfig,
    /// Directory holding the view templates, relative to the working
    /// directory (absolute paths are allowed too). It must exist at
    /// startup while rendering is enabled. `..` segments are rejected
    /// at validation.
    pub views_dir: String,
    /// Cache compiled templates, invalidating entries when their
    /// modification time changes (recompiles are automatic; no restart
    /// needed).
    pub cache: bool,
    /// HTML-escape all dynamic output (`<?= ?>` expressions and
    /// `echo()` calls); literal template text is never escaped, and
    /// `raw()` always bypasses the escaper.
    pub auto_escape: bool,
    /// Expose the authenticated identity to templates as the `user`
    /// global (`{ id, username, role }`, or `null` for anonymous
    /// visitors). Requires `[auth]` to be enabled; invalid or missing
    /// tokens degrade to the anonymous form instead of rejecting the
    /// render.
    pub expose_user: bool,
    /// Upper bound on loop iterations inside one render. Protects the
    /// workers from runaway template loops (the sandbox throws when a
    /// template exceeds it).
    pub loop_iteration_limit: u64,
    /// Installs `require()` in the template sandbox (v0.9.0): the
    /// `crypto` polyfill plus CommonJS loading of pure-JS modules from
    /// `modules_dir`, guarded by `forbidden_modules`. While `false`,
    /// `require` is undefined and templates behave exactly like the
    /// v0.8.x sandbox.
    pub require_enabled: bool,
    /// Directory local JavaScript modules resolve under, relative to
    /// the working directory (absolute paths are allowed too).
    /// Unlike `views_dir` it may be absent at startup — requiring any
    /// local module then answers a descriptive "Cannot find module"
    /// error, while the `crypto` polyfill keeps working.
    pub modules_dir: String,
    /// The module banner: package names `require()` rejects with a
    /// descriptive error, checked before polyfills and files (so an
    /// entry can ban even the `crypto` polyfill or a local module).
    /// Defaults to the dangerous Node built-ins (`fs`,
    /// `child_process`, `net`, …) plus `vm`, `jhs` and `mv`.
    pub forbidden_modules: Vec<String>,
}

impl Default for TemplatesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: String::from("auto"),
            sidecar: SidecarConfig::default(),
            views_dir: String::from("views"),
            cache: true,
            auto_escape: true,
            expose_user: true,
            loop_iteration_limit: 10_000_000,
            require_enabled: true,
            modules_dir: String::from("modules"),
            forbidden_modules: crate::template_engine::DEFAULT_FORBIDDEN_MODULES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }
}

/// The `[templates.sidecar]` section: Node sidecar tuning
/// (v0.10.0).
///
/// Every field has a working default, so an empty
/// `[templates.sidecar]` table is a valid configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SidecarConfig {
    /// Node.js binary used to launch the sidecar (resolved through
    /// `PATH`).
    pub node_command: String,
    /// The sidecar service script, relative to the working directory
    /// (absolute paths are allowed too).
    pub script: String,
    /// Render worker threads inside the sidecar. Each worker runs one
    /// render at a time and is terminated (and respawned) when a render
    /// exceeds `render_budget_ms`.
    pub workers: u32,
    /// Budget for the sidecar launch: process spawn, the READY
    /// handshake and the startup selftest.
    pub startup_timeout_ms: u64,
    /// Client-side timeout for one render request, queue wait
    /// included. Must exceed `render_budget_ms` (validated).
    pub request_timeout_ms: u64,
    /// Wall-clock hard-kill budget per render inside the sidecar — the
    /// bound the original engine's ineffective `vm` timeout never had.
    pub render_budget_ms: u64,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            node_command: String::from("node"),
            script: String::from("sidecar/jhs-sidecar.mjs"),
            workers: 2,
            startup_timeout_ms: 8_000,
            request_timeout_ms: 10_000,
            render_budget_ms: 5_000,
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
        self.validate_external_api()?;
        self.validate_database()?;
        self.validate_auth()?;
        self.validate_cms()?;
        self.validate_static()?;
        self.validate_templates()?;
        self.validate_metrics()?;
        self.validate_tls()?;
        self.validate_trusted_proxies()?;
        Ok(())
    }

    /// Validates the `[external_api]` section.
    ///
    /// Static shape only (names, URLs, limits, header names): the
    /// `${ENV}` expansion and the resulting header values are resolved
    /// when the state is built (see [`crate::external_api`]).
    fn validate_external_api(&self) -> Result<(), ConfigError> {
        if self.external_api.endpoints.is_empty() {
            return Ok(());
        }
        if self.external_api.timeout_secs == 0 || self.external_api.timeout_secs > 60 {
            return Err(ConfigError::Message(String::from(
                "`external_api.timeout_secs` must be between 1 and 60 (and below \
                 `server.request_timeout_secs` so the proxy answers before the global \
                 request timeout)",
            )));
        }
        if self.external_api.response_limit_bytes == 0
            || self.external_api.response_limit_bytes > MAX_EXTERNAL_API_RESPONSE_BYTES
        {
            return Err(ConfigError::Message(format!(
                "`external_api.response_limit_bytes` must be between 1 and \
                 {MAX_EXTERNAL_API_RESPONSE_BYTES} bytes"
            )));
        }
        let mut seen = std::collections::HashSet::with_capacity(self.external_api.endpoints.len());
        for endpoint in &self.external_api.endpoints {
            if !crate::util::valid_slug(&endpoint.name) {
                return Err(ConfigError::Message(format!(
                    "invalid `external_api` endpoint name `{}`; expected the same slug shape \
                     the CMS enforces: 1-64 characters of lowercase letters, digits and \
                     single dashes",
                    endpoint.name
                )));
            }
            if !seen.insert(endpoint.name.as_str()) {
                return Err(ConfigError::Message(format!(
                    "duplicate `external_api` endpoint name `{}`",
                    endpoint.name
                )));
            }
            let url = reqwest::Url::parse(&endpoint.url).map_err(|_| {
                ConfigError::Message(format!(
                    "`external_api` endpoint `{}` has an invalid URL `{}`",
                    endpoint.name, endpoint.url
                ))
            })?;
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err(ConfigError::Message(format!(
                    "`external_api` endpoint `{}` URL must use http or https (got `{}`)",
                    endpoint.name,
                    url.scheme()
                )));
            }
            for (name, value) in &endpoint.headers {
                if crate::external_api::RESERVED_HEADER_NAMES
                    .contains(&name.to_ascii_lowercase().as_str())
                {
                    return Err(ConfigError::Message(format!(
                        "`external_api` endpoint `{}` sets reserved header `{name}`; the HTTP \
                         client owns it",
                        endpoint.name
                    )));
                }
                if HeaderName::from_bytes(name.as_bytes()).is_err() {
                    return Err(ConfigError::Message(format!(
                        "`external_api` endpoint `{}` has an invalid header name `{name}`",
                        endpoint.name
                    )));
                }
                if value.is_empty() {
                    return Err(ConfigError::Message(format!(
                        "`external_api` endpoint `{}` header `{name}` is empty",
                        endpoint.name
                    )));
                }
            }
        }
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

    /// Validates the `[cms]` section.
    ///
    /// The cross-feature requirements (`database`, `auth`, `templates`)
    /// are enforced here as hard errors when `cms.enabled` is set: a
    /// silently degraded CMS would be far more confusing than a clear
    /// startup failure.
    fn validate_cms(&self) -> Result<(), ConfigError> {
        // The slug shape is checked even while the CMS is off: a
        // typo'd `default_page` is a configuration mistake regardless
        // of the switch, and validating it costs nothing.
        if let Some(slug) = self.cms.default_page.as_deref() {
            if !crate::util::valid_slug(slug) {
                return Err(ConfigError::Message(format!(
                    "invalid `cms.default_page` `{slug}`; expected the same slug shape the \
                     panel forms enforce: 1-64 characters of lowercase letters, digits and \
                     single dashes"
                )));
            }
        }
        if !self.cms.enabled {
            return Ok(());
        }
        if !self.database.enabled || !self.auth.enabled {
            return Err(ConfigError::Message(
                "`cms.enabled` requires both `database.enabled` and `auth.enabled`".to_owned(),
            ));
        }
        if !self.templates.enabled {
            return Err(ConfigError::Message(
                "`cms.enabled` requires `templates.enabled` (CMS pages are `.jhs` content)"
                    .to_owned(),
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

    /// Validates the `[templates]` section.
    fn validate_templates(&self) -> Result<(), ConfigError> {
        let templates = &self.templates;

        if templates.views_dir.is_empty() || templates.views_dir.contains('\0') {
            return Err(ConfigError::Message(
                "`templates.views_dir` must be a non-empty path".to_owned(),
            ));
        }
        if has_parent_segment(&templates.views_dir) {
            return Err(ConfigError::Message(format!(
                "`templates.views_dir` must not contain `..` path segments: {:?}",
                templates.views_dir
            )));
        }
        if templates.loop_iteration_limit == 0 {
            return Err(ConfigError::Message(
                "`templates.loop_iteration_limit` must be greater than zero".to_owned(),
            ));
        }
        if templates.require_enabled
            && (templates.modules_dir.trim().is_empty() || templates.modules_dir.contains('\0'))
        {
            return Err(ConfigError::Message(
                "`templates.modules_dir` must be a non-empty path".to_owned(),
            ));
        }
        if has_parent_segment(&templates.modules_dir) {
            return Err(ConfigError::Message(format!(
                "`templates.modules_dir` must not contain `..` path segments: {:?}",
                templates.modules_dir
            )));
        }
        for name in &templates.forbidden_modules {
            let name = name.trim();
            if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains('\0') {
                return Err(ConfigError::Message(format!(
                    "`templates.forbidden_modules` entries must be plain module names, not paths \
                     (got {name:?})"
                )));
            }
        }

        let backend = templates.backend.trim();
        if !matches!(backend, "boa" | "sidecar" | "auto") {
            return Err(ConfigError::Message(format!(
                "`templates.backend` must be one of \"boa\", \"sidecar\" or \"auto\" (got {backend:?})"
            )));
        }
        let sidecar = &templates.sidecar;
        if sidecar.workers == 0 || sidecar.workers > 16 {
            return Err(ConfigError::Message(
                "`templates.sidecar.workers` must be between 1 and 16".to_owned(),
            ));
        }
        if sidecar.startup_timeout_ms == 0 || sidecar.request_timeout_ms == 0 {
            return Err(ConfigError::Message(
                "`templates.sidecar.startup_timeout_ms` and \
                 `templates.sidecar.request_timeout_ms` must be greater than zero"
                    .to_owned(),
            ));
        }
        if sidecar.render_budget_ms < 100 {
            return Err(ConfigError::Message(
                "`templates.sidecar.render_budget_ms` must be at least 100".to_owned(),
            ));
        }
        if sidecar.request_timeout_ms <= sidecar.render_budget_ms {
            return Err(ConfigError::Message(
                "`templates.sidecar.request_timeout_ms` must exceed \
                 `templates.sidecar.render_budget_ms` (the client must outwait the \
                 hard-kill budget, or every budgeted render would time out client-side \
                 first)"
                    .to_owned(),
            ));
        }
        if backend != "boa" && (sidecar.script.trim().is_empty() || sidecar.script.contains('\0')) {
            return Err(ConfigError::Message(
                "`templates.sidecar.script` must be a non-empty path while the sidecar \
                 backend is selected"
                    .to_owned(),
            ));
        }
        if backend != "boa" && has_parent_segment(&sidecar.script) {
            return Err(ConfigError::Message(format!(
                "`templates.sidecar.script` must not contain `..` path segments: {:?}",
                sidecar.script
            )));
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
    fn cms_default_page_parses_and_defaults_to_none() {
        let config = AppConfig::default();
        assert!(
            config.cms.default_page.is_none(),
            "the homepage takeover stays off by default"
        );
        assert!(config.validate().is_ok());

        let config = from_toml(
            r#"
            [cms]
            enabled = true
            default_page = "inicio"
            "#,
        );
        assert_eq!(config.cms.default_page.as_deref(), Some("inicio"));
        assert!(config.cms.enabled);
    }

    #[test]
    fn cms_default_page_requires_a_well_formed_slug() {
        let mut config = AppConfig::default();
        config.cms.enabled = true;
        config.database.enabled = true;
        config.auth.enabled = true;
        config.templates.enabled = true;

        config.cms.default_page = Some(String::from("inicio"));
        assert!(
            config.validate_cms().is_ok(),
            "a well-formed slug passes with the CMS on"
        );

        config.cms.default_page = Some(String::from("Malformed Slug!"));
        assert!(
            config.validate_cms().is_err(),
            "a malformed slug refuses startup"
        );

        // The shape is validated even while the CMS is off: a typo is a
        // configuration mistake regardless of the switch.
        config.cms.enabled = false;
        assert!(
            config.validate_cms().is_err(),
            "shape errors are not silent while the CMS is off"
        );

        // A well-formed slug with the CMS off is accepted — and ignored,
        // with a startup warning from `build_state`.
        config.cms.default_page = Some(String::from("inicio"));
        assert!(config.validate_cms().is_ok());
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

    #[test]
    fn external_api_defaults_to_disabled() {
        let config = AppConfig::default();
        assert!(config.external_api.endpoints.is_empty());
        assert_eq!(config.external_api.timeout_secs, 8);
        assert_eq!(config.external_api.response_limit_bytes, 262_144);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn external_api_endpoints_parse_from_toml() {
        let config = from_toml(
            r#"
            [external_api]
            timeout_secs = 5
            response_limit_bytes = 4096

            [[external_api.endpoints]]
            name = "weather"
            url = "https://api.example.com/data"
            auth_required = true

            [external_api.endpoints.headers]
            X-Api-Key = "${WEATHER_API_KEY}"
            "#,
        );

        assert_eq!(config.external_api.timeout_secs, 5);
        assert_eq!(config.external_api.response_limit_bytes, 4096);
        assert_eq!(config.external_api.endpoints.len(), 1);
        let endpoint = &config.external_api.endpoints[0];
        assert_eq!(endpoint.name, "weather");
        assert_eq!(endpoint.url, "https://api.example.com/data");
        assert!(endpoint.auth_required);
        assert_eq!(
            endpoint.headers.get("X-Api-Key").map(String::as_str),
            Some("${WEATHER_API_KEY}")
        );
        assert!(config.validate().is_ok());
    }

    fn external_api_config_with(name: &str, url: &str, headers: &[(&str, &str)]) -> AppConfig {
        let mut config = AppConfig::default();
        config.external_api.endpoints.push(ExternalEndpointConfig {
            name: name.to_owned(),
            url: url.to_owned(),
            auth_required: false,
            headers: headers
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        });
        config
    }

    #[test]
    fn external_api_rejects_bad_endpoint_names() {
        for name in [
            "",
            "UPPER",
            "-lead",
            "trail-",
            "a--b",
            "with space",
            "with/slash",
        ] {
            let config = external_api_config_with(name, "https://api.example.com", &[]);
            assert!(config.validate().is_err(), "name `{name}` must be rejected");
        }
        let config = external_api_config_with("weather-2", "https://api.example.com", &[]);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn external_api_rejects_duplicate_names() {
        let mut config = external_api_config_with("weather", "https://a.example.com", &[]);
        config.external_api.endpoints.push(ExternalEndpointConfig {
            name: String::from("weather"),
            url: String::from("https://b.example.com"),
            auth_required: false,
            headers: Default::default(),
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn external_api_rejects_non_http_urls() {
        for url in ["", "not a url", "ftp://example.com", "file:///etc/passwd"] {
            let config = external_api_config_with("svc", url, &[]);
            assert!(config.validate().is_err(), "url `{url}` must be rejected");
        }
        let config = external_api_config_with("svc", "http://intranet.local/api", &[]);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn external_api_rejects_reserved_and_invalid_headers() {
        let config =
            external_api_config_with("svc", "https://api.example.com", &[("Cookie", "session=1")]);
        assert!(config.validate().is_err());

        let config =
            external_api_config_with("svc", "https://api.example.com", &[("X Api Key", "value")]);
        assert!(config.validate().is_err());

        let config = external_api_config_with("svc", "https://api.example.com", &[("X-Key", "")]);
        assert!(config.validate().is_err());

        let config =
            external_api_config_with("svc", "https://api.example.com", &[("X-Api-Key", "value")]);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn external_api_rejects_out_of_range_limits() {
        let mut config = external_api_config_with("svc", "https://api.example.com", &[]);
        config.external_api.timeout_secs = 0;
        assert!(config.validate().is_err());

        let mut config = external_api_config_with("svc", "https://api.example.com", &[]);
        config.external_api.timeout_secs = 61;
        assert!(config.validate().is_err());

        let mut config = external_api_config_with("svc", "https://api.example.com", &[]);
        config.external_api.response_limit_bytes = 0;
        assert!(config.validate().is_err());

        let mut config = external_api_config_with("svc", "https://api.example.com", &[]);
        config.external_api.response_limit_bytes = 16 * 1_048_576 + 1;
        assert!(config.validate().is_err());
    }
}
