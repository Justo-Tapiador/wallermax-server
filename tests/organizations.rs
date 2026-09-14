//! The organizations and memberships battery (F15).
//!
//! Boots the full server exactly like the binary (database, auth,
//! templates, static files and the CMS) over a fresh temporary SQLite
//! file, and proves the two authorization planes apart:
//!
//! - **identity + platform role** live in the token (`sub`, `role`) —
//!   what the account *is*;
//! - **access to the CMS panel** is a membership of the CMS
//!   organization (`memberships`) — where the account may go.
//!
//! The happy paths (the bootstrap admin, panel-created editors) run
//! over plain HTTP the way a browser would. The cross-plane cases — a
//! role without a membership, a membership without a role — are
//! seeded straight into the SQLite file with a side connection, the
//! only honest way to produce states the F15 API itself never creates
//! (the repository mirrors role changes into memberships, so over HTTP
//! the two planes never disagree).

mod common;

use common::{auth_config, TestServer};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;
use wallermax_server::config::AppConfig;

/// A configuration with everything the CMS needs, on a fresh database.
fn orgs_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    (config, db)
}

/// A cookie-storing client: `Set-Cookie` in, `Cookie` out — a browser.
/// Redirects stay manual so each hop can be asserted.
fn browser_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .expect("browser client builds")
}

/// Registers the bootstrap admin over the JSON API.
async fn register_admin(server: &TestServer, username: &str, password: &str) {
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(
        response.status(),
        201,
        "the first account becomes the admin"
    );
}

/// Logs a browser in through the **form** endpoint and returns the
/// cookie-bearing client.
async fn login_browser(server: &TestServer, username: &str, password: &str) -> reqwest::Client {
    let client = browser_client();
    let response = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={username}&password={password}&redirect=/"
        ))
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    client
}

/// Grants `role` to a fresh account through the admin panel.
async fn create_user_with_role(
    server: &TestServer,
    admin: &reqwest::Client,
    username: &str,
    role: &str,
) {
    let response = admin
        .post(server.url("/admin/users"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={username}&password=password-123&role={role}"
        ))
        .send()
        .await
        .expect("user creation succeeds");
    assert_eq!(response.status(), 303, "user creation redirects");
}

/// Opens a side connection to the test's SQLite file (the same URL
/// shape the server itself connects with), with the same busy timeout
/// so concurrent server writes degrade gracefully.
async fn side_db(url: &str) -> sqlx::sqlite::SqlitePool {
    let options = SqliteConnectOptions::from_str(url)
        .expect("database url parses")
        .busy_timeout(Duration::from_secs(5));
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("side pool connects")
}

/// The id of `username` in the users table.
async fn user_id(pool: &sqlx::sqlite::SqlitePool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = ?1")
        .bind(username)
        .fetch_one(pool)
        .await
        .expect("user exists")
}

#[tokio::test]
async fn the_bootstrap_admin_owns_the_cms() {
    let (config, _db) = orgs_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // The first account bootstraps as the platform admin, and the
    // repository mirrors that into the CMS organization's
    // administrator membership — the panel opens from the very first
    // login, exactly as before F15.
    let response = admin
        .get(server.url("/admin"))
        .send()
        .await
        .expect("dashboard answers");
    assert_eq!(response.status(), 200, "the bootstrap admin sees the panel");

    let response = admin
        .get(server.url("/admin/users"))
        .send()
        .await
        .expect("user management answers");
    assert_eq!(
        response.status(),
        200,
        "the bootstrap admin manages the CMS accounts"
    );
}

#[tokio::test]
async fn panel_created_editors_manage_content_not_accounts() {
    let (config, _db) = orgs_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "penelope", "editor").await;

    let editor = login_browser(&server, "penelope", "password-123").await;
    let response = editor
        .get(server.url("/admin/pages"))
        .send()
        .await
        .expect("page management answers");
    assert_eq!(
        response.status(),
        200,
        "the editor membership opens content"
    );

    let response = editor
        .get(server.url("/admin/users"))
        .send()
        .await
        .expect("user management answers");
    assert_eq!(
        response.status(),
        403,
        "an editor membership is not an administrator membership"
    );
}

#[tokio::test]
async fn a_role_without_a_membership_cannot_enter() {
    let (config, db) = orgs_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "ghost", "editor").await;

    // Take the membership away by hand, leaving the role (and the
    // token that carries it) untouched — the state the F15 API never
    // produces, and exactly the question the guards must answer with
    // "no": an editor ROLE is not CMS ACCESS.
    let side = side_db(db.url()).await;
    let ghost = user_id(&side, "ghost").await;
    sqlx::query("DELETE FROM memberships WHERE user_id = ?1")
        .bind(ghost)
        .execute(&side)
        .await
        .expect("membership removed");
    side.close().await;

    let ghost_client = login_browser(&server, "ghost", "password-123").await;
    let response = ghost_client
        .get(server.url("/admin/pages"))
        .send()
        .await
        .expect("page management answers");
    assert_eq!(
        response.status(),
        403,
        "the token still says editor; the missing membership says no"
    );
}

#[tokio::test]
async fn a_membership_without_a_role_enters() {
    let (config, db) = orgs_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "plucky", "user").await;

    // The mirror image of the previous test: a plain user whose CMS
    // membership was granted by hand. Membership is what the guards
    // read, so the panel opens — the token's role claim is irrelevant.
    let side = side_db(db.url()).await;
    let plucky = user_id(&side, "plucky").await;
    sqlx::query(
        "INSERT INTO memberships (user_id, organization_id, role, created_at) \
         SELECT ?1, id, 'editor', 0 FROM organizations WHERE key = 'cms'",
    )
    .bind(plucky)
    .execute(&side)
    .await
    .expect("membership granted");
    side.close().await;

    let plucky_client = login_browser(&server, "plucky", "password-123").await;
    let response = plucky_client
        .get(server.url("/admin/pages"))
        .send()
        .await
        .expect("page management answers");
    assert_eq!(
        response.status(),
        200,
        "the token says user; the editor membership opens the panel"
    );
}

#[tokio::test]
async fn demotion_closes_the_door_immediately() {
    let (config, db) = orgs_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "shortlived", "editor").await;

    // Login FIRST: the cookie carries an access token that keeps
    // claiming `editor` until it expires.
    let editor = login_browser(&server, "shortlived", "password-123").await;
    let response = editor
        .get(server.url("/admin/pages"))
        .send()
        .await
        .expect("page management answers");
    assert_eq!(response.status(), 200, "the editor starts inside");

    // Demote through the panel; the repository moves the membership
    // with the role — no token involved.
    let side = side_db(db.url()).await;
    let shortlived = user_id(&side, "shortlived").await;
    side.close().await;
    let response = admin
        .post(server.url(&format!("/admin/users/{shortlived}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("role=user&password=password-456")
        .send()
        .await
        .expect("demotion succeeds");
    assert_eq!(response.status(), 303, "demotion redirects");

    // The SAME cookie, the SAME unexpired editor token — and the door
    // is closed: authorization read the membership, not the token.
    let response = editor
        .get(server.url("/admin/pages"))
        .send()
        .await
        .expect("page management answers");
    assert_eq!(
        response.status(),
        403,
        "the unexpired token no longer opens the panel"
    );
}

#[tokio::test]
async fn deleted_accounts_leave_no_membership_behind() {
    let (config, db) = orgs_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "gone", "editor").await;

    let side = side_db(db.url()).await;
    let gone = user_id(&side, "gone").await;
    side.close().await;

    let response = admin
        .post(server.url(&format!("/admin/users/{gone}/delete")))
        .send()
        .await
        .expect("deletion succeeds");
    assert_eq!(response.status(), 303, "deletion redirects");

    let side = side_db(db.url()).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memberships WHERE user_id = ?1")
        .bind(gone)
        .fetch_one(&side)
        .await
        .expect("memberships counted");
    side.close().await;
    assert_eq!(rows, 0, "the membership went with the account");
}
