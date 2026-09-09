//! Integration tests for the Node sidecar template backend (v0.10.0).
//!
//! Two layers are exercised:
//!
//! - **direct renderer assertions**: `AppState::new` with
//!   `backend = "sidecar"` hands out the [`TemplateRenderer`] seam, and
//!   `render`/`render_string` results (html, captured console, redirect
//!   intents, error wording, the `require()` banner, the hard-kill
//!   budget) are checked without HTTP in between;
//! - **end-to-end parity**: the same fixture tree served once with
//!   `backend = "boa"` and once with `backend = "sidecar"` must render
//!   byte-identical HTML — the contract that makes the sidecar a
//!   drop-in backend rather than a second template language.
//!
//! Every sidecar-booting test is gated on a usable `node` on `PATH`
//! (present on the CI runners and dev machines); without it the tests
//! skip so the suite stays green on Node-less hosts — mirroring the
//! `auto` backend's silent boa fallback.

mod common;

use std::path::PathBuf;

use serde_json::Value;
use wallermax_server::config::AppConfig;
use wallermax_server::state::AppState;
use wallermax_server::template_engine::{JhsError, TemplateRenderer};

use common::TestServer;

/// The repository's sidecar service, resolved from the crate root at
/// compile time (tests may run from any working directory).
const SIDECAR_SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/sidecar/jhs-sidecar.mjs");

/// Fixture tree: a static root (the parity template), a views tree with
/// a shared partial, and a modules directory.
struct FixtureDir {
    path: PathBuf,
}

impl FixtureDir {
    fn create(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "wallermax-sidecar-{tag}-{}-{}",
            std::process::id(),
            tag.replace('-', "_")
        ));
        let static_root = path.join("static");
        let views = path.join("views");
        std::fs::create_dir_all(static_root.join("api")).expect("static dirs create");
        std::fs::create_dir_all(views.join("partials")).expect("partials dir create");
        std::fs::create_dir_all(path.join("modules")).expect("modules dir create");

        std::fs::write(static_root.join("rich.jhs"), RICH_JHS).expect("rich write");
        std::fs::write(views.join("index.jhs"), VIEW_INDEX).expect("view index write");
        std::fs::write(views.join("partials").join("header.jhs"), PARTIAL_HEADER)
            .expect("partial write");

        Self { path }
    }

    /// Configuration with templates enabled on this fixture tree and the
    /// given backend (`"boa"`, `"sidecar"` or `"auto"`).
    fn config(&self, backend: &str) -> AppConfig {
        self.config_with(backend, |_| {})
    }

    fn config_with(&self, backend: &str, tune: impl FnOnce(&mut AppConfig)) -> AppConfig {
        let mut config = AppConfig::default();
        config.static_files.enabled = true;
        config.static_files.root_dir = self.path.join("static").to_string_lossy().into_owned();
        config.templates.enabled = true;
        config.templates.views_dir = self.path.join("views").to_string_lossy().into_owned();
        config.templates.modules_dir = self.path.join("modules").to_string_lossy().into_owned();
        config.templates.backend = backend.to_owned();
        config.templates.sidecar.script = SIDECAR_SCRIPT.to_owned();
        config.templates.sidecar.workers = 1;
        config.templates.sidecar.startup_timeout_ms = 8_000;
        config.templates.sidecar.request_timeout_ms = 4_000;
        config.templates.sidecar.render_budget_ms = 3_000;
        tune(&mut config);
        config
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const RICH_JHS: &str = concat!(
    "<?jhs include(\"partials/header\") ?>\n",
    "<main>\n",
    "hello <?= user ?>\n",
    "<?= \"<b>\" ?>\n",
    "<?= raw(\"<b>ok</b>\") ?>\n",
    "<?jhs var items = [\"a\", \"b\"]; ?>\n",
    "<?jhs items.forEach(function (item) { ?>",
    " <li>#{<?= item ?>}</li>\n",
    "<?jhs }); ?>\n",
    "</main>\n",
    "foot<?= 40 + 2 ?>\n",
);

const VIEW_INDEX: &str = "<html>view index <?= 6 * 7 ?></html>";

const PARTIAL_HEADER: &str = "<header>shared <?= 1 + 1 ?></header>";

/// Whether a usable Node.js runtime answers `node --version`.
fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The renderer behind a freshly built state (or `None` while
/// `[templates]` is disabled).
fn renderer(config: &AppConfig) -> std::sync::Arc<dyn TemplateRenderer> {
    let state = AppState::new(config.clone());
    state.templates().expect("templates enabled").engine()
}

// ── Direct renderer assertions (backend = "sidecar") ─────────────────

#[test]
fn sidecar_renders_string_templates_with_the_original_semantics() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("string");
    let renderer = renderer(&fixture.config("sidecar"));

    let output = renderer
        .render_string("<?= \"<b>\" ?><?= raw(\"<i>\") ?>", &empty_data())
        .expect("renders");
    assert_eq!(output.html, "&lt;b&gt;<i>");
    assert!(output.console.is_empty());
    assert!(output.redirect.is_none());
}

#[test]
fn sidecar_captures_console_lines_per_render() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("console");
    let renderer = renderer(&fixture.config("sidecar"));

    let output = renderer
        .render_string(
            "<?jhs console.log(\"one\"); console.warn(\"two\") ?>",
            &empty_data(),
        )
        .expect("renders");

    assert_eq!(output.console.len(), 2);
    assert_eq!(output.console[0].level, "log");
    assert_eq!(output.console[0].message, "one");
    assert_eq!(output.console[1].level, "warn");
    assert_eq!(output.console[1].message, "two");
}

#[test]
fn sidecar_honours_res_redirect_intents() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("redirect");
    let renderer = renderer(&fixture.config("sidecar"));

    let output = renderer
        .render_string("<?jhs res.redirect(\"/login\", 303) ?>", &empty_data())
        .expect("renders");
    let redirect = output.redirect.expect("redirect intent");
    assert_eq!(redirect.location, "/login");
    assert_eq!(redirect.status, 303);

    // External targets stay rejected (open-redirect protection) with
    // the port's execution-error wording.
    let error = renderer
        .render_string(
            "<?jhs res.redirect(\"https://evil.example\") ?>",
            &empty_data(),
        )
        .expect_err("external redirect rejected");
    assert!(
        error.to_string().contains("open-redirect protection"),
        "error wording: {error}"
    );
}

#[test]
fn sidecar_answers_real_node_require_behind_the_banner() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("require");
    let renderer = renderer(&fixture.config("sidecar"));

    // THE motivating case: real Node built-ins resolve (boa answers the
    // module banner instead).
    let output = renderer
        .render_string(
            "<?jhs var q = require(\"url\").parse(\"/x?probe=7\", true).query; echo(q.probe) ?>",
            &empty_data(),
        )
        .expect("url require works");
    assert_eq!(output.html, "7");

    // The banner keeps the port's exact wording.
    let error = renderer
        .render_string("<?jhs require(\"fs\") ?>", &empty_data())
        .expect_err("fs is forbidden");
    assert!(
        error.to_string().contains(
            "require('fs') is forbidden: the module 'fs' is listed in \
             [templates] forbidden_modules"
        ),
        "banner wording: {error}"
    );
}

#[test]
fn runaway_renders_are_hard_killed_within_the_budget() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("loop");
    let renderer = renderer(&fixture.config_with("sidecar", |config| {
        config.templates.sidecar.render_budget_ms = 300;
        config.templates.sidecar.request_timeout_ms = 2_000;
    }));

    let error = renderer
        .render_string("<?jhs while (true) { } ?>", &empty_data())
        .expect_err("runaway render is bounded");
    assert!(
        error.to_string().contains("worker was terminated"),
        "hard-kill wording: {error}"
    );

    // The killed worker respawns: the very next render works.
    let output = renderer
        .render_string("still <?= \"alive\" ?>", &empty_data())
        .expect("worker respawned");
    assert_eq!(output.html, "still alive");
}

#[test]
fn sidecar_render_string_resolves_includes_from_the_views_dir() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("includes");
    let renderer = renderer(&fixture.config("sidecar"));

    // The CMS seam: stored page bodies embed the shared partials.
    let output = renderer
        .render_string("<?jhs include(\"partials/header\") ?> body", &empty_data())
        .expect("renders");
    assert!(
        output.html.contains("<header>shared 2</header> body"),
        "html: {}",
        output.html
    );
}

// ── Strict and auto backend behaviour ────────────────────────────────

#[test]
fn strict_sidecar_backend_fails_ready_without_node() {
    let fixture = FixtureDir::create("strict");
    let config = fixture.config_with("sidecar", |config| {
        config.templates.sidecar.node_command =
            String::from("wallermax-test-node-binary-that-does-not-exist");
    });

    let state = AppState::new(config);
    let templates = state.templates().expect("templates enabled");
    assert!(
        templates.ensure_ready().is_err(),
        "strict backend must refuse readiness without Node"
    );

    // Every render answers the spawn failure.
    let error = templates
        .engine()
        .render_string("x", &empty_data())
        .expect_err("renders fail");
    assert!(matches!(error, JhsError::Sidecar(_)), "error: {error:?}");
}

#[test]
fn auto_backend_falls_back_to_boa_without_node() {
    let fixture = FixtureDir::create("auto");
    let config = fixture.config_with("auto", |config| {
        config.templates.sidecar.node_command =
            String::from("wallermax-test-node-binary-that-does-not-exist");
    });

    // Without Node the auto backend renders on boa without errors.
    let output = renderer(&config)
        .render_string("<?= \"fallback\" ?><?= 6 * 7 ?>", &empty_data())
        .expect("boa fallback renders");
    assert_eq!(output.html, "fallback42");
}

#[test]
fn backend_name_identifies_the_live_backend() {
    // v0.10.1: `backend_name()` is what `/health` and the
    // `wallermax_template_backend` gauge read. The boa backend always
    // identifies itself, no Node required.
    let fixture = FixtureDir::create("names");
    assert_eq!(renderer(&fixture.config("boa")).backend_name(), "boa");

    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    // The strict sidecar backend reports the Node half.
    assert_eq!(
        renderer(&fixture.config("sidecar")).backend_name(),
        "sidecar"
    );
    // "auto" resolves to whichever half is currently serving — the
    // sidecar, while it answers — and the state accessor surfaces the
    // same value that the health probe and the gauge would report.
    let state = AppState::new(fixture.config("auto"));
    assert_eq!(state.template_backend(), Some("sidecar"));
}

// ── End-to-end: HTTP parity between the boa and sidecar backends ─────

#[tokio::test]
async fn boa_and_sidecar_backends_render_identical_html() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("parity");

    let boa_server = TestServer::start_with_config(fixture.config("boa")).await;
    let sidecar_server = TestServer::start_with_config(fixture.config("sidecar")).await;

    // The static-root template: includes, escaping, raw(), data, loop
    // interleave — everything but request-dependent globals (the two
    // servers live on different ports).
    let boa_body = reqwest::get(boa_server.url("/rich.jhs"))
        .await
        .expect("boa request")
        .text()
        .await
        .expect("boa body");
    let sidecar_body = reqwest::get(sidecar_server.url("/rich.jhs"))
        .await
        .expect("sidecar request")
        .text()
        .await
        .expect("sidecar body");

    assert_eq!(boa_body, sidecar_body, "backends must agree byte for byte");

    // And the auto-routed views tree renders identically too.
    let boa_view = reqwest::get(boa_server.url("/"))
        .await
        .expect("boa view request")
        .text()
        .await
        .expect("boa view body");
    let sidecar_view = reqwest::get(sidecar_server.url("/"))
        .await
        .expect("sidecar view request")
        .text()
        .await
        .expect("sidecar view body");
    assert_eq!(boa_view, sidecar_view);
    assert!(boa_view.contains("view index 42"), "view body: {boa_view}");
}

#[tokio::test]
async fn http_template_errors_keep_the_json_envelope_on_the_sidecar() {
    if !node_available() {
        eprintln!("skipping: no node on PATH");
        return;
    }
    let fixture = FixtureDir::create("errors");
    std::fs::write(
        fixture.path.join("views").join("boom.jhs"),
        "<?jhs throw new Error(\"boom\"); ?>",
    )
    .expect("boom view write");

    let server = TestServer::start_with_config(fixture.config("sidecar")).await;
    let response = reqwest::get(server.url("/boom")).await.expect("request");
    assert_eq!(response.status(), 500);
    let body: Value = response.json().await.expect("json envelope");
    assert_eq!(body["error"]["code"], "INTERNAL_ERROR");
    assert!(body["error"]["request_id"].is_string());
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("Template execution error"),
        "envelope: {body}"
    );
}

fn empty_data() -> serde_json::Map<String, Value> {
    serde_json::Map::new()
}
