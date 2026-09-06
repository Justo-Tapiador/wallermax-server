//! `tracing` subscriber setup.
//!
//! The output format follows `logging.format` (`pretty`, `json` or
//! `compact`) and the level follows `logging.level`. When `RUST_LOG` is set
//! it takes precedence, so standard Rust tooling keeps working.

use tracing_subscriber::EnvFilter;

use crate::config::{AppConfig, LogFormat};

/// Installs the global tracing subscriber.
///
/// Called exactly once, at startup, before the server begins serving.
pub fn init(config: &AppConfig) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.logging.level.clone()));

    match config.logging.format {
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .pretty()
                .init();
        }
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        LogFormat::Compact => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .compact()
                .init();
        }
    }
}
