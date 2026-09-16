//! Per-tenant panels (F20): the organizations own their content, and
//! every mapped host serves its own.
//!
//! Boots the full server twice — once to seed the bootstrap
//! organizations (the `[cms] hosts` list maps the CMS host), then again
//! with a third organization and its domain inserted straight into the
//! database — and exercises the split with `Host`-pinned, cookie-storing
//! clients:
//!
//! - the **CMS organization's** host keeps the platform surface (users,
//!   tenants, the import tool) plus its own content;
//! - the **tenant's** host serves the visitor surface scoped to the
//!   tenant: pages, search, feeds, sitemap, media, and the panel's
//!   content pages — with the platform routes answering its standard
//!   404 (they are not mounted there at all);
//! - authorization follows the request's organization: a membership of
//!   one tenant is worth nothing on another's host, and the platform
//!   admin reaches the tenants only through explicit memberships —
//!   F20's "zero surprises" rule.

mod common;

use std::time::Duration;

use reqwest::header::{HeaderValue, HOST};
use wallermax_server::config::AppConfig;

use common::{auth_config, TestServer};

// ─── Harness ────────────────────────────────────────────────────────

/// The CMS host and the tenant host these tests pin.
const CMS_HOST: &str = "cms.f20.test";
const TENANT_HOST: &str = "shop.f20.test";

/// The full stack over `url`, with `[cms] hosts` naming the CMS host.
fn tenant_cms_config(url: &str) -> AppConfig {
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

/// Minimal percent-encoding for form values (titles and bodies).
fn enc(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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

/// A throwaway directory standing in for the tenant's site root,
/// removed (best-effort) on drop.
struct TenantSite {
    root: std::path::PathBuf,
}

impl TenantSite {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "wallermax-f20-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("tenant root created");
        Self { root }
    }

    fn write(&self, relative: &str, contents: &str) {
        std::fs::write(self.root.join(relative), contents).expect("file written");
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

/// Boots once (seeding the organizations), inserts the tenant
/// organization and its domain, then reboots — the double boot every
/// data-driven test needs.
async fn boot_with_tenant(
    config: AppConfig,
    org: &str,
    document_root: &str,
    hostname: &str,
) -> TestServer {
    {
        let server = TestServer::start_full(config.clone()).await;
        drop(server);
    }
    let pool = side_db(&config.database.url).await;
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

/// Grants `username` the `role` membership of the tenant organization,
/// straight into the file — the shape a platform admin's grant through
/// the Tenants page produces.
async fn grant_tenant_membership(url: &str, username: &str, role: &str) {
    let pool = side_db(url).await;
    sqlx::query(
        "INSERT INTO memberships (user_id, organization_id, role, created_at) \
         SELECT u.id, o.id, ?3, 0 FROM users u, organizations o \
         WHERE u.username = ?1 AND o.key = 'shop' \
         ON CONFLICT (user_id, organization_id) DO UPDATE SET role = excluded.role",
    )
    .bind(username)
    .bind(username)
    .bind(role)
    .execute(&pool)
    .await
    .expect("membership granted");
}

/// Creates a page through the panel forms on `host`, returning its id.
async fn create_page(
    client: &reqwest::Client,
    server: &TestServer,
    host: &str,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
) -> i64 {
    let body = format!(
        "slug={slug}&title={}&content={}&redirect=%2Fadmin%2Fpages{}",
        enc(title),
        enc(content),
        if publish { "&is_published=on" } else { "" }
    );
    let response = post(client, server, "/admin/pages", host, &body).await;
    assert_eq!(response.status(), 303, "the page saves (PRG)");
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    // /admin/pages/{id}/edit?ok=creada
    let id = location
        .trim_start_matches("/admin/pages/")
        .split('/')
        .next()
        .expect("numeric page id");
    id.parse().expect("numeric page id")
}

// ─── The battery ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_same_slug_serves_two_organizations() {
    let (_, db) = auth_config();
    let site = TenantSite::new();
    site.write("index.html", "<h1>Shop home</h1>");

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    // Two accounts: the platform admin (a CMS-organization member by
    // the F15 mirror) and the tenant's own admin.
    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    // The same slug, created by each organization's own member on its
    // own host.
    let cms_id = create_page(
        &platform,
        &server,
        CMS_HOST,
        "quienes-somos",
        "Quienes somos (CMS)",
        "<p>the cms copy</p>",
        true,
    )
    .await;
    let shop_id = create_page(
        &tenant_admin,
        &server,
        TENANT_HOST,
        "quienes-somos",
        "Quienes somos (Shop)",
        "<p>the shop copy</p>",
        true,
    )
    .await;
    assert_ne!(cms_id, shop_id, "two rows, two organizations");

    // Each host serves its own organization's page.
    for (host, title, copy) in [
        (CMS_HOST, "Quienes somos (CMS)", "the cms copy"),
        (TENANT_HOST, "Quienes somos (Shop)", "the shop copy"),
    ] {
        let response = get(&reqwest::Client::new(), &server, "/p/quienes-somos", host).await;
        assert_eq!(response.status(), 200, "{host} serves the slug");
        let body = response.text().await.expect("body");
        assert!(body.contains(title), "{host} shows its own title: {body}");
        assert!(body.contains(copy), "{host} shows its own body: {body}");
    }
}

#[tokio::test]
async fn cross_organization_page_ids_are_foreign() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    let cms_id = create_page(
        &platform,
        &server,
        CMS_HOST,
        "solo-cms",
        "CMS page",
        "<p>cms</p>",
        true,
    )
    .await;

    // The tenant's admin cannot edit, update or delete the CMS page:
    // the id simply does not exist in their organization.
    let response = get(
        &tenant_admin,
        &server,
        &format!("/admin/pages/{cms_id}/edit"),
        TENANT_HOST,
    )
    .await;
    assert_eq!(
        response.status(),
        303,
        "a foreign id redirects to the listing"
    );
    assert!(
        response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|location| location == "/admin/pages"),
        "the foreign page is a non-event"
    );

    let body = "slug=solo-cms&title=Hijacked&content=hijacked&redirect=%2Fadmin%2Fpages";
    let response = post(
        &tenant_admin,
        &server,
        &format!("/admin/pages/{cms_id}"),
        TENANT_HOST,
        body,
    )
    .await;
    assert_eq!(
        response.status(),
        303,
        "a foreign update is a non-event (back to the listing)"
    );

    let response = post(
        &tenant_admin,
        &server,
        &format!("/admin/pages/{cms_id}/delete"),
        TENANT_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "a foreign delete is a no-op");

    // And the page is still there, untouched, on its own host.
    let response = get(&reqwest::Client::new(), &server, "/p/solo-cms", CMS_HOST).await;
    assert_eq!(response.status(), 200, "the CMS page survived");
}

#[tokio::test]
async fn the_platform_surface_stays_on_the_cms_host() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    // Users, tenants and the import tool: mounted only on the CMS
    // organization's tree — the tenant host does not serve them. The
    // import path additionally collides with the tenant's own
    // `/admin/pages/{id}` (POST-only), so it answers 405 there — the
    // generic method fallback, never a panel page.
    for path in ["/admin/users", "/admin/users/new", "/admin/tenants"] {
        let response = get(&platform, &server, path, TENANT_HOST).await;
        assert_eq!(
            response.status(),
            404,
            "{path} is not mounted on the tenant host"
        );
    }
    let response = get(&platform, &server, "/admin/pages/import", TENANT_HOST).await;
    assert_eq!(
        response.status(),
        405,
        "the import tool does not serve on the tenant host (the POST-only page route's method fallback)"
    );

    // ...while the CMS host serves them to the same account.
    for path in ["/admin/users", "/admin/tenants", "/admin/pages/import"] {
        let response = get(&platform, &server, path, CMS_HOST).await;
        assert_eq!(response.status(), 200, "{path} serves on the CMS host");
    }

    // The tenant's admin reaches their own content pages, not those.
    let response = get(&tenant_admin, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the tenant's own panel serves");
}

#[tokio::test]
async fn memberships_authorize_per_host() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    // The platform admin (a member of the CMS organization only) runs
    // the CMS host's panel...
    let response = get(&platform, &server, "/admin", CMS_HOST).await;
    assert_eq!(response.status(), 200, "the CMS host answers its admin");
    // ...and is a stranger on the tenant's — F20's zero-surprises rule:
    // reach into a tenant through an explicit membership, never by
    // virtue of the platform role.
    let response = get(&platform, &server, "/admin", TENANT_HOST).await;
    assert_eq!(response.status(), 403, "no implicit platform bypass");

    // The tenant's admin is the mirror image.
    let response = get(&tenant_admin, &server, "/admin", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the tenant host answers its admin");
    let response = get(&tenant_admin, &server, "/admin", CMS_HOST).await;
    assert_eq!(response.status(), 403, "the CMS host does not know them");

    // And the sidebar keeps the platform links on the CMS organization.
    let body = get(&tenant_admin, &server, "/admin", TENANT_HOST)
        .await
        .text()
        .await
        .expect("dashboard");
    assert!(
        !body.contains("href=\"/admin/users\"") && !body.contains("href=\"/admin/tenants\""),
        "the tenant's panel hides the platform sections: {body}"
    );
    let body = get(&platform, &server, "/admin", CMS_HOST)
        .await
        .text()
        .await
        .expect("dashboard");
    assert!(
        body.contains("href=\"/admin/users\"") && body.contains("href=\"/admin/tenants\""),
        "the CMS organization's panel keeps them"
    );
}

#[tokio::test]
async fn drafts_gate_by_the_request_organization() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    // A tenant draft.
    create_page(
        &tenant_admin,
        &server,
        TENANT_HOST,
        "borrador",
        "Draft",
        "<p>not yet</p>",
        false,
    )
    .await;

    // Anonymous: the same 404 as a missing slug — on both hosts (the
    // slug does not exist in the CMS organization at all).
    for host in [TENANT_HOST, CMS_HOST] {
        let response = get(&reqwest::Client::new(), &server, "/p/borrador", host).await;
        assert_eq!(response.status(), 404, "anonymous on {host}");
    }

    // The tenant's member previews it on their own host...
    let response = get(&tenant_admin, &server, "/p/borrador", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "the tenant's member previews");
    // ...and the platform admin cannot, even signed in: preview rights
    // are a membership of the request's organization, not the token's
    // platform role.
    let response = get(&platform, &server, "/p/borrador", TENANT_HOST).await;
    assert_eq!(
        response.status(),
        404,
        "the platform role does not open another tenant's drafts"
    );
}

#[tokio::test]
async fn search_feeds_and_sitemap_scope_to_the_organization() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    create_page(
        &platform,
        &server,
        CMS_HOST,
        "cms-needle",
        "CMS needle page",
        "<p>the haystack cms needle</p>",
        true,
    )
    .await;
    create_page(
        &tenant_admin,
        &server,
        TENANT_HOST,
        "shop-needle",
        "Shop needle page",
        "<p>the haystack shop needle</p>",
        true,
    )
    .await;

    // Search: each host finds only its organization's hit.
    for (host, hit, miss) in [
        (CMS_HOST, "CMS needle page", "Shop needle page"),
        (TENANT_HOST, "Shop needle page", "CMS needle page"),
    ] {
        let response = get(&reqwest::Client::new(), &server, "/search?q=needle", host).await;
        assert_eq!(response.status(), 200);
        let body = response.text().await.expect("body");
        assert!(body.contains(hit), "{host} finds its own page: {body}");
        assert!(!body.contains(miss), "{host} never shows the other's");
    }

    // Feeds and sitemap: the same scoping, absolute URLs built from
    // the request's own host for the tenant.
    let response = get(&reqwest::Client::new(), &server, "/feed.xml", TENANT_HOST).await;
    let body = response.text().await.expect("body");
    assert!(body.contains("shop-needle"), "the tenant's feed: {body}");
    assert!(!body.contains("cms-needle"), "only the tenant's pages");
    assert!(
        body.contains(&format!("http://{TENANT_HOST}/p/shop-needle")),
        "the tenant's own host builds the URLs: {body}"
    );
    let response = get(
        &reqwest::Client::new(),
        &server,
        "/sitemap.xml",
        TENANT_HOST,
    )
    .await;
    let body = response.text().await.expect("body");
    assert!(
        body.contains("shop-needle") && !body.contains("cms-needle"),
        "sitemap: {body}"
    );
}

#[tokio::test]
async fn the_tenant_panel_manages_its_own_content() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    // The tenant's dashboard starts empty.
    let body = get(&tenant_admin, &server, "/admin", TENANT_HOST)
        .await
        .text()
        .await
        .expect("dashboard");
    assert!(body.contains("No revisions yet"), "empty tenant: {body}");

    // A page and a menu, created on the tenant's host.
    create_page(
        &tenant_admin,
        &server,
        TENANT_HOST,
        "catalogo",
        "Catálogo",
        "<p>the catalog</p>",
        true,
    )
    .await;
    let response = post(
        &tenant_admin,
        &server,
        "/admin/menus",
        TENANT_HOST,
        "name=main&title=Main&redirect=%2Fadmin%2Fmenus",
    )
    .await;
    assert_eq!(response.status(), 303, "the menu saves");

    // The dashboard counts the tenant's own content, not the world's.
    let body = get(&tenant_admin, &server, "/admin", TENANT_HOST)
        .await
        .text()
        .await
        .expect("dashboard");
    assert!(
        body.contains("Catálogo"),
        "the activity feed shows the save: {body}"
    );

    // And the same menu name is free on the CMS organization's host.
    let response = post(
        &platform,
        &server,
        "/admin/menus",
        CMS_HOST,
        "name=main&title=Main too&redirect=%2Fadmin%2Fmenus",
    )
    .await;
    assert_eq!(
        response.status(),
        303,
        "the same menu name per organization"
    );
}

#[tokio::test]
async fn the_login_round_trip_returns_to_the_tenant_page() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;
    create_page(
        &tenant_admin,
        &server,
        TENANT_HOST,
        "privada",
        "Private-ish",
        "<p>the page</p>",
        true,
    )
    .await;

    // A fresh browser signs in on the tenant host with the page it
    // came from in the redirect field.
    let client = browser();
    let response = client
        .post(server.url("/api/auth/login"))
        .header(
            HOST,
            HeaderValue::from_str(TENANT_HOST).expect("tenant host"),
        )
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=tiendero&password=secreto-123456&redirect=%2Fp%2Fprivada")
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "the login redirects");
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("location")
        .to_owned();
    assert_eq!(location, "/p/privada", "back to the very page, same-origin");

    let response = get(&client, &server, "/p/privada", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "signed in and back home");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("tiendero"),
        "the session works on the tenant host"
    );
}

#[tokio::test]
async fn media_serves_only_its_organization() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;

    // A tiny valid PNG, uploaded on the CMS host.
    let png = image_fixture();
    let boundary = format!("wmsBoundary{}", std::process::id());
    let mut multipart = Vec::new();
    multipart.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    multipart.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"logo.png\"\r\n",
    );
    multipart.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    multipart.extend_from_slice(&png);
    multipart.extend_from_slice(b"\r\n");
    multipart.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let response = platform
        .post(server.url("/admin/media"))
        .header(HOST, HeaderValue::from_str(CMS_HOST).expect("cms host"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(multipart)
        .send()
        .await
        .expect("upload");
    if response.status() != 303 {
        let body = response.text().await.unwrap_or_default();
        panic!("the upload lands (PRG): {body}");
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("location")
        .to_owned();
    let media_id: i64 = location
        .trim_start_matches("/admin/media/")
        .parse()
        .expect("numeric media id");

    // The stored name from the side connection (the canonical URL).
    let pool = side_db(db.url()).await;
    let stored_name: String = sqlx::query_scalar("SELECT stored_name FROM media WHERE id = ?1")
        .bind(media_id)
        .fetch_one(&pool)
        .await
        .expect("row");
    drop(pool);

    // The CMS organization's host serves it...
    let response = get(
        &reqwest::Client::new(),
        &server,
        &format!("/media/{media_id}/{stored_name}"),
        CMS_HOST,
    )
    .await;
    assert_eq!(response.status(), 200, "the owner's host serves it");

    // ...and the tenant's host answers 404 for the very same id: an
    // id is only reachable on its own organization's host.
    let response = get(
        &reqwest::Client::new(),
        &server,
        &format!("/media/{media_id}/{stored_name}"),
        TENANT_HOST,
    )
    .await;
    assert_eq!(response.status(), 404, "cross-tenant media is a 404");

    // And the media listing on the tenant host is empty, not shared.
    let response = get(&tenant_admin, &server, "/admin/media", TENANT_HOST).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(!body.contains("logo.png"), "the tenant's grid is its own");
}

#[tokio::test]
async fn a_tenant_with_content_is_not_deletable() {
    let (_, db) = auth_config();
    let site = TenantSite::new();

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    let platform = register_and_login(&server, "plataforma", "secreto-123456").await;
    let tenant_admin = register_and_login(&server, "tiendero", "secreto-123456").await;
    grant_tenant_membership(db.url(), "tiendero", "admin").await;
    create_page(
        &tenant_admin,
        &server,
        TENANT_HOST,
        "irreplaceable",
        "Content lives here",
        "<p>mine</p>",
        false,
    )
    .await;

    // The platform admin tries to delete the tenant through the
    // Tenants page — refused while it owns content.
    let response = post(
        &platform,
        &server,
        "/admin/tenants/shop/delete",
        CMS_HOST,
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the delete route answers");
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("location")
        .to_owned();
    assert!(
        location.contains("error=content"),
        "refused with the content guard: {location}"
    );

    // The organization and its page are still there.
    let pool = side_db(db.url()).await;
    let tenants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organizations WHERE key = 'shop'")
        .fetch_one(&pool)
        .await
        .expect("count");
    drop(pool);
    assert_eq!(tenants, 1, "the tenant survived its own content");
    let response = get(&tenant_admin, &server, "/admin/pages", TENANT_HOST).await;
    assert_eq!(response.status(), 200, "and its panel keeps working");
}

#[tokio::test]
async fn the_tenants_own_static_home_still_wins() {
    let (_, db) = auth_config();
    let site = TenantSite::new();
    site.write("index.html", "<h1>The shop's own home</h1>");

    let config = tenant_cms_config(db.url());
    let server = boot_with_tenant(config, "shop", &site.root_string(), TENANT_HOST).await;

    // The tenant's files outrank the shared visitor surface: GET / is
    // the tenant's own index, not the shared homepage view.
    let response = get(&reqwest::Client::new(), &server, "/", TENANT_HOST).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("The shop's own home"),
        "the tenant's static home: {body}"
    );
}

// ─── Fixture ─────────────────────────────────────────────────────────

/// A minimal valid PNG (1x1) the upload accepts.
fn image_fixture() -> Vec<u8> {
    let mut image = image::RgbImage::new(6, 4);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let value = ((x + y) % 256) as u8;
        *pixel = image::Rgb([value, 255 - value, value / 2]);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("fixture encodes");
    bytes
}
