//! The tenant battery (F17): per-organization serving.
//!
//! Boots the full server exactly like the binary (database, auth,
//! templates, static files and the CMS) over a fresh temporary SQLite
//! file and proves the dispatch resolves Host names against the
//! `domains` table joined to the `organizations` rows:
//!
//! - a third organization — created by hand, its `document_root` its
//!   own data — serves a **self-contained static site** on its mapped
//!   host names: the file tree, the directory indexes, the on-the-fly
//!   `.jhs` rendering, and nothing else (no API, no panel, no
//!   borrowed assets from the main root);
//! - several host names of one organization share its tree;
//! - the bootstrap organizations keep serving exactly what the
//!   configuration says (the seed keeps their rows in step) — the
//!   zero-visible-change story in one sweep;
//! - a tenant works with `[cms] hosts` empty and even with
//!   `[static] enabled` off: tenant trees are data, not configuration;
//! - a missing tenant root is a warning, not a boot failure — the
//!   tree answers 404s until the directory appears;
//! - `cms_origin` derives from the `domains` table (the first CMS
//!   row), no configuration involved.
//!
//! The double boots are the point: the bindings load at startup, so
//! a row only takes effect after one. Rows and organizations are
//! seeded straight into the SQLite file with a side connection
//! between boots — the only honest way to produce states the
//! configuration never describes.

mod common;

use common::{auth_config, TestServer};
use reqwest::header::{HeaderValue, HOST};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use wallermax_server::config::AppConfig;

/// The full stack over `url`, with `[cms] hosts` set to `hosts`.
fn tenant_config(url: &str, hosts: &[&str]) -> AppConfig {
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
    config.cms.hosts = hosts.iter().map(|host| String::from(*host)).collect();
    config
}

/// A redirect-free client (each response is asserted by hand).
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds")
}

/// GETs `path` pinned to `host_value` (or with the client's default
/// host — the ephemeral `127.0.0.1:port`, a main-host request).
async fn get(server: &TestServer, path: &str, host_value: Option<&str>) -> reqwest::Response {
    let mut request = client().get(server.url(path));
    if let Some(value) = host_value {
        request = request.header(HOST, HeaderValue::from_str(value).expect("test host value"));
    }
    request.send().await.expect("request")
}

/// Asserts `host` (or the default host) serves the main tree: the
/// static landing page, not a tenant's or the CMS's content.
async fn assert_static_home(server: &TestServer, host: Option<&str>) {
    let response = get(server, "/", host).await;
    assert_eq!(response.status(), 200, "the static home answers");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Wallermax") && !body.contains("Home — wallermax"),
        "the static site, not the CMS home: {body}"
    );
}

/// Opens a side connection to the test's SQLite file, with the same
/// busy timeout the server itself uses (see the organizations suite).
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

/// Inserts an organization row straight into the file — the shape of
/// a third tenant created by hand (F17's SQL cookbook).
async fn insert_organization(pool: &sqlx::sqlite::SqlitePool, key: &str, document_root: &str) {
    sqlx::query(
        "INSERT INTO organizations (key, name, document_root, created_at) \
         VALUES (?1, ?2, ?3, 0)",
    )
    .bind(key)
    .bind(format!("{} site", key))
    .bind(document_root)
    .execute(pool)
    .await
    .expect("organization row inserted");
}

/// Inserts a domain row for `hostname` straight into the file,
/// pointing at the organization `org`.
async fn insert_domain(pool: &sqlx::sqlite::SqlitePool, hostname: &str, org: &str) {
    sqlx::query(
        "INSERT INTO domains (hostname, organization_id, created_at) \
         SELECT ?1, id, 0 FROM organizations WHERE key = ?2",
    )
    .bind(hostname)
    .bind(org)
    .execute(pool)
    .await
    .expect("domain row inserted");
}

/// A throwaway directory tree standing in for one tenant's site,
/// removed (best-effort) on drop.
struct TenantSite {
    root: std::path::PathBuf,
}

static SITE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TenantSite {
    /// A fresh, existing root directory.
    fn new(tag: &str) -> Self {
        let unique = SITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wallermax-tenant-{}-{tag}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("tenant root created");
        Self { root }
    }

    /// A path that does not exist (a missing document root).
    fn missing(tag: &str) -> Self {
        let unique = SITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wallermax-tenant-{}-{tag}-missing-{unique}",
            std::process::id()
        ));
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

/// Boots once (seeding the organizations), then inserts the tenant
/// organization and its domain, then reboots — the double boot every
/// data-driven test needs.
async fn boot_with_tenant(
    config: AppConfig,
    org: &str,
    document_root: &str,
    hostnames: &[&str],
) -> TestServer {
    {
        let server = TestServer::start_full(config.clone()).await;
        drop(server);
    }
    let pool = side_db(&config.database.url).await;
    insert_organization(&pool, org, document_root).await;
    for hostname in hostnames {
        insert_domain(&pool, hostname, org).await;
    }
    drop(pool);
    TestServer::start_full(config).await
}

#[tokio::test]
async fn a_third_organization_serves_its_own_tree() {
    let (_, db) = auth_config();
    let site = TenantSite::new("third");
    site.write("index.html", "<h1>Third site</h1>");
    site.write("docs/guide.html", "<p>Guide</p>");

    let config = tenant_config(db.url(), &[]);
    let server = boot_with_tenant(config, "third", &site.root_string(), &["x.third.test"]).await;

    // The mapped host serves the tenant's own tree...
    let response = get(&server, "/", Some("x.third.test")).await;
    assert_eq!(response.status(), 200, "the tenant home answers");
    let body = response.text().await.expect("body");
    assert!(body.contains("Third site"), "the tenant's index: {body}");

    let response = get(&server, "/docs/guide.html", Some("x.third.test")).await;
    assert_eq!(
        response.status(),
        200,
        "nested files resolve inside the root"
    );

    // ...while the main tree keeps the static site — for the default
    // host and for unknown names alike.
    assert_static_home(&server, None).await;
    assert_static_home(&server, Some("unknown.test")).await;

    // The main host's own files do not leak onto the tenant host:
    // `public/hello.jhs` is the main tree's content.
    let response = get(&server, "/hello.jhs", Some("x.third.test")).await;
    assert_eq!(
        response.status(),
        404,
        "main-host files stay on the main host"
    );
}

#[tokio::test]
async fn tenant_trees_render_their_jhs_files() {
    let (_, db) = auth_config();
    let site = TenantSite::new("jhs");
    site.write("index.html", "<h1>static fallback</h1>");
    site.write("index.jhs", "<h1><?= 'rendered home' ?></h1>");
    site.write("page.jhs", "<p><?= 6 * 7 ?></p>");
    site.write("docs/index.jhs", "<nav><?= 'docs index' ?></nav>");

    let config = tenant_config(db.url(), &[]);
    let server = boot_with_tenant(config, "jhs", &site.root_string(), &["jhs.third.test"]).await;

    // The dynamic index takes the directory, exactly like the main
    // host's `index.jhs` rule.
    let response = get(&server, "/", Some("jhs.third.test")).await;
    assert_eq!(response.status(), 200, "the rendered home answers");
    let body = response.text().await.expect("body");
    assert!(body.contains("rendered home"), "the .jhs renders: {body}");
    assert!(!body.contains("<?jhs"), "never the raw source");

    // Explicit .jhs requests render...
    let response = get(&server, "/page.jhs", Some("jhs.third.test")).await;
    assert_eq!(response.status(), 200, "the page renders");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("<p>42</p>"),
        "the expression evaluated: {body}"
    );
    assert!(!body.contains("<?jhs"), "never the raw source");

    // ...and directory requests render the directory's index.jhs.
    let response = get(&server, "/docs/", Some("jhs.third.test")).await;
    assert_eq!(response.status(), 200, "the directory index renders");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("docs index"),
        "the nested .jhs renders: {body}"
    );
}

#[tokio::test]
async fn tenant_trees_are_self_contained() {
    let (_, db) = auth_config();
    let site = TenantSite::new("solo");
    site.write("index.html", "<h1>Solo site</h1>");

    let config = tenant_config(db.url(), &[]);
    let server = boot_with_tenant(config, "solo", &site.root_string(), &["solo.third.test"]).await;

    // F20: the shared panel chrome serves on the tenant host as a
    // fallback — a tenant that ships its own file wins (its root is
    // the first serve), and one that does not still gets the default
    // look instead of a bare 404.
    site.write("assets/wallermax.css", "/* the tenant's own look */");
    let response = get(&server, "/assets/wallermax.css", Some("solo.third.test")).await;
    assert_eq!(response.status(), 200, "the tenant's own asset wins first");
    let body = response.text().await.expect("body");
    assert_eq!(
        body, "/* the tenant's own look */",
        "served from the tenant root: {body}"
    );
    let response = get(&server, "/assets/admin.css", Some("solo.third.test")).await;
    assert_eq!(
        response.status(),
        200,
        "the shared panel chrome falls through (F20)"
    );
    let response = get(&server, "/assets/wallermax.css", None).await;
    assert_eq!(response.status(), 200, "the main host keeps its own assets");

    // No operator machinery: /api, /health and /metrics are the main
    // tree's. The auth family is the exception F20 sanctions — the
    // tenant's pages need the same-origin login form and its POST
    // target, exactly like the CMS host's.
    for path in ["/api", "/health", "/metrics"] {
        let response = get(&server, path, Some("solo.third.test")).await;
        assert_eq!(response.status(), 404, "{path} stays off the tenant host");
    }
    let response = get(&server, "/api/auth/me", Some("solo.third.test")).await;
    assert_eq!(
        response.status(),
        401,
        "the auth family rides along (F20) — anonymous, but mounted"
    );
    let response = get(&server, "/login", Some("solo.third.test")).await;
    assert_eq!(
        response.status(),
        200,
        "the no-JS login page serves on the tenant host (F20)"
    );
    let response = get(&server, "/health", None).await;
    assert_eq!(response.status(), 200, "the main host keeps its machinery");

    // The misses answer the standard JSON 404 envelope, like every
    // other tree.
    let response = get(&server, "/missing.txt", Some("solo.third.test")).await;
    assert_eq!(response.status(), 404, "misses are 404s");
    assert!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json")),
        "the JSON envelope, not an HTML page"
    );
}

#[tokio::test]
async fn two_hostnames_of_one_organization_share_the_tree() {
    let (_, db) = auth_config();
    let site = TenantSite::new("shared");
    site.write("index.html", "<h1>Shared tree</h1>");

    let config = tenant_config(db.url(), &[]);
    let server = boot_with_tenant(
        config,
        "shared",
        &site.root_string(),
        &["a.shared.test", "b.shared.test"],
    )
    .await;

    for host in ["a.shared.test", "b.shared.test"] {
        let response = get(&server, "/", Some(host)).await;
        assert_eq!(response.status(), 200, "the tree answers on {host}");
        let body = response.text().await.expect("body");
        assert!(
            body.contains("Shared tree"),
            "the same tree on {host}: {body}"
        );
    }
}

#[tokio::test]
async fn tenants_do_not_need_the_static_switch() {
    let (_, db) = auth_config();
    let site = TenantSite::new("noswitch");
    site.write("index.html", "<h1>Data, not configuration</h1>");

    let mut config = tenant_config(db.url(), &[]);
    config.static_files.enabled = false;
    let server = boot_with_tenant(
        config,
        "noswitch",
        &site.root_string(),
        &["x.noswitch.test"],
    )
    .await;

    // `[static] enabled` governs the MAIN organization's static
    // surface: with it off, the main host answers the JSON service
    // index at `/` — while the tenant tree serves its files, because
    // the rows are data and the phase's point is tenants without
    // touching wallermax.toml.
    let response = get(&server, "/", Some("x.noswitch.test")).await;
    assert_eq!(response.status(), 200, "the tenant home answers");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Data, not configuration"),
        "the tenant site: {body}"
    );

    let response = get(&server, "/", None).await;
    assert_eq!(response.status(), 200, "the main host answers");
    assert!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json")),
        "the JSON service index, not a static page"
    );
}

#[tokio::test]
async fn a_missing_tenant_root_boots_with_the_visitor_surface() {
    let (_, db) = auth_config();
    let site = TenantSite::missing("ghost");

    let config = tenant_config(db.url(), &[]);
    // The boot must not fail: a missing tenant root is a warning
    // (the row is data; the tree fills in when the directory appears).
    let server = boot_with_tenant(config, "ghost", &site.root_string(), &["x.ghost.test"]).await;

    // F20: a tenant host carries the CMS visitor surface even with no
    // files of its own — `/` renders the shared homepage view (the
    // same one the CMS host serves), scoped to the tenant's (empty)
    // content, and plain file misses still answer the JSON 404.
    let response = get(&server, "/", Some("x.ghost.test")).await;
    assert_eq!(response.status(), 200, "the shared homepage view answers");
    let response = get(&server, "/anything.txt", Some("x.ghost.test")).await;
    assert_eq!(
        response.status(),
        404,
        "file misses answer 404 from the empty tree"
    );
    // And the rest of the server is unaffected.
    assert_static_home(&server, None).await;
}

#[tokio::test]
async fn the_bootstrap_organizations_still_serve_their_configured_roots() {
    let (_, db) = auth_config();
    let site = TenantSite::new("beside");
    site.write("index.html", "<h1>Third site</h1>");

    // The classic F14 split (`[cms] hosts` names the CMS host) beside
    // a data-created tenant: main and CMS keep serving exactly what
    // the configuration says — the seed keeps their rows in step, so
    // routing the serving path through the organizations table
    // changes nothing.
    let config = tenant_config(db.url(), &["cms.test"]);
    let server = boot_with_tenant(config, "beside", &site.root_string(), &["x.beside.test"]).await;

    assert_static_home(&server, None).await;

    let response = get(&server, "/", Some("cms.test")).await;
    assert_eq!(response.status(), 200, "the CMS home renders");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Home — wallermax"),
        "the views/index.jhs home, not the static page: {body}"
    );

    let response = get(&server, "/", Some("x.beside.test")).await;
    assert_eq!(response.status(), 200, "the tenant home answers");
    let body = response.text().await.expect("body");
    assert!(body.contains("Third site"), "the tenant's own tree: {body}");
}

#[tokio::test]
async fn cms_origin_derives_from_the_domains_table() {
    let (_, db) = auth_config();

    // A views directory of our own, with a view that echoes the
    // global — the observable surface of the derivation. It is
    // reached through the 404 auto-routing (`GET /origin`), which
    // serves the single-host server and the CMS host alike.
    let views = TenantSite::new("origin-views");
    views.write("origin.jhs", "<p><?= cms_origin ?></p>");

    let mut config = tenant_config(db.url(), &[]);
    config.templates.views_dir = views.root_string();
    // Port 0 is the OS-picked ephemeral port test boots bind —
    // nobody publishes a link to it, so the derived origin omits it
    // (the port behaviour is the cms_origin suite's to pin).
    config.server.port = 0;

    {
        let server = TestServer::start_full(config.clone()).await;
        // Before the row exists: single-host mode, no mapped host, no
        // origin — the relative-link world of the unsplit server.
        let response = get(&server, "/origin", None).await;
        assert_eq!(response.status(), 200, "the view renders");
        let body = response.text().await.expect("body");
        assert_eq!(body, "<p></p>", "no mapped host, no origin");
    }

    let pool = side_db(db.url()).await;
    insert_domain(&pool, "panel.data.test", "cms").await;
    drop(pool);

    // The reboot loads the table: the origin derives from the first
    // CMS row (insertion order) — zero configuration involved.
    let server = TestServer::start_full(config).await;
    let response = get(&server, "/origin", Some("panel.data.test")).await;
    assert_eq!(
        response.status(),
        200,
        "the view renders on the mapped host"
    );
    let body = response.text().await.expect("body");
    assert_eq!(
        body, "<p>http://panel.data.test</p>",
        "the origin comes from the domains table"
    );
}
