//! End-to-end integration tests for the core HTTP API.
//!
//! Each test boots the *real* server stack (routes + middleware, exactly as
//! the binary serves it) on an ephemeral port and talks to it over plain
//! HTTP with `reqwest`.

mod common;

use common::TestServer;
use serde_json::Value;

#[tokio::test]
async fn root_returns_service_index() {
    let server = TestServer::start().await;
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
        .starts_with("application/json"));

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["service"], "wallermax-server");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    let endpoints = body["endpoints"].as_array().expect("endpoints array");
    assert!(endpoints.iter().any(|e| e == "GET /health"));
    assert!(endpoints.iter().any(|e| e == "GET /api/stats"));
    assert!(endpoints.iter().any(|e| e == "POST /api/echo"));
}

#[tokio::test]
async fn health_reports_ok() {
    let server = TestServer::start().await;
    let response = reqwest::get(server.url("/health"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn stats_expose_runtime_metrics() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let first: Value = client
        .get(server.url("/api/stats"))
        .send()
        .await
        .expect("first request succeeds")
        .json()
        .await
        .expect("JSON body");
    let second: Value = client
        .get(server.url("/api/stats"))
        .send()
        .await
        .expect("second request succeeds")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(second["service"], "wallermax-server");
    assert!(
        first["total_requests"].as_u64().expect("counter") >= 1,
        "the first stats request itself is counted"
    );
    assert!(
        second["total_requests"].as_u64().expect("counter")
            > first["total_requests"].as_u64().expect("counter")
    );
    assert!(second["uptime_seconds"].as_f64().expect("uptime") >= 0.0);
    assert!(second["requests_per_second"].as_f64().expect("rate") >= 0.0);
    assert_eq!(second["rate_limited_requests"], 0);
}

#[tokio::test]
async fn echo_returns_received_body() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let body: Value = client
        .post(server.url("/api/echo"))
        .header("Content-Type", "text/plain")
        .body("hello wallermax")
        .send()
        .await
        .expect("request succeeds")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(body["received_bytes"], 15);
    assert_eq!(body["body"], "hello wallermax");
    assert_eq!(body["content_type"], "text/plain");
}

#[tokio::test]
async fn unknown_routes_return_structured_json_404() {
    let server = TestServer::start().await;
    let response = reqwest::get(server.url("/definitely-not-here"))
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
        .contains("GET /definitely-not-here"));
    assert!(!body["error"]["request_id"]
        .as_str()
        .unwrap_or("")
        .is_empty());
}

#[tokio::test]
async fn unsupported_methods_return_structured_json_405() {
    let server = TestServer::start().await;
    let response = reqwest::Client::new()
        .post(server.url("/health"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 405);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "METHOD_NOT_ALLOWED");
    let message = body["error"]["message"].as_str().expect("message string");
    assert!(message.contains("POST"));
    assert!(message.contains("/health"));
}

#[tokio::test]
async fn security_headers_are_present_on_every_response() {
    let server = TestServer::start().await;

    for path in ["/", "/health", "/api/stats", "/missing"] {
        let response = reqwest::get(server.url(path))
            .await
            .expect("request succeeds");
        let headers = response.headers();

        for (name, expected) in [
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            ("referrer-policy", "no-referrer"),
            (
                "content-security-policy",
                "default-src 'none'; frame-ancestors 'none'",
            ),
        ] {
            let actual = headers
                .get(name)
                .unwrap_or_else(|| panic!("{name} header missing for {path}"))
                .to_str()
                .expect("ascii value");
            assert_eq!(actual, expected, "{name} mismatch for {path}");
        }

        assert!(
            headers
                .get("strict-transport-security")
                .expect("HSTS header")
                .to_str()
                .expect("ascii value")
                .starts_with("max-age="),
            "HSTS header missing for {path}"
        );
    }
}

#[tokio::test]
async fn request_ids_are_generated_per_request() {
    let server = TestServer::start().await;

    let first = reqwest::get(server.url("/health")).await.expect("request");
    let second = reqwest::get(server.url("/health")).await.expect("request");

    let first_id = first
        .headers()
        .get("x-request-id")
        .expect("request id header")
        .to_str()
        .expect("ascii value")
        .to_owned();
    let second_id = second
        .headers()
        .get("x-request-id")
        .expect("request id header")
        .to_str()
        .expect("ascii value")
        .to_owned();

    assert!(!first_id.is_empty());
    assert!(!second_id.is_empty());
    assert_ne!(first_id, second_id, "each request gets a fresh id");
}

#[tokio::test]
async fn client_supplied_request_id_is_honored() {
    let server = TestServer::start().await;

    let response = reqwest::Client::new()
        .get(server.url("/"))
        .header("X-Request-Id", "integration-test-42")
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .expect("request id header")
            .to_str()
            .expect("ascii value"),
        "integration-test-42"
    );
}

#[tokio::test]
async fn response_time_header_is_present() {
    let server = TestServer::start().await;

    let response = reqwest::get(server.url("/health")).await.expect("request");

    let value = response
        .headers()
        .get("x-response-time")
        .expect("response time header")
        .to_str()
        .expect("ascii value");
    assert!(value.parse::<f64>().expect("milliseconds value") >= 0.0);
}

#[tokio::test]
async fn middleware_can_be_disabled_via_config() {
    let mut config = wallermax_server::config::AppConfig::default();
    config.middleware.security_headers = false;
    config.middleware.request_id = false;
    config.middleware.logging = false;

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::get(server.url("/health"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    assert!(response.headers().get("x-content-type-options").is_none());
    assert!(response.headers().get("x-request-id").is_none());
    assert!(response.headers().get("x-response-time").is_none());
}
