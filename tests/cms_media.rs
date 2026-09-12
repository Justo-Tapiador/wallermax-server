//! The media library battery (F9).
//!
//! Boots the **full** server exactly like the binary (fresh temporary
//! SQLite, auth, templates, static files and the CMS with a temporary
//! `media_dir`) and drives it with a cookie-storing `reqwest` client
//! through plain HTML forms and hand-rolled `multipart/form-data`
//! bodies — the same "closest thing to a browser" approach as the
//! other CMS suites.
//!
//! Covered: the upload happy path (row + files + immutable-cached
//! serving + thumbnail), the validation gates (non-images, disallowed
//! formats, truncated payloads, the `media_max_bytes` cap, the missing
//! file field), the alt-text round trip, deletion (row + files + 404
//! afterwards), the exact-name serving rule, the role gates, and the
//! promise that the library writes into its own directory — never into
//! `public/`.

mod common;

use common::{auth_config, TestServer};
use wallermax_server::config::AppConfig;

/// A media-library config: everything the corporate CMS needs, plus a
/// fresh temporary `media_dir` the guard removes on drop.
fn media_config(marker: &str) -> (AppConfig, MediaDirGuard) {
    let (mut config, _db) = auth_config();
    config.templates.enabled = true;
    config.static_files.enabled = true;
    config.static_files.root_dir = String::from("public");
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    let guard = MediaDirGuard::new(marker);
    config.cms.media_dir = guard.path.display().to_string();
    (config, guard)
}

/// A temporary media directory removed on drop (files included).
struct MediaDirGuard {
    path: std::path::PathBuf,
}

impl MediaDirGuard {
    fn new(marker: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("wallermax-media-{}-{marker}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp media dir");
        Self { path }
    }
}

impl Drop for MediaDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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

/// Registers the bootstrap admin over the JSON API and logs a browser
/// in through the form endpoint.
async fn admin_browser(server: &TestServer) -> reqwest::Client {
    let response = reqwest::Client::new()
        .post(server.url("/api/auth/register"))
        .json(&serde_json::json!({
            "username": "admin",
            "password": "media-library-secret"
        }))
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
        .body("username=admin&password=media-library-secret&redirect=/")
        .send()
        .await
        .expect("form login succeeds");
    assert_eq!(response.status(), 303, "form login redirects");
    client
}

/// Encodes a `width × height` RGB gradient image in memory as the
/// given format (the same fixture approach as the `media` unit tests).
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

/// A PNG fixture wide enough to deterministically exceed a small
/// `media_max_bytes` (a 200 × 200 gradient is several KB).
fn oversized_png_fixture() -> Vec<u8> {
    image_fixture(image::ImageFormat::Png, 200, 200)
}

/// Builds a `multipart/form-data` body by hand: an optional `alt` text
/// field plus the `file` part. Returns (body, boundary).
fn multipart_body(alt: Option<&str>, file_name: &str, file_bytes: &[u8]) -> (Vec<u8>, String) {
    let boundary = format!("wmsBoundary{}", std::process::id());
    let mut body = Vec::new();
    if let Some(alt) = alt {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"alt\"\r\n\r\n");
        body.extend_from_slice(alt.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; \
             filename=\"{file_name}\"\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
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

/// Extracts the canonical `/media/<id>/<name>` URL from the detail
/// page: the «URL definitiva» row renders it inside `<code>…</code>`,
/// which anchors the parse far away from the form actions.
fn media_url_from_page(html: &str) -> Option<String> {
    let marker = "<code>/media/";
    let start = html.find(marker)? + "<code>".len();
    let rest = &html[start..];
    let end = rest.find('<').unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

#[tokio::test]
async fn uploads_round_trip_from_form_to_public_url() {
    let (config, _media_dir) = media_config("round-trip");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    let png = image_fixture(image::ImageFormat::Png, 400, 300);
    let response = post_upload(&server, &client, Some("Logo del sitio"), "logo.png", &png).await;
    assert_eq!(response.status(), 303, "a valid upload redirects (PRG)");
    let location = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    assert!(location.starts_with("/admin/media/"), "{location}");
    let media_id: i64 = location
        .trim_start_matches("/admin/media/")
        .parse()
        .expect("numeric media id");

    // The detail page carries the canonical URL and both snippets.
    let detail = client
        .get(server.url(&location))
        .send()
        .await
        .expect("detail page loads");
    assert_eq!(detail.status(), 200);
    let html = detail.text().await.expect("detail body");
    let url = media_url_from_page(&html).expect("the canonical /media/ URL");
    assert!(
        url.contains(&format!("/media/{media_id}/")),
        "canonical URL: {url}"
    );
    assert!(url.ends_with(".png"), "canonical URL: {url}");
    assert!(
        html.contains(&format!("![Logo del sitio]({url})")),
        "the Markdown snippet uses the alt text: {url}"
    );

    // The public URL serves the exact bytes with the sniffed mime type
    // and the immutable cache policy.
    let served = reqwest::Client::new()
        .get(server.url(&url))
        .send()
        .await
        .expect("public serving");
    assert_eq!(served.status(), 200);
    assert_eq!(
        served.headers()["Content-Type"],
        "image/png",
        "the sniffed mime type, not the client's"
    );
    let cache = served.headers()["Cache-Control"].to_str().expect("ascii");
    assert!(
        cache.contains("max-age=31536000") && cache.contains("immutable"),
        "immutable year-long caching: {cache}"
    );
    let body = served.bytes().await.expect("media bytes");
    assert_eq!(&body[..], &png[..], "byte-for-byte what was uploaded");

    // The thumbnail decodes as a PNG bounded by the thumbnail edge.
    let thumb = reqwest::Client::new()
        .get(server.url(&format!("/media/thumb/{media_id}")))
        .send()
        .await
        .expect("thumbnail serving");
    assert_eq!(thumb.status(), 200);
    assert_eq!(thumb.headers()["Content-Type"], "image/png");
    let thumb_bytes = thumb.bytes().await.expect("thumbnail bytes");
    let decoded = image::load_from_memory(&thumb_bytes).expect("thumbnail decodes");
    assert_eq!((decoded.width(), decoded.height()), (320, 240));

    // The listing shows the item with its thumbnail.
    let list = client
        .get(server.url("/admin/media"))
        .send()
        .await
        .expect("list page loads");
    let list_html = list.text().await.expect("list body");
    assert!(list_html.contains(&format!("/media/thumb/{media_id}")));
    assert!(list_html.contains("logo.png"));
}

#[tokio::test]
async fn uploads_write_into_the_media_dir_never_public() {
    let (config, media_dir) = media_config("own-dir");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    let png = image_fixture(image::ImageFormat::Png, 10, 10);
    let response = post_upload(&server, &client, None, "uno.png", &png).await;
    assert_eq!(response.status(), 303);

    // Exactly two files in the configured media directory: the file
    // and its thumbnail, both flat server-generated names.
    let entries: Vec<String> = std::fs::read_dir(&media_dir.path)
        .expect("media dir readable")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries.len(), 2, "file + thumbnail: {entries:?}");
    assert!(
        entries
            .iter()
            .all(|name| { !name.contains('/') && !name.contains('\\') }),
        "flat names only: {entries:?}"
    );

    // And nothing landed in the repository's public/ tree.
    let public_media = std::path::Path::new("public").join("media");
    assert!(
        !public_media.exists(),
        "the library never writes into public/"
    );
}

#[tokio::test]
async fn uploads_reject_non_images_and_disallowed_formats() {
    let (config, _media_dir) = media_config("reject-bytes");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    // Plain text renamed to .png: the sniff fails.
    let response = post_upload(&server, &client, None, "falso.txt.png", b"esto es texto").await;
    assert_eq!(response.status(), 200, "errors re-render the form");
    let html = response.text().await.expect("error body");
    assert!(html.contains("no parece una imagen"), "{html:?}");
    assert!(
        html.contains("falso.txt.png") || html.contains("vacio") || html.contains("biblioteca"),
        "the listing context is rendered"
    );

    // A BMP magic header: sniffed fine, whitelisted out.
    let bmp = b"BM\x36\x00\x00\x00\x00\x00\x00\x00\x28\x00\x00\x00".to_vec();
    let response = post_upload(&server, &client, None, "foto.bmp", &bmp).await;
    assert_eq!(response.status(), 200);
    let html = response.text().await.expect("error body");
    assert!(html.contains("Formato no admitido"), "{html:?}");

    // A truncated PNG: sniffed, whitelisted, undecodable.
    let mut truncated = image_fixture(image::ImageFormat::Png, 50, 50);
    truncated.truncate(truncated.len() / 2);
    let response = post_upload(&server, &client, None, "roto.png", &truncated).await;
    assert_eq!(response.status(), 200);
    let html = response.text().await.expect("error body");
    assert!(html.contains("no se pudo decodificar"), "{html:?}");

    // Nothing was stored: the listing is still empty and no media URL
    // resolves.
    let list = client
        .get(server.url("/admin/media"))
        .send()
        .await
        .expect("list page loads");
    let html = list.text().await.expect("list body");
    assert!(html.contains("La biblioteca está vacía"), "{html:?}");
    let served = reqwest::Client::new()
        .get(server.url("/media/thumb/1"))
        .send()
        .await
        .expect("probe");
    assert_eq!(served.status(), 404);
}

#[tokio::test]
async fn uploads_respect_the_configured_byte_cap() {
    let (mut config, _media_dir) = media_config("byte-cap");
    config.cms.media_max_bytes = 2_048;
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    // A valid PNG of several KB against a 2 KB cap.
    let response = post_upload(
        &server,
        &client,
        Some("grande"),
        "grande.png",
        &oversized_png_fixture(),
    )
    .await;
    assert_eq!(response.status(), 200, "capped uploads bounce to the form");
    let html = response.text().await.expect("error body");
    assert!(
        html.contains("supera el límite") && html.contains("2,0 KB"),
        "the cap names the configured budget: {html:?}"
    );
    // The typed alt text survived the bounce.
    assert!(html.contains("value=\"grande\""), "{html:?}");
}

#[tokio::test]
async fn uploads_without_a_file_field_bounce_back() {
    let (config, _media_dir) = media_config("no-file");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    let (mut _body, boundary) = multipart_body(Some("sin archivo"), "x.png", b"");
    // Alt-only body: drop the file part entirely.
    let mut alt_body = Vec::new();
    alt_body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    alt_body.extend_from_slice(b"Content-Disposition: form-data; name=\"alt\"\r\n\r\n");
    alt_body.extend_from_slice(b"sin archivo");
    alt_body.extend_from_slice(b"\r\n");
    alt_body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let response = client
        .post(server.url("/admin/media"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(alt_body)
        .send()
        .await
        .expect("upload request succeeds");
    assert_eq!(response.status(), 200);
    let html = response.text().await.expect("error body");
    assert!(html.contains("campo «file»"), "{html:?}");
}

#[tokio::test]
async fn alt_text_round_trips_through_the_detail_form() {
    let (config, _media_dir) = media_config("alt-edit");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    let png = image_fixture(image::ImageFormat::Png, 8, 8);
    let response = post_upload(&server, &client, Some("Texto inicial"), "a.png", &png).await;
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    let media_id: i64 = location
        .trim_start_matches("/admin/media/")
        .parse()
        .expect("numeric media id");

    // Edit the alt text through the form.
    let response = client
        .post(server.url(&format!("/admin/media/{media_id}/alt")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("alt=Nuevo%20texto%20descriptivo")
        .send()
        .await
        .expect("alt update succeeds");
    assert_eq!(response.status(), 303, "PRG to the detail page");
    let redirect = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    assert!(redirect.contains("ok=alt"), "{redirect}");

    // The detail page (and the snippet) carry the new text.
    let detail = client
        .get(server.url(&format!("/admin/media/{media_id}?ok=alt")))
        .send()
        .await
        .expect("detail page loads");
    let html = detail.text().await.expect("detail body");
    assert!(
        html.contains("![Nuevo texto descriptivo]("),
        "the snippet uses the updated alt: {html:?}"
    );
    assert!(html.contains("Texto alternativo actualizado"));

    // An over-long alt text bounces back with the error, page intact.
    let long = "x".repeat(600);
    let response = client
        .post(server.url(&format!("/admin/media/{media_id}/alt")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("alt={long}"))
        .send()
        .await
        .expect("alt rejection succeeds");
    assert_eq!(response.status(), 200);
    let html = response.text().await.expect("error body");
    assert!(html.contains("500 caracteres"), "{html:?}");
}

#[tokio::test]
async fn deletion_removes_the_row_the_files_and_the_urls() {
    let (config, media_dir) = media_config("delete");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    let png = image_fixture(image::ImageFormat::Png, 6, 6);
    let response = post_upload(&server, &client, None, "borrame.png", &png).await;
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    let media_id: i64 = location
        .trim_start_matches("/admin/media/")
        .parse()
        .expect("numeric media id");
    let files_before: Vec<String> = std::fs::read_dir(&media_dir.path)
        .expect("media dir readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(files_before.len(), 2);

    let url = format!(
        "/media/{media_id}/{}",
        files_before
            .iter()
            .find(|n| !n.contains("_t"))
            .expect("stored name")
    );

    // Delete through the form.
    let response = client
        .post(server.url(&format!("/admin/media/{media_id}/delete")))
        .send()
        .await
        .expect("delete succeeds");
    assert_eq!(response.status(), 303);
    let redirect = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    assert_eq!(redirect, "/admin/media?ok=eliminado");

    // Files gone, row gone, URL gone.
    let files_after: Vec<String> = std::fs::read_dir(&media_dir.path)
        .expect("media dir readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        files_after.is_empty(),
        "both files removed: {files_after:?}"
    );
    let served = reqwest::Client::new()
        .get(server.url(&url))
        .send()
        .await
        .expect("serving after delete");
    assert_eq!(served.status(), 404);
    let detail = client
        .get(server.url(&format!("/admin/media/{media_id}")))
        .send()
        .await
        .expect("detail after delete");
    assert_eq!(detail.status(), 404);
}

#[tokio::test]
async fn serving_requires_the_exact_canonical_name() {
    let (config, _media_dir) = media_config("exact-name");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    let png = image_fixture(image::ImageFormat::Png, 5, 5);
    let response = post_upload(&server, &client, None, "secreto.png", &png).await;
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    let media_id: i64 = location
        .trim_start_matches("/admin/media/")
        .parse()
        .expect("numeric media id");

    // The id alone is not enough, and a wrong (or traversal-ish) name
    // never resolves.
    for probe in [
        format!("/media/{media_id}"),
        format!("/media/{media_id}/no-es-el-nombre.png"),
        format!("/media/{media_id}/..%2F..%2Fetc"),
        String::from("/media/999/lo-que-sea.png"),
        String::from("/media/thumb/999"),
    ] {
        let served = reqwest::Client::new()
            .get(server.url(&probe))
            .send()
            .await
            .expect("probe");
        assert_eq!(served.status(), 404, "probe {probe}");
    }
}

#[tokio::test]
async fn jpeg_and_gif_uploads_round_trip() {
    let (config, _media_dir) = media_config("formats");
    let server = TestServer::start_full(config).await;
    let client = admin_browser(&server).await;

    for (name, format, mime) in [
        ("foto.jpg", image::ImageFormat::Jpeg, "image/jpeg"),
        ("dibujo.gif", image::ImageFormat::Gif, "image/gif"),
    ] {
        let bytes = image_fixture(format, 30, 20);
        let response = post_upload(&server, &client, None, name, &bytes).await;
        assert_eq!(response.status(), 303, "{name} uploads");
        let location = response
            .headers()
            .get("Location")
            .and_then(|value| value.to_str().ok())
            .expect("redirect location")
            .to_owned();
        let media_id: i64 = location
            .trim_start_matches("/admin/media/")
            .parse()
            .expect("numeric media id");

        let detail = client
            .get(server.url(&location))
            .send()
            .await
            .expect("detail page loads");
        let html = detail.text().await.expect("detail body");
        let url = media_url_from_page(&html).expect("canonical URL");

        let served = reqwest::Client::new()
            .get(server.url(&url))
            .send()
            .await
            .expect("public serving");
        assert_eq!(served.status(), 200, "{name} serves");
        assert_eq!(served.headers()["Content-Type"], mime, "{name} mime");
        let served_bytes = served.bytes().await.expect("media bytes");
        assert_eq!(&served_bytes[..], &bytes[..], "{name} byte-for-byte");

        let thumb = reqwest::Client::new()
            .get(server.url(&format!("/media/thumb/{media_id}")))
            .send()
            .await
            .expect("thumbnail serving");
        assert_eq!(thumb.status(), 200, "{name} thumbnail");
        assert_eq!(
            thumb.headers()["Content-Type"],
            "image/png",
            "{name} thumb is PNG"
        );
    }
}

#[tokio::test]
async fn the_media_panel_is_editors_only_and_serving_is_public() {
    let (config, _media_dir) = media_config("roles");
    let server = TestServer::start_full(config).await;
    let admin = admin_browser(&server).await;

    // A upload exists so there is something to serve publicly.
    let png = image_fixture(image::ImageFormat::Png, 4, 4);
    let response = post_upload(&server, &admin, None, "publico.png", &png).await;
    assert_eq!(response.status(), 303);
    let location = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location")
        .to_owned();
    let media_id: i64 = location
        .trim_start_matches("/admin/media/")
        .parse()
        .expect("numeric media id");

    // Anonymous visitors: redirected to the login for the panel, but
    // served the media like any <img> would be.
    let anon = browser_client();
    let response = anon
        .get(server.url("/admin/media"))
        .send()
        .await
        .expect("anonymous panel probe");
    assert_eq!(response.status(), 303);
    assert!(
        response
            .headers()
            .get("Location")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|location| location.starts_with("/login")),
        "anonymous users are sent to the login form"
    );
    let response = anon
        .get(server.url(&format!("/media/thumb/{media_id}")))
        .send()
        .await
        .expect("anonymous thumbnail probe");
    assert_eq!(response.status(), 200, "public serving, no gate");

    // The `user` role (authenticated, not an editor) is refused.
    let response = admin
        .post(server.url("/admin/users"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=lector&password=password-del-lector&role=user")
        .send()
        .await
        .expect("user creation succeeds");
    assert_eq!(response.status(), 303, "admin creates the account");

    let reader = browser_client();
    let response = reader
        .post(server.url("/api/auth/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=lector&password=password-del-lector&redirect=/")
        .send()
        .await
        .expect("login succeeds");
    assert_eq!(response.status(), 303);

    let response = reader
        .get(server.url("/admin/media"))
        .send()
        .await
        .expect("reader panel probe");
    assert_eq!(response.status(), 403, "non-editors get the HTML 403 page");

    let response = reader
        .post(server.url(&format!("/admin/media/{media_id}/delete")))
        .send()
        .await
        .expect("reader delete probe");
    assert_eq!(response.status(), 403, "non-editors cannot delete either");

    // The upload is still there.
    let detail = admin
        .get(server.url(&location))
        .send()
        .await
        .expect("detail still loads");
    assert_eq!(detail.status(), 200);
}
