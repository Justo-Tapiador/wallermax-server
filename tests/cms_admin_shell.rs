//! The F12 admin-shell battery: the panel's own app shell, stylesheet
//! and no-JavaScript theme toggle.
//!
//! Boots the full server like every CMS battery and drives it with
//! cookie-storing `reqwest` clients. Covered:
//!
//! - `/assets/admin.css` is served as the panel's own stylesheet;
//! - every admin view renders the sidebar + topbar shell, links
//!   `admin.css` (never the public `wallermax.css`), is `lang="en"`
//!   and contains zero `<script>` — the redesign is as JavaScript-free
//!   as everything else;
//! - `GET /admin/theme?to=dark|light&back=<path>` pins the HttpOnly
//!   `wm_theme` cookie and bounces back; the next render carries
//!   `data-theme` and the toggle flips direction; a garbage cookie
//!   reads as "no preference" (the OS fallback decides); a `to` that
//!   is neither dark nor light redirects untouched; and `back` is
//!   honored only as a printable `/admin` path without `..` — the
//!   route can never become an open redirect;
//! - the dashboard's F12 enrichment: the recent-activity feed (one row
//!   per revision, editor and note included) and the "needs attention"
//!   card for pending schedules, plus the new media/scheduled
//!   counters;
//! - the pages tree indents through `.depth-N` **classes**, never an
//!   inline style — the shipped CSP (`style-src 'self'`, no
//!   `'unsafe-inline'`) silently dropped the old `style="padding…"`
//!   attribute, so the view now renders CSP-clean by construction;
//! - the shared admin pagination renders numbered page buttons
//!   (`aria-current` included) once a listing outgrows one page.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::{auth_config, TestServer};
use serde_json::json;
use wallermax_server::config::AppConfig;

/// A cookie-storing client with manual redirects, the same browser
/// stance as the other CMS batteries.
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

/// Everything the CMS needs on a fresh database.
fn shell_config() -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.cms.enabled = true;
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

/// Logs a browser in through the form endpoint, returning the client
/// and the raw session cookie (`name=value`) for hand-built Cookie
/// headers in the tests that mix cookies.
async fn login_browser_with_cookie(
    server: &TestServer,
    username: &str,
    password: &str,
) -> (reqwest::Client, String) {
    let client = browser_client();
    let response = client
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={username}&password={password}&redirect=%2F"
        ))
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("session cookie set")
        .split(';')
        .next()
        .expect("name=value prefix")
        .to_owned();
    assert!(cookie.starts_with("wallermax_session="), "{cookie}");
    (client, cookie)
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

/// Posts the page form (create flavor) and returns the new page id
/// from the redirect target.
#[allow(clippy::too_many_arguments)]
async fn create_page(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
    schedule: Option<&str>,
    note: Option<&str>,
    parent_id: Option<i64>,
) -> i64 {
    let mut body = format!(
        "slug={}&title={}&content={}",
        slug,
        urlencoding_simple(title),
        urlencoding_simple(content),
    );
    if publish {
        body.push_str("&is_published=on");
    }
    if let Some(schedule) = schedule {
        body.push_str(&format!("&publish_at={}", urlencoding_simple(schedule)));
    }
    if let Some(note) = note {
        body.push_str(&format!("&revision_note={}", urlencoding_simple(note)));
    }
    if let Some(parent) = parent_id {
        body.push_str(&format!("&parent_id={parent}"));
    }
    let response = client
        .post(server.url("/admin/pages"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("page create succeeds");
    assert_eq!(response.status(), 303, "create redirects to the edit view");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location");
    location
        .trim_start_matches("/admin/pages/")
        .split(['/', '?'])
        .next()
        .expect("page id in location")
        .parse::<i64>()
        .expect("page id parses")
}

/// Unix seconds as the `YYYY-MM-DDTHH:MM` UTC value the
/// `datetime-local` field accepts (civil-date math inlined: the
/// tests cannot reach the crate-private helpers).
fn datetime_local(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}",
        hour = time_of_day / 3_600,
        minute = (time_of_day % 3_600) / 60
    )
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[tokio::test]
async fn admin_css_is_served_as_the_panels_own_stylesheet() {
    let (mut config, _db) = shell_config();
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    let server = TestServer::start_full(config).await;

    let response = anon_client()
        .get(server.url("/assets/admin.css"))
        .send()
        .await
        .expect("stylesheet request succeeds");
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .expect("content type present");
    assert!(content_type.starts_with("text/css"), "{content_type}");
    let body = response.text().await.expect("css body");
    assert!(body.contains("--sidebar"), "the shell tokens: {body}");
    assert!(body.contains("[data-theme=\"dark\"]"), "dark theme: {body}");
}

#[tokio::test]
async fn every_admin_page_renders_the_shell_without_javascript() {
    let (config, _db) = shell_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "justo-admin", "password-123").await;
    let (admin, _session) = login_browser_with_cookie(&server, "justo-admin", "password-123").await;

    for path in [
        "/admin",
        "/admin/pages",
        "/admin/pages/new",
        "/admin/menus",
        "/admin/users",
    ] {
        let response = admin.get(server.url(path)).send().await.expect(path);
        assert_eq!(response.status(), 200, "{path}");
        let body = response.text().await.expect("html body");
        assert!(
            body.contains("<aside class=\"sidebar\">"),
            "sidebar: {path}"
        );
        assert!(body.contains("<header class=\"topbar\">"), "topbar: {path}");
        assert!(body.contains("admin.css"), "own stylesheet: {path}");
        assert!(!body.contains("wallermax.css"), "no public css: {path}");
        assert!(body.contains("<html lang=\"en\""), "english panel: {path}");
        assert!(body.contains("Sign out"), "logout in the nav: {path}");
        assert!(body.contains("View site"), "site link in the nav: {path}");
        assert!(!body.contains("<script"), "zero javascript: {path}");
    }
}

#[tokio::test]
async fn theme_toggle_pins_the_cookie_and_flips_the_direction() {
    let (config, _db) = shell_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "clara-admin", "password-123").await;
    let (admin, session) = login_browser_with_cookie(&server, "clara-admin", "password-123").await;

    // No cookie yet: the OS fallback decides, no data-theme attribute,
    // and the toggle offers dark.
    let body = admin
        .get(server.url("/admin"))
        .send()
        .await
        .expect("dashboard")
        .text()
        .await
        .expect("html");
    assert!(!body.contains("data-theme"), "no pinned theme: {body}");
    assert!(body.contains("to=dark"), "the toggle offers dark: {body}");

    // Pin dark: the cookie is HttpOnly + same-site + one year, and the
    // browser comes back to the requested admin path.
    let response = admin
        .get(server.url("/admin/theme?to=dark&back=/admin/users"))
        .send()
        .await
        .expect("theme toggle");
    assert_eq!(response.status(), 303);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/admin/users")
    );
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("cookie set");
    assert!(cookie.starts_with("wm_theme=dark"), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Max-Age=31536000"), "{cookie}");

    // The next render carries the attribute and flips the offer.
    let body = admin
        .get(server.url("/admin"))
        .send()
        .await
        .expect("dashboard")
        .text()
        .await
        .expect("html");
    assert!(body.contains("data-theme=\"dark\""), "pinned dark: {body}");
    assert!(
        body.contains("to=light"),
        "the toggle now offers light: {body}"
    );

    // A garbage cookie reads as "no preference": only the two literal
    // values ever render an attribute. (A plain client with the
    // session cookie hand-built alongside the garbage one.)
    let response = browser_client()
        .get(server.url("/admin"))
        .header("Cookie", format!("{session}; wm_theme=purple"))
        .send()
        .await
        .expect("dashboard with garbage cookie");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("html");
    assert!(!body.contains("data-theme"), "garbage = no pin: {body}");

    // to=banana redirects without touching the cookie.
    let response = admin
        .get(server.url("/admin/theme?to=banana"))
        .send()
        .await
        .expect("invalid theme value");
    assert_eq!(response.status(), 303);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/admin")
    );
    assert!(
        response.headers().get("set-cookie").is_none(),
        "no cookie touched"
    );
}

#[tokio::test]
async fn theme_back_is_sanitized_to_printable_admin_paths() {
    let (config, _db) = shell_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "pedro-admin", "password-123").await;
    let (admin, _session) = login_browser_with_cookie(&server, "pedro-admin", "password-123").await;

    for back in ["https://evil.example.com", "/perfil", "/admin/../../etc"] {
        let url = format!("/admin/theme?to=dark&back={}", urlencoding_simple(back));
        let response = admin.get(server.url(&url)).send().await.expect("toggle");
        assert_eq!(response.status(), 303, "back={back}");
        assert_eq!(
            response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok()),
            Some("/admin"),
            "the bounce falls back home: back={back}"
        );
        assert!(
            response.headers().get("set-cookie").is_some(),
            "the cookie is still pinned: back={back}"
        );
    }

    // A percent-encoded payload stays percent-encoded: the guard only
    // lets printable bytes through, so no CR/LF can ever split the
    // Location header — the bounce stays on this origin, verbatim.
    let back = "/admin%0d%0aSet-Cookie:x=1";
    let response = admin
        .get(server.url(&format!(
            "/admin/theme?to=dark&back={}",
            urlencoding_simple(back)
        )))
        .send()
        .await
        .expect("toggle");
    assert_eq!(response.status(), 303);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some(back),
        "the encoded bounce stays verbatim and same-origin"
    );
    assert!(response.headers().get("set-cookie").is_some());

    // Anonymous visitors never reach the toggle: the editor gate
    // redirects them to the login form.
    let response = anon_client()
        .get(server.url("/admin/theme?to=dark"))
        .send()
        .await
        .expect("anonymous toggle");
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("login redirect");
    assert!(location.starts_with("/login?redirect="), "{location}");
    assert!(response.headers().get("set-cookie").is_none());
}

#[tokio::test]
async fn dashboard_shows_activity_attention_and_the_new_counters() {
    let (config, _db) = shell_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "ana-admin", "password-123").await;
    let (admin, _session) = login_browser_with_cookie(&server, "ana-admin", "password-123").await;

    // A fresh panel: the empty states, and the new counters.
    let body = admin
        .get(server.url("/admin"))
        .send()
        .await
        .expect("dashboard")
        .text()
        .await
        .expect("html");
    assert!(body.contains("Recent activity"), "{body}");
    assert!(body.contains("Needs attention"), "{body}");
    assert!(body.contains("No revisions yet"), "{body}");
    assert!(body.contains("Nothing pending"), "{body}");
    assert!(body.contains("Media files"), "{body}");
    assert!(body.contains("Scheduled"), "{body}");

    // A save lands in the activity feed, note and editor included.
    create_page(
        &server,
        &admin,
        "quienes-somos",
        "Quienes somos",
        "<p>first draft</p>",
        false,
        None,
        Some("first draft"),
        None,
    )
    .await;
    let body = admin
        .get(server.url("/admin"))
        .send()
        .await
        .expect("dashboard")
        .text()
        .await
        .expect("html");
    assert!(body.contains("ana-admin"), "the editor: {body}");
    assert!(body.contains("first draft"), "the note: {body}");
    assert!(body.contains("Saved"), "the action: {body}");
    assert!(body.contains("Quienes somos"), "the page title: {body}");

    // A pending schedule lands in "needs attention" and the counter.
    let scheduled_at = datetime_local(unix_now() + 3_600);
    create_page(
        &server,
        &admin,
        "pagina-programada",
        "La pagina programada",
        "<p>waiting</p>",
        false,
        Some(&scheduled_at),
        Some("programada para el test"),
        None,
    )
    .await;
    let body = admin
        .get(server.url("/admin"))
        .send()
        .await
        .expect("dashboard")
        .text()
        .await
        .expect("html");
    assert!(body.contains("goes live"), "the schedule row: {body}");
    assert!(body.contains("La pagina programada"), "{body}");
    assert!(
        body.contains("<strong>Scheduled</strong>"),
        "the attention row: {body}"
    );

    // Drafts surface as attention too, with the plural right.
    assert!(body.contains("2 drafts"), "draft counter: {body}");
}

#[tokio::test]
async fn pages_tree_indents_with_classes_never_inline_styles() {
    let (config, _db) = shell_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "laura-admin", "password-123").await;
    let (admin, _session) = login_browser_with_cookie(&server, "laura-admin", "password-123").await;

    let parent = create_page(
        &server,
        &admin,
        "seccion",
        "Seccion",
        "<p>parent</p>",
        true,
        None,
        None,
        None,
    )
    .await;
    create_page(
        &server,
        &admin,
        "hija",
        "Hija",
        "<p>child</p>",
        true,
        None,
        None,
        Some(parent),
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
    assert!(body.contains("depth-0"), "top level: {body}");
    assert!(body.contains("depth-1"), "the child indent: {body}");
    // The CSP ships style-src 'self' without 'unsafe-inline': an
    // inline style attribute is silently dropped by the browser, so
    // the panel must never emit one.
    assert!(
        !body.contains("style=\""),
        "no inline styles anywhere: {body}"
    );
}

#[tokio::test]
async fn admin_pagination_renders_numbered_page_buttons() {
    let (mut config, _db) = shell_config();
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    // A dedicated media dir under /tmp, like the F9 battery.
    let media_dir =
        std::env::temp_dir().join(format!("wallermax-shell-media-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&media_dir);
    std::fs::create_dir_all(&media_dir).expect("temp media dir");
    config.cms.media_dir = media_dir.display().to_string();
    let server = TestServer::start_full(config).await;
    register_admin(&server, "bo-admin", "password-123").await;
    let (admin, _session) = login_browser_with_cookie(&server, "bo-admin", "password-123").await;

    // 25 tiny uploads: 24 per page means two pages.
    let mut image = image::RgbImage::new(4, 4);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = image::Rgb([(x * 40) as u8, (y * 40) as u8, 128]);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("fixture encodes");
    for index in 0..25 {
        let boundary = format!("wmsBoundary{index}");
        let file_name = format!("pixel-{index}.png");
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; \
                 filename=\"{file_name}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(&bytes);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let response = admin
            .post(server.url("/admin/media"))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .expect("upload succeeds");
        assert_eq!(response.status(), 303, "upload {index} redirects");
    }

    let body = admin
        .get(server.url("/admin/media"))
        .send()
        .await
        .expect("media grid")
        .text()
        .await
        .expect("html");
    assert!(body.contains("Page 1 of 2"), "{body}");
    assert!(body.contains("25 items"), "{body}");
    assert!(
        body.contains("aria-current=\"page\""),
        "the current page is marked: {body}"
    );
    assert!(body.contains("page=2"), "the next page link: {body}");
    assert!(body.contains("&#8250;"), "the next arrow: {body}");

    let _ = std::fs::remove_dir_all(&media_dir);
}
