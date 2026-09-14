//! The domains battery (F16): Host names as data.
//!
//! Boots the full server exactly like the binary (database, auth,
//! templates, static files and the CMS) over a fresh temporary SQLite
//! file and proves the virtual-host split reads the `domains` table,
//! not the configuration:
//!
//! - `[cms] hosts` is the **bootstrap**: the seeder keeps its own rows
//!   in step with the list (added when added, removed when removed);
//! - rows created by hand are **data**: they survive every boot and
//!   serve the CMS tree with the list empty — virtual hosting with
//!   zero configuration;
//! - rows pointing at other organizations are data too, but no tree
//!   serves them yet (the per-organization phases build those);
//! - the F14 static-root rule extends to the data plane: CMS domains
//!   without `static.enabled` refuse to boot.
//!
//! The double boots are the point: the host list loads at startup, so
//! a row only takes effect after one. Rows are seeded straight into
//! the SQLite file with a side connection between boots — the only
//! honest way to produce states the configuration never describes.

mod common;

use common::{auth_config, TestServer};
use reqwest::header::{HeaderValue, HOST};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;
use wallermax_server::config::AppConfig;
use wallermax_server::server::build_state;

/// The full stack over `url`, with `[cms] hosts` set to `hosts`.
fn domains_config(url: &str, hosts: &[&str]) -> AppConfig {
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

/// Asserts `host` serves the CMS tree: the views/ home, not the
/// static landing page.
async fn assert_cms_home(server: &TestServer, host: &str) {
    let response = get(server, "/", Some(host)).await;
    assert_eq!(response.status(), 200, "the CMS home renders on {host}");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Home — wallermax"),
        "the views/index.jhs home, not the static page: {body}"
    );
}

/// Asserts `host` (or the default host) serves the main tree: the
/// static landing page, not the CMS home.
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

/// Inserts a domain row for `hostname` straight into the file,
/// pointing at the organization `org` (manual by default: the INSERT
/// does not name `source`).
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

#[tokio::test]
async fn config_hosts_seed_the_table_and_serve() {
    let (_, db) = auth_config();
    let config = domains_config(db.url(), &["cms.test"]);
    let server = TestServer::start_full(config).await;

    assert_cms_home(&server, "cms.test").await;
    assert_static_home(&server, None).await;

    let pool = side_db(db.url()).await;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT hostname, source FROM domains ORDER BY hostname")
            .fetch_all(&pool)
            .await
            .expect("domains listed");
    assert_eq!(
        rows,
        vec![(String::from("cms.test"), String::from("config"))],
        "the bootstrap list seeded the table"
    );
}

#[tokio::test]
async fn manual_domains_serve_the_cms_without_any_configuration() {
    let (_, db) = auth_config();
    let config = domains_config(db.url(), &[]);
    {
        let server = TestServer::start_full(config.clone()).await;
        // Before the row exists there is no split at all: any host
        // gets the single-host tree, the static home included.
        assert_static_home(&server, Some("panel.other.test")).await;
    }

    let pool = side_db(db.url()).await;
    insert_domain(&pool, "panel.other.test", "cms").await;
    drop(pool);

    // The reboot loads the table: the host serves the CMS tree with
    // `[cms] hosts` empty — virtual hosting with zero configuration.
    let server = TestServer::start_full(config).await;
    assert_cms_home(&server, "panel.other.test").await;
    assert_static_home(&server, None).await;

    let response = get(&server, "/p", Some("panel.other.test")).await;
    assert_eq!(response.status(), 200, "the CMS surface answers");
    let response = get(&server, "/p", None).await;
    assert_eq!(response.status(), 404, "and only on the mapped host");
}

#[tokio::test]
async fn removing_a_host_from_the_configuration_stops_serving_it() {
    let (_, db) = auth_config();
    {
        let config = domains_config(db.url(), &["keep.test", "drop.test"]);
        let server = TestServer::start_full(config).await;
        assert_cms_home(&server, "drop.test").await;
    }

    // The reboot with a shorter list: the seeder prunes its own row
    // for the retired name — config edits behave exactly as in F14.
    let config = domains_config(db.url(), &["keep.test"]);
    let server = TestServer::start_full(config).await;
    assert_static_home(&server, Some("drop.test")).await;
    assert_cms_home(&server, "keep.test").await;

    let pool = side_db(db.url()).await;
    let rows: Vec<(String,)> = sqlx::query_as("SELECT hostname FROM domains")
        .fetch_all(&pool)
        .await
        .expect("domains listed");
    assert_eq!(rows, vec![(String::from("keep.test"),)]);
}

#[tokio::test]
async fn manual_domains_survive_configuration_changes() {
    let (_, db) = auth_config();
    {
        let config = domains_config(db.url(), &["cms.test"]);
        TestServer::start_full(config).await;
    }
    let pool = side_db(db.url()).await;
    insert_domain(&pool, "hand.test", "cms").await;
    drop(pool);

    // The hand-made row serves alongside the seeded one...
    let config = domains_config(db.url(), &["cms.test"]);
    let server = TestServer::start_full(config).await;
    assert_cms_home(&server, "hand.test").await;
    assert_cms_home(&server, "cms.test").await;

    // ...and even emptying `[cms] hosts` leaves it alone: only the
    // seeder's own rows follow the list. Data survives.
    drop(server);
    let config = domains_config(db.url(), &[]);
    let server = TestServer::start_full(config).await;
    assert_cms_home(&server, "hand.test").await;

    let pool = side_db(db.url()).await;
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT hostname, source FROM domains")
        .fetch_all(&pool)
        .await
        .expect("domains listed");
    assert_eq!(
        rows,
        vec![(String::from("hand.test"), String::from("manual"))],
        "the hand-made row is untouched"
    );
}

#[tokio::test]
async fn other_organizations_domains_get_the_main_tree_for_now() {
    let (_, db) = auth_config();
    {
        let config = domains_config(db.url(), &[]);
        TestServer::start_full(config).await;
    }
    let pool = side_db(db.url()).await;
    sqlx::query(
        "INSERT INTO organizations (key, name, document_root, created_at) \
         VALUES ('third', 'Third site', 'sites/third', 0)",
    )
    .execute(&pool)
    .await
    .expect("third organization inserted");
    insert_domain(&pool, "x.third.test", "third").await;
    drop(pool);

    let config = domains_config(db.url(), &[]);
    let server = TestServer::start_full(config).await;

    // F16 dispatches binary: the CMS tree or the main tree. The third
    // organization's domain is data (it survives boots), but nothing
    // serves it yet — the per-organization phases build trees from the
    // document roots. Until then it classifies as any other unknown
    // host: the main tree.
    assert_static_home(&server, Some("x.third.test")).await;
}

#[tokio::test]
async fn table_domains_need_the_static_root_like_config_hosts() {
    let (_, db) = auth_config();
    {
        let config = domains_config(db.url(), &[]);
        TestServer::start_full(config).await;
    }
    let pool = side_db(db.url()).await;
    insert_domain(&pool, "broken.test", "cms").await;
    drop(pool);

    // The same database with static serving off: the hosts list is
    // empty, so load-time validation has nothing to reject — the boot
    // check catches the table row instead (the F14 rule, extended to
    // the data plane: the CMS host borrows /assets from the static
    // root).
    let mut config = domains_config(db.url(), &[]);
    config.static_files.enabled = false;
    let message = match build_state(&config).await {
        Ok(_) => panic!("boot must refuse: the CMS host has no static root to borrow from"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("the domains table maps CMS hosts"),
        "the boot error explains the conflict: {message}"
    );
}
