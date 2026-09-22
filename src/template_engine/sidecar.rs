//! The Node sidecar backend (`[templates] backend = "sidecar" | "auto"`).
//!
//! Runs the ORIGINAL `node-jhs2` engine — vendored at
//! `sidecar/engine.js` — as a child Node process serving loopback HTTP:
//! real `require()` behind the `[templates] forbidden_modules` banner,
//! the exact original compiler, and a wall-clock render budget enforced
//! by terminating runaway worker threads (the fix for the original
//! engine's ineffective `vm` timeout).
//!
//! Lifecycle:
//!
//! 1. [`SidecarRenderer::spawn`] launches `node sidecar/jhs-sidecar.mjs`
//!    with a fresh random token, parses the single stdout handshake
//!    line (`{"event":"ready","port":…}`) and then requires
//!    `GET /selftest` to pass **all** checks before the backend is
//!    considered usable.
//! 2. Renders talk `POST /render` / `POST /render-string` over a
//!    throwaway loopback TCP connection with the token header — a
//!    minimal HTTP/1.1 exchange that needs no client dependency.
//! 3. The sidecar exits when its stdin pipe closes, so it never outlives
//!    the server; [`Drop`] kills it as well for in-process teardown.
//!
//! [`AutoRenderer`] wraps a healthy sidecar plus the in-process
//! [`JhsEngine`] (the hardened boa backend): sidecar transport failures
//! fall back to boa for that render (with an error log), a health
//! probe with a cooldown brings a wedged sidecar back once it recovers,
//! and a crashed process is re-spawned in the background by the
//! [`SidecarSupervisor`] (strict mode self-heals the same way, minus
//! the fallback). Template errors — a broken template fails
//! identically on both backends — never trigger a fallback.
//!
//! This module is deliberately synchronous: rendering happens on the
//! blocking pool, and the spawn handshake runs once at startup.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::engine::{ConsoleLine, JhsEngine, JhsError, RedirectIntent, RenderOutput};
use super::renderer::TemplateRenderer;

/// How long to wait for the loopback TCP connect alone.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on one HTTP response from the sidecar (renders are HTML strings;
/// anything bigger is a runaway, not a page).
const RESPONSE_LIMIT: usize = 64 * 1024 * 1024;

/// How long the auto backend waits before re-probing a dead sidecar.
const PROBE_COOLDOWN: Duration = Duration::from_secs(10);

/// How many stderr lines to keep for spawn-failure diagnostics.
const STDERR_TAIL: usize = 40;

/// V8 old-space ceiling for the sidecar's own heap, in MiB.
///
/// The broker process only parses requests and shuttles jobs, so this
/// stays small; the render workers carry their own, independent ceiling
/// (see `JHS_SIDECAR_WORKER_HEAP_MB` in `jhs-sidecar.mjs`). Without a
/// cap a wedged sidecar grows until the OS OOM killer takes the
/// server's container down with it.
const SIDECAR_MAIN_HEAP_MB: u32 = 192;

/// Everything the sidecar process needs to know, distilled from
/// `[templates]` + `[templates.sidecar]` by the state constructor.
/// Clone-able because the supervisor keeps a second copy to re-spawn
/// a dead sidecar with the same recipe.
#[derive(Clone)]
pub struct SidecarOptions {
    /// Node.js binary (`node_command`).
    pub node_command: String,
    /// The sidecar service script, resolved to an absolute path.
    pub script: PathBuf,
    /// Views directory (absolute) — include resolution target.
    pub views_dir: PathBuf,
    /// Modules directory (absolute) — local `require()` root.
    pub modules_dir: PathBuf,
    /// The `[templates] forbidden_modules` banner.
    pub forbidden: Vec<String>,
    /// `[templates] auto_escape`.
    pub auto_escape: bool,
    /// `[templates] require_enabled`.
    pub require_enabled: bool,
    /// Worker thread count.
    pub workers: u32,
    /// Budget for spawn + handshake + selftest.
    pub startup_timeout: Duration,
    /// Client-side timeout for one render request (incl. queue wait).
    pub request_timeout: Duration,
    /// Wall-clock hard-kill budget inside the sidecar.
    pub render_budget: Duration,
}

/// The strict sidecar backend: a supervised child process plus the
/// loopback client. Transport failures are [`JhsError::Sidecar`] errors
/// (no silent fallback — use `[templates] backend = "auto"` for that).
pub struct SidecarRenderer {
    addr: SocketAddr,
    token: String,
    request_timeout: Duration,
    child: Mutex<Option<Child>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

/// Kills the child on scope exit, including every error path in
/// [`SidecarRenderer::spawn`].
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn release(mut self) -> Option<Child> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SidecarRenderer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().expect("sidecar child mutex").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl SidecarRenderer {
    /// Launches the sidecar, performs the READY handshake and requires
    /// the selftest to pass.
    ///
    /// # Errors
    ///
    /// Returns a operator-facing description (including the sidecar's
    /// stderr tail) when the script is missing, Node cannot be
    /// launched, the handshake times out or the selftest fails.
    pub fn spawn(options: &SidecarOptions) -> Result<Arc<Self>, String> {
        if !options.script.is_file() {
            return Err(format!(
                "the JHS sidecar script {} does not exist (see [templates.sidecar] script)",
                options.script.display()
            ));
        }

        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );

        let mut command = Command::new(&options.node_command);
        command
            // Cap the broker's own heap (see SIDECAR_MAIN_HEAP_MB) —
            // Node accepts its V8 flags before the script path.
            .arg(format!("--max-old-space-size={SIDECAR_MAIN_HEAP_MB}"))
            .arg(options.script.as_os_str())
            .env("JHS_SIDECAR_TOKEN", &token)
            .env("JHS_SIDECAR_PORT", "0")
            .env("JHS_SIDECAR_VIEWS_DIR", options.views_dir.as_os_str())
            .env("JHS_SIDECAR_MODULES_DIR", options.modules_dir.as_os_str())
            .env(
                "JHS_SIDECAR_FORBIDDEN",
                serde_json::to_string(&options.forbidden).unwrap_or_else(|_| String::from("[]")),
            )
            .env(
                "JHS_SIDECAR_AUTO_ESCAPE",
                if options.auto_escape { "true" } else { "false" },
            )
            .env(
                "JHS_SIDECAR_REQUIRE_ENABLED",
                if options.require_enabled {
                    "true"
                } else {
                    "false"
                },
            )
            .env("JHS_SIDECAR_WORKERS", options.workers.to_string())
            .env(
                "JHS_SIDECAR_RENDER_BUDGET_MS",
                options.render_budget.as_millis().to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = command.spawn().map_err(|error| {
            format!(
                "could not launch the JHS sidecar ({} {}): {error} — is Node.js installed, \
                 and is [templates.sidecar] node_command correct? \
                 (set [templates] backend = \"boa\" or \"auto\" to render without Node)",
                options.node_command,
                options.script.display()
            )
        })?;

        let mut guard = ChildGuard(Some(child));

        let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        if let Some(stderr) = guard.0.as_mut().and_then(|child| child.stderr.take()) {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let mut tail = tail.lock().expect("sidecar stderr mutex");
                    if tail.len() == STDERR_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }

        // Handshake: the sidecar writes exactly one stdout line with the
        // chosen port; a reader thread parses it and then drains stdout
        // forever so the child can never block on a full pipe.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<String>();
        if let Some(stdout) = guard.0.as_mut().and_then(|child| child.stdout.take()) {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut first = String::new();
                if reader.read_line(&mut first).unwrap_or(0) > 0 {
                    let _ = ready_tx.send(first.trim_end().to_owned());
                }
                let mut sink = String::new();
                loop {
                    match reader.read_line(&mut sink) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => sink.clear(),
                    }
                }
            });
        }

        let handshake_line = ready_rx
            .recv_timeout(options.startup_timeout)
            .map_err(|_| {
                format!(
                    "the JHS sidecar did not report readiness within {} ms \
                     (see [templates.sidecar] startup_timeout_ms){}",
                    options.startup_timeout.as_millis(),
                    stderr_report(&stderr_tail.lock().expect("sidecar stderr mutex"))
                )
            })?;
        let ready: Value = serde_json::from_str(&handshake_line).map_err(|error| {
            format!("the JHS sidecar handshake line was not valid JSON ({error}): {handshake_line}")
        })?;
        let port = ready
            .get("port")
            .and_then(Value::as_u64)
            .filter(|port| *port > 0 && *port <= u16::MAX as u64)
            .ok_or_else(|| {
                format!("the JHS sidecar handshake line carries no port: {handshake_line}")
            })?;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port as u16);

        let renderer = Arc::new(Self {
            addr,
            token,
            request_timeout: options.request_timeout,
            child: Mutex::new(guard.release()),
            stderr_tail,
        });

        // The startup gate: every selftest check must pass before the
        // backend is trusted with production renders.
        let report = renderer
            .exchange("GET", "/selftest", None)
            .map_err(|error| format!("the JHS sidecar selftest could not be fetched: {error}"))?;
        if report.get("ok").and_then(Value::as_bool) != Some(true) {
            let failed: Vec<String> = report
                .get("checks")
                .and_then(Value::as_array)
                .map(|checks| {
                    checks
                        .iter()
                        .filter(|check| check.get("ok").and_then(Value::as_bool) != Some(true))
                        .filter_map(|check| check.get("name").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            return Err(format!(
                "the JHS sidecar selftest failed ({}); the backend stays disabled{}",
                if failed.is_empty() {
                    String::from("unknown checks")
                } else {
                    failed.join(", ")
                },
                renderer.report_stderr(),
            ));
        }

        let workers = ready
            .get("workers")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        tracing::info!(
            %addr,
            workers,
            budget_ms = options.render_budget.as_millis(),
            "the JHS sidecar is up: templates render on the original node-jhs2 engine"
        );

        Ok(renderer)
    }

    /// One HTTP/1.1 exchange with the sidecar. The connection is
    /// throwaway (`connection: close`) — on loopback the extra TCP
    /// handshake costs microseconds next to the render itself.
    fn exchange(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let mut stream = TcpStream::connect_timeout(&self.addr, CONNECT_TIMEOUT)
            .map_err(|error| format!("connect failed: {error}"))?;
        stream
            .set_read_timeout(Some(self.request_timeout))
            .map_err(|error| format!("read timeout could not be set: {error}"))?;
        stream
            .set_write_timeout(Some(self.request_timeout))
            .map_err(|error| format!("write timeout could not be set: {error}"))?;

        let payload = body.map(|value| value.to_string());
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: jhs-sidecar\r\nx-sidecar-token: {}\r\n\
             connection: close\r\n",
            self.token
        );
        if let Some(payload) = &payload {
            request.push_str(&format!(
                "content-type: application/json\r\ncontent-length: {}\r\n",
                payload.len()
            ));
        }
        request.push_str("\r\n");

        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("write failed: {error}"))?;
        if let Some(payload) = &payload {
            stream
                .write_all(payload.as_bytes())
                .map_err(|error| format!("write failed: {error}"))?;
        }
        let _ = stream.flush();

        read_json_response(&mut stream)
    }

    /// Runs one render call (either endpoint) through the protocol and
    /// maps the envelope onto the port's error model.
    fn render_call(&self, body: Value, fallback_label: &str) -> Result<RenderOutput, JhsError> {
        let endpoint = if body.get("path").is_some() {
            "/render"
        } else {
            "/render-string"
        };
        let response = self
            .exchange("POST", endpoint, Some(&body))
            .map_err(|error| {
                JhsError::Sidecar(format!(
                    "the JHS sidecar at {} did not answer: {error}",
                    self.addr
                ))
            })?;

        if response.get("ok").and_then(Value::as_bool) == Some(true) {
            let success: SidecarSuccess = serde_json::from_value(response).map_err(|error| {
                JhsError::Sidecar(format!("malformed sidecar success envelope: {error}"))
            })?;
            return Ok(RenderOutput {
                html: success.html,
                console: success.console,
                redirect: success.redirect,
            });
        }

        let failure = match response.get("error") {
            Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                JhsError::Sidecar(format!("malformed sidecar error envelope: {error}"))
            })?,
            None => SidecarFailure {
                kind: String::from("protocol"),
                message: String::from("the sidecar answered without an error payload"),
                path: None,
            },
        };

        let label = failure
            .path
            .clone()
            .unwrap_or_else(|| fallback_label.to_owned());
        match failure.kind.as_str() {
            // The file layer: the sidecar reports the raw io message and
            // the JhsError::Io Display adds the port's prefix.
            "io" => Err(JhsError::Io(std::io::Error::other(failure.message))),
            // Include resolution failures carry the port's inner text.
            "include" => Err(JhsError::Include(failure.message)),
            // Execution, hard-kill timeouts and worker deaths all carry
            // the full port-format message already.
            "execution" | "timeout" | "worker" => Err(JhsError::Execution {
                path: label,
                message: failure.message,
            }),
            _ => Err(JhsError::Sidecar(format!(
                "sidecar protocol error: {}",
                failure.message
            ))),
        }
    }

    /// Cheap liveness probe (`GET /health`).
    fn health_probe(&self) -> bool {
        self.exchange("GET", "/health", None)
            .ok()
            .and_then(|value| value.get("ok").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    /// The stderr tail as a report suffix.
    fn report_stderr(&self) -> String {
        stderr_report(&self.stderr_tail.lock().expect("sidecar stderr mutex"))
    }
}

impl TemplateRenderer for SidecarRenderer {
    fn render(
        &self,
        template_path: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        let body = json!({ "path": template_path, "data": data });
        self.render_call(body, template_path)
    }

    fn render_string(
        &self,
        template: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        let body = json!({ "source": template, "data": data });
        self.render_call(body, "<string>")
    }

    fn backend_name(&self) -> &'static str {
        "sidecar"
    }
}

/// The success envelope of `/render` and `/render-string`.
#[derive(Debug, serde::Deserialize)]
struct SidecarSuccess {
    html: String,
    #[serde(default)]
    console: Vec<ConsoleLine>,
    redirect: Option<RedirectIntent>,
}

/// The error payload inside a failed render envelope.
#[derive(Debug, serde::Deserialize)]
struct SidecarFailure {
    kind: String,
    message: String,
    path: Option<String>,
}

/// Reads one full HTTP/1.1 response (status line + headers +
/// content-length-framed body) and parses the JSON body.
fn read_json_response(stream: &mut TcpStream) -> Result<Value, String> {
    let mut raw: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];

    let header_end = loop {
        let newline = find_header_end(&raw);
        if let Some(at) = newline {
            break at;
        }
        if raw.len() > RESPONSE_LIMIT {
            return Err(String::from("the response headers exceeded the size limit"));
        }
        let read = stream
            .read(&mut chunk)
            .map_err(|error| format!("read failed: {error}"))?;
        if read == 0 {
            return Err(String::from(
                "the sidecar closed the connection before answering",
            ));
        }
        raw.extend_from_slice(&chunk[..read]);
    };

    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = headers.lines();
    let status = lines
        .next()
        .ok_or_else(|| String::from("the response has no status line"))?;
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "the sidecar answered with a non-200 status: {status}"
        ));
    }

    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| String::from("the response has no content-length header"))?;

    let mut body = raw[header_end + 4..].to_vec();
    while body.len() < content_length {
        if body.len() > RESPONSE_LIMIT {
            return Err(String::from("the response body exceeded the size limit"));
        }
        let read = stream
            .read(&mut chunk)
            .map_err(|error| format!("read failed: {error}"))?;
        if read == 0 {
            return Err(String::from("the sidecar closed the connection mid-body"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    serde_json::from_slice(&body)
        .map_err(|error| format!("the sidecar response body was not valid JSON: {error}"))
}

/// Offset of the `\r\n\r\n` header terminator, if present.
fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Formats the stderr tail as a diagnostic suffix.
fn stderr_report(tail: &VecDeque<String>) -> String {
    if tail.is_empty() {
        String::new()
    } else {
        let lines: Vec<&str> = tail.iter().map(String::as_str).collect();
        format!("\nsidecar stderr (tail):\n  {}", lines.join("\n  "))
    }
}

// ── Supervision: keeping a sidecar alive ───────────────────────────

/// Wait between respawn attempts after the first failure: 10s, 20s,
/// 40s... doubling.
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(10);

/// Ceiling for the respawn backoff.
const RESPAWN_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Owns the live sidecar process and brings it back when it dies.
///
/// [`SidecarRenderer::spawn`] is the one-shot constructor the state
/// boots with; everything after that — a crashed child, a process
/// that stopped answering — belongs here. Whoever notices (a
/// transport error mid-render, a failed health probe) calls
/// [`Self::request_respawn`]; the respawn itself runs on a detached
/// thread so renders never wait for Node, at most one runs at a time,
/// and consecutive failures back off exponentially (capped at
/// [`RESPAWN_MAX_BACKOFF`]) so a broken Node installation cannot be
/// fork-bombed.
pub struct SidecarSupervisor {
    options: SidecarOptions,
    current: Mutex<Arc<SidecarRenderer>>,
    /// Bumped every time a respawn swaps the live sidecar: readers
    /// treat a generation they have not served as healthy (a fresh
    /// spawn passed its full selftest by construction).
    generation: AtomicU64,
    respawning: AtomicBool,
    failures: Mutex<u32>,
    next_attempt: Mutex<Instant>,
}

impl SidecarSupervisor {
    /// Wraps a live sidecar with the recipe that spawned it.
    pub fn new(sidecar: Arc<SidecarRenderer>, options: SidecarOptions) -> Self {
        Self {
            options,
            current: Mutex::new(sidecar),
            generation: AtomicU64::new(1),
            respawning: AtomicBool::new(false),
            failures: Mutex::new(0),
            // An Instant in the past: the first respawn may run at once.
            next_attempt: Mutex::new(
                Instant::now()
                    .checked_sub(RESPAWN_BACKOFF_BASE)
                    .unwrap_or_else(Instant::now),
            ),
        }
    }

    /// The live sidecar handle (possibly stale: callers react to its
    /// failures by calling [`Self::request_respawn`]).
    pub fn current(&self) -> Arc<SidecarRenderer> {
        self.current
            .lock()
            .expect("sidecar supervisor mutex")
            .clone()
    }

    /// The live generation (bumps on every successful respawn).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Whether the current sidecar answers its health probe.
    pub fn alive(&self) -> bool {
        self.current().health_probe()
    }

    /// Best-effort respawn: claims the one respawn slot and hands the
    /// work to a detached thread. Requests inside the backoff window
    /// return without spawning anything.
    pub fn request_respawn(self: &Arc<Self>) {
        if self.respawning.swap(true, Ordering::SeqCst) {
            return; // somebody is already on it
        }

        // Claim the attempt pessimistically: the NEXT attempt's
        // earliest start is scheduled before this one runs, so a
        // failure leaves the backoff in place; a success resets it.
        let due = {
            let mut next_attempt = self.next_attempt.lock().expect("sidecar respawn mutex");
            let now = Instant::now();
            if now < *next_attempt {
                false
            } else {
                let failures = *self.failures.lock().expect("sidecar respawn mutex");
                *next_attempt = now + respawn_backoff(failures);
                true
            }
        };
        if !due {
            self.respawning.store(false, Ordering::SeqCst);
            return;
        }

        let supervisor = Arc::clone(self);
        std::thread::spawn(move || supervisor.respawn_once());
    }

    /// One respawn attempt (detached thread): spawn a fresh sidecar
    /// and swap it in, or record the failure for the backoff.
    fn respawn_once(&self) {
        match SidecarRenderer::spawn(&self.options) {
            Ok(fresh) => {
                *self.failures.lock().expect("sidecar respawn mutex") = 0;
                *self.next_attempt.lock().expect("sidecar respawn mutex") = Instant::now();
                // Dropping the swapped-out Arc kills its (already
                // dead) child; the generation bump tells readers the
                // fresh process needs no probe.
                *self.current.lock().expect("sidecar supervisor mutex") = fresh;
                self.generation.fetch_add(1, Ordering::Relaxed);
                tracing::info!("the JHS sidecar respawned; a fresh generation is serving renders");
            }
            Err(error) => {
                let mut failures = self.failures.lock().expect("sidecar respawn mutex");
                *failures += 1;
                tracing::warn!(
                    %error,
                    consecutive = *failures,
                    "the JHS sidecar could not be respawned; retrying with backoff"
                );
            }
        }
        self.respawning.store(false, Ordering::SeqCst);
    }
}

/// The wait before the next attempt after `consecutive_failures`
/// failed respawns: 10s, 20s, 40s... capped at [`RESPAWN_MAX_BACKOFF`].
fn respawn_backoff(consecutive_failures: u32) -> Duration {
    let factor = 1_u64.checked_shl(consecutive_failures.min(5)).unwrap_or(32);
    RESPAWN_BACKOFF_BASE
        .saturating_mul(factor as u32)
        .min(RESPAWN_MAX_BACKOFF)
}

/// The `[templates] backend = "auto"` composition: sidecar first, boa
/// when the sidecar is unavailable.
///
/// Only **transport** failures ([`JhsError::Sidecar`]) fall back: a
/// broken template is equally broken on boa, so its error is returned
/// as-is. Once the sidecar has failed, renders go straight to boa, a
/// health probe (at most one every [`PROBE_COOLDOWN`]) checks whether
/// the old process recovered, and the [`SidecarSupervisor`] re-spawns
/// a crashed process in the background — a fresh generation flips
/// back to the sidecar without waiting for a probe slot, because a
/// respawned sidecar passed its full selftest by construction.
pub struct AutoRenderer {
    sidecar: Arc<SidecarSupervisor>,
    boa: Arc<JhsEngine>,
    healthy: AtomicBool,
    last_probe: Mutex<Instant>,
    /// The sidecar generation this renderer last served or probed: a
    /// bump means a respawn landed and the new process is healthy by
    /// construction — no probe cooldown applies to it.
    last_generation: AtomicU64,
}

impl AutoRenderer {
    /// Composes the sidecar (under supervision, re-spawnable) and the
    /// boa backends.
    pub fn new(
        sidecar: Arc<SidecarRenderer>,
        options: SidecarOptions,
        boa: Arc<JhsEngine>,
    ) -> Self {
        let supervisor = Arc::new(SidecarSupervisor::new(sidecar, options));
        let last_generation = supervisor.generation();
        Self {
            sidecar: supervisor,
            boa,
            healthy: AtomicBool::new(true),
            // An Instant in the past: the first probe may run at once.
            last_probe: Mutex::new(
                Instant::now()
                    .checked_sub(PROBE_COOLDOWN)
                    .unwrap_or_else(Instant::now),
            ),
            last_generation: AtomicU64::new(last_generation),
        }
    }

    /// Whether the sidecar currently backs this renderer.
    pub fn sidecar_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
}

impl TemplateRenderer for AutoRenderer {
    fn render(
        &self,
        template_path: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        self.dispatch(
            |sidecar| sidecar.render(template_path, data),
            |boa| boa.render(template_path, data),
        )
    }

    fn render_string(
        &self,
        template: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        self.dispatch(
            |sidecar| sidecar.render_string(template, data),
            |boa| boa.render_string(template, data),
        )
    }

    fn backend_name(&self) -> &'static str {
        // The live half of the composition: flips to "boa" the moment
        // a transport failure trips the fallback and back to
        // "sidecar" once the recovery probe answers.
        if self.sidecar_healthy() {
            "sidecar"
        } else {
            "boa"
        }
    }
}

impl AutoRenderer {
    /// Tries the sidecar (when healthy) and falls back to boa on
    /// transport errors, probing for recovery under a cooldown and
    /// re-spawning a dead process in the background.
    fn dispatch<Render>(
        &self,
        sidecar_call: impl Fn(&SidecarRenderer) -> Result<Render, JhsError>,
        boa_call: impl Fn(&JhsEngine) -> Result<Render, JhsError>,
    ) -> Result<Render, JhsError> {
        // At most two rounds: (1) sidecar path, (2) recovery probe then
        // sidecar again — any failure in round 2 falls back for good.
        for _ in 0..2 {
            // A respawned sidecar (a generation we have not served) is
            // healthy by construction: spawn gates on the full
            // selftest. Flip back without waiting for a probe slot.
            let generation = self.sidecar.generation();
            if self.last_generation.load(Ordering::Relaxed) != generation {
                self.last_generation.store(generation, Ordering::Relaxed);
                if !self.healthy.load(Ordering::Relaxed) {
                    tracing::info!("a respawned JHS sidecar took over; resuming sidecar rendering");
                }
                self.healthy.store(true, Ordering::Relaxed);
            }

            if self.healthy.load(Ordering::Relaxed) {
                let sidecar = self.sidecar.current();
                match sidecar_call(sidecar.as_ref()) {
                    Ok(value) => return Ok(value),
                    Err(error) => match error {
                        JhsError::Sidecar(message) => {
                            self.healthy.store(false, Ordering::Relaxed);
                            tracing::error!(
                                %message,
                                "the JHS sidecar failed mid-render; falling back to the \
                                 boa backend until it recovers"
                            );
                            // The failed process may be dead, not just
                            // wedged: ask for a respawn (deduplicated
                            // and backed off by the supervisor).
                            self.sidecar.request_respawn();
                            return boa_call(&self.boa);
                        }
                        template_error => return Err(template_error),
                    },
                }
            }

            // Unhealthy: probe (rate-limited), then loop for one more
            // sidecar attempt; a probe that says dead asks for a
            // respawn instead of waiting to be asked again.
            let probe_due = {
                let mut last_probe = self.last_probe.lock().expect("sidecar probe mutex");
                if last_probe.elapsed() >= PROBE_COOLDOWN {
                    *last_probe = Instant::now();
                    true
                } else {
                    false
                }
            };
            if probe_due {
                if self.sidecar.current().health_probe() {
                    self.healthy.store(true, Ordering::Relaxed);
                    tracing::info!(
                        "the JHS sidecar answered its health probe; resuming sidecar rendering"
                    );
                    continue;
                }
                self.sidecar.request_respawn();
            }
            return boa_call(&self.boa);
        }
        // Unreachable: round 2 either renders on the recovered sidecar
        // or falls back inside its error branch.
        boa_call(&self.boa)
    }
}

/// The strict sidecar backend with self-healing.
///
/// Strict means no silent fallback — a dead sidecar's renders answer
/// the transport error (that is what `"auto"` is for) — but not no
/// recovery: a failed process is re-spawned in the background and the
/// next render after the swap rides the fresh process.
pub struct StrictSidecar {
    supervisor: Arc<SidecarSupervisor>,
}

impl StrictSidecar {
    /// Wraps a live sidecar with the respawn recipe.
    pub fn new(sidecar: Arc<SidecarRenderer>, options: SidecarOptions) -> Self {
        Self {
            supervisor: Arc::new(SidecarSupervisor::new(sidecar, options)),
        }
    }
}

impl TemplateRenderer for StrictSidecar {
    fn render(
        &self,
        template_path: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        self.dispatch(|sidecar| sidecar.render(template_path, data))
    }

    fn render_string(
        &self,
        template: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        self.dispatch(|sidecar| sidecar.render_string(template, data))
    }

    fn backend_name(&self) -> &'static str {
        "sidecar"
    }
}

impl StrictSidecar {
    fn dispatch<Render>(
        &self,
        sidecar_call: impl Fn(&SidecarRenderer) -> Result<Render, JhsError>,
    ) -> Result<Render, JhsError> {
        let sidecar = self.supervisor.current();
        match sidecar_call(sidecar.as_ref()) {
            Err(JhsError::Sidecar(message)) => {
                // The error surfaces as-is (strict), but the process
                // gets its respawn on the way out.
                self.supervisor.request_respawn();
                Err(JhsError::Sidecar(message))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_end_is_detected_at_the_first_blank_line() {
        assert_eq!(
            find_header_end(b"HTTP/1.1 200 OK\r\nA: 1\r\n\r\nbody"),
            Some("HTTP/1.1 200 OK\r\nA: 1".len())
        );
        assert_eq!(find_header_end(b"no terminator"), None);
        assert_eq!(find_header_end(b"\r\n\r\n"), Some(0));
    }

    #[test]
    fn json_responses_are_framed_and_parsed() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let addr = listener.local_addr().expect("local address");
        let writer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let body = br#"{"ok":true,"html":"<p>hi</p>","console":[],"redirect":null}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).expect("head write");
            socket.write_all(body).expect("body write");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        let value = read_json_response(&mut client).expect("parsed response");
        assert_eq!(value["html"], "<p>hi</p>");
        writer.join().expect("writer thread");
    }

    #[test]
    fn truncated_responses_are_rejected() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let addr = listener.local_addr().expect("local address");
        let writer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            // Content-length says 50 but only 10 bytes arrive.
            let head = "HTTP/1.1 200 OK\r\ncontent-length: 50\r\n\r\n";
            socket.write_all(head.as_bytes()).expect("head write");
            socket.write_all(b"0123456789").expect("body write");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("timeout");
        assert!(read_json_response(&mut client).is_err());
        writer.join().expect("writer thread");
    }

    #[test]
    fn failure_envelopes_map_onto_the_ports_error_kinds() {
        let io = SidecarFailure {
            kind: String::from("io"),
            message: String::from("ENOENT: no such file"),
            path: None,
        };
        match map_failure(&io) {
            JhsError::Io(error) => {
                assert!(error.to_string().contains("ENOENT"));
            }
            other => panic!("expected an io error, got {other:?}"),
        }

        let execution = SidecarFailure {
            kind: String::from("execution"),
            message: String::from("Template execution error (<string>): boom"),
            path: Some(String::from("<string>")),
        };
        match map_failure(&execution) {
            JhsError::Execution { path, message } => {
                assert_eq!(path, "<string>");
                assert!(message.starts_with("Template execution error"));
            }
            other => panic!("expected an execution error, got {other:?}"),
        }
    }

    /// Shared by the test above (and mirroring render_call's match).
    fn map_failure(failure: &SidecarFailure) -> JhsError {
        match failure.kind.as_str() {
            "io" => JhsError::Io(std::io::Error::other(failure.message.clone())),
            "include" => JhsError::Include(failure.message.clone()),
            "execution" | "timeout" | "worker" => JhsError::Execution {
                path: failure.path.clone().unwrap_or_default(),
                message: failure.message.clone(),
            },
            _ => JhsError::Sidecar(failure.message.clone()),
        }
    }

    // ── Supervision: respawn after a crash ──────────────────────────

    use crate::template_engine::{JhsOptions, RequireOptions, DEFAULT_FORBIDDEN_MODULES};

    /// Whether a usable Node.js runtime answers `node --version`
    /// (mirrors tests/sidecar.rs: the whole section needs a real one).
    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// Spawn options against the repository's own trees.
    fn supervisor_options() -> SidecarOptions {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        SidecarOptions {
            node_command: String::from("node"),
            script: root.join("sidecar/jhs-sidecar.mjs"),
            views_dir: root.join("views"),
            modules_dir: root.join("modules"),
            // The port's default banner: the startup selftest requires
            // require('fs') to be rejected, so an empty list would fail
            // spawn() outright.
            forbidden: DEFAULT_FORBIDDEN_MODULES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            auto_escape: true,
            require_enabled: true,
            workers: 1,
            startup_timeout: Duration::from_secs(8),
            request_timeout: Duration::from_secs(4),
            render_budget: Duration::from_secs(3),
        }
    }

    /// The boa companion a state would build from the same options.
    fn boa_for_options(options: &SidecarOptions) -> Arc<JhsEngine> {
        Arc::new(JhsEngine::new(JhsOptions {
            views_path: options.views_dir.clone(),
            cache: true,
            auto_escape: options.auto_escape,
            tags: Default::default(),
            loop_iteration_limit: 10_000_000,
            require: RequireOptions {
                enabled: options.require_enabled,
                modules_dir: options.modules_dir.clone(),
                forbidden: options.forbidden.clone(),
            },
        }))
    }

    /// Kills the supervisor's live child outright (the crash being
    /// simulated); the handle is left empty so the renderer's Drop has
    /// nothing left to kill.
    fn kill_the_child(supervisor: &SidecarSupervisor) {
        let current = supervisor.current();
        let mut guard = current.child.lock().expect("sidecar child mutex");
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[test]
    fn auto_backend_respawns_a_dead_sidecar() {
        if !node_available() {
            eprintln!("skipping: no node on PATH");
            return;
        }
        let options = supervisor_options();
        let sidecar = SidecarRenderer::spawn(&options).expect("the sidecar spawns");
        let renderer = AutoRenderer::new(sidecar, options.clone(), boa_for_options(&options));

        assert_eq!(renderer.backend_name(), "sidecar");
        kill_the_child(&renderer.sidecar);

        // The crash surfaces as a boa fallback, never as an error.
        let output = renderer
            .render_string("after <?= \"crash\" ?>", &Map::new())
            .expect("boa fallback renders");
        assert_eq!(output.html, "after crash");
        assert_eq!(renderer.backend_name(), "boa");

        // The supervisor brings the sidecar back: the fresh generation
        // flips the renderer without waiting for a probe slot (a real
        // spawn + selftest takes a couple of seconds — poll with a
        // generous bound).
        let deadline = Instant::now() + Duration::from_secs(20);
        while renderer.backend_name() != "sidecar" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            let _ = renderer.render_string("poke", &Map::new());
        }
        assert_eq!(
            renderer.backend_name(),
            "sidecar",
            "the sidecar must respawn"
        );

        let output = renderer
            .render_string("revived <?= 6 * 7 ?>", &Map::new())
            .expect("the respawned sidecar renders");
        assert_eq!(output.html, "revived 42");
    }

    #[test]
    fn strict_backend_recovers_after_a_sidecar_crash() {
        if !node_available() {
            eprintln!("skipping: no node on PATH");
            return;
        }
        let options = supervisor_options();
        let sidecar = SidecarRenderer::spawn(&options).expect("the sidecar spawns");
        let renderer = StrictSidecar::new(sidecar, options.clone());

        let output = renderer
            .render_string("first <?= 1 ?>", &Map::new())
            .expect("the sidecar renders");
        assert_eq!(output.html, "first 1");
        assert_eq!(renderer.backend_name(), "sidecar");

        kill_the_child(&renderer.supervisor);

        // Strict: the crash IS an error — no silent boa fallback.
        let error = renderer
            .render_string("x", &Map::new())
            .expect_err("a dead strict sidecar errors");
        assert!(matches!(error, JhsError::Sidecar(_)), "error: {error:?}");

        // ...but not a permanent one: the respawn lands and the next
        // renders ride the fresh process.
        let deadline = Instant::now() + Duration::from_secs(20);
        let revived = loop {
            match renderer.render_string("revived <?= 2 ?>", &Map::new()) {
                Ok(output) => break output,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "the strict sidecar must respawn; last error: {error}"
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        };
        assert_eq!(revived.html, "revived 2");
    }

    #[test]
    fn failed_respawns_back_off_instead_of_fork_bombing() {
        if !node_available() {
            eprintln!("skipping: no node on PATH");
            return;
        }
        let good = supervisor_options();
        let sidecar = SidecarRenderer::spawn(&good).expect("the sidecar spawns");

        // The supervisor's recipe points at a Node that does not
        // exist, so every respawn attempt fails fast.
        let mut broken = good.clone();
        broken.node_command = String::from("wallermax-test-node-binary-that-does-not-exist");
        let supervisor = Arc::new(SidecarSupervisor::new(sidecar, broken));

        // First attempt: immediate.
        supervisor.request_respawn();
        let deadline = Instant::now() + Duration::from_secs(5);
        while supervisor.respawning.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !supervisor.respawning.load(Ordering::SeqCst),
            "the failed attempt must release the slot"
        );
        assert_eq!(
            *supervisor.failures.lock().expect("sidecar respawn mutex"),
            1,
            "the failed attempt must be counted"
        );

        // An immediate second request is swallowed by the backoff
        // window (the failure counter must not move).
        supervisor.request_respawn();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            *supervisor.failures.lock().expect("sidecar respawn mutex"),
            1,
            "the backoff window must hold a second attempt back"
        );
    }
}
