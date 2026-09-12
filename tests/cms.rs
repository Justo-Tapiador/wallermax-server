//! CMS integration battery (v0.8.0).
//!
//! Boots the **full** server (database, auth, templates, static files
//! and the CMS) exactly like the binary, backed by a fresh temporary
//! SQLite file, and exercises it with a cookie-storing `reqwest`
//! client — the closest thing to a browser the test suite can get:
//! `Set-Cookie` headers land in the client's jar and ride along on the
//! next request, just like in Chrome.
//!
//! The battery covers the privilege split (anonymous → login redirect,
//! user → 403, editor → content only, admin → content + accounts), the
//! page lifecycle over plain HTML forms, draft visibility, the
//! login/registration modal flows, the self-service password change,
//! the last-admin lockout guard, the import from `public/`, and the
//! `.jhs` rendering of CMS page bodies (globals and `include()`
//! included).

mod common;

use common::{auth_config, TestServer};
use serde_json::{json, Value};
use wallermax_server::config::AppConfig;

/// A configuration with everything the CMS needs, on a fresh database.
fn cms_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    (config, db)
}

/// `cms_config()` with an **empty** static root: `GET /` falls through
/// to the dynamic view chain (the CMS pages home, or `views/index.jhs`
/// when the CMS is off) whatever the repository's own `public/` ships.
///
/// The sample site now serves a static homepage from `public/index.html`
/// with `[cms] default_page` disabled, so the home-route tests below pin
/// their own static root: they verify server behaviour, not the layout
/// of the shipped sample site. (The import battery keeps the real
/// `public/` on purpose — it imports the shipped `hello.jhs` demo.)
fn dynamic_home_config(marker: &str) -> (AppConfig, common::TempDbGuard, StaticRootGuard) {
    let (mut config, db) = cms_config();
    let root = StaticRootGuard::empty(marker);
    config.static_files.root_dir = root.root_dir();
    (config, db, root)
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

/// A cookie-storing client that follows redirects like a real browser.
fn following_browser() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("browser client builds")
}

/// A redirect-free plain client for anonymous assertions.
fn anon_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds")
}

/// Registers the bootstrap admin over the JSON API.
async fn register_admin(server: &TestServer, username: &str, password: &str) {
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": username, "password": password }))
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

/// Creates a page through the admin panel forms, returning its id.
async fn create_page(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
) -> i64 {
    let body = format!(
        "slug={}&title={}&content={}&redirect=%2Fadmin%2Fpages{}",
        slug,
        urlencoding_simple(title),
        urlencoding_simple(content),
        if publish { "&is_published=on" } else { "" }
    );
    let response = client
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("page creation succeeds");

    assert_eq!(response.status(), 303, "creation redirects (PRG)");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    location
        .trim_start_matches("/admin/pages/")
        .split('?')
        .next()
        .expect("path")
        .trim_end_matches("/edit")
        .parse()
        .expect("numeric page id")
}

/// Minimal percent-encoding for form bodies (spaces and the few
/// characters the tests actually use).
fn urlencoding_simple(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
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

// ── Mounting and guards ──────────────────────────────────────────────

#[tokio::test]
async fn cms_routes_are_absent_when_disabled() {
    let (mut config, _db) = cms_config();
    config.cms.enabled = false;
    let server = TestServer::start_full(config).await;
    let client = anon_client();

    let response = client
        .get(server.url("/admin"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404, "no panel without the CMS");

    let response = client
        .get(server.url("/p/hola"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404, "no public pages without the CMS");
}

#[tokio::test]
async fn anonymous_visitors_are_redirected_to_the_login_page() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    let client = anon_client();

    for panel_path in ["/admin", "/admin/pages", "/admin/users"] {
        let response = client
            .get(server.url(panel_path))
            .send()
            .await
            .expect("request");
        assert_eq!(
            response.status(),
            303,
            "the guard redirects instead of answering JSON: {panel_path}"
        );
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("redirect target");
        assert!(
            location.starts_with("/login?redirect="),
            "browsers get a login page, not an envelope: {location}"
        );
        let encoded = panel_path.replace('/', "%2F");
        assert!(
            location.ends_with(&encoded),
            "the panel path rides along: {location}"
        );
    }
}

#[tokio::test]
async fn regular_users_cannot_manage_content() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "ana", "user").await;

    let ana = login_browser(&server, "ana", "password-123").await;
    for panel_path in ["/admin", "/admin/pages"] {
        let response = ana
            .get(server.url(panel_path))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), 403, "users cannot manage content");
        let body = response.text().await.expect("html body");
        assert!(
            body.contains("rol de editor o administrador"),
            "a human-readable privilege page: {body}"
        );
    }
}

#[tokio::test]
async fn editors_manage_content_but_not_accounts() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "borja-editor", "editor").await;

    let editor = login_browser(&server, "borja-editor", "password-123").await;

    let response = editor
        .get(server.url("/admin"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200, "editors reach the panel");
    let body = response.text().await.expect("panel html");
    assert!(body.contains("Panel del CMS"), "dashboard renders: {body}");

    let response = editor
        .get(server.url("/admin/users"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 403, "editors cannot manage accounts");

    // ...but the pages forms work for them.
    let page_id = create_page(
        &server,
        &editor,
        "editor-page",
        "Página del editor",
        "<p>contenido</p>",
        false,
    )
    .await;
    assert!(page_id > 0, "editors create pages");
}

// ── The public site ──────────────────────────────────────────────────

#[tokio::test]
async fn the_home_lists_published_pages_for_everyone() {
    let (config, _db, _root) = dynamic_home_config("home-listing");
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(
        &server,
        &admin,
        "quienes-somos",
        "Quiénes somos",
        "<p>El equipo.</p>",
        true,
    )
    .await;

    // No static index in this fixture: `/` falls through to the
    // dynamic view with the shared header and the page listing.
    let response = reqwest::get(server.url("/")).await.expect("home request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("home html");
    assert!(
        body.contains("Páginas publicadas"),
        "listing section: {body}"
    );
    assert!(body.contains("Quiénes somos"), "the page is listed: {body}");
    assert!(
        body.contains("href=\"/p/quienes-somos\""),
        "slug link: {body}"
    );
    assert!(
        body.contains("href=\"#login\""),
        "login modal opens: {body}"
    );
    assert!(
        body.contains("href=\"#registrar\""),
        "register modal opens: {body}"
    );
    assert!(body.contains("assets/wallermax.css"), "stylesheet: {body}");
}

#[tokio::test]
async fn the_public_pages_index_auto_routes() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(
        &server,
        &admin,
        "servicios",
        "Servicios",
        "<p>Servicios.</p>",
        true,
    )
    .await;

    let response = reqwest::get(server.url("/p")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("Servicios"), "published pages listed: {body}");
}

#[tokio::test]
async fn drafts_are_invisible_to_the_public_and_visible_to_editors() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(
        &server,
        &admin,
        "borrador-secreto",
        "Borrador",
        "<p>Secreto.</p>",
        false,
    )
    .await;

    let response = reqwest::get(server.url("/p/borrador-secreto"))
        .await
        .expect("request");
    assert_eq!(
        response.status(),
        404,
        "drafts are indistinguishable from missing pages"
    );
    let body = response.text().await.expect("html body");
    assert!(body.contains("404"), "the HTML 404 view: {body}");

    let response = admin
        .get(server.url("/p/borrador-secreto"))
        .send()
        .await
        .expect("editor preview");
    assert_eq!(response.status(), 200, "editors preview drafts");
    let body = response.text().await.expect("html body");
    assert!(body.contains("Borrador:"), "the draft banner: {body}");
}

// ── The v0.11.0 homepage takeover (`[cms] default_page`) ─────────────

/// Creates a temp directory with an `index.html` carrying `marker`, to
/// prove the default page outranks (and falls back to) the static
/// index file. Best-effort cleanup: the tests remove it on drop.
struct StaticRootGuard {
    dir: std::path::PathBuf,
}

impl StaticRootGuard {
    fn with_index(marker: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wallermax-cms-home-{}-{marker}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp static root");
        std::fs::write(dir.join("index.html"), format!("<h1>{marker}</h1>"))
            .expect("index fixture");
        Self { dir }
    }

    /// An empty static root: no index file, so `/` never stops at the
    /// static layer and falls through to the dynamic view chain —
    /// regardless of any homepage the repository's own `public/` ships.
    fn empty(marker: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wallermax-cms-root-{}-{marker}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp static root");
        // A leftover index from a crashed run would defeat the fixture's
        // whole point: this root must ship no index file.
        let _ = std::fs::remove_file(dir.join("index.html"));
        Self { dir }
    }

    /// Forward-slash path (valid on Windows as well).
    fn root_dir(&self) -> String {
        self.dir.display().to_string().replace('\\', "/")
    }
}

impl Drop for StaticRootGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn default_page_takes_over_the_homepage() {
    let (mut config, _db) = cms_config();
    config.cms.default_page = Some(String::from("inicio"));
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(
        &server,
        &admin,
        "inicio",
        "Portada del sitio",
        "<p>contenido-de-la-portada</p>",
        true,
    )
    .await;

    let response = anon_client()
        .get(server.url("/"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200, "the default page renders at /");
    // Rendered directly, not redirected: / stays the canonical URL.
    assert!(
        response.headers().get("location").is_none(),
        "no redirect to /p/inicio"
    );
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html")),
        "the homepage is HTML"
    );
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Portada del sitio"),
        "the cms_page.jhs wrapper title: {body}"
    );
    assert!(body.contains("/p/inicio"), "the wrapper meta: {body}");
    assert!(
        body.contains("contenido-de-la-portada"),
        "the page body: {body}"
    );

    // The page keeps its canonical /p/{slug} URL too — same pipeline.
    let response = anon_client()
        .get(server.url("/p/inicio"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("contenido-de-la-portada"),
        "same page: {body}"
    );
}

#[tokio::test]
async fn default_page_outranks_the_static_index_and_misses_degrade() {
    let (mut config, _db) = cms_config();
    let root = StaticRootGuard::with_index("estatica");
    config.static_files.root_dir = root.root_dir();
    config.cms.default_page = Some(String::from("inicio"));
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    let page_id = create_page(
        &server,
        &admin,
        "inicio",
        "Portada",
        "<p>la-portada-vive</p>",
        true,
    )
    .await;

    // The explicit configuration beats the static index file.
    let response = anon_client()
        .get(server.url("/"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("la-portada-vive"),
        "the CMS page wins: {body}"
    );
    assert!(!body.contains("estatica"), "the static index loses: {body}");

    // A slug that stops existing mid-flight degrades gracefully: the
    // homepage falls back to the normal chain instead of hard-failing.
    let response = admin
        .post(server.url(&format!("/admin/pages/{page_id}/delete")))
        .send()
        .await
        .expect("deletion succeeds");
    assert_eq!(response.status(), 303, "deletion redirects (PRG)");

    let response = anon_client()
        .get(server.url("/"))
        .send()
        .await
        .expect("request");
    assert_eq!(
        response.status(),
        200,
        "the homepage falls back, never hard-fails"
    );
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("estatica"),
        "the static index file serves the fallback: {body}"
    );
}

#[tokio::test]
async fn default_page_draft_follows_the_p_gating() {
    let (mut config, _db) = cms_config();
    config.cms.default_page = Some(String::from("borrador-portada"));
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(
        &server,
        &admin,
        "borrador-portada",
        "Portada oculta",
        "<p>secreto-portada</p>",
        false,
    )
    .await;

    // The public gets the same 404 as /p/{slug} — drafts stay drafts
    // wherever they are mounted.
    let response = anon_client()
        .get(server.url("/"))
        .send()
        .await
        .expect("request");
    assert_eq!(
        response.status(),
        404,
        "a draft homepage is invisible to the public"
    );
    let body = response.text().await.expect("html body");
    assert!(body.contains("404"), "the HTML 404 view: {body}");

    // Editors preview it with the banner, straight from /.
    let response = admin
        .get(server.url("/"))
        .send()
        .await
        .expect("editor preview");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("Borrador:"), "the draft banner: {body}");
    assert!(body.contains("secreto-portada"), "the draft body: {body}");
}

#[tokio::test]
async fn default_page_is_ignored_while_the_cms_is_disabled() {
    let (mut config, _db, _root) = dynamic_home_config("cms-disabled");
    config.cms.enabled = false;
    config.cms.default_page = Some(String::from("inicio"));
    let server = TestServer::start_full(config).await;

    let response = anon_client()
        .get(server.url("/"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Páginas publicadas"),
        "the normal views/index.jhs chain serves the homepage: {body}"
    );
}

#[tokio::test]
async fn cms_page_bodies_render_as_jhs_with_the_globals() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let content = "<h2>Hola <?= user ? user.username : \"anónimo\" ?></h2>\
                   <?jhs if (pages.length > 0) { ?><p>hay <?= pages.length ?> páginas</p>\
                   <?jhs } else { ?><p>aún sin páginas</p><?jhs } ?>";
    create_page(
        &server,
        &admin,
        "personalizada",
        "Personalizada",
        content,
        true,
    )
    .await;

    let response = reqwest::get(server.url("/p/personalizada"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Hola anónimo"),
        "the body personalises anonymously: {body}"
    );
    assert!(
        body.contains("hay 1 páginas"),
        "the `pages` global reaches CMS bodies: {body}"
    );

    let response = admin
        .get(server.url("/p/personalizada"))
        .send()
        .await
        .expect("editor view");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Hola root-admin"),
        "the body personalises for sessions: {body}"
    );
}

#[tokio::test]
async fn cms_page_bodies_can_include_the_shared_partials() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let content = "<?jhs include(\"partials/footer\") ?>";
    create_page(&server, &admin, "con-pie", "Con pie", content, true).await;

    let response = reqwest::get(server.url("/p/con-pie"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("sin una línea de JavaScript"),
        "the footer partial is embedded: {body}"
    );
}

#[tokio::test]
async fn broken_page_bodies_answer_the_500_envelope() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(
        &server,
        &admin,
        "rota",
        "Rota",
        "<?= variable_inexistente ?>",
        true,
    )
    .await;

    let response = reqwest::get(server.url("/p/rota")).await.expect("request");
    assert_eq!(response.status(), 500);
    let body: Value = response.json().await.expect("JSON envelope");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("variable_inexistente"),
        "author diagnostics: {body}"
    );
}

// ── The admin panel over forms ───────────────────────────────────────

#[tokio::test]
async fn the_page_lifecycle_over_plain_forms() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // Create as a draft.
    let page_id = create_page(
        &server,
        &admin,
        "ciclo",
        "Ciclo de vida",
        "<p>versión uno</p>",
        false,
    )
    .await;

    // The edit view carries the values.
    let response = admin
        .get(server.url(&format!("/admin/pages/{page_id}/edit?ok=creada")))
        .send()
        .await
        .expect("edit view");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Página creada como borrador"),
        "ok banner: {body}"
    );
    assert!(body.contains("value=\"ciclo\""), "slug preserved: {body}");
    assert!(
        body.contains("&lt;p&gt;versión uno&lt;/p&gt;"),
        "content preserved escaped: {body}"
    );

    // Publish through the update form.
    let response = admin
        .post(server.url(&format!("/admin/pages/{page_id}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("slug=ciclo&title=Ciclo%20de%20vida&content=%3Cp%3Eversi%C3%B3n%20dos%3C%2Fp%3E&is_published=on")
        .send()
        .await
        .expect("update");
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert_eq!(location, format!("/admin/pages/{page_id}/edit?ok=guardada"));

    // The public page reflects the new content.
    let response = reqwest::get(server.url("/p/ciclo")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("versión dos"), "updated content: {body}");

    // The listing shows it as published, and the delete works.
    let response = admin
        .get(server.url("/admin/pages"))
        .send()
        .await
        .expect("listing");
    let body = response.text().await.expect("html body");
    assert!(body.contains("publicada"), "state chip: {body}");

    let response = admin
        .post(server.url(&format!("/admin/pages/{page_id}/delete")))
        .send()
        .await
        .expect("delete");
    assert_eq!(response.status(), 303);
    let response = reqwest::get(server.url("/p/ciclo")).await.expect("request");
    assert_eq!(response.status(), 404, "the page is gone");
}

#[tokio::test]
async fn invalid_forms_re_render_without_losing_the_draft() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let response = admin
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("slug=Mal_Slug&title=T%C3%ADtulo&content=mi%20borrador%20precioso")
        .send()
        .await
        .expect("invalid creation");
    assert_eq!(
        response.status(),
        200,
        "validation failures re-render the form (no redirect)"
    );
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("El slug debe tener"),
        "the validation message: {body}"
    );
    assert!(
        body.contains("mi borrador precioso"),
        "the submitted content survives: {body}"
    );
}

#[tokio::test]
async fn duplicate_slugs_are_rejected_without_data_loss() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(&server, &admin, "unico", "Primera", "<p>uno</p>", true).await;

    let response = admin
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("slug=unico&title=Segunda&content=%3Cp%3Edos%3C%2Fp%3E")
        .send()
        .await
        .expect("duplicate creation");
    assert_eq!(response.status(), 200, "the form re-renders");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Ese slug ya existe"),
        "the conflict message: {body}"
    );
}

#[tokio::test]
async fn the_dashboard_counts_content() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(&server, &admin, "uno", "Uno", "<p>1</p>", true).await;
    create_page(&server, &admin, "dos", "Dos", "<p>2</p>", false).await;

    let response = admin
        .get(server.url("/admin"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("2") && body.contains("Borradores"),
        "counters: {body}"
    );
}

// ── Import from public/ ──────────────────────────────────────────────

#[tokio::test]
async fn importing_copies_a_public_file_into_a_draft() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // The import listing offers the shipped demo template.
    let response = admin
        .get(server.url("/admin/pages/import"))
        .send()
        .await
        .expect("import listing");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("hello.jhs"),
        "the listing includes hello.jhs: {body}"
    );

    let response = admin
        .post(server.url("/admin/pages/import"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("file=hello.jhs")
        .send()
        .await
        .expect("import");
    assert_eq!(response.status(), 303, "import redirects to the edit view");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert!(
        location.contains("/edit?ok=importada"),
        "location: {location}"
    );

    // The draft is private until published...
    let response = reqwest::get(server.url("/p/hello")).await.expect("request");
    assert_eq!(response.status(), 404, "imports start as drafts");
}

#[tokio::test]
async fn importing_rejects_traversal_and_foreign_files() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    for file in ["../Cargo.toml", "../../etc/passwd", "wallermax.toml"] {
        let response = admin
            .post(server.url("/admin/pages/import"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("file={file}"))
            .send()
            .await
            .expect("import attempt");
        assert_eq!(response.status(), 200, "the form re-renders: {file}");
        let body = response.text().await.expect("html body");
        assert!(
            body.contains("no es importable"),
            "a clear rejection for {file}: {body}"
        );
    }
}

// ── Account management ───────────────────────────────────────────────

#[tokio::test]
async fn user_management_creates_roles_and_resets_passwords() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "carla", "editor").await;

    // The listing shows her.
    let response = admin
        .get(server.url("/admin/users"))
        .send()
        .await
        .expect("listing");
    let body = response.text().await.expect("html body");
    assert!(body.contains("carla"), "the account is listed: {body}");

    // Find her id through the JSON API (admins keep it).
    let response = reqwest::Client::new()
        .get(server.url("/api/admin/users"))
        .header("Cookie", session_cookie(&admin, &server).await)
        .send()
        .await
        .expect("json listing");
    let body: Value = response.json().await.expect("JSON");
    let carla = body["users"]
        .as_array()
        .expect("users")
        .iter()
        .find(|user| user["username"] == "carla")
        .expect("carla exists")
        .clone();
    let carla_id = carla["id"].as_i64().expect("id");
    assert_eq!(carla["role"], "editor", "the created role");

    // Demote her to user and reset her password.
    let response = admin
        .post(server.url(&format!("/admin/users/{carla_id}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("role=user&password=nueva-clave-9")
        .send()
        .await
        .expect("update");
    assert_eq!(response.status(), 303);

    // The new password works over the API.
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "carla", "password": "nueva-clave-9" }))
        .send()
        .await
        .expect("login");
    assert_eq!(response.status(), 200, "the reset password works");
}

/// Extracts the client's session cookie value (tests only: the cookie
/// store keeps it hidden from `headers()`).
async fn session_cookie(client: &reqwest::Client, server: &TestServer) -> String {
    // A throwaway request whose Set-Cookie we can capture: login again
    // with the same credentials through this client's... simpler —
    // re-login through a fresh client and reuse its token, since both
    // sessions share the account.
    let response = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=root-admin&password=sup3r-secret!&redirect=/")
        .send()
        .await
        .expect("login");
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("session cookie")
        .to_owned();
    set_cookie
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned()
}

#[tokio::test]
async fn the_last_admin_is_never_demoted_or_deleted() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "dani", "admin").await;

    // Self-edit guard: the root admin editing their own account.
    let me = find_user_id(&server, &admin, "root-admin").await;
    let response = admin
        .post(server.url(&format!("/admin/users/{me}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("role=user&password=")
        .send()
        .await
        .expect("self edit");
    assert_eq!(response.status(), 200, "the form re-renders");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("tu propia cuenta"),
        "self-edit is refused: {body}"
    );

    // Demote dani (two admins → allowed) ...
    let dani = find_user_id(&server, &admin, "dani").await;

    // ... but dani logged in while still an admin, so the cookie keeps
    // carrying the admin role (JWT claims are signed at login). The
    // stale-admin attempt must hit the lockout below.
    let dani_client = login_browser(&server, "dani", "password-123").await;

    let response = admin
        .post(server.url(&format!("/admin/users/{dani}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("role=user&password=")
        .send()
        .await
        .expect("demote");
    assert_eq!(response.status(), 303, "the second admin can be demoted");

    // The stale-admin token (role claim signed before the demotion) tries
    // to demote the only real admin: the lockout must fire.
    let response = dani_client
        .post(server.url(&format!("/admin/users/{me}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("role=user&password=")
        .send()
        .await
        .expect("stale-admin demotion");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("el último administrador"),
        "the lockout guard fires even for stale-admin tokens: {body}"
    );

    let response = dani_client
        .post(server.url(&format!("/admin/users/{me}/delete")))
        .send()
        .await
        .expect("stale-admin deletion");
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert_eq!(
        location, "/admin/users?error=ultimo-admin",
        "location: {location}"
    );
}

/// Resolves a user id through the admin JSON API using the client's
/// session (the client re-logs-in to capture a Bearer token).
async fn find_user_id(server: &TestServer, client: &reqwest::Client, username: &str) -> i64 {
    let cookie = session_cookie(client, server).await;
    let response = reqwest::Client::new()
        .get(server.url("/api/admin/users"))
        .header("Cookie", cookie)
        .send()
        .await
        .expect("listing");
    let body: Value = response.json().await.expect("JSON");
    body["users"]
        .as_array()
        .expect("users")
        .iter()
        .find(|user| user["username"] == username)
        .expect("user exists")["id"]
        .as_i64()
        .expect("id")
}

#[tokio::test]
async fn deleting_an_account_kills_its_refresh_tokens() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "effi", "user").await;

    // effi logs in over JSON and keeps a refresh token.
    let login: Value = reqwest::Client::new()
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "effi", "password": "password-123" }))
        .send()
        .await
        .expect("login")
        .json()
        .await
        .expect("JSON");
    let refresh_token = login["refresh_token"]
        .as_str()
        .expect("refresh token")
        .to_owned();

    let effi_id = find_user_id(&server, &admin, "effi").await;
    let response = admin
        .post(server.url(&format!("/admin/users/{effi_id}/delete")))
        .send()
        .await
        .expect("delete");
    assert_eq!(response.status(), 303, "deletion redirects");

    // The refresh token is dead: SQL cascaded the revocation.
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/refresh"))
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .expect("refresh");
    assert_eq!(response.status(), 401, "the family died with the account");
}

// ── The browser circle ───────────────────────────────────────────────

#[tokio::test]
async fn the_full_browser_circle_login_logout_modal() {
    let (config, _db, _root) = dynamic_home_config("browser-circle");
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;

    let client = browser_client();

    // Anonymous home: the modals and buttons are all there.
    let response = client.get(server.url("/")).send().await.expect("home");
    let body = response.text().await.expect("html body");
    assert!(body.contains("href=\"#login\""), "entrar link: {body}");
    assert!(body.contains("id=\"login\""), "login modal: {body}");
    assert!(body.contains("id=\"registrar\""), "register modal: {body}");
    assert!(
        body.contains("name=\"redirect\" value=\"/\""),
        "the modal posts the current path: {body}"
    );

    // Form login (the modal's POST) sets the session cookie.
    let response = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=root-admin&password=sup3r-secret!&redirect=/")
        .send()
        .await
        .expect("form login");
    assert_eq!(response.status(), 303);
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("session cookie set");
    assert!(set_cookie.contains("HttpOnly"), "HttpOnly: {set_cookie}");
    assert!(
        set_cookie.contains("SameSite=Strict"),
        "SameSite: {set_cookie}"
    );
    assert!(
        !set_cookie.contains("Secure"),
        "no Secure over plain HTTP: {set_cookie}"
    );

    // The next page visit is personalised: the header shows the user
    // and the logout button (a form styled as a link), no modals.
    let response = client.get(server.url("/")).send().await.expect("home");
    let body = response.text().await.expect("html body");
    assert!(body.contains("root-admin"), "the username appears: {body}");
    assert!(body.contains("Salir"), "the logout control: {body}");
    assert!(
        body.contains("action=\"/api/auth/logout\""),
        "logout form: {body}"
    );
    assert!(
        !body.contains("id=\"registrar\""),
        "no register modal while logged in: {body}"
    );

    // The /perfil page shows the password-change form.
    let response = client
        .get(server.url("/perfil"))
        .send()
        .await
        .expect("perfil");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("action=\"/perfil/password\""),
        "password form: {body}"
    );

    // Form logout: 303 back to the page, cookie cleared.
    let response = client
        .post(server.url("/api/auth/logout"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("redirect=/")
        .send()
        .await
        .expect("form logout");
    assert_eq!(response.status(), 303);
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("cookie cleared");
    assert!(set_cookie.contains("Max-Age=0"), "cleared: {set_cookie}");

    let response = client.get(server.url("/")).send().await.expect("home");
    let body = response.text().await.expect("html body");
    assert!(body.contains("href=\"#login\""), "anonymous again: {body}");
}

#[tokio::test]
async fn form_registration_logs_the_fresh_account_in() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;

    let client = browser_client();
    let response = client
        .post(server.url("/api/auth/register"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=nuevo&password=password-123&redirect=/")
        .send()
        .await
        .expect("form registration");
    assert_eq!(
        response.status(),
        303,
        "the fresh account is logged in and redirected"
    );
    assert!(
        response.headers().get("set-cookie").is_some(),
        "the session cookie rides along"
    );

    let response = client
        .get(server.url("/perfil"))
        .send()
        .await
        .expect("perfil");
    let body = response.text().await.expect("html body");
    assert!(body.contains("nuevo"), "the session is live: {body}");
    assert!(
        body.contains("user"),
        "later accounts are regular users: {body}"
    );
}

#[tokio::test]
async fn form_registration_failures_bounce_back_with_the_error() {
    let (config, _db, _root) = dynamic_home_config("register-bounce");
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;

    // Duplicate username → back to the page with the code + fragment.
    let response = browser_client()
        .post(server.url("/api/auth/register"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=root-admin&password=password-123&redirect=/p")
        .send()
        .await
        .expect("duplicate registration");
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert_eq!(
        location, "/p?register_error=tomado#registrar",
        "the modal re-opens with the message"
    );

    // Short password → the contrasena code.
    let response = browser_client()
        .post(server.url("/api/auth/register"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=otro&password=corta&redirect=/")
        .send()
        .await
        .expect("weak password");
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert_eq!(
        location, "/?register_error=contrasena#registrar",
        "location: {location}"
    );

    // And the error renders inside the modal on the landing page.
    let client = browser_client();
    let response = client
        .post(server.url("/api/auth/register"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=otro&password=corta&redirect=/")
        .send()
        .await
        .expect("weak password");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect")
        .to_owned();
    let path = location.split('#').next().expect("path");
    let response = client.get(server.url(path)).send().await.expect("landing");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("La contraseña no cumple los requisitos"),
        "the message shows in the modal: {body}"
    );
}

#[tokio::test]
async fn the_self_service_password_change_requires_the_current_one() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let client = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // Wrong current password.
    let response = client
        .post(server.url("/perfil/password"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("current_password=wrong-one&new_password=nueva-clave-9&redirect=/perfil")
        .send()
        .await
        .expect("wrong current");
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert_eq!(location, "/perfil?pw_error=actual", "location: {location}");

    // The right one.
    let response = client
        .post(server.url("/perfil/password"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("current_password=sup3r-secret!&new_password=nueva-clave-9&redirect=/perfil")
        .send()
        .await
        .expect("password change");
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert_eq!(location, "/perfil?ok=contrasena", "location: {location}");

    // The old password stops working, the new one works.
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "root-admin", "password": "sup3r-secret!" }))
        .send()
        .await
        .expect("old login");
    assert_eq!(response.status(), 401, "the old password is dead");

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "root-admin", "password": "nueva-clave-9" }))
        .send()
        .await
        .expect("new login");
    assert_eq!(response.status(), 200, "the new password works");
}

// ── Cookie hardening ─────────────────────────────────────────────────

#[tokio::test]
async fn the_secure_attribute_follows_the_configuration() {
    // `always`: Secure even on the plain-HTTP test server (the
    // TLS-terminating-proxy posture).
    let (mut config, _db) = cms_config();
    config.auth.secure_cookies = wallermax_server::config::SecureCookieMode::Always;
    let server = TestServer::start_full(config).await;
    register_admin(&server, "secure-user", "password-123").await;

    let response = reqwest::Client::new()
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": "secure-user", "password": "password-123" }))
        .send()
        .await
        .expect("login");
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("cookie");
    assert!(cookie.contains("Secure"), "pinned Secure: {cookie}");

    // `never`: no Secure even hypothetically under TLS... covered by
    // the auto/always pair above plus the session unit tests.
}

#[tokio::test]
async fn redirect_following_browser_completes_the_login_circle() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;

    // A real browser follows the 303: login → /perfil personalised.
    let client = following_browser();
    let response = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=root-admin&password=sup3r-secret!&redirect=/perfil")
        .send()
        .await
        .expect("login");
    assert_eq!(response.status(), 200, "the redirect landed");
    let url = response.url().to_string();
    assert!(url.ends_with("/perfil"), "landed on /perfil: {url}");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("root-admin"),
        "the page is personalised: {body}"
    );
}
