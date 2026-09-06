//! Phase 2 security integration tests.
//!
//! Boots the real server stack (routes + middleware) with tailored
//! configurations and asserts on real HTTP behaviour: rate limiting,
//! CORS, request body limits, timeouts, configurable security headers
//! and the strict request-id policy.

mod common;

use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use common::TestServer;
use serde_json::Value;
use wallermax_server::config::AppConfig;
use wallermax_server::state::AppState;

/// A configuration with rate limiting enabled and tuned for tests.
fn rate_limited_config(capacity: u64, refill_per_second: f64) -> AppConfig {
    let mut config = AppConfig::default();
    config.middleware.rate_limit = true;
    config.rate_limit.capacity = capacity;
    config.rate_limit.refill_per_second = refill_per_second;
    config
}

#[tokio::test]
async fn rate_limit_allows_bursts_up_to_capacity() {
    // Essentially no refill: the burst budget is all there is.
    let server = TestServer::start_with_config(rate_limited_config(3, 0.001)).await;
    let client = reqwest::Client::new();

    for expected_remaining in ["2", "1", "0"] {
        let response = client
            .get(server.url("/health"))
            .send()
            .await
            .expect("request succeeds");

        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("x-ratelimit-limit")
                .and_then(|v| v.to_str().ok()),
            Some("3"),
            "capacity is advertised"
        );
        assert_eq!(
            response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok()),
            Some(expected_remaining),
            "remaining tokens decrease"
        );
    }
}

#[tokio::test]
async fn rate_limit_rejects_above_capacity_with_json_429() {
    let server = TestServer::start_with_config(rate_limited_config(3, 0.001)).await;
    let client = reqwest::Client::new();

    for _ in 0..3 {
        let response = client
            .get(server.url("/health"))
            .send()
            .await
            .expect("request succeeds");
        assert_eq!(response.status(), 200);
    }

    let rejected = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(rejected.status(), 429);
    assert!(
        rejected
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|secs| secs >= 1),
        "Retry-After advertises a positive wait"
    );
    assert_eq!(
        rejected
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("0")
    );
    assert!(
        rejected
            .headers()
            .get("x-request-id")
            .is_some_and(|v| !v.to_str().unwrap_or("").is_empty()),
        "rejected responses still carry a request id"
    );
    // Security headers must survive on rejections too.
    assert_eq!(
        rejected
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );

    let body: Value = rejected.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "RATE_LIMITED");
    assert!(!body["error"]["request_id"]
        .as_str()
        .unwrap_or("")
        .is_empty());
}

#[tokio::test]
async fn rate_limit_recovers_after_refill() {
    // Capacity 1, fast refill: 20 tokens per second.
    let server = TestServer::start_with_config(rate_limited_config(1, 20.0)).await;
    let client = reqwest::Client::new();

    let first = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("first request succeeds");
    assert_eq!(first.status(), 200);

    let second = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("second request succeeds");
    assert_eq!(second.status(), 429);

    // 150ms at 20 tokens/s regenerate 3 tokens; comfortable margin.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let third = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("third request succeeds");
    assert_eq!(third.status(), 200);
}

#[tokio::test]
async fn rate_limit_counts_rejections_in_stats() {
    // Capacity 1 with a fast refill: the first request consumes the token,
    // the second is rejected, and after a short wait the stats request
    // itself is allowed through.
    let server = TestServer::start_with_config(rate_limited_config(1, 20.0)).await;
    let client = reqwest::Client::new();

    let _allowed = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("request succeeds");
    let rejected = client
        .get(server.url("/health"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(rejected.status(), 429);

    // 150ms at 20 tokens/s regenerate plenty of budget for the stats call.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let stats: Value = client
        .get(server.url("/api/stats"))
        .send()
        .await
        .expect("request succeeds")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(stats["rate_limited_requests"], 1);
    assert!(
        stats["total_requests"].as_u64().expect("counter") >= 3,
        "rejected requests are counted as arrivals too"
    );
}

#[tokio::test]
async fn rate_limit_disabled_keeps_responses_header_free() {
    let server = TestServer::start().await;

    let response = reqwest::get(server.url("/health"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    assert!(response.headers().get("x-ratelimit-limit").is_none());
    assert!(response.headers().get("x-ratelimit-remaining").is_none());
}

#[tokio::test]
async fn cors_allowed_origin_is_reflected() {
    let mut config = AppConfig::default();
    config.middleware.cors = true;
    config.cors.allowed_origins = vec![
        String::from("https://app.example.com"),
        String::from("http://localhost:3000"),
    ];

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::Client::new()
        .get(server.url("/health"))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
}

#[tokio::test]
async fn cors_disallowed_origin_gets_no_allow_header() {
    let mut config = AppConfig::default();
    config.middleware.cors = true;
    config.cors.allowed_origins = vec![String::from("https://app.example.com")];

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::Client::new()
        .get(server.url("/health"))
        .header("Origin", "https://evil.example.net")
        .send()
        .await
        .expect("request succeeds");

    // CORS is browser-enforced: the request itself still succeeds.
    assert_eq!(response.status(), 200);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "no CORS grant for origins outside the allowlist"
    );
}

#[tokio::test]
async fn cors_preflight_is_answered_directly() {
    let mut config = AppConfig::default();
    config.middleware.cors = true;
    config.cors.allowed_origins = vec![String::from("https://app.example.com")];

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, server.url("/api/echo"))
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .expect("preflight succeeds");

    assert!(response.status().is_success());
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    assert!(
        response
            .headers()
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_uppercase().contains("POST")),
        "POST is part of the allowed methods"
    );
}

#[tokio::test]
async fn cors_wildcard_allows_any_origin() {
    let mut config = AppConfig::default();
    config.middleware.cors = true;
    config.cors.allowed_origins = vec![String::from("*")];

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::Client::new()
        .get(server.url("/health"))
        .header("Origin", "https://anything.example.net")
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}

#[tokio::test]
async fn cors_disabled_adds_no_headers() {
    let server = TestServer::start().await;
    let response = reqwest::Client::new()
        .get(server.url("/health"))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
}

#[tokio::test]
async fn body_limit_rejects_oversized_bodies_with_json_413() {
    let mut config = AppConfig::default();
    config.server.max_body_size_bytes = 32;

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::Client::new()
        .post(server.url("/api/echo"))
        .body("x".repeat(100))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 413);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "PAYLOAD_TOO_LARGE");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message string")
        .contains("100"));
    assert!(!body["error"]["request_id"]
        .as_str()
        .unwrap_or("")
        .is_empty());
}

#[tokio::test]
async fn body_limit_allows_bodies_within_the_limit() {
    let mut config = AppConfig::default();
    config.server.max_body_size_bytes = 1024;

    let server = TestServer::start_with_config(config).await;
    let body: Value = reqwest::Client::new()
        .post(server.url("/api/echo"))
        .json(&serde_json::json!({ "ping": "pong" }))
        .send()
        .await
        .expect("request succeeds")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(body["received_bytes"], 15);
    // The echo endpoint returns the raw body as a string.
    let echoed: Value = serde_json::from_str(body["body"].as_str().expect("raw body string"))
        .expect("echoed body is the same JSON");
    assert_eq!(echoed["ping"], "pong");
}

#[tokio::test]
async fn timeout_returns_json_408() {
    let mut config = AppConfig::default();
    config.server.request_timeout_secs = 1;

    // A route whose handler sleeps far beyond the request budget.
    let router: Router<AppState> = Router::new().route(
        "/api/slow",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Json(serde_json::json!({ "late": true }))
        }),
    );

    let server = TestServer::start_with_router(config, router).await;
    let response = reqwest::Client::new()
        .post(server.url("/api/slow"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 408);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "REQUEST_TIMEOUT");
    assert!(!body["error"]["request_id"]
        .as_str()
        .unwrap_or("")
        .is_empty());
}

#[tokio::test]
async fn request_id_overwrite_mode_ignores_client_values() {
    let mut config = AppConfig::default();
    config.request_id.mode = wallermax_server::config::RequestIdMode::Overwrite;

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::Client::new()
        .get(server.url("/health"))
        .header("X-Request-Id", "client-supplied-id")
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let echoed = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("a generated id is echoed");
    assert_ne!(echoed, "client-supplied-id");
    assert!(!echoed.is_empty());
}

#[tokio::test]
async fn security_headers_are_configurable() {
    let mut config = AppConfig::default();
    config.security_headers.x_frame_options = String::from("SAMEORIGIN");
    config.security_headers.referrer_policy = String::new();

    let server = TestServer::start_with_config(config).await;
    let response = reqwest::get(server.url("/health"))
        .await
        .expect("request succeeds");

    let headers = response.headers();
    assert_eq!(
        headers.get("x-frame-options").and_then(|v| v.to_str().ok()),
        Some("SAMEORIGIN")
    );
    assert!(
        headers.get("referrer-policy").is_none(),
        "empty values omit the header"
    );
    // Untouched headers keep their defaults.
    assert_eq!(
        headers
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
}
