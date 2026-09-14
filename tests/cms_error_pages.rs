//! F13 integration battery: the shared HTML error page and the
//! anglicized public surface.
//!
//! The error middleware negotiates on `Accept`: browser navigations
//! (text/html) get the styled, script-free English error page; API
//! clients (application/json, `*/*`, no header at all) keep the JSON
//! envelope byte-for-byte. These tests pin the three faces of that
//! contract — the swap, the JSON passthrough and the 429 self-heal —
//! plus the legacy Spanish URLs that now answer redirects.

mod common;

use common::{auth_config, TestServer};
use serde_json::{json, Value};
use wallermax_server::config::AppConfig;

/// A full-CMS configuration on a fresh database.
fn cms_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    (config, db)
}

/// The `Accept` header a browser sends on a navigation.
const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/// A redirect-free client (each hop asserted by hand).
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .expect("client builds")
}

/// Registers the bootstrap admin over the JSON API and returns a
/// logged-in, cookie-storing client.
async fn admin_browser(server: &TestServer) -> reqwest::Client {
    let registration = client()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "root-admin", "password": "sup3r-secret!" }))
        .send()
        .await
        .expect("registration");
    assert_eq!(registration.status(), 201, "the bootstrap admin registers");

    let browser = client();
    let response = browser
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=root-admin&password=sup3r-secret!&redirect=/admin")
        .send()
        .await
        .expect("login");
    assert_eq!(response.status(), 303, "the browser session starts");
    browser
}

// ── Content negotiation ──────────────────────────────────────────────

#[tokio::test]
async fn browsers_get_the_html_error_page_on_a_404() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;

    let response = client()
        .get(server.url("/no-such-page"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404, "the status is kept verbatim");

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .expect("content type")
        .to_owned();
    assert!(
        content_type.starts_with("text/html"),
        "the envelope became a page: {content_type}"
    );

    let body = response.text().await.expect("html body");
    assert!(body.contains("<html lang=\"en\""), "English page: {body}");
    assert!(body.contains("Page not found"), "friendly title: {body}");
    assert!(body.contains("NOT_FOUND"), "the code tag: {body}");
    assert!(body.contains("assets/error.css"), "the stylesheet: {body}");
    assert!(
        body.contains("href=\"/admin\">CMS panel"),
        "the way-out links: {body}"
    );
    assert!(
        body.contains("Request id</dt>"),
        "the correlation id block: {body}"
    );
    assert!(
        body.contains("served without a line of JavaScript"),
        "the brand line: {body}"
    );
}

#[tokio::test]
async fn api_clients_keep_the_json_envelope() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;

    // No Accept header at all (curl's default when none is passed).
    for accept in ["*/*", "application/json", ""] {
        let mut request = client().get(server.url("/no-such-page"));
        if !accept.is_empty() {
            request = request.header("Accept", accept);
        }
        let response = request.send().await.expect("request");
        assert_eq!(response.status(), 404, "accept {accept:?}");

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .expect("content type");
        assert!(
            content_type.starts_with("application/json"),
            "the envelope survives accept {accept:?}: {content_type}"
        );

        let body: Value = response.json().await.expect("envelope json");
        assert_eq!(body["error"]["code"], "NOT_FOUND");
        assert!(body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("/no-such-page"));
    }
}

#[tokio::test]
async fn stylesheet_and_image_requests_never_become_pages() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;

    let response = client()
        .get(server.url("/no-such-page"))
        .header("Accept", "image/avif,image/webp,image/png,*/*;q=0.8")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .expect("content type");
    assert!(
        content_type.starts_with("application/json"),
        "subresources keep the envelope: {content_type}"
    );
}

#[tokio::test]
async fn errors_on_other_media_types_pass_through_untouched() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;

    // A 405 on a JSON API route with a browser Accept stays operational
    // JSON only when it is the envelope; /api/auth/login is a POST-only
    // route whose 405 IS the envelope — a browser gets the page.
    let response = client()
        .get(server.url("/api/auth/login"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 405);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("Method not allowed"),
        "405 pages render for browsers: {body}"
    );
}

#[tokio::test]
async fn the_403_privilege_error_is_a_page_for_browsers() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;

    // The bootstrap admin first, so the second account is a plain user.
    client()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "root-admin", "password": "sup3r-secret!" }))
        .send()
        .await
        .expect("bootstrap registration");

    client()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "plain-user", "password": "password-123" }))
        .send()
        .await
        .expect("plain registration");

    let browser = client();
    browser
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=plain-user&password=password-123&redirect=/")
        .send()
        .await
        .expect("login");

    let response = browser
        .get(server.url("/admin"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 403);
    let body = response.text().await.expect("html body");
    assert!(body.contains("Access denied"), "title: {body}");
    assert!(body.contains("CMS membership required"), "message: {body}");
}

// ── The 429 self-heal ────────────────────────────────────────────────

#[tokio::test]
async fn the_rate_limit_429_self_heals_for_browsers() {
    let (mut config, _db) = cms_config();
    config.middleware.rate_limit = true;
    config.rate_limit.capacity = 2;
    config.rate_limit.refill_per_second = 0.001;
    let server = TestServer::start_full(config).await;
    let browser = client();

    // Drain the budget.
    for _ in 0..3 {
        let response = browser
            .get(server.url("/login"))
            .header("Accept", BROWSER_ACCEPT)
            .send()
            .await
            .expect("request");
        assert!(response.status().is_success() || response.status() == 429);
    }

    let response = browser
        .get(server.url("/admin/theme?to=dark&back=/admin"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("throttled toggle");
    assert_eq!(response.status(), 429, "the burst limiter tripped");

    // The limiter's headers survive the swap.
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .expect("retry-after kept");
    assert!(!retry_after.is_empty());

    let body = response.text().await.expect("html body");
    assert!(body.contains("Slow down for a moment"), "title: {body}");
    assert!(
        body.contains("<meta http-equiv=\"refresh\""),
        "the page reloads itself: {body}"
    );
    assert!(
        body.contains("This page reloads itself"),
        "the plain-language note: {body}"
    );
    // The swap must never leak the envelope to a browser.
    assert!(!body.contains("{\"error\""), "no raw JSON: {body}");
}

#[tokio::test]
async fn the_error_page_honors_the_pinned_theme() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    let browser = admin_browser(&server).await;

    // Pin the dark theme like the panel toggle does.
    let response = browser
        .get(server.url("/admin/theme?to=dark&back=/admin"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("toggle");
    assert_eq!(response.status(), 303, "the toggle bounces");

    // The next envelope error renders with the editor's pinned theme.
    let response = browser
        .get(server.url("/no-such-page"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<html lang=\"en\" data-theme=\"dark\""),
        "the panel theme carries over: {body}"
    );
}

// ── The anglicized routes and their legacy spellings ─────────────────

#[tokio::test]
async fn legacy_spanish_urls_redirect_to_the_english_ones() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    let anon = client();

    // GET pages: permanent redirects, query preserved.
    for (old, new) in [
        ("/registro", "/register"),
        ("/perfil", "/profile"),
        ("/contacto", "/contact"),
        ("/buscar?q=hello+world", "/search?q=hello+world"),
    ] {
        let response = anon
            .get(server.url(old))
            .header("Accept", BROWSER_ACCEPT)
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), 301, "redirect for {old}");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("location");
        assert_eq!(location, new, "the query survives: {old}");
    }

    // The password form: a 307 so the POST replays at the new path.
    let response = anon
        .post(server.url("/perfil/password"))
        .header("Accept", BROWSER_ACCEPT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("current_password=old&new_password=new-password-9")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 307);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("location");
    assert_eq!(location, "/profile/password");
}

#[tokio::test]
async fn the_english_routes_serve_the_english_views() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    let anon = client();

    let response = anon
        .get(server.url("/login"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("<h1>Sign in</h1>"), "login view: {body}");
    assert!(body.contains("href=\"#register\""), "register link: {body}");

    let response = anon
        .get(server.url("/register"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<h1>Create an account</h1>"),
        "register view: {body}"
    );

    let response = anon
        .get(server.url("/search"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("<h1>Search</h1>"), "search view: {body}");
    assert!(
        body.contains("action=\"/search\""),
        "the form posts to the English route: {body}"
    );

    let response = anon
        .get(server.url("/contact"))
        .header("Accept", BROWSER_ACCEPT)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("<h1>Contact</h1>"), "contact view: {body}");
}

// ── Feeds ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_feeds_speak_english() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    let admin = admin_browser(&server).await;

    // One published page so the feeds have an entry.
    let response = admin
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("slug=welcome&title=Welcome&content=<p>Hello.</p>&is_published=on")
        .send()
        .await
        .expect("page created");
    assert_eq!(response.status(), 303);

    let rss = client()
        .get(server.url("/feed.xml"))
        .send()
        .await
        .expect("rss");
    assert_eq!(rss.status(), 200);
    let body = rss.text().await.expect("rss body");
    assert!(
        body.contains("<title>Pages — wallermax</title>"),
        "title: {body}"
    );
    assert!(
        body.contains("<language>en</language>"),
        "the language is English: {body}"
    );
    assert!(
        body.contains("Published pages from the wallermax CMS"),
        "description: {body}"
    );
    assert!(body.contains("<title>Welcome</title>"), "entry: {body}");

    let atom = client()
        .get(server.url("/atom.xml"))
        .send()
        .await
        .expect("atom");
    assert_eq!(atom.status(), 200);
    let body = atom.text().await.expect("atom body");
    assert!(
        body.contains("<title>Pages — wallermax</title>"),
        "title: {body}"
    );
}
