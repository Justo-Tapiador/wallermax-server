/* wallermax-manager — the UI logic. Vanilla JS, no framework, no build.
 * Every call goes to a thin Tauri command that delegates to the core. */

"use strict";

const invoke = window.__TAURI__.core.invoke;

/* ------------------------------------------------------------------ */
/* helpers                                                             */
/* ------------------------------------------------------------------ */

const $ = (id) => document.getElementById(id);

function toast(message, kind) {
  const node = document.createElement("div");
  node.className = `toast ${kind || ""}`;
  node.textContent = message;
  $("toasts").appendChild(node);
  setTimeout(() => node.remove(), 6000);
}

function showBanner(id, message, kind) {
  const node = $(id);
  if (!message) {
    node.style.display = "none";
    return;
  }
  node.className = `banner ${kind || ""}`;
  node.innerHTML = ansiToHtml(message);
  node.style.display = "block";
}

/* The centered transition state: from the button press until the
 * manager reflects the change. It waits a beat before appearing, so
 * snappy actions never flash it — and while it is up, the backdrop
 * swallows stray clicks (no double-starts, no double-saves). */
async function withBusy(label, task) {
  const overlay = $("busy");
  if (!overlay) return task(); // a stripped-down shell: behave as before
  $("busy-text").textContent = label;
  const reveal = setTimeout(() => { overlay.style.display = "grid"; }, 130);
  try {
    return await task();
  } finally {
    clearTimeout(reveal);
    overlay.style.display = "none";
  }
}

function fmtDuration(seconds) {
  if (seconds === null || seconds === undefined) return "—";
  const s = Math.floor(seconds);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s % 60}s`;
  return `${s}s`;
}

function fmtNumber(value) {
  if (value === null || value === undefined) return "—";
  const n = Number(value);
  if (!Number.isFinite(n)) return "—";
  if (Number.isInteger(n)) return n.toLocaleString("en-US");
  return n.toFixed(2);
}

function fmtWhen(unixSeconds) {
  if (!unixSeconds) return "—";
  return new Date(unixSeconds * 1000).toLocaleString();
}

function fmtBytes(bytes) {
  if (bytes === null || bytes === undefined) return "—";
  if (bytes < 1024) return `${bytes} B`;
  return `${(bytes / 1024).toFixed(1)} KiB`;
}

function textNode(value) {
  return document.createTextNode(value === null || value === undefined ? "—" : String(value));
}

/* ------------------------------------------------------------------ */
/* ANSI: the server's console colours, rendered as HTML                */
/* ------------------------------------------------------------------ */

/* The server's tracing subscriber writes Select Graphic Rendition
 * codes (green INFO, yellow WARN, dim timestamps, bold targets) meant
 * for a real terminal. A webview would paint them as literal "[32m"
 * noise, so every surface that shows captured output — the Logs page,
 * the server-page preview and the boot-probe banner — renders through
 * `ansiToHtml`: the text is HTML-escaped first (log lines can carry
 * user-controlled text), SGR sequences become semantic spans (the
 * .ansi-* classes in styles.css), and anything else the terminal
 * protocols define (cursor moves, window titles, ...) is swallowed.
 * Unknown SGR codes are ignored, so a future server logging something
 * new degrades to plain, safe text. */

const ANSI_FG = {
  30: "ansi-black", 31: "ansi-red", 32: "ansi-green", 33: "ansi-yellow",
  34: "ansi-blue", 35: "ansi-magenta", 36: "ansi-cyan", 37: "ansi-white",
  90: "ansi-bright-black", 91: "ansi-bright-red", 92: "ansi-bright-green",
  93: "ansi-bright-yellow", 94: "ansi-bright-blue", 95: "ansi-bright-magenta",
  96: "ansi-bright-cyan", 97: "ansi-bright-white",
};

function escapeHtml(value) {
  return value.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function dropStyle(style, ...classes) {
  for (const name of classes) {
    const at = style.indexOf(name);
    if (at !== -1) style.splice(at, 1);
  }
}

/* Applies one SGR parameter list ("1;32") to the running style. */
function applySgr(params, style) {
  const parts = params === "" ? ["0"] : params.split(";");
  for (let i = 0; i < parts.length; i += 1) {
    const code = parts[i] === "" ? 0 : Number.parseInt(parts[i], 10);
    if (Number.isNaN(code)) continue;
    if (code === 0) {
      style.length = 0;
    } else if (code === 1) {
      style.push("ansi-bold");
    } else if (code === 2) {
      style.push("ansi-dim");
    } else if (code === 3) {
      style.push("ansi-italic");
    } else if (code === 4) {
      style.push("ansi-underline");
    } else if (code === 22) {
      dropStyle(style, "ansi-bold", "ansi-dim");
    } else if (code === 23) {
      dropStyle(style, "ansi-italic");
    } else if (code === 24) {
      dropStyle(style, "ansi-underline");
    } else if (ANSI_FG[code] !== undefined) {
      dropStyle(style, ...Object.values(ANSI_FG));
      style.push(ANSI_FG[code]);
    } else if (code === 39) {
      dropStyle(style, ...Object.values(ANSI_FG));
    } else if (code === 38 && parts[i + 1] === "5") {
      /* Indexed colour: the first 16 mirror the classic palette; the
       * rest is beyond what a log line needs, but is consumed either
       * way so its parameters are not misread as new codes. */
      const indexed = Number.parseInt(parts[i + 2], 10);
      dropStyle(style, ...Object.values(ANSI_FG));
      if (indexed >= 0 && indexed <= 7) style.push(ANSI_FG[30 + indexed]);
      else if (indexed >= 8 && indexed <= 15) style.push(ANSI_FG[82 + indexed]);
      i += 2;
    } else if (code === 48 && parts[i + 1] === "5") {
      i += 2; /* background colour: consumed, never rendered */
    } else if ((code === 38 || code === 48) && parts[i + 1] === "2") {
      i += 4; /* 24-bit colour: consumed, never rendered */
    }
  }
}

function ansiToHtml(text) {
  const out = [];
  let run = "";      // plain text waiting to be emitted
  let style = [];    // classes the next span will carry
  let open = false;  // a span is currently open

  const flush = () => {
    if (run === "") return;
    if (!open && style.length > 0) {
      out.push(`<span class="${style.join(" ")}">`);
      open = true;
    }
    out.push(escapeHtml(run));
    run = "";
  };
  const close = () => {
    flush();
    if (open) {
      out.push("</span>");
      open = false;
    }
  };

  let i = 0;
  while (i < text.length) {
    if (text[i] !== "\x1b") {
      run += text[i];
      i += 1;
      continue;
    }
    if (text[i + 1] === "[") {
      /* CSI: parameter bytes (0x20-0x3f), then one final byte. */
      let j = i + 2;
      while (j < text.length) {
        const byte = text.charCodeAt(j);
        if (byte < 0x20 || byte > 0x3f) break;
        j += 1;
      }
      if (j >= text.length) break; // truncated sequence: drop the tail
      if (text[j] === "m") {
        close();
        /* SGR is incremental: a sequence like 22 only switches one
         * attribute off and must keep the rest (colour included), so
         * the running style carries over — only code 0 clears it. */
        applySgr(text.slice(i + 2, j), style);
      }
      i = j + 1;
    } else if (text[i + 1] === "]") {
      /* OSC (window titles and friends): swallow up to BEL or ST. */
      let j = i + 2;
      while (j < text.length && text[j] !== "\x07" && !(text[j] === "\x1b" && text[j + 1] === "\\")) {
        j += 1;
      }
      i = Math.min(j + (text[j] === "\x07" ? 1 : 2), text.length);
    } else {
      /* Any other escape: the introducer, then any intermediate bytes
       * (0x20-0x2f), then the final byte — e.g. ESC ( B ("select US
       * charset") is three bytes, and its final must not leak as text. */
      let j = i + 1;
      while (j < text.length) {
        const byte = text.charCodeAt(j);
        j += 1;
        if (byte >= 0x20 && byte <= 0x2f) continue;
        break;
      }
      i = j;
    }
  }
  flush();
  if (open) out.push("</span>");
  return out.join("");
}

/* ------------------------------------------------------------------ */
/* the configuration form (curated, scalar fields only)                */
/* ------------------------------------------------------------------ */

const CONFIG_FIELDS = [
  { section: "server", key: "server.host", label: "Bind host", placeholder: "127.0.0.1" },
  { section: "server", key: "server.port", label: "Port", placeholder: "8080" },
  { section: "server", key: "server.request_timeout_secs", label: "Request timeout (s)", placeholder: "15" },
  { section: "server", key: "server.shutdown_timeout_secs", label: "Shutdown timeout (s)", placeholder: "15" },
  { section: "server", key: "server.max_body_size_bytes", label: "Max body (bytes)", placeholder: "1048576" },
  { section: "logging", key: "logging.level", label: "Log level", select: ["trace", "debug", "info", "warn", "error"], placeholder: "info" },
  { section: "logging", key: "logging.format", label: "Log format", select: ["pretty", "json", "compact"], placeholder: "pretty" },
  { section: "rate_limit", key: "rate_limit.capacity", label: "Rate-limit capacity", placeholder: "60" },
  { section: "rate_limit", key: "rate_limit.refill_per_second", label: "Rate-limit refill/s", placeholder: "10" },
  { section: "database", key: "database.enabled", label: "Database enabled", check: true },
  { section: "database", key: "database.url", label: "SQLite URL", wide: true, placeholder: "sqlite://wallermax.db?mode=rwc" },
  { section: "database", key: "database.max_connections", label: "DB pool size", placeholder: "5" },
  { section: "auth", key: "auth.enabled", label: "Auth enabled", check: true },
  { section: "cms", key: "cms.enabled", label: "CMS enabled", check: true },
  { section: "static", key: "static.enabled", label: "Static serving enabled", check: true },
  { section: "static", key: "static.root_dir", label: "Static root dir", placeholder: "public" },
  { section: "static", key: "static.index_file", label: "Static index file", placeholder: "index.html" },
  { section: "templates", key: "templates.enabled", label: "Templates enabled", check: true },
  { section: "templates", key: "templates.views_dir", label: "Views dir", placeholder: "views" },
  { section: "metrics", key: "metrics.enabled", label: "Metrics enabled", check: true },
  { section: "tls", key: "tls.enabled", label: "TLS enabled", check: true },
  { section: "tls", key: "tls.http_listen", label: "Plain-HTTP redirect", hint: "optional host:port that 308-redirects to HTTPS" },
  { section: "tls", key: "tls.cert_path", label: "TLS certificate (PEM)", wide: true, hint: "PEM certificate chain — required while tls.enabled; relative to the server's working directory" },
  { section: "tls", key: "tls.key_path", label: "TLS private key (PEM)", wide: true, hint: "PEM private key — required while tls.enabled; relative to the server's working directory" },
];

const MIDDLEWARE_SWITCHES = [
  ["middleware.request_id", "request-id"],
  ["middleware.logging", "logging"],
  ["middleware.security_headers", "security headers"],
  ["middleware.timeout", "timeout"],
  ["middleware.rate_limit", "rate limit"],
  ["middleware.cors", "cors"],
  ["middleware.body_limit", "body limit"],
];

/* ------------------------------------------------------------------ */
/* state                                                               */
/* ------------------------------------------------------------------ */

const state = {
  page: "dashboard",
  file: "base",          // configuration layer being edited
  mode: "form",           // form | raw
  backupFile: "base",
  logCursor: 0,
  logLines: [],
  timers: { status: null, logs: null, dashboard: null },
  lastStatus: null,       // the process state behind the dashboard's banners
  origin: "",             // the origin the endpoints derive from
  serverTls: false,        // whether the configuration enables [tls] on the server
  detectedOrigin: "",     // the server's own address, read from the configuration
  siteUrl: "",            // the website URL behind the Open website buttons
  portStatus: null,        // who holds the server's port, when someone does
};

/* ------------------------------------------------------------------ */
/* polling                                                             */
/* ------------------------------------------------------------------ */

async function pollStatus() {
  try {
    const status = await invoke("server_status");
    state.lastStatus = status;
    paintStatus(status);
  } catch (error) {
    /* a transient IPC problem must not kill the loop */
    console.error("status poll failed", error);
  }
}

async function pollDashboard() {
  try {
    const dash = await invoke("dashboard_fetch");
    // The port probe answers the question the unreachable banner
    // cannot: is the server's port already held by someone else (a
    // leftover from an earlier session)? Only asked when it matters —
    // nothing reachable and nothing supervised.
    const running = state.lastStatus && state.lastStatus.state === "running";
    if (!dash.reachable && !running) {
      try {
        state.portStatus = await invoke("port_status");
      } catch (error) {
        console.error("port probe failed", error);
        state.portStatus = null;
      }
    } else {
      state.portStatus = null;
    }
    paintDashboard(dash);
    paintTakeover();
  } catch (error) {
    showBanner("dash-error", String(error), "error");
  }
}

async function pollLogs() {
  try {
    const page = await invoke("server_logs", { since: state.logCursor, limit: 400 });
    state.logCursor = page.next_seq;
    for (const line of page.lines) state.logLines.push(line);
    if (state.logLines.length > 4000) {
      state.logLines.splice(0, state.logLines.length - 4000);
    }
    renderLogs();
  } catch (error) {
    console.error("log poll failed", error);
  }
}

function restartTimers() {
  for (const key of Object.keys(state.timers)) {
    if (state.timers[key]) clearInterval(state.timers[key]);
    state.timers[key] = null;
  }
  state.timers.status = setInterval(pollStatus, 2000);
  if (state.page === "dashboard") {
    state.timers.dashboard = setInterval(pollDashboard, 5000);
  }
  if (state.page === "logs" || state.page === "server") {
    state.timers.logs = setInterval(pollLogs, 1200);
  }
}

/* ------------------------------------------------------------------ */
/* painting                                                            */
/* ------------------------------------------------------------------ */

function paintStatus(status) {
  const running = status.state === "running";
  const exited = status.state === "exited";
  for (const pill of [$("pill-side"), $("pill-top")]) {
    pill.className = `state-pill ${running ? "state-running" : exited ? "state-exited" : "state-stopped"}`;
  }
  const label = running ? `Running (pid ${status.pid})` : exited ? "Exited" : "Stopped";
  $("pill-text").textContent = label;
  $("pill-text-top").textContent = label;
  $("pill-sub").textContent = running
    ? `supervised since ${fmtWhen(status.started_at_unix)}`
    : exited
      ? `exit code ${status.exit_code === null ? "unknown" : status.exit_code}`
      : "no supervised process yet";

  $("srv-state").textContent = label;
  $("srv-pid").textContent = status.pid ?? "—";
  $("srv-started").textContent = fmtWhen(status.started_at_unix);
  $("srv-exit").textContent = exited ? (status.exit_code ?? "signal / unknown") : "—";

  $("btn-start").disabled = running;
  $("btn-start-side").disabled = running;
  $("btn-stop").disabled = !running;
  $("btn-stop-side").disabled = !running;

  // The website link lives exactly as long as the server does: shown
  // while the process runs, gone the moment it stops. A button — not
  // an anchor — so the click goes to the OS opener and never navigates
  // the manager's own webview.
  for (const node of [$("btn-open-site"), $("btn-open-site-side")]) {
    node.style.display = running ? "" : "none";
    node.title = running && state.siteUrl ? state.siteUrl : "";
  }

  $("dash-state").textContent = running ? "Running" : exited ? "Exited" : "Stopped";
  $("dash-state-sub").textContent = running ? `pid ${status.pid}` : "process supervision";
}

function paintDashboard(dash) {
  // The banners must say what is actually wrong. "Start it from the
  // sidebar" is a lie when the process IS running and the endpoint is
  // what refuses to answer — that is an origin/port mismatch, and the
  // message has to point there.
  const status = state.lastStatus;
  const running = status && status.state === "running";
  const exited = status && status.state === "exited";
  if (dash.reachable) {
    showBanner("dash-notice", "");
    showBanner("dash-error", "");
  } else if (running) {
    const where = state.origin ? ` at ${state.origin}` : "";
    // A TLS server behind an `http://` origin is the classic mismatch
    // once certificates enter the picture — say so by name.
    const tlsHint = state.serverTls && state.origin.startsWith("http://")
      ? " The server serves TLS — Settings offers its https origin."
      : "";
    showBanner("dash-notice", `The server process is running (pid ${status.pid}) but its endpoint${where} is not answering — the origin in Settings must match the server's own host and port.${tlsHint}`, "error");
    showBanner("dash-error", dash.error || "endpoint unreachable", "error");
  } else if (exited) {
    showBanner("dash-notice", "The server process has exited — the recent output is on the Logs page.", "");
    showBanner("dash-error", "");
  } else if (state.portStatus && state.portStatus.occupied) {
    // The takeover banner below tells the whole story — the notice must
    // not suggest a start that would only collide with the squatter.
    showBanner("dash-notice", "");
    showBanner("dash-error", "");
  } else {
    showBanner("dash-notice", "The server is not answering yet — start it from the sidebar.", "");
    showBanner("dash-error", "");
  }
  $("dash-uptime").textContent = dash.reachable ? fmtDuration(dash.uptime_seconds) : "—";
  $("dash-requests").textContent = fmtNumber(dash.requests_total);
  $("dash-rps").textContent = fmtNumber(dash.requests_per_second);
  $("dash-limited").textContent = fmtNumber(dash.rate_limited_requests);
  $("dash-users").textContent = dash.registered_users === null || dash.registered_users === undefined ? "auth off" : fmtNumber(dash.registered_users);
  $("dash-version").textContent = dash.version || "—";
}

/* ------------------------------------------------------------------ */
/* log rendering                                                       */
/* ------------------------------------------------------------------ */

function renderLogs() {
  const box = $("log-box");
  const filter = $("log-filter").value.trim().toLowerCase();
  const showOut = $("log-show-out").checked;
  const showErr = $("log-show-err").checked;

  const keep = [];
  for (const line of state.logLines) {
    if (line.stream === "out" && !showOut) continue;
    if (line.stream === "err" && !showErr) continue;
    if (filter && !line.text.toLowerCase().includes(filter)) continue;
    keep.push(line);
  }

  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 60;
  box.replaceChildren();
  if (keep.length === 0) {
    const empty = document.createElement("span");
    empty.className = "muted";
    empty.textContent = "nothing captured yet";
    box.appendChild(empty);
  } else {
    const fragment = document.createDocumentFragment();
    for (const line of keep.slice(-2500)) {
      const row = document.createElement("div");
      row.className = `log-line ${line.stream}`;
      const stream = document.createElement("span");
      stream.className = "stream";
      stream.textContent = line.stream === "out" ? "out" : "err";
      const seq = document.createElement("span");
      seq.className = "seq";
      seq.textContent = `#${line.seq}`;
      const text = document.createElement("span");
      text.className = "text";
      text.innerHTML = ansiToHtml(line.text);
      row.append(stream, seq, text);
      fragment.appendChild(row);
    }
    box.appendChild(fragment);
  }
  if ($("log-autoscroll").checked && nearBottom) {
    box.scrollTop = box.scrollHeight;
  }
  $("log-count").textContent = `${keep.length} line${keep.length === 1 ? "" : "s"}`;

  // The server page shows a small preview too.
  const preview = $("srv-log-preview");
  preview.replaceChildren();
  const recent = state.logLines.slice(-25);
  if (recent.length === 0) {
    const empty = document.createElement("span");
    empty.className = "muted";
    empty.textContent = "no output captured yet — start the server or open Logs";
    preview.appendChild(empty);
  } else {
    for (const line of recent) {
      const row = document.createElement("div");
      row.className = `log-line ${line.stream}`;
      const text = document.createElement("span");
      text.className = "text";
      text.innerHTML = ansiToHtml(line.text);
      row.appendChild(text);
      preview.appendChild(row);
    }
  }
}

/* ------------------------------------------------------------------ */
/* configuration editor                                                */
/* ------------------------------------------------------------------ */

function buildForm() {
  const form = $("config-form");
  form.replaceChildren();

  const sectionTitles = new Map();
  for (const field of CONFIG_FIELDS) {
    if (!sectionTitles.has(field.section)) {
      sectionTitles.set(field.section, []);
    }
    sectionTitles.get(field.section).push(field);
  }

  const buildField = (field) => {
    const wrap = document.createElement("div");
    wrap.className = "field" + (field.wide ? " wide" : "");
    const label = document.createElement("label");
    label.htmlFor = `cfg-${field.key}`;
    label.textContent = field.label + " ";
    const code = document.createElement("code");
    code.textContent = field.key;
    label.appendChild(code);
    wrap.appendChild(label);

    let input;
    if (field.check) {
      input = document.createElement("input");
      input.type = "checkbox";
      input.id = `cfg-${field.key}`;
      input.dataset.check = "1";
    } else if (field.select) {
      input = document.createElement("select");
      input.id = `cfg-${field.key}`;
      const blank = document.createElement("option");
      blank.value = "";
      blank.textContent = `(${field.placeholder})`;
      input.appendChild(blank);
      for (const option of field.select) {
        const node = document.createElement("option");
        node.value = option;
        node.textContent = option;
        input.appendChild(node);
      }
    } else {
      input = document.createElement("input");
      input.type = "text";
      input.id = `cfg-${field.key}`;
      // `hint` fields describe what to type (the TLS paths have no
      // default to show); the others advertise the file's default.
      input.placeholder = field.hint || (field.placeholder ? `default: ${field.placeholder}` : "");
    }
    input.dataset.key = field.key;
    wrap.appendChild(input);
    return wrap;
  };

  for (const [section, fields] of sectionTitles) {
    const heading = document.createElement("h3");
    heading.textContent = `[${section}]`;
    heading.style.margin = "16px 0 10px";
    if (form.childElementCount === 0) heading.style.marginTop = "0";
    form.appendChild(heading);

    const grid = document.createElement("div");
    grid.className = "form-grid";
    for (const field of fields) grid.appendChild(buildField(field));
    form.appendChild(grid);

    if (section === "server") {
      const mwTitle = document.createElement("h3");
      mwTitle.textContent = "[middleware] switches";
      mwTitle.style.margin = "16px 0 10px";
      const checks = document.createElement("div");
      checks.className = "checks";
      for (const [key, label] of MIDDLEWARE_SWITCHES) {
        const box = document.createElement("label");
        box.className = "check";
        const input = document.createElement("input");
        input.type = "checkbox";
        input.id = `cfg-${key}`;
        input.dataset.key = key;
        input.dataset.check = "1";
        box.appendChild(input);
        box.appendChild(textNode(label));
        checks.appendChild(box);
      }
      form.append(mwTitle, checks);
    }
  }
}

async function loadForm() {
  const which = state.file;
  const jobs = [];
  for (const key of fieldKeys()) {
    jobs.push(invoke("config_get_value", { file: which, key }).catch(() => null));
  }
  const values = await Promise.all(jobs);
  const inputs = document.querySelectorAll("#config-form [data-key]");
  inputs.forEach((input) => {
    const value = values[fieldKeys().indexOf(input.dataset.key)];
    if (input.dataset.check) {
      input.checked = value === "true";
      input.indeterminate = value === null || value === undefined;
    } else {
      input.value = value === null || value === undefined ? "" : value;
    }
  });
}

function fieldKeys() {
  return [...CONFIG_FIELDS.map((f) => f.key), ...MIDDLEWARE_SWITCHES.map(([k]) => k)];
}

async function saveForm() {
  await withBusy("Saving the configuration…", async () => {
    const pairs = [];
    for (const input of document.querySelectorAll("#config-form [data-key]")) {
      let value;
      if (input.dataset.check) {
        if (input.indeterminate) continue; // untouched: leave the file as it is
        value = input.checked ? "true" : "false";
      } else {
        value = input.value.trim();
        if (value === "") continue; // untouched: leave the file as it is
      }
      pairs.push([input.dataset.key, value]);
    }
    if (pairs.length === 0) {
      showBanner("config-banner", "Nothing to save — every field is untouched.", "");
      return;
    }
    try {
      await invoke("config_set_values", { file: state.file, values: pairs });
      showBanner("config-banner", `Saved ${pairs.length} value${pairs.length === 1 ? "" : "s"} to ${state.file === "base" ? "wallermax.toml" : "wallermax.local.toml"} (validated, backup kept).`, "ok");
    } catch (error) {
      showBanner("config-banner", String(error), "error");
    }
    await loadBackups();
  });
}

async function loadRaw() {
  try {
    const text = await invoke("config_read", { file: state.file });
    $("raw-editor").value = text;
    const exists = await invoke("config_exists", { file: state.file });
    $("raw-meta").textContent = exists
      ? "the file exists on disk — saving validates first and keeps a backup"
      : "the file does not exist yet — the first save creates it";
  } catch (error) {
    showBanner("config-banner", String(error), "error");
  }
}

async function saveRaw() {
  await withBusy("Saving the layer…", async () => {
    try {
      await invoke("config_write", { file: state.file, content: $("raw-editor").value });
      showBanner("config-banner", "Layer saved (validated, backup kept).", "ok");
    } catch (error) {
      showBanner("config-banner", String(error), "error");
    }
    await loadBackups();
  });
}

/* ------------------------------------------------------------------ */
/* tools                                                               */
/* ------------------------------------------------------------------ */

async function loadPaths() {
  try {
    const info = await invoke("paths_info");
    $("path-settings").replaceChildren(textNode(info.settings_path));
    $("path-configdir").replaceChildren(textNode(info.config_dir));
    $("path-base").replaceChildren(textNode(info.base_path));
    $("path-local").replaceChildren(textNode(info.local_path));
    $("settings-path-label").replaceChildren(textNode(info.settings_path));
    showBanner("tools-notice", info.notice || "", "error");
  } catch (error) {
    showBanner("tools-notice", String(error), "error");
  }
}

async function loadBackups() {
  const which = state.backupFile;
  try {
    const backups = await invoke("config_backups", { file: which });
    const rows = $("backup-rows");
    rows.replaceChildren();
    if (backups.length === 0) {
      const cell = document.createElement("td");
      cell.colSpan = 4;
      cell.className = "muted";
      cell.textContent = "no backups yet — every save keeps one";
      const row = document.createElement("tr");
      row.appendChild(cell);
      rows.appendChild(row);
      return;
    }
    for (const backup of backups) {
      const row = document.createElement("tr");
      const name = document.createElement("td");
      name.className = "mono";
      name.textContent = backup.file_name;
      const size = document.createElement("td");
      size.textContent = fmtBytes(backup.bytes);
      const modified = document.createElement("td");
      modified.textContent = fmtWhen(backup.modified_unix);
      const actions = document.createElement("td");
      const button = document.createElement("button");
      button.className = "btn small";
      button.textContent = "Restore";
      button.addEventListener("click", async () => {
        if (!window.confirm(`Restore ${backup.file_name} over the current ${which === "base" ? "wallermax.toml" : "wallermax.local.toml"}?`)) {
          return;
        }
        await withBusy(`Restoring ${backup.file_name}…`, async () => {
          try {
            await invoke("config_restore_backup", { file: which, fileName: backup.file_name });
            toast(`Restored ${backup.file_name} (the replaced file was backed up too).`, "ok");
          } catch (error) {
            toast(String(error), "error");
          }
          await loadBackups();
        });
      });
      actions.appendChild(button);
      row.append(name, size, modified, actions);
      rows.appendChild(row);
    }
  } catch (error) {
    toast(String(error), "error");
  }
}

/* ------------------------------------------------------------------ */
/* settings                                                            */
/* ------------------------------------------------------------------ */

async function loadSettings() {
  try {
    const settings = await invoke("settings_get");
    $("set-command").value = settings.server_command;
    $("set-workdir").value = settings.working_dir;
    $("set-configdir").value = settings.config_dir;
    $("set-origin").value = settings.origin;
    $("set-site").value = settings.site_url || "";
    $("set-health").value = settings.health_path;
    $("set-metrics").value = settings.metrics_path;
    $("set-stats").value = settings.stats_path;
    $("set-probe").value = settings.probe_window_ms;
    $("set-grace").value = settings.stop_grace_ms;
    $("origin-chip").textContent = settings.origin;
    state.origin = settings.origin;
    state.siteUrl = (settings.site_url || "").trim() || settings.origin;
    await loadOriginHint();
  } catch (error) {
    toast(String(error), "error");
  }
}

/* The server's own address, straight out of the configuration pair —
 * the one-click correction for a mismatched origin (the classic
 * "http://localhost" versus a server bound to 127.0.0.1:8080, or an
 * `http://` origin against a server serving TLS on the same port). */
async function loadOriginHint() {
  const [host, port, tlsEnabled, httpListen] = await Promise.all([
    configValue("server.host"),
    configValue("server.port"),
    configValue("tls.enabled"),
    configValue("tls.http_listen"),
  ]);
  const detectedHost = host || "127.0.0.1";
  const detectedPort = String(port || "8080");
  const tls = String(tlsEnabled).toLowerCase() === "true";
  state.serverTls = tls;
  state.detectedOrigin = `${tls ? "https" : "http"}://${detectedHost}:${detectedPort}`;
  const hint = $("origin-hint");
  if (!hint) return;
  $("origin-hint-text").textContent = tls
    ? `the server's wallermax.toml serves TLS on ${detectedHost}:${detectedPort}${httpListen ? ` (plain ${httpListen} redirects to it)` : ""}`
    : `the server's wallermax.toml binds ${detectedHost}:${detectedPort}`;
  hint.style.display = "block";
}

/* One configuration value, read the way the server reads it: the local
 * layer beats the base layer, and anything missing is null. */
async function configValue(key) {
  for (const file of ["local", "base"]) {
    const value = await invoke("config_get_value", { file, key }).catch(() => null);
    if (value !== null && value !== undefined && String(value).trim() !== "") return value;
  }
  return null;
}

async function saveSettings() {
  await withBusy("Saving the settings…", async () => {
    const settings = {
      server_command: $("set-command").value.trim(),
      working_dir: $("set-workdir").value.trim(),
      config_dir: $("set-configdir").value.trim(),
      origin: $("set-origin").value.trim(),
      site_url: $("set-site").value.trim(),
      health_path: $("set-health").value.trim(),
      metrics_path: $("set-metrics").value.trim(),
      stats_path: $("set-stats").value.trim(),
      probe_window_ms: Number($("set-probe").value) || 4000,
      stop_grace_ms: Number($("set-grace").value) || 5000,
    };
    try {
      await invoke("settings_save", { settings });
      toast("Settings saved and applied.", "ok");
      $("origin-chip").textContent = settings.origin;
      await loadPaths();
      await Promise.all([pollDashboard(), pollStatus()]);
    } catch (error) {
      toast(String(error), "error");
    }
  });
}

/* ------------------------------------------------------------------ */
/* the port banner: who holds it, and the one-click takeover            */
/* ------------------------------------------------------------------ */

/* Painted after the unreachable banner: when something already holds
 * the address a fresh server would bind, the banner names it — and,
 * when every owner is the configured server program (the classic
 * invisible leftover from an earlier session), offers to take the
 * port over and start. */
function paintTakeover() {
  const node = $("dash-takeover");
  const status = state.portStatus;
  const running = state.lastStatus && state.lastStatus.state === "running";
  if (!status || !status.occupied || running) {
    node.style.display = "none";
    return;
  }
  const who = status.owners.length
    ? status.owners.map((owner) => `pid ${owner.pid} (${owner.image})`).join(", ")
    : "a process this OS could not name";
  const cause = status.owners.length
    ? " — likely a server left over from an earlier manager session (servers run without a console window)."
    : ".";
  node.className = "banner error";
  node.replaceChildren(
    document.createTextNode(
      `The server's address ${status.address} is already in use by ${who}${cause}`
    )
  );
  if (status.takeover_ready) {
    const button = document.createElement("button");
    button.id = "btn-takeover";
    button.className = "btn small";
    button.textContent = "Take over the port and start";
    button.addEventListener("click", takeoverStart);
    node.append(document.createElement("br"), button);
  }
  node.style.display = "block";
}

async function takeoverStart() {
  const button = $("btn-takeover");
  if (button) button.disabled = true;
  await withBusy("Taking over the port…", async () => {
    try {
      const report = await invoke("server_takeover");
      if (report.probe.booted) {
        showBanner("probe-banner", report.probe.output.trim()
          ? `Takeover done — boot probe: healthy.\n${report.probe.output.trim()}`
          : `Takeover done — the process is up (pid ${report.pid}).`, "ok");
      } else {
        showBanner("probe-banner",
          `Takeover done, but the boot probe failed${report.probe.exit_code === null ? "" : ` (exit code ${report.probe.exit_code})`}.\n${report.probe.output.trim() || "no output captured"}`,
          "error");
      }
    } catch (error) {
      showBanner("probe-banner", String(error), "error");
    }
    state.portStatus = null;
    paintTakeover();
    await Promise.all([pollStatus(), pollDashboard()]);
  });
}

/* ------------------------------------------------------------------ */
/* server actions                                                      */
/* ------------------------------------------------------------------ */

async function startServer() {
  await withBusy("Starting the server…", async () => {
    try {
      const report = await invoke("server_start");
      if (report.probe.booted) {
        showBanner("probe-banner", report.probe.output.trim()
          ? `Boot probe: healthy.\n${report.probe.output.trim()}`
          : `Boot probe: the process is up (pid ${report.pid}).`, "ok");
      } else {
        showBanner("probe-banner",
          `Boot probe: the process refused to start${report.probe.exit_code === null ? "" : ` (exit code ${report.probe.exit_code})`}.\n${report.probe.output.trim() || "no output captured"}`,
          "error");
      }
    } catch (error) {
      showBanner("probe-banner", String(error), "error");
      // A refused start may be a busy port — the banner knows who holds
      // it, so ask right away (the takeover button rides on it).
      try {
        state.portStatus = await invoke("port_status");
      } catch (probeError) {
        console.error("port probe failed", probeError);
        state.portStatus = null;
      }
      paintTakeover();
    }
    await pollStatus();
  });
}

async function stopServer() {
  await withBusy("Stopping the server…", async () => {
    try {
      await invoke("server_stop");
      toast("Stop issued — the tree is taken down and the exit recorded.", "ok");
    } catch (error) {
      toast(String(error), "error");
    }
    await pollStatus();
  });
}

async function openSite() {
  try {
    await invoke("open_site");
  } catch (error) {
    toast(String(error), "error");
  }
}

/* ------------------------------------------------------------------ */
/* navigation                                                          */
/* ------------------------------------------------------------------ */

const TITLES = {
  dashboard: "Dashboard",
  configuration: "Configuration",
  server: "Server",
  logs: "Logs",
  tools: "Tools",
  settings: "Settings",
};

async function showPage(page) {
  state.page = page;
  for (const button of $("nav").querySelectorAll("button")) {
    button.classList.toggle("active", button.dataset.page === page);
  }
  for (const section of document.querySelectorAll("section.page")) {
    section.classList.toggle("active", section.id === `page-${page}`);
  }
  $("where").textContent = TITLES[page] || page;
  restartTimers();

  if (page === "dashboard") pollDashboard();
  if (page === "configuration") {
    if (state.mode === "form") await loadForm();
    else await loadRaw();
  }
  if (page === "tools") {
    await loadPaths();
    await loadBackups();
  }
  if (page === "settings") await loadSettings();
  if (page === "logs") {
    state.logCursor = 0;
    state.logLines = [];
    await pollLogs();
  }
  pollStatus();
}

/* ------------------------------------------------------------------ */
/* wiring                                                              */
/* ------------------------------------------------------------------ */

window.addEventListener("DOMContentLoaded", () => {
  buildForm();

  for (const button of $("nav").querySelectorAll("button")) {
    button.addEventListener("click", () => showPage(button.dataset.page));
  }

  for (const button of $("file-seg").querySelectorAll("button")) {
    button.addEventListener("click", async () => {
      state.file = button.dataset.file;
      for (const other of $("file-seg").querySelectorAll("button")) {
        other.classList.toggle("active", other === button);
      }
      if (state.mode === "form") await loadForm();
      else await loadRaw();
    });
  }

  for (const button of $("mode-seg").querySelectorAll("button")) {
    button.addEventListener("click", async () => {
      state.mode = button.dataset.mode;
      for (const other of $("mode-seg").querySelectorAll("button")) {
        other.classList.toggle("active", other === button);
      }
      $("config-form-wrap").style.display = state.mode === "form" ? "block" : "none";
      $("config-raw-wrap").style.display = state.mode === "raw" ? "block" : "none";
      if (state.mode === "form") await loadForm();
      else await loadRaw();
    });
  }

  for (const button of $("backup-seg").querySelectorAll("button")) {
    button.addEventListener("click", async () => {
      state.backupFile = button.dataset.file;
      for (const other of $("backup-seg").querySelectorAll("button")) {
        other.classList.toggle("active", other === button);
      }
      await loadBackups();
    });
  }

  $("btn-form-save").addEventListener("click", saveForm);
  $("btn-config-validate").addEventListener("click", async () => {
    await withBusy("Validating the pair…", async () => {
      try {
        await invoke("config_validate");
        showBanner("config-banner", "The pair on disk validates against the server's real rules.", "ok");
      } catch (error) {
        showBanner("config-banner", String(error), "error");
      }
    });
  });
  $("btn-raw-save").addEventListener("click", saveRaw);
  $("btn-raw-reload").addEventListener("click", loadRaw);

  $("btn-start").addEventListener("click", startServer);
  $("btn-stop").addEventListener("click", stopServer);
  $("btn-start-side").addEventListener("click", startServer);
  $("btn-stop-side").addEventListener("click", stopServer);
  $("btn-open-site").addEventListener("click", openSite);
  $("btn-open-site-side").addEventListener("click", openSite);
  $("btn-refresh-status").addEventListener("click", pollStatus);
  $("btn-refresh-dash").addEventListener("click", pollDashboard);

  $("btn-open-folder").addEventListener("click", async () => {
    try {
      await invoke("open_config_folder");
    } catch (error) {
      toast(String(error), "error");
    }
  });
  $("btn-tools-validate").addEventListener("click", async () => {
    await withBusy("Validating the pair…", async () => {
      try {
        await invoke("config_validate");
        toast("The pair on disk validates.", "ok");
      } catch (error) {
        toast(String(error), "error");
      }
    });
  });

  $("btn-settings-save").addEventListener("click", saveSettings);

  $("btn-use-origin").addEventListener("click", () => {
    if (state.detectedOrigin) {
      $("set-origin").value = state.detectedOrigin;
    }
  });

  $("log-filter").addEventListener("input", renderLogs);
  $("log-show-out").addEventListener("change", renderLogs);
  $("log-show-err").addEventListener("change", renderLogs);
  $("log-autoscroll").addEventListener("change", renderLogs);
  $("btn-log-clear").addEventListener("click", () => {
    state.logLines = [];
    renderLogs();
  });

  restartTimers();
  showPage("dashboard");
  loadPaths();
  loadSettings();
});
