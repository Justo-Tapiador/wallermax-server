//! Virtual-host integration battery (F14): one IP, one port, two names.
//!
//! Boots the **full** server (database, auth, templates, static files
//! and the CMS) with `cms.hosts = ["cms.test"]`, so the dispatcher
//! splits the route trees by the `Host` header — and then walks the
//! whole split: the CMS home vs. the static home, `.jhs` rendering
//! pinned to the main host, the admin gate pinned to the CMS host,
//! the API machinery pinned to the main host, the auth family alive
//! on both (the no-JS login forms need it), shared `/assets` on both,
//! case/port-insensitive matching, unknown hosts failing safe to the
//! main site, a full browser session that never leaves the CMS host,
//! sitemap URLs following the serving host — and, as the control, the
//! same configuration with empty `hosts` behaving exactly like the
//! single-host server of every phase before F14.

mod common;

use common::{auth_config, TestServer};
use reqwest::header::{HeaderValue, HOST};
use serde_json::json;
use wallermax_server::config::AppConfig;

/// The hostname the CMS answers on in this battery.
const CMS_HOST: &str = "cms.test";

/// The full stack with the CMS pinned to [`CMS_HOST`].
fn vhost_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    config.cms.hosts = vec![String::from(CMS_HOST)];
    config.metrics.enabled = true;
    (config, db)
}

/// The full stack with `cms.hosts` empty — the single-host control.
fn single_host_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = vhost_config();
    config.cms.hosts = Vec::new();
    (config, db)
}

/// A redirect-free client (each hop is asserted by hand).
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds")
}

/// A redirect-free, cookie-storing client — a browser.
fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .expect("browser client builds")
}

/// A `Host` header value for the tests.
fn host(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("test host value")
}

/// GETs `path` pinned to `host_value`, or with the client's default
/// host (the ephemeral `127.0.0.1:port` — a main-host request).
async fn get(server: &TestServer, path: &str, host_value: Option<&str>) -> reqwest::Response {
    let mut request = client().get(server.url(path));
    if let Some(value) = host_value {
        request = request.header(HOST, host(value));
    }
    request.send().await.expect("request")
}

/// Registers the bootstrap admin and logs a browser in through the
/// CMS host's own form endpoint — a session that never leaves the
/// CMS host, exactly like an editor's browser.
async fn cms_session(server: &TestServer) -> reqwest::Client {
    let session = browser();

    let response = session
        .post(server.url("/api/auth/register"))
        .header(HOST, host(CMS_HOST))
        .json(&json!({ "username": "justo", "password": "supersecret8" }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(
        response.status(),
        201,
        "the first account becomes the admin"
    );

    let response = session
        .post(server.url("/api/auth/login"))
        .header(HOST, host(CMS_HOST))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=justo&password=supersecret8&redirect=/")
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");

    session
}

/// Creates one published page through the panel forms (PRG), on the
/// CMS host.
async fn create_published_page(server: &TestServer, client: &reqwest::Client, slug: &str) {
    let response = client
        .post(server.url("/admin/pages"))
        .header(HOST, host(CMS_HOST))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "slug={slug}&title=Page&content=Hello%20world&redirect=%2Fadmin%2Fpages&is_published=on"
        ))
        .send()
        .await
        .expect("page creation succeeds");
    assert_eq!(response.status(), 303, "creation redirects (PRG)");
}

#[tokio::test]
async fn the_cms_host_serves_the_dynamic_home() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/", Some(CMS_HOST)).await;
    assert_eq!(response.status(), 200, "the CMS home renders");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Home — wallermax"),
        "the views/index.jhs home, not the static page: {body}"
    );
}

#[tokio::test]
async fn the_main_host_serves_the_static_home() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/", None).await;
    assert_eq!(response.status(), 200, "public/index.html answers");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Wallermax") && !body.contains("Home — wallermax"),
        "the static site, not the CMS home: {body}"
    );
}

#[tokio::test]
async fn public_jhs_templates_render_on_the_main_host_only() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/hello.jhs", None).await;
    assert_eq!(response.status(), 200, "the main host renders public/*.jhs");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Hello from a .jhs template"),
        "rendered output, never the raw source: {body}"
    );

    let response = get(&server, "/hello.jhs", Some(CMS_HOST)).await;
    assert_eq!(response.status(), 404, "public/ is the main host's root");
}

#[tokio::test]
async fn cms_routes_answer_only_on_the_cms_host() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    for path in [
        "/p",
        "/search?q=hello",
        "/feed.xml",
        "/atom.xml",
        "/sitemap.xml",
    ] {
        let response = get(&server, path, Some(CMS_HOST)).await;
        assert_eq!(
            response.status(),
            200,
            "the CMS surface answers on the CMS host: {path}"
        );
        let response = get(&server, path, None).await;
        assert_eq!(
            response.status(),
            404,
            "and only there — the main host has no CMS routes: {path}"
        );
    }

    let response = get(&server, "/buscar?q=hello", Some(CMS_HOST)).await;
    assert_eq!(response.status(), 301, "the Spanish alias redirects (F13)");

    let response = get(&server, "/feed.xml", Some(CMS_HOST)).await;
    let body = response.text().await.expect("feed body");
    assert!(body.contains("<rss"), "RSS 2.0 shape: {body}");
}

#[tokio::test]
async fn the_admin_gate_lives_on_the_cms_host() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/admin", Some(CMS_HOST)).await;
    assert_eq!(
        response.status(),
        303,
        "the editor gate redirects to the login page"
    );
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert!(
        location.starts_with("/login?redirect="),
        "browsers get a login page: {location}"
    );

    let response = get(&server, "/admin", None).await;
    assert_eq!(response.status(), 404, "no panel on the main host");
}

#[tokio::test]
async fn the_api_machinery_lives_on_the_main_host() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    for path in ["/api", "/health", "/metrics"] {
        let response = get(&server, path, None).await;
        assert_eq!(
            response.status(),
            200,
            "the operator machinery answers on the main host: {path}"
        );
        let response = get(&server, path, Some(CMS_HOST)).await;
        assert_eq!(response.status(), 404, "and stays off the CMS host: {path}");
    }
}

#[tokio::test]
async fn the_auth_family_answers_on_both_hosts() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    // `GET /api/auth/me` is unauthenticated on both — 401, not 404:
    // the family exists everywhere.
    for host_value in [None, Some(CMS_HOST)] {
        let response = get(&server, "/api/auth/me", host_value).await;
        assert_eq!(
            response.status(),
            401,
            "the auth family is mounted on both hosts"
        );
    }

    // The no-JS login form POSTs `/api/auth/login`; bogus credentials
    // redirect to the login page with the flash code — on both hosts,
    // because the CMS host's login page needs its own endpoint.
    for host_value in [None, Some(CMS_HOST)] {
        let mut request = client().post(server.url("/api/auth/login"));
        if let Some(value) = host_value {
            request = request.header(HOST, host(value));
        }
        let response = request
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("username=nobody&password=wrong-password&redirect=/")
            .send()
            .await
            .expect("form login attempt");
        assert_eq!(
            response.status(),
            303,
            "form semantics answer a redirect, not JSON"
        );
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("redirect target");
        assert!(
            location.contains("login_error"),
            "the English flash code rides the redirect: {location}"
        );
    }
}

#[tokio::test]
async fn host_matching_ignores_case_and_port() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/p", Some("CMS.TEST")).await;
    assert_eq!(response.status(), 200, "DNS names are case-insensitive");

    let response = get(&server, "/p", Some("cms.test:8443")).await;
    assert_eq!(response.status(), 200, "the port is not part of the name");

    let response = get(&server, "/p", Some("cms.test.evil.com")).await;
    assert_eq!(
        response.status(),
        404,
        "a longer name is a different host — no suffix matching"
    );
}

#[tokio::test]
async fn unknown_hosts_get_the_main_site() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/api", Some("evil.example")).await;
    assert_eq!(
        response.status(),
        200,
        "an unknown host fails safe to the main site"
    );
    let response = get(&server, "/p", Some("evil.example")).await;
    assert_eq!(response.status(), 404, "and never sees the CMS");
}

#[tokio::test]
async fn shared_assets_serve_on_both_hosts() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    for host_value in [None, Some(CMS_HOST)] {
        let response = get(&server, "/assets/wallermax.css", host_value).await;
        assert_eq!(
            response.status(),
            200,
            "the shared stylesheet is same-origin everywhere"
        );
        assert!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/css")),
            "stylesheets answer as text/css"
        );
    }

    let response = get(&server, "/assets/admin.css", Some(CMS_HOST)).await;
    assert_eq!(response.status(), 200, "the panel styles ride along");

    let response = get(&server, "/assets/missing.css", Some(CMS_HOST)).await;
    assert_eq!(response.status(), 404, "missing assets stay 404");

    // The rest of public/ belongs to the main host only.
    let response = get(&server, "/index.html", Some(CMS_HOST)).await;
    assert_eq!(
        response.status(),
        404,
        "the main site's content does not duplicate onto the CMS host"
    );
}

#[tokio::test]
async fn views_auto_route_only_on_the_cms_host() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/login", Some(CMS_HOST)).await;
    assert_eq!(response.status(), 200, "views/login.jhs auto-routes");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("action=\"/api/auth/login\""),
        "the login form posts to its own origin: {body}"
    );

    let response = get(&server, "/login", None).await;
    assert_eq!(
        response.status(),
        404,
        "the main host's 404s stay 404s — no views auto-routing there"
    );
}

#[tokio::test]
async fn a_browser_session_lives_entirely_on_the_cms_host() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;
    let session = cms_session(&server).await;

    // The panel opens for the authenticated browser — on the CMS host.
    let response = session
        .get(server.url("/admin"))
        .header(HOST, host(CMS_HOST))
        .send()
        .await
        .expect("panel request");
    assert_eq!(response.status(), 200, "the session opens the panel");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Recent activity"),
        "the F12 dashboard renders: {body}"
    );
}

#[tokio::test]
async fn the_theme_toggle_stays_on_the_cms_host() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;
    let session = cms_session(&server).await;

    let response = session
        .get(server.url("/admin/theme?to=dark&back=/admin"))
        .header(HOST, host(CMS_HOST))
        .send()
        .await
        .expect("toggle request");
    assert_eq!(response.status(), 303, "the toggle bounces straight back");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect target");
    assert_eq!(location, "/admin", "the validated back path");
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("the theme cookie is pinned");
    assert!(
        cookie.contains("wm_theme=dark"),
        "the no-JS dark mode pins its cookie: {cookie}"
    );

    let response = get(&server, "/admin/theme?to=dark&back=/admin", None).await;
    assert_eq!(response.status(), 404, "no toggle on the main host");
}

#[tokio::test]
async fn sitemap_urls_follow_the_serving_host() {
    let (config, _db) = vhost_config();
    let server = TestServer::start_full(config).await;
    let session = cms_session(&server).await;
    create_published_page(&server, &session, "vhosted").await;

    let response = get(&server, "/sitemap.xml", Some(CMS_HOST)).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("sitemap body");
    assert!(
        body.contains("http://cms.test/"),
        "absolute <loc> URLs build from the request's host: {body}"
    );
}

#[tokio::test]
async fn empty_hosts_behave_as_the_single_host() {
    let (config, _db) = single_host_config();
    let server = TestServer::start_full(config).await;

    // Everything on one host, exactly as before F14: the static home,
    // the rendered demo template, the CMS listing and the API index.
    let response = get(&server, "/", None).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("Wallermax"), "the static home: {body}");

    let response = get(&server, "/hello.jhs", None).await;
    assert_eq!(response.status(), 200, "public .jhs renders");

    let response = get(&server, "/p", None).await;
    assert_eq!(response.status(), 200, "the CMS listing answers");

    let response = get(&server, "/api", None).await;
    assert_eq!(response.status(), 200, "the service index answers");
}
