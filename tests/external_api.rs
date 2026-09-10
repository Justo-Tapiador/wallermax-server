//! End-to-end integration tests for the `[external_api]` proxy.
//!
//! Each test boots the *real* server stack (routes + middleware, exactly
//! as the binary serves it) on an ephemeral port and a **second** tiny
//! axum server plays the external upstream, so the whole chain
//! browser → proxy → upstream → proxy → browser runs over real HTTP.

mod common;

use std::net::SocketAddr;

use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use common::TestServer;
use wallermax_server::config::{AppConfig, ExternalApiConfig, ExternalEndpointConfig};

/// A stub upstream server on an ephemeral port; aborted on drop.
struct Upstream {
    base_url: String,
    abort_handle: tokio::task::AbortHandle,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

impl Upstream {
    async fn spawn() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream binds an ephemeral port");
        let addr: SocketAddr = listener.local_addr().expect("upstream address");

        let app = Router::new()
            .route(
                "/ok",
                get(|| async { Json(json!({"ok": true, "source": "upstream"})) }),
            )
            .route("/headers", get(see_headers))
            .route("/query", get(echo_query))
            .route("/echo", post(echo_body))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    Json(json!({"late": true}))
                }),
            )
            .route(
                "/huge",
                get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "x".repeat(2048)) }),
            )
            .route(
                "/binary",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/octet-stream")],
                        vec![0u8, 1, 2, 3],
                    )
                }),
            )
            .route(
                "/teapot",
                get(|| async { (StatusCode::IM_A_TEAPOT, "short and stout") }),
            );

        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("upstream serves");
        });

        Self {
            base_url: format!("http://{addr}"),
            abort_handle: task.abort_handle(),
        }
    }
}

/// Echoes the headers the proxy actually delivered upstream.
async fn see_headers(headers: HeaderMap) -> Json<Value> {
    let pick = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    Json(json!({
        "x_api_key": pick("x-api-key"),
        "authorization": pick("authorization"),
        "cookie": pick("cookie"),
        "accept": pick("accept"),
        "user_agent": pick("user-agent"),
    }))
}

/// Echoes the query string the proxy appended.
async fn echo_query(uri: Uri) -> Json<Value> {
    Json(json!({"query": uri.query()}))
}

/// Echoes the body and media type the proxy forwarded.
async fn echo_body(headers: HeaderMap, body: String) -> Json<Value> {
    Json(json!({
        "body": body,
        "content_type": headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    }))
}

/// Builds a config with one endpoint pointing at the stub upstream.
fn config_with_endpoint(
    url: String,
    headers: Vec<(&str, &str)>,
    auth_required: bool,
    response_limit_bytes: usize,
) -> AppConfig {
    AppConfig {
        external_api: ExternalApiConfig {
            timeout_secs: 2,
            response_limit_bytes,
            endpoints: vec![ExternalEndpointConfig {
                name: String::from("svc"),
                url,
                auth_required,
                headers: headers
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
            }],
        },
        ..AppConfig::default()
    }
}

#[tokio::test]
async fn forwards_json_answers_with_status_and_media_type() {
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/ok", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/svc"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    // Header reads must happen before the body consumes the response.
    let forwarded_content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("content type forwards")
        .to_owned();
    // The outer middleware still decorates proxied answers.
    let nosniff = response
        .headers()
        .get("x-content-type-options")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let body: Value = response.json().await.expect("JSON body forwards");
    assert_eq!(body["ok"], true);
    assert_eq!(body["source"], "upstream");
    assert!(forwarded_content_type.starts_with("application/json"));
    assert_eq!(nosniff.as_deref(), Some("nosniff"));
}

#[tokio::test]
async fn configured_headers_travel_and_client_headers_do_not() {
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/headers", upstream.base_url),
        vec![
            ("X-Api-Key", "test-key-42"),
            ("Authorization", "Bearer proxy-secret"),
        ],
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    // The client tries to smuggle its own cookie upstream; the proxy
    // must not forward it (header hygiene).
    let response = reqwest::Client::new()
        .get(server.url("/api/ext/svc"))
        .header(header::COOKIE, "wallermax_session=leaked")
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let seen: Value = response.json().await.expect("JSON body forwards");
    assert_eq!(seen["x_api_key"], "test-key-42");
    assert_eq!(seen["authorization"], "Bearer proxy-secret");
    assert_eq!(
        seen["cookie"],
        Value::Null,
        "client cookies must never reach the upstream"
    );
    assert_eq!(seen["accept"], "application/json");
    assert!(
        seen["user_agent"]
            .as_str()
            .expect("user agent is a string")
            .starts_with("wallermax-server/"),
        "the proxy identifies itself: {}",
        seen["user_agent"]
    );
}

#[tokio::test]
async fn incoming_query_string_is_forwarded() {
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/query", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/svc?city=Madrid&units=metric"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("JSON body forwards");
    assert_eq!(body["query"], "city=Madrid&units=metric");
}

#[tokio::test]
async fn post_bodies_and_media_types_forward() {
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/echo", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::Client::new()
        .post(server.url("/api/ext/svc"))
        .json(&json!({"hello": "world"}))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("JSON body forwards");
    // The stub echoes the raw body as a string; parsing it back proves
    // the bytes arrived intact.
    let forwarded: Value =
        serde_json::from_str(body["body"].as_str().expect("body echoes as a string"))
            .expect("forwarded body is valid JSON");
    assert_eq!(forwarded["hello"], "world");
    assert!(body["content_type"]
        .as_str()
        .expect("content type echoes")
        .starts_with("application/json"));
}

#[tokio::test]
async fn unknown_endpoint_name_is_a_json_404() {
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/ok", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/nope"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn upstream_status_codes_pass_through() {
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/teapot", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/svc"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 418);
    assert_eq!(
        response.text().await.expect("text body forwards"),
        "short and stout"
    );
}

#[tokio::test]
async fn oversized_upstream_answers_become_502() {
    let upstream = Upstream::spawn().await;
    // A 64-byte cap against a 2048-byte answer.
    let config = config_with_endpoint(format!("{}/huge", upstream.base_url), Vec::new(), false, 64);
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/svc"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "BAD_GATEWAY");
}

#[tokio::test]
async fn binary_upstream_media_types_become_502() {
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/binary", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/svc"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "BAD_GATEWAY");
}

#[tokio::test]
async fn upstream_timeouts_become_502() {
    let upstream = Upstream::spawn().await;
    let mut config = config_with_endpoint(
        format!("{}/slow", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    // The endpoint answers in 3s; the proxy gives up after 1s.
    config.external_api.timeout_secs = 1;
    let server = TestServer::start_with_config(config).await;

    let started = std::time::Instant::now();
    let response = reqwest::get(server.url("/api/ext/svc"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 502);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "the proxy answered before the upstream did"
    );
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "BAD_GATEWAY");
}

#[tokio::test]
async fn the_family_mounts_only_while_endpoints_exist() {
    let plain = TestServer::start().await;
    let index: Value = reqwest::get(plain.url("/api"))
        .await
        .expect("request succeeds")
        .json()
        .await
        .expect("JSON index");
    let endpoints = index["endpoints"].as_array().expect("endpoints array");
    assert!(
        !endpoints
            .iter()
            .any(|entry| entry.as_str().unwrap().contains("/api/ext/")),
        "no proxy entry without endpoints: {endpoints:?}"
    );

    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/ok", upstream.base_url),
        Vec::new(),
        false,
        262_144,
    );
    let with_endpoint = TestServer::start_with_config(config).await;
    let index: Value = reqwest::get(with_endpoint.url("/api"))
        .await
        .expect("request succeeds")
        .json()
        .await
        .expect("JSON index");
    let endpoints = index["endpoints"].as_array().expect("endpoints array");
    assert!(
        endpoints
            .iter()
            .any(|entry| entry.as_str().unwrap() == "GET/POST /api/ext/{name} (external API proxy)"),
        "the proxy family is listed once configured: {endpoints:?}"
    );
}

#[tokio::test]
async fn env_referenced_secrets_reach_the_upstream() {
    // Unique name: tests run in parallel within one process, and the
    // variable is read at server-build time.
    std::env::set_var("WMS_F6_IT_KEY", "env-injected-secret");
    let upstream = Upstream::spawn().await;
    let config = config_with_endpoint(
        format!("{}/headers", upstream.base_url),
        vec![("X-Api-Key", "${WMS_F6_IT_KEY}")],
        false,
        262_144,
    );
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/svc"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let seen: Value = response.json().await.expect("JSON body forwards");
    assert_eq!(seen["x_api_key"], "env-injected-secret");
}
#[tokio::test]
async fn auth_required_endpoints_reject_anonymous_calls() {
    let upstream = Upstream::spawn().await;
    let (mut config, _db) = common::auth_config();
    config.external_api = ExternalApiConfig {
        timeout_secs: 2,
        response_limit_bytes: 262_144,
        endpoints: vec![ExternalEndpointConfig {
            name: String::from("private"),
            url: format!("{}/ok", upstream.base_url),
            auth_required: true,
            headers: Default::default(),
        }],
    };
    let server = TestServer::start_full(config).await;

    let response = reqwest::get(server.url("/api/ext/private"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn auth_required_endpoints_serve_logged_in_sessions() {
    let upstream = Upstream::spawn().await;
    let (mut config, _db) = common::auth_config();
    config.external_api = ExternalApiConfig {
        timeout_secs: 2,
        response_limit_bytes: 262_144,
        endpoints: vec![ExternalEndpointConfig {
            name: String::from("private"),
            url: format!("{}/ok", upstream.base_url),
            auth_required: true,
            headers: Default::default(),
        }],
    };
    let server = TestServer::start_full(config).await;
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("cookie client builds");

    let username = format!("proxy-user-{}", std::process::id());
    let register = client
        .post(server.url("/api/auth/register"))
        .json(&json!({
            "username": username,
            "password": "a-long-enough-password"
        }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(register.status(), 201);

    let login = client
        .post(server.url("/api/auth/login"))
        .json(&json!({
            "username": username,
            "password": "a-long-enough-password"
        }))
        .send()
        .await
        .expect("login succeeds");
    assert_eq!(login.status(), 200);

    let response = client
        .get(server.url("/api/ext/private"))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("JSON body forwards");
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn text_media_types_forward_even_with_charset_parameters() {
    let app = Router::new().route(
        "/csv",
        get(|| async {
            (
                [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
                "a,b,c\n1,2,3\n",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("extra upstream binds");
    let addr: SocketAddr = listener.local_addr().expect("extra upstream address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("extra upstream serves");
    });

    let config = config_with_endpoint(format!("http://{addr}/csv"), Vec::new(), false, 262_144);
    let server = TestServer::start_with_config(config).await;

    let response = reqwest::get(server.url("/api/ext/svc"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("content type forwards")
        .starts_with("text/csv"));
    assert_eq!(
        response.text().await.expect("text body forwards"),
        "a,b,c\n1,2,3\n"
    );
    task.abort();
}
