//! The public search page (F10): the visitor's words against the
//! published index, zero JavaScript, HTML only.
//!
//! The battery pins the page's own contract — the pieces the FTS5
//! plumbing underneath (already unit-tested in `db.rs`) does not
//! cover on its own:
//!
//! - an empty — or quotes-only, which sanitizes to nothing — query
//!   renders the invitation instead of an error: a search box never
//!   answers 4xx;
//! - the results link their pages, and the snippet highlights the
//!   hit through the engine's `⟦ ⟧` markers — the highlight is
//!   escaped by construction, and a stray `<mark>` typed into a page
//!   body arrives as text, never as markup;
//! - the title and the content are both searched; drafts are not;
//! - FTS5's own operators (`OR`, `NEAR(…)`, `title:`) stay inert
//!   literals: every token is quoted, so a visitor cannot widen,
//!   narrow or crash the match;
//! - the pagination carries the query along, re-encoded, and the
//!   visitor's words are echoed back escaped.
//!
//! The F20 per-organization scoping lives in `tests/tenant_cms.rs`;
//! the legacy `/buscar` redirect in `tests/cms_error_pages.rs`.

mod common;

use common::{auth_config, TestServer};
use wallermax_server::config::AppConfig;

// ─── Harness ────────────────────────────────────────────────────────

/// A configuration with everything the search page needs, on a fresh
/// database (a single-host CMS: the organization is "cms" by
/// construction).
fn search_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    (config, db)
}

/// A cookie-storing client: `Set-Cookie` in, `Cookie` out — a browser
/// that never follows redirects (each hop is asserted by hand).
fn browser_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .expect("browser client builds")
}

/// Registers the bootstrap admin and returns a logged-in browser.
async fn admin(server: &TestServer) -> reqwest::Client {
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&serde_json::json!({ "username": "justo", "password": "secreto-123456" }))
        .send()
        .await
        .expect("registration succeeds");
    assert_eq!(
        response.status(),
        201,
        "the first account becomes the admin"
    );

    let client = browser_client();
    let response = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=justo&password=secreto-123456&redirect=/")
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    client
}

/// Creates a page through the admin panel forms.
async fn create_page(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
) {
    let body = format!(
        "slug={}&title={}&content={}&redirect=%2Fadmin%2Fpages{}",
        slug,
        enc(title),
        enc(content),
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
}

/// Minimal percent-encoding for form bodies (the characters the tests
/// actually use).
fn enc(value: &str) -> String {
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

/// GETs `path` (with its raw query string) anonymously.
async fn get(server: &TestServer, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(server.url(path))
        .send()
        .await
        .expect("request")
}

// ─── The battery ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_empty_query_invites_instead_of_erroring() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/search").await;
    assert_eq!(response.status(), 200, "a search box never answers 4xx");
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("action=\"/search\"") && body.contains("name=\"q\""),
        "the form is there: {body}"
    );
    assert!(
        body.contains("Type what you are looking for"),
        "the invitation: {body}"
    );
    assert!(
        !body.contains("class=\"resultado\""),
        "no results section for the empty query: {body}"
    );
}

#[tokio::test]
async fn results_link_their_pages_and_highlight_the_hits() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;
    let admin = admin(&server).await;

    create_page(
        &server,
        &admin,
        "sobre-la-heno",
        "Sobre la paja",
        "<p>the haystack full of dry grass and one needle</p>",
        true,
    )
    .await;

    let response = get(&server, "/search?q=needle").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("1 result(s) for"),
        "the count leads the listing: {body}"
    );
    assert!(
        body.contains("href=\"/p/sobre-la-heno\""),
        "the result links its page: {body}"
    );
    assert!(
        body.contains("Sobre la paja"),
        "the result shows the page's title: {body}"
    );
    assert!(
        body.contains("<mark>needle</mark>"),
        "the hit is highlighted where it stands: {body}"
    );
}

#[tokio::test]
async fn the_title_is_searched_too() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;
    let admin = admin(&server).await;

    // The term lives in the title only; the body never says it.
    create_page(
        &server,
        &admin,
        "la-guia",
        "The guidance needle",
        "<p>nothing about the findable word here</p>",
        true,
    )
    .await;

    let response = get(&server, "/search?q=needle").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("href=\"/p/la-guia\""),
        "a title hit is a hit: {body}"
    );
}

#[tokio::test]
async fn drafts_stay_out_of_the_public_index() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;
    let admin = admin(&server).await;

    create_page(
        &server,
        &admin,
        "borrador-aguja",
        "Draft needle",
        "<p>the unpublished needle</p>",
        false,
    )
    .await;

    let response = get(&server, "/search?q=needle").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("No results for"),
        "drafts are the admin filter's business, never the visitor's: {body}"
    );
    assert!(
        !body.contains("href=\"/p/borrador-aguja\""),
        "the draft never links: {body}"
    );
}

#[tokio::test]
async fn no_results_is_an_answer_not_an_error() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;

    let response = get(&server, "/search?q=zzzz-nada-de-nada").await;
    assert_eq!(response.status(), 200, "zero hits is still a page");
    let body = response.text().await.expect("search page");
    assert!(body.contains("No results for"), "the explanation: {body}");
    assert!(
        body.contains("action=\"/search\""),
        "the form stays for the next try: {body}"
    );
}

#[tokio::test]
async fn quotes_only_sanitize_to_the_invitation() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;

    // `""` sanitizes to no query at all (the embedded quotes are
    // stripped, nothing quotable remains): the page answers the
    // invitation, not an empty result set for words the visitor
    // never really typed.
    let response = get(&server, "/search?q=%22%22").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("Type what you are looking for"),
        "the invitation: {body}"
    );
    assert!(
        !body.contains("No results for"),
        "not an empty result set: {body}"
    );
}

#[tokio::test]
async fn fts_operators_stay_inert_literals() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;
    let admin = admin(&server).await;

    create_page(
        &server,
        &admin,
        "la-cocina",
        "The kitchen",
        "<p>the kitchen with the wood stove</p>",
        true,
    )
    .await;

    // Every token is quoted, so the operators cannot widen the
    // match: `kitchen stove` (an implicit AND) finds the page...
    let response = get(&server, "/search?q=kitchen%20stove").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("href=\"/p/la-cocina\""),
        "both words present, both quoted, one hit: {body}"
    );

    // ...while `kitchen NEAR(stove)` is the literal token
    // "NEAR(stove)" — not an operator, and not in the page.
    let response = get(&server, "/search?q=kitchen%20NEAR%28stove%29").await;
    assert_eq!(
        response.status(),
        200,
        "an operator-shaped query is a search, never an error"
    );
    let body = response.text().await.expect("search page");
    assert!(
        !body.contains("href=\"/p/la-cocina\""),
        "the operator acts as an inert literal: {body}"
    );

    // A column filter is a literal token too: nothing crashes and
    // the match does not widen.
    let response = get(&server, "/search?q=title%3Akitchen").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        !body.contains("href=\"/p/la-cocina\""),
        "title: is not a filter when it is quoted: {body}"
    );
}

#[tokio::test]
async fn pagination_carries_the_query_along() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;
    let admin = admin(&server).await;

    // Twelve pages carrying the term: the default page size is ten.
    for number in 1..=12 {
        create_page(
            &server,
            &admin,
            &format!("husk-{number:02}"),
            &format!("Husk page {number}"),
            "<p>the dry husk of the coconut</p>",
            true,
        )
        .await;
    }

    let response = get(&server, "/search?q=husk").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("12 result(s) for"),
        "the count is exact: {body}"
    );
    assert_eq!(
        body.matches("class=\"resultado\"").count(),
        10,
        "the default page size is ten"
    );
    assert!(
        body.contains("page=2"),
        "the next page carries the query: {body}"
    );

    let response = get(&server, "/search?q=husk&page=2").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert_eq!(
        body.matches("class=\"resultado\"").count(),
        2,
        "the tail lands on page two"
    );
}

#[tokio::test]
async fn the_visitors_words_are_echoed_escaped() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;

    // The query is echoed back through the auto-escaping `<?= ?>`:
    // markup typed into the search box arrives as text.
    let response = get(&server, "/search?q=%3Cscript%3Ealert%281%29%3C%2Fscript%3E").await;
    assert_eq!(response.status(), 200, "a search box never answers 4xx");
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("&lt;script&gt;alert(1)&lt;&#x2F;script&gt;"),
        "the echo escapes: {body}"
    );
    assert!(
        !body.contains("<script>alert"),
        "the echo never injects: {body}"
    );
}

#[tokio::test]
async fn a_stray_highlight_marker_cannot_inject_markup() {
    let (config, _db) = search_config();
    let server = TestServer::start_full(config).await;
    let admin = admin(&server).await;

    // A page body carrying its own literal <mark>: the engine's
    // highlight is the real one, and the typed marker arrives as
    // text — never as markup.
    create_page(
        &server,
        &admin,
        "marcas",
        "The marks",
        "<p>the body says <mark>evil</mark> literally</p>",
        true,
    )
    .await;
    let response = get(&server, "/search?q=evil").await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("search page");
    assert!(
        body.contains("<mark>evil</mark>"),
        "the engine's own highlight stands: {body}"
    );
    assert!(
        body.contains("&lt;mark&gt;"),
        "the typed marker arrives as text: {body}"
    );
    assert!(
        !body.contains("<mark>&lt;mark&gt;"),
        "the typed marker is never markup: {body}"
    );
}
