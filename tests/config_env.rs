//! Environment-variable override semantics.
//!
//! Kept in its own integration-test binary (a separate process) so the
//! mutated environment cannot interfere with other tests.
//!
//! [`AppConfig::load`] reads more than the checked-in file, though: it
//! also merges `wallermax.local.toml` from the working directory (the
//! shipped configuration recommends one for F14 host testing) and every
//! ambient `WALLERMAX_*` variable. A test that flips `static.enabled`
//! off would then collide with a local `[cms] hosts` entry through the
//! F14 validation — a failure that has nothing to do with the override
//! under test. The five tests also mutated the environment from
//! parallel threads, and the environment is process-global state: one
//! test's variables could bleed into another's loads.
//!
//! Each test therefore opens a [`ControlledConfigChain`], which
//! serializes the binary and pins the chain: ambient `WALLERMAX_*`
//! variables cleared, the working directory moved to an empty
//! temporary one (no local override file can exist there) and
//! `WALLERMAX_CONFIG` aimed at the checked-in `wallermax.toml` by
//! absolute path. The file the assertions describe and the test's own
//! variables become the only inputs — on every machine, CI included.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use wallermax_server::config::{AppConfig, LogFormat, RequestIdMode};

/// Serializes the tests in this binary: the process environment and the
/// working directory are process-global state.
static CONFIG_CHAIN: Mutex<()> = Mutex::new(());

/// Names the empty temporary working directories (one per test).
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Pins [`AppConfig::load`]'s inputs for one test.
///
/// While alive, the configuration chain is exactly: built-in defaults,
/// the checked-in `wallermax.toml` (by absolute path, through
/// `WALLERMAX_CONFIG`) and the environment variables the test itself
/// sets. Drop restores the process to how it was found — original
/// working directory first, because Windows refuses to delete the
/// process's working directory.
struct ControlledConfigChain {
    saved_cwd: PathBuf,
    saved_vars: Vec<(String, String)>,
    temp_dir: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl ControlledConfigChain {
    /// Takes the process-wide lock and isolates the configuration chain.
    fn isolate() -> Self {
        let _lock = CONFIG_CHAIN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Every ambient WALLERMAX_* variable goes — the shell (or the
        // CI) may carry leftovers, `WALLERMAX_CONFIG` included.
        let saved_vars: Vec<(String, String)> = std::env::vars()
            .filter(|(name, _)| name.starts_with("WALLERMAX"))
            .collect();
        for (name, _) in &saved_vars {
            std::env::remove_var(name);
        }

        // An empty temporary working directory: `wallermax.local.toml`
        // cannot exist there, so the checked-in file is the only file
        // the chain reads.
        let unique = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!(
            "wallermax-config-env-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).expect("empty temporary directory");
        let saved_cwd = std::env::current_dir().expect("current directory");
        std::env::set_current_dir(&temp_dir).expect("move into the temporary directory");

        // The checked-in file, resolved absolutely: moving the working
        // directory away from the repository must not move the base
        // configuration with it.
        std::env::set_var(
            "WALLERMAX_CONFIG",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("wallermax.toml"),
        );

        Self {
            saved_cwd,
            saved_vars,
            temp_dir,
            _lock,
        }
    }
}

impl Drop for ControlledConfigChain {
    fn drop(&mut self) {
        // Working directory first: Windows refuses to delete the
        // process's working directory, so the temporary one has to be
        // vacated before it can be removed.
        let _ = std::env::set_current_dir(&self.saved_cwd);
        for (name, value) in &self.saved_vars {
            std::env::set_var(name, value);
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

#[test]
fn environment_overrides_file_and_defaults() {
    let _chain = ControlledConfigChain::isolate();

    // The checked-in `wallermax.toml` ships port = 8080; the environment wins.
    std::env::set_var("WALLERMAX_SERVER__PORT", "9123");
    std::env::set_var("WALLERMAX_SERVER__HOST", "0.0.0.0");
    std::env::set_var("WALLERMAX_LOGGING__FORMAT", "json");
    std::env::set_var("WALLERMAX_MIDDLEWARE__TIMEOUT", "false");

    let config = AppConfig::load().expect("configuration loads");

    assert_eq!(config.server.port, 9123);
    assert_eq!(config.server.host.to_string(), "0.0.0.0");
    assert_eq!(config.logging.format, LogFormat::Json);
    assert!(!config.middleware.timeout);
    assert!(
        config.middleware.request_id,
        "untouched keys keep file values"
    );
}

#[test]
fn environment_overrides_phase2_sections() {
    let _chain = ControlledConfigChain::isolate();

    std::env::set_var("WALLERMAX_SERVER__MAX_BODY_SIZE_BYTES", "4096");
    std::env::set_var("WALLERMAX_REQUEST_ID__MODE", "overwrite");
    std::env::set_var("WALLERMAX_RATE_LIMIT__CAPACITY", "100");
    std::env::set_var("WALLERMAX_RATE_LIMIT__REFILL_PER_SECOND", "5.5");
    std::env::set_var("WALLERMAX_MIDDLEWARE__RATE_LIMIT", "true");
    std::env::set_var("WALLERMAX_MIDDLEWARE__CORS", "false");
    std::env::set_var("WALLERMAX_SECURITY_HEADERS__X_FRAME_OPTIONS", "SAMEORIGIN");

    let config = AppConfig::load().expect("configuration loads");

    assert_eq!(config.server.max_body_size_bytes, 4096);
    assert_eq!(config.request_id.mode, RequestIdMode::Overwrite);
    assert_eq!(config.rate_limit.capacity, 100);
    assert_eq!(config.rate_limit.refill_per_second, 5.5);
    // wallermax.toml enables rate limiting; the env var agrees here.
    assert!(config.middleware.rate_limit);
    assert!(!config.middleware.cors);
    assert_eq!(config.security_headers.x_frame_options, "SAMEORIGIN");
}

#[test]
fn environment_overrides_phase3_sections() {
    let _chain = ControlledConfigChain::isolate();

    // The checked-in `wallermax.toml` ships auth + database enabled with a
    // development secret; the environment wins over both.
    std::env::set_var(
        "WALLERMAX_AUTH__JWT_SECRET",
        "env-override-secret-0123456789abcdef",
    );
    std::env::set_var("WALLERMAX_AUTH__TOKEN_TTL_SECS", "7200");
    std::env::set_var("WALLERMAX_AUTH__REGISTRATION_ENABLED", "false");
    std::env::set_var(
        "WALLERMAX_DATABASE__URL",
        "sqlite://env-override.db?mode=rwc",
    );
    std::env::set_var("WALLERMAX_DATABASE__MAX_CONNECTIONS", "3");

    let config = AppConfig::load().expect("configuration loads");

    assert_eq!(
        config.auth.jwt_secret,
        "env-override-secret-0123456789abcdef"
    );
    assert_eq!(config.auth.token_ttl_secs, 7200);
    assert!(!config.auth.registration_enabled);
    assert_eq!(config.database.url, "sqlite://env-override.db?mode=rwc");
    assert_eq!(config.database.max_connections, 3);
    // Untouched keys keep their file values.
    assert!(config.auth.enabled);
    assert_eq!(config.auth.min_password_len, 8);
}

#[test]
fn environment_overrides_cms_section() {
    let _chain = ControlledConfigChain::isolate();

    // The checked-in `wallermax.toml` ships the CMS enabled without a
    // default page; the environment wins over the enabled switch and
    // adds the v0.11.0 homepage takeover.
    std::env::set_var("WALLERMAX_CMS__ENABLED", "false");
    std::env::set_var("WALLERMAX_CMS__DEFAULT_PAGE", "inicio");

    let config = AppConfig::load().expect("configuration loads");

    assert!(!config.cms.enabled);
    assert_eq!(
        config.cms.default_page.as_deref(),
        Some("inicio"),
        "the slug string survives try_parsing"
    );
    // `load()` already validated the merged configuration (the slug
    // shape survives the env round trip).
}

#[test]
fn environment_overrides_static_section() {
    let _chain = ControlledConfigChain::isolate();

    // The checked-in `wallermax.toml` ships static serving enabled with
    // root_dir = "public"; the environment wins over each value.
    std::env::set_var("WALLERMAX_STATIC__ENABLED", "false");
    std::env::set_var("WALLERMAX_STATIC__ROOT_DIR", "site");
    std::env::set_var("WALLERMAX_STATIC__INDEX_FILE", "home.html");

    let config = AppConfig::load().expect("configuration loads");

    assert!(!config.static_files.enabled);
    assert_eq!(config.static_files.root_dir, "site");
    assert_eq!(config.static_files.index_file, "home.html");
    // `load()` already validated the merged configuration — including
    // the F14 rule that `cms.hosts` requires `static.enabled`, which is
    // exactly why no local override file may sit in the chain here.
}
