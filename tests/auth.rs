//! End-to-end integration tests for authentication and persistence.
//!
//! Each test boots the *full* application (database pool, embedded
//! migrations, auth services) on an ephemeral port, backed by a fresh
//! temporary SQLite file, and exercises the HTTP API with `reqwest`.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::{auth_config, TestServer};
use serde_json::{json, Value};
use wallermax_server::auth::JwtService;
use wallermax_server::db::{User, UserRole};

/// Registers a user and returns the response body.
async fn register(server: &TestServer, username: &str, password: &str) -> Value {
    reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("registration request succeeds")
        .json()
        .await
        .expect("registration body is JSON")
}

/// Logs in and returns the parsed response body.
async fn login(server: &TestServer, username: &str, password: &str) -> Value {
    reqwest::Client::new()
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("login request succeeds")
        .json()
        .await
        .expect("login body is JSON")
}

#[tokio::test]
async fn registration_bootstraps_the_first_user_as_admin() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "root-admin", "password": "sup3r-secret!" }))
        .send()
        .await
        .expect("registration succeeds");

    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["username"], "root-admin");
    assert_eq!(body["role"], "admin");
    assert!(body["id"].as_i64().expect("user id") > 0);
    // The password hash never leaks into API responses.
    assert!(body.get("password_hash").is_none());
}

#[tokio::test]
async fn later_users_get_the_regular_role() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    register(&server, "first", "password-123").await;
    let second = register(&server, "second", "password-456").await;

    assert_eq!(second["role"], "user");
}

#[tokio::test]
async fn duplicate_usernames_conflict_case_insensitively() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    register(&server, "Alice", "password-123").await;

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "alice", "password": "password-456" }))
        .send()
        .await
        .expect("registration succeeds");

    assert_eq!(response.status(), 409);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "CONFLICT");
}

#[tokio::test]
async fn invalid_credentials_input_is_rejected() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    let client = reqwest::Client::new();

    for (username, password) in [
        ("ab", "password-123"),         // username too short
        ("has spaces", "password-123"), // username charset
        ("valid-user", "short"),        // password too short
    ] {
        let response = client
            .post(server.url("/api/auth/register"))
            .json(&json!({ "username": username, "password": password }))
            .send()
            .await
            .expect("registration succeeds");
        assert_eq!(
            response.status(),
            400,
            "expected 400 for {username:?}/{password:?}"
        );
    }

    // Malformed JSON bodies answer with the JSON envelope, not plain text.
    let response = client
        .post(server.url("/api/auth/register"))
        .header("Content-Type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn login_returns_a_usable_bearer_token() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    register(&server, "alice", "correct-password").await;
    let body = login(&server, "alice", "correct-password").await;

    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], 3600);
    assert_eq!(body["user"]["username"], "alice");
    let token = body["access_token"]
        .as_str()
        .expect("access token string")
        .to_owned();
    assert!(!token.is_empty());

    // The token immediately grants access to the profile endpoint.
    let profile = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("profile request succeeds");
    assert_eq!(profile.status(), 200);
    let profile: Value = profile.json().await.expect("JSON body");
    assert_eq!(profile["username"], "alice");
    // The login stamp is visible on the profile.
    assert!(profile["last_login_at"].as_i64().is_some());
}

#[tokio::test]
async fn login_failures_are_generic_and_identical() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    let client = reqwest::Client::new();

    register(&server, "alice", "correct-password").await;

    let wrong_password = client
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "alice", "password": "wrong-password" }))
        .send()
        .await
        .expect("request succeeds");
    let unknown_user = client
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "ghost", "password": "whatever-123" }))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(wrong_password.status(), 401);
    assert_eq!(unknown_user.status(), 401);

    let wrong_password: Value = wrong_password.json().await.expect("JSON body");
    let unknown_user: Value = unknown_user.json().await.expect("JSON body");
    // Identical code and message: no user enumeration.
    assert_eq!(wrong_password["error"]["code"], "UNAUTHORIZED");
    assert_eq!(
        wrong_password["error"]["message"],
        unknown_user["error"]["message"]
    );
}

#[tokio::test]
async fn profile_requires_a_valid_token() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    let client = reqwest::Client::new();

    let missing = client
        .get(server.url("/api/auth/me"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(missing.status(), 401);
    let body: Value = missing.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");

    let garbage = client
        .get(server.url("/api/auth/me"))
        .header("Authorization", "Bearer not-a-real-token")
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(garbage.status(), 401);

    let wrong_scheme = client
        .get(server.url("/api/auth/me"))
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(wrong_scheme.status(), 401);
}

#[tokio::test]
async fn expired_tokens_are_rejected() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config.clone()).await;

    register(&server, "alice", "correct-password").await;

    // Forge a token that expired well before the verification leeway.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock moves forward")
        .as_secs();
    let jwt = JwtService::new(&config.auth.jwt_secret, &config.auth.issuer, 10);
    let user = User {
        id: 1,
        username: String::from("alice"),
        password_hash: String::new(),
        role: UserRole::Admin,
        created_at: 0,
        last_login_at: None,
    };
    let stale = jwt
        .issue_token_at(&user, now - 100)
        .expect("stale token signs");

    let response = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .bearer_auth(stale)
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn admin_listing_requires_the_admin_role() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    let client = reqwest::Client::new();

    register(&server, "admin-user", "password-123").await;
    register(&server, "regular-user", "password-456").await;

    let regular = login(&server, "regular-user", "password-456").await;
    let token = regular["access_token"].as_str().expect("token").to_owned();

    let forbidden = client
        .get(server.url("/api/admin/users"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(forbidden.status(), 403);
    let body: Value = forbidden.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "FORBIDDEN");

    let unauthenticated = client
        .get(server.url("/api/admin/users"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(unauthenticated.status(), 401);
}

#[tokio::test]
async fn admins_can_list_users() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    let client = reqwest::Client::new();

    register(&server, "admin-user", "password-123").await;
    register(&server, "regular-user", "password-456").await;

    let admin = login(&server, "admin-user", "password-123").await;
    let token = admin["access_token"].as_str().expect("token").to_owned();

    let response = client
        .get(server.url("/api/admin/users"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["total"], 2);
    let users = body["users"].as_array().expect("users array");
    assert_eq!(users.len(), 2);
    // Newest first, and no password hashes anywhere.
    assert_eq!(users[0]["username"], "regular-user");
    assert!(users.iter().all(|user| user.get("password_hash").is_none()));
}

#[tokio::test]
async fn auth_routes_are_absent_when_disabled() {
    // Default config: database and auth disabled.
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let response = client
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "x", "password": "y" }))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 404);

    let response = client
        .get(server.url("/api/admin/users"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 404);

    // The service index reflects the absence.
    let index: Value = reqwest::get(server.url("/"))
        .await
        .expect("ok")
        .json()
        .await
        .expect("JSON");
    let endpoints = index["endpoints"].as_array().expect("endpoints array");
    assert!(!endpoints.iter().any(|endpoint| endpoint
        .as_str()
        .expect("str")
        .starts_with("POST /api/auth")));
}

#[tokio::test]
async fn auth_routes_are_listed_in_the_index_when_enabled() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    let index: Value = reqwest::get(server.url("/"))
        .await
        .expect("ok")
        .json()
        .await
        .expect("JSON");
    let endpoints = index["endpoints"].as_array().expect("endpoints array");

    for expected in [
        "POST /api/auth/register",
        "POST /api/auth/login",
        "GET /api/auth/me",
        "GET /api/admin/users",
    ] {
        assert!(
            endpoints.iter().any(|endpoint| endpoint == expected),
            "missing {expected} in {endpoints:?}"
        );
    }
}

#[tokio::test]
async fn registration_can_be_disabled() {
    let (mut config, _db) = auth_config();
    config.auth.registration_enabled = false;
    let server = TestServer::start_full(config).await;

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "newcomer", "password": "password-123" }))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 403);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "FORBIDDEN");
}

#[tokio::test]
async fn stats_include_registered_users_when_auth_is_enabled() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    register(&server, "alice", "password-123").await;
    register(&server, "bob", "password-456").await;

    let stats: Value = reqwest::get(server.url("/api/stats"))
        .await
        .expect("request succeeds")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(stats["registered_users"], 2);
}

#[tokio::test]
async fn stats_omit_registered_users_when_auth_is_disabled() {
    let server = TestServer::start().await;

    let stats: Value = reqwest::get(server.url("/api/stats"))
        .await
        .expect("request succeeds")
        .json()
        .await
        .expect("JSON body");

    assert!(stats.get("registered_users").is_none());
}

#[tokio::test]
async fn auth_responses_carry_the_standard_headers_and_envelope() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    let response = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .send()
        .await
        .expect("request succeeds");

    // Security headers and the request id are on auth responses too.
    assert!(response.headers().contains_key("x-content-type-options"));
    assert!(response.headers().contains_key("x-request-id"));

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
    assert!(!body["error"]["request_id"]
        .as_str()
        .unwrap_or("")
        .is_empty());
}

#[tokio::test]
async fn data_survives_restarts() {
    // Persistence is the point of Phase 3: register on one server, then
    // boot a brand-new one against the same file and log in.
    let (config, db) = auth_config();
    let first = TestServer::start_full(config.clone()).await;
    register(&first, "persistent", "password-123").await;
    drop(first);

    let second = TestServer::start_full(config).await;
    let body = login(&second, "persistent", "password-123").await;

    assert_eq!(body["user"]["username"], "persistent");
    let _ = db; // guard keeps cleanup until the end of the test
}
