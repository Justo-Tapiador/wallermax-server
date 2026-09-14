//! Integration battery for the main host's directory indexes: an
//! `index.jhs` beside the static `index.html` takes the directory.
//!
//! Boots the real server stack against fixture trees carrying both
//! index flavours and walks the whole contract: `/` and `/docs/`
//! rendering the `.jhs` first (engine output, never the raw source),
//! the static fallbacks when no `.jhs` exists, `.jhs`-only
//! directories, the trailing-slash redirect, `HEAD` and `POST`
//! handling, the 404 end of the chain, the `cms.default_page`
//! precedence (and its graceful degrade), the CMS host staying on
//! the views tree while `public/` never leaks there, and the vhost
//! main host adopting the same order.

mod common;

use std::path::PathBuf;

use common::{auth_config, TestServer};
use reqwest::header::{HeaderValue, HOST};
use serde_json::{json, Value};
use wallermax_server::config::AppConfig;

/// The hostname the CMS answers on in the vhost tests.
const CMS_HOST: &str = "cms.test";

/// Fixture tree: a static root and a views directory, every flavour
/// of index present — `index.jhs` beside `index.html` at the root,
/// `docs/` with both, `manual/` static-only, `about/` jhs-only.
struct FixtureDir {
    path: PathBuf,
}

impl FixtureDir {
    fn create(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("wallermax-index-jhs-{tag}-{}", std::process::id()));
        let static_root = path.join("static");
        let views = path.join("views");
        let modules = path.join("modules");
        std::fs::create_dir_all(static_root.join("docs")).expect("docs dir");
        std::fs::create_dir_all(static_root.join("manual")).expect("manual dir");
        std::fs::create_dir_all(static_root.join("about")).expect("about dir");
        std::fs::create_dir_all(&views).expect("views dir");
        std::fs::create_dir_all(&modules).expect("modules dir");

        std::fs::write(static_root.join("index.jhs"), ROOT_JHS).expect("root jhs write");
        std::fs::write(static_root.join("index.html"), ROOT_HTML).expect("root html write");
        std::fs::write(static_root.join("docs").join("index.jhs"), DOCS_JHS)
            .expect("docs jhs write");
        std::fs::write(static_root.join("docs").join("index.html"), DOCS_HTML)
            .expect("docs html write");
        std::fs::write(static_root.join("manual").join("index.html"), MANUAL_HTML)
            .expect("manual html write");
        std::fs::write(static_root.join("about").join("index.jhs"), ABOUT_JHS)
            .expect("about jhs write");
        std::fs::write(views.join("index.jhs"), VIEW_INDEX).expect("view index write");

        Self { path }
    }

    /// Single-host configuration: static + templates on this tree.
    fn config(&self) -> AppConfig {
        let mut config = AppConfig::default();
        config.static_files.enabled = true;
        config.static_files.root_dir = self.static_root();
        config.templates.enabled = true;
        config.templates.views_dir = self.views_dir();
        config.templates.modules_dir = self.modules_dir();
        config
    }

    /// Vhost configuration (the full stack): the CMS pinned to
    /// [`CMS_HOST`], this tree behind the main host.
    fn vhost_config(&self) -> (AppConfig, common::TempDbGuard) {
        let (mut config, db) = auth_config();
        config.static_files.enabled = true;
        config.static_files.root_dir = self.static_root();
        config.templates.enabled = true;
        config.templates.views_dir = self.views_dir();
        config.templates.modules_dir = self.modules_dir();
        config.cms.enabled = true;
        config.cms.hosts = vec![String::from(CMS_HOST)];
        (config, db)
    }

    /// Forward-slash path (valid on Windows as well).
    fn static_root(&self) -> String {
        self.path
            .join("static")
            .display()
            .to_string()
            .replace('\\', "/")
    }

    /// Forward-slash path (valid on Windows as well).
    fn views_dir(&self) -> String {
        self.path
            .join("views")
            .display()
            .to_string()
            .replace('\\', "/")
    }

    /// Forward-slash path (valid on Windows as well).
    fn modules_dir(&self) -> String {
        self.path
            .join("modules")
            .display()
            .to_string()
            .replace('\\', "/")
    }

    /// Removes a file from the static root (fallback scenarios).
    fn without(&self, relative: &str) {
        std::fs::remove_file(self.path.join("static").join(relative))
            .expect("fixture file removed");
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The dynamic indexes print runtime expressions so the assertions
/// prove the engine ran — the raw source bytes would read differently.
const ROOT_JHS: &str = "<h1><?= \"home \" + \"jhs\" ?></h1><footer><?= 6 * 7 ?></footer>";
const ROOT_HTML: &str = "<h1>static home</h1>";
const DOCS_JHS: &str = "<h1><?= \"docs \" + \"jhs\" ?></h1>";
const DOCS_HTML: &str = "<h1>static docs</h1>";
const MANUAL_HTML: &str = "<h1>static manual</h1>";
const ABOUT_JHS: &str = "<h1><?= \"about \" + \"jhs\" ?></h1>";
const VIEW_INDEX: &str = "<h1>view index</h1>";

/// `true` when the response carries an HTML content type.
fn is_html(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"))
}

/// The `cache-control` value, empty when absent.
fn cache_control(response: &reqwest::Response) -> &str {
    response
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// A `Host` header value for the vhost tests.
fn host(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("test host value")
}

// ── The single host: the chain from `index.jhs` down to the 404 ────

#[tokio::test]
async fn root_renders_index_jhs_before_the_static_index() {
    let fixture = FixtureDir::create("root");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/")).await.expect("request");
    assert_eq!(response.status(), 200);
    assert!(is_html(&response), "the homepage is HTML");
    assert_eq!(cache_control(&response), "no-store", "the render path");
    let body = response.text().await.expect("body");
    assert!(body.contains("home jhs"), "engine output: {body}");
    assert!(body.contains("42"), "the expression ran: {body}");
    assert!(
        !body.contains("<?jhs") && !body.contains("<?="),
        "never the raw source: {body}"
    );
    assert!(
        !body.contains("static home"),
        "the .jhs takes the directory: {body}"
    );
}

#[tokio::test]
async fn root_falls_back_to_the_static_index_without_jhs() {
    let fixture = FixtureDir::create("fallback");
    fixture.without("index.jhs");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/")).await.expect("request");
    assert_eq!(response.status(), 200);
    assert!(is_html(&response), "the homepage is HTML");
    // Read before `text()` consumes the response.
    let cache = cache_control(&response).to_owned();
    let body = response.text().await.expect("body");
    assert!(
        body.contains("static home"),
        "the static index answers: {body}"
    );
    assert!(
        !cache.contains("no-store"),
        "the static path, not the render path"
    );
}

#[tokio::test]
async fn root_falls_through_to_the_view_when_no_index_at_all() {
    // The single-host chain keeps its last resort: no public index at
    // all -> `views/index.jhs` takes the homepage.
    let fixture = FixtureDir::create("view");
    fixture.without("index.jhs");
    fixture.without("index.html");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("view index"),
        "the views tree takes over: {body}"
    );
}

#[tokio::test]
async fn directory_renders_its_index_jhs() {
    let fixture = FixtureDir::create("dirs");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/docs/")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("docs jhs"), "the dynamic index: {body}");
    assert!(
        !body.contains("static docs"),
        "it outranks the static one: {body}"
    );
}

#[tokio::test]
async fn directory_falls_back_to_its_index_html() {
    let fixture = FixtureDir::create("manual");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/manual/")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("static manual"), "the static index: {body}");
}

#[tokio::test]
async fn a_jhs_only_directory_renders_without_index_html() {
    let fixture = FixtureDir::create("about");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/about/")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("about jhs"), "no static index needed: {body}");
}

#[tokio::test]
async fn directory_urls_redirect_to_the_slash_form_first() {
    let fixture = FixtureDir::create("redirect");
    let server = TestServer::start_with_config(fixture.config()).await;
    // Redirect-free: each hop is asserted by hand.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds");

    let response = client
        .get(server.url("/docs"))
        .send()
        .await
        .expect("request");
    assert!(
        response.status().is_redirection(),
        "the static layer adds the slash: {}",
        response.status()
    );
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/docs/");

    let response = client
        .get(server.url("/docs/"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("docs jhs"), "the slash form renders: {body}");
}

#[tokio::test]
async fn the_explicit_index_jhs_path_still_renders() {
    let fixture = FixtureDir::create("explicit");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/index.jhs"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("home jhs"), "rendered: {body}");
    assert!(!body.contains("<?="), "never the raw source: {body}");
}

#[tokio::test]
async fn head_root_answers_the_render_without_a_body() {
    let fixture = FixtureDir::create("head");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::Client::new()
        .head(server.url("/"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.is_empty(), "HEAD carries headers only: {body:?}");
}

#[tokio::test]
async fn post_root_still_answers_the_405_envelope() {
    // The takeover is read-only: `/` stays a known route, so the wrong
    // method keeps the 405 method-not-allowed fallback.
    let fixture = FixtureDir::create("post-root");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::Client::new()
        .post(server.url("/"))
        .body("x")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 405);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "METHOD_NOT_ALLOWED");
}

// ── Virtual hosts: the main host adopts the chain, the CMS host ────
// ── never lets `public/` indexes leak onto its surface ─────────────

#[tokio::test]
async fn the_main_host_root_renders_index_jhs() {
    let fixture = FixtureDir::create("vhost-main");
    let (config, _db) = fixture.vhost_config();
    let server = TestServer::start_full(config).await;

    // No Host override: the ephemeral 127.0.0.1:port is a main-host
    // request (an unknown host fails safe to the main tree).
    let response = reqwest::get(server.url("/")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("home jhs"),
        "the main host's dynamic home: {body}"
    );
    assert!(!body.contains("view index"), "not the views tree: {body}");
}

#[tokio::test]
async fn the_main_host_root_is_a_404_without_any_index() {
    let fixture = FixtureDir::create("vhost-404");
    fixture.without("index.jhs");
    fixture.without("index.html");
    let (config, _db) = fixture.vhost_config();
    let server = TestServer::start_full(config).await;

    // The main host's 404s stay 404s — no views auto-routing there.
    let response = reqwest::get(server.url("/")).await.expect("request");
    assert_eq!(response.status(), 404);
    let body = response.text().await.expect("body");
    assert!(
        !body.contains("home jhs") && !body.contains("view index"),
        "nothing dynamic answers: {body}"
    );
}

#[tokio::test]
async fn the_cms_host_home_stays_views_even_with_a_public_index_jhs() {
    let fixture = FixtureDir::create("vhost-cms");
    let (config, _db) = fixture.vhost_config();
    let server = TestServer::start_full(config).await;

    let response = reqwest::Client::new()
        .get(server.url("/"))
        .header(HOST, host(CMS_HOST))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("view index"),
        "the CMS home is the views tree: {body}"
    );
    assert!(
        !body.contains("home jhs"),
        "public/ never leaks there: {body}"
    );
}

#[tokio::test]
async fn the_cms_host_directories_do_not_render_public_index_jhs() {
    let fixture = FixtureDir::create("vhost-cms-dirs");
    let (config, _db) = fixture.vhost_config();
    let server = TestServer::start_full(config).await;

    let response = reqwest::Client::new()
        .get(server.url("/docs/"))
        .header(HOST, host(CMS_HOST))
        .send()
        .await
        .expect("request");
    assert_eq!(
        response.status(),
        404,
        "no views/docs* exists, and public/ stays out"
    );
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

// ── The `cms.default_page` precedence on the single host ───────────

/// Registers the bootstrap admin and logs a cookie-storing browser in
/// through the form endpoint (the no-JS login flow).
async fn editor_session(server: &TestServer) -> reqwest::Client {
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "root-admin", "password": "sup3r-secret!" }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(
        response.status(),
        201,
        "the first account becomes the admin"
    );

    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .expect("browser client builds");
    let response = browser
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=root-admin&password=sup3r-secret!&redirect=/")
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    browser
}

/// Creates a published page through the panel form (PRG) and returns
/// its id, parsed from the redirect target. ASCII-only fields on
/// purpose: the body stays hand-encoded.
async fn create_published_page(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
) -> i64 {
    let body = format!(
        "slug={slug}&title={title}&content={content}&redirect=%2Fadmin%2Fpages&is_published=on"
    );
    let response = client
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("page creation succeeds");
    assert_eq!(response.status(), 303, "creation redirects (PRG)");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    location
        .trim_start_matches("/admin/pages/")
        .split('?')
        .next()
        .expect("path")
        .trim_end_matches("/edit")
        .parse()
        .expect("numeric page id")
}

#[tokio::test]
async fn default_page_beats_the_public_index_jhs() {
    // The explicit configuration beats the filesystem conventions —
    // including the new `public/index.jhs`. Full stack with the
    // repository's own views (the `cms_page.jhs` wrapper included)
    // and the fixture static root carrying both index flavours.
    let fixture = FixtureDir::create("default-page");
    let (mut config, db) = auth_config();
    config.static_files.enabled = true;
    config.static_files.root_dir = fixture.static_root();
    config.templates.enabled = true;
    config.cms.enabled = true;
    config.cms.default_page = Some(String::from("inicio"));
    let server = TestServer::start_full(config).await;

    let admin = editor_session(&server).await;
    let page_id = create_published_page(
        &server,
        &admin,
        "inicio",
        "Portada",
        "portada-dinamica-vive",
    )
    .await;

    let response = reqwest::get(server.url("/")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Portada"),
        "the default page renders at /: {body}"
    );
    assert!(
        body.contains("portada-dinamica-vive"),
        "the CMS page body: {body}"
    );
    assert!(
        !body.contains("home jhs"),
        "the public index.jhs loses: {body}"
    );

    // A slug that stops existing mid-flight degrades gracefully — into
    // the new chain: the public `index.jhs` takes the homepage back,
    // outranking the static index.
    let response = admin
        .post(server.url(&format!("/admin/pages/{page_id}/delete")))
        .send()
        .await
        .expect("deletion succeeds");
    assert_eq!(response.status(), 303, "deletion redirects (PRG)");

    let response = reqwest::get(server.url("/")).await.expect("request");
    assert_eq!(response.status(), 200, "the homepage never hard-fails");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("home jhs"),
        "the public index.jhs takes over back: {body}"
    );
    assert!(
        !body.contains("static home"),
        "and it outranks the static index: {body}"
    );

    drop(db);
}
