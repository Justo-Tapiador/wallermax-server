//! Corporate content model battery (F7).
//!
//! Boots the **full** server exactly like the binary (fresh temporary
//! SQLite, auth, templates, static files and the CMS) and drives it
//! with a cookie-storing `reqwest` client through plain HTML forms —
//! the same "closest thing to a browser" approach as `tests/cms.rs`.
//!
//! Covered: the page hierarchy (parent/position, the cycle-proof move
//! guard, breadcrumbs, the reparent-on-delete rule), the admin tree
//! listing, the named menus over `/admin/menus` (page and custom-URL
//! items, the draft-skipping `menus` template global), the SEO
//! metadata in the wrapper's `<head>`, and `/sitemap.xml` (absolute
//! `<loc>` from `[cms] site_url` or the request host, the config
//! flag, the canonical homepage while `default_page` is published).

mod common;

use common::{auth_config, TestServer};
use serde_json::json;
use wallermax_server::config::AppConfig;

/// A configuration with everything the corporate CMS needs, on a
/// fresh database. `site_url` feeds the sitemap's absolute `<loc>`.
fn corporate_config(site_url: Option<&str>) -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    config.cms.site_url = site_url.map(str::to_owned);
    (config, db)
}

/// A cookie-storing client: `Set-Cookie` in, `Cookie` out — a browser
/// with manual redirects so each hop can be asserted.
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

/// Minimal percent-encoding for form bodies.
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

/// A page payload with the F7 knobs (parent, position, SEO).
#[derive(Default)]
struct PageSpec<'a> {
    slug: &'a str,
    title: &'a str,
    content: &'a str,
    publish: bool,
    parent: Option<i64>,
    position: Option<i64>,
    meta_title: Option<&'a str>,
    meta_description: Option<&'a str>,
    og_image: Option<&'a str>,
}

/// Creates a page through the admin panel forms, returning its id.
async fn create_page(server: &TestServer, client: &reqwest::Client, spec: &PageSpec<'_>) -> i64 {
    let mut body = format!(
        "slug={}&title={}&content={}&position={}",
        spec.slug,
        urlencoding_simple(spec.title),
        urlencoding_simple(spec.content),
        spec.position.unwrap_or(0),
    );
    if let Some(parent) = spec.parent {
        body.push_str(&format!("&parent_id={parent}"));
    }
    if let Some(meta_title) = spec.meta_title {
        body.push_str(&format!("&meta_title={}", urlencoding_simple(meta_title)));
    }
    if let Some(description) = spec.meta_description {
        body.push_str(&format!(
            "&meta_description={}",
            urlencoding_simple(description)
        ));
    }
    if let Some(og_image) = spec.og_image {
        body.push_str(&format!("&og_image={}", urlencoding_simple(og_image)));
    }
    if spec.publish {
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

/// Moves a page to a new parent (or the top level) through the edit
/// form, returning the re-rendered response for error assertions.
async fn move_page(
    server: &TestServer,
    client: &reqwest::Client,
    id: i64,
    slug: &str,
    parent: Option<i64>,
) -> reqwest::Response {
    let mut body = format!("slug={slug}&title=Movida&content=%3Cp%3Ex%3C%2Fp%3E&position=0");
    if let Some(parent) = parent {
        body.push_str(&format!("&parent_id={parent}"));
    }
    body.push_str("&is_published=on");
    client
        .post(server.url(&format!("/admin/pages/{id}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("page move succeeds")
}

/// Creates a menu through the admin panel, returning its id.
async fn create_menu(
    server: &TestServer,
    client: &reqwest::Client,
    name: &str,
    title: &str,
) -> i64 {
    let response = client
        .post(server.url("/admin/menus"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("name={name}&title={}", urlencoding_simple(title)))
        .send()
        .await
        .expect("menu creation succeeds");
    assert_eq!(response.status(), 303, "menu creation redirects");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    location
        .trim_start_matches("/admin/menus/")
        .split('?')
        .next()
        .expect("path")
        .parse()
        .expect("numeric menu id")
}

/// A menu item payload: a page link or a custom URL.
struct ItemSpec<'a> {
    label: Option<&'a str>,
    page: Option<i64>,
    url: Option<&'a str>,
    position: i64,
}

/// Adds an item to a menu, returning the response (status varies with
/// validation outcomes).
async fn add_item(
    server: &TestServer,
    client: &reqwest::Client,
    menu_id: i64,
    spec: &ItemSpec<'_>,
) -> reqwest::Response {
    let mut body = format!("position={}", spec.position);
    if let Some(label) = spec.label {
        body.push_str(&format!("&label={}", urlencoding_simple(label)));
    }
    if let Some(page) = spec.page {
        body.push_str(&format!("&page_id={page}"));
    }
    if let Some(url) = spec.url {
        body.push_str(&format!("&url={}", urlencoding_simple(url)));
    }
    client
        .post(server.url(&format!("/admin/menus/{menu_id}/items")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("item creation succeeds")
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

// ── Hierarchy ────────────────────────────────────────────────────────

#[tokio::test]
async fn child_pages_render_breadcrumbs_and_seo_metadata() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let parent = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "servicios",
            title: "Servicios",
            content: "<p>índice de servicios</p>",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    let child = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "diseno-web",
            title: "Diseño web",
            content: "<p>páginas que venden</p>",
            publish: true,
            parent: Some(parent),
            position: Some(2),
            meta_title: Some("Diseño web corporativo en Madrid"),
            meta_description: Some("Páginas web rápidas y seguras para empresas."),
            og_image: Some("/assets/og-diseno.png"),
        },
    )
    .await;
    assert_ne!(parent, child, "distinct pages");

    let response = anon_client()
        .get(server.url("/p/diseno-web"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");

    // Breadcrumbs: Inicio · Servicios (the parent link), then the title.
    assert!(body.contains("migas"), "the breadcrumb block: {body}");
    assert!(
        body.contains("href=\"/p/servicios\""),
        "the parent breadcrumb link: {body}"
    );
    assert!(body.contains("Servicios"), "the parent title: {body}");

    // SEO: title override, description, og:image.
    assert!(
        body.contains("<title>Diseño web corporativo en Madrid — wallermax</title>"),
        "the meta title override: {body}"
    );
    assert!(
        body.contains(
            "<meta name=\"description\" content=\"Páginas web rápidas y seguras para empresas.\">"
        ),
        "the meta description: {body}"
    );
    assert!(
        body.contains("<meta property=\"og:image\" content=\"/assets/og-diseno.png\">"),
        "the og:image: {body}"
    );
}

#[tokio::test]
async fn legacy_pages_render_unchanged_without_hierarchy_or_seo() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "plana",
            title: "Página plana",
            content: "<p>sin jerarquía ni SEO</p>",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;

    let response = anon_client()
        .get(server.url("/p/plana"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("<title>Página plana — wallermax</title>"),
        "the title falls back to the page title: {body}"
    );
    assert!(!body.contains("migas"), "no breadcrumbs without a parent");
    assert!(
        !body.contains("meta name=\"description\""),
        "no description without metadata: {body}"
    );
}

#[tokio::test]
async fn the_parent_select_excludes_the_page_and_its_descendants() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // A > B > C chain plus an unrelated page R.
    let a = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "a",
            title: "Página A",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    let b = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "b",
            title: "Página B",
            content: "x",
            publish: true,
            parent: Some(a),
            ..PageSpec::default()
        },
    )
    .await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "c",
            title: "Página C",
            content: "x",
            publish: true,
            parent: Some(b),
            ..PageSpec::default()
        },
    )
    .await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "r",
            title: "Página R",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;

    // B's form: no B, no C (B's subtree), but A and R present.
    let body = admin
        .get(server.url(&format!("/admin/pages/{b}/edit")))
        .send()
        .await
        .expect("edit form")
        .text()
        .await
        .expect("html");
    assert!(
        !body.contains(&format!("<option value=\"{b}\"")),
        "B is not its own parent option"
    );
    let b_options = body.split("<option").skip(1).collect::<Vec<_>>();
    assert!(
        b_options.iter().any(|option| option.contains("Página A")),
        "the ancestor A is available: {body}"
    );
    assert!(
        b_options.iter().any(|option| option.contains("Página R")),
        "the unrelated R is available: {body}"
    );
    assert!(
        !b_options.iter().any(|option| option.contains("Página B")),
        "B itself is not available: {body}"
    );
    assert!(
        !b_options.iter().any(|option| option.contains("Página C")),
        "B's descendant C is not available: {body}"
    );
}

#[tokio::test]
async fn parent_cycles_and_missing_parents_are_rejected() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let a = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "raiz",
            title: "Raíz",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    let b = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "hija",
            title: "Hija",
            content: "x",
            publish: true,
            parent: Some(a),
            ..PageSpec::default()
        },
    )
    .await;

    // A under B (B already hangs under A): a cycle.
    let response = move_page(&server, &admin, a, "raiz", Some(b)).await;
    assert_eq!(response.status(), 200, "the form re-renders, no redirect");
    let body = response.text().await.expect("html body");
    assert!(body.contains("ciclo"), "the cycle error: {body}");

    // A under A: itself.
    let response = move_page(&server, &admin, a, "raiz", Some(a)).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("propia padre"),
        "the self-parent error: {body}"
    );

    // A under a page that does not exist.
    let response = move_page(&server, &admin, a, "raiz", Some(99_999)).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("no existe"),
        "the missing-parent error: {body}"
    );
}

#[tokio::test]
async fn deleting_a_parent_reparents_children_to_the_top() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let parent = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "rama",
            title: "Rama",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "supervive",
            title: "Supervive",
            content: "<p>sigue aquí</p>",
            publish: true,
            parent: Some(parent),
            ..PageSpec::default()
        },
    )
    .await;

    let response = admin
        .post(server.url(&format!("/admin/pages/{parent}/delete")))
        .send()
        .await
        .expect("deletion");
    assert_eq!(response.status(), 303, "deletion redirects");

    // The child survives at the top level: same URL, no breadcrumbs.
    let response = anon_client()
        .get(server.url("/p/supervive"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200, "the child keeps serving");
    let body = response.text().await.expect("html body");
    assert!(body.contains("sigue aquí"), "the child content: {body}");
    assert!(
        !body.contains("migas"),
        "reparented: no breadcrumbs anymore"
    );
}

#[tokio::test]
async fn position_orders_siblings_in_the_admin_tree() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "tercera",
            title: "Tercera",
            content: "x",
            publish: true,
            position: Some(30),
            ..PageSpec::default()
        },
    )
    .await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "primera",
            title: "Primera",
            content: "x",
            publish: true,
            position: Some(10),
            ..PageSpec::default()
        },
    )
    .await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "segunda",
            title: "Segunda",
            content: "x",
            publish: true,
            position: Some(20),
            ..PageSpec::default()
        },
    )
    .await;

    let body = admin
        .get(server.url("/admin/pages"))
        .send()
        .await
        .expect("listing")
        .text()
        .await
        .expect("html");
    let first = body.find("Primera").expect("Primera listed");
    let second = body.find("Segunda").expect("Segunda listed");
    let third = body.find("Tercera").expect("Tercera listed");
    assert!(first < second && second < third, "position order: {body}");
}

// ── Menus ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_menus_global_resolves_pages_and_skips_drafts() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let publicada = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "quienes-somos",
            title: "Quiénes somos",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    let borrador = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "proximamente",
            title: "Próximamente",
            content: "x",
            publish: false,
            ..PageSpec::default()
        },
    )
    .await;

    let menu = create_menu(&server, &admin, "principal", "Navegación principal").await;
    // A page item without label (falls back to the page title).
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: None,
            page: Some(publicada),
            url: None,
            position: 1,
        },
    )
    .await;
    assert_eq!(response.status(), 303, "page item added");
    // A draft page item: stored, but skipped publicly.
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: None,
            page: Some(borrador),
            url: None,
            position: 2,
        },
    )
    .await;
    assert_eq!(response.status(), 303, "draft item added (admin sees it)");
    // A custom URL item.
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: Some("Blog externo"),
            page: None,
            url: Some("https://blog.example.com"),
            position: 3,
        },
    )
    .await;
    assert_eq!(response.status(), 303, "url item added");

    // A page whose body echoes the resolved global through the sandbox.
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "eco-menus",
            title: "Eco",
            content: "<?jhs echo(raw(JSON.stringify(menus.principal))) ?>",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;

    let body = anon_client()
        .get(server.url("/p/eco-menus"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("html body");
    assert!(
        body.contains("{\"href\":\"/p/quienes-somos\",\"label\":\"Quiénes somos\"}"),
        "the page item resolves to the page title and /p/<slug>: {body}"
    );
    assert!(
        body.contains("{\"href\":\"https://blog.example.com\",\"label\":\"Blog externo\"}"),
        "the custom URL item passes through: {body}"
    );
    assert!(
        !body.contains("proximamente"),
        "the draft item is skipped in the public global: {body}"
    );
}

#[tokio::test]
async fn menus_are_managed_over_the_admin_forms() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // Create.
    let menu = create_menu(&server, &admin, "pie", "Pie de página").await;
    let response = admin
        .get(server.url(&format!("/admin/menus/{menu}")))
        .send()
        .await
        .expect("detail");
    assert_eq!(response.status(), 200, "the detail renders");
    let body = response.text().await.expect("html body");
    assert!(body.contains("menus.pie"), "the template key shows: {body}");

    // Rename the title (the name stays immutable).
    let response = admin
        .post(server.url(&format!("/admin/menus/{menu}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("title=Pie%20del%20sitio")
        .send()
        .await
        .expect("rename");
    assert_eq!(response.status(), 303);
    let body = admin
        .get(server.url(&format!("/admin/menus/{menu}")))
        .send()
        .await
        .expect("detail")
        .text()
        .await
        .expect("html body");
    assert!(body.contains("Pie del sitio"), "the new title: {body}");
    assert!(
        body.contains("menus.pie"),
        "the name did not change: {body}"
    );

    // Add, edit and delete an item.
    let page = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "contacto",
            title: "Contacto",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: None,
            page: Some(page),
            url: None,
            position: 1,
        },
    )
    .await;
    assert_eq!(response.status(), 303);

    // The item appears in the detail with its resolved href.
    let body = admin
        .get(server.url(&format!("/admin/menus/{menu}")))
        .send()
        .await
        .expect("detail")
        .text()
        .await
        .expect("html body");
    assert!(body.contains("/p/contacto"), "the item href: {body}");

    // Edit the item label through its edit form URL (parse the id).
    let item_id: i64 = body
        .split("/items/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|id| id.parse().ok())
        .expect("item id parsed from the detail");
    let response = admin
        .post(server.url(&format!("/admin/menus/{menu}/items/{item_id}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(
            "label=Escr%C3%ADbenos&page_id={page_id}&position=1"
                .replace("{page_id}", &page.to_string()),
        )
        .send()
        .await
        .expect("item edit");
    assert_eq!(response.status(), 303, "item edit redirects");
    let body = admin
        .get(server.url(&format!("/admin/menus/{menu}")))
        .send()
        .await
        .expect("detail")
        .text()
        .await
        .expect("html body");
    assert!(body.contains("Escríbenos"), "the edited label: {body}");

    // Delete the item.
    let response = admin
        .post(server.url(&format!("/admin/menus/{menu}/items/{item_id}/delete")))
        .send()
        .await
        .expect("item deletion");
    assert_eq!(response.status(), 303);
    let body = admin
        .get(server.url(&format!("/admin/menus/{menu}")))
        .send()
        .await
        .expect("detail")
        .text()
        .await
        .expect("html body");
    assert!(!body.contains("/p/contacto"), "the item is gone: {body}");

    // Delete the whole menu.
    let response = admin
        .post(server.url(&format!("/admin/menus/{menu}/delete")))
        .send()
        .await
        .expect("menu deletion");
    assert_eq!(response.status(), 303);
    let body = admin
        .get(server.url("/admin/menus"))
        .send()
        .await
        .expect("listing")
        .text()
        .await
        .expect("html body");
    assert!(!body.contains("menus.pie"), "the menu is gone: {body}");
}

#[tokio::test]
async fn item_forms_reject_missing_or_double_destinations() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    let menu = create_menu(&server, &admin, "pruebas", "Pruebas").await;
    let page = create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "destino",
            title: "Destino",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;

    // Neither page nor URL.
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: Some("Nada"),
            page: None,
            url: None,
            position: 0,
        },
    )
    .await;
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("necesita un destino"),
        "no destination: {body}"
    );

    // Both at once.
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: Some("Ambos"),
            page: Some(page),
            url: Some("https://x.example"),
            position: 0,
        },
    )
    .await;
    let body = response.text().await.expect("html body");
    assert!(body.contains("no ambas"), "double destination: {body}");

    // A custom URL without a label.
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: None,
            page: None,
            url: Some("https://x.example"),
            position: 0,
        },
    )
    .await;
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("necesitan una etiqueta"),
        "label required: {body}"
    );

    // A bad URL scheme.
    let response = add_item(
        &server,
        &admin,
        menu,
        &ItemSpec {
            label: Some("Malo"),
            page: None,
            url: Some("javascript:alert(1)"),
            position: 0,
        },
    )
    .await;
    let body = response.text().await.expect("html body");
    assert!(
        body.contains("debe empezar por"),
        "url scheme rejected: {body}"
    );
}

#[tokio::test]
async fn menu_management_requires_the_editor_role() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_user_with_role(&server, &admin, "plain-user", "user").await;
    let plain = login_browser(&server, "plain-user", "password-123").await;

    // Anonymous: redirected to the login view.
    let response = anon_client()
        .get(server.url("/admin/menus"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 303, "anonymous visitors are redirected");
    assert!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|location| location.starts_with("/login")),
        "the redirect goes to the login view"
    );

    // A plain user: the friendly 403.
    let response = plain
        .get(server.url("/admin/menus"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 403, "plain users cannot manage menus");
    let response = plain
        .post(server.url("/admin/menus"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("name=hack&title=Hack")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 403, "the guard covers POSTs too");

    // An editor manages them (the role exists since v0.8.0).
    create_user_with_role(&server, &admin, "editora", "editor").await;
    let editor = login_browser(&server, "editora", "password-123").await;
    let response = editor
        .get(server.url("/admin/menus"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200, "editors manage menus");
}

// ── Sitemap ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_sitemap_lists_published_pages_and_the_canonical_homepage() {
    let (mut config, _db) = corporate_config(Some("https://www.example.com"));
    config.cms.default_page = Some(String::from("inicio"));
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "inicio",
            title: "Inicio",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "publicada",
            title: "Publicada",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "borrador-sitemap",
            title: "Borrador",
            content: "x",
            publish: false,
            ..PageSpec::default()
        },
    )
    .await;

    let response = anon_client()
        .get(server.url("/sitemap.xml"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/xml")),
        "the sitemap is XML: {:?}",
        response.headers().get("content-type")
    );
    let body = response.text().await.expect("xml body");

    assert!(
        body.contains("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
        "the XML declaration: {body}"
    );
    assert!(
        body.contains("<loc>https://www.example.com/</loc>"),
        "the canonical homepage from default_page: {body}"
    );
    assert!(
        body.contains("<loc>https://www.example.com/p/publicada</loc>"),
        "the published page: {body}"
    );
    assert!(
        !body.contains("borrador-sitemap"),
        "drafts stay out of the sitemap: {body}"
    );
    assert!(
        body.contains("<lastmod>2"),
        "the lastmod dates are ISO years: {body}"
    );
}

#[tokio::test]
async fn the_sitemap_without_site_url_uses_the_request_host() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;
    create_page(
        &server,
        &admin,
        &PageSpec {
            slug: "una",
            title: "Una",
            content: "x",
            publish: true,
            ..PageSpec::default()
        },
    )
    .await;

    let body = anon_client()
        .get(server.url("/sitemap.xml"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("xml body");
    assert!(
        body.contains("<loc>http://127.0.0.1:"),
        "the Host header carries the origin under plain http: {body}"
    );
    assert!(body.contains("/p/una</loc>"), "the page is listed: {body}");
}

#[tokio::test]
async fn the_sitemap_honours_the_config_flag() {
    let (mut config, _db) = corporate_config(None);
    config.cms.sitemap = false;
    let server = TestServer::start_full(config).await;

    let response = anon_client()
        .get(server.url("/sitemap.xml"))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 404, "the flag turns the route off");
}

// ── Form validation ───────────────────────────────────────────────────

#[tokio::test]
async fn page_forms_validate_the_new_fields() {
    let (config, _db) = corporate_config(None);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "root-admin", "sup3r-secret!").await;
    let admin = login_browser(&server, "root-admin", "sup3r-secret!").await;

    // A bad og_image scheme.
    let response = admin
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("slug=mala&title=Mala&content=x&og_image=javascript:alert(1)&is_published=on")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200, "the form re-renders");
    let body = response.text().await.expect("html body");
    assert!(body.contains("imagen social"), "the og_image error: {body}");

    // A non-numeric position.
    let response = admin
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("slug=mala2&title=Mala2&content=x&position=abc&is_published=on")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("posición"), "the position error: {body}");

    // A nonsense parent id.
    let response = admin
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("slug=mala3&title=Mala3&content=x&parent_id=xyz&is_published=on")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html body");
    assert!(body.contains("padre"), "the parent error: {body}");
}
