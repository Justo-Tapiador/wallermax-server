//! # wallermax-manager — the Tauri shell
//!
//! A deliberately thin layer: every command is a one-line delegation to
//! [`wallermax_manager_core::app::App`], so everything the window shows is
//! logic that already ran — and passed its tests — on Linux, Windows and
//! macOS alike inside the core crate.
//!
//! Two conventions keep this file honest:
//!
//! - the **slow** commands (`server_start`, `server_stop`,
//!   `dashboard_fetch`) are `async` and hop onto
//!   `tauri::async_runtime::spawn_blocking`, so the boot probe and the
//!   stop grace never freeze the window;
//! - no command parameter ever shares a name with a helper function —
//!   the `which`/`which` shadowing compile error of the first cut is
//!   structurally impossible now: the helper is `config_file`, the
//!   parameters are `file`.

// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::Command;
use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use wallermax_manager_core::app::{App, DashboardSnapshot, PortStatus};
use wallermax_manager_core::config_manager::{BackupInfo, ConfigWhich};
use wallermax_manager_core::process_manager::{LogPage, ProcessStatus, StartReport};
use wallermax_manager_core::settings::Settings;

/// The paths the Tools page shows.
#[derive(Debug, Clone, Serialize)]
struct PathsInfo {
    settings_path: String,
    config_dir: String,
    base_path: String,
    local_path: String,
    notice: Option<String>,
}

/// Resolves the UI's `base` / `local` into a [`ConfigWhich`].
fn config_file(name: &str) -> Result<ConfigWhich, String> {
    ConfigWhich::from_name(name)
}

#[tauri::command]
fn paths_info(state: State<'_, Arc<App>>) -> PathsInfo {
    let config = state.config();
    PathsInfo {
        settings_path: state.settings_path().display().to_string(),
        config_dir: config.dir().display().to_string(),
        base_path: config.path(ConfigWhich::Base).display().to_string(),
        local_path: config.path(ConfigWhich::Local).display().to_string(),
        notice: state.notice(),
    }
}

#[tauri::command]
fn settings_get(state: State<'_, Arc<App>>) -> Settings {
    state.settings()
}

#[tauri::command]
fn settings_save(state: State<'_, Arc<App>>, settings: Settings) -> Result<(), String> {
    state.save_settings(settings)
}

#[tauri::command]
fn config_exists(state: State<'_, Arc<App>>, file: String) -> Result<bool, String> {
    Ok(state.config_exists(config_file(&file)?))
}

#[tauri::command]
fn config_read(state: State<'_, Arc<App>>, file: String) -> Result<String, String> {
    state.config_read(config_file(&file)?)
}

#[tauri::command]
fn config_write(state: State<'_, Arc<App>>, file: String, content: String) -> Result<(), String> {
    state.config_write(config_file(&file)?, &content)
}

#[tauri::command]
fn config_validate(state: State<'_, Arc<App>>) -> Result<(), String> {
    state.config_validate()
}

#[tauri::command]
fn config_set_values(
    state: State<'_, Arc<App>>,
    file: String,
    values: Vec<(String, String)>,
) -> Result<(), String> {
    state.config_set_values(config_file(&file)?, &values)
}

#[tauri::command]
fn config_get_value(
    state: State<'_, Arc<App>>,
    file: String,
    key: String,
) -> Result<Option<String>, String> {
    Ok(state.config_get_value(config_file(&file)?, &key))
}

#[tauri::command]
fn config_backups(state: State<'_, Arc<App>>, file: String) -> Result<Vec<BackupInfo>, String> {
    Ok(state.config_backups(config_file(&file)?))
}

#[tauri::command]
fn config_restore_backup(
    state: State<'_, Arc<App>>,
    file: String,
    file_name: String,
) -> Result<(), String> {
    state.config_restore_backup(config_file(&file)?, &file_name)
}

#[tauri::command]
async fn server_start(state: State<'_, Arc<App>>) -> Result<StartReport, String> {
    let app = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || app.start_server())
        .await
        .map_err(|error| format!("the start task failed: {error}"))?
}

/// The takeover: the one-click recovery for a port still held by a
/// previous session's server — same slow-shape as `server_start` (the
/// taskkills and the port wait must never freeze the window).
#[tauri::command]
async fn server_takeover(state: State<'_, Arc<App>>) -> Result<StartReport, String> {
    let app = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || app.takeover_and_start())
        .await
        .map_err(|error| format!("the takeover task failed: {error}"))?
}

/// The port probe — the connect attempt (plus the netstat/tasklist
/// pass on Windows while the port is busy) is slow by UI standards.
#[tauri::command]
async fn port_status(state: State<'_, Arc<App>>) -> Result<PortStatus, String> {
    let app = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || app.port_status())
        .await
        .map_err(|error| format!("the port probe failed: {error}"))
}

#[tauri::command]
async fn server_stop(state: State<'_, Arc<App>>) -> Result<(), String> {
    let app = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || app.stop_server())
        .await
        .map_err(|error| format!("the stop task failed: {error}"))?
}

#[tauri::command]
fn server_status(state: State<'_, Arc<App>>) -> ProcessStatus {
    state.server_status()
}

#[tauri::command]
fn server_logs(
    state: State<'_, Arc<App>>,
    since: u64,
    limit: Option<usize>,
) -> Result<LogPage, String> {
    Ok(state.server_logs(since, limit.unwrap_or(500)))
}

#[tauri::command]
async fn dashboard_fetch(state: State<'_, Arc<App>>) -> Result<DashboardSnapshot, String> {
    let app = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || app.fetch_dashboard())
        .await
        .map_err(|error| format!("the dashboard task failed: {error}"))
}

/// The per-platform "open this in the user's world" program: folders
/// land in the file manager, URLs in the default browser (`explorer`,
/// `open` and `xdg-open` all take both).
fn system_opener() -> &'static str {
    if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

#[tauri::command]
fn open_config_folder(state: State<'_, Arc<App>>) -> Result<(), String> {
    let dir = state.config().dir().to_path_buf();
    if !dir.is_dir() {
        return Err(format!(
            "the configuration directory `{}` does not exist",
            dir.display()
        ));
    }
    Command::new(system_opener())
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open {}: {error}", dir.display()))
}

#[tauri::command]
fn open_site(state: State<'_, Arc<App>>) -> Result<(), String> {
    let url = state.site_url();
    // The URL may have been hand-edited into the settings file since
    // the last save; the scheme guard keeps the opener from being
    // pointed at local files.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!(
            "the site URL must be an `http://` or `https://` URL; got `{url}`"
        ));
    }
    Command::new(system_opener())
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open {url}: {error}"))
}

fn main() {
    let app = App::discover();
    tauri::Builder::default()
        .manage(Arc::new(app))
        .invoke_handler(tauri::generate_handler![
            paths_info,
            settings_get,
            settings_save,
            config_exists,
            config_read,
            config_write,
            config_validate,
            config_set_values,
            config_get_value,
            config_backups,
            config_restore_backup,
            server_start,
            server_takeover,
            server_stop,
            server_status,
            server_logs,
            port_status,
            dashboard_fetch,
            open_config_folder,
            open_site
        ])
        .run(tauri::generate_context!())
        .expect("error while running wallermax-manager");
}
