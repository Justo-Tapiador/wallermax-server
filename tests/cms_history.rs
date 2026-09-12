//! The F11 history battery: page revisions with restore, and
//! scheduled publishing.
//!
//! Boots the full server like every CMS battery and drives it with
//! cookie-storing `reqwest` clients. The revision cap is pinned to 2
//! in its own test so the pruning assertions stay small, while the
//! rest runs on the real defaults.
//!
//! Coverage:
//! - every save appends a revision (create, edit, restore) with the
//!   editor's optional note, newest first, the current one marked;
//! - the revision detail view renders the body as **escaped source**
//!   (a revision is never executed, not even by an editor);
//! - restoring copies an old snapshot back as a NEW revision — and
//!   never touches the live publication state (a draft stays draft,
//!   a published page stays published);
//! - the restore re-validates today's world: a slug another page has
//!   taken since, and a parent that no longer exists, both bounce
//!   back as inline errors without appending anything;
//! - `[cms] max_revisions` prunes the oldest snapshots on save;
//! - a draft with a future `publish_at` is invisible to anonymous
//!   visitors on every public read (`/p/{slug}`, the `/p` index,
//!   FTS5 search, both feeds, the sitemap) and shows the editor the
//!   "se publicará automáticamente" banner — then becomes publicly
//!   visible on all of them the moment the clock passes the schedule
//!   (read-time visibility: no background task exists to wait for);
//! - saving normalizes the schedule: a published page drops it, a
//!   past date is spent and dropped, a future one round-trips into
//!   the edit form;
//! - the history routes sit behind the role gates (anonymous →
//!   `/login` redirect, plain users → the friendly 403) and missing
//!   pages/revisions redirect home instead of erroring.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

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

/// Everything the CMS needs on a fresh database, with a small
/// `max_revisions` when the pruning battery asks for it.
fn history_config(max_revisions: u32) -> (AppConfig, common::TempDbGuard) {
    let (mut config, db) = auth_config();
    config.templates.enabled = true;
    config.cms.enabled = true;
    config.cms.max_revisions = max_revisions;
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
            "username={username}&password={password}&redirect=%2F"
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

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix seconds as the `YYYY-MM-DDTHH:MM:SS` UTC value the
/// `datetime-local` field accepts — with seconds, so a `+4s` future
/// really is four seconds out (minute truncation would eat the
/// margin). Carries its own civil-date math (the test cannot reach
/// the crate-private helpers).
fn datetime_local(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    // civil_from_days, inlined (the tests never leave 1970..9999).
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
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// The full page-creation form: publication flag, optional schedule
/// (`datetime-local` value), optional revision note and optional
/// parent. Returns the new page id.
#[allow(clippy::too_many_arguments)]
async fn create_page_full(
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

/// Creates a plain published page with no riders.
async fn create_page(
    server: &TestServer,
    client: &reqwest::Client,
    slug: &str,
    title: &str,
    content: &str,
) -> i64 {
    create_page_full(server, client, slug, title, content, true, None, None, None).await
}

/// Rewrites a page through the edit form (publish state, optional
/// schedule and note included).
#[allow(clippy::too_many_arguments)]
async fn update_page_full(
    server: &TestServer,
    client: &reqwest::Client,
    id: i64,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
    schedule: Option<&str>,
    note: Option<&str>,
) {
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
    let response = client
        .post(server.url(&format!("/admin/pages/{id}")))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("page update succeeds");
    assert_eq!(response.status(), 303, "update redirects (PRG)");
}

/// Edits a page without riders.
async fn update_page(
    server: &TestServer,
    client: &reqwest::Client,
    id: i64,
    slug: &str,
    title: &str,
    content: &str,
    publish: bool,
) {
    update_page_full(
        server, client, id, slug, title, content, publish, None, None,
    )
    .await;
}

/// Fetches the history page body.
async fn history_body(server: &TestServer, client: &reqwest::Client, id: i64) -> String {
    let response = client
        .get(server.url(&format!("/admin/pages/{id}/history")))
        .send()
        .await
        .expect("history listing succeeds");
    assert_eq!(response.status(), 200, "the history renders for editors");
    response.text().await.expect("history body")
}

/// How many revision rows the history page carries.
fn revision_rows(body: &str) -> usize {
    body.matches(">Revisión ").count()
}

/// Boots the standard battery server with the default revision cap.
/// The `TempDbGuard` travels back with the server: dropping it
/// earlier would unlink the database under the live pool.
async fn standard_server() -> (TestServer, reqwest::Client, common::TempDbGuard) {
    let (config, db) = history_config(25);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "admin", "password-12345678").await;
    let client = login_browser(&server, "admin", "password-12345678").await;
    (server, client, db)
}

#[tokio::test]
async fn every_save_appends_a_revision() {
    let (server, client, _guard) = standard_server().await;

    let id = create_page_full(
        &server,
        &client,
        "historial-pagina",
        "La página con historial",
        "Versión <strong>uno</strong> del cuerpo",
        false,
        None,
        Some("primera versión"),
        None,
    )
    .await;

    // Revision 1: the creation snapshot, noted by its author.
    let body = history_body(&server, &client, id).await;
    assert_eq!(revision_rows(&body), 1, "creation leaves revision 1");
    assert!(body.contains(">Revisión 1"), "the single row is revision 1");
    assert!(
        body.contains("— actual"),
        "the newest revision is marked current"
    );
    assert!(body.contains("primera versión"), "the note rides along");
    assert!(body.contains("por admin"), "the editor is attributed");

    // Two edits: one with a note, one without.
    update_page_full(
        &server,
        &client,
        id,
        "historial-pagina",
        "La página con historial",
        "Versión dos del cuerpo",
        false,
        None,
        Some("reescrita"),
    )
    .await;
    update_page(
        &server,
        &client,
        id,
        "historial-pagina",
        "La página con historial",
        "Versión tres del cuerpo",
        true,
    )
    .await;

    let body = history_body(&server, &client, id).await;
    assert_eq!(revision_rows(&body), 3, "every save appended one");
    // Newest first: revision 3 leads the list.
    let first = body.find(">Revisión ").expect("a first row exists");
    assert!(
        body[first..].starts_with(">Revisión 3"),
        "newest first (got {})",
        &body[first..first + 12]
    );
    assert!(body.contains(">Revisión 2") && body.contains(">Revisión 1"));
    assert!(body.contains("reescrita"), "the second note rides along");

    // The detail view of revision 1 shows the ORIGINAL body, escaped —
    // a revision is source, never rendered.
    let response = client
        .get(server.url(&format!("/admin/pages/{id}/history/1")))
        .send()
        .await
        .expect("revision detail succeeds");
    assert_eq!(response.status(), 200);
    let detail = response.text().await.expect("revision body");
    assert!(detail.contains("Revisión 1 de «La página con historial»"));
    assert!(
        detail.contains("&lt;strong&gt;uno&lt;/strong&gt;"),
        "the body renders as escaped source, not HTML"
    );
    assert!(
        detail.contains("Restaurar esta revisión"),
        "the restore form is offered"
    );
}

#[tokio::test]
async fn restore_recovers_content_and_records_itself() {
    let (server, client, _guard) = standard_server().await;

    let id = create_page(
        &server,
        &client,
        "restaurable",
        "Restaurable",
        "Contenido original v1",
    )
    .await;
    update_page(
        &server,
        &client,
        id,
        "restaurable",
        "Restaurable",
        "Contenido nuevo v2",
        true,
    )
    .await;

    // Restore revision 1 (the original content).
    let response = client
        .post(server.url(&format!("/admin/pages/{id}/history/1/restore")))
        .send()
        .await
        .expect("restore succeeds");
    assert_eq!(response.status(), 303, "restore redirects (PRG)");
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("redirect location");
    assert_eq!(
        location,
        format!("/admin/pages/{id}/edit?ok=restaurada"),
        "the edit form greets the restored page"
    );

    // The public page is back to the original content — and still
    // published (the live state, not the snapshot's, was preserved).
    let response = anon_client()
        .get(server.url("/p/restaurable"))
        .send()
        .await
        .expect("public fetch succeeds");
    assert_eq!(response.status(), 200, "the page stays published");
    let body = response.text().await.expect("public body");
    assert!(body.contains("Contenido original v1"));
    assert!(!body.contains("Contenido nuevo v2"));

    // The restore itself is recorded as revision 3, noted as such.
    let body = history_body(&server, &client, id).await;
    assert_eq!(revision_rows(&body), 3, "the restore appended a revision");
    assert!(body.contains("Restaurada desde la revisión 1."));
}

#[tokio::test]
async fn restore_never_touches_the_publication_state() {
    let (server, client, _guard) = standard_server().await;

    // Draft → published → restore: stays PUBLISHED (a restore does
    // not resurrect the draft state of the snapshot).
    let drafted = create_page_full(
        &server,
        &client,
        "sube-y-restaura",
        "Publicada después",
        "cuerpo v1",
        false,
        None,
        None,
        None,
    )
    .await;
    update_page(
        &server,
        &client,
        drafted,
        "sube-y-restaura",
        "Publicada después",
        "cuerpo v2",
        true,
    )
    .await;
    client
        .post(server.url(&format!("/admin/pages/{drafted}/history/1/restore")))
        .send()
        .await
        .expect("restore succeeds");
    let response = anon_client()
        .get(server.url("/p/sube-y-restaura"))
        .send()
        .await
        .expect("public fetch succeeds");
    assert_eq!(
        response.status(),
        200,
        "restoring a draft snapshot keeps the page live"
    );

    // Published → draft → restore: stays DRAFT (a restore does not
    // publish).
    let live = create_page(
        &server,
        &client,
        "baja-y-restaura",
        "Publicada antes",
        "cuerpo v1",
    )
    .await;
    update_page(
        &server,
        &client,
        live,
        "baja-y-restaura",
        "Publicada antes",
        "cuerpo v2",
        false,
    )
    .await;
    client
        .post(server.url(&format!("/admin/pages/{live}/history/1/restore")))
        .send()
        .await
        .expect("restore succeeds");
    let response = anon_client()
        .get(server.url("/p/baja-y-restaura"))
        .send()
        .await
        .expect("public fetch succeeds");
    assert_eq!(
        response.status(),
        404,
        "restoring a published snapshot keeps the draft hidden"
    );
}

#[tokio::test]
async fn restore_validates_todays_world() {
    let (server, client, _guard) = standard_server().await;

    // A slug another page has taken since the snapshot.
    let moved = create_page(&server, &client, "slug-conflicto", "Movida", "v1").await;
    update_page(&server, &client, moved, "slug-libre", "Movida", "v2", true).await;
    create_page(
        &server,
        &client,
        "slug-conflicto",
        "La otra página",
        "ocupante",
    )
    .await;

    let response = client
        .post(server.url(&format!("/admin/pages/{moved}/history/1/restore")))
        .send()
        .await
        .expect("restore bounces");
    assert_eq!(
        response.status(),
        200,
        "the slug conflict re-renders the revision view with the error"
    );
    let body = response.text().await.expect("revision body");
    assert!(body.contains("Ese slug ya existe"));
    // Nothing was appended: the history still holds the two saves.
    let body = history_body(&server, &client, moved).await;
    assert_eq!(
        revision_rows(&body),
        2,
        "the failed restore appended nothing"
    );
    // And the live page kept its post-edit content.
    let public = anon_client()
        .get(server.url("/p/slug-libre"))
        .send()
        .await
        .expect("public fetch succeeds");
    let public_body = public.text().await.expect("public body");
    assert!(public_body.contains("v2"));

    // A parent that no longer exists.
    let parent = create_page(
        &server,
        &client,
        "padre-fallecido",
        "El padre",
        "contenido padre",
    )
    .await;
    let child = create_page_full(
        &server,
        &client,
        "hija",
        "La hija",
        "contenido hija",
        true,
        None,
        None,
        Some(parent),
    )
    .await;
    // Deleting the parent reparents the child, but revision 1 of the
    // child still names the dead parent.
    client
        .post(server.url(&format!("/admin/pages/{parent}/delete")))
        .send()
        .await
        .expect("parent deletion succeeds");
    let response = client
        .post(server.url(&format!("/admin/pages/{child}/history/1/restore")))
        .send()
        .await
        .expect("restore bounces");
    assert_eq!(
        response.status(),
        200,
        "the dead parent re-renders with the error"
    );
    let body = response.text().await.expect("revision body");
    assert!(body.contains("La página padre de la revisión ya no existe"));
}

#[tokio::test]
async fn revision_cap_prunes_the_oldest() {
    let (config, _db) = history_config(2);
    let server = TestServer::start_full(config).await;
    register_admin(&server, "admin", "password-12345678").await;
    let client = login_browser(&server, "admin", "password-12345678").await;

    let id = create_page(&server, &client, "podada", "Podada", "v1").await;
    for round in 2..=5 {
        update_page(
            &server,
            &client,
            id,
            "podada",
            "Podada",
            &format!("v{round}"),
            true,
        )
        .await;
    }

    // Five saves, cap 2: only revisions 5 and 4 remain, newest first.
    let body = history_body(&server, &client, id).await;
    assert_eq!(
        revision_rows(&body),
        2,
        "the cap pruned the oldest snapshots"
    );
    let first = body.find(">Revisión ").expect("a first row exists");
    assert!(
        body[first..].starts_with(">Revisión 5"),
        "newest first (got {})",
        &body[first..first + 12]
    );
    assert!(body.contains(">Revisión 4"));
    assert!(!body.contains(">Revisión 1"), "the pruned ones are gone");

    // The pruned revision does not resolve anymore.
    let response = client
        .get(server.url(&format!("/admin/pages/{id}/history/1")))
        .send()
        .await
        .expect("pruned revision lookup");
    assert_eq!(
        response.status(),
        303,
        "a pruned revision redirects back to the history"
    );
}

#[tokio::test]
async fn scheduled_pages_hide_then_publish_at_read_time() {
    let (server, client, _guard) = standard_server().await;
    let anon = anon_client();

    // A draft scheduled four seconds out (with-second precision, so
    // the margin is real).
    let schedule = unix_now() + 4;
    let id = create_page_full(
        &server,
        &client,
        "programada-criogenica",
        "La página programada",
        "Contenido con la palabra única criptobúho",
        false,
        Some(&datetime_local(schedule)),
        Some("programada para el test"),
        None,
    )
    .await;

    // --- While the schedule is pending: invisible on EVERY public
    //     read, visible (with the banner) to editors.
    let response = anon
        .get(server.url("/p/programada-criogenica"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404, "the detail page 404s while pending");

    let listing = anon
        .get(server.url("/p"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !listing.contains("programada-criogenica"),
        "not in the index"
    );
    assert!(
        !listing.contains("La página programada"),
        "not in the index by title"
    );

    let search = anon
        .get(server.url("/buscar?q=criptob%C3%BAho"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !search.contains("programada-criogenica"),
        "not in FTS5 search"
    );

    let rss = anon
        .get(server.url("/feed.xml"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !rss.contains("programada-criogenica"),
        "not in the RSS feed"
    );
    let atom = anon
        .get(server.url("/atom.xml"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !atom.contains("programada-criogenica"),
        "not in the Atom feed"
    );
    let sitemap = anon
        .get(server.url("/sitemap.xml"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !sitemap.contains("programada-criogenica"),
        "not in the sitemap"
    );

    // Editors see it, with the schedule banner.
    let response = client
        .get(server.url("/p/programada-criogenica"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "editors preview the scheduled draft"
    );
    let preview = response.text().await.unwrap();
    assert!(preview.contains("Borrador"));
    assert!(
        preview.contains("Se publicará automáticamente"),
        "the banner states the schedule"
    );

    // The admin tree badges it.
    let tree = client
        .get(server.url("/admin/pages"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        tree.contains("programada"),
        "the admin tree shows the schedule badge"
    );

    // --- The clock passes the schedule: no write happens anywhere,
    //     the reads simply start seeing it.
    tokio::time::sleep(std::time::Duration::from_secs(7)).await;

    let response = anon
        .get(server.url("/p/programada-criogenica"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "the detail page went live on time");
    let body = response.text().await.unwrap();
    assert!(body.contains("Contenido con la palabra única criptobúho"));
    assert!(
        !body.contains("Borrador"),
        "the public render has no draft banner"
    );

    let listing = anon
        .get(server.url("/p"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        listing.contains("programada-criogenica"),
        "in the index once due"
    );

    let search = anon
        .get(server.url("/buscar?q=criptob%C3%BAho"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        search.contains("programada-criogenica"),
        "FTS5 finds it once due"
    );

    let rss = anon
        .get(server.url("/feed.xml"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        rss.contains("programada-criogenica"),
        "in the RSS feed once due"
    );
    let atom = anon
        .get(server.url("/atom.xml"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        atom.contains("programada-criogenica"),
        "in the Atom feed once due"
    );
    let sitemap = anon
        .get(server.url("/sitemap.xml"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        sitemap.contains("programada-criogenica"),
        "in the sitemap once due"
    );

    // The page itself is untouched: one revision, still a draft row
    // (visibility is read-time, the flag never flipped).
    let body = history_body(&server, &client, id).await;
    assert_eq!(revision_rows(&body), 1, "going live wrote nothing");
}

#[tokio::test]
async fn schedules_normalize_on_save() {
    let (server, client, _guard) = standard_server().await;
    let anon = anon_client();

    // Published + a schedule: the flag wins, the field is cleared.
    let both = create_page_full(
        &server,
        &client,
        "publicada-con-fecha",
        "Publicada y con fecha",
        "contenido",
        true,
        Some(&datetime_local(unix_now() + 3_600)),
        None,
        None,
    )
    .await;
    let edit = client
        .get(server.url(&format!("/admin/pages/{both}/edit")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        edit.contains("name=\"publish_at\" value=\"\""),
        "a published page carries no schedule"
    );
    assert!(edit.contains("checked"), "the page is live");
    let tree = client
        .get(server.url("/admin/pages"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        tree.contains(">publicada<"),
        "the badge says publicada, not programada"
    );

    // Draft + a past date: the schedule is spent, dropped.
    let spent = create_page_full(
        &server,
        &client,
        "fecha-gastada",
        "Con una fecha pasada",
        "contenido",
        false,
        Some(&datetime_local(unix_now() - 3_600)),
        None,
        None,
    )
    .await;
    let edit = client
        .get(server.url(&format!("/admin/pages/{spent}/edit")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        edit.contains("name=\"publish_at\" value=\"\""),
        "a spent schedule is dropped on save"
    );
    let response = anon
        .get(server.url("/p/fecha-gastada"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404, "a spent schedule is just a draft");

    // Draft + a future date: round-trips into the edit form.
    let future = datetime_local(unix_now() + 86_400);
    let pending = create_page_full(
        &server,
        &client,
        "fecha-futura",
        "Con una fecha futura",
        "contenido",
        false,
        Some(&future),
        None,
        None,
    )
    .await;
    let edit = client
        .get(server.url(&format!("/admin/pages/{pending}/edit")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The form re-emits the schedule at minute precision (what
    // `datetime-local` inputs carry): compare against the truncated
    // value, seconds and all.
    let future_minutes = &future[..16];
    assert!(
        edit.contains(&format!("name=\"publish_at\" value=\"{future_minutes}\"")),
        "the future schedule round-trips into the form"
    );

    // Unchecking «Publicada» on a formerly scheduled page unpublishes
    // for real: the round-tripped spent date is dropped instead of
    // resurrecting the schedule (a future date with the flag off would
    // be a legitimate re-schedule and stays).
    update_page_full(
        &server,
        &client,
        pending,
        "fecha-futura",
        "Con una fecha futura",
        "contenido editado",
        false,
        Some(&datetime_local(unix_now() - 60)),
        None,
    )
    .await;
    let edit = client
        .get(server.url(&format!("/admin/pages/{pending}/edit")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        edit.contains("name=\"publish_at\" value=\"\""),
        "unpublishing drops the schedule, it does not resurrect it"
    );
    let response = anon
        .get(server.url("/p/fecha-futura"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404, "the page is a plain draft again");
}

#[tokio::test]
async fn history_routes_sit_behind_the_role_gates() {
    let (server, client, _guard) = standard_server().await;
    let id = create_page(&server, &client, "vigilada", "Vigilada", "contenido").await;

    // Anonymous: redirected to the login.
    let response = anon_client()
        .get(server.url(&format!("/admin/pages/{id}/history")))
        .send()
        .await
        .expect("anonymous history fetch");
    assert_eq!(response.status(), 303, "anonymous visitors are redirected");
    assert!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|location| location.starts_with("/login")),
        "the redirect points at the login"
    );

    // A plain user: the friendly 403, on the list, the detail and the
    // restore POST alike.
    register_admin(&server, "plainuser", "password-12345678").await;
    let plain = login_browser(&server, "plainuser", "password-12345678").await;
    let response = plain
        .get(server.url(&format!("/admin/pages/{id}/history")))
        .send()
        .await
        .expect("plain user history fetch");
    assert_eq!(response.status(), 403, "plain users cannot browse history");
    let response = plain
        .get(server.url(&format!("/admin/pages/{id}/history/1")))
        .send()
        .await
        .expect("plain user revision fetch");
    assert_eq!(response.status(), 403, "plain users cannot read revisions");
    let response = plain
        .post(server.url(&format!("/admin/pages/{id}/history/1/restore")))
        .send()
        .await
        .expect("plain user restore");
    assert_eq!(response.status(), 403, "the guard covers the restore POST");
}

#[tokio::test]
async fn missing_pages_and_revisions_redirect_home() {
    let (server, client, _guard) = standard_server().await;
    let id = create_page(&server, &client, "existente", "Existente", "contenido").await;

    // A page that does not exist: back to the list.
    let response = client
        .get(server.url("/admin/pages/999999/history"))
        .send()
        .await
        .expect("missing page history fetch");
    assert_eq!(response.status(), 303);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/admin/pages")
    );

    // A revision that does not exist: back to the history.
    let response = client
        .get(server.url(&format!("/admin/pages/{id}/history/99")))
        .send()
        .await
        .expect("missing revision fetch");
    assert_eq!(response.status(), 303);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some(format!("/admin/pages/{id}/history").as_str())
    );

    // Restoring a missing revision: back to the history, harmlessly.
    let response = client
        .post(server.url(&format!("/admin/pages/{id}/history/99/restore")))
        .send()
        .await
        .expect("missing revision restore");
    assert_eq!(response.status(), 303);
    let body = history_body(&server, &client, id).await;
    assert_eq!(revision_rows(&body), 1, "nothing was appended");
}
