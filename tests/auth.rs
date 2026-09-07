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

// ── The browser session cookie (`wallermax_session`) ─────────────────
//
// The cookie mirrors the access token: same verification, same
// endpoints, browsers only.

/// Logs in over JSON and returns the raw response (for `Set-Cookie`).
async fn login_raw(server: &TestServer, username: &str, password: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("login request succeeds")
}

#[tokio::test]
async fn login_sets_the_session_cookie() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "cookie-monster", "password-123").await;

    let response = login_raw(&server, "cookie-monster", "password-123").await;

    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("login sets a session cookie")
        .to_owned();

    assert!(
        cookie.starts_with("wallermax_session="),
        "cookie name: {cookie}"
    );
    assert!(cookie.contains("Path=/"), "cookie path: {cookie}");
    assert!(
        cookie.contains("Max-Age=3600"),
        "cookie mirrors token_ttl_secs: {cookie}"
    );
    assert!(cookie.contains("HttpOnly"), "cookie is HttpOnly: {cookie}");
    // v0.8.0: `Secure` only while TLS is on — the test server speaks
    // plain HTTP, and browsers refuse to store `Secure` cookies on
    // insecure origins (the silent-login-bug of v0.7.0).
    assert!(
        !cookie.contains("Secure"),
        "cookie without Secure on HTTP: {cookie}"
    );
    assert!(
        cookie.contains("SameSite=Strict"),
        "cookie is SameSite=Strict: {cookie}"
    );
}

#[tokio::test]
async fn the_session_cookie_authenticates_api_requests() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "cookie-user", "password-123").await;
    let body = login(&server, "cookie-user", "password-123").await;
    let token = body["access_token"]
        .as_str()
        .expect("access token")
        .to_owned();

    // No Authorization header at all: the cookie alone authenticates.
    let response = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .header("Cookie", format!("wallermax_session={token}"))
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["username"], "cookie-user");
}

#[tokio::test]
async fn invalid_session_cookies_are_rejected() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    let response = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .header("Cookie", "wallermax_session=not.a.jwt")
        .send()
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn the_bearer_header_wins_over_the_session_cookie() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "alice", "password-123").await;
    register(&server, "bob", "password-456").await;
    let alice = login(&server, "alice", "password-123").await;
    let bob = login(&server, "bob", "password-456").await;
    let alice_token = alice["access_token"].as_str().expect("token").to_owned();
    let bob_token = bob["access_token"].as_str().expect("token").to_owned();

    let response = reqwest::Client::new()
        .get(server.url("/api/auth/me"))
        .header("Authorization", format!("Bearer {alice_token}"))
        .header("Cookie", format!("wallermax_session={bob_token}"))
        .send()
        .await
        .expect("request succeeds");

    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["username"], "alice");
}

#[tokio::test]
async fn logout_clears_the_session_cookie() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "bye-user", "password-123").await;
    let body = login(&server, "bye-user", "password-123").await;
    let token = body["access_token"].as_str().expect("token").to_owned();
    let refresh = body["refresh_token"].as_str().expect("refresh").to_owned();

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/logout"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "refresh_token": refresh }))
        .send()
        .await
        .expect("logout succeeds");

    assert_eq!(response.status(), 204);
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("logout clears the cookie");
    assert!(
        cookie.starts_with("wallermax_session=;"),
        "cookie: {cookie}"
    );
    assert!(cookie.contains("Max-Age=0"), "cookie: {cookie}");
}

#[tokio::test]
async fn logout_all_clears_the_session_cookie() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "bye-all", "password-123").await;
    let body = login(&server, "bye-all", "password-123").await;
    let token = body["access_token"].as_str().expect("token").to_owned();

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/logout_all"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("logout_all succeeds");

    assert_eq!(response.status(), 204);
    assert!(response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("logout_all clears the cookie")
        .contains("Max-Age=0"));
}

#[tokio::test]
async fn refresh_rotates_the_session_cookie() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "rotating", "password-123").await;
    let body = login(&server, "rotating", "password-123").await;
    let refresh = body["refresh_token"].as_str().expect("refresh").to_owned();

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/refresh"))
        .json(&json!({ "refresh_token": refresh }))
        .send()
        .await
        .expect("refresh succeeds");

    assert_eq!(response.status(), 200);
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("refresh rotates the session cookie")
        .to_owned();
    let new_body: Value = response.json().await.expect("JSON body");
    let new_token = new_body["access_token"]
        .as_str()
        .expect("new token")
        .to_owned();
    // Tokens issued within the same second can be byte-identical, so
    // the meaningful invariant is the cookie tracking the freshly
    // issued access token (plus its TTL and flags).
    assert!(
        cookie.contains(&format!("wallermax_session={new_token}")),
        "the cookie carries the new token: {cookie}"
    );
    assert!(cookie.contains("Max-Age=3600"), "cookie: {cookie}");
    assert!(cookie.contains("SameSite=Strict"), "cookie: {cookie}");
}

// ── Browser form login (the `/login` view's POST) ────────────────────

/// A client that does NOT follow redirects, so tests observe the `303`
/// itself.
fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds")
}

#[tokio::test]
async fn form_login_redirects_with_the_session_cookie() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "browser-user", "password-123").await;

    let response = no_redirect_client()
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=browser-user&password=password-123&redirect=/perfil")
        .send()
        .await
        .expect("form login succeeds");

    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers()["location"],
        "/perfil",
        "the form's redirect field is honoured"
    );
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("form login sets the cookie");
    assert!(cookie.starts_with("wallermax_session="), "cookie: {cookie}");
    assert!(cookie.contains("SameSite=Strict"), "cookie: {cookie}");
}

#[tokio::test]
async fn form_login_without_a_redirect_field_lands_on_the_site_root() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "root-user", "password-123").await;

    let response = no_redirect_client()
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=root-user&password=password-123")
        .send()
        .await
        .expect("form login succeeds");

    assert_eq!(response.status(), 303);
    assert_eq!(response.headers()["location"], "/");
}

#[tokio::test]
async fn form_login_rejects_off_site_redirect_targets() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "safe-user", "password-123").await;

    for evil in [
        "https://evil.example",
        "//evil.example",
        "evil.example",
        "/\\evil",
    ] {
        let response = no_redirect_client()
            .post(server.url("/api/auth/login"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "username=safe-user&password=password-123&redirect={evil}"
            ))
            .send()
            .await
            .expect("form login succeeds");

        assert_eq!(response.status(), 303, "redirect target: {evil}");
        assert_eq!(
            response.headers()["location"],
            "/",
            "off-site target {evil:?} must fall back to /"
        );
    }
}

#[tokio::test]
async fn form_login_failures_bounce_back_to_the_page() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "real-user", "password-123").await;

    let response = no_redirect_client()
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=real-user&password=wrong-password&redirect=/p/inicio")
        .send()
        .await
        .expect("form login attempt succeeds");

    // v0.8.0: browsers bounce back to the page they came from with
    // `?login_error=credenciales#login`, which re-opens the modal and
    // shows the message. No cookie is set on failure.
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location");
    assert_eq!(location, "/p/inicio?login_error=credenciales#login");
    assert!(
        response.headers().get("set-cookie").is_some(),
        "cookie cleared"
    );

    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("cleared cookie");
    assert!(cookie.contains("Max-Age=0"), "cookie: {cookie}");
}

#[tokio::test]
async fn form_logout_clears_the_session_cookie_without_a_refresh_token() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;
    register(&server, "form-logout", "password-123").await;
    let body = login(&server, "form-logout", "password-123").await;
    let token = body["access_token"].as_str().expect("token").to_owned();

    // A browser form posts no refresh token; the cookie authenticates.
    // v0.8.0: the answer is a 303 back to the redirect field (a browser
    // must never land on a blank 204 page), the cookie is cleared, and
    // an already-gone session clears it just the same. The client keeps
    // redirects manual so the 303 itself is what gets asserted.
    let response = no_redirect_client()
        .post(server.url("/api/auth/logout"))
        .header("Cookie", format!("wallermax_session={token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("redirect=/p")
        .send()
        .await
        .expect("form logout succeeds");

    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location");
    assert_eq!(location, "/p");
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("form logout clears the cookie");
    assert!(cookie.contains("Max-Age=0"), "cookie: {cookie}");
}

#[tokio::test]
async fn form_logout_is_idempotent_without_any_session() {
    let (config, _db) = auth_config();
    let server = TestServer::start_full(config).await;

    let response = no_redirect_client()
        .post(server.url("/api/auth/logout"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .expect("anonymous form logout succeeds");

    // No session, no error: the cookie is cleared and the browser lands
    // on the site root.
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location");
    assert_eq!(location, "/");
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("anonymous logout still clears the cookie");
    assert!(cookie.contains("Max-Age=0"), "cookie: {cookie}");
}
