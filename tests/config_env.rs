//! Environment-variable override semantics.
//!
//! Kept in its own integration-test binary (a separate process) so the
//! mutated environment cannot interfere with other tests.

use wallermax_server::config::{AppConfig, LogFormat, RequestIdMode};

#[test]
fn environment_overrides_file_and_defaults() {
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

    std::env::remove_var("WALLERMAX_SERVER__PORT");
    std::env::remove_var("WALLERMAX_SERVER__HOST");
    std::env::remove_var("WALLERMAX_LOGGING__FORMAT");
    std::env::remove_var("WALLERMAX_MIDDLEWARE__TIMEOUT");
}

#[test]
fn environment_overrides_phase2_sections() {
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

    std::env::remove_var("WALLERMAX_SERVER__MAX_BODY_SIZE_BYTES");
    std::env::remove_var("WALLERMAX_REQUEST_ID__MODE");
    std::env::remove_var("WALLERMAX_RATE_LIMIT__CAPACITY");
    std::env::remove_var("WALLERMAX_RATE_LIMIT__REFILL_PER_SECOND");
    std::env::remove_var("WALLERMAX_MIDDLEWARE__RATE_LIMIT");
    std::env::remove_var("WALLERMAX_MIDDLEWARE__CORS");
    std::env::remove_var("WALLERMAX_SECURITY_HEADERS__X_FRAME_OPTIONS");
}

#[test]
fn environment_overrides_phase3_sections() {
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

    std::env::remove_var("WALLERMAX_AUTH__JWT_SECRET");
    std::env::remove_var("WALLERMAX_AUTH__TOKEN_TTL_SECS");
    std::env::remove_var("WALLERMAX_AUTH__REGISTRATION_ENABLED");
    std::env::remove_var("WALLERMAX_DATABASE__URL");
    std::env::remove_var("WALLERMAX_DATABASE__MAX_CONNECTIONS");
}

#[test]
fn environment_overrides_static_section() {
    // The checked-in `wallermax.toml` ships static serving enabled with
    // root_dir = "public"; the environment wins over each value.
    std::env::set_var("WALLERMAX_STATIC__ENABLED", "false");
    std::env::set_var("WALLERMAX_STATIC__ROOT_DIR", "site");
    std::env::set_var("WALLERMAX_STATIC__INDEX_FILE", "home.html");

    let config = AppConfig::load().expect("configuration loads");

    assert!(!config.static_files.enabled);
    assert_eq!(config.static_files.root_dir, "site");
    assert_eq!(config.static_files.index_file, "home.html");
    // `load()` already validated the merged configuration.

    std::env::remove_var("WALLERMAX_STATIC__ENABLED");
    std::env::remove_var("WALLERMAX_STATIC__ROOT_DIR");
    std::env::remove_var("WALLERMAX_STATIC__INDEX_FILE");
}
