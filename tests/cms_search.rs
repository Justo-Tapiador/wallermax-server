//! The F10 findability battery: FTS5 search, paginated listings and
//! the RSS/Atom feeds.
//!
//! Boots the full server like every CMS battery and drives it with
//! cookie-storing `reqwest` clients. The page size is pinned to 2 so
//! the pagination assertions stay small (three pages of results from
//! five rows), while everything else runs on the real defaults.
//!
//! Coverage:
//! - `GET /p` paginates (window, prev/next edges, out-of-range and
//!   garbage `?page=` clamp to the nearest real page);
//! - `GET /buscar?q=` round-trips: quoted snippets with `<mark>`
//!   highlights, operators as inert literals, HTML in bodies escaped
//!   by construction, result pagination carrying the query;
//! - the index follows the page lifecycle (draft → published →
//!   reworded → deleted: the FTS5 triggers keep the index in step);
//! - `GET /admin/pages?q=` flattens the tree into ranked hits for
//!   editors (drafts and media references included) and stays behind
//!   the role gates;
//! - `GET /feed.xml` / `GET /atom.xml` serve published pages only,
//!   with absolute URLs from `site_url` or the Host header, and 404
//!   while `[cms] feed` is off;
//! - the media grid paginates at 24 per page.

mod common;

use common::{auth_config, TestServer};
use serde_json::json;
use wallermax_server::config::AppConfig;

/// A cookie-storing client that keeps redirects manual (each hop
/// assertable) — the same browser stance as the other CMS batteries.
fn browser_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
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

/// Everything the CMS needs on a fresh database, with a tiny
/// `index_page_size` (2) so three rows already paginate.
fn findability_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.cms.enabled = true;
    config.cms.index_page_size = 2;
    (config, db)
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
            "username={username}&password={password}&redirect=/"
        ))
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    client
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

/// The full page-creation form (including the optional SEO
/// description the feeds read), returning the new page id.
async fn create_page_full(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
    meta_description: Option<&str>,
) -> i64 {
    let mut body = format!(
        "slug={}&title={}&content={}&redirect=%2Fadmin%2Fpages",
        slug,
        urlencoding_simple(title),
        urlencoding_simple(content),
    );
    if publish {
        body.push_str("&is_published=on");
    }
    if let Some(description) = meta_description {
        body.push_str(&format!(
            "&meta_description={}",
            urlencoding_simple(description)
        ));
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

/// Creates a published page with no SEO riders.
async fn create_page(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
) -> i64 {
    create_page_full(server, client, slug, title, content, true, None).await
}

/// Rewrites a page through the edit form (publish state included).
async fn update_page(
    server: &TestServer,
    client: &reqwest::Client,
    id: i64,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
) {
    let body = format!(
        "slug={}&title={}&content={}&redirect=%2Fadmin%2Fpages{}",
        slug,
        urlencoding_simple(title),
        urlencoding_simple(content),
        if publish { "&is_published=on" } else { "" }
    );
    let response = client
        .post(server.url(&format!("/admin/pages/{id}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("page update succeeds");
    assert_eq!(response.status(), 303, "update redirects (PRG)");
}

/// Deletes a page through the panel form.
async fn delete_page(server: &TestServer, client: &reqwest::Client, id: i64) {
    let response = client
        .post(server.url(&format!("/admin/pages/{id}/delete")))
        .send()
        .await
        .expect("page delete succeeds");
    assert_eq!(response.status(), 303, "delete redirects (PRG)");
}

// ─── `GET /p`: the paginated index ──────────────────────────────────

#[tokio::test]
async fn the_p_index_paginates_and_clamps() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    for index in 1..=5 {
        create_page(
            &server,
            &admin,
            &format!("pagina-{index}"),
            &format!("Página número {index}"),
            &format!("Contenido de la página {index}."),
        )
        .await;
    }

    let first = reqwest::get(server.url("/p")).await.expect("request");
    assert_eq!(first.status(), 200);
    let body = first.text().await.expect("html body");
    assert!(body.contains("Página 1 de 3"), "page counter: {body}");
    assert!(body.contains("Siguiente"), "next edge link: {body}");
    assert!(!body.contains("Anterior"), "no prev on page one: {body}");
    // Newest first: page one carries the two latest pages.
    assert!(
        body.contains("pagina-5") && body.contains("pagina-4"),
        "items: {body}"
    );
    assert!(!body.contains("pagina-3"), "window of two: {body}");

    let second = reqwest::get(server.url("/p?page=2"))
        .await
        .expect("request");
    assert_eq!(second.status(), 200);
    let body = second.text().await.expect("html body");
    assert!(body.contains("Página 2 de 3") && body.contains("pagina-2"));
    assert!(body.contains("Anterior") && body.contains("Siguiente"));

    let third = reqwest::get(server.url("/p?page=3"))
        .await
        .expect("request");
    let body = third.text().await.expect("html body");
    assert!(body.contains("Página 3 de 3") && body.contains("pagina-1"));
    assert!(body.contains("Anterior"));
    assert!(!body.contains("Siguiente"), "no next on the last page");

    // Out-of-range and garbage numbers clamp to the nearest real page.
    for (path, expected) in [
        ("/p?page=99", "Página 3 de 3"),
        ("/p?page=0", "Página 1 de 3"),
        ("/p?page=-4", "Página 1 de 3"),
        ("/p?page=basura", "Página 1 de 3"),
    ] {
        let response = reqwest::get(server.url(path)).await.expect("request");
        assert_eq!(response.status(), 200);
        let body = response.text().await.expect("html body");
        assert!(body.contains(expected), "{path} → {expected}: {body}");
    }
}

#[tokio::test]
async fn the_header_carries_the_search_form() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;

    let response = reqwest::get(server.url("/p")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("action=\"/buscar\"") && body.contains("name=\"q\""),
        "the shared header renders the search form: {body}"
    );
}

// ─── `GET /buscar`: the public search ───────────────────────────────

#[tokio::test]
async fn the_public_search_round_trips_with_escapes_and_operators() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    create_page(
        &server,
        &admin,
        "receta",
        "Receta de tarta",
        "la tarta <img src=x onerror=alert(1)> casera se hornea despacio",
    )
    .await;
    create_page(
        &server,
        &admin,
        "ajena",
        "Página ajena",
        "nada que ver aqui",
    )
    .await;

    // A hit: title link, highlighted snippet, no raw markup from the
    // body (the fragment is escaped by construction).
    let response = reqwest::get(server.url("/buscar?q=tarta"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<mark>tarta</mark>"),
        "highlighted hit: {body}"
    );
    assert!(body.contains("href=\"/p/receta\""), "result link: {body}");
    assert!(
        body.contains("&lt;img"),
        "body markup escaped in the snippet: {body}"
    );
    assert!(
        !body.contains("<img src=x"),
        "no raw markup rides along: {body}"
    );

    // FTS5 operators are inert literals: this ANDs the three words,
    // and no page contains the literal token "OR".
    let response = reqwest::get(server.url("/buscar?q=tarta%20OR"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("No hay resultados"),
        "operators behave as words, not syntax: {body}"
    );

    // Embedded quotes are stripped, not FTS syntax.
    let response = reqwest::get(server.url("/buscar?q=%22tarta%22"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<mark>tarta</mark>"),
        "quotes stripped: {body}"
    );

    // A query with markup in it is echoed escaped, never rendered.
    let response = reqwest::get(server.url("/buscar?q=%3Cb%3E"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("No hay resultados") && body.contains("&lt;b&gt;"),
        "the echoed query is escaped: {body}"
    );

    // The empty query is an invitation, not an error.
    let response = reqwest::get(server.url("/buscar")).await.expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Escribe lo que buscas"),
        "empty-state hint: {body}"
    );

    // Quotes-only sanitizes to nothing: same friendly state.
    let response = reqwest::get(server.url("/buscar?q=%22%20%22"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(body.contains("Escribe lo que buscas"), "no query: {body}");
}

#[tokio::test]
async fn search_results_paginate_carrying_the_query() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    for index in 1..=3 {
        create_page(
            &server,
            &admin,
            &format!("iguana-{index}"),
            &format!("Iguana {index}"),
            &format!("La iguana número {index} descansa al sol."),
        )
        .await;
    }

    let first = reqwest::get(server.url("/buscar?q=iguana"))
        .await
        .expect("request");
    assert_eq!(first.status(), 200);
    let body = first.text().await.expect("html body");
    assert!(body.contains("3 resultado(s)"), "total: {body}");
    assert!(body.contains("Página 1 de 2") && body.contains("Siguiente"));
    assert!(
        body.contains("href=\"/buscar?q=iguana&amp;page=2\""),
        "the query rides along in the page links: {body}"
    );

    let second = reqwest::get(server.url("/buscar?q=iguana&page=2"))
        .await
        .expect("request");
    let body = second.text().await.expect("html body");
    assert!(body.contains("Página 2 de 2") && body.contains("Anterior"));
    assert!(
        body.contains("iguana-3") && !body.contains("iguana-1"),
        "window: {body}"
    );
}

#[tokio::test]
async fn the_index_follows_the_page_lifecycle() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // A draft is invisible to the public search.
    let id = create_page_full(
        &server,
        &admin,
        "secreto",
        "Página secreta",
        "el ingrediente secreto de la receta",
        false,
        None,
    )
    .await;
    let response = reqwest::get(server.url("/buscar?q=secreto"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("No hay resultados"),
        "drafts stay private: {body}"
    );

    // Publishing indexes it (the insert trigger ran while it was a
    // draft; the search gate hides it — now it flows through).
    update_page(
        &server,
        &admin,
        id,
        "secreto",
        "Página secreta",
        "el ingrediente secreto",
        true,
    )
    .await;
    let response = reqwest::get(server.url("/buscar?q=secreto"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<mark>secreto</mark>"),
        "published is found: {body}"
    );

    // Rewording retires the old token and indexes the new one.
    update_page(
        &server,
        &admin,
        id,
        "secreto",
        "Página secreta",
        "el ingrediente confidencial de la receta",
        true,
    )
    .await;
    let response = reqwest::get(server.url("/buscar?q=secreto"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("No hay resultados"),
        "the update trigger retired the old words: {body}"
    );
    let response = reqwest::get(server.url("/buscar?q=confidencial"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<mark>confidencial</mark>"),
        "new words indexed: {body}"
    );

    // Deleting removes it from the index entirely.
    delete_page(&server, &admin, id).await;
    let response = reqwest::get(server.url("/buscar?q=confidencial"))
        .await
        .expect("request");
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("No hay resultados"),
        "delete unindexes: {body}"
    );
}

// ─── `GET /admin/pages?q=`: the editor's filter ─────────────────────

#[tokio::test]
async fn the_admin_filter_finds_references_and_respects_the_roles() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // A published page plus a draft, both carrying the reference an
    // editor would want to find (a media URL inside a body). FTS5
    // tokenizes the 32-hex stem as ONE token — the query must carry
    // it whole, exactly like an editor pasting it from a page.
    let referencia = "8f3a11b2c9d4e5f6a7b8c9d0e1f2a3b4";
    create_page(
        &server,
        &admin,
        "galeria",
        "Galería",
        &format!("las fotos viven en /media/{referencia}/foto.png"),
    )
    .await;
    create_page_full(
        &server,
        &admin,
        "borrador-foto",
        "Borrador con foto",
        &format!("pendiente de revisar la foto del /media/{referencia}"),
        false,
        None,
    )
    .await;

    // Anonymous visitors are redirected to the login page.
    let response = anon_client()
        .get(server.url(&format!("/admin/pages?q={referencia}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    assert!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|location| location.contains("/login")),
        "anonymous → login"
    );

    // Regular users keep their 403.
    reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": "plain-user", "password": "sup3r-secret!" }))
        .send()
        .await
        .expect("registration");
    let user = login_browser(&server, "plain-user", "sup3r-secret!").await;
    let response = user
        .get(server.url(&format!("/admin/pages?q={referencia}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403, "user role cannot filter");

    // Editors get the flat, ranked list: both rows (draft included),
    // fragments with highlights, and the badge telling them which is
    // which.
    let response = admin
        .get(server.url(&format!("/admin/pages?q={referencia}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("2 resultado(s)"), "both rows match: {body}");
    assert!(body.contains("Galería") && body.contains("Borrador con foto"));
    assert!(
        body.contains(&format!("<mark>{referencia}</mark>")),
        "the reference is highlighted: {body}"
    );
    assert!(body.contains("borrador</span>"), "draft badge: {body}");

    // Without a filter, the tree comes back (the plain listing).
    let response = admin.get(server.url("/admin/pages")).send().await.unwrap();
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Galería") && !body.contains("resultado(s)"),
        "no filter, no results branch: {body}"
    );

    // A filter nobody matches says so.
    let response = admin
        .get(server.url("/admin/pages?q=inexistente"))
        .send()
        .await
        .unwrap();
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("Ninguna página"),
        "empty filter state: {body}"
    );
}

// ─── The feeds: `/feed.xml` and `/atom.xml` ─────────────────────────

#[tokio::test]
async fn the_feeds_serve_published_pages_with_absolute_urls() {
    let (mut config, _db) = findability_config();
    config.cms.site_url = Some(String::from("https://feeds.example"));
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    create_page(
        &server,
        &admin,
        "primera",
        "Primera página",
        "contenido de la primera página",
    )
    .await;
    create_page_full(
        &server,
        &admin,
        "segunda",
        "Segunda página",
        "contenido de la segunda página",
        true,
        Some("La descripción SEO de la segunda página"),
    )
    .await;
    // A draft: never in a feed.
    create_page_full(
        &server,
        &admin,
        "borrador",
        "Borrador",
        "contenido privado",
        false,
        None,
    )
    .await;

    let response = reqwest::get(server.url("/feed.xml"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/rss+xml; charset=utf-8")
    );
    let rss = response.text().await.expect("rss body");
    assert!(rss.contains("<rss version=\"2.0\""), "rss root: {rss}");
    assert_eq!(rss.matches("<item>").count(), 2, "published only: {rss}");
    assert!(
        !rss.contains("borrador") && !rss.contains("private"),
        "drafts excluded: {rss}"
    );
    assert!(
        rss.contains("<link>https://feeds.example/p/segunda</link>"),
        "absolute links from site_url: {rss}"
    );
    assert!(
        rss.contains("<description>La descripción SEO de la segunda página</description>"),
        "SEO description as item description: {rss}"
    );
    // The newest page leads (updated_at DESC, id DESC tiebreak).
    let segunda_pos = rss.find("segunda").expect("second present");
    let primera_pos = rss.find("primera").expect("first present");
    assert!(segunda_pos < primera_pos, "newest first: {rss}");
    // RSS dates are RFC 822 with an explicit UTC offset.
    assert!(rss.contains(" +0000</pubDate>"), "RFC 822 dates: {rss}");

    let response = reqwest::get(server.url("/atom.xml"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/atom+xml; charset=utf-8")
    );
    let atom = response.text().await.expect("atom body");
    assert!(
        atom.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\""),
        "atom root: {atom}"
    );
    assert_eq!(atom.matches("<entry>").count(), 2, "published only: {atom}");
    assert!(
        atom.contains("<id>https://feeds.example/atom.xml</id>"),
        "feed id: {atom}"
    );
    assert!(
        atom.contains("<summary>La descripción SEO de la segunda página</summary>"),
        "SEO description as summary: {atom}"
    );
    // Atom timestamps are RFC 3339.
    assert!(
        atom.contains("</updated>\n") && atom.contains("T"),
        "RFC 3339: {atom}"
    );
    let updated = atom
        .split("<updated>")
        .nth(1)
        .and_then(|rest| rest.split("</updated>").next())
        .expect("updated element");
    assert!(
        updated.ends_with('Z') && updated.contains('T'),
        "shape: {updated}"
    );
}

#[tokio::test]
async fn the_feeds_fall_back_to_the_request_host() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(&server, &admin, "unica", "Única", "contenido").await;

    // No site_url: the Host header carries the visible origin.
    let response = reqwest::get(server.url("/feed.xml"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let rss = response.text().await.expect("rss body");
    assert!(
        rss.contains("http://127.0.0.1:") && rss.contains("/p/unica"),
        "host fallback: {rss}"
    );
}

#[tokio::test]
async fn the_feeds_can_be_switched_off() {
    let (mut config, _db) = findability_config();
    config.cms.feed = false;
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(&server, &admin, "unica", "Única", "contenido").await;

    for path in ["/feed.xml", "/atom.xml"] {
        let response = reqwest::get(server.url(path)).await.expect("request");
        assert_eq!(response.status(), 404, "{path} off");
    }
}

// ─── The media grid pagination ──────────────────────────────────────

/// Encodes a small RGB gradient PNG in memory.
fn image_fixture(format: image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let mut image = image::RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let value = ((x + y) % 256) as u8;
        *pixel = image::Rgb([value, 255 - value, value / 2]);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut std::io::Cursor::new(&mut bytes), format)
        .expect("fixture encodes");
    bytes
}

/// Builds a `multipart/form-data` body: an optional `alt` field plus
/// the `file` part. Returns (body, boundary).
fn multipart_body(alt: Option<&str>, file_name: &str, file_bytes: &[u8]) -> (Vec<u8>, String) {
    let boundary = format!("wmsBoundary{}", std::process::id());
    let mut body = Vec::new();
    if let Some(text) = alt {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\ncontent-disposition: form-data; name=\"alt\"\r\n\r\n{text}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; \
             filename=\"{file_name}\"\r\ncontent-type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (body, boundary)
}

/// Posts an upload and returns the response.
async fn post_upload(
    server: &TestServer,
    client: &reqwest::Client,
    alt: Option<&str>,
    file_name: &str,
    file_bytes: &[u8],
) -> reqwest::Response {
    let (body, boundary) = multipart_body(alt, file_name, file_bytes);
    client
        .post(server.url("/admin/media"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .expect("upload request succeeds")
}

#[tokio::test]
async fn the_media_grid_paginates() {
    let (config, _db) = findability_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // 25 uploads against a 24-per-page grid.
    let fixture = image_fixture(image::ImageFormat::Png, 8, 8);
    for index in 0..25 {
        let response = post_upload(
            &server,
            &admin,
            None,
            &format!("imagen-{index}.png"),
            &fixture,
        )
        .await;
        assert_eq!(response.status(), 303, "upload {index} redirects (PRG)");
    }

    let first = admin.get(server.url("/admin/media")).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let body = first.text().await.expect("html body");
    assert!(body.contains("25 archivos en total"), "total: {body}");
    assert!(body.contains("Página 1 de 2") && body.contains("Siguiente"));

    let second = admin
        .get(server.url("/admin/media?page=2"))
        .send()
        .await
        .unwrap();
    let body = second.text().await.expect("html body");
    assert!(body.contains("Página 2 de 2") && body.contains("Anterior"));

    // Far-out page numbers clamp to the last real page.
    let clamped = admin
        .get(server.url("/admin/media?page=99"))
        .send()
        .await
        .unwrap();
    let body = clamped.text().await.expect("html body");
    assert!(body.contains("Página 2 de 2"), "clamped: {body}");
}
