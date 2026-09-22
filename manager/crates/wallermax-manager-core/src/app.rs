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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config_manager::{BackupInfo, ConfigManager, ConfigWhich, BASE_FILE};
use crate::http;
use crate::metrics::MetricsSnapshot;
use crate::ports::{self, Occupancy, PortOwner};
use crate::process_manager::{
    self, ProcessManager, ProcessStatus, ServerState, SpawnSpec, StartReport,
};
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

/// How long one pre-flight connect probe may take — short, because it
/// runs between a Start click and the spawn.
const PORT_PROBE: Duration = Duration::from_millis(400);

/// The pre-flight picture of the server's port, for the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PortStatus {
    /// The address the server will bind ("127.0.0.1:8080"), when it can
    /// be derived from the configuration.
    pub address: Option<String>,
    /// Whether something already listens on that address.
    pub occupied: bool,
    /// The listening processes, when the OS can name them.
    pub owners: Vec<PortOwner>,
    /// Whether the port is held *only* by processes whose image matches
    /// the configured server program — takeover material.
    pub takeover_ready: bool,
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
    /// an `http://` or `https://` URL — a server with `[tls]` enabled
    /// serves its endpoints over TLS, and the manager talks to it the
    /// same way a browser would (its development certificate is
    /// accepted, see [`crate::http`]). The site URL is an `http://` or
    /// `https://` one (it opens in a browser, so TLS is the norm), and
    /// the endpoint paths must start with `/`; the
    /// configuration directory only changes when a value is given, so
    /// clearing the field keeps the discovered directory.
    ///
    /// # Errors
    ///
    /// The first invalid field, or filesystem problems from the save.
    pub fn save_settings(&self, incoming: Settings) -> Result<(), String> {
        let origin = incoming.origin.trim();
        if !origin.starts_with("http://") && !origin.starts_with("https://") {
            return Err(format!(
                "the origin must be an `http://` or `https://` URL — the manager talks to the \
                 local server, and `https://` suits a server serving TLS; got `{}`",
                incoming.origin
            ));
        }
        let site = incoming.site_url.trim();
        if !site.is_empty() && !site.starts_with("http://") && !site.starts_with("https://") {
            return Err(format!(
                "the site URL must be an `http://` or `https://` URL (it opens in the \
                 browser); got `{site}`"
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
    /// The port is probed first: a busy one is refused with the
    /// squatter named (see [`App::port_conflict`]) instead of letting
    /// the fresh server die on its own bind with a bare "os error
    /// 10048". A server this manager already supervises is left to the
    /// process manager's clearer "already running" refusal.
    ///
    /// # Errors
    ///
    /// When no command is configured, the command line is malformed, the
    /// working directory is missing, the port is already taken, a
    /// server is already running, or the program cannot be spawned.
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
        if self.process.status().state != ServerState::Running {
            if let Some(conflict) = self.port_conflict() {
                return Err(conflict);
            }
        }
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

    // ----- the server's port ----------------------------------------------

    /// The address the configured server will bind, derived the way the
    /// server itself derives it: `WALLERMAX_SERVER__HOST` / `__PORT`
    /// beat the local layer, which beats the base layer, which beats the
    /// built-in `127.0.0.1:8080`.
    ///
    /// `None` when there is nothing useful to probe: a `server.port` of
    /// `0` (the OS picks a free port — nothing can conflict) or a host
    /// that is not an IP literal (the server's `host` is an `IpAddr`;
    /// a name that fails to parse will fail the server's own startup
    /// with the real error). A `WALLERMAX_CONFIG` redirect to another
    /// file is deliberately not followed — the pre-flight is advisory,
    /// never a gate.
    fn server_bind(&self) -> Option<SocketAddr> {
        let config = self.config();
        let host = std::env::var("WALLERMAX_SERVER__HOST")
            .ok()
            .or_else(|| config.get_value(ConfigWhich::Local, "server.host"))
            .or_else(|| config.get_value(ConfigWhich::Base, "server.host"))
            .unwrap_or_else(|| String::from("127.0.0.1"));
        let port = std::env::var("WALLERMAX_SERVER__PORT")
            .ok()
            .or_else(|| config.get_value(ConfigWhich::Local, "server.port"))
            .or_else(|| config.get_value(ConfigWhich::Base, "server.port"))
            .unwrap_or_else(|| String::from("8080"));
        let port: u16 = port.trim().parse().ok()?;
        if port == 0 {
            return None;
        }
        Some(SocketAddr::new(host.trim().parse().ok()?, port))
    }

    /// The pre-flight verdict on the server's port: the full diagnosis
    /// when something already listens there, `None` when the coast is
    /// clear (or nothing can be probed).
    fn port_conflict(&self) -> Option<String> {
        let bind = self.server_bind()?;
        let Occupancy::Busy(address) = ports::probe(&probe_targets(bind), PORT_PROBE) else {
            return None;
        };
        let owners = ports::listening_owners(address.port());
        let expected = self.configured_server_image();
        Some(describe_conflict(address, &owners, expected.as_deref()))
    }

    /// The dashboard's one-call answer to "who has the server's port":
    /// the address the server will bind, whether something already
    /// listens there, the owners when the OS names them, and whether
    /// those owners are all the configured server (takeover material).
    pub fn port_status(&self) -> PortStatus {
        let Some(bind) = self.server_bind() else {
            return PortStatus::default();
        };
        if !matches!(
            ports::probe(&probe_targets(bind), PORT_PROBE),
            Occupancy::Busy(_)
        ) {
            return PortStatus {
                address: Some(bind.to_string()),
                occupied: false,
                owners: Vec::new(),
                takeover_ready: false,
            };
        }
        let owners = ports::listening_owners(bind.port());
        let takeover_ready = match self.configured_server_image() {
            Some(expected) => {
                !owners.is_empty()
                    && owners
                        .iter()
                        .all(|owner| image_matches(&owner.image, &expected))
            }
            None => false,
        };
        PortStatus {
            address: Some(bind.to_string()),
            occupied: true,
            owners,
            takeover_ready,
        }
    }

    /// The one-click recovery for the classic aftermath: an earlier
    /// manager session died before the kill-on-close job existed, and
    /// its invisible server still holds the port. Every owner whose
    /// image matches the configured server program is taken down
    /// (tree-level, exactly like an explicit stop), the port is awaited
    /// free, and the server starts normally.
    ///
    /// Nothing else is ever killed: a port held by an unrelated process
    /// — or by one the OS cannot even name — is refused with the full
    /// diagnosis, not fought over.
    ///
    /// # Errors
    ///
    /// When the port is held by anything but the configured server, or
    /// the stale server refuses to die — plus everything
    /// [`App::start_server`] can refuse.
    pub fn takeover_and_start(&self) -> Result<StartReport, String> {
        let Some(bind) = self.server_bind() else {
            return self.start_server();
        };
        if !matches!(
            ports::probe(&probe_targets(bind), PORT_PROBE),
            Occupancy::Busy(_)
        ) {
            return self.start_server();
        }
        let Some(expected) = self.configured_server_image() else {
            return Err(String::from(
                "no server command is configured — set it in Settings before taking a port over",
            ));
        };
        let owners = ports::listening_owners(bind.port());
        if owners.is_empty()
            || owners
                .iter()
                .any(|owner| !image_matches(&owner.image, &expected))
        {
            return Err(describe_conflict(bind, &owners, Some(expected.as_str())));
        }

        // All named owners are the configured server: take each tree
        // down. A taskkill failure is not fatal here — a squatter that
        // died between the netstat and the kill makes taskkill fail
        // harmlessly, and the port check below decides what happens.
        #[cfg(windows)]
        for owner in &owners {
            let _ = process_manager::kill_tree(owner.pid);
        }

        // Wait for the port to actually free up, then start normally.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !matches!(
                ports::probe(&probe_targets(bind), PORT_PROBE),
                Occupancy::Busy(_)
            ) {
                return self.start_server();
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the previous server did not give up {bind} within five seconds — stop it \
                     manually (`taskkill /PID {} /T /F`) and start again",
                    owners[0].pid
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The configured server program's name — `wallermax-server.exe`
    /// from `C:\path\wallermax-server.exe`, `wallermax-server` from a
    /// bare name — the identity a port owner must carry to be
    /// takeover-killable.
    fn configured_server_image(&self) -> Option<String> {
        let settings = self.settings();
        if settings.server_command.trim().is_empty() {
            return None;
        }
        process_manager::split_command_line(settings.server_command.trim())
            .ok()
            .and_then(|tokens| tokens.first().cloned())
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

    /// The website URL the dashboard links to — `site_url` from the
    /// settings when set, the shared origin otherwise.
    pub fn site_url(&self) -> String {
        self.settings().site_url()
    }

    /// Collects the dashboard in one call: health, Prometheus metrics and
    /// the stats JSON, each with a short timeout, merged into one flat
    /// snapshot. Nothing here holds a lock while waiting.
    pub fn fetch_dashboard(&self) -> DashboardSnapshot {
        let settings = self.settings();
        let timeout = Duration::from_secs(2);
        let mut snapshot = DashboardSnapshot::default();

        match http::http_get_text(&settings.health_url(), timeout) {
            Ok(_) => snapshot.reachable = true,
            Err(error) => snapshot.error = Some(error),
        }
        if let Ok(text) = http::http_get_text(&settings.metrics_url(), timeout) {
            snapshot.reachable = true;
            fill_from_metrics(&MetricsSnapshot::parse(&text), &mut snapshot);
        }
        if let Ok(text) = http::http_get_text(&settings.stats_url(), timeout) {
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

/// The addresses worth probing for a bind address: a wildcard host is
/// probed on loopback (a listener on *any* interface answers there), a
/// specific host is probed as itself.
fn probe_targets(bind: SocketAddr) -> Vec<SocketAddr> {
    match bind.ip() {
        IpAddr::V4(address) if address.is_unspecified() => vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            bind.port(),
        )],
        IpAddr::V6(address) if address.is_unspecified() => vec![SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            bind.port(),
        )],
        _ => vec![bind],
    }
}

/// The "who is on my port" message: names every owner the OS can,
/// distinguishes the manager's own leftovers from a foreign process,
/// and always ends with the way out. Owners are only named on Windows,
/// so the named branches speak Windows (Task Manager, taskkill).
fn describe_conflict(address: SocketAddr, owners: &[PortOwner], expected: Option<&str>) -> String {
    if owners.is_empty() {
        return format!(
            "the server's address {address} is already in use by another process the OS \
             could not name — find it with `netstat -ano` (or `ss -ltnp`) and stop it, or \
             change `server.port` in the configuration"
        );
    }
    let who: Vec<String> = owners
        .iter()
        .map(|owner| format!("pid {} ({})", owner.pid, owner.image))
        .collect();
    let ours =
        expected.is_some_and(|name| owners.iter().all(|owner| image_matches(&owner.image, name)));
    if ours {
        return format!(
            "the server's address {address} is already in use by {} — a server left over \
             from an earlier manager session (servers run without a console window). Take it \
             over from the dashboard, or end it yourself: `taskkill /PID {first} /T /F`",
            who.join(", "),
            first = owners[0].pid
        );
    }
    format!(
        "the server's address {address} is already in use by {} — {command}; stop whatever it \
         is (Task Manager, or `taskkill /PID {first} /T /F`), or change `server.port` in the \
         configuration",
        who.join(", "),
        command = match expected {
            Some(name) => format!("none of them is the configured server command (`{name}`)"),
            None => String::from("no server command is configured to compare them against"),
        },
        first = owners[0].pid
    )
}

/// Whether a listening owner's image is the configured server program:
/// the file name decides (never the directory), the case does not, and
/// the `.exe` extension may be spelled or implied — `wallermax-server`
/// matches `WALLERMAX-SERVER.exe`, but `wallermax-serverd.exe` never
/// matches `wallermax-server`. Both separators are honoured, so a
/// Windows-shaped program path parses identically on every platform
/// (`Path::file_name` would treat `\` as an ordinary character on
/// Unix).
fn image_matches(image: &str, program: &str) -> bool {
    let file_name = |value: &str| {
        value
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(value)
            .to_ascii_lowercase()
    };
    let image = file_name(image);
    let program = file_name(program);
    image == program || image.trim_end_matches(".exe") == program.trim_end_matches(".exe")
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
    use crate::config_manager::LOCAL_FILE;
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
        edited.site_url = String::from("https://localhost");
        app.save_settings(edited.clone()).expect("save applies");

        let reloaded = Settings::load(app.settings_path()).expect("persisted");
        assert_eq!(reloaded, edited);
        assert_eq!(app.settings().origin, "http://127.0.0.1:9090");
        assert_eq!(app.site_url(), "https://localhost");

        // Invalid settings never reach the disk.
        let broken = Settings {
            origin: String::from("ftp://nope"),
            ..Settings::default()
        };
        let refusal = app
            .save_settings(broken)
            .expect_err("an unknown scheme is refused");
        assert!(
            refusal.contains("ftp://nope"),
            "the refusal names what was offered: {refusal}"
        );
        let bad_site = Settings {
            site_url: String::from("file:///etc"),
            ..Settings::default()
        };
        assert!(app.save_settings(bad_site).is_err());
        assert_eq!(
            Settings::load(app.settings_path())
                .expect("still the good one")
                .origin,
            "http://127.0.0.1:9090"
        );
    }

    #[test]
    fn an_https_origin_is_accepted_for_a_tls_server() {
        let dir = crate::testutil::temp_dir("app-https-origin");
        let app = app_in(&dir);
        let mut edited = app.settings();
        edited.origin = String::from("https://127.0.0.1:443");
        app.save_settings(edited.clone()).expect("https applies");

        assert_eq!(app.settings().origin, "https://127.0.0.1:443");
        assert_eq!(app.health_url(), "https://127.0.0.1:443/health");
        assert_eq!(app.metrics_url(), "https://127.0.0.1:443/metrics");
        assert_eq!(app.stats_url(), "https://127.0.0.1:443/api/stats");
        // The site URL still opens wherever the browser should land,
        // independently of the origin the manager talks to.
        assert_eq!(app.site_url(), "https://127.0.0.1:443");
        edited.site_url = String::from("https://app.localhost");
        app.save_settings(edited).expect("site URL applies");
        assert_eq!(app.site_url(), "https://app.localhost");
        assert_eq!(app.health_url(), "https://127.0.0.1:443/health");
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
        // `server.port = 0`: the OS picks a free port, so the pre-flight
        // has nothing to probe and the test never depends on some
        // environment port being free.
        std::fs::write(
            dir.join(BASE_FILE),
            "[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .expect("seed the base config");
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

    #[test]
    fn the_bind_address_follows_the_layers() {
        let dir = crate::testutil::temp_dir("app-bind");
        std::fs::write(
            dir.join(BASE_FILE),
            "[server]\nhost = \"127.0.0.1\"\nport = 9000\n",
        )
        .expect("seed the base");
        let app = app_in(&dir);
        assert_eq!(
            app.server_bind().expect("the base answers"),
            "127.0.0.1:9000"
                .parse::<std::net::SocketAddr>()
                .expect("valid address")
        );

        // The local layer wins over the base.
        std::fs::write(dir.join(LOCAL_FILE), "[server]\nport = 9001\n").expect("seed the local");
        assert_eq!(
            app_in(&dir).server_bind().expect("the local wins").port(),
            9001
        );

        // Port 0 means the OS picks a free port: nothing to probe.
        std::fs::write(dir.join(LOCAL_FILE), "[server]\nport = 0\n").expect("zero the port");
        assert!(
            app_in(&dir).server_bind().is_none(),
            "port 0 cannot conflict"
        );

        // A wildcard host is probed on loopback instead.
        std::fs::write(
            dir.join(LOCAL_FILE),
            "[server]\nhost = \"0.0.0.0\"\nport = 9002\n",
        )
        .expect("seed the wildcard");
        assert_eq!(
            app_in(&dir).server_bind().expect("the wildcard answers"),
            "0.0.0.0:9002"
                .parse::<std::net::SocketAddr>()
                .expect("valid address")
        );
    }

    #[test]
    fn start_refuses_when_the_port_is_already_taken() {
        let dir = crate::testutil::temp_dir("app-port-busy");
        // A real listener is the squatter.
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the squatter");
        let port = squatter.local_addr().expect("address").port();
        std::fs::write(
            dir.join(BASE_FILE),
            format!("[server]\nhost = \"127.0.0.1\"\nport = {port}\n"),
        )
        .expect("aim the server at the squatter");

        let settings = Settings {
            server_command: String::from(survivor_command_line()),
            ..Settings::default()
        };
        let app = App::with_settings(settings, dir.join("settings.json"), &dir);
        let error = app.start_server().expect_err("the busy port is refused");
        assert!(error.contains("already in use"), "{error}");
        assert!(
            error.contains(&port.to_string()),
            "the message names the port: {error}"
        );
        // The squatter survived: the manager never kills what it did not
        // start (that is the takeover's job, and only ever on purpose).
        assert!(squatter.local_addr().is_ok(), "the squatter lives on");
        drop(squatter);
    }

    #[test]
    fn port_status_names_a_free_port_and_a_busy_one() {
        let dir = crate::testutil::temp_dir("app-port-status");
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the squatter");
        let port = squatter.local_addr().expect("address").port();
        std::fs::write(
            dir.join(BASE_FILE),
            format!("[server]\nhost = \"127.0.0.1\"\nport = {port}\n"),
        )
        .expect("aim at the squatter");

        let busy = app_in(&dir).port_status();
        assert!(busy.occupied, "the squatter is seen");
        assert_eq!(
            busy.address.as_deref(),
            Some(format!("127.0.0.1:{port}").as_str())
        );
        // Default settings carry no server command, so nothing is ever
        // takeover-ready by default — the button needs a configured
        // command to compare the owners against.
        assert!(!busy.takeover_ready, "no command, no takeover");
        drop(squatter);

        let free = app_in(&dir).port_status();
        assert!(!free.occupied, "the dropped listener is gone");
        assert_eq!(
            free.address.as_deref(),
            Some(format!("127.0.0.1:{port}").as_str()),
            "the address is still reported — only occupancy changes"
        );
    }

    #[test]
    fn image_matching_decides_by_file_name_only() {
        assert!(image_matches("wallermax-server.exe", "wallermax-server"));
        assert!(image_matches(
            "WALLERMAX-SERVER.EXE",
            "C:\\opt\\wallermax\\wallermax-server.exe"
        ));
        assert!(image_matches(
            "C:\\elsewhere\\wallermax-server.exe",
            "C:\\here\\wallermax-server.exe"
        ));
        assert!(
            !image_matches("wallermax-serverd.exe", "wallermax-server"),
            "a shared stem is not a shared identity"
        );
        assert!(!image_matches("node.exe", "wallermax-server"));
    }

    #[test]
    fn takeover_starts_on_a_free_port_and_never_fights_for_a_busy_one() {
        let dir = crate::testutil::temp_dir("app-takeover");
        // A free port (0 = the OS picks): the takeover is just a start.
        std::fs::write(
            dir.join(BASE_FILE),
            "[server]\nhost = \"127.0.0.1\"\nport = 0\n",
        )
        .expect("seed the base config");
        let settings = Settings {
            server_command: String::from(survivor_command_line()),
            probe_window_ms: 1_200,
            stop_grace_ms: 5_000,
            ..Settings::default()
        };
        let app = App::with_settings(settings, dir.join("settings.json"), &dir);
        let report = app
            .takeover_and_start()
            .expect("a free port starts like a plain start");
        assert!(report.probe.booted, "probe: {:?}", report.probe);
        app.stop_server().expect("stop works");
        wait_for("the exit", || {
            app.server_status().state == crate::process_manager::ServerState::Exited
        });

        // A busy port is never fought over: with no owner named (the
        // non-Windows answer) or a foreign one (the Windows answer for
        // this test's squatter), the takeover refuses and the squatter
        // survives untouched.
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the squatter");
        let port = squatter.local_addr().expect("address").port();
        std::fs::write(
            dir.join(BASE_FILE),
            format!("[server]\nhost = \"127.0.0.1\"\nport = {port}\n"),
        )
        .expect("aim at the squatter");
        let app = App::with_settings(
            Settings {
                server_command: String::from(survivor_command_line()),
                ..Settings::default()
            },
            dir.join("settings.json"),
            &dir,
        );
        let error = app
            .takeover_and_start()
            .expect_err("a busy port is refused, never fought for");
        assert!(error.contains("already in use"), "{error}");
        assert!(
            squatter.local_addr().is_ok(),
            "the squatter survives the refusal"
        );
        drop(squatter);
    }

    #[test]
    fn takeover_without_a_server_command_only_diagnoses() {
        let dir = crate::testutil::temp_dir("app-takeover-nocommand");
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the squatter");
        let port = squatter.local_addr().expect("address").port();
        std::fs::write(
            dir.join(BASE_FILE),
            format!("[server]\nhost = \"127.0.0.1\"\nport = {port}\n"),
        )
        .expect("aim at the squatter");
        let app = app_in(&dir);
        let error = app
            .takeover_and_start()
            .expect_err("there is nothing to compare the owners against");
        assert!(
            error.contains("no server command"),
            "the missing command is the headline: {error}"
        );
        assert!(
            squatter.local_addr().is_ok(),
            "the squatter survives the refusal"
        );
    }
}
