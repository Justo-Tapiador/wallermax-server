//! Parent-process supervision: spawn, boot probe, log capture and stop.
//!
//! ## The design, and why it looks the way it does
//!
//! The manager is the **parent** of the server it starts, and the child
//! is spawned *directly* — the command from the settings is split by
//! [`split_command_line`] and handed to the OS as program + arguments,
//! **without** a `cmd.exe`/`sh` wrapper in between. That choice is what
//! makes process control honest on every platform:
//!
//! - the tracked process *is* the server — no intermediate shell whose
//!   death would orphan the real one;
//! - the child inherits the manager's environment untouched (a server
//!   that needs `PATH`, `HOME` or `WALLERMAX_*` sees exactly what the
//!   operator's shell would have given it);
//! - on Windows the program resolves through the standard
//!   `CreateProcess` search order, which always includes `System32` —
//!   `ping.exe`, `cmd.exe` and friends are found even under a stripped
//!   `PATH`;
//! - also on Windows, the child is spawned with `CREATE_NO_WINDOW`:
//!   the server is a console program and the manager a GUI one, and
//!   without the flag Windows would allocate the child a fresh console
//!   — a black window squatting on the desktop for as long as the
//!   server lives. With it the child gets an invisible console whose
//!   output still flows through the pipes, and the server's own
//!   helpers (the `.jhs` sidecar) simply inherit that console.
//!
//! ## Stopping is tree-level
//!
//! The server may have helpers of its own (the `.jhs` template sidecar
//! among them), so stopping targets the whole tree:
//!
//! - **Unix**: the child is created in its own process group
//!   (`process_group(0)`), so `SIGTERM` is delivered to the group and
//!   escalates to `SIGKILL` after the grace period.
//! - **Windows**: one `taskkill /PID <pid> /T /F` takes the tree down in
//!   a single captured call. Console children cannot be closed
//!   gracefully on Windows; the server's storage (SQLite, atomic writes)
//!   is crash-safe by design, and capturing `taskkill`'s output keeps the
//!   manager's console clean instead of spraying
//!   *"no se pudo terminar"* errors.
//!
//! ## The server dies with the manager
//!
//! On Windows the fresh child is also assigned to a job object armed
//! with `JOB_OBJECT_LIMIT_KILL_ON_CLOSE`. The manager keeps that job's
//! handle open for its whole life, so whenever the manager goes away —
//! window closed, crash, `taskkill` — the kernel closes the handle and
//! takes the whole server tree down with it. Without this a manager
//! closed while the server runs leaves an *invisible* orphan behind
//! (the server has no console window), and the next start dies on the
//! old bind with "os error 10048" — port already in use. Unix keeps the
//! classic behaviour instead: the child lives in its own process group
//! and survives its parent, daemon-like, until it is stopped
//! explicitly.
//!
//! ## The boot probe
//!
//! Every start is watched for a short window: a process that **dies**
//! reports its captured output as the refusal reason, a process that
//! prints [`READY_MARKER`] is **booted** immediately, and a silent
//! **survivor** is accepted once the window passes. The server's own
//! startup line — `wallermax-server listening` — is the marker, so a
//! healthy boot is confirmed in milliseconds instead of guesses.
//!
//! ## Logs
//!
//! Both output pipes are drained by dedicated reader threads into a
//! sequence-numbered ring buffer (oldest lines evicted at capacity).
//! Readers use a **cursor**: `logs(since)` answers only newer lines plus
//! the next cursor, so a UI can poll forever without duplicates or
//! gaps — even across an eviction.

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// The server's own startup line — when it appears in the output, the
/// boot succeeded. The plain and the HTTPS variant share the prefix.
pub const READY_MARKER: &str = "wallermax-server listening";

/// Lines kept per process before the oldest are evicted.
const LOG_CAPACITY: usize = 10_000;

/// Hard cap for a single log line; longer lines are truncated.
const LOG_LINE_MAX: usize = 8 * 1024;

/// How much captured output a boot probe keeps in its report.
const PROBE_OUTPUT_MAX: usize = 8 * 1024;

/// Which output stream a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    /// The child's stdout.
    Out,
    /// The child's stderr.
    Err,
}

/// One captured output line with its global sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    pub seq: u64,
    pub stream: LogStream,
    pub text: String,
}

/// A page of lines plus the cursor the next call should pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LogPage {
    pub lines: Vec<LogLine>,
    pub next_seq: u64,
}

/// What a fresh process did during its boot probe.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BootProbe {
    /// Whether the process is considered up: it survived the window, or
    /// printed the readiness marker.
    pub booted: bool,
    /// The exit code when the process refused to boot.
    pub exit_code: Option<i32>,
    /// The first output it produced — the refusal reason.
    pub output: String,
}

/// What [`ProcessManager::start`] reports back.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StartReport {
    pub pid: u32,
    pub probe: BootProbe,
}

/// The coarse lifetime of the supervised process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    /// Nothing has been started since the last exit (or ever).
    Stopped,
    /// A child is alive right now.
    Running,
    /// The last child has exited.
    Exited,
}

/// The status snapshot the UI polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProcessStatus {
    pub state: ServerState,
    pub pid: Option<u32>,
    pub started_at_unix: Option<u64>,
    pub exit_code: Option<i32>,
}

/// Everything needed to spawn the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// Program path or name; resolved by the OS (`System32` is always
    /// searched on Windows).
    pub program: String,
    /// Arguments, already separated — no shell syntax is interpreted.
    pub args: Vec<String>,
    /// Working directory for the child.
    pub cwd: PathBuf,
}

/// Splits a command line into program + arguments: whitespace outside
/// double quotes separates tokens; quotes group paths with spaces.
///
/// # Errors
///
/// An empty command, or one that ends inside quotes, is rejected with a
/// message that says so.
pub fn split_command_line(line: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut token_started = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                token_started = true;
            }
            _ if in_quotes => current.push(ch),
            c if c.is_whitespace() => {
                if token_started || !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            c => {
                current.push(c);
                token_started = true;
            }
        }
    }
    if in_quotes {
        return Err(format!("unbalanced quotes in command line: {line}"));
    }
    if token_started || !current.is_empty() {
        tokens.push(current);
    }
    if tokens.is_empty() {
        return Err(String::from("the server command is empty"));
    }
    Ok(tokens)
}

/// A sequence-numbered ring buffer of captured lines.
#[derive(Debug, Default)]
struct LogBuffer {
    lines: Mutex<VecDeque<LogLine>>,
    written: AtomicU64,
    capacity: usize,
}

impl LogBuffer {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            lines: Mutex::new(VecDeque::new()),
            written: AtomicU64::new(0),
            capacity,
        }
    }

    fn push(&self, stream: LogStream, text: &str) {
        // Truncate on a character boundary — never mid-codepoint.
        let text = if text.chars().count() > LOG_LINE_MAX {
            let mut cut: String = text.chars().take(LOG_LINE_MAX).collect();
            cut.push('…');
            cut
        } else {
            text.to_owned()
        };
        let mut lines = self.lines.lock().expect("the log lock is live");
        // The sequence number is allocated UNDER the lock that appends
        // the line. The old version allocated it before locking, so a
        // concurrent `page` could observe the bumped counter while the
        // matching line was still queued behind the lock — and point
        // `next_seq` past a line the poller never saw (a skipped line,
        // permanently). With the allocation inside, `page` (which
        // reads `written` while holding the same lock) only ever
        // advertises numbers whose lines are already in the buffer.
        let seq = self.written.fetch_add(1, Ordering::SeqCst);
        lines.push_back(LogLine { seq, stream, text });
        while lines.len() > self.capacity {
            lines.pop_front();
        }
    }

    fn page(&self, since: u64, limit: usize) -> LogPage {
        let lines = self.lines.lock().expect("the log lock is live");
        let wanted: Vec<LogLine> = lines
            .iter()
            .filter(|line| line.seq >= since)
            .take(limit)
            .cloned()
            .collect();
        LogPage {
            next_seq: self.written.load(Ordering::SeqCst),
            lines: wanted,
        }
    }
}

/// What the reaper recorded when the child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExitInfo {
    code: Option<i32>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The supervised process bookkeeping. The `Child` itself lives in the
/// reaper thread (it owns the handle and performs the `try_wait` loop);
/// everything here is plain data, which keeps the whole manager `Send +
/// Sync` for the UI layer.
#[derive(Debug)]
struct Managed {
    pid: u32,
    started_at_unix: u64,
    exit: Arc<Mutex<Option<ExitInfo>>>,
    logs: Arc<LogBuffer>,
}

impl Managed {
    fn is_alive(&self) -> bool {
        self.exit.lock().expect("the exit lock is live").is_none()
    }

    fn exit_code(&self) -> Option<Option<i32>> {
        self.exit
            .lock()
            .expect("the exit lock is live")
            .map(|info| info.code)
    }
}

/// Watches a child until it ends, recording the exit code exactly once.
/// Owning the `Child` here also guarantees the process is reaped — no
/// zombie outlives the manager.
fn spawn_reaper(mut child: Child) -> Arc<Mutex<Option<ExitInfo>>> {
    let exit = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&exit);
    std::thread::spawn(move || loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                *slot.lock().expect("the exit lock is live") = Some(ExitInfo {
                    code: status.code(),
                });
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(120)),
            Err(_) => {
                *slot.lock().expect("the exit lock is live") = Some(ExitInfo { code: None });
                return;
            }
        }
    });
    exit
}

/// Drains one output pipe into the shared buffer until it closes.
fn drain_stream<R: std::io::Read + Send + 'static>(
    stream: R,
    which: LogStream,
    buffer: &Arc<LogBuffer>,
) {
    let buffer = Arc::clone(buffer);
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(text) => buffer.push(which, &text),
                Err(_) => return,
            }
        }
    });
}

/// Supervises at most one server process at a time. The previous run's
/// logs and exit code stay visible until the next start replaces them.
#[derive(Debug, Default)]
pub struct ProcessManager {
    slot: Mutex<Option<Managed>>,
}

impl ProcessManager {
    /// An idle manager: nothing started, nothing to stop.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts `spec` and watches it for `probe_window`.
    ///
    /// Starting while a previous child is still alive is rejected — one
    /// supervised process at a time, no accidental doubles.
    ///
    /// # Errors
    ///
    /// When a process is already running, the program cannot be spawned,
    /// or the working directory does not exist.
    pub fn start(&self, spec: &SpawnSpec, probe_window: Duration) -> Result<StartReport, String> {
        if !spec.cwd.is_dir() {
            return Err(format!(
                "the working directory `{}` does not exist",
                spec.cwd.display()
            ));
        }
        let (pid, logs, exit) = {
            let mut slot = self.lock()?;
            if let Some(previous) = slot.as_ref() {
                if previous.is_alive() {
                    return Err(format!(
                        "the server is already running (pid {}); stop it first",
                        previous.pid
                    ));
                }
            }

            let mut command = Command::new(&spec.program);
            command
                .args(&spec.args)
                .current_dir(&spec.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // The child gets its own process group: signals to the group
            // reach the server and any helper it spawned, and nothing
            // else in the manager's own group is ever touched.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            // No console window next to the GUI: the server keeps its
            // stdio through the pipes above, and its own children
            // inherit the invisible console.
            #[cfg(windows)]
            windowless(&mut command);
            let mut child = command
                .spawn()
                .map_err(|error| format!("could not start `{}`: {error}", spec.program))?;

            let pid = child.id();
            let logs = Arc::new(LogBuffer::with_capacity(LOG_CAPACITY));
            if let Some(stdout) = child.stdout.take() {
                drain_stream(stdout, LogStream::Out, &logs);
            }
            if let Some(stderr) = child.stderr.take() {
                drain_stream(stderr, LogStream::Err, &logs);
            }
            let exit = spawn_reaper(child);

            // Tie the fresh child to this manager's lifetime (Windows):
            // whenever the manager goes away, the whole tree goes with
            // it. A failure here never blocks the start — the explicit
            // stop path still works; only the crash-orphan safety net
            // is lost, and the captured output says so.
            #[cfg(windows)]
            if let Err(problem) = supervise_kill_on_close(pid) {
                logs.push(
                    LogStream::Err,
                    &format!(
                        "wallermax-manager: could not tie the server to the manager's \
                         lifetime ({problem}); a manager exit may leave it running"
                    ),
                );
            }

            *slot = Some(Managed {
                pid,
                started_at_unix: now_unix(),
                exit: Arc::clone(&exit),
                logs: Arc::clone(&logs),
            });
            (pid, logs, exit)
        };

        // The probe runs with the manager lock released, so status, logs
        // and stop calls stay responsive while it watches.
        let probe = probe_boot(&logs, &exit, probe_window);
        Ok(StartReport { pid, probe })
    }

    /// Stops the supervised process tree. Stopping nothing, or a process
    /// that already exited on its own, is a successful no-op.
    ///
    /// # Errors
    ///
    /// When the tree refuses to die even after the escalation.
    pub fn stop(&self, grace: Duration) -> Result<(), String> {
        // Only the identity is taken under the manager lock; the (possibly
        // slow) waiting happens with the lock released, so status, logs and
        // start calls stay responsive while a stop is in flight.
        let target = {
            let slot = self.lock()?;
            let Some(managed) = slot.as_ref() else {
                return Ok(());
            };
            if !managed.is_alive() {
                return Ok(());
            }
            (
                managed.pid,
                Arc::clone(&managed.exit) as Arc<Mutex<Option<ExitInfo>>>,
            )
        };
        let (pid, exit) = target;

        #[cfg(unix)]
        {
            let pgid = pid as libc::pid_t;
            let _ = kill_group(pgid, libc::SIGTERM);
            if !wait_until_dead(&exit, grace) {
                let _ = kill_group(pgid, libc::SIGKILL);
                if !wait_until_dead(&exit, Duration::from_secs(2)) {
                    return Err(format!("the server (pid {pid}) did not exit after SIGKILL"));
                }
            }
        }

        #[cfg(windows)]
        {
            // One captured, windowless taskkill takes the tree down (the
            // storage is crash-safe by design).
            if let Err(problem) = kill_tree(pid) {
                if is_exit_recorded(&exit) {
                    return Ok(());
                }
                return Err(problem);
            }
            if !wait_until_dead(&exit, grace.max(Duration::from_secs(3))) {
                return Err(format!(
                    "the server (pid {pid}) did not exit after taskkill"
                ));
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (pid, exit, grace);
            return Err(String::from("stopping is not implemented on this platform"));
        }

        Ok(())
    }

    /// The current status, reaping on demand.
    pub fn status(&self) -> ProcessStatus {
        let Ok(slot) = self.lock() else {
            return ProcessStatus {
                state: ServerState::Stopped,
                pid: None,
                started_at_unix: None,
                exit_code: None,
            };
        };
        match slot.as_ref() {
            None => ProcessStatus {
                state: ServerState::Stopped,
                pid: None,
                started_at_unix: None,
                exit_code: None,
            },
            Some(managed) if managed.is_alive() => ProcessStatus {
                state: ServerState::Running,
                pid: Some(managed.pid),
                started_at_unix: Some(managed.started_at_unix),
                exit_code: None,
            },
            Some(managed) => ProcessStatus {
                state: ServerState::Exited,
                pid: Some(managed.pid),
                started_at_unix: Some(managed.started_at_unix),
                exit_code: managed.exit_code().flatten(),
            },
        }
    }

    /// The lines captured so far, strictly newer than `since`.
    pub fn logs(&self, since: u64, limit: usize) -> LogPage {
        let Ok(slot) = self.lock() else {
            return LogPage {
                lines: Vec::new(),
                next_seq: since,
            };
        };
        match slot.as_ref() {
            Some(managed) => managed.logs.page(since, limit),
            None => LogPage {
                lines: Vec::new(),
                next_seq: since,
            },
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Managed>>, String> {
        self.slot
            .lock()
            .map_err(|_| String::from("the process state was poisoned by a panic"))
    }
}

/// Marks a console command so Windows does not allocate a visible
/// console window for it when the GUI manager spawns it. The child's
/// stdio is unaffected — it arrives through the pipes.
#[cfg(windows)]
fn windowless(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// A windowless `taskkill` command (no console flash from the GUI).
#[cfg(windows)]
fn taskkill_command() -> Command {
    let mut command = Command::new("taskkill");
    windowless(&mut command);
    command
}

/// Takes one process tree down the way [`ProcessManager::stop`] does:
/// a single captured, windowless `taskkill /PID <pid> /T /F`. Public for
/// the takeover path, which must fell a tree the manager did not spawn
/// (a previous session's server) exactly like a supervised one.
///
/// # Errors
///
/// When taskkill itself refuses or cannot run — the caller decides
/// whether that is fatal (a stop) or a race already won (a squatter
/// that died between the netstat and the kill).
#[cfg(windows)]
pub fn kill_tree(pid: u32) -> Result<(), String> {
    let pid_text = pid.to_string();
    let outcome = taskkill_command()
        .args(["/PID", pid_text.as_str(), "/T", "/F"])
        .output();
    match outcome {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "taskkill could not stop pid {pid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Err(format!("could not run taskkill for pid {pid}: {error}")),
    }
}

/// Arms the kill-on-close safety net for a fresh child (Windows): the
/// child joins a job object whose only handle this manager holds. When
/// the manager exits — any way it exits — the kernel closes the handle
/// and the job (the server and every helper it spawned, the `.jhs`
/// sidecar included) is terminated with it. The job handle is
/// deliberately never closed: its lifetime *is* the kill switch.
#[cfg(windows)]
#[allow(unsafe_code)]
fn supervise_kill_on_close(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    // SAFETY: four raw Win32 calls with no pointers into Rust data
    // beyond the `limits` struct zeroed and filled in below, and no
    // handle escaping this function — the process handle is closed
    // right after the single assignment, and the job handle is
    // intentionally left open for the manager's whole life (that open
    // handle is the mechanism; see the doc comment). Every failure path
    // closes what it opened.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(String::from("CreateJobObjectW returned null"));
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let armed = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if armed == 0 {
            CloseHandle(job);
            return Err(String::from(
                "SetInformationJobObject refused the kill-on-close flag",
            ));
        }
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            CloseHandle(job);
            return Err(format!("OpenProcess({pid}) returned null"));
        }
        let assigned = AssignProcessToJobObject(job, process);
        CloseHandle(process);
        if assigned == 0 {
            CloseHandle(job);
            return Err(format!("AssignProcessToJobObject({pid}) failed"));
        }
        // Deliberately not closing `job`: its open handle is the kill
        // switch — see the doc comment.
        Ok(())
    }
}

/// Polls until the child is recorded as dead, at most `budget` long.
fn wait_until_dead(exit: &Arc<Mutex<Option<ExitInfo>>>, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while !is_exit_recorded(exit) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Whether the exit has been recorded yet.
fn is_exit_recorded(exit: &Arc<Mutex<Option<ExitInfo>>>) -> bool {
    exit.lock().expect("the exit lock is live").is_some()
}

/// Delivers `SIGTERM`/`SIGKILL` to the whole process group the manager
/// created for its child.
///
/// # Safety
///
/// The single `unsafe` in the crate, scoped here: `libc::kill` is unsafe
/// only because a signal can target any process. The argument is always
/// the negated pid of a group this very manager created in
/// [`ProcessManager::start`], so the signal can only ever reach the
/// supervised server and its helpers.
#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_group(pgid: libc::pid_t, signal: libc::c_int) -> bool {
    unsafe { libc::kill(-pgid, signal) == 0 }
}

/// Watches the first moments of a fresh process.
fn probe_boot(
    logs: &Arc<LogBuffer>,
    exit: &Arc<Mutex<Option<ExitInfo>>>,
    window: Duration,
) -> BootProbe {
    let deadline = Instant::now() + window;
    let mut cursor = 0u64;
    let mut output = String::new();
    loop {
        let page = logs.page(cursor, 128);
        for line in &page.lines {
            if output.len() < PROBE_OUTPUT_MAX {
                output.push_str(&line.text);
                output.push('\n');
            }
            if line.text.contains(READY_MARKER) {
                return BootProbe {
                    booted: true,
                    exit_code: None,
                    output,
                };
            }
        }
        cursor = page.next_seq.max(cursor);

        if let Some(info) = exit.lock().expect("the exit lock is live").as_ref() {
            // Give the pipes a heartbeat to drain the dying process's
            // last words before reporting them.
            std::thread::sleep(Duration::from_millis(150));
            let page = logs.page(cursor, 128);
            for line in &page.lines {
                if output.len() < PROBE_OUTPUT_MAX {
                    output.push_str(&line.text);
                    output.push('\n');
                }
            }
            return BootProbe {
                booted: false,
                exit_code: info.code,
                output,
            };
        }

        if Instant::now() >= deadline {
            return BootProbe {
                booted: true,
                exit_code: None,
                output,
            };
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{chatty_spec, refusal_spec, short_lived_spec, survivor_spec, wait_for};

    const WINDOW: Duration = Duration::from_millis(1_200);
    const GRACE: Duration = Duration::from_secs(5);

    #[test]
    fn split_command_line_handles_quotes_and_spaces() {
        assert_eq!(
            split_command_line("wallermax-server --config app.toml").expect("splits"),
            vec!["wallermax-server", "--config", "app.toml"]
        );
        assert_eq!(
            split_command_line("\"C:\\Program Files\\wallermax\\server.exe\" run")
                .expect("quotes group"),
            vec!["C:\\Program Files\\wallermax\\server.exe", "run"]
        );
        assert_eq!(
            split_command_line("  cmd   /C   echo\thello  ").expect("tabs and runs collapse"),
            vec!["cmd", "/C", "echo", "hello"]
        );
        assert!(split_command_line("   ").is_err(), "blank is rejected");
        assert!(
            split_command_line("prog \"dangling").is_err(),
            "dangling quote is rejected"
        );
    }

    #[test]
    fn log_pages_never_advertise_an_unseen_line() {
        // The invariant the paging protocol rests on: after any push,
        // a page read at `since = 0` contains the pushed line and
        // `next_seq` points exactly one past it — a poller following
        // `next_seq` can never be told to skip a number whose line it
        // has not seen.
        let buffer = LogBuffer::with_capacity(64);
        for expected in 0..100_u64 {
            buffer.push(LogStream::Out, &format!("line {expected}"));
            let page = buffer.page(0, 1000);
            assert_eq!(
                page.lines.last().map(|line| line.seq),
                Some(expected),
                "the freshly pushed line must be visible"
            );
            assert_eq!(
                page.next_seq,
                expected + 1,
                "next_seq must point exactly one past the visible tail"
            );
        }
    }

    #[test]
    fn concurrent_push_and_page_never_lose_lines() {
        // Four writers push while a reader pages continuously. Nothing
        // is evicted (capacity exceeds the total), so the reader must
        // see every sequence number exactly once, in order — the race
        // the under-the-lock allocation closes.
        let buffer = std::sync::Arc::new(LogBuffer::with_capacity(8192));
        let writers: Vec<_> = (0..4_u64)
            .map(|writer| {
                let buffer = std::sync::Arc::clone(&buffer);
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        buffer.push(LogStream::Out, "concurrent");
                    }
                    writer
                })
            })
            .collect();
        let reader = {
            let buffer = std::sync::Arc::clone(&buffer);
            std::thread::spawn(move || {
                let mut expected = 0_u64;
                let mut since = 0_u64;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while expected < 2_000 {
                    let page = buffer.page(since, 10_000);
                    for line in &page.lines {
                        assert_eq!(
                            line.seq, expected,
                            "a line was skipped or reordered: expected {expected}, got {}",
                            line.seq
                        );
                        expected += 1;
                    }
                    since = page.next_seq;
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the reader stalled at {expected} lines"
                    );
                }
                expected
            })
        };

        for writer in writers {
            writer.join().expect("writer finishes");
        }
        let seen = reader.join().expect("reader finishes");
        assert_eq!(seen, 2_000, "every pushed line must be paged exactly once");
    }

    #[test]
    fn spawns_captures_and_stops() {
        let manager = ProcessManager::new();
        let report = manager
            .start(&survivor_spec(), WINDOW)
            .expect("the survivor starts");
        assert!(report.probe.booted, "probe: {:?}", report.probe);

        wait_for("the spawn banner in the logs", || {
            manager
                .logs(0, 100)
                .lines
                .iter()
                .any(|line| line.text.contains("manager-spawn-ok"))
        });
        assert_eq!(manager.status().state, ServerState::Running);

        manager.stop(GRACE).expect("stop works");
        wait_for("the exit to be recorded", || {
            manager.status().state == ServerState::Exited
        });
    }

    #[test]
    fn boot_probe_accepts_a_survivor() {
        let manager = ProcessManager::new();
        let report = manager
            .start(&survivor_spec(), WINDOW)
            .expect("the survivor starts");
        assert!(
            report.probe.booted,
            "a silent survivor is accepted: {:?}",
            report.probe
        );
        assert_eq!(report.probe.exit_code, None);
        manager.stop(GRACE).expect("cleanup");
    }

    #[test]
    fn boot_probe_reports_a_refusal_with_its_reason() {
        let manager = ProcessManager::new();
        let report = manager
            .start(&refusal_spec(), WINDOW)
            .expect("the refuser still starts");
        assert!(!report.probe.booted, "a dead process is a refusal");
        assert_eq!(report.probe.exit_code, Some(3));
        assert!(
            report.probe.output.contains("manager-refusal-reason"),
            "the reason is the captured output: {:?}",
            report.probe.output
        );
    }

    #[test]
    fn second_start_while_alive_is_rejected() {
        let manager = ProcessManager::new();
        manager
            .start(&survivor_spec(), WINDOW)
            .expect("the first start works");
        let error = manager
            .start(&survivor_spec(), WINDOW)
            .expect_err("a second start is rejected");
        assert!(
            error.contains("already running"),
            "the rejection says why: {error}"
        );
        manager.stop(GRACE).expect("cleanup");
    }

    #[test]
    #[cfg(windows)]
    fn starting_arms_the_kill_on_close_job() {
        // The job is armed right after the spawn; a failure to arm it
        // would be recorded as a manager line in the captured output.
        // (The kill-on-close *behaviour* itself — the whole tree dying
        // with the manager — cannot be observed from inside the manager;
        // arming without an error is the observable half, and the stop
        // path below proves the child still behaves normally.)
        let manager = ProcessManager::new();
        let report = manager
            .start(&survivor_spec(), WINDOW)
            .expect("the survivor starts");
        assert!(report.probe.booted, "probe: {:?}", report.probe);
        let captured: Vec<String> = manager
            .logs(0, 1000)
            .lines
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert!(
            !captured.iter().any(|line| line.contains("could not tie")),
            "arming must succeed, captured: {captured:?}"
        );
        manager.stop(GRACE).expect("stop still works");
    }

    #[test]
    fn stop_is_idempotent() {
        let manager = ProcessManager::new();
        manager
            .stop(GRACE)
            .expect("stopping an idle manager is a no-op");
        manager
            .start(&survivor_spec(), WINDOW)
            .expect("the survivor starts");
        manager.stop(GRACE).expect("the real stop works");
        wait_for("the exit to be recorded", || {
            manager.status().state == ServerState::Exited
        });
        manager
            .stop(GRACE)
            .expect("stopping an exited process is a no-op too");
    }

    #[test]
    fn logs_respect_the_sequence_cursor() {
        let manager = ProcessManager::new();
        manager
            .start(&chatty_spec(), WINDOW)
            .expect("the chatty process starts");

        wait_for("the first line to arrive", || {
            manager
                .logs(0, 100)
                .lines
                .iter()
                .any(|line| line.text.contains("first"))
        });
        let first = manager.logs(0, 100);
        assert!(!first.lines.is_empty());
        let cursor = first.next_seq;

        wait_for("the second line to arrive", || {
            manager
                .logs(cursor, 100)
                .lines
                .iter()
                .any(|line| line.text.contains("second"))
        });
        let second = manager.logs(cursor, 100);
        let max_first = first.lines.iter().map(|line| line.seq).max().unwrap_or(0);
        assert!(
            second.lines.iter().all(|line| line.seq > max_first),
            "newer pages carry only newer sequences"
        );
        assert!(
            !second.lines.iter().any(|line| line.text.contains("first")),
            "already-seen lines never repeat"
        );
        assert!(
            manager.logs(10_000_000, 100).lines.is_empty(),
            "a cursor past the end answers empty"
        );

        manager.stop(GRACE).expect("cleanup");
    }

    #[test]
    fn state_reaps_exited_children() {
        let manager = ProcessManager::new();
        manager
            .start(&short_lived_spec(), WINDOW)
            .expect("the short-lived process starts");
        wait_for("the exit status to be reaped", || {
            manager.status().state == ServerState::Exited
        });
        assert_eq!(manager.status().exit_code, Some(0));
    }
}
