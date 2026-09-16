//! The Team page (F21): the tenant's own administrators run the
//! tenant's own team, from the tenant's own panel.
//!
//! Boots the full server twice — once to seed the bootstrap
//! organizations, then again with the tenant organizations and their
//! domains inserted straight into the database — and exercises the
//! page with `Host`-pinned, cookie-storing clients:
//!
//! - the page answers an `admin` membership of the request's
//!   organization, and nobody else: not the tenant's editors, not
//!   the platform administrator (F20's explicit-membership rule,
//!   applied to the people surface), not a member of another tenant;
//! - the add-or-move form grants and changes memberships, and a
//!   removed membership locks the panel on the very next request
//!   (the guards read the table, never the token);
//! - **an organization never loses its last administrator** — the
//!   tenant's own Team page refuses the final demotion and removal,
//!   the platform's Tenants page answers the same refusal, and
//!   deleting the account itself is refused in the Users page until
//!   the team has another administrator;
//! - the page is a tenant surface: the CMS host answers it with the
//!   standard 404 (it is not mounted there at all), and the CMS
//!   organization's team stays where it always was — the platform's
//!   Users page, behind the F15 mirror.

mod common;

use std::time::Duration;

use reqwest::header::{HeaderValue, HOST, LOCATION};
use wallermax_server::config::AppConfig;

use common::{auth_config, TestServer};

// ─── Harness ────────────────────────────────────────────────────────

/// The CMS host and the two tenant hosts these tests pin.
const CMS_HOST: &str = "cms.f21.test";
const TENANT_HOST: &str = "shop.f21.test";
const OTHER_HOST: &str = "press.f21.test";

/// The full stack over `url`, with `[cms] hosts` naming the CMS host.
fn team_config(url: &str) -> AppConfig {
    let mut config = AppConfig::default();
    config.database.enabled = true;
    config.database.url = url.to_owned();
    config.database.max_connections = 2;
    config.auth.enabled = true;
    config.auth.jwt_secret = String::from("integration-test-secret-0123456789abcdef0123");
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    config.cms.hosts = vec![CMS_HOST.to_owned()];
    config
}

/// A cookie-storing client: `Set-Cookie` in, `Cookie` out — a browser
/// that never follows redirects (each hop is asserted by hand).
fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .expect("browser client builds")
}

/// GETs `path` pinned to `host` with `client`.
async fn get(
    client: &reqwest::Client,
    server: &TestServer,
    path: &str,
    host: &str,
) -> reqwest::Response {
    client
        .get(server.url(path))
        .header(HOST, HeaderValue::from_str(host).expect("test host value"))
        .send()
        .await
        .expect("request")
}

/// POSTs a urlencoded `body` to `path` pinned to `host` with `client`.
async fn post(
    client: &reqwest::Client,
    server: &TestServer,
    path: &str,
    host: &str,
    body: &str,
) -> reqwest::Response {
    client
        .post(server.url(path))
        .header(HOST, HeaderValue::from_str(host).expect("test host value"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.to_owned())
        .send()
        .await
        .expect("request")
}

/// Opens a side connection to the test's SQLite file, with the same
/// busy timeout the server itself uses.
async fn side_db(url: &str) -> sqlx::sqlite::SqlitePool {
    use std::str::FromStr as _;
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(url)
        .expect("database url parses")
        .busy_timeout(Duration::from_secs(5));
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("side pool connects")
}

/// A throwaway directory standing in for a tenant's site root,
/// removed (best-effort) on drop.
struct TenantSite {
    root: std::path::PathBuf,
}

impl TenantSite {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "wallermax-f21-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("tenant root created");
        Self { root }
    }

    fn root_string(&self) -> String {
        self.root.display().to_string().replace('\\', "/")
    }
}

impl Drop for TenantSite {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Boots once (seeding the organizations), inserts one tenant
/// organization and its domain, then reboots.
async fn boot_with_tenant(
    config: AppConfig,
    org: &str,
    document_root: &str,
    hostname: &str,
) -> TestServer {
    boot_with_orgs(config, &[(org, document_root, hostname)]).await
}

/// Boots once (seeding the organizations), inserts every tenant
/// organization and its domain, then reboots — the double boot every
/// data-driven test needs.
async fn boot_with_orgs(config: AppConfig, tenants: &[(&str, &str, &str)]) -> TestServer {
    {
        let server = TestServer::start_full(config.clone()).await;
        drop(server);
    }
    let pool = side_db(&config.database.url).await;
    for (org, document_root, hostname) in tenants {
        sqlx::query(
            "INSERT INTO organizations (key, name, document_root, created_at) \
             VALUES (?1, ?2, ?3, 0)",
        )
        .bind(org)
        .bind(format!("{org} site"))
        .bind(document_root)
        .execute(&pool)
        .await
        .expect("organization row inserted");
        sqlx::query(
            "INSERT INTO domains (hostname, organization_id, created_at) \
             SELECT ?1, id, 0 FROM organizations WHERE key = ?2",
        )
        .bind(hostname)
        .bind(org)
        .execute(&pool)
        .await
        .expect("domain row inserted");
    }
    drop(pool);
    TestServer::start_full(config).await
}

/// Registers `username` (the first account becomes the platform
/// admin) and returns a logged-in browser client.
async fn register_and_login(
    server: &TestServer,
    username: &str,
    password: &str,
) -> reqwest::Client {
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .header(HOST, HeaderValue::from_str(CMS_HOST).expect("cms host"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(response.status(), 201, "the account registers");

    let client = browser();
    let response = client
        .post(server.url("/api/auth/login"))
        .header(HOST, HeaderValue::from_str(CMS_HOST).expect("cms host"))
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

/// Grants `username` the `role` membership of `organization`, the
/// shape an administrator's grant through either member form
/// produces.
async fn grant_membership(url: &str, username: &str, org: &str, role: &str) {
    let pool = side_db(url).await;
    sqlx::query(
        "INSERT INTO memberships (user_id, organization_id, role, created_at) \
         SELECT u.id, o.id, ?3, 0 FROM users u, organizations o \
         WHERE u.username = ?1 AND o.key = ?2 \
         ON CONFLICT (user_id, organization_id) DO UPDATE SET role = excluded.role",
    )
    .bind(username)
    .bind(org)
    .bind(role)
    .execute(&pool)
    .await
    .expect("membership granted");
}

/// The account's row id (the member forms key on it).
async fn user_id_of(url: &str, username: &str) -> i64 {
    let pool = side_db(url).await;
    sqlx::query_scalar("SELECT id FROM users WHERE username = ?1")
        .bind(username)
        .fetch_one(&pool)
        .await
        .expect("user id")
}

/// The account's current `role` membership of `organization`.
async fn membership_role_of(url: &str, username: &str, org: &str) -> Option<String> {
    let pool = side_db(url).await;
    sqlx::query_scalar(
        "SELECT m.role FROM memberships m JOIN organizations o ON o.id = m.organization_id \
         WHERE m.user_id = (SELECT id FROM users WHERE username = ?1) AND o.key = ?2",
    )
    .bind(username)
    .bind(org)
    .fetch_optional(&pool)
    .await
    .expect("membership role")
}

// ─── The battery ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_team_page_answers_an_administrator_of_the_organization() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;

    let response = get(&tenant_admin, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the tenant's admin reads the team");
    let body = response.text().await.expect("team page");
    assert!(body.contains("tiendero"), "the member list: {body}");
    assert!(body.contains("status scheduled"), "the admin badge: {body}");
    assert!(
        body.contains("action=\"/admin/team\""),
        "the add-or-move form: {body}"
    );
    assert!(
        body.contains("href=\"/admin/team\""),
        "the sidebar link for the organization's admin: {body}"
    );
    assert!(
        body.contains("<span>admin</span>"),
        "the identity chip shows the organization's own role: {body}"
    );
}

#[tokio::test]
async fn an_editor_manages_content_not_people() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    register_and_login(&server, "tiendero", "secreto-123456").await;
    let editor = register_and_login(&server, "redactor", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;
    grant_membership(db.url(), "redactor", "shop", "editor").await;

    // The editor runs the content pages...
    let response = get(&editor, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "an editor manages content");
    let body = response.text().await.expect("pages page");
    assert!(
        !body.contains("href=\"/admin/team\""),
        "the sidebar hides the Team section from an editor: {body}"
    );
    assert!(
        body.contains("<span>editor</span>"),
        "the identity chip shows the organization's own role: {body}"
    );

    // ...and not the people page.
    let response = get(&editor, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(
        response.status(),
        403,
        "the team page is administrator-only"
    );
}

#[tokio::test]
async fn a_membership_ends_at_the_organizations_border() {
    let (_, db) = auth_config();
    let shop = TenantSite::new();
    let press = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_orgs(
        config,
        &[
            ("shop", &shop.root_string(), TENANT_HOST),
            ("press", &press.root_string(), OTHER_HOST),
        ],
    )
    .await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;

    // An administrator of one tenant is a stranger on another's host.
    let response = get(&tenant_admin, &server, "/admin/team", OTHER_HOST).await;
    assert_eq!(
        response.status(),
        403,
        "a membership of one tenant opens nothing on another's"
    );

    // The page is a tenant surface: the CMS host and the main tree
    // answer it with the standard 404 (not mounted there at all).
    let response = get(&tenant_admin, &server, "/admin/team", CMS_HOST).await;
    assert_eq!(response.status(), 404, "the CMS host does not serve it");
    let response = tenant_admin
        .get(server.url("/admin/team"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404, "the main tree does not serve it");
}

#[tokio::test]
async fn the_platform_admin_reaches_no_team_without_a_membership() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;

    // F20's zero-surprises rule, applied to the people surface: the
    // platform role opens no tenant team.
    let response = get(&platform, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(response.status(), 403, "no implicit platform bypass");

    // The door is the membership, exactly like the panel's content
    // pages: an editor membership still answers 403...
    grant_membership(db.url(), "plataforma", "shop", "editor").await;
    let response = get(&platform, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(response.status(), 403, "an editor is not an administrator");

    // ...and an administrator's answers.
    grant_membership(db.url(), "plataforma", "shop", "admin").await;
    let response = get(&platform, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the membership is the door");
}

#[tokio::test]
async fn an_administrator_grants_and_moves_a_membership() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    let newcomer = register_and_login(&server, "redactor", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;

    // A stranger so far: no membership, no panel.
    let response = get(&newcomer, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 403, "no membership, no panel");

    // The administrator grants the editor membership.
    let response = post(
        &tenant_admin,
        &server,
        "/admin/team",
        TENANT_HOST,
        "username=redactor&role=editor",
    )
    .await;
    assert_eq!(response.status(), 303, "the form saves (PRG)");
    assert_eq!(
        response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/admin/team?ok=member-added"),
        "the flash names the save"
    );

    // The membership opens the panel on the very next request.
    let response = get(&newcomer, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the editor manages content");
    let response = get(&newcomer, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(response.status(), 403, "but not the people");

    // The member list shows them.
    let body = get(&tenant_admin, &server, "/admin/team", TENANT_HOST)
        .await
        .text()
        .await
        .expect("team page");
    assert!(body.contains("redactor"), "the list: {body}");

    // Re-adding with another role moves them.
    let response = post(
        &tenant_admin,
        &server,
        "/admin/team",
        TENANT_HOST,
        "username=redactor&role=admin",
    )
    .await;
    assert_eq!(response.status(), 303, "the move saves");
    let response = get(&newcomer, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "an administrator reads the team");
}

#[tokio::test]
async fn a_removed_membership_locks_the_panel_immediately() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    let editor = register_and_login(&server, "redactor", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;
    grant_membership(db.url(), "redactor", "shop", "editor").await;

    let response = get(&editor, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the editor is in");

    let response = post(
        &tenant_admin,
        &server,
        &format!(
            "/admin/team/{}/delete",
            user_id_of(db.url(), "redactor").await
        ),
        TENANT_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the removal saves (PRG)");
    assert_eq!(
        response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/admin/team?ok=member-removed"),
        "the flash names the removal"
    );

    // The still-valid cookie opens nothing anymore: the guards read
    // the table, never the token.
    let response = get(&editor, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 403, "the door closes immediately");
}

#[tokio::test]
async fn the_last_administrator_never_steps_down_alone() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    let other = register_and_login(&server, "redactor", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;

    // The demotion is refused with an explanation, the form keeping
    // what was typed.
    let response = post(
        &tenant_admin,
        &server,
        "/admin/team",
        TENANT_HOST,
        "username=tiendero&role=editor",
    )
    .await;
    assert_eq!(response.status(), 200, "the form re-renders");
    let body = response.text().await.expect("team page");
    assert!(
        body.contains("last administrator"),
        "the explanation: {body}"
    );
    assert_eq!(
        membership_role_of(db.url(), "tiendero", "shop")
            .await
            .as_deref(),
        Some("admin"),
        "the membership did not move"
    );

    // The removal is refused the same way.
    let response = post(
        &tenant_admin,
        &server,
        &format!(
            "/admin/team/{}/delete",
            user_id_of(db.url(), "tiendero").await
        ),
        TENANT_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the refusal redirects");
    assert_eq!(
        response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/admin/team?error=last-admin"),
        "the flash names the law"
    );
    let response = get(&tenant_admin, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the administrator stays");

    // With a second administrator in place, the first may leave.
    let response = post(
        &tenant_admin,
        &server,
        "/admin/team",
        TENANT_HOST,
        "username=redactor&role=admin",
    )
    .await;
    assert_eq!(
        response.status(),
        303,
        "the second administrator is granted"
    );

    let response = post(
        &tenant_admin,
        &server,
        &format!(
            "/admin/team/{}/delete",
            user_id_of(db.url(), "tiendero").await
        ),
        TENANT_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the leaving saves");
    let response = get(&other, &server, "/admin/team", TENANT_HOST).await;
    assert_eq!(
        response.status(),
        200,
        "the remaining administrator runs it"
    );
    let response = get(&tenant_admin, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 403, "the leaver is out");
}

#[tokio::test]
async fn unknown_accounts_and_plain_roles_are_refused_with_an_explanation() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    register_and_login(&server, "redactor", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;

    // No account answers to that username.
    let response = post(
        &tenant_admin,
        &server,
        "/admin/team",
        TENANT_HOST,
        "username=ghost&role=editor",
    )
    .await;
    assert_eq!(response.status(), 200, "the form re-renders");
    assert!(
        response
            .text()
            .await
            .expect("team page")
            .contains("No account answers to that username"),
        "the explanation"
    );

    // A plain user role is not a membership.
    let response = post(
        &tenant_admin,
        &server,
        "/admin/team",
        TENANT_HOST,
        "username=redactor&role=user",
    )
    .await;
    assert_eq!(response.status(), 200, "the form re-renders");
    assert!(
        response
            .text()
            .await
            .expect("team page")
            .contains("Memberships are administrator or editor"),
        "the explanation"
    );
}

#[tokio::test]
async fn the_platform_abides_by_the_same_law() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    register_and_login(&server, "tiendero", "secreto-123456").await;
    register_and_login(&server, "redactor", "secreto-123456").await;
    grant_membership(db.url(), "tiendero", "shop", "admin").await;

    // The platform's Tenants page refuses the last administrator's
    // demotion, the form keeping what was typed.
    let response = post(
        &platform,
        &server,
        "/admin/tenants/shop/members",
        CMS_HOST,
        "username=tiendero&role=editor",
    )
    .await;
    assert_eq!(response.status(), 200, "the form re-renders");
    assert!(
        response
            .text()
            .await
            .expect("tenant detail")
            .contains("last administrator"),
        "the explanation"
    );

    // ...and the removal, with the flash naming the law.
    let response = post(
        &platform,
        &server,
        &format!(
            "/admin/tenants/shop/members/{}/delete",
            user_id_of(db.url(), "tiendero").await
        ),
        CMS_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the refusal redirects");
    assert!(
        response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("location")
            .contains("error=last-admin"),
        "the flash names the law"
    );
    assert_eq!(
        membership_role_of(db.url(), "tiendero", "shop")
            .await
            .as_deref(),
        Some("admin"),
        "the membership stays"
    );

    // With a second administrator granted through the same form,
    // the removal goes through.
    let response = post(
        &platform,
        &server,
        "/admin/tenants/shop/members",
        CMS_HOST,
        "username=redactor&role=admin",
    )
    .await;
    assert_eq!(response.status(), 303, "the grant saves");
    let response = post(
        &platform,
        &server,
        &format!(
            "/admin/tenants/shop/members/{}/delete",
            user_id_of(db.url(), "tiendero").await
        ),
        CMS_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the removal saves");
    assert_eq!(
        membership_role_of(db.url(), "tiendero", "shop").await,
        None,
        "the membership is gone"
    );
}

#[tokio::test]
async fn deleting_the_solo_administrator_of_a_tenant_is_refused() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = team_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    register_and_login(&server, "tiendero", "secreto-123456").await;
    register_and_login(&server, "redactor", "secreto-123456").await;
    // A plain platform account that solo-administers the tenant.
    grant_membership(db.url(), "redactor", "shop", "admin").await;

    let response = post(
        &platform,
        &server,
        &format!(
            "/admin/users/{}/delete",
            user_id_of(db.url(), "redactor").await
        ),
        CMS_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the refusal redirects");
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("location")
        .to_owned();
    assert!(
        location.contains("error=solo-tenant-admin") && location.contains("tenant=shop"),
        "the flash names the organization: {location}"
    );

    // The Users page explains, naming the organization.
    let body = get(&platform, &server, &location, CMS_HOST)
        .await
        .text()
        .await
        .expect("users page");
    assert!(
        body.contains("last administrator of the shop organization"),
        "the explanation: {body}"
    );

    // The account is still there.
    assert_eq!(
        user_id_of(db.url(), "redactor").await,
        user_id_of(db.url(), "redactor").await,
        "the account survives"
    );

    // With a second administrator in the tenant, the deletion goes
    // through.
    grant_membership(db.url(), "tiendero", "shop", "admin").await;
    let response = post(
        &platform,
        &server,
        &format!(
            "/admin/users/{}/delete",
            user_id_of(db.url(), "redactor").await
        ),
        CMS_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the deletion saves");
    assert!(
        response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("location")
            .contains("ok=deleted"),
        "the flash names the deletion"
    );
}
