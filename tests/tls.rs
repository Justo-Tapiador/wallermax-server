//! End-to-end integration tests for TLS serving and the HTTP -> HTTPS
//! redirect listener.
//!
//! The TLS test server boots the full application over HTTPS with a
//! freshly generated self-signed certificate (see `common::TlsTestServer`),
//! mirroring the production `axum-server` + rustls path.

mod common;

use std::net::SocketAddr;

use common::{auth_config, tls_test_client, TlsTestServer};
use serde_json::{json, Value};

#[tokio::test]
async fn health_answers_over_https() {
    let (config, _db) = auth_config();
    let server = TlsTestServer::start_full(config).await;

    let response = tls_test_client()
        .get(server.url("/health"))
        .send()
        .await
        .expect("https request succeeds");

    assert_eq!(response.status(), 200);
    // Capture headers before `.json()` consumes the response.
    let content_type_options = response
        .headers()
        .get("x-content-type-options")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["status"], "ok");

    // The middleware pipeline applies over TLS as well.
    assert_eq!(content_type_options.as_deref(), Some("nosniff"));
}

#[tokio::test]
async fn the_full_auth_flow_works_over_https() {
    let (config, _db) = auth_config();
    let server = TlsTestServer::start_full(config).await;
    let client = tls_test_client();

    let registered = client
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "alice", "password": "password-123" }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(registered.status(), 201);

    let login: Value = client
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "alice", "password": "password-123" }))
        .send()
        .await
        .expect("login succeeds")
        .json()
        .await
        .expect("JSON body");
    assert!(login["access_token"].is_string());

    let me = client
        .get(server.url("/api/auth/me"))
        .bearer_auth(login["access_token"].as_str().expect("token"))
        .send()
        .await
        .expect("me succeeds");
    assert_eq!(me.status(), 200);
}

#[tokio::test]
async fn https_serves_the_json_error_envelope() {
    let (config, _db) = auth_config();
    let server = TlsTestServer::start_full(config).await;

    let response = tls_test_client()
        .get(server.url("/no-such-path"))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    assert!(body["error"]["request_id"].is_string());
}

#[tokio::test]
async fn untrusted_certificates_are_rejected_by_strict_clients() {
    let (config, _db) = auth_config();
    let server = TlsTestServer::start_full(config).await;

    // A strict rustls client (no danger flags) must refuse the
    // self-signed certificate: the handshake never completes.
    let strict = reqwest::Client::builder()
        .use_rustls_tls()
        .build()
        .expect("strict client builds");

    let result = strict.get(server.url("/health")).send().await;
    assert!(result.is_err(), "self-signed certificate must be rejected");
}

#[tokio::test]
async fn the_http_listener_redirects_to_the_https_port() {
    // Run the production redirect router on its own ephemeral port,
    // pointed at a TLS instance.
    let (config, _db) = auth_config();
    let tls_server = TlsTestServer::start_full(config).await;

    // The TLS address is embedded in the server's base URL.
    let tls_addr: SocketAddr = tls_server
        .url("/health")
        .trim_start_matches("https://")
        .trim_end_matches("/health")
        .parse()
        .expect("tls address parses");

    let redirect = wallermax_server::server::redirect_router(tls_addr);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral redirect port binds");
    let http_addr: SocketAddr = listener.local_addr().expect("local address");
    let redirect_task = tokio::spawn(async move {
        axum::serve(listener, redirect.into_make_service())
            .await
            .expect("redirect server runs");
    });

    // Any path and any method redirect, preserving the path and query.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds");

    let response = client
        .get(format!("http://{http_addr}/api/stats?x=1"))
        .send()
        .await
        .expect("plain http request succeeds");
    assert_eq!(response.status(), 308);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some(format!("https://127.0.0.1:{}/api/stats?x=1", tls_addr.port()).as_str())
    );

    // POST is preserved as well (308 semantics).
    let response = client
        .post(format!("http://{http_addr}/api/auth/login"))
        .send()
        .await
        .expect("plain http request succeeds");
    assert_eq!(response.status(), 308);
    assert!(response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|location| location.starts_with("https://")));

    // Following the redirect with the TLS test client lands on the real
    // endpoint.
    let response = tls_test_client()
        .get(format!("http://{http_addr}/health"))
        .send()
        .await
        .expect("redirect followed");
    assert_eq!(response.status(), 200);

    redirect_task.abort();
}

#[tokio::test]
async fn metrics_are_served_over_https() {
    let (mut config, _db) = auth_config();
    config.metrics.enabled = true;
    let server = TlsTestServer::start_full(config).await;

    // Warm up a counter child.
    let _ = tls_test_client()
        .get(server.url("/health"))
        .send()
        .await
        .expect("health succeeds");

    let response = tls_test_client()
        .get(server.url("/metrics"))
        .send()
        .await
        .expect("metrics succeeds");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("metrics body");
    assert!(body.contains("wallermax_requests_total"));
}
