//! The facade the Tauri commands delegate to: one object that owns the
//! settings, the configuration pair and the supervised process, and
//! exposes them as plain, lock-light methods.
//!
//! [`App`] is `Send + Sync` (all mutation goes through interior locks) so
//! the shell manages a single `Arc<App>` and every command is a one-line
//! delegation. Slow operations — the boot probe inside
//! [`App::start_server`], the endpoint round-trips inside
//! [`App::fetch_dashboard`] — never hold a lock while they wait, so the
//! status and log polling the UI does stays responsive throughout.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::config_manager::{BackupInfo, ConfigManager, ConfigWhich, BASE_FILE};
use crate::metrics::{self, MetricsSnapshot};
use crate::process_manager::{self, ProcessManager, ProcessStatus, SpawnSpec, StartReport};
use crate::settings::Settings;

/// The dashboard's flat, ready-to-show answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DashboardSnapshot {
    /// Whether any endpoint answered at all.
    pub reachable: bool,
    /// The first problem seen, when nothing answered or something broke.
    pub error: Option<String>,
    pub uptime_seconds: Option<f64>,
    pub requests_total: Option<f64>,
    pub requests_per_second: Option<f64>,
    pub rate_limited_requests: Option<f64>,
    pub registered_users: Option<f64>,
    /// The server's own version string, from `/api/stats`.
    pub version: Option<String>,
}

/// The whole manager, minus the window.
pub struct App {
    settings_path: PathBuf,
    settings: Mutex<Settings>,
    config_dir: Mutex<PathBuf>,
    notice: Mutex<Option<String>>,
    process: ProcessManager,
}

impl App {
    /// Builds an app around explicit inputs — the constructor the tests
    /// and any future embedding use.
    pub fn with_settings(
        settings: Settings,
        settings_path: impl Into<PathBuf>,
        config_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            settings_path: settings_path.into(),
            settings: Mutex::new(settings),
            config_dir: Mutex::new(config_dir.into()),
            notice: Mutex::new(None),
            process: ProcessManager::new(),
        }
    }

    /// Discovers everything: loads the settings from their conventional
    /// path (a corrupt file surfaces as a banner instead of a dead app)
    /// and resolves the configuration directory — the setting when set,
    /// otherwise the nearest ancestor directory holding
    /// `wallermax.toml`, otherwise the current directory with a notice.
    pub fn discover() -> Self {
        let settings_path = Settings::default_settings_path();
        let (settings, mut notice) = match Settings::load(&settings_path) {
            Ok(settings) => (settings, None),
            Err(error) => (Settings::default(), Some(error)),
        };

        let configured = (!settings.config_dir.trim().is_empty())
            .then(|| PathBuf::from(settings.config_dir.trim()));
        let (config_dir, discovered) = match configured {
            Some(dir) => (dir, true),
            None => {
                discover_config_dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            }
        };
        if !discovered {
            let hint = format!(
                "no {BASE_FILE} was found near the working directory — set the \
                 configuration directory in Settings"
            );
            notice = Some(match notice {
                Some(existing) => format!("{existing}. {hint}"),
                None => hint,
            });
        }

        Self {
            settings_path,
            settings: Mutex::new(settings),
            config_dir: Mutex::new(config_dir),
            notice: Mutex::new(notice),
            process: ProcessManager::new(),
        }
    }

    /// A message the UI should banner on startup (corrupt settings, an
    /// undiscovered configuration directory, ...), if any.
    pub fn notice(&self) -> Option<String> {
        self.notice.lock().expect("the notice lock is live").clone()
    }

    /// Where the settings live.
    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    /// The current settings.
    pub fn settings(&self) -> Settings {
        self.settings
            .lock()
            .expect("the settings lock is live")
            .clone()
    }

    /// Validates, persists and applies new settings. The origin must be
    /// an `http://` URL and the endpoint paths must start with `/`; the
    /// configuration directory only changes when a value is given, so
    /// clearing the field keeps the discovered directory.
    ///
    /// # Errors
    ///
    /// The first invalid field, or filesystem problems from the save.
    pub fn save_settings(&self, incoming: Settings) -> Result<(), String> {
        if !incoming.origin.trim().starts_with("http://") {
            return Err(format!(
                "the origin must be an `http://` URL (the manager talks to the local \
                 server); got `{}`",
                incoming.origin
            ));
        }
        for (name, path) in [
            ("health", incoming.health_path.as_str()),
            ("metrics", incoming.metrics_path.as_str()),
            ("stats", incoming.stats_path.as_str()),
        ] {
            if !path.starts_with('/') {
                return Err(format!("the {name} path must start with `/`: `{path}`"));
            }
        }
        if incoming.probe_window_ms == 0 || incoming.stop_grace_ms == 0 {
            return Err(String::from(
                "the probe window and the stop grace must be at least one millisecond",
            ));
        }

        incoming.save(&self.settings_path)?;
        let mut current = self.settings.lock().expect("the settings lock is live");
        if !incoming.config_dir.trim().is_empty() {
            *self.config_dir.lock().expect("the config-dir lock is live") =
                PathBuf::from(incoming.config_dir.trim());
            *self.notice.lock().expect("the notice lock is live") = None;
        }
        *current = incoming;
        Ok(())
    }

    /// The configuration manager for the current directory.
    pub fn config(&self) -> ConfigManager {
        ConfigManager::new(
            self.config_dir
                .lock()
                .expect("the config-dir lock is live")
                .clone(),
        )
    }

    // ----- configuration -------------------------------------------------

    /// Whether one of the two files exists.
    pub fn config_exists(&self, which: ConfigWhich) -> bool {
        self.config().exists(which)
    }

    /// Reads one layer as text.
    ///
    /// # Errors
    ///
    /// See [`ConfigManager::read`].
    pub fn config_read(&self, which: ConfigWhich) -> Result<String, String> {
        self.config().read(which)
    }

    /// Validates and writes one layer as text.
    ///
    /// # Errors
    ///
    /// See [`ConfigManager::write_raw`].
    pub fn config_write(&self, which: ConfigWhich, content: &str) -> Result<(), String> {
        self.config().write_raw(which, content)
    }

    /// Validates the pair currently on disk.
    ///
    /// # Errors
    ///
    /// See [`ConfigManager::validate`].
    pub fn config_validate(&self) -> Result<(), String> {
        let base = self.config_read(ConfigWhich::Base)?;
        let local = self.config_read(ConfigWhich::Local)?;
        self.config().validate(Some(&base), Some(&local))
    }

    /// Applies surgical edits to one layer.
    ///
    /// # Errors
    ///
    /// See [`ConfigManager::set_values`].
    pub fn config_set_values(
        &self,
        which: ConfigWhich,
        pairs: &[(String, String)],
    ) -> Result<(), String> {
        self.config().set_values(which, pairs)
    }

    /// Reads one dotted key from one layer.
    pub fn config_get_value(&self, which: ConfigWhich, dotted: &str) -> Option<String> {
        self.config().get_value(which, dotted)
    }

    /// The backups of one layer, newest first.
    pub fn config_backups(&self, which: ConfigWhich) -> Vec<BackupInfo> {
        self.config().backups(which)
    }

    /// Restores a backup of one layer.
    ///
    /// # Errors
    ///
    /// See [`ConfigManager::restore_backup`].
    pub fn config_restore_backup(&self, which: ConfigWhich, file_name: &str) -> Result<(), String> {
        self.config().restore_backup(which, file_name)
    }

    // ----- process --------------------------------------------------------

    /// Starts the server with the configured command, watching its boot
    /// for the configured window.
    ///
    /// # Errors
    ///
    /// When no command is configured, the command line is malformed, the
    /// working directory is missing, a server is already running, or the
    /// program cannot be spawned.
    pub fn start_server(&self) -> Result<StartReport, String> {
        let settings = self.settings();
        if settings.server_command.trim().is_empty() {
            return Err(String::from(
                "no server command is configured — set it in Settings (the path to the \
                 wallermax-server binary)",
            ));
        }
        let tokens = process_manager::split_command_line(settings.server_command.trim())?;
        let (program, args) = tokens
            .split_first()
            .map(|(first, rest)| (first.clone(), rest.to_vec()))
            .ok_or_else(|| String::from("the server command is empty"))?;
        let cwd = if settings.working_dir.trim().is_empty() {
            self.config_dir
                .lock()
                .expect("the config-dir lock is live")
                .clone()
        } else {
            PathBuf::from(settings.working_dir.trim())
        };
        self.process.start(
            &SpawnSpec { program, args, cwd },
            Duration::from_millis(settings.probe_window_ms),
        )
    }

    /// Stops the server tree, with the configured grace.
    ///
    /// # Errors
    ///
    /// See [`ProcessManager::stop`].
    pub fn stop_server(&self) -> Result<(), String> {
        let grace = Duration::from_millis(self.settings().stop_grace_ms);
        self.process.stop(grace)
    }

    /// The supervised process status.
    pub fn server_status(&self) -> ProcessStatus {
        self.process.status()
    }

    /// The log page after `since`.
    pub fn server_logs(&self, since: u64, limit: usize) -> process_manager::LogPage {
        self.process.logs(since, limit)
    }

    // ----- endpoints ------------------------------------------------------

    /// The health URL — the same origin the metrics come from.
    pub fn health_url(&self) -> String {
        self.settings().health_url()
    }

    /// The Prometheus URL — the same origin the health check uses.
    pub fn metrics_url(&self) -> String {
        self.settings().metrics_url()
    }

    /// The stats URL — the same origin again.
    pub fn stats_url(&self) -> String {
        self.settings().stats_url()
    }

    /// Collects the dashboard in one call: health, Prometheus metrics and
    /// the stats JSON, each with a short timeout, merged into one flat
    /// snapshot. Nothing here holds a lock while waiting.
    pub fn fetch_dashboard(&self) -> DashboardSnapshot {
        let settings = self.settings();
        let timeout = Duration::from_secs(2);
        let mut snapshot = DashboardSnapshot::default();

        match metrics::http_get_text(&settings.health_url(), timeout) {
            Ok(_) => snapshot.reachable = true,
            Err(error) => snapshot.error = Some(error),
        }
        if let Ok(text) = metrics::http_get_text(&settings.metrics_url(), timeout) {
            snapshot.reachable = true;
            fill_from_metrics(&MetricsSnapshot::parse(&text), &mut snapshot);
        }
        if let Ok(text) = metrics::http_get_text(&settings.stats_url(), timeout) {
            snapshot.reachable = true;
            fill_from_stats(&text, &mut snapshot);
        }
        snapshot
    }
}

/// Finds the nearest ancestor of `from` holding `wallermax.toml`.
fn discover_config_dir(from: PathBuf) -> (PathBuf, bool) {
    let mut cursor: Option<&Path> = Some(from.as_path());
    while let Some(dir) = cursor {
        if dir.join(BASE_FILE).is_file() {
            return (dir.to_path_buf(), true);
        }
        cursor = dir.parent();
    }
    (from, false)
}

/// Copies the known metric families into the snapshot.
fn fill_from_metrics(parsed: &MetricsSnapshot, snapshot: &mut DashboardSnapshot) {
    snapshot.uptime_seconds = parsed.uptime_seconds();
    snapshot.requests_total = parsed.requests_total();
    snapshot.rate_limited_requests = parsed.rate_limited_total();
    snapshot.registered_users = snapshot.registered_users.or(parsed.registered_users());
}

/// Copies the stats JSON into the snapshot, filling gaps.
fn fill_from_stats(text: &str, snapshot: &mut DashboardSnapshot) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let number = |key: &str| value.get(key).and_then(serde_json::Value::as_f64);
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    if snapshot.uptime_seconds.is_none() {
        snapshot.uptime_seconds = number("uptime_seconds");
    }
    if snapshot.requests_total.is_none() {
        snapshot.requests_total = number("total_requests");
    }
    if snapshot.rate_limited_requests.is_none() {
        snapshot.rate_limited_requests = number("rate_limited_requests");
    }
    snapshot.requests_per_second = number("requests_per_second");
    snapshot.registered_users = snapshot
        .registered_users
        .or_else(|| number("registered_users"));
    snapshot.version = string("version");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::testutil::{survivor_command_line, wait_for};

    fn app_in(dir: &Path) -> App {
        App::with_settings(Settings::default(), dir.join("settings.json"), dir)
    }

    #[test]
    fn health_url_shares_the_metrics_origin() {
        let dir = crate::testutil::temp_dir("app-urls");
        let app = app_in(&dir);
        let origin = "http://127.0.0.1:8080";
        assert!(
            app.health_url().starts_with(&format!("{origin}/")),
            "{}",
            app.health_url()
        );
        assert!(app.metrics_url().starts_with(&format!("{origin}/")));
        assert!(app.stats_url().starts_with(&format!("{origin}/")));
        assert_eq!(app.health_url(), format!("{origin}/health"));
        assert_eq!(app.metrics_url(), format!("{origin}/metrics"));
        assert_eq!(app.stats_url(), format!("{origin}/api/stats"));
    }

    #[test]
    fn settings_roundtrip_through_the_app() {
        let dir = crate::testutil::temp_dir("app-settings");
        let app = app_in(&dir);
        let mut edited = app.settings();
        edited.server_command = String::from("wallermax-server");
        edited.origin = String::from("http://127.0.0.1:9090");
        app.save_settings(edited.clone()).expect("save applies");

        let reloaded = Settings::load(app.settings_path()).expect("persisted");
        assert_eq!(reloaded, edited);
        assert_eq!(app.settings().origin, "http://127.0.0.1:9090");

        // Invalid settings never reach the disk.
        let broken = Settings {
            origin: String::from("ftp://nope"),
            ..Settings::default()
        };
        assert!(app.save_settings(broken).is_err());
        assert_eq!(
            Settings::load(app.settings_path())
                .expect("still the good one")
                .origin,
            "http://127.0.0.1:9090"
        );
    }

    #[test]
    fn the_app_edits_configuration_end_to_end() {
        let dir = crate::testutil::temp_dir("app-config");
        let app = app_in(&dir);

        assert!(!app.config_exists(ConfigWhich::Base));
        assert_eq!(
            app.config_read(ConfigWhich::Base)
                .expect("missing reads as empty"),
            ""
        );

        app.config_set_values(
            ConfigWhich::Base,
            &[("server.port".to_owned(), "8123".to_owned())],
        )
        .expect("the surgical edit applies");
        assert_eq!(
            app.config_get_value(ConfigWhich::Base, "server.port")
                .expect("read back"),
            "8123"
        );

        let error = app
            .config_write(ConfigWhich::Base, "not toml at all =")
            .expect_err("broken raw text is refused");
        assert!(error.contains("line"), "with its line: {error}");
        assert_eq!(
            app.config_get_value(ConfigWhich::Base, "server.port")
                .expect("intact"),
            "8123",
            "a refused write leaves the file alone"
        );

        app.config_write(ConfigWhich::Base, "[server]\nport = 8124\n")
            .expect("a valid raw write lands");
        assert_eq!(
            app.config_get_value(ConfigWhich::Base, "server.port")
                .expect("read back"),
            "8124"
        );
        app.config_validate().expect("the pair on disk validates");
        assert!(
            !app.config_backups(ConfigWhich::Base).is_empty(),
            "the raw write kept a backup"
        );
    }

    #[test]
    fn the_app_starts_and_stops_a_server() {
        let dir = crate::testutil::temp_dir("app-process");
        let settings = Settings {
            server_command: String::from(survivor_command_line()),
            probe_window_ms: 1_200,
            stop_grace_ms: 5_000,
            ..Settings::default()
        };
        let app = App::with_settings(settings, dir.join("settings.json"), &dir);

        let report = app
            .start_server()
            .expect("the command line spawns the survivor");
        assert!(report.probe.booted, "probe: {:?}", report.probe);
        assert_eq!(
            app.server_status().state,
            crate::process_manager::ServerState::Running
        );

        wait_for("the banner in the logs", || {
            app.server_logs(0, 100)
                .lines
                .iter()
                .any(|line| line.text.contains("app-server-up"))
        });

        app.stop_server().expect("stop works");
        wait_for("the exit", || {
            app.server_status().state == crate::process_manager::ServerState::Exited
        });

        let empty = Settings {
            server_command: String::new(),
            ..Settings::default()
        };
        let app = App::with_settings(empty, dir.join("none.json"), &dir);
        let error = app.start_server().expect_err("an empty command is refused");
        assert!(error.contains("server command"), "{error}");
    }
}
