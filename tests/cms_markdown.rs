//! The F8 editor battery: Markdown bodies and the server-side
//! previsualización.
//!
//! Boots the full server exactly like `tests/cms.rs` and exercises the
//! two body modes side by side: Markdown pages render through the safe
//! subset (raw HTML dropped, URL schemes filtered), `.jhs` pages keep
//! rendering through the engine exactly as before F8 (the
//! backward-compatibility contract), and the preview button — pure
//! HTML (`formaction`), zero JavaScript — renders on the server
//! without writing anything.

mod common;

use common::{auth_config, TestServer};
use serde_json::json;
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

/// A cookie-storing client with redirects kept manual, like a browser.
fn browser_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .expect("browser client builds")
}

/// A plain redirect-free client for anonymous assertions.
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

/// Logs a browser in through the form endpoint.
async fn login_browser(server: &TestServer, username: &str, password: &str) -> reqwest::Client {
    let client = browser_client();
    let response = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={username}&password={password}&redirect=/admin/pages"
        ))
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    client
}

/// Registers a second, regular (non-editor) account.
async fn register_user(server: &TestServer, username: &str, password: &str) {
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(response.status(), 201, "the second account is a user");
}

fn urlencode(value: &str) -> String {
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

/// Creates a page over the admin form with an explicit body format,
/// returning the id from the redirect.
async fn create_page(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
    body_format: Option<&str>,
    publish: bool,
) -> i64 {
    let mut body = format!(
        "slug={}&title={}&content={}&redirect=%2Fadmin%2Fpages",
        slug,
        urlencode(title),
        urlencode(content)
    );
    if let Some(format) = body_format {
        body.push_str(&format!("&body_format={format}"));
    }
    if publish {
        body.push_str("&is_published=on");
    }
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

/// POSTs the whole form to the preview endpoint.
async fn post_preview(
    server: &TestServer,
    client: &reqwest::Client,
    body: &str,
) -> reqwest::Response {
    client
        .post(server.url("/admin/pages/preview"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body.to_owned())
        .send()
        .await
        .expect("preview request")
}

// ── The two body modes ───────────────────────────────────────────────

#[tokio::test]
async fn markdown_pages_render_as_html_not_source() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let content = "# Servicios\n\nOfrecemos **soporte** e *integración*.\n\n\
                   - una cosa\n- otra cosa\n\n\
                   | plan | precio |\n|---|---|\n| básico | 10 |\n\n\
                   [web externa](https://example.com)";
    create_page(
        &server,
        &admin,
        "servicios",
        "Servicios",
        content,
        Some("markdown"),
        true,
    )
    .await;

    let response = reqwest::get(server.url("/p/servicios"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("<h1>Servicios</h1>"), "heading: {body}");
    assert!(
        body.contains("<strong>soporte</strong>"),
        "emphasis: {body}"
    );
    assert!(body.contains("<em>integración</em>"), "{body}");
    assert!(body.contains("<li>una cosa</li>"), "list: {body}");
    assert!(body.contains("<table>"), "table: {body}");
    assert!(body.contains("<td>básico</td>"), "{body}");
    assert!(
        body.contains("href=\"https://example.com\""),
        "safe external link: {body}"
    );
    // The Markdown SOURCE never leaks through:
    assert!(!body.contains("**soporte**"), "source hidden: {body}");
    assert!(!body.contains("| plan |"), "source hidden: {body}");
}

#[tokio::test]
async fn jhs_pages_keep_rendering_as_templates() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // Pre-F8 shape: the form carries no body_format field at all.
    let content = "<p>Hola <?= user ? user.username : \"anónimo\" ?></p>";
    let id_implicit = create_page(
        &server,
        &admin,
        "implicita",
        "Implícita",
        content,
        None,
        true,
    )
    .await;

    // And the explicit .jhs mode, as the new select posts it.
    create_page(
        &server,
        &admin,
        "explicita",
        "Explícita",
        "<p><?= 1 + 1 ?></p>",
        Some("jhs"),
        true,
    )
    .await;

    for slug in ["implicita", "explicita"] {
        let response = reqwest::get(server.url(&format!("/p/{slug}")))
            .await
            .expect("request");
        assert_eq!(response.status(), 200, "{slug}");
        let body = response.text().await.expect("html body");
        assert!(body.contains("<p>"), "still a template: {body}");
    }

    let response = reqwest::get(server.url("/p/implicita"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(body.contains("Hola anónimo"), "globals ride along: {body}");

    // The stored mode is readable back from the edit form.
    let response = admin
        .get(server.url(&format!("/admin/pages/{id_implicit}/edit")))
        .send()
        .await
        .expect("edit form");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<option value=\"markdown\"") && body.contains("<option value=\"jhs\""),
        "the format select is offered: {body}"
    );
    assert!(
        !body.contains("value=\"markdown\" selected"),
        "an implicit page defaults to jhs: {body}"
    );
}

#[tokio::test]
async fn drafts_gate_markdown_pages_like_any_other() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    create_page(
        &server,
        &admin,
        "borrador-md",
        "Borrador Markdown",
        "# Secreto\n",
        Some("markdown"),
        false,
    )
    .await;

    // The public gets the same 404 as any missing page.
    let response = anon_client()
        .get(server.url("/p/borrador-md"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404, "drafts stay drafts");

    // The editor sees it rendered.
    let response = admin
        .get(server.url("/p/borrador-md"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<h1>Secreto</h1>"),
        "editor sees the render: {body}"
    );
    assert!(body.contains("Borrador:"), "the draft banner: {body}");
}

#[tokio::test]
async fn markdown_bodies_cannot_carry_scripts() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let content = "# Portada\n\n\
                   <script>alert(1)</script>\n\n\
                   [pincha aquí](javascript:alert(2))\n\n\
                   <img src=x onerror=alert(3)>\n\n\
                   ![pwn](data:text/html,<b>)\n\n\
                   texto normal";
    create_page(
        &server,
        &admin,
        "ataque",
        "Ataque",
        content,
        Some("markdown"),
        true,
    )
    .await;

    let response = reqwest::get(server.url("/p/ataque"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(!body.contains("<script"), "no script markup: {body}");
    assert!(
        !body.contains("src=x"),
        "no unfiltered image source: {body}"
    );
    assert!(!body.contains("onerror"), "no handlers: {body}");
    assert!(!body.contains("javascript:"), "no js schemes: {body}");
    assert!(!body.contains("data:text/html"), "no data schemes: {body}");
    assert!(
        body.contains("<img src=\"#\""),
        "the payload image degrades to a fragment: {body}"
    );
    assert!(body.contains("texto normal"), "prose survives: {body}");
    assert!(body.contains("<h1>Portada</h1>"), "{body}");
}

// ── The previsualización ─────────────────────────────────────────────

#[tokio::test]
async fn the_preview_renders_server_side_without_saving() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let response = post_preview(
        &server,
        &admin,
        "slug=nueva&title=Nueva&body_format=markdown&content=%23%20T%C3%ADtulo%0A%0Aun%20**borrador**",
    )
    .await;
    assert_eq!(response.status(), 200, "the preview renders");
    let body = response.text().await.expect("html body");
    assert!(body.contains("Vista previa"), "the preview section: {body}");
    assert!(
        body.contains("<h1>Título</h1>"),
        "rendered markdown: {body}"
    );
    assert!(body.contains("<strong>borrador</strong>"), "{body}");
    // The form rides along, values kept, so the editor keeps writing.
    assert!(
        body.contains("name=\"content\""),
        "the form is back: {body}"
    );
    assert!(
        body.contains("un%20**borrador**") || body.contains("un **borrador**"),
        "content kept: {body}"
    );

    // Nothing was persisted: the page is nowhere public nor in the panel.
    let response = reqwest::get(server.url("/p/nueva")).await.expect("request");
    assert_eq!(response.status(), 404, "the preview never writes");

    // The preview button is pure HTML: the form carries it.
    let response = admin
        .get(server.url("/admin/pages/new"))
        .send()
        .await
        .expect("new form");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("formaction=\"/admin/pages/preview\""),
        "the no-JS preview button: {body}"
    );
}

#[tokio::test]
async fn preview_jhs_mode_runs_the_engine_and_reports_errors() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // A good template renders through the engine with the live globals.
    let response = post_preview(
        &server,
        &admin,
        "slug=plantilla&title=Plantilla&body_format=jhs&content=%3Cp%3E2%20%2B%202%20%3D%20%3C%3F%3D%202%20%2B%202%20%3F%3E%3C%2Fp%3E",
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("2 + 2 = 4"), "the engine ran: {body}");

    // A broken template bounces back as a form error, not a 500 —
    // that is the whole point of previewing.
    let response = post_preview(
        &server,
        &admin,
        "slug=rota&title=Rota&body_format=jhs&content=%3C%3Fjhs%20nada%20de%20nada%20%3F%3E",
    )
    .await;
    assert_eq!(response.status(), 200, "errors bounce, not 500");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("aviso-error"),
        "the error shows inline: {body}"
    );
}

#[tokio::test]
async fn the_preview_is_editors_only() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    register_user(&server, "redactor-llano", "password-123").await;

    // Anonymous: a browser-facing redirect to the login form.
    let response = post_preview(
        &server,
        &anon_client(),
        "slug=x&title=X&body_format=markdown&content=hola",
    )
    .await;
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect");
    assert!(
        location.starts_with("/login?redirect="),
        "location: {location}"
    );

    // An authenticated regular user: the HTML 403 page.
    let plain = login_browser(&server, "redactor-llano", "password-123").await;
    let response = post_preview(
        &server,
        &plain,
        "slug=x&title=X&body_format=markdown&content=hola",
    )
    .await;
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn preview_roundtrips_the_edit_form_and_switches_modes() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // A .jhs page exists…
    let id = create_page(
        &server,
        &admin,
        "cambiable",
        "Cambiable",
        "<p>viejo</p>",
        Some("jhs"),
        true,
    )
    .await;

    // …the editor previews it as Markdown, carrying the page_id:
    let response = post_preview(
        &server,
        &admin,
        &format!(
            "slug=cambiable&title=Cambiable&body_format=markdown&page_id={id}\
             &content=%23%20Nuevo%20formato%0A%0A**cambiado**"
        ),
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<h1>Nuevo formato</h1>"),
        "markdown preview: {body}"
    );
    // The re-rendered form still saves to the edit route…
    assert!(
        body.contains(&format!("/admin/pages/{id}\"")),
        "the edit action survives the round-trip: {body}"
    );
    // …and the hidden id rides along for the next hop.
    assert!(
        body.contains(&format!("name=\"page_id\" value=\"{id}\"")),
        "the hidden id rides along: {body}"
    );

    // Saving applies the switch, and the public page re-renders.
    let response = admin
        .post(server.url(&format!("/admin/pages/{id}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(
            "slug=cambiable&title=Cambiable&body_format=markdown&is_published=on\
             &redirect=%2Fadmin%2Fpages&content=%23%20Nuevo%20formato%0A%0A**cambiado**",
        )
        .send()
        .await
        .expect("save");
    assert_eq!(response.status(), 303, "save redirects");

    let response = reqwest::get(server.url("/p/cambiable"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<strong>cambiado</strong>"),
        "the mode switched: {body}"
    );
    assert!(
        !body.contains("<p>viejo</p>"),
        "the old body is gone: {body}"
    );

    // And the edit form now selects the stored mode.
    let response = admin
        .get(server.url(&format!("/admin/pages/{id}/edit")))
        .send()
        .await
        .expect("edit form");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("value=\"markdown\" selected"),
        "the select reflects the stored mode: {body}"
    );
}

#[tokio::test]
async fn malformed_formats_bounce_back_with_the_draft_kept() {
    let (config, _db) = cms_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let response = admin
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(
            "slug=formato-raro&title=Formato&body_format=html5&redirect=%2Fadmin%2Fpages\
             &content=%23%20no%20se%20guarda",
        )
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), 200, "bounces back to the form");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("El formato del contenido debe ser jhs o markdown."),
        "the Spanish error: {body}"
    );
    assert!(body.contains("no se guarda"), "the draft is kept: {body}");

    let response = reqwest::get(server.url("/p/formato-raro"))
        .await
        .expect("request");
    assert_eq!(response.status(), 404, "nothing was stored");
}
