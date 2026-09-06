//! End-to-end integration tests for static file serving (`[static]`).
//!
//! Each test boots the real server stack against a temporary directory
//! containing fixture files and exercises the static family: index file,
//! nested assets, directory indexes, JSON 404 envelope for missing files,
//! method handling, traversal rejection and coexistence with the API.

mod common;

use std::path::Path;

use common::TestServer;
use serde_json::Value;
use wallermax_server::config::AppConfig;

/// Test fixture directory: index.html, style.css, app.js and docs/index.html.
struct FixtureDir {
    path: std::path::PathBuf,
}

impl FixtureDir {
    /// Creates the fixture tree in a unique temporary directory.
    fn create(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("wallermax-static-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(path.join("docs")).expect("fixture dirs create");
        std::fs::write(path.join("index.html"), INDEX_HTML).expect("index write");
        std::fs::write(path.join("style.css"), CSS).expect("css write");
        std::fs::write(path.join("app.js"), JS).expect("js write");
        std::fs::write(path.join("docs").join("index.html"), DOCS_HTML).expect("docs index write");
        Self { path }
    }

    /// Builds a configuration with static serving enabled on this root.
    fn config(&self) -> AppConfig {
        let mut config = AppConfig::default();
        config.static_files.enabled = true;
        config.static_files.root_dir = self.path.to_string_lossy().into_owned();
        config
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const INDEX_HTML: &str = "<!DOCTYPE html><html><body><h1>Hello, world!</h1></body></html>";
const CSS: &str = "body { margin: 0; }";
const JS: &str = "console.log('fixture');";
const DOCS_HTML: &str = "<!DOCTYPE html><html><body>docs index</body></html>";

#[tokio::test]
async fn root_serves_the_index_file() {
    let fixture = FixtureDir::create("root");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii value")
        .starts_with("text/html"));

    let body = response.text().await.expect("body text");
    assert!(body.contains("Hello, world!"), "index body: {body}");
}

#[tokio::test]
async fn head_root_returns_headers_without_body() {
    let fixture = FixtureDir::create("head");
    let server = TestServer::start_with_config(fixture.config()).await;
    let client = reqwest::Client::new();

    let response = client.head(server.url("/")).send().await.expect("head ok");
    assert_eq!(response.status(), 200);

    let body = response.text().await.expect("body text");
    assert!(body.is_empty(), "HEAD body should be empty: {body:?}");
}

#[tokio::test]
async fn assets_are_served_with_content_types() {
    let fixture = FixtureDir::create("assets");
    let server = TestServer::start_with_config(fixture.config()).await;
    let client = reqwest::Client::new();

    let css = client
        .get(server.url("/style.css"))
        .send()
        .await
        .expect("css request succeeds");
    assert_eq!(css.status(), 200);
    assert!(css
        .headers()
        .get("content-type")
        .expect("css content type")
        .to_str()
        .expect("ascii value")
        .starts_with("text/css"));
    assert_eq!(css.text().await.expect("css body"), CSS);

    let js = client
        .get(server.url("/app.js"))
        .send()
        .await
        .expect("js request succeeds");
    assert_eq!(js.status(), 200);
    assert!(js
        .headers()
        .get("content-type")
        .expect("js content type")
        .to_str()
        .expect("ascii value")
        .starts_with("text/javascript"));
    assert_eq!(js.text().await.expect("js body"), JS);
}

#[tokio::test]
async fn directory_requests_serve_their_index() {
    let fixture = FixtureDir::create("dirs");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/docs/"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii value")
        .starts_with("text/html"));
    let body = response.text().await.expect("body text");
    assert!(body.contains("docs index"), "docs body: {body}");
}

#[tokio::test]
async fn missing_files_answer_the_json_404_envelope() {
    let fixture = FixtureDir::create("missing");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/does-not-exist.css"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 404);
    assert!(response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii value")
        .starts_with("application/json"));

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message string")
        .contains("/does-not-exist.css"));
    assert!(!body["error"]["request_id"]
        .as_str()
        .unwrap_or("")
        .is_empty());
}

#[tokio::test]
async fn post_to_a_file_path_answers_the_json_404_envelope() {
    // Mirrors the API behaviour for unmatched paths: the fallback layer
    // answers 404 regardless of method.
    let fixture = FixtureDir::create("post");
    let server = TestServer::start_with_config(fixture.config()).await;
    let client = reqwest::Client::new();

    let response = client
        .post(server.url("/style.css"))
        .body("overwrite attempt")
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 404);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn post_root_answers_the_json_405_envelope() {
    // `/` is a known route (the static index file), so the wrong method
    // triggers the 405 method-not-allowed fallback.
    let fixture = FixtureDir::create("post-root");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::Client::new()
        .post(server.url("/"))
        .body("x")
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 405);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "METHOD_NOT_ALLOWED");
}

#[tokio::test]
async fn traversal_attempts_stay_inside_the_root() {
    let fixture = FixtureDir::create("traversal");
    let server = TestServer::start_with_config(fixture.config()).await;
    let client = reqwest::Client::new();

    // Percent-encoded `..` segments must be rejected by the file service
    // before anything touches the filesystem.
    let response = client
        .get(server.url("/%2e%2e/%2e%2e/etc/passwd"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 404);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn static_responses_carry_the_security_headers() {
    let fixture = FixtureDir::create("headers");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert_eq!(
        headers
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        headers.get("x-frame-options").and_then(|v| v.to_str().ok()),
        Some("DENY")
    );
    assert!(headers.contains_key("content-security-policy"));
}

#[tokio::test]
async fn api_routes_keep_precedence_over_static_files() {
    let fixture = FixtureDir::create("coexist");
    let server = TestServer::start_with_config(fixture.config()).await;
    let client = reqwest::Client::new();

    let health: Value = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("health request succeeds")
        .json()
        .await
        .expect("health JSON");
    assert_eq!(health["status"], "ok");

    let stats: Value = client
        .get(server.url("/api/stats"))
        .send()
        .await
        .expect("stats request succeeds")
        .json()
        .await
        .expect("stats JSON");
    assert!(stats["uptime_seconds"].is_number());
}

#[tokio::test]
async fn service_index_is_served_at_api_with_static_enabled() {
    let fixture = FixtureDir::create("index");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/api"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["service"], "wallermax-server");
    let endpoints = body["endpoints"].as_array().expect("endpoints array");
    assert!(endpoints
        .iter()
        .any(|e| e == "GET / (static index + files)"));
    assert!(endpoints.iter().any(|e| e == "GET /api"));
}

#[tokio::test]
async fn service_index_stays_at_root_when_static_is_disabled() {
    // Regression: the Phase 1-3 behaviour (JSON index at `/`) is intact.
    let server = TestServer::start().await;
    let response = reqwest::get(server.url("/"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["service"], "wallermax-server");
    let endpoints = body["endpoints"].as_array().expect("endpoints array");
    assert!(endpoints.iter().any(|e| e == "GET /"));

    // The `/api` alias exists in both modes.
    let api = reqwest::get(server.url("/api"))
        .await
        .expect("api request succeeds");
    assert_eq!(api.status(), 200);
}

#[tokio::test]
async fn conditional_get_answers_304_with_last_modified() {
    let fixture = FixtureDir::create("conditional");
    let server = TestServer::start_with_config(fixture.config()).await;
    let client = reqwest::Client::new();

    let first = client
        .get(server.url("/style.css"))
        .send()
        .await
        .expect("first");
    let last_modified = first
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .expect("last-modified header on first response");

    let second = client
        .get(server.url("/style.css"))
        .header("if-modified-since", last_modified)
        .send()
        .await
        .expect("second");
    assert_eq!(second.status(), 304);
    assert!(second.text().await.expect("empty body").is_empty());
}

#[tokio::test]
async fn range_requests_answer_206_partial_content() {
    let fixture = FixtureDir::create("range");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::Client::new()
        .get(server.url("/style.css"))
        .header("range", "bytes=0-4")
        .send()
        .await
        .expect("range request succeeds");
    assert_eq!(response.status(), 206);
    let body = response.text().await.expect("partial body");
    assert_eq!(body, "body ");
}

#[tokio::test]
async fn missing_root_directory_is_a_startup_error() {
    let mut config = AppConfig::default();
    config.static_files.enabled = true;
    config.static_files.root_dir = "no-such-directory-for-wallermax-tests".to_owned();

    let error = wallermax_server::server::build_state(&config)
        .await
        .err()
        .expect("missing root dir rejected");
    let message = error.to_string();
    assert!(
        message.contains("no-such-directory-for-wallermax-tests"),
        "{message}"
    );
}

#[tokio::test]
async fn build_state_accepts_an_existing_root() {
    let fixture = FixtureDir::create("startup");
    let state = wallermax_server::server::build_state(&fixture.config())
        .await
        .expect("state builds with static enabled");
    assert!(!state.auth_enabled());
    assert!(Path::new(&fixture.config().static_files.root_dir).is_dir());
}
