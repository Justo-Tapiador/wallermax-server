//! The Tenants battery (F18): the panel's management pages.
//!
//! Boots the full server exactly like the binary (database, auth,
//! templates, static files and the CMS) over a fresh temporary SQLite
//! file and drives the pages the way a browser would — forms in,
//! `303`s out, flash codes in the query string:
//!
//! - the pages answer only to an administrator of the CMS
//!   organization (the F15 guard), never to an editor or an
//!   anonymous visitor;
//! - a tenant lives its whole lifecycle in forms — created, edited,
//!   deleted, its rows going with it;
//! - **a host name moves without a restart**: the panel write swaps
//!   the live vhost snapshot, so the very next request is classified
//!   with it — the headline promise of the phase;
//! - the provenance rules hold: a `[cms] hosts`-seeded row cannot be
//!   unmapped here (it would come back), and a configured host name
//!   cannot be claimed for another organization;
//! - the membership mirror's boundary holds: the CMS organization's
//!   members are refused (they follow the platform roles), a
//!   tenant's members are data — the first sanctioned divergence,
//!   and one that opens nothing by itself.
//!
//! The main tree these tests lean on is the battery's own fixture:
//! `[static] root_dir` points at a per-test directory holding the
//! index the assertions name, never at the repository's live
//! `public/`. That tree is the operator's to experiment in — an
//! `index.jhs` for the main site is the F19 direction — and a
//! template that misbehaves there must never be able to fail
//! `cargo test`.

mod common;

use common::{auth_config, TempDbGuard, TestServer};
use reqwest::header::{HeaderValue, HOST};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use wallermax_server::config::AppConfig;

/// The single-host shape (no `[cms] hosts`): every surface on one
/// host, no Host pinning needed. The vhost tests start from here and
/// set `cms.hosts` — the same shape with the panel on its own name.
///
/// The static root is [`main_tree_fixture`]'s directory, kept alive
/// by the returned guard for as long as the server serves it — the
/// one call every test in this battery starts from.
fn single_host_config() -> (AppConfig, TempDbGuard, TenantSite) {
    let (mut config, db) = auth_config();
    let main = main_tree_fixture();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = main.root_string();
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    (config, db, main)
}

/// The battery's own main tree: a directory whose index says what
/// the assertions expect to read ("Wallermax"), so the main-tree
/// checks are exact instead of ambient.
///
/// The live `public/` stays out on purpose. `GET /` on a
/// main-classified host renders `index.jhs` when one lives there,
/// and `host_names_move_without_a_restart` sends the one request
/// shape in the whole battery that does so **with a session
/// attached** (the admin's cookie jar travels with the client) — so
/// an operator's main-site template, however broken for signed-in
/// visitors, used to fail this suite with a bare `500` that blamed
/// the wrong layer. The fixture ends that class of failure: the
/// battery answers for its own tree, the operator answers for
/// theirs.
fn main_tree_fixture() -> TenantSite {
    let main = TenantSite::new("main-tree");
    main.write(
        "index.html",
        "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\">\n  \
         <title>Wallermax Server</title>\n</head>\n<body>\n  <h1>Wallermax Server</h1>\n  <p>The tenants battery's own main tree.</p>\n</body>\n</html>\n",
    );
    main
}

/// A cookie-storing client: `Set-Cookie` in, `Cookie` out — a
/// browser. Redirects stay manual so each hop can be asserted.
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
/// cookie-bearing client. `host` pins the request when the panel
/// lives on its own host name.
async fn login_browser(
    server: &TestServer,
    host: Option<&str>,
    username: &str,
    password: &str,
) -> reqwest::Client {
    let client = browser_client();
    let mut request = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={username}&password={password}&redirect=/"
        ));
    if let Some(value) = host {
        request = request.header(HOST, HeaderValue::from_str(value).expect("test host value"));
    }
    let response = request.send().await.expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    client
}

/// POSTs a form body, optionally pinned to `host`, and returns the
/// response (redirects never followed — each hop is asserted).
async fn post_form(
    client: &reqwest::Client,
    server: &TestServer,
    host: Option<&str>,
    path: &str,
    body: &str,
) -> reqwest::Response {
    let mut request = client
        .post(server.url(path))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.to_owned());
    if let Some(value) = host {
        request = request.header(HOST, HeaderValue::from_str(value).expect("test host value"));
    }
    request.send().await.expect("form post succeeds")
}

/// GETs `path`, optionally pinned to `host`.
async fn get(
    client: &reqwest::Client,
    server: &TestServer,
    host: Option<&str>,
    path: &str,
) -> reqwest::Response {
    let mut request = client.get(server.url(path));
    if let Some(value) = host {
        request = request.header(HOST, HeaderValue::from_str(value).expect("test host value"));
    }
    request.send().await.expect("request")
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

/// A throwaway directory tree standing in for a served site — a
/// tenant's, or the battery's own main tree (see
/// [`main_tree_fixture`]) — removed (best-effort) on drop.
struct TenantSite {
    root: std::path::PathBuf,
}

static SITE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TenantSite {
    /// A fresh, existing root directory.
    fn new(tag: &str) -> Self {
        let unique = SITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wallermax-panel-{}-{tag}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("tenant root created");
        Self { root }
    }

    /// Writes `contents` at `relative` (creating the directories).
    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent directory"))
            .expect("directories created");
        std::fs::write(path, contents).expect("file written");
    }

    /// The root as a forward-slash path (valid on Windows too).
    fn root_string(&self) -> String {
        self.root.display().to_string().replace('\\', "/")
    }
}

impl Drop for TenantSite {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Creates a tenant through the panel and asserts the redirect.
async fn create_tenant(
    client: &reqwest::Client,
    server: &TestServer,
    host: Option<&str>,
    key: &str,
    root: &str,
) {
    let response = post_form(
        client,
        server,
        host,
        "/admin/tenants",
        &format!("key={key}&name={key}%20site&document_root={root}"),
    )
    .await;
    assert_eq!(response.status(), 303, "tenant creation redirects");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, format!("/admin/tenants/{key}?ok=created"));
}

#[tokio::test]
async fn the_tenants_pages_are_admin_only() {
    let (config, _db, _main) = single_host_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, None, "root-admin", "sup3r-secret!").await;
    let response = get(&admin, &server, None, "/admin/tenants").await;
    assert_eq!(response.status(), 200, "the administrator sees the tenants");

    // An editor of the CMS organization manages content, not tenants.
    let response = post_form(
        &admin,
        &server,
        None,
        "/admin/users",
        "username=penelope&password=password-123&role=editor",
    )
    .await;
    assert_eq!(response.status(), 303, "user creation redirects");
    let editor = login_browser(&server, None, "penelope", "password-123").await;
    let response = get(&editor, &server, None, "/admin/tenants").await;
    assert_eq!(
        response.status(),
        403,
        "an editor membership is not an administrator membership"
    );

    // An anonymous visitor is walked to the login page.
    let anonymous = browser_client();
    let response = get(&anonymous, &server, None, "/admin/tenants").await;
    assert_eq!(response.status(), 303, "anonymous visitors redirect");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert!(
        location.starts_with("/login?redirect="),
        "the redirect points at the login page: {location}"
    );
}

#[tokio::test]
async fn the_bootstrap_organizations_are_listed_but_read_only() {
    let (config, db, _main) = single_host_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, None, "root-admin", "sup3r-secret!").await;

    let response = get(&admin, &server, None, "/admin/tenants").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("main") && body.contains("cms"),
        "both listed: {body}"
    );

    // The detail page renders read-only, with the pointer to the
    // configuration.
    let response = get(&admin, &server, None, "/admin/tenants/cms").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("wallermax.toml"),
        "the settings explain where the bootstrap data lives: {body}"
    );

    // Settings edits are refused (the seed would restore them).
    let response = post_form(
        &admin,
        &server,
        None,
        "/admin/tenants/main",
        "name=Hacked&document_root=elsewhere",
    )
    .await;
    assert_eq!(response.status(), 303, "the edit is refused");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants/main?error=seeded");

    // Deletes too.
    let response = post_form(&admin, &server, None, "/admin/tenants/cms/delete", "").await;
    assert_eq!(response.status(), 303, "the delete is refused");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants/cms?error=seeded");

    let side = side_db(db.url()).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organizations")
        .fetch_one(&side)
        .await
        .expect("organizations counted");
    side.close().await;
    assert_eq!(rows, 2, "nothing was deleted");
}

#[tokio::test]
async fn a_tenant_lives_its_lifecycle_in_forms() {
    let (config, db, _main) = single_host_config();
    let server = TestServer::start_full(config).await;

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, None, "root-admin", "sup3r-secret!").await;

    let site = TenantSite::new("lifecycle");
    site.write("index.html", "<p>Acme home</p>");
    create_tenant(&admin, &server, None, "acme", &site.root_string()).await;

    // The detail page lists it with its counts.
    let response = get(&admin, &server, None, "/admin/tenants/acme").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("acme") && body.contains(&site.root_string()),
        "detail: {body}"
    );

    // Duplicate keys, reserved keys and malformed keys are refused
    // with the form re-rendered (200) and the values kept.
    for (key, note) in [
        ("acme", "duplicate"),
        ("main", "reserved seeder key"),
        ("new", "reserved route segment"),
        ("Acme", "uppercase"),
        ("a b", "whitespace"),
        ("x", "too short"),
    ] {
        let response = post_form(
            &admin,
            &server,
            None,
            "/admin/tenants",
            &format!("key={key}&name=Whatever&document_root=sites/whatever"),
        )
        .await;
        assert_eq!(response.status(), 200, "refused ({note}): {key}");
        let body = response.text().await.expect("body");
        assert!(
            body.contains("notice-error"),
            "the refusal re-renders the form with an error ({note}): {key}"
        );
    }

    // The settings edit lands.
    let response = post_form(
        &admin,
        &server,
        None,
        "/admin/tenants/acme",
        "name=Acme%20Incorporated&document_root=sites/acme",
    )
    .await;
    assert_eq!(response.status(), 303, "the edit redirects");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants/acme?ok=saved");

    // The delete cascades: the row and its host names go together.
    let response = post_form(
        &admin,
        &server,
        None,
        "/admin/tenants/acme/domains",
        "hostname=acme.example.com",
    )
    .await;
    assert_eq!(response.status(), 303, "the host name maps");
    let response = post_form(&admin, &server, None, "/admin/tenants/acme/delete", "").await;
    assert_eq!(response.status(), 303, "the delete redirects");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants?ok=deleted");

    let side = side_db(db.url()).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organizations WHERE key = 'acme'")
        .fetch_one(&side)
        .await
        .expect("organizations counted");
    assert_eq!(rows, 0, "the tenant is gone");
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM domains WHERE hostname = 'acme.example.com'")
            .fetch_one(&side)
            .await
            .expect("domains counted");
    assert_eq!(rows, 0, "its host name went with it");
    side.close().await;

    // A missing tenant redirects to the listing.
    let response = get(&admin, &server, None, "/admin/tenants/acme").await;
    assert_eq!(response.status(), 303, "the detail redirects");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants");
}

#[tokio::test]
async fn host_names_move_without_a_restart() {
    let (mut config, db, _main) = single_host_config();
    config.cms.hosts = vec![String::from("cms.localhost")];
    let server = TestServer::start_full(config).await;
    let panel = Some("cms.localhost");

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, panel, "root-admin", "sup3r-secret!").await;

    let site = TenantSite::new("hot");
    site.write("index.html", "<p>Acme hot home</p>");
    site.write("about.jhs", "<p><?= \"rendered \" + (2 + 3) ?></p>");

    // Before the mapping, the would-be tenant host serves the main
    // tree — that is the F17 behaviour this test starts from.
    let response = get(&admin, &server, Some("acme.localhost"), "/").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Wallermax") && !body.contains("Acme hot home"),
        "the main tree answers before the mapping: {body}"
    );

    // Create the tenant and map its host name — both over the panel,
    // no restart anywhere.
    create_tenant(&admin, &server, panel, "acme", &site.root_string()).await;
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/domains",
        "hostname=ACME.localhost",
    )
    .await;
    assert_eq!(response.status(), 303, "the host name maps (normalized)");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants/acme?ok=domain-added");

    // The very next request on that host name serves the tenant's
    // tree — the live snapshot, not a boot-time capture.
    let response = get(&admin, &server, Some("acme.localhost"), "/").await;
    assert_eq!(response.status(), 200, "the tenant answers");
    let body = response.text().await.expect("body");
    assert!(body.contains("Acme hot home"), "the tenant's index: {body}");

    // The templates middleware classifies with the same live table:
    // the tenant's `.jhs` files render on the fly (never the source).
    let response = get(&admin, &server, Some("acme.localhost"), "/about.jhs").await;
    assert_eq!(response.status(), 200, "the .jhs renders");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("rendered 5") && !body.contains("<?jhs"),
        "rendered output, not the source: {body}"
    );

    // The row is data — `manual`, not the seeder's.
    let side = side_db(db.url()).await;
    let source: String =
        sqlx::query_scalar("SELECT source FROM domains WHERE hostname = 'acme.localhost'")
            .fetch_one(&side)
            .await
            .expect("domain row found");
    side.close().await;
    assert_eq!(source, "manual");

    // Unmapping is just as immediate: the host falls back to the
    // main tree for the next request.
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/domains/acme.localhost/delete",
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the host name unmaps");
    let response = get(&admin, &server, Some("acme.localhost"), "/").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Wallermax") && !body.contains("Acme hot home"),
        "the main tree is back: {body}"
    );
}

#[tokio::test]
async fn config_bootstrapped_host_names_are_protected() {
    let (mut config, db, _main) = single_host_config();
    config.cms.hosts = vec![String::from("cms.localhost")];
    let server = TestServer::start_full(config).await;
    let panel = Some("cms.localhost");

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, panel, "root-admin", "sup3r-secret!").await;

    // The seeder's own row cannot be unmapped from the panel — it
    // would come back on the next boot.
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/cms/domains/cms.localhost/delete",
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the unmapping is refused");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants/cms?error=domain-config");

    let side = side_db(db.url()).await;
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM domains WHERE hostname = 'cms.localhost'")
            .fetch_one(&side)
            .await
            .expect("domains counted");
    side.close().await;
    assert_eq!(rows, 1, "the row is still there");

    // And a configured host name cannot be claimed for another
    // organization: the form explains where the name lives.
    let site = TenantSite::new("protected");
    create_tenant(&admin, &server, panel, "acme", &site.root_string()).await;
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/domains",
        "hostname=cms.localhost",
    )
    .await;
    assert_eq!(response.status(), 200, "the claim is refused");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("[cms] hosts"),
        "the refusal points at the configuration: {body}"
    );
}

#[tokio::test]
async fn a_host_name_maps_to_one_organization() {
    let (mut config, _db, _main) = single_host_config();
    config.cms.hosts = vec![String::from("cms.localhost")];
    let server = TestServer::start_full(config).await;
    let panel = Some("cms.localhost");

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, panel, "root-admin", "sup3r-secret!").await;

    let acme = TenantSite::new("owner-a");
    let beta = TenantSite::new("owner-b");
    create_tenant(&admin, &server, panel, "acme", &acme.root_string()).await;
    create_tenant(&admin, &server, panel, "beta", &beta.root_string()).await;

    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/domains",
        "hostname=shared.example.com",
    )
    .await;
    assert_eq!(response.status(), 303, "the first mapping lands");

    // The second claim is refused, and the error names the owner.
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/beta/domains",
        "hostname=shared.example.com",
    )
    .await;
    assert_eq!(response.status(), 200, "the second claim is refused");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("`acme`"),
        "the refusal names the owning organization: {body}"
    );
}

#[tokio::test]
async fn memberships_stop_at_the_cms_mirror() {
    let (mut config, db, _main) = single_host_config();
    config.cms.hosts = vec![String::from("cms.localhost")];
    let server = TestServer::start_full(config).await;
    let panel = Some("cms.localhost");

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, panel, "root-admin", "sup3r-secret!").await;

    // A plain account for the tenant's team.
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/users",
        "username=penelope&password=password-123&role=user",
    )
    .await;
    assert_eq!(response.status(), 303, "user creation redirects");

    let site = TenantSite::new("members");
    create_tenant(&admin, &server, panel, "acme", &site.root_string()).await;

    // The CMS organization's memberships are the mirror's territory.
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/cms/members",
        "username=penelope&role=editor",
    )
    .await;
    assert_eq!(response.status(), 303, "the grant is refused");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin/tenants/cms?error=mirror");

    // A tenant's memberships are data: granted, listed, moved,
    // removed — all through the forms.
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/members",
        "username=penelope&role=editor",
    )
    .await;
    assert_eq!(response.status(), 303, "the grant lands");

    // An unknown account is refused with the form re-rendered.
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/members",
        "username=ghost&role=editor",
    )
    .await;
    assert_eq!(response.status(), 200, "the unknown account is refused");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("No account answers"),
        "the refusal is browser-facing: {body}"
    );

    // Re-adding with another role moves the member (the upsert).
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/members",
        "username=penelope&role=admin",
    )
    .await;
    assert_eq!(response.status(), 303, "the move lands");

    let side = side_db(db.url()).await;
    let penelope: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'penelope'")
        .fetch_one(&side)
        .await
        .expect("user exists");
    let role: String = sqlx::query_scalar(
        "SELECT m.role FROM memberships m JOIN organizations o ON o.id = m.organization_id \
         WHERE m.user_id = ?1 AND o.key = 'acme'",
    )
    .bind(penelope)
    .fetch_one(&side)
    .await
    .expect("membership found");
    assert_eq!(role, "admin", "the upsert moved the role");
    side.close().await;

    // The member list shows on the detail page.
    let response = get(&admin, &server, panel, "/admin/tenants/acme").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("penelope"), "the member is listed: {body}");

    // And the removal.
    let response = post_form(
        &admin,
        &server,
        panel,
        &format!("/admin/tenants/acme/members/{penelope}/delete"),
        "",
    )
    .await;
    assert_eq!(response.status(), 303, "the removal lands");
    let side = side_db(db.url()).await;
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memberships m JOIN organizations o ON o.id = m.organization_id \
         WHERE m.user_id = ?1 AND o.key = 'acme'",
    )
    .bind(penelope)
    .fetch_one(&side)
    .await
    .expect("memberships counted");
    side.close().await;
    assert_eq!(rows, 0, "the membership is gone");
}

#[tokio::test]
async fn tenant_admins_do_not_open_the_panel() {
    let (mut config, _db, _main) = single_host_config();
    config.cms.hosts = vec![String::from("cms.localhost")];
    let server = TestServer::start_full(config).await;
    let panel = Some("cms.localhost");

    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, panel, "root-admin", "sup3r-secret!").await;

    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/users",
        "username=plucky&password=password-123&role=user",
    )
    .await;
    assert_eq!(response.status(), 303, "user creation redirects");

    let site = TenantSite::new("boundary");
    create_tenant(&admin, &server, panel, "acme", &site.root_string()).await;
    let response = post_form(
        &admin,
        &server,
        panel,
        "/admin/tenants/acme/members",
        "username=plucky&role=admin",
    )
    .await;
    assert_eq!(response.status(), 303, "the grant lands");

    // The first sanctioned divergence, in one assertion: plucky is an
    // administrator of the acme organization — a membership that
    // mirrors no platform role — and the panel still refuses them,
    // because the guards read the CMS organization, nothing else.
    let plucky = login_browser(&server, panel, "plucky", "password-123").await;
    let response = get(&plucky, &server, panel, "/admin/tenants").await;
    assert_eq!(
        response.status(),
        403,
        "a tenant membership is not CMS access"
    );
    let response = get(&plucky, &server, panel, "/admin/pages").await;
    assert_eq!(response.status(), 403, "not even the content pages");
}
