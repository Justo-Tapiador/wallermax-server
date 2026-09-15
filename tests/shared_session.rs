//! The shared session battery (F19): one sign-in, every host.
//!
//! Boots the full server exactly like the binary (database, auth,
//! templates, static files and the CMS) with `cms.hosts` =
//! `["cms.app.localhost"]` and the new `[auth] cookie_domain =
//! "app.localhost"`, so one IP and one port serve a main host
//! (`app.localhost`, the unknown-name fallback), a CMS host
//! (`cms.app.localhost`) and a tenant host (`tenant.app.localhost`, a
//! side-SQL organization) — all of them under the shared
//! two-label parent.
//!
//! The walk mirrors the operator's browser:
//!
//! - the **form login** on the CMS host answers the `303` with a
//!   `Set-Cookie` carrying `Domain=app.localhost` (plus the `Path=/`,
//!   `HttpOnly`, `SameSite=Strict` of every phase since v0.7.0) —
//!   the attribute is what makes one sign-in travel;
//! - that cookie, presented to the **CMS host**, personalises
//!   `/profile` — the exact regression of the first F19 attempt,
//!   where the browser silently refused the cookie (a one-label
//!   `Domain=localhost` from `cms.localhost`) and every page
//!   answered anonymous right after a successful sign-in;
//! - the **same cookie**, presented to the **main host**, greets
//!   `user.username` from `public/index.jhs` — the F19 acceptance:
//!   the template needed zero changes;
//! - the **tenant host** personalises the same way: its own
//!   document root, the same shared session;
//! - `logout` (form and JSON) and `refresh` address the cookie with
//!   the **same** `Domain` — a clear that forgets it would leave the
//!   old session alive, a rotation that drops it would log the
//!   browser out;
//! - the **register** form path logs the fresh account straight into
//!   the shared session;
//! - the control: with `cookie_domain` empty (the default) the
//!   `Set-Cookie` carries **no** `Domain` — the host-only cookie of
//!   every phase before F19.
//!
//! ## What this battery cannot prove
//!
//! reqwest pins the `Host` header per request but keys its cookie jar
//! on the connection's URL host (the ephemeral `127.0.0.1:port`), so
//! a jar cannot model a browser storing `Domain=app.localhost` for
//! virtual host names — and, being lax where browsers are strict, it
//! would have accepted the one-label `localhost` value that killed
//! the first attempt. The battery therefore pins the **server
//! contract** — the exact `Set-Cookie` attributes, and that any host
//! holding the cookie personalises its pages. The **browser side**
//! (a real `Domain=app.localhost` cookie stored from
//! `cms.app.localhost`, replayed on the main and tenant hosts,
//! cleared on logout) is verified against a live server with a real
//! browser before the patch ships — the acceptance bar for any
//! future change to the cookie attributes.

mod common;

use common::{auth_config, TestServer};
use reqwest::header::{HeaderValue, HOST};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use wallermax_server::config::AppConfig;

/// The hostname the CMS answers on.
const CMS_HOST: &str = "cms.app.localhost";

/// A main-host name: unknown to the bindings, so the dispatcher
/// serves it the main tree — the `public/` static site.
const MAIN_HOST: &str = "app.localhost";

/// The tenant organization's hostname (a side-SQL row, loaded at the
/// second boot).
const TENANT_HOST: &str = "tenant.app.localhost";

/// The shared parent domain of every host above — the F19 value
/// under test (two labels: a one-label domain is a public suffix to
/// browsers, which is what killed the first attempt).
const SHARED_DOMAIN: &str = "app.localhost";

/// The bootstrap administrator.
const ADMIN: (&str, &str) = ("justo", "supersecret8");

/// A redirect-free client (each hop is asserted by hand).
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builds")
}

/// A `Host` header value for the tests.
fn host(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("test host value")
}

/// The full stack with the CMS pinned to [`CMS_HOST`] and the shared
/// session on: `cookie_domain` = [`SHARED_DOMAIN`].
///
/// The returned [`FixtureDir`] must outlive the server: its `Drop`
/// removes the tree, so binding it in the test keeps the static root
/// on disk while the requests run.
fn shared_config() -> (AppConfig, common::TempDbGuard, FixtureDir) {
    let (mut config, db) = auth_config();
    let fixture = apply_full_stack(&mut config);
    config.auth.cookie_domain = String::from(SHARED_DOMAIN);
    (config, db, fixture)
}

/// The full stack with the host-only cookie (the pre-F19 control).
fn host_only_config() -> (AppConfig, common::TempDbGuard, FixtureDir) {
    let (mut config, db) = auth_config();
    let fixture = apply_full_stack(&mut config);
    (config, db, fixture)
}

/// The database, auth, templates, static and CMS layers with this
/// battery's fixture tree (see [`FixtureDir`]).
fn apply_full_stack(config: &mut AppConfig) -> FixtureDir {
    let fixture = FixtureDir::create();
    config.templates.enabled = true;
    config.templates.views_dir = fixture.views_dir();
    config.templates.modules_dir = fixture.modules_dir();
    config.static_files.enabled = true;
    config.static_files.root_dir = fixture.static_root();
    config.static_files.index_file = String::from("index.html");
    config.cms.enabled = true;
    config.cms.hosts = vec![String::from(CMS_HOST)];
    fixture
}

/// Registers [`ADMIN`] (the first account — the bootstrap admin) over
/// JSON on the CMS host.
async fn register_admin(server: &TestServer) {
    let response = client()
        .post(server.url("/api/auth/register"))
        .header(HOST, host(CMS_HOST))
        .json(&serde_json::json!({
            "username": ADMIN.0,
            "password": ADMIN.1,
        }))
        .send()
        .await
        .expect("registration request");
    assert_eq!(
        response.status(),
        201,
        "the first account becomes the admin"
    );
}

/// The browser form login on the CMS host: `username`, `password` and
/// the `redirect` field the `/login` view posts. Returns the raw
/// response so each test pins what it needs (status, `Location`,
/// `Set-Cookie`).
async fn form_login(server: &TestServer) -> reqwest::Response {
    client()
        .post(server.url("/api/auth/login"))
        .header(HOST, host(CMS_HOST))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={}&password={}&redirect=%2Fprofile",
            ADMIN.0, ADMIN.1
        ))
        .send()
        .await
        .expect("form login request")
}

/// The `Set-Cookie` header of a response, if any.
fn set_cookie(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// The `wallermax_session` value inside a `Set-Cookie` header.
fn session_token_of(cookie: &str) -> String {
    let value = cookie
        .strip_prefix("wallermax_session=")
        .unwrap_or(cookie)
        .split(';')
        .next()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !value.is_empty(),
        "a session token rides the cookie: {cookie}"
    );
    value
}

/// GETs `path` pinned to `host_value` presenting the session cookie.
async fn get_with_session(
    server: &TestServer,
    path: &str,
    host_value: &str,
    token: &str,
) -> reqwest::Response {
    client()
        .get(server.url(path))
        .header(HOST, host(host_value))
        .header("Cookie", format!("wallermax_session={token}"))
        .send()
        .await
        .expect("session request")
}

/// A throwaway directory tree standing in for the main host's
/// `public/` (an `index.jhs` that greets beside the static
/// `index.html`) and the template directories, removed (best-effort)
/// on drop.
struct FixtureDir {
    path: PathBuf,
}

/// The main host's dynamic homepage: the F19 acceptance template,
/// with the round trip's sign-in link beside the greeting.
const MAIN_INDEX_JHS: &str = r#"<!DOCTYPE html>
<html>
<body>
<?jhs if (user) { ?>
<p id="saludo">Hola, <?= user.username ?> (<?= user.role ?>)</p>
<?jhs } else { ?>
<p id="saludo">Hola, anónimo</p>
<a id="entrar" href="<?= login_url ?>">Sign in</a>
<?jhs } ?>
</body>
</html>
"#;

/// The CMS host's `/profile` view: the same shape the shipped one
/// has — the session greeting and the guest fallback the first F19
/// attempt famously served instead.
const PROFILE_JHS: &str = r#"<!DOCTYPE html>
<html>
<body>
<?jhs if (user) { ?>
<p>Signed in as <?= user.username ?> (<?= user.role ?>)</p>
<?jhs } else { ?>
<p>This page is only for signed-in sessions. Sign in with your
   account from the header or go to the sign-in form.</p>
<?jhs } ?>
</body>
</html>
"#;

impl FixtureDir {
    /// Builds the tree under a unique temporary directory.
    fn create() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wallermax-shared-session-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(path.join("static")).expect("static dir");
        std::fs::create_dir_all(path.join("views")).expect("views dir");
        std::fs::create_dir_all(path.join("modules")).expect("modules dir");

        std::fs::write(path.join("static").join("index.jhs"), MAIN_INDEX_JHS)
            .expect("main index.jhs");
        std::fs::write(
            path.join("static").join("index.html"),
            "<!DOCTYPE html><html><body>static home</body></html>",
        )
        .expect("main index.html");
        std::fs::write(path.join("views").join("profile.jhs"), PROFILE_JHS).expect("profile view");
        Self { path }
    }

    /// Forward-slash path (valid on Windows as well).
    fn static_root(&self) -> String {
        to_forward_slashes(&self.path.join("static"))
    }

    /// Forward-slash path (valid on Windows as well).
    fn views_dir(&self) -> String {
        to_forward_slashes(&self.path.join("views"))
    }

    /// Forward-slash path (valid on Windows as well).
    fn modules_dir(&self) -> String {
        to_forward_slashes(&self.path.join("modules"))
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A path as a forward-slash string.
fn to_forward_slashes(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}

// ── The sign-in: the cookie that travels ───────────────────────────

#[tokio::test]
async fn form_login_on_the_cms_host_sets_the_shared_domain() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;

    let response = form_login(&server).await;
    assert_eq!(response.status(), 303, "the form login redirects");
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/profile"),
        "the redirect target the /login view posts"
    );

    let cookie = set_cookie(&response);
    assert!(
        cookie.starts_with("wallermax_session="),
        "the session cookie: {cookie}"
    );
    assert!(
        cookie.contains(&format!("; Domain={SHARED_DOMAIN}")),
        "the F19 attribute: {cookie}"
    );
    assert!(
        cookie.contains("Path=/")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Strict"),
        "the v0.7.0 posture survives: {cookie}"
    );
    assert!(
        !cookie.contains("Secure"),
        "plain-HTTP test server, auto mode: {cookie}"
    );
}

#[tokio::test]
async fn the_shared_session_personalises_the_cms_host() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;

    let token = session_token_of(&set_cookie(&form_login(&server).await));

    // The exact regression of the first F19 attempt: right after the
    // sign-in, the redirect target must know the session. The
    // repo's views/profile.jhs answers "Signed in as <username>".
    let response = get_with_session(&server, "/profile", CMS_HOST, &token).await;
    assert_eq!(response.status(), 200, "the profile renders");
    let body = response.text().await.expect("profile body");
    assert!(
        body.contains(ADMIN.0),
        "the CMS host greets the session: {body}"
    );
    assert!(
        !body.contains("This page is only for signed-in sessions"),
        "the guest fallback must not appear: {body}"
    );
}

#[tokio::test]
async fn the_shared_session_personalises_the_main_host() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;

    // Anonymous first: the homepage greets a visitor.
    let response = client()
        .get(server.url("/"))
        .header(HOST, host(MAIN_HOST))
        .send()
        .await
        .expect("anonymous main home");
    let body = response.text().await.expect("anonymous body");
    assert!(
        body.contains("Hola, anónimo"),
        "the anonymous greeting: {body}"
    );

    // Then signed-in: the same URL, the same template, zero template
    // changes — only the cookie travelled. The F19 acceptance.
    let token = session_token_of(&set_cookie(&form_login(&server).await));
    let response = get_with_session(&server, "/", MAIN_HOST, &token).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("signed-in body");
    assert!(
        body.contains(&format!("Hola, {}", ADMIN.0)),
        "the main host greets the shared session: {body}"
    );
}

#[tokio::test]
async fn the_shared_session_personalises_the_tenant_host() {
    let (config, db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;
    let token = session_token_of(&set_cookie(&form_login(&server).await));

    // A third organization with its own document root and host name,
    // created by hand between boots — the F17 SQL cookbook (the
    // bindings load at startup).
    let tenant_root = std::env::temp_dir().join(format!(
        "wallermax-shared-session-tenant-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&tenant_root).expect("tenant root");
    std::fs::write(
        tenant_root.join("index.jhs"),
        r#"<!DOCTYPE html>
<html>
<body>
<?jhs if (user) { ?>
<p id="saludo">Bienvenido, <?= user.username ?></p>
<?jhs } else { ?>
<p id="saludo">Bienvenido, visitante</p>
<?jhs } ?>
</body>
</html>
"#,
    )
    .expect("tenant index.jhs");
    std::fs::write(
        tenant_root.join("index.html"),
        "<!DOCTYPE html><html><body>tenant static</body></html>",
    )
    .expect("tenant index.html");

    let pool = side_db(db.url()).await;
    sqlx::query(
        "INSERT INTO organizations (key, name, document_root, created_at) \
         VALUES ('tenant1', 'Tenant One', ?1, 0)",
    )
    .bind(to_forward_slashes(&tenant_root))
    .execute(&pool)
    .await
    .expect("tenant organization inserted");
    sqlx::query(
        "INSERT INTO domains (hostname, organization_id, created_at) \
         SELECT ?1, id, 0 FROM organizations WHERE key = 'tenant1'",
    )
    .bind(TENANT_HOST)
    .execute(&pool)
    .await
    .expect("tenant domain inserted");
    pool.close().await;
    drop(server);

    // The second boot loads the tenant binding. A fresh fixture backs
    // the main host's tree; the database is the first boot's.
    let (mut config, _db2, _fixture2) = shared_config();
    config.database.url = db.url().to_owned();
    let server = TestServer::start_full(config).await;

    let response = get_with_session(&server, "/", TENANT_HOST, &token).await;
    assert_eq!(response.status(), 200, "the tenant home renders");
    let body = response.text().await.expect("tenant body");
    assert!(
        body.contains(&format!("Bienvenido, {}", ADMIN.0)),
        "the tenant host greets the shared session: {body}"
    );

    let _ = std::fs::remove_dir_all(&tenant_root);
}

// ── The round trip: the sign-in link that returns ───────────────

#[tokio::test]
async fn the_sign_in_link_carries_the_return_page() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;

    // The anonymous homepage's sign-in link: the login page on the
    // CMS host, carrying this very page back as the redirect target
    // (the derived origin carries the configured port — 8080, the
    // default — while the return URL takes the Host header as the
    // browser addressed it).
    let response = client()
        .get(server.url("/"))
        .header(HOST, host(MAIN_HOST))
        .send()
        .await
        .expect("anonymous main home");
    let body = response.text().await.expect("main home body");
    assert!(
        body.contains(
            "href=\"http://cms.app.localhost:8080/login?redirect=http%3A%2F%2Fapp.localhost%2F\""
        ),
        "the sign-in link points at the login page carrying this page back: {body}"
    );
}

#[tokio::test]
async fn the_form_login_returns_to_the_main_host() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;

    // The second half of the round trip: the login form posts the
    // page it was opened from, and the 303 crosses the Host line
    // back to it — verbatim, query string included.
    let response = client()
        .post(server.url("/api/auth/login"))
        .header(HOST, host(CMS_HOST))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={}&password={}&redirect=http%3A%2F%2Fapp.localhost%2Fdocs%3Fpage%3D2",
            ADMIN.0, ADMIN.1
        ))
        .send()
        .await
        .expect("form login request");
    assert_eq!(response.status(), 303, "the form login redirects");
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("http://app.localhost/docs?page=2"),
        "the redirect crosses the Host line back to the main host"
    );
    assert!(
        set_cookie(&response).contains("; Domain=app.localhost"),
        "the session follows the redirect: {}",
        set_cookie(&response)
    );

    // A failed sign-in bounces back to the same page with the error
    // code composed into its query string.
    let response = client()
        .post(server.url("/api/auth/login"))
        .header(HOST, host(CMS_HOST))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "username={}&password=wrong-pass&redirect=http%3A%2F%2Fapp.localhost%2Fdocs%3Fpage%3D2",
            ADMIN.0
        ))
        .send()
        .await
        .expect("failed form login request");
    assert_eq!(response.status(), 303, "the failed login redirects");
    assert_eq!(
        response.headers()["location"],
        "http://app.localhost/docs?page=2&login_error=invalid#login",
        "the bounce-back composes with the target's query string"
    );
}

#[tokio::test]
async fn the_csp_widens_form_action_to_the_family() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;

    // The browser side of the round trip: `form-action 'self'` pins a
    // form's redirect to one origin, and the login form's `303` must
    // cross the Host line back to the main host — so the policy the
    // server ships has to admit the family. The configured port (8080,
    // the default) tags along, the domain and its subdomains are the
    // additions, and nothing foreign rides along with them.
    let response = client()
        .get(server.url("/login"))
        .header(HOST, host(CMS_HOST))
        .send()
        .await
        .expect("login page request");
    let csp = response
        .headers()
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        csp.contains("form-action 'self' http://*.app.localhost:8080 http://app.localhost:8080;"),
        "the shipped policy, widened to the round-trip family: {csp}"
    );
    assert!(!csp.contains("evil"), "nothing beyond the family: {csp}");

    // The control: the single-host server keeps the policy as
    // configured — no family, no widening.
    let (mut config, _db, _fixture) = host_only_config();
    config.cms.hosts = Vec::new();
    let server = TestServer::start_full(config).await;
    let response = client()
        .get(server.url("/"))
        .header(HOST, host("localhost"))
        .send()
        .await
        .expect("single-host home request");
    let csp = response
        .headers()
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        !csp.contains("app.localhost"),
        "the single-host policy stays as written: {csp}"
    );
}

#[tokio::test]
async fn off_family_redirect_targets_fall_back_to_the_cms_home() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;

    // The local-dev trap this battery pins: a main host the shared
    // domain does not cover (the one-label `localhost` split —
    // `cookie_domain` cannot name it) is NOT ours, so its round trip
    // is refused and the fallback is the CMS host's root.
    for target in [
        "http%3A%2F%2Flocalhost%3A8080%2F",
        "http%3A%2F%2Fevil.example%2Fphish",
    ] {
        let response = client()
            .post(server.url("/api/auth/login"))
            .header(HOST, host(CMS_HOST))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "username={}&password={}&redirect={target}",
                ADMIN.0, ADMIN.1
            ))
            .send()
            .await
            .expect("form login request");
        assert_eq!(response.status(), 303, "redirect target: {target}");
        assert_eq!(
            response.headers()["location"],
            "/",
            "off-family target {target} must fall back to /"
        );
    }
}

// ── The lifecycle: the same Domain everywhere ─────────────────────

#[tokio::test]
async fn form_logout_on_the_cms_host_clears_the_shared_cookie() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;
    let token = session_token_of(&set_cookie(&form_login(&server).await));

    let response = client()
        .post(server.url("/api/auth/logout"))
        .header(HOST, host(CMS_HOST))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Cookie", format!("wallermax_session={token}"))
        .body("redirect=%2F")
        .send()
        .await
        .expect("form logout request");
    assert_eq!(response.status(), 303, "the form logout redirects");

    let cookie = set_cookie(&response);
    assert!(
        cookie.starts_with("wallermax_session=;"),
        "the cookie is expired: {cookie}"
    );
    assert!(
        cookie.contains(&format!("; Domain={SHARED_DOMAIN}")),
        "the clear addresses the shared cookie the same way — a Domain-less clear \
         would leave it alive in the browser: {cookie}"
    );
    assert!(cookie.contains("Max-Age=0"), "the expiry: {cookie}");
}

#[tokio::test]
async fn json_logout_clears_the_shared_cookie_too() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;
    let token = session_token_of(&set_cookie(&form_login(&server).await));

    let response = client()
        .post(server.url("/api/auth/logout"))
        .header(HOST, host(CMS_HOST))
        .header("Cookie", format!("wallermax_session={token}"))
        // A logout without a refresh token to retire: the JSON path
        // still needs the field, an unknown value simply revokes
        // nothing (the idempotent 204).
        .json(&serde_json::json!({ "refresh_token": "absent" }))
        .send()
        .await
        .expect("json logout request");
    assert_eq!(response.status(), 204, "the json logout is a 204");

    let cookie = set_cookie(&response);
    assert!(
        cookie.contains(&format!("; Domain={SHARED_DOMAIN}")) && cookie.contains("Max-Age=0"),
        "the JSON clear carries the shared domain: {cookie}"
    );
}

#[tokio::test]
async fn refresh_rotates_the_shared_cookie() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;

    // A JSON login: the refresh token only rides the JSON body.
    let response = client()
        .post(server.url("/api/auth/login"))
        .header(HOST, host(CMS_HOST))
        .json(&serde_json::json!({
            "username": ADMIN.0,
            "password": ADMIN.1,
        }))
        .send()
        .await
        .expect("json login request");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("login body");
    let refresh_token = body
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .expect("refresh tokens are enabled")
        .to_owned();

    let response = client()
        .post(server.url("/api/auth/refresh"))
        .header(HOST, host(CMS_HOST))
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .expect("refresh request");
    assert_eq!(response.status(), 200, "the rotation succeeds");

    let cookie = set_cookie(&response);
    assert!(
        cookie.contains(&format!("; Domain={SHARED_DOMAIN}")),
        "the rotation keeps the shared session: {cookie}"
    );

    // And the rotated cookie still greets on the main host.
    let token = session_token_of(&cookie);
    let response = get_with_session(&server, "/", MAIN_HOST, &token).await;
    let body = response.text().await.expect("main home body");
    assert!(
        body.contains(&format!("Hola, {}", ADMIN.0)),
        "the rotated session personalises: {body}"
    );
}

#[tokio::test]
async fn register_logs_the_browser_into_the_shared_session() {
    let (config, _db, _fixture) = shared_config();
    let server = TestServer::start_full(config).await;

    // A second account over the browser form path: created and logged
    // in directly, session cookie included.
    let response = client()
        .post(server.url("/api/auth/register"))
        .header(HOST, host(CMS_HOST))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=ana&password=supersecret8&redirect=%2Fprofile")
        .send()
        .await
        .expect("form register request");
    assert_eq!(response.status(), 303, "the form register redirects");

    let cookie = set_cookie(&response);
    assert!(
        cookie.contains(&format!("; Domain={SHARED_DOMAIN}")),
        "the fresh session is shared from the first second: {cookie}"
    );

    let token = session_token_of(&cookie);
    let response = get_with_session(&server, "/", MAIN_HOST, &token).await;
    let body = response.text().await.expect("main home body");
    assert!(
        body.contains("Hola, ana"),
        "the main host greets the new account: {body}"
    );
}

// ── The control: the host-only default survives ────────────────────

#[tokio::test]
async fn without_a_configured_domain_the_cookie_stays_host_only() {
    let (config, _db, _fixture) = host_only_config();
    let server = TestServer::start_full(config).await;
    register_admin(&server).await;

    let response = form_login(&server).await;
    assert_eq!(response.status(), 303, "the form login still redirects");

    let cookie = set_cookie(&response);
    assert!(
        !cookie.contains("Domain"),
        "the pre-F19 default: no Domain attribute: {cookie}"
    );
    assert!(
        cookie.contains("SameSite=Strict") && cookie.contains("Path=/"),
        "the v0.7.0 posture: {cookie}"
    );

    // The host contract is unchanged: the CMS host still greets the
    // session it set.
    let token = session_token_of(&cookie);
    let response = get_with_session(&server, "/profile", CMS_HOST, &token).await;
    let body = response.text().await.expect("profile body");
    assert!(body.contains(ADMIN.0), "the CMS host greets: {body}");
}

// ── The side connection ────────────────────────────────────────────

/// Opens a side connection to the test's SQLite file, with the same
/// busy timeout the server itself uses (see the tenants suite).
async fn side_db(url: &str) -> sqlx::sqlite::SqlitePool {
    let options = SqliteConnectOptions::from_str(url)
        .expect("database url parses")
        .busy_timeout(Duration::from_secs(5));
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("side pool connects")
}
