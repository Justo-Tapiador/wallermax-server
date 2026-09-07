//! End-to-end integration tests for dynamic template rendering
//! (`[templates]`).
//!
//! Each test boots the real server stack (routes + middleware, exactly
//! as the binary serves it) against temporary fixture directories and
//! exercises the whole contract: on-the-fly rendering of `.jhs` under
//! the static root (source never leaked), view auto-routing, API
//! precedence, error envelopes, traversal rejection, cache
//! invalidation by mtime and coexistence with plain static files.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{TempDbGuard, TestServer};
use serde_json::{json, Value};
use wallermax_server::config::AppConfig;

/// Fixture tree: a static root (with `hello.jhs`, `raw.jhs`, `style.css`
/// and an optional index) plus a views directory (`index.jhs`,
/// `contacto.jhs`, `blog/index.jhs`, `onlyview.jhs`, `boom.jhs`,
/// `loopy.jhs`, and `health.jhs` for the route-precedence check).
struct FixtureDir {
    path: PathBuf,
}

impl FixtureDir {
    fn create(tag: &str) -> Self {
        Self::create_with(tag, true)
    }

    /// `with_index` controls whether `index.html` exists in the static
    /// root (when absent, `GET /` auto-routes to `views/index.jhs`).
    fn create_with(tag: &str, with_index: bool) -> Self {
        let path =
            std::env::temp_dir().join(format!("wallermax-templates-{tag}-{}", std::process::id()));
        let static_root = path.join("static");
        let views = path.join("views");
        std::fs::create_dir_all(static_root.join("api")).expect("static dirs create");
        std::fs::create_dir_all(views.join("blog")).expect("blog view dir");

        if with_index {
            std::fs::write(static_root.join("index.html"), INDEX_HTML).expect("index write");
        }
        std::fs::write(static_root.join("hello.jhs"), HELLO_JHS).expect("hello write");
        std::fs::write(static_root.join("raw.jhs"), RAW_JHS).expect("raw write");
        std::fs::write(static_root.join("style.css"), CSS).expect("css write");

        std::fs::write(views.join("index.jhs"), VIEW_INDEX).expect("view index write");
        std::fs::write(views.join("contacto.jhs"), VIEW_CONTACTO).expect("view write");
        std::fs::write(views.join("blog").join("index.jhs"), VIEW_BLOG).expect("blog write");
        std::fs::write(views.join("onlyview.jhs"), VIEW_ONLY).expect("only view write");
        std::fs::write(views.join("boom.jhs"), BOOM_JHS).expect("boom write");
        std::fs::write(views.join("health.jhs"), VIEW_HEALTH).expect("health view write");
        std::fs::write(views.join("profile.jhs"), VIEW_PROFILE).expect("profile view write");
        Self { path }
    }

    /// Configuration with static serving and templates enabled on this
    /// fixture tree.
    fn config(&self) -> AppConfig {
        self.config_with(|_| {})
    }

    fn config_with(&self, tune: impl FnOnce(&mut AppConfig)) -> AppConfig {
        let mut config = AppConfig::default();
        config.static_files.enabled = true;
        config.static_files.root_dir = self.path.join("static").to_string_lossy().into_owned();
        config.templates.enabled = true;
        config.templates.views_dir = self.path.join("views").to_string_lossy().into_owned();
        config.templates.loop_iteration_limit = 10_000_000;
        tune(&mut config);
        config
    }

    /// Configuration additionally enabling `[database]` and `[auth]`
    /// against a fresh temporary database (removed on drop), so tests
    /// can exercise the `user` template global.
    fn config_with_auth(&self, tune: impl FnOnce(&mut AppConfig)) -> (AppConfig, TempDbGuard) {
        let db = TempDbGuard::new();
        let config = self.config_with(|config| {
            config.database.enabled = true;
            config.database.url = db.url().to_owned();
            config.database.max_connections = 2;
            config.auth.enabled = true;
            config.auth.jwt_secret = String::from("integration-test-secret-0123456789abcdef0123");
            tune(config);
        });
        (config, db)
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const INDEX_HTML: &str = "<!DOCTYPE html><html><body><h1>static index</h1></body></html>";
const CSS: &str = "body { margin: 0; }";
const HELLO_JHS: &str = "<h1><?= \"hola\" ?></h1><?jhs var items = [\"a\", \"b\"]; \
    items.forEach(function (item) { echo(\"<li>\" + item + \"</li>\"); }); \
    ?>footer<?= 40 + 2 ?>";
const RAW_JHS: &str = "<?= raw(val) ?>";
const VIEW_INDEX: &str = "<html>view index</html>";
const VIEW_CONTACTO: &str = "<html>contacto <?= 1 + 1 ?></html>";
const VIEW_BLOG: &str = "<html>blog index</html>";
const VIEW_ONLY: &str = "<html>only view</html>";
const BOOM_JHS: &str = "<?jhs throw new Error(\"boom\"); ?>";
const VIEW_HEALTH: &str = "<html>view health</html>";
const VIEW_PROFILE: &str = "\
<?jhs if (user && user.role == 'admin') { ?>\
<h1>Bienvenido, administrador <?= user.username ?></h1>\
<?jhs } else if (user) { ?>\
<h1>Hola <?= user.username ?> (<?= user.role ?>)</h1>\
<?jhs } else { ?>\
<h1>por favor, inicia sesión</h1>\
<?jhs } ?>";

async fn body_json(response: reqwest::Response) -> Value {
    let bytes = response.bytes().await.expect("body bytes");
    serde_json::from_slice(&bytes).expect("body is valid JSON")
}

#[tokio::test]
async fn renders_jhs_under_the_static_root() {
    let fixture = FixtureDir::create("render");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/hello.jhs"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii value")
        .starts_with("text/html"));
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );

    let body = response.text().await.expect("body text");
    assert!(body.contains("<h1>hola</h1>"), "html literal: {body}");
    // echo() output is HTML-escaped (node-jhs2 semantics); raw strings
    // are covered by the fidelity battery.
    assert!(
        body.contains("&lt;li&gt;a&lt;/li&gt;&lt;li&gt;b&lt;/li&gt;"),
        "echo output: {body}"
    );
    assert!(body.ends_with("footer42"), "expression output: {body}");
}

#[tokio::test]
async fn raw_jhs_source_is_never_served() {
    let fixture = FixtureDir::create("raw");
    let server = TestServer::start_with_config(fixture.config()).await;
    let body = reqwest::get(server.url("/hello.jhs"))
        .await
        .expect("request ok")
        .text()
        .await
        .expect("body text");

    assert!(!body.contains("<?jhs"), "template source leaked: {body}");
    assert!(!body.contains("<?= "), "expression source leaked: {body}");
}

#[tokio::test]
async fn extensionless_path_auto_routes_to_the_view() {
    let fixture = FixtureDir::create("extensionless");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/contacto"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii value")
        .starts_with("text/html"));
    let body = response.text().await.expect("body text");
    assert!(body.contains("contacto 2"), "rendered view: {body}");
}

#[tokio::test]
async fn directory_path_auto_routes_to_the_index_view() {
    let fixture = FixtureDir::create("directory");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/blog")).await.expect("request ok");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert!(body.contains("blog index"), "nested view: {body}");
}

#[tokio::test]
async fn trailing_slash_still_auto_routes() {
    let fixture = FixtureDir::create("trailing");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/contacto/"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert!(body.contains("contacto 2"), "rendered view: {body}");
}

#[tokio::test]
async fn root_auto_routes_to_the_view_when_the_static_index_is_missing() {
    let fixture = FixtureDir::create_with("root", false);
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/")).await.expect("request ok");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert!(body.contains("view index"), "root view: {body}");
}

#[tokio::test]
async fn static_index_wins_over_the_root_view() {
    let fixture = FixtureDir::create("static-wins");
    let server = TestServer::start_with_config(fixture.config()).await;
    let body = reqwest::get(server.url("/"))
        .await
        .expect("request ok")
        .text()
        .await
        .expect("body text");

    assert!(
        body.contains("static index"),
        "static index expected: {body}"
    );
}

#[tokio::test]
async fn jhs_path_missing_from_static_falls_back_to_the_view() {
    let fixture = FixtureDir::create("fallback-view");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/onlyview.jhs"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert!(body.contains("only view"), "view fallback: {body}");
}

#[tokio::test]
async fn api_routes_take_precedence_over_views() {
    let fixture = FixtureDir::create("api-precedence");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/health"))
        .await
        .expect("request ok");

    // A view exists for /health, but the API route answers first.
    assert_eq!(response.status(), 200);
    let json = body_json(response).await;
    assert_eq!(json["status"], "ok");
    assert!(json.get("view").is_none(), "view must not answer: {json}");
}

#[tokio::test]
async fn missing_paths_keep_the_json_404_envelope() {
    let fixture = FixtureDir::create("missing");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/no-existe"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 404);
    let json = body_json(response).await;
    assert_eq!(json["error"]["code"], "NOT_FOUND");
    assert!(json["error"]["request_id"].is_string());
}

#[tokio::test]
async fn non_get_methods_bypass_rendering() {
    let fixture = FixtureDir::create("non-get");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::Client::new()
        .post(server.url("/hello.jhs"))
        .send()
        .await
        .expect("request ok");

    // Non-GET/HEAD requests to file paths answer the JSON 404 envelope.
    assert_eq!(response.status(), 404);
    let json = body_json(response).await;
    assert_eq!(json["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn head_requests_are_rendered() {
    let fixture = FixtureDir::create("head");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::Client::new()
        .head(server.url("/hello.jhs"))
        .send()
        .await
        .expect("head ok");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii value")
        .starts_with("text/html"));
    let body = response.text().await.expect("body text");
    assert!(body.is_empty(), "HEAD body should be empty: {body:?}");
}

#[tokio::test]
async fn template_errors_answer_the_json_500_envelope() {
    let fixture = FixtureDir::create("error");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/boom")).await.expect("request ok");

    assert_eq!(response.status(), 500);
    let json = body_json(response).await;
    assert_eq!(json["error"]["code"], "INTERNAL_ERROR");
    let message = json["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("Template execution error"),
        "engine message missing: {message}"
    );
    assert!(json["error"]["request_id"].is_string());
}

#[tokio::test]
async fn runaway_loops_are_bounded_by_the_iteration_limit() {
    let fixture = FixtureDir::create("loop");
    let server = TestServer::start_with_config(fixture.config_with(|config| {
        config.templates.loop_iteration_limit = 1_000;
    }))
    .await;
    std::fs::write(
        fixture.path.join("views").join("loopy.jhs"),
        "<?jhs while (true) { } ?>",
    )
    .expect("loopy write");
    let response = reqwest::get(server.url("/loopy"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 500);
    let json = body_json(response).await;
    assert_eq!(json["error"]["code"], "INTERNAL_ERROR");
}

#[tokio::test]
async fn traversal_paths_are_never_intercepted() {
    let fixture = FixtureDir::create("traversal");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/..%2f..%2fetc%2fpasswd.jhs"))
        .await
        .expect("request ok");

    // ServeDir rejects the traversal; no template ever runs.
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn edited_templates_reload_via_mtime_invalidation() {
    let fixture = FixtureDir::create("reload");
    let server = TestServer::start_with_config(fixture.config()).await;
    let first = reqwest::get(server.url("/hello.jhs"))
        .await
        .expect("request ok")
        .text()
        .await
        .expect("body text");
    assert!(first.ends_with("footer42"), "first render: {first}");

    // Move the mtime, then rewrite the template.
    std::thread::sleep(Duration::from_millis(30));
    std::fs::write(
        fixture.path.join("static").join("hello.jhs"),
        "rewritten <?= 7 * 6 ?>",
    )
    .expect("rewrite");

    let second = reqwest::get(server.url("/hello.jhs"))
        .await
        .expect("request ok")
        .text()
        .await
        .expect("body text");
    assert_eq!(second, "rewritten 42", "cache must invalidate by mtime");
}

#[tokio::test]
async fn static_non_jhs_files_pass_through_untouched() {
    let fixture = FixtureDir::create("static-files");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/style.css"))
        .await
        .expect("css ok");

    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("content-type")
        .expect("content type header")
        .to_str()
        .expect("ascii value")
        .starts_with("text/css"));
    assert_eq!(response.text().await.expect("css body"), CSS);
}

#[tokio::test]
async fn disabled_config_serves_jhs_as_plain_static_files() {
    let fixture = FixtureDir::create("disabled");
    let server = TestServer::start_with_config(fixture.config_with(|config| {
        config.templates.enabled = false;
    }))
    .await;
    let response = reqwest::get(server.url("/hello.jhs"))
        .await
        .expect("request ok");

    // Without [templates], the raw source is a plain static asset.
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert!(body.contains("<?jhs"), "raw source expected: {body}");
}

#[tokio::test]
async fn template_responses_carry_the_security_headers() {
    let fixture = FixtureDir::create("headers");
    let server = TestServer::start_with_config(fixture.config()).await;
    let response = reqwest::get(server.url("/hello.jhs"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 200);
    for header in [
        "x-content-type-options",
        "x-frame-options",
        "referrer-policy",
    ] {
        assert!(
            response.headers().get(header).is_some(),
            "missing security header: {header}"
        );
    }
    assert!(
        response.headers().get("x-request-id").is_some(),
        "template responses carry the correlation id"
    );
}

#[tokio::test]
async fn templates_without_static_serving_still_auto_route_views() {
    let fixture = FixtureDir::create("views-only");
    let server = TestServer::start_with_config(fixture.config_with(|config| {
        config.static_files.enabled = false;
    }))
    .await;
    let response = reqwest::get(server.url("/contacto"))
        .await
        .expect("request ok");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert!(body.contains("contacto 2"), "view without static: {body}");
}

// ── The `user` template global (node-jhs2's extra data argument) ──────

/// Registers `username` (the first account bootstraps the `admin` role,
/// later ones are regular users) and logs in, returning the access token.
async fn register_and_login(server: &TestServer, username: &str, password: &str) -> String {
    let client = reqwest::Client::new();
    let registered = client
        .post(server.url("/api/auth/register"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("registration request succeeds");
    assert_eq!(registered.status(), 201, "registration must succeed");

    let login = client
        .post(server.url("/api/auth/login"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .expect("login request succeeds");
    assert_eq!(login.status(), 200, "login must succeed");
    let body: Value = login.json().await.expect("login body is JSON");
    body["access_token"]
        .as_str()
        .expect("access token present")
        .to_owned()
}

#[tokio::test]
async fn templates_receive_the_authenticated_user() {
    let fixture = FixtureDir::create("user-data");
    let (config, _db) = fixture.config_with_auth(|_| {});
    let server = TestServer::start_full(config).await;

    // The first registered account bootstraps the admin role.
    let admin_token = register_and_login(&server, "root-admin", "sup3r-secret!").await;
    let client = reqwest::Client::new();
    let response = client
        .get(server.url("/profile"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("request ok");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert_eq!(body, "<h1>Bienvenido, administrador root-admin</h1>");

    // Later accounts are regular users: the member branch renders.
    let member_token = register_and_login(&server, "ana", "password-456").await;
    let response = client
        .get(server.url("/profile"))
        .header("Authorization", format!("Bearer {member_token}"))
        .send()
        .await
        .expect("request ok");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert_eq!(body, "<h1>Hola ana (user)</h1>");
}

#[tokio::test]
async fn templates_render_anonymous_without_a_token() {
    let fixture = FixtureDir::create("user-anon");
    let (config, _db) = fixture.config_with_auth(|_| {});
    let server = TestServer::start_full(config).await;

    let response = reqwest::get(server.url("/profile"))
        .await
        .expect("request ok");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert_eq!(body, "<h1>por favor, inicia sesión</h1>");
}

#[tokio::test]
async fn templates_render_anonymous_for_invalid_tokens() {
    let fixture = FixtureDir::create("user-invalid");
    let (config, _db) = fixture.config_with_auth(|_| {});
    let server = TestServer::start_full(config).await;

    // A public page must not become an error because of a bad token:
    // rendering degrades to the anonymous branch instead.
    let client = reqwest::Client::new();
    for token in ["garbage-token", "a.b.c", "too.many.segments.here"] {
        let response = client
            .get(server.url("/profile"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("request ok");
        assert_eq!(response.status(), 200, "token: {token}");
        let body = response.text().await.expect("body text");
        assert_eq!(
            body, "<h1>por favor, inicia sesión</h1>",
            "anonymous render expected for token: {token}"
        );
    }
}

#[tokio::test]
async fn exposing_the_user_object_can_be_disabled() {
    let fixture = FixtureDir::create("user-off");
    let (config, _db) = fixture.config_with_auth(|config| {
        config.templates.expose_user = false;
    });
    let server = TestServer::start_full(config).await;

    let token = register_and_login(&server, "root-admin", "sup3r-secret!").await;
    let client = reqwest::Client::new();
    let response = client
        .get(server.url("/profile"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("request ok");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert_eq!(body, "<h1>por favor, inicia sesión</h1>");
}

#[tokio::test]
async fn user_is_null_when_auth_is_disabled() {
    let fixture = FixtureDir::create("user-noauth");
    let server = TestServer::start_with_config(fixture.config()).await;

    let response = reqwest::get(server.url("/profile"))
        .await
        .expect("request ok");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body text");
    assert_eq!(body, "<h1>por favor, inicia sesión</h1>");
}
