//! # wallermax-manager-core
//!
//! The headless, testable heart of the **wallermax-manager** desktop
//! application (F22): everything the Tauri shell shows lives here, behind
//! plain Rust types that compile and run identically on Windows, Linux and
//! macOS — no GUI, no WebView, no global state.
//!
//! ## What the manager manages
//!
//! **Servers, not websites.** The manager configures and supervises the
//! `wallermax-server` *process* and its two configuration layers — the
//! versioned `wallermax.toml` and the git-ignored `wallermax.local.toml`
//! personal overrides. Web content, tenants and user accounts are
//! deliberately out of scope; the server already has a panel for those.
//!
//! ## Module map
//!
//! | Module                 | Responsibility                                    |
//! |------------------------|---------------------------------------------------|
//! | [`settings`]          | The manager's own JSON settings (survives restarts)|
//! | [`config_manager`]    | Layered TOML editing: validate, backup, write     |
//! | [`process_manager`]   | Spawn, boot probe, logs, tree-kill stop, lifetime |
//! | [`metrics`]           | Prometheus text parsing + a tiny blocking HTTP GET|
//! | [`app`]               | The facade the Tauri commands delegate to         |
//!
//! ## Zero-drift validation
//!
//! [`config_manager`] validates every edit through
//! `wallermax_server::config::AppConfig::load_from_toml_layers` — the
//! server's real `config`-crate pipeline (defaults, then base, then local,
//! then `normalize()`/`validate()`). The editor can therefore never accept
//! a file the server would reject, nor reject one the server would accept.
//!
//! ## Process control, cross-platform
//!
//! The manager is the **parent** of the server it starts: children are
//! spawned *directly* (no `cmd.exe` / `sh` wrapper in between — the
//! command from settings is split by [`process_manager::split_command_line`]),
//! with the parent's environment inherited untouched and stdout/stderr
//! piped into a sequence-numbered log ring buffer. Stopping is a
//! tree-level operation so the server's own helpers (e.g. the template
//! sidecar) die with it:
//!
//! - **Unix** — the child is created in its own process group
//!   (`process_group(0)`), so SIGTERM is delivered to the whole group,
//!   escalating to SIGKILL after the configured grace period.
//! - **Windows** — one captured `taskkill /PID <pid> /T /F` takes the
//!   tree down. Console children cannot be closed gracefully on Windows;
//!   the server's storage (SQLite, atomic writes) is crash-safe by design,
//!   and `taskkill`'s output is captured (never sprayed on the console).
//!   The child is also tied to the manager's lifetime through a
//!   kill-on-close job object, so a manager that goes away — however it
//!   goes away — never leaves an invisible server behind holding the
//!   port.
//!
//! A boot probe watches the first seconds of every start: a process that
//! dies reports its captured output as the refusal reason, a process
//! that prints `wallermax-server listening` is booted immediately, and a
//! silent survivor is accepted once the probe window passes.

//! ## Safety posture
//!
//! The crate stays `unsafe`-free with two deliberate, contained
//! exceptions, mirroring the server's own `require_bridge` pattern:
//! [`process_manager`] signals the process group it created on Unix
//! through `libc::kill`, and ties the server's lifetime to the
//! manager's on Windows through the Win32 job-object calls — a handful
//! of raw syscalls whose safety arguments are written next to each
//! call. Both exceptions are scoped to their helper with `deny` plus a
//! function-local allow; everywhere else `unsafe` remains a hard error.

#![deny(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

pub mod app;
pub mod config_manager;
pub mod metrics;
pub mod process_manager;
pub mod settings;

/// Shared helpers for the unit batteries (temporary directories with
/// unique names, bounded polling, and the cross-platform command
/// specifications the process tests spawn).
#[cfg(test)]
pub(crate) mod testutil;
