//! End-to-end integration tests for refresh tokens: issue, rotation,
//! reuse (family revocation), logout and logout_all.

mod common;

use common::{auth_config, TestServer};
use serde_json::{json, Value};

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

/// Exchanges a refresh token, returning the response.
async fn refresh_raw(server: &TestServer, refresh_token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(server.url("/api/auth/refresh"))
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .expect("refresh request succeeds")
}

/// Exchanges a refresh token, returning the parsed body.
async fn refresh(server: &TestServer, refresh_token: &str) -> Value {
    refresh_raw(server, refresh_token)
        .await
        .json()
        .await
        .expect("refresh body is JSON")
}

#[tokio::test]
async fn login_returns_a_refresh_token() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    let body = login(&server, "alice", "password-123").await;

    let refresh_token = body["refresh_token"].as_str().expect("refresh token");
    assert_eq!(refresh_token.len(), 43);
    assert_eq!(body["refresh_expires_in"].as_i64().expect("ttl"), 2_592_000);
    // Shape: base64url (letters, digits, `-`, `_`).
    assert!(refresh_token
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
}

#[tokio::test]
async fn register_does_not_return_tokens() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    let body = register(&server, "alice", "password-123").await;

    assert!(body.get("refresh_token").is_none());
    assert!(body.get("access_token").is_none());
}

#[tokio::test]
async fn refresh_rotates_tokens_and_keeps_the_user() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    let login_body = login(&server, "alice", "password-123").await;
    let first = login_body["refresh_token"]
        .as_str()
        .expect("token")
        .to_owned();

    let refreshed = refresh(&server, &first).await;

    // New access token works immediately.
    let access = refreshed["access_token"].as_str().expect("access token");
    let me = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .bearer_auth(access)
        .send()
        .await
        .expect("me request succeeds");
    assert_eq!(me.status(), 200);

    // A successor refresh token was issued.
    let second = refreshed["refresh_token"].as_str().expect("successor");
    assert_ne!(second, first);
    assert_eq!(refreshed["user"]["username"], "alice");
}

#[tokio::test]
async fn replaying_a_rotated_token_revokes_the_family() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    let login_body = login(&server, "alice", "password-123").await;
    let first = login_body["refresh_token"]
        .as_str()
        .expect("token")
        .to_owned();

    // Legitimate rotation.
    let refreshed = refresh(&server, &first).await;
    let second = refreshed["refresh_token"]
        .as_str()
        .expect("successor")
        .to_owned();

    // Replaying the retired token must fail...
    let replay = refresh_raw(&server, &first).await;
    assert_eq!(replay.status(), 401);
    let body: Value = replay.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");

    // ...and the family revocation must have killed the successor too.
    let after_replay = refresh_raw(&server, &second).await;
    assert_eq!(after_replay.status(), 401);
}

#[tokio::test]
async fn garbage_and_unknown_tokens_answer_a_generic_401() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;
    login(&server, "alice", "password-123").await;

    for token in ["", "not-a-token", &"x".repeat(43)] {
        let response = refresh_raw(&server, token).await;
        assert_eq!(response.status(), 401, "token: {token:?}");
        let body: Value = response.json().await.expect("JSON envelope");
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
    }

    // Oversized values are rejected as 400 without touching the database.
    let response = refresh_raw(&server, &"x".repeat(600)).await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn expired_refresh_tokens_are_rejected() {
    let (mut config, _db) = auth_config();
    config.auth.refresh_token_ttl_secs = 1;
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    let login_body = login(&server, "alice", "password-123").await;
    let token = login_body["refresh_token"].as_str().expect("token");

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let response = refresh_raw(&server, token).await;
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn logout_revokes_the_token_family() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    let login_body = login(&server, "alice", "password-123").await;
    let access = login_body["access_token"].as_str().expect("access");
    let refresh_token = login_body["refresh_token"].as_str().expect("refresh");

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/logout"))
        .bearer_auth(access)
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .expect("logout succeeds");
    assert_eq!(response.status(), 204);

    // The whole family is dead: neither the token nor a successor can be
    // obtained (refreshing the logged-out token fails).
    let response = refresh_raw(&server, refresh_token).await;
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn logout_requires_authentication_and_is_idempotent() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    // No bearer token: 401, not 204.
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/logout"))
        .json(&json!({ "refresh_token": "anything" }))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 401);

    let login_body = login(&server, "alice", "password-123").await;
    let access = login_body["access_token"].as_str().expect("access");

    // Unknown or foreign tokens still answer 204 (idempotent, silent).
    for token in ["unknown-token", ""] {
        let response = reqwest::Client::new()
            .post(server.url("/api/auth/logout"))
            .bearer_auth(access)
            .json(&json!({ "refresh_token": token }))
            .send()
            .await
            .expect("logout succeeds");
        assert_eq!(response.status(), 204, "token: {token:?}");
    }
}

#[tokio::test]
async fn logout_all_kills_every_session_but_keeps_other_users() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;
    register(&server, "bob", "password-456").await;

    // Two sessions for alice (two logins), one for bob.
    let alice_one = login(&server, "alice", "password-123").await;
    let alice_two = login(&server, "alice", "password-123").await;
    let bob = login(&server, "bob", "password-456").await;

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/logout_all"))
        .bearer_auth(alice_one["access_token"].as_str().expect("access"))
        .send()
        .await
        .expect("logout_all succeeds");
    assert_eq!(response.status(), 204);

    for token in [
        alice_one["refresh_token"].as_str().expect("token"),
        alice_two["refresh_token"].as_str().expect("token"),
    ] {
        let response = refresh_raw(&server, token).await;
        assert_eq!(response.status(), 401);
    }

    // Bob's session is untouched.
    let response = refresh_raw(&server, bob["refresh_token"].as_str().expect("token")).await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn refresh_tokens_can_be_disabled() {
    let (mut config, _db) = auth_config();
    config.auth.refresh_tokens_enabled = false;
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    let login_body = login(&server, "alice", "password-123").await;
    assert!(login_body.get("refresh_token").is_none());
    assert!(login_body.get("refresh_expires_in").is_none());

    // The endpoints are unmounted: standard JSON 404.
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/refresh"))
        .json(&json!({ "refresh_token": "whatever" }))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("JSON envelope");
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/logout_all"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn refresh_maintains_the_session_across_multiple_rotations() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;

    let mut token = login(&server, "alice", "password-123").await;

    for _ in 0..3 {
        let response = refresh_raw(&server, token["refresh_token"].as_str().expect("token")).await;
        assert_eq!(response.status(), 200);
        token = response.json().await.expect("JSON body");
    }

    // The last access token in the chain still authenticates.
    let me = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .bearer_auth(token["access_token"].as_str().expect("access"))
        .send()
        .await
        .expect("me request succeeds");
    assert_eq!(me.status(), 200);
    let body: Value = me.json().await.expect("JSON body");
    assert_eq!(body["username"], "alice");
}

#[tokio::test]
async fn the_index_lists_the_refresh_endpoints_while_enabled() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    let index: Value = reqwest::get(server.url("/api"))
        .await
        .expect("ok")
        .json()
        .await
        .expect("JSON");
    let endpoints = index["endpoints"].as_array().expect("endpoints array");

    for expected in [
        "POST /api/auth/refresh",
        "POST /api/auth/logout",
        "POST /api/auth/logout_all",
    ] {
        assert!(
            endpoints.iter().any(|endpoint| endpoint == expected),
            "missing {expected} in {endpoints:?}"
        );
    }
}
