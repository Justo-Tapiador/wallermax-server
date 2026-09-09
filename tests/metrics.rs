//! End-to-end integration tests for the Prometheus metrics endpoint.

mod common;

use common::{auth_config, TestServer};
use serde_json::Value;

#[tokio::test]
async fn metrics_answer_the_prometheus_text_format() {
    let mut config = wallermax_config();
    config.metrics.enabled = true;
    let server = TestServer::start_with_config(config).await;

    // Generate some traffic first: vec families only render children
    // that exist.
    reqwest::get(server.url("/health"))
        .await
        .expect("health request succeeds");
    reqwest::get(server.url("/nope"))
        .await
        .expect("404 request succeeds");

    let response = reqwest::get(server.url("/metrics"))
        .await
        .expect("metrics request succeeds");

    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii")
        .to_owned();
    assert_eq!(content_type, "text/plain; version=0.0.4");

    let body = response.text().await.expect("metrics body");
    assert!(body.contains("# HELP wallermax_requests_total"));
    assert!(body.contains("wallermax_requests_total{code=\"200\",method=\"GET\"} 1"));
    assert!(body.contains("wallermax_requests_total{code=\"404\",method=\"GET\"} 1"));
    assert!(body.contains("wallermax_request_duration_seconds_count{method=\"GET\"}"));
    assert!(body.contains("wallermax_rate_limited_requests_total 0"));
    assert!(body.contains("wallermax_uptime_seconds"));
}

#[tokio::test]
async fn metrics_flow_through_the_middleware_pipeline() {
    let mut config = wallermax_config();
    config.metrics.enabled = true;
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/metrics"))
        .await
        .expect("metrics request succeeds");

    // Security headers and the correlation id apply to /metrics too.
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-content-type-options")
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    assert!(response.headers().get("x-request-id").is_some());
}

#[tokio::test]
async fn metrics_are_absent_while_disabled() {
    let server = TestServer::start_with_config(wallermax_config()).await;

    let response = reqwest::get(server.url("/metrics"))
        .await
        .expect("metrics request succeeds");

    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn the_metrics_path_is_configurable() {
    let mut config = wallermax_config();
    config.metrics.enabled = true;
    config.metrics.path = String::from("/internal/metrics");
    let server = TestServer::start_with_config(config).await;

    let moved = reqwest::get(server.url("/internal/metrics"))
        .await
        .expect("custom path serves");
    assert_eq!(moved.status(), 200);

    // The default path is now a regular 404.
    let old = reqwest::get(server.url("/metrics"))
        .await
        .expect("default path absent");
    assert_eq!(old.status(), 404);
}

#[tokio::test]
async fn registered_users_appear_while_auth_is_enabled() {
    let (mut config, _db) = auth_config();
    config.metrics.enabled = true;
    let server = TestServer::start_full(config).await;

    // One registration -> gauge = 1 at scrape time.
    reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&serde_json::json!({
            "username": "alice",
            "password": "password-123"
        }))
        .send()
        .await
        .expect("registration succeeds");

    let body = reqwest::get(server.url("/metrics"))
        .await
        .expect("metrics request succeeds")
        .text()
        .await
        .expect("metrics body");

    assert!(body.contains("wallermax_registered_users 1"));
}

#[tokio::test]
async fn rate_limited_requests_are_counted() {
    let mut config = wallermax_config();
    config.metrics.enabled = true;
    config.middleware.rate_limit = true;
    config.rate_limit.capacity = 2;
    config.rate_limit.refill_per_second = 0.001; // effectively no refill
    let server = TestServer::start_with_config(config).await;

    for _ in 0..4 {
        let _ = reqwest::get(server.url("/health"))
            .await
            .expect("request succeeds");
    }

    let response = reqwest::get(server.url("/metrics"))
        .await
        .expect("metrics request succeeds");

    // Two allowed + two rejected; the scrape itself is exempt from the
    // limiter (monitoring must keep working under throttling).
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("metrics body");
    assert!(
        body.contains("wallermax_rate_limited_requests_total 2"),
        "rate limited counter present in {body}"
    );
}

#[tokio::test]
async fn the_index_lists_the_metrics_endpoint_while_enabled() {
    let mut config = wallermax_config();
    config.metrics.enabled = true;
    let server = TestServer::start_with_config(config).await;

    let index: Value = reqwest::get(server.url("/api"))
        .await
        .expect("index request succeeds")
        .json()
        .await
        .expect("JSON body");
    let endpoints = index["endpoints"].as_array().expect("endpoints array");

    assert!(endpoints.iter().any(|endpoint| endpoint == "GET /metrics"));
}

#[tokio::test]
async fn the_template_backend_gauge_matches_health() {
    // v0.10.1: templates enabled (backend = "auto") so a live backend
    // exists — the sidecar while Node answers, boa otherwise — and the
    // gauge must agree with whatever `/health` reports on this host.
    let dir =
        std::env::temp_dir().join(format!("wallermax-metrics-backend-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("static")).expect("static dir create");
    std::fs::create_dir_all(dir.join("views")).expect("views dir create");
    std::fs::create_dir_all(dir.join("modules")).expect("modules dir create");

    let mut config = wallermax_config();
    config.static_files.enabled = true;
    config.static_files.root_dir = dir.join("static").to_string_lossy().into_owned();
    config.templates.enabled = true;
    config.templates.views_dir = dir.join("views").to_string_lossy().into_owned();
    config.templates.modules_dir = dir.join("modules").to_string_lossy().into_owned();
    config.metrics.enabled = true;
    let server = TestServer::start_with_config(config).await;

    let health: Value = reqwest::get(server.url("/health"))
        .await
        .expect("health request succeeds")
        .json()
        .await
        .expect("health JSON");
    let backend = health["template_backend"]
        .as_str()
        .expect("the template_backend field names the live backend");
    assert!(
        backend == "boa" || backend == "sidecar",
        "unexpected backend: {backend}"
    );

    let body = reqwest::get(server.url("/metrics"))
        .await
        .expect("metrics request succeeds")
        .text()
        .await
        .expect("metrics body");

    // The live backend is 1, the other half of the pair is 0 —
    // whichever way "auto" resolved on this host.
    let other = if backend == "boa" { "sidecar" } else { "boa" };
    assert!(
        body.contains(&format!(
            "wallermax_template_backend{{backend=\"{backend}\"}} 1"
        )),
        "live backend missing in {body}"
    );
    assert!(
        body.contains(&format!(
            "wallermax_template_backend{{backend=\"{other}\"}} 0"
        )),
        "inactive backend must stay at 0 in {body}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Base config without database/auth (plain HTTP stack).
fn wallermax_config() -> wallermax_server::config::AppConfig {
    wallermax_server::config::AppConfig::default()
}
