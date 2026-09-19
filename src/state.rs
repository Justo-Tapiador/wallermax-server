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
//! - the boot-time virtual-host resolution (F17): the organizations'
//!   document roots plus the host bindings table — see [`VhostData`];
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
use crate::db::{
    MediaRepository, MenuRepository, PageRepository, UserRepository, CMS_ORGANIZATION_KEY,
    MAIN_ORGANIZATION_KEY,
};
use crate::external_api::ExternalApi;
use crate::metrics::Metrics;
use crate::proxy::{self, Cidr};
use crate::rate_limit::{LoginThrottle, RateLimiter};
use crate::template_engine::renderer::BrokenRenderer;
use crate::template_engine::{
    AutoRenderer, JhsEngine, JhsOptions, RequireOptions, SidecarOptions, SidecarRenderer,
    StrictSidecar, TemplateRenderer,
};
use crate::vhosts::HostBinding;

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

/// CMS services shared by handlers when `[cms]` (plus `[database]`,
/// `[auth]` and `[templates]`) is enabled.
///
/// Built once at startup (see [`crate::server::build_state`]) and
/// immutable afterwards.
pub struct CmsContext {
    /// Page storage behind the [`PageRepository`] abstraction.
    pub pages: Arc<dyn PageRepository>,
    /// Named navigation menus behind the [`MenuRepository`] abstraction
    /// (F7): the `menus` template global and `/admin/menus` read here.
    pub menus: Arc<dyn MenuRepository>,
    /// Media library metadata behind the [`MediaRepository`]
    /// abstraction (F9).
    pub media: Arc<dyn MediaRepository>,
    /// The tenant model behind the [`OrganizationRepository`]
    /// abstraction (F18): the organizations, domains and memberships
    /// the Tenants pages manage — and the reads the live vhost
    /// snapshot refreshes from.
    pub organizations: Arc<dyn crate::db::OrganizationRepository>,
    /// Absolute, startup-frozen directory the media files live in
    /// (F9). Resolved from `[cms] media_dir` exactly like `views_dir`
    /// and created at startup, so request handling only ever *joins*
    /// flat server-generated names onto it.
    pub media_root: std::path::PathBuf,
}

/// Dynamic template rendering services shared by the template
/// middleware when `[templates]` is enabled.
///
/// Built once at startup from the configuration and immutable
/// afterwards. The active [`TemplateRenderer`] backend is chosen by
/// `[templates] backend`: the in-process sandboxed boa engine
/// (`"boa"`), the Node sidecar running the original node-jhs2 engine
/// (`"sidecar"`, strict), or the sidecar with transparent boa fallback
/// (`"auto"`, the default). Every render in the process — public
/// `.jhs` files, auto-routed views and CMS page bodies — flows through
/// the same handle.
pub struct TemplateEngine {
    /// The active rendering backend.
    renderer: std::sync::Arc<dyn TemplateRenderer>,
    /// The configured backend name (diagnostics for [`Self::ensure_ready`]).
    backend: String,
    /// Set when `backend = "sidecar"` and the sidecar failed to start;
    /// the renderer is then a [`BrokenRenderer`] that answers this
    /// error on every render.
    sidecar_spawn_error: Option<String>,
    /// Directory view templates are auto-routed from, resolved to an
    /// absolute path at construction (relative paths resolve against
    /// the working directory, exactly like `[static] root_dir`, and the
    /// resolution is frozen so the sandbox never re-joins a candidate
    /// onto the views path).
    views_dir: std::path::PathBuf,
    /// Static root from `[static]`, present only while static serving is
    /// enabled (`.jhs` files there are rendered on the fly). Absolute,
    /// resolved exactly like `views_dir`.
    static_root: Option<std::path::PathBuf>,
}

impl TemplateEngine {
    /// Builds the rendering backend from the `[templates]` and
    /// `[static]` configuration.
    ///
    /// While the sidecar backend is selected, this spawns the Node
    /// child process, performs the READY handshake and requires the
    /// startup selftest to pass — a synchronous, startup-only wait
    /// (bounded by `[templates.sidecar] startup_timeout_ms`).
    fn new(
        templates: &crate::config::TemplatesConfig,
        views_dir: &str,
        static_root: Option<&str>,
    ) -> Self {
        let views_dir = absolutize(views_dir);
        let modules_dir = absolutize(&templates.modules_dir);
        let forbidden: Vec<String> = templates
            .forbidden_modules
            .iter()
            .map(|name| name.trim().trim_start_matches("node:").to_owned())
            .collect();

        // The hardened in-process backend — always constructed: it is
        // the "boa" choice, the "auto" fallback and the strict-mode
        // companion that never goes to waste.
        let boa = std::sync::Arc::new(JhsEngine::new(JhsOptions {
            views_path: views_dir.clone(),
            cache: templates.cache,
            auto_escape: templates.auto_escape,
            tags: Default::default(),
            loop_iteration_limit: templates.loop_iteration_limit,
            require: RequireOptions {
                enabled: templates.require_enabled,
                modules_dir: modules_dir.clone(),
                forbidden: forbidden.clone(),
            },
        }));

        let backend = templates.backend.trim().to_owned();
        let sidecar_options = SidecarOptions {
            node_command: templates.sidecar.node_command.clone(),
            script: absolutize(&templates.sidecar.script),
            views_dir: views_dir.clone(),
            modules_dir,
            forbidden,
            auto_escape: templates.auto_escape,
            require_enabled: templates.require_enabled,
            workers: templates.sidecar.workers,
            startup_timeout: Duration::from_millis(templates.sidecar.startup_timeout_ms),
            request_timeout: Duration::from_millis(templates.sidecar.request_timeout_ms),
            render_budget: Duration::from_millis(templates.sidecar.render_budget_ms),
        };

        let renderer: std::sync::Arc<dyn TemplateRenderer>;
        let mut sidecar_spawn_error = None;
        match backend.as_str() {
            "boa" => {
                tracing::info!("template backend: boa (the in-process sandboxed engine)");
                renderer = boa;
            }
            "sidecar" => match SidecarRenderer::spawn(&sidecar_options) {
                Ok(sidecar) => {
                    // Strict, but not brittle: a crashed sidecar is
                    // re-spawned in the background (renders answer the
                    // transport error until the fresh one lands).
                    renderer =
                        std::sync::Arc::new(StrictSidecar::new(sidecar, sidecar_options.clone()));
                }
                Err(error) => {
                    sidecar_spawn_error = Some(error.clone());
                    renderer = std::sync::Arc::new(BrokenRenderer::new(error));
                }
            },
            _ => match SidecarRenderer::spawn(&sidecar_options) {
                Ok(sidecar) => {
                    tracing::info!("template backend: auto (Node sidecar, boa fallback)");
                    renderer = std::sync::Arc::new(AutoRenderer::new(
                        sidecar,
                        sidecar_options.clone(),
                        boa,
                    ));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "the JHS sidecar is unavailable; rendering falls back to the \
                         boa backend ([templates] backend = \"auto\")"
                    );
                    renderer = boa;
                }
            },
        };

        Self {
            renderer,
            backend,
            sidecar_spawn_error,
            views_dir,
            static_root: static_root.map(absolutize),
        }
    }

    /// A cloneable handle to the active rendering backend (for
    /// `spawn_blocking` renders).
    pub fn engine(&self) -> std::sync::Arc<dyn TemplateRenderer> {
        std::sync::Arc::clone(&self.renderer)
    }

    /// Startup gate for the strict sidecar backend: fails when the
    /// sidecar could not be launched or pass its selftest, so the
    /// server refuses to start instead of serving 500s. The `auto`
    /// backend has already fallen back (warned above) and `boa` is
    /// always ready.
    ///
    /// Public so embedders and integration tests can gate their own
    /// startup on the backend being live.
    pub fn ensure_ready(&self) -> Result<(), String> {
        if let Some(error) = &self.sidecar_spawn_error {
            return Err(format!(
                "templates.backend = \"sidecar\" ({}) but the Node sidecar could not \
                 start: {error}",
                self.backend
            ));
        }
        Ok(())
    }

    /// The configured views directory.
    pub fn views_dir(&self) -> &std::path::Path {
        &self.views_dir
    }

    /// The static root while `[static]` is enabled.
    pub fn static_root(&self) -> Option<&std::path::Path> {
        self.static_root.as_deref()
    }
}

/// Resolves a configured path into an absolute one: absolute paths pass
/// through unchanged, relative paths resolve against the current
/// working directory (the documented convention for `[static] root_dir`
/// and `[templates] views_dir`). Freezing the resolution at construction
/// keeps template candidate paths stable regardless of later CWD
/// changes and prevents the sandbox from re-joining an already-resolved
/// candidate onto the views path. The same resolution now backs the
/// media directory (F9): every handler joins onto a frozen absolute
/// path, never a re-resolved one.
pub(crate) fn absolutize(path: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

/// The boot-time virtual-host resolution (F17): what the serving
/// layers read instead of the configuration.
///
/// `main_root` and `cms_root` are the two bootstrap organizations'
/// document roots — the `organizations` rows on a database boot
/// (where the startup seed keeps them in step with `[static]
/// root_dir` and `[templates] views_dir`), the configuration values
/// themselves otherwise. `bindings` is the host resolution table:
/// the `domains` rows joined to their organizations on a database
/// boot, the `[cms] hosts` list otherwise.
///
/// Every path is resolved to an absolute one at construction — the
/// documented convention for `root_dir`/`views_dir` — so request
/// handling never re-resolves a root, and a candidate path is never
/// re-joined onto a relative one.
pub(crate) struct VhostData {
    /// The `main` organization's document root: the static tree's
    /// serving truth.
    pub(crate) main_root: String,
    /// The `cms` organization's document root: the views tree's
    /// serving truth (the template engine's views directory).
    pub(crate) cms_root: String,
    /// Every mapped hostname, in insertion order.
    pub(crate) bindings: Vec<HostBinding>,
}

impl VhostData {
    /// The configuration bootstrap (no database attached): the
    /// bootstrap organizations' roots from `[static]`/`[templates]`
    /// and the `[cms] hosts` list as CMS bindings — the pre-F17
    /// shape, byte for byte.
    fn from_config(config: &AppConfig) -> Self {
        let cms_root = absolutize(&config.templates.views_dir)
            .display()
            .to_string();
        Self {
            main_root: absolutize(&config.static_files.root_dir)
                .display()
                .to_string(),
            bindings: config
                .cms
                .hosts
                .iter()
                .map(|hostname| HostBinding {
                    hostname: hostname.clone(),
                    organization: String::from(CMS_ORGANIZATION_KEY),
                    document_root: cms_root.clone(),
                })
                .collect(),
            cms_root,
        }
    }

    /// The database boot (F17): the organizations' document roots
    /// and the `domains` table, both loaded after the seeders ran.
    /// The `main`/`cms` rows win over the configuration — they are
    /// the serving truth now — with the configured values as
    /// belt-and-braces fallbacks (the seed creates both rows before
    /// this runs; the fallback only guards a hand-mangled database).
    pub(crate) fn from_database(
        config: &AppConfig,
        roots: &[(String, String)],
        mut bindings: Vec<HostBinding>,
    ) -> Self {
        let root_of = |key: &str| {
            roots
                .iter()
                .find(|(organization, _)| organization == key)
                .map(|(_, root)| root.as_str())
        };
        let main_root = root_of(MAIN_ORGANIZATION_KEY)
            .unwrap_or(&config.static_files.root_dir)
            .to_owned();
        let cms_root = root_of(CMS_ORGANIZATION_KEY)
            .unwrap_or(&config.templates.views_dir)
            .to_owned();
        // Freeze every root into its absolute shape (relative paths
        // resolve against the working directory — the documented
        // convention; the boot never re-resolves them afterwards).
        for binding in &mut bindings {
            binding.document_root = absolutize(&binding.document_root).display().to_string();
        }
        Self {
            main_root: absolutize(&main_root).display().to_string(),
            cms_root: absolutize(&cms_root).display().to_string(),
            bindings,
        }
    }

    /// The mapped hostnames, in insertion order (the order the
    /// `cms_origin` derivation treats as canonical).
    pub(crate) fn bindings(&self) -> &[HostBinding] {
        &self.bindings
    }

    /// The `cms_origin` template global this resolution derives
    /// (F17): `cms.site_url` when set, else an origin from the first
    /// mapped CMS host, else the empty string.
    pub(crate) fn cms_origin(&self, config: &AppConfig) -> String {
        let cms_hosts: Vec<&str> = self
            .bindings
            .iter()
            .filter(|binding| binding.organization == CMS_ORGANIZATION_KEY)
            .map(|binding| binding.hostname.as_str())
            .collect();
        crate::vhosts::cms_origin(
            config.cms.site_url.as_deref(),
            &cms_hosts,
            config.tls.enabled,
            config.server.port,
        )
    }
}

/// The live virtual-host snapshot (F18): everything the request path
/// needs to classify and serve a host — the resolution data of
/// [`VhostData`] plus one ready router per tenant organization —
/// swapped **wholesale** behind a single write lock so a refresh is
/// atomic: a reader either sees the old table and the old trees, or
/// the new table and the new trees, never a mixture.
///
/// The boot installs the initial snapshot (trees included, by
/// [`crate::routes::vhost_routes`]); the Tenants pages re-derive a
/// whole snapshot from the database after every write and swap it
/// in, which is what makes a host move — or a tenant's whole site —
/// take effect without a restart.
#[derive(Clone)]
pub(crate) struct VhostSnapshot {
    /// The `main` organization's document root (see
    /// [`AppState::main_document_root`]).
    pub(crate) main_root: String,
    /// The `cms_origin` template global (see
    /// [`AppState::cms_origin`]).
    pub(crate) cms_origin: String,
    /// Every mapped hostname with its organization and that
    /// organization's document root (absolute).
    pub(crate) bindings: Vec<HostBinding>,
    /// One self-contained serving tree per tenant organization (the
    /// bootstrap two excluded — the main and CMS trees are mounted
    /// once at boot, their roots being the configuration's).
    pub(crate) tenants: Vec<(String, axum::Router)>,
}

impl VhostSnapshot {
    /// The snapshot a fresh state starts from: the boot resolution,
    /// no tenant trees yet ([`crate::routes::vhost_routes`] installs
    /// them right after — a state without them simply serves every
    /// host from the main tree, the single-host shape).
    fn from_data(data: VhostData, cms_origin: String) -> Self {
        Self {
            main_root: data.main_root,
            cms_origin,
            bindings: data.bindings,
            tenants: Vec::new(),
        }
    }
}

/// Cheap-to-clone shared application state (internally an `Arc`).
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
    /// Credential-level login throttling (see [`LoginThrottle`]):
    /// failed-login lockouts per (client IP, username) and per IP,
    /// independent of the request-level token bucket.
    login_throttle: LoginThrottle,
    security_headers: Vec<(HeaderName, HeaderValue)>,
    /// The resolved `[external_api]` proxy (endpoints plus client), or
    /// an inert instance while no endpoints are configured.
    external_api: ExternalApi,
    trusted_proxies: Vec<Cidr>,
    auth: Option<AuthContext>,
    metrics: Option<Metrics>,
    templates: Option<TemplateEngine>,
    cms: Option<CmsContext>,
    /// The live virtual-host snapshot (the data since F17, reloadable
    /// since F18): the host resolution table, the derived origins and
    /// one serving tree per tenant organization — everything behind
    /// one lock so a panel refresh swaps it atomically. See
    /// [`AppState::vhost_snapshot`].
    vhosts: std::sync::RwLock<VhostSnapshot>,
}

impl AppState {
    /// Creates a fresh application state from a validated configuration,
    /// **without** authentication services (previous-phase behaviour).
    pub fn new(config: AppConfig) -> Self {
        let vhosts = VhostData::from_config(&config);
        Self::build(config, None, None, vhosts)
    }

    /// Creates a fresh application state with the authentication services
    /// attached (see [`AuthContext`]).
    pub fn with_auth(config: AppConfig, auth: AuthContext) -> Self {
        let vhosts = VhostData::from_config(&config);
        Self::build(config, Some(auth), None, vhosts)
    }

    /// Creates a fresh application state with the authentication and CMS
    /// services attached (see [`AuthContext`] and [`CmsContext`]).
    pub fn with_cms(config: AppConfig, auth: AuthContext, cms: CmsContext) -> Self {
        let vhosts = VhostData::from_config(&config);
        Self::build(config, Some(auth), Some(cms), vhosts)
    }

    /// Creates a state whose virtual-host resolution came from the
    /// database (F17's database boot path) — the organizations'
    /// document roots and the `domains` table, loaded by
    /// [`crate::server::build_state`] after the seeders ran. The
    /// configuration stays the fallback for database-less states
    /// and the seeders' input.
    pub(crate) fn with_vhosts(
        config: AppConfig,
        auth: Option<AuthContext>,
        cms: Option<CmsContext>,
        vhosts: VhostData,
    ) -> Self {
        Self::build(config, auth, cms, vhosts)
    }

    /// Shared constructor for the public builders.
    ///
    /// `vhosts` is the boot-time resolution (F17): the
    /// organizations' document roots plus the host bindings — the
    /// configuration bootstrap for the public builders, the loaded
    /// `organizations`/`domains` rows for the database boot (see
    /// [`Self::with_vhosts`]).
    fn build(
        config: AppConfig,
        auth: Option<AuthContext>,
        cms: Option<CmsContext>,
        vhosts: VhostData,
    ) -> Self {
        let security_headers = config.security_headers.header_pairs();
        // The `[external_api]` resolution fails only for states built
        // programmatically past `AppConfig::load` (file-backed startup
        // already validated and resolved the section). Instead of
        // panicking, the feature is disabled loudly — `build_state`
        // double-checks and turns this into a hard startup error for
        // the real binary.
        let external_api = ExternalApi::resolve(&config.external_api).unwrap_or_else(|error| {
            tracing::error!(
                %error,
                "external API proxy disabled: endpoints failed to resolve"
            );
            ExternalApi::disabled()
        });
        let rate_limiter = RateLimiter::new(
            config.rate_limit.capacity,
            config.rate_limit.refill_per_second,
        );
        let login_throttle = LoginThrottle::new();
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

        let templates = if config.templates.enabled {
            // F17: the serving roots come from the boot-time
            // resolution — the organizations' rows on a database
            // boot, the configuration otherwise (the seed keeps the
            // two in step, so this changes nothing for existing
            // setups while making the rows the truth).
            let static_root = config
                .static_files
                .enabled
                .then_some(vhosts.main_root.as_str());
            Some(TemplateEngine::new(
                &config.templates,
                &vhosts.cms_root,
                static_root,
            ))
        } else {
            None
        };

        // The `cms_origin` global (F17): `cms.site_url` when set, else
        // an origin derived from the first mapped CMS host — the
        // domains table's insertion order on a database boot, the
        // `[cms] hosts` list otherwise. F18 recomputes it with the
        // same derivation on every snapshot refresh.
        let cms_origin = vhosts.cms_origin(&config);

        Self {
            inner: Arc::new(StateInner {
                config,
                started_at: Instant::now(),
                requests_served: AtomicU64::new(0),
                rate_limited_requests: AtomicU64::new(0),
                rate_limiter,
                login_throttle,
                security_headers,
                external_api,
                trusted_proxies,
                auth,
                metrics,
                templates,
                cms,
                vhosts: std::sync::RwLock::new(VhostSnapshot::from_data(vhosts, cms_origin)),
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

    /// Returns the shared login brute-force throttle.
    pub fn login_throttle(&self) -> &LoginThrottle {
        &self.inner.login_throttle
    }

    /// Returns the security header pairs applied to every response.
    pub fn security_headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.inner.security_headers
    }

    /// Returns the resolved `[external_api]` proxy state (endpoints,
    /// secrets and shared client); inert while no endpoints are
    /// configured.
    pub fn external_api(&self) -> &ExternalApi {
        &self.inner.external_api
    }

    /// Whether the external API proxy routes are mounted (at least one
    /// `[[external_api.endpoints]]` entry is configured and resolved).
    pub fn external_api_enabled(&self) -> bool {
        self.inner.external_api.enabled()
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

    /// The main organization's document root (F17): the static
    /// tree's serving truth — the `main` organization's row on a
    /// database boot (kept in step with `[static] root_dir` by the
    /// startup seed), the configuration value otherwise. Absolute,
    /// resolved at boot.
    pub fn main_document_root(&self) -> String {
        self.vhost_snapshot().main_root
    }

    /// The host resolution table (F17, live since F18): every mapped
    /// hostname with its organization and that organization's
    /// document root — the `domains` table joined to the
    /// `organizations` rows on a database boot (seeded from
    /// `[cms] hosts`), the `[cms] hosts` list itself while no
    /// database is attached. The dispatcher and the templates
    /// middleware both classify requests against this table — never
    /// the configuration — so the two layers agree on every request
    /// by construction. A panel write swaps the whole snapshot (see
    /// [`Self::replace_vhosts`]), so what this returns is always the
    /// table the next request will be classified with.
    pub fn host_bindings(&self) -> Vec<HostBinding> {
        self.vhost_snapshot().bindings
    }

    /// The `cms_origin` template global (F14 follow-up, data-driven
    /// since F17): `cms.site_url` when set, else an origin derived
    /// from the first mapped CMS host (the domains table's insertion
    /// order on a database boot), else the empty string — the
    /// single-host server needs no cross-host links. Derived at boot
    /// and re-derived on every snapshot refresh (F18).
    pub fn cms_origin(&self) -> String {
        self.vhost_snapshot().cms_origin
    }

    /// The `Content-Security-Policy` value with `form-action` widened
    /// to the round-trip family (F19's follow-up), or `None` while the
    /// configured policy needs no widening — the precomputed pair from
    /// [`Self::security_headers`] then stands.
    ///
    /// A form's `303` may only cross the `Host` line when the browser's
    /// CSP lets the redirect target through; `form-action 'self'`
    /// (the shipped default) pins it to one origin, which swallowed
    /// the round trip's navigation whole — the sign-in succeeded, the
    /// cookie was stored, and the browser never left the login page.
    /// The widening adds exactly the family the `redirect` allowlist
    /// trusts (see [`crate::routes::auth`]): every host the live vhost
    /// table maps plus, while the shared session is on, the
    /// `cookie_domain` and its subdomains — never a foreign origin.
    /// A policy whose `form-action` names other sources, or carries
    /// none, is the operator's own and is left as written.
    pub fn form_action_csp(&self) -> Option<HeaderValue> {
        let family = self.form_action_sources();
        if family.is_empty() {
            return None;
        }
        let policy = &self.config().security_headers.content_security_policy;
        crate::middleware::security_headers::expand_form_action(policy, &family)
            .and_then(|widened| HeaderValue::from_str(&widened).ok())
    }

    /// The origins the round trip may move forms between, as CSP
    /// source expressions: the shared session's `cookie_domain` (the
    /// domain itself and its subdomains) while set, plus every host
    /// the live vhost table maps that the domain does not cover — a
    /// tenant served under its own name still gets its returning
    /// redirects. The scheme follows `[tls]`, the port `[server]`
    /// (omitted on the scheme's default and on the ephemeral test
    /// port, the same convention `cms_origin` derives by).
    fn form_action_sources(&self) -> Vec<String> {
        let config = self.config();
        let domain = config.auth.cookie_domain.as_str();
        let bindings = self.host_bindings();
        if domain.is_empty() && bindings.is_empty() {
            return Vec::new();
        }
        let scheme = if config.tls.enabled { "https" } else { "http" };
        let default_port = if config.tls.enabled { 443 } else { 80 };
        let port = config.server.port;
        let port_suffix = if port != 0 && port != default_port {
            format!(":{port}")
        } else {
            String::new()
        };

        let mut sources = Vec::new();
        if !domain.is_empty() {
            sources.push(format!("{scheme}://{domain}{port_suffix}"));
            sources.push(format!("{scheme}://*.{domain}{port_suffix}"));
        }
        for binding in &bindings {
            let host = binding.hostname.to_ascii_lowercase();
            let under_domain =
                !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")));
            if !under_domain {
                sources.push(format!("{scheme}://{host}{port_suffix}"));
            }
        }
        sources.sort();
        sources.dedup();
        sources
    }

    /// A clone of the live virtual-host snapshot (F18): the host
    /// resolution table, the derived origins, and one ready serving
    /// tree per tenant organization. What the dispatcher and the
    /// templates middleware classify and route every request with.
    ///
    /// The clone is cheap by construction — the bindings are a
    /// handful of small rows and a router is a shared handle — the
    /// same trade the F16 dispatcher already made with its captured
    /// host list.
    pub(crate) fn vhost_snapshot(&self) -> VhostSnapshot {
        // Poison-tolerant: a panic mid-swap leaves a structurally
        // complete snapshot behind (the write is a plain assignment),
        // and refusing to serve the whole server over a lock flag
        // would be the worse failure.
        self.inner
            .vhosts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Swaps the live virtual-host snapshot wholesale (F18): the one
    /// write path, so the table and the tenant trees it routes to
    /// always change together — atomic for every reader.
    ///
    /// The boot installs the initial snapshot's trees this way (via
    /// [`crate::routes::vhost_routes`]); the Tenants pages call it
    /// after their writes, with a snapshot re-derived from the
    /// database (see [`crate::routes::refresh_vhost_state`]).
    pub(crate) fn replace_vhosts(&self, replacement: VhostSnapshot) {
        // Poison-tolerant, same reasoning as the read side.
        *self
            .inner
            .vhosts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = replacement;
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

    /// Returns the template rendering services when the `[templates]`
    /// feature is enabled.
    pub fn templates(&self) -> Option<&TemplateEngine> {
        self.inner.templates.as_ref()
    }

    /// Whether dynamic template rendering is enabled.
    pub fn templates_enabled(&self) -> bool {
        self.inner.templates.is_some()
    }

    /// The effective `.jhs` rendering backend (v0.10.1): `None` while
    /// `[templates]` is disabled, otherwise the live identity behind
    /// [`TemplateRenderer::backend_name`] — `"boa"` or `"sidecar"`
    /// (`"auto"` resolves to whichever half is currently serving).
    /// Backs the `template_backend` field of `GET /health` and the
    /// `wallermax_template_backend` Prometheus gauge; a pure
    /// in-memory read, safe for liveness probes.
    pub fn template_backend(&self) -> Option<&'static str> {
        self.inner
            .templates
            .as_ref()
            .map(|engine| engine.engine().backend_name())
    }

    /// Returns the CMS services when the `[cms]` feature is enabled
    /// (which additionally requires `[database]`, `[auth]` and
    /// `[templates]`).
    pub fn cms(&self) -> Option<&CmsContext> {
        self.inner.cms.as_ref()
    }

    /// Whether the CMS routes are mounted.
    pub fn cms_enabled(&self) -> bool {
        self.inner.cms.is_some()
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

        async fn count_with_role(
            &self,
            _role: crate::db::UserRole,
        ) -> Result<i64, crate::db::RepositoryError> {
            Ok(0)
        }

        async fn membership_role(
            &self,
            _user_id: i64,
            _organization_key: &str,
        ) -> Result<Option<crate::db::UserRole>, crate::db::RepositoryError> {
            Ok(None)
        }

        async fn list(
            &self,
            _limit: i64,
        ) -> Result<Vec<crate::db::User>, crate::db::RepositoryError> {
            Ok(Vec::new())
        }

        async fn update_password(
            &self,
            _id: i64,
            _password_hash: &str,
        ) -> Result<(), crate::db::RepositoryError> {
            Ok(())
        }

        async fn update_role(
            &self,
            _id: i64,
            _role: crate::db::UserRole,
        ) -> Result<(), crate::db::RepositoryError> {
            Ok(())
        }

        async fn delete(&self, _id: i64) -> Result<(), crate::db::RepositoryError> {
            Ok(())
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
        let body = metrics.render(1.0, None, None).expect("renders");
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
