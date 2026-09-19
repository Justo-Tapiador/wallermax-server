//! The manager's own settings — a small JSON document that survives
//! restarts: which command starts the server, where the configuration
//! lives, which origin serves health/metrics/stats, and how patient the
//! boot probe and the stop grace are.
//!
//! Loading is deliberately **tolerant of the future and loud about
//! corruption**: unknown fields from newer manager versions are ignored
//! (an upgrade must never brick the settings file), a missing file means
//! defaults, but a present-yet-corrupt file is reported as an error
//! instead of being silently replaced.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The manager settings document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Command that starts the server, e.g.
    /// `C:\wallermax\target\release\wallermax-server.exe` or
    /// `wallermax-server`. Split on spaces outside double quotes by
    /// [`crate::process_manager::split_command_line`]; point it at the
    /// binary itself, not at `cargo run`, so the manager tracks the real
    /// server process.
    pub server_command: String,
    /// Directory the server runs in — the base for its relative paths
    /// (`wallermax.toml`, the database, roots). Empty means the directory
    /// holding the configuration.
    pub working_dir: String,
    /// Directory holding `wallermax.toml` / `wallermax.local.toml`.
    /// Empty makes the manager search upward from its own working
    /// directory for `wallermax.toml`.
    pub config_dir: String,
    /// Base origin the manager talks to: health, metrics and stats URLs
    /// are all derived from it, e.g. `http://127.0.0.1:8080`.
    pub origin: String,
    /// The website the dashboard links to while the server runs —
    /// e.g. `https://localhost` when TLS terminates at the server
    /// itself. Empty means "the origin" (the common case: the site and
    /// the management endpoints share one address). Unlike the origin
    /// this one is opened in a browser, so `https://` is allowed.
    pub site_url: String,
    /// Path of the health endpoint relative to `origin`.
    pub health_path: String,
    /// Path of the Prometheus endpoint relative to `origin`.
    pub metrics_path: String,
    /// Path of the stats endpoint relative to `origin`.
    pub stats_path: String,
    /// How long the boot probe watches a fresh server, in milliseconds,
    /// before accepting a silent survivor as booted.
    pub probe_window_ms: u64,
    /// How long [`crate::process_manager::ProcessManager::stop`] waits
    /// after the polite signal before escalating, in milliseconds.
    pub stop_grace_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_command: String::new(),
            working_dir: String::new(),
            config_dir: String::new(),
            origin: String::from("http://127.0.0.1:8080"),
            site_url: String::new(),
            health_path: String::from("/health"),
            metrics_path: String::from("/metrics"),
            stats_path: String::from("/api/stats"),
            probe_window_ms: 4_000,
            stop_grace_ms: 5_000,
        }
    }
}

impl Settings {
    /// Loads the settings from `path`.
    ///
    /// A missing file is the pristine first run: the defaults answer.
    /// A present but unreadable or malformed file is corruption and is
    /// reported loudly — the caller decides whether to surface a banner
    /// or refuse to continue, but the file is never silently overwritten.
    ///
    /// # Errors
    ///
    /// Returns a human-readable error when the file exists but cannot be
    /// read or parsed as JSON.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(format!(
                    "could not read the settings file {}: {error}",
                    path.display()
                ));
            }
        };
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&raw).map_err(|error| {
            format!(
                "the settings file {} is not valid JSON: {error}",
                path.display()
            )
        })
    }

    /// Persists the settings to `path` as pretty JSON, atomically (write
    /// to a sibling temporary, then rename over the target).
    ///
    /// # Errors
    ///
    /// Returns a human-readable error when the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body = serde_json::to_string_pretty(self)
            .map_err(|error| format!("could not encode the settings: {error}"))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
            }
        }
        write_atomically(path, body.as_bytes())
    }

    /// The health URL, derived from the shared `origin`.
    pub fn health_url(&self) -> String {
        self.url(&self.health_path)
    }

    /// The Prometheus URL, derived from the shared `origin`.
    pub fn metrics_url(&self) -> String {
        self.url(&self.metrics_path)
    }

    /// The stats URL, derived from the shared `origin`.
    pub fn stats_url(&self) -> String {
        self.url(&self.stats_path)
    }

    /// The website URL shown while the server runs: `site_url` when
    /// set, the shared origin otherwise.
    pub fn site_url(&self) -> String {
        let site = self.site_url.trim();
        if site.is_empty() {
            self.origin.trim_end_matches('/').to_owned()
        } else {
            site.trim_end_matches('/').to_owned()
        }
    }

    /// Joins `path` onto the origin, tolerating trailing slashes on
    /// either side and missing leading slashes on the path.
    fn url(&self, path: &str) -> String {
        let origin = self.origin.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        format!("{origin}/{path}")
    }

    /// Where the settings live on this platform: the conventional
    /// application-data directory under `wallermax-manager/`.
    pub fn default_settings_path() -> PathBuf {
        config_home()
            .join("wallermax-manager")
            .join("settings.json")
    }
}

/// The per-platform application-configuration home.
fn config_home() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support"))
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Writes `bytes` to `path` atomically: a uniquely named sibling is
/// written first, then renamed over the destination.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension(format!(
        "tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    fs::write(&temp, bytes)
        .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
    fs::rename(&temp, path)
        .map_err(|error| format!("could not replace {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_means_defaults() {
        let dir = crate::testutil::temp_dir("settings-missing");
        let path = dir.join("settings.json");
        assert!(!path.exists());
        let loaded = Settings::load(&path).expect("a missing file loads as defaults");
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn roundtrips_through_json() {
        let dir = crate::testutil::temp_dir("settings-roundtrip");
        let path = dir.join("settings.json");
        let settings = Settings {
            server_command: String::from("C:/wallermax/wallermax-server.exe"),
            working_dir: String::from("C:/wallermax"),
            config_dir: String::from("C:/wallermax"),
            origin: String::from("http://127.0.0.1:9000/"),
            site_url: String::from("https://localhost"),
            health_path: String::from("/health"),
            metrics_path: String::from("/metrics"),
            stats_path: String::from("/api/stats"),
            probe_window_ms: 1_500,
            stop_grace_ms: 2_500,
        };
        settings.save(&path).expect("save works");
        let loaded = Settings::load(&path).expect("the saved file loads");
        assert_eq!(loaded, settings);
    }

    #[test]
    fn a_corrupt_file_complains_loudly() {
        let dir = crate::testutil::temp_dir("settings-corrupt");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ this is not json").expect("seed the corrupt file");
        let error = Settings::load(&path).expect_err("corruption must surface");
        assert!(!error.is_empty(), "the complaint carries a message");
        assert!(
            error.contains("not valid JSON"),
            "the complaint names the problem: {error}"
        );
    }

    #[test]
    fn unknown_fields_from_newer_versions_are_tolerated() {
        let dir = crate::testutil::temp_dir("settings-future");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            "{\"origin\":\"http://127.0.0.1:9999\",\"some_future_field\":[1,2,3]}",
        )
        .expect("seed a settings file from a newer manager");
        let loaded = Settings::load(&path).expect("unknown fields do not fail the load");
        assert_eq!(loaded.origin, "http://127.0.0.1:9999");
        assert_eq!(loaded.probe_window_ms, Settings::default().probe_window_ms);
    }

    #[test]
    fn urls_join_the_shared_origin_cleanly() {
        let settings = Settings {
            origin: String::from("http://127.0.0.1:8080/"),
            health_path: String::from("health"),
            metrics_path: String::from("/metrics"),
            stats_path: String::from("/api/stats"),
            ..Settings::default()
        };
        assert_eq!(settings.health_url(), "http://127.0.0.1:8080/health");
        assert_eq!(settings.metrics_url(), "http://127.0.0.1:8080/metrics");
        assert_eq!(settings.stats_url(), "http://127.0.0.1:8080/api/stats");
    }

    #[test]
    fn the_site_url_falls_back_to_the_origin() {
        let mut settings = Settings::default();
        assert_eq!(settings.site_url(), settings.origin);
        settings.origin = String::from("http://127.0.0.1:9000/");
        assert_eq!(settings.site_url(), "http://127.0.0.1:9000");
        settings.site_url = String::from("  https://localhost  ");
        assert_eq!(settings.site_url(), "https://localhost");
    }
}

// `write_atomically` is shared with the config manager; keep it compiled
// for tests and the binary alike (it lives here because settings needs it
// first).
