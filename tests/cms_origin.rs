//! Integration battery for the `cms_origin` template global (F14
//! follow-up): the cross-host link origin.
//!
//! Boots the real stacks against a fixture tree whose main-host
//! template echoes `<?= cms_origin ?>/login` — the exact shape a
//! `public/` navigation link takes — and walks the whole contract:
//! the single-host server rendering the empty origin (relative
//! links; `site_url` ignored while every surface shares one host),
//! the derived origin from the first `cms.hosts` entry (scheme from
//! `[tls]`, a non-default `[server] port` tagging along, the
//! OS-picked ephemeral port omitted), the `cms.site_url` override,
//! and the CMS host's own templates seeing the same origin the main
//! site links to.

mod common;

use std::path::PathBuf;

use common::{auth_config, tls_test_client, TestServer, TlsTestServer};
use reqwest::header::{HeaderValue, HOST};
use wallermax_server::config::AppConfig;

/// The hostname the CMS answers on in this battery.
const CMS_HOST: &str = "cms.test";

/// Fixture tree: a static root with the echoing template, a views
/// directory with its own, and the modules dir the engine wants.
struct FixtureDir {
    path: PathBuf,
}

impl FixtureDir {
    fn create(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("wallermax-cms-origin-{tag}-{}", std::process::id()));
        let static_root = path.join("static");
        let views = path.join("views");
        let modules = path.join("modules");
        std::fs::create_dir_all(&static_root).expect("static root dir");
        std::fs::create_dir_all(&views).expect("views dir");
        std::fs::create_dir_all(&modules).expect("modules dir");
        std::fs::write(static_root.join("origin.jhs"), MAIN_TEMPLATE).expect("main template write");
        std::fs::write(views.join("origin.jhs"), VIEW_TEMPLATE).expect("view template write");
        Self { path }
    }

    /// Single-host configuration: static + templates, no CMS split.
    fn config(&self) -> AppConfig {
        let mut config = AppConfig::default();
        config.static_files.enabled = true;
        config.static_files.root_dir = self.static_root();
        config.templates.enabled = true;
        config.templates.views_dir = self.views_dir();
        config.templates.modules_dir = self.modules_dir();
        config
    }

    /// Vhost configuration (the full stack): the CMS pinned to
    /// [`CMS_HOST`], this tree behind the main host, `port` telling
    /// the `[server] port` story under test.
    fn vhost_config(&self, port: u16) -> (AppConfig, common::TempDbGuard) {
        let (mut config, db) = auth_config();
        config.static_files.enabled = true;
        config.static_files.root_dir = self.static_root();
        config.templates.enabled = true;
        config.templates.views_dir = self.views_dir();
        config.templates.modules_dir = self.modules_dir();
        config.cms.enabled = true;
        config.cms.hosts = vec![String::from(CMS_HOST)];
        config.server.port = port;
        (config, db)
    }

    /// Forward-slash path (valid on Windows as well).
    fn static_root(&self) -> String {
        self.path
            .join("static")
            .display()
            .to_string()
            .replace('\\', "/")
    }

    /// Forward-slash path (valid on Windows as well).
    fn views_dir(&self) -> String {
        self.path
            .join("views")
            .display()
            .to_string()
            .replace('\\', "/")
    }

    /// Forward-slash path (valid on Windows as well).
    fn modules_dir(&self) -> String {
        self.path
            .join("modules")
            .display()
            .to_string()
            .replace('\\', "/")
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The main host's template: a navigation link exactly as a
/// `public/` template writes it.
const MAIN_TEMPLATE: &str = "<nav><?= cms_origin ?>/login</nav>";

/// The CMS host's own template, echoing the bare global.
const VIEW_TEMPLATE: &str = "<p><?= cms_origin ?></p>";

/// A `Host` header value for the tests.
fn host(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("test host value")
}

/// The rendered body of `path` — a main-host request by default (the
/// client's ephemeral `127.0.0.1:port` Host), `Some(value)` to pin
/// another host.
async fn body(server: &TestServer, path: &str, host_value: Option<&str>) -> String {
    let mut request = reqwest::Client::new().get(server.url(path));
    if let Some(value) = host_value {
        request = request.header(HOST, host(value));
    }
    let response = request.send().await.expect("request succeeds");
    assert_eq!(response.status(), 200, "the template renders");
    response.text().await.expect("body text")
}

// ── The single host: relative links, no origin ────────────────────

#[tokio::test]
async fn the_single_host_links_relatively() {
    let fixture = FixtureDir::create("single");
    let server = TestServer::start_with_config(fixture.config()).await;

    assert_eq!(
        body(&server, "/origin.jhs", None).await,
        "<nav>/login</nav>"
    );
}

#[tokio::test]
async fn site_url_does_not_create_an_origin_while_single_host() {
    let fixture = FixtureDir::create("site-url");
    let mut config = fixture.config();
    config.cms.site_url = Some(String::from("https://www.example.com"));
    let server = TestServer::start_with_config(config).await;

    // `site_url` is the feeds' key, not a link target: while every
    // surface shares one host the browser is already on the right
    // origin, and relative links stay correct.
    assert_eq!(
        body(&server, "/origin.jhs", None).await,
        "<nav>/login</nav>"
    );
}

// ── Virtual hosts: the derived origin ─────────────────────────────

#[tokio::test]
async fn vhosts_derive_the_first_host_as_the_origin() {
    let fixture = FixtureDir::create("derive");
    // Port 0 is the OS-picked ephemeral port test boots bind —
    // nobody publishes a link to it, so the origin omits it.
    let (config, _db) = fixture.vhost_config(0);
    let server = TestServer::start_full(config).await;

    assert_eq!(
        body(&server, "/origin.jhs", None).await,
        format!("<nav>http://{CMS_HOST}/login</nav>")
    );
}

#[tokio::test]
async fn non_default_ports_tag_along() {
    let fixture = FixtureDir::create("port");
    let (config, _db) = fixture.vhost_config(8080);
    let server = TestServer::start_full(config).await;

    assert_eq!(
        body(&server, "/origin.jhs", None).await,
        format!("<nav>http://{CMS_HOST}:8080/login</nav>")
    );
}

#[tokio::test]
async fn tls_lends_the_https_scheme() {
    let fixture = FixtureDir::create("tls");
    let (mut config, _db) = fixture.vhost_config(443);
    // The serving socket speaks TLS (the test server generates its
    // own certificate), so `[tls] enabled` is the truth the origin
    // derives its scheme from.
    config.tls.enabled = true;
    let server = TlsTestServer::start_full(config).await;

    let response = tls_test_client()
        .get(server.url("/origin.jhs"))
        .send()
        .await
        .expect("https request succeeds");
    assert_eq!(response.status(), 200, "the template renders");
    assert_eq!(
        response.text().await.expect("body text"),
        format!("<nav>https://{CMS_HOST}/login</nav>")
    );
}

#[tokio::test]
async fn site_url_overrides_the_derived_origin() {
    let fixture = FixtureDir::create("override");
    let (mut config, _db) = fixture.vhost_config(8080);
    config.cms.site_url = Some(String::from("https://cms.example.com"));
    let server = TestServer::start_full(config).await;

    // Behind a reverse proxy the derived origin would be wrong; the
    // operator's declared origin wins, exactly as in the sitemap.
    assert_eq!(
        body(&server, "/origin.jhs", None).await,
        "<nav>https://cms.example.com/login</nav>"
    );
}

// ── The CMS host's own templates ──────────────────────────────────

#[tokio::test]
async fn cms_host_templates_see_the_same_origin() {
    let fixture = FixtureDir::create("cms-view");
    let (config, _db) = fixture.vhost_config(0);
    let server = TestServer::start_full(config).await;

    // The views auto-routing reaches `views/origin.jhs` on the CMS
    // host — the same `base_data` globals, so the CMS's own templates
    // and the main site's links can never disagree about the origin.
    assert_eq!(
        body(&server, "/origin", Some(CMS_HOST)).await,
        format!("<p>http://{CMS_HOST}</p>")
    );
}
