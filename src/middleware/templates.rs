//! Dynamic `.jhs` template rendering (the `[templates]` section).
//!
//! Sits between the timeout middleware and the route tree, so every
//! template response still carries the security headers, the request id
//! and the access log entry, and runaway renders stay bounded by the
//! request timeout on top of the sandbox's loop limit.
//!
//! Per request (while enabled):
//!
//! 1. Non-`GET`/`HEAD` requests pass through untouched — rendering is
//!    read-only by definition.
//! 2. Paths with `..` segments (checked **after** percent-decoding) or
//!    backslashes pass through; `[static]` keeps its own traversal
//!    protections and answers them.
//! 3. `GET`/`HEAD` for an existing `*.jhs` file under the `[static]`
//!    root is **rendered** — the template source is never served raw.
//!    (F14: on the **main host** only — `public/` belongs to it; the
//!    CMS host never renders or serves `.jhs` sources from there.
//!    F17: a **tenant host** gets the same treatment for the `.jhs`
//!    files under its own organization's document root.)
//! 4. `GET`/`HEAD /` while `[cms] default_page` names an existing page
//!    renders that CMS page directly (v0.11.0) — through the exact
//!    `GET /p/{slug}` pipeline (sandboxed body render,
//!    `views/cms_page.jhs` wrapper, draft gating) and **before** the
//!    normal pipeline below, so the explicit configuration beats the
//!    public `index.jhs`, the static index file, the views
//!    auto-routing and the JSON 404.
//!    Rendering directly (no redirect) keeps `/` the canonical URL. A
//!    slug that no longer exists logs a warning and falls back to the
//!    normal chain; a draft default page follows the `/p/{slug}`
//!    gating (404 for the public, banner for editors).
//! 5. Directory requests on the **main host** (the single host
//!    included) render the directory's `index.jhs` when one exists:
//!    `GET /` renders `root/index.jhs`, `GET /docs/` renders
//!    `root/docs/index.jhs` — before the static index answer, after
//!    the `default_page` check (the explicit configuration still
//!    wins). The chain per directory: `index.jhs` rendered, then
//!    `index_file`/`index.html` served statically, then the normal
//!    404 handling below. (F17: a tenant host's directories follow
//!    the same chain inside its own document root.)
//! 6. Everything else runs the normal pipeline (API routes first, then
//!    static files).
//! 7. A pipeline `404` auto-routes to the views directory before the
//!    JSON envelope is returned: `GET /contact` renders
//!    `views/contact.jhs`, `GET /blog` renders `views/blog.jhs` or
//!    `views/blog/index.jhs`, and `GET /` falls back to
//!    `views/index.jhs` when the static index file is missing (and no
//!    CMS default page took it over). (F14: while hostnames are
//!    mapped, steps 4 and 7 — the CMS-host behaviours — only run for
//!    requests whose `Host` maps to the CMS organization, and step 5
//!    — the main-like hosts' directory indexes — for every other
//!    host; the main host's 404s stay 404s.)
//!
//! Rendering happens on the blocking pool (`spawn_blocking`): the JS
//! engine is CPU-bound and the fresh-sandbox-per-render design keeps it
//! off the async workers. Template errors answer the standard JSON
//! error envelope (`500 INTERNAL_ERROR`) with the engine's message,
//! which is template-author-facing diagnostics rather than a leak:
//! template code never sees server internals.
//!
//! ## Template data: the `user`, `path`, `query`, `pages`, `menus` and
//! `req` globals
//!
//! Every render receives a small set of globals (see [`base_data`]):
//!
//! - `user` — the verified identity (`{ id, username, role }`) or
//!   `null` for anonymous visitors, exactly as in v0.6/v0.7;
//! - `path` — the request path (v0.8.0): the login/register modals post
//!   it back as their `redirect` field so users land where they were;
//! - `query` — the request's query parameters as an object of
//!   first-value strings (v0.8.0): how the flash-style error codes
//!   (`?login_error=…`) reach the templates;
//! - `pages` — the published CMS pages (`[{ id, slug, title,
//!   updated_at }]`, newest first, capped) while the CMS is enabled:
//!   navigation menus and the home listing render from it;
//! - `menus` — the named navigation menus (F7): `{ main: [{ label,
//!   href }] }`-shaped, items resolved to published pages or custom
//!   URLs, `{}` while the CMS is off — the corporate header/footer
//!   render from it;
//! - `req` — an Express-shaped request object (v0.9.0):
//!   `{ method, url, path, query, headers }`. Only a fixed allowlist
//!   of harmless headers (`accept`, `accept-language`, `content-type`,
//!   `host`, `referer`, `user-agent`) is exposed — CMS **editors**
//!   author page bodies, so `cookie`/`authorization` values must never
//!   reach template code (an editor page echoing the viewer's session
//!   cookie would leak it to the page author).
//! - `cms_origin` — the CMS surface's origin for cross-host links
//!   (F14 follow-up): `https://cms.example.com` while a CMS host is
//!   mapped — `cms.site_url` overrides, the sitemap's key — and the
//!   empty string otherwise, so `<?= cms_origin ?>/login` is a
//!   relative link on the single-host server and an absolute
//!   cross-host link once the split is on (see
//!   [`crate::vhosts::cms_origin`]; since F17 the first mapped CMS
//!   host is the domains table's first CMS row on a database boot);
//!
//! Templates can also call `res.redirect('/path')` (the Express-shaped
//! `res` shim, v0.9.0): the render records a **local-path-only**
//! redirect intent — same anti-open-redirect posture as the auth
//! forms — and [`render_response`] answers it with the corresponding
//! HTTP redirect instead of the HTML.
//!
//! `console.*` output inside templates is routed to `tracing` at the
//! matching level instead of the process stdout.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::Response;
use percent_encoding::percent_decode_str;
use serde_json::{json, Map, Value};

use crate::error::AppError;
use crate::extractors::{bearer_token, AuthUser};
use crate::middleware::request_id::RequestId;
use crate::routes::cms::{render_public_page, PageParts, PublicPageOutcome};
use crate::session;
use crate::state::AppState;
use crate::template_engine::{RedirectIntent, RenderOutput, TemplateRenderer};
use crate::util::encode_uri_component;

/// How many published CMS pages the `pages` global carries.
const PAGES_GLOBAL_LIMIT: i64 = 50;

/// Headers the `req` global may expose to templates. The allowlist is
/// fixed: CMS editors author template code, so anything carrying
/// credentials (`cookie`, `authorization`) or proxy secrets (`x-*`,
/// forwarding headers) stays out of the sandbox by construction.
const SAFE_REQ_HEADERS: [&str; 6] = [
    "accept",
    "accept-language",
    "content-type",
    "host",
    "referer",
    "user-agent",
];

/// Template middleware entry point; see the module docs for the
/// behaviour contract.
pub async fn run(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(templates) = state.templates() else {
        return next.run(request).await;
    };

    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return next.run(request).await;
    }

    let decoded = percent_decode_str(request.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    if !is_safe_relative(&decoded) {
        return next.run(request).await;
    }

    // F14/F17: while any hostname is mapped, this middleware
    // classifies the request with the same pure function the
    // dispatcher uses (crate::vhosts::classify), so both layers
    // agree on every request. The bindings live in the state's live
    // snapshot (F17 loaded it at boot; F18 made it reloadable — the
    // Tenants pages swap it after their writes). The CMS-host
    // behaviours (default page, views auto-routing) then run only
    // there, and `.jhs` files render from the static root on the
    // main host and from the organization's document root on a
    // tenant host. While nothing is mapped — the default — every
    // check below runs for everyone, exactly as before F14.
    let bindings = state.host_bindings();
    let vhosts = !bindings.is_empty();
    let class = crate::vhosts::classify(request.uri(), request.headers(), &bindings);
    let on_cms_host = matches!(class, crate::vhosts::HostClass::Cms);
    // F20: a mapped tenant host is a visitor host too — it mounts the
    // CMS family scoped to its organization, so the views
    // auto-routing (the no-JS `/login`, `/register`, `/profile`
    // pages) answers there exactly like on the CMS host. Only the
    // main host stays out (its surface is the static site plus the
    // operator machinery).
    let on_visitor_host = on_cms_host || matches!(class, crate::vhosts::HostClass::Tenant(_));

    // The `.jhs` rendering root for this request (F17): the tenant's
    // document root on a tenant host, the static root on the main
    // host (the single-host server included). The CMS host has no
    // root to render from — its surface is the views tree, and the
    // `!on_cms_host` guards below keep it that way.
    let render_root: Option<&std::path::Path> = match &class {
        crate::vhosts::HostClass::Tenant(binding) => Some(binding.root()),
        _ => templates.static_root(),
    };

    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());

    let data = base_data(&state, request.headers(), request.uri(), &method).await;

    // On-the-fly rendering of `.jhs` files — the main-like hosts'
    // dynamic surface (F14: the CMS host never renders `public/`
    // templates; F17: a tenant's document root is its own dynamic
    // surface, rendered the same way).
    if let Some(render_root) = render_root {
        if decoded.ends_with(".jhs") && !on_cms_host {
            let candidate = render_root.join(trim_leading_slash(&decoded));
            if candidate.is_file() {
                return render_response(
                    templates.engine(),
                    candidate,
                    data,
                    request_id,
                    &method,
                    StatusCode::OK,
                )
                .await;
            }
        }
    }

    // The CMS default page takes over the homepage (v0.11.0). This
    // runs BEFORE the pipeline, so the explicit configuration wins
    // over `public/index.html` and the views auto-routing by
    // construction. A slug that no longer exists (page deleted,
    // typo) degrades gracefully: a warning, then the normal chain —
    // the homepage never hard-fails because of a content lookup.
    // (F14: with virtual hosts, `/` belongs to the CMS host — the
    // main host's homepage is its own `public/index.html`.)
    if decoded == "/" && (!vhosts || on_cms_host) {
        if let Some(slug) = state.config().cms.default_page.as_deref() {
            if state.cms().is_some() {
                let parts = PageParts::of(&request);
                match render_public_page(&state, &parts, &data, slug).await {
                    PublicPageOutcome::Served(response) => return response,
                    PublicPageOutcome::Missing => tracing::warn!(
                        slug,
                        "the configured `cms.default_page` does not exist; GET / falls back \
                         to the normal homepage chain"
                    ),
                }
            }
        }
    }

    // The main-like hosts' directory indexes: `index.jhs` before the
    // static index (the single host included). A directory request —
    // `/` or any path ending in `/` — whose directory holds an
    // `index.jhs` renders it through the same pipeline an explicit
    // `.jhs` request takes (sandboxed render, `no-store`, never the
    // raw source); directories without one flow on to the static
    // answer unchanged. The CMS host is excluded: its directories
    // are the views tree's business, and `public/` never leaks there.
    // A tenant host's directories resolve inside its own root (F17).
    if (decoded == "/" || decoded.ends_with('/')) && (!vhosts || !on_cms_host) {
        if let Some(render_root) = render_root {
            let candidate = directory_index_jhs(render_root, &decoded);
            if candidate.is_file() {
                return render_response(
                    templates.engine(),
                    candidate,
                    data,
                    request_id,
                    &method,
                    StatusCode::OK,
                )
                .await;
            }
        }
    }

    // Normal pipeline (API routes, static files, JSON 404 fallback).
    let response = next.run(request).await;

    // Auto-route views for otherwise-unmatched paths — the visitor
    // hosts' surface (F14; F20: every tenant host included).
    if response.status() == StatusCode::NOT_FOUND && (!vhosts || on_visitor_host) {
        if let Some(view) = view_candidate(templates.views_dir(), &decoded) {
            return render_response(
                templates.engine(),
                view,
                data,
                request_id,
                &method,
                StatusCode::OK,
            )
            .await;
        }
    }

    response
}

/// Builds the per-request template data: the `user`, `path`, `query`,
/// `pages` and `req` globals.
///
/// The identity comes from the **verified** Bearer header or session
/// cookie (the same `JwtService::verify_token` path the API extractors
/// use), so the injected `role` is server-issued. Any verification
/// failure — no header and no cookie, auth disabled, malformed,
/// expired or foreign-signed token — degrades to `user = null`: a
/// public page renders anonymously rather than erroring out.
///
/// Crate-visible so the CMS handlers render their views with the exact
/// same globals the auto-routed pages get.
pub(crate) async fn base_data(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
    method: &Method,
) -> Map<String, Value> {
    let auth = state
        .auth_context()
        .filter(|_| state.config().templates.expose_user);
    let auth_user = auth
        .and_then(|auth| {
            bearer_token(headers)
                .or_else(|| session::session_token(headers))
                .and_then(|token| auth.jwt.verify_token(token).ok())
        })
        .and_then(|claims| AuthUser::from_claims(&claims).ok());
    let user = auth_user
        .as_ref()
        .map(|user| {
            json!({
                "id": user.user_id,
                "username": user.username,
                "role": user.role.as_str(),
            })
        })
        .unwrap_or(Value::Null);

    // F20: the content this request's templates see belongs to the
    // organization its host resolved to — the same pure resolution
    // the dispatcher and the guards use, computed once here and
    // threaded into the scoped globals (and exposed as the
    // `organization` global so panel chrome like the sidebar can
    // condition on it).
    let organization = crate::vhosts::organization_key(uri, headers, &state.host_bindings());

    let mut data = Map::new();
    data.insert(String::from("user"), user);
    data.insert(String::from("path"), Value::String(uri.path().to_owned()));
    data.insert(String::from("query"), query_global(uri));
    data.insert(
        String::from("pages"),
        pages_global(state, &organization).await,
    );
    data.insert(
        String::from("menus"),
        menus_global(state, &organization).await,
    );
    data.insert(String::from("req"), req_global(headers, uri, method));
    data.insert(String::from("theme"), theme_global(headers));
    data.insert(String::from("cms_origin"), cms_origin_global(state));
    data.insert(
        String::from("login_url"),
        Value::String(login_url_global(state, headers, uri)),
    );
    // F21: the signed-in user's role inside the request's
    // organization — what the panel's chrome needs to tell a
    // tenant's own administrators from its editors (the token's
    // platform role says nothing about a tenant, by design). Only
    // the panel's own paths pay the indexed read: the public
    // surface never does, anonymous requests included, and a
    // storage failure degrades to "no role shown" — the guards
    // still authorize per request against the table.
    let member_role = match (auth, auth_user.as_ref()) {
        (Some(auth), Some(user)) if uri.path().starts_with("/admin") => {
            match auth
                .repository
                .membership_role(user.user_id, &organization)
                .await
            {
                Ok(Some(role)) => Value::String(role.as_str().to_owned()),
                Ok(None) => Value::Null,
                Err(error) => {
                    tracing::warn!(%error, "member role lookup failed");
                    Value::Null
                }
            }
        }
        _ => Value::Null,
    };
    data.insert(String::from("member_role"), member_role);
    data.insert(String::from("organization"), Value::String(organization));
    data
}

/// The panel theme preference (F12): the `wm_theme` cookie pinned by
/// `GET /admin/theme`, as `"dark"`/`"light"`, or `null` while the
/// visitor never chose — the admin stylesheet's
/// `prefers-color-scheme` fallback then follows the OS. Only the two
/// literal values survive: a corrupted or hand-crafted cookie reads
/// as "no preference".
///
/// The cookie is HttpOnly, and `SAFE_REQ_HEADERS` keeps the raw
/// cookie header out of the `req` global — this Rust-side read is
/// the only place templates can learn the preference from.
fn theme_global(headers: &HeaderMap) -> Value {
    pinned_theme(headers).map_or(Value::Null, |theme| Value::String(theme.to_owned()))
}

/// The raw `wm_theme` cookie value ("dark"/"light") for Rust-side
/// pages that render outside the engine (the shared error page, F13)
/// — the borrowed core of [`theme_global`].
pub(crate) fn pinned_theme(headers: &HeaderMap) -> Option<&str> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(raw) = value.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some((name, theme)) = pair.split_once('=') {
                if name == "wm_theme" && (theme == "dark" || theme == "light") {
                    return Some(theme);
                }
            }
        }
    }
    None
}

/// The `cms_origin` global (F14 follow-up): the CMS surface's origin
/// for cross-host links, or the empty string while every surface
/// shares one host. Public information by construction — the first
/// mapped CMS host and the serving scheme — so no gating is needed;
/// the value is derived at boot and re-derived on every snapshot
/// refresh (F17/F18), the derivation and precedence living in
/// [`crate::vhosts::cms_origin`].
fn cms_origin_global(state: &AppState) -> Value {
    Value::String(state.cms_origin())
}

/// The `login_url` global (F19's round trip): the no-JavaScript
/// sign-in entry that **returns to the page asking for it**.
///
/// While the CMS surface has its own host, the sign-in link on the
/// main host's `public/index.jhs` needs both halves of the trip: the
/// login page lives on the CMS host (`cms_origin` + `/login`), and
/// the page to return to lives on the caller's — so the global is
/// `cms_origin` + `/login?redirect=` + this request's absolute URL
/// (the `Host` header as the browser addressed it, with the scheme
/// `[tls]` serves — the same derivation `cms_origin` uses; behind a
/// TLS-terminating proxy the derived scheme can lie about the
/// protocol, never about the host, and hosts are what the redirect
/// allowlist checks). On the single-host server the link is the
/// relative `/login?redirect=<this page's path>`, because `/login`
/// is same-origin there and a local path is all the `redirect` field
/// needs.
///
/// One template, both shapes: `<a href="<?= login_url ?>">Sign in</a>`
/// signs the visitor in and lands them back on the very page they
/// clicked from — the `redirect` allowlist in [`crate::routes::auth`]
/// is exactly the family of hosts the shared session covers.
fn login_url_global(state: &AppState, headers: &HeaderMap, uri: &Uri) -> String {
    let here = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or("/");
    // F20: a tenant host serves `/login` itself (its tree mounts the
    // auth forms), so its sign-in link is the same-origin relative
    // shape the single-host server uses — no detour through the CMS
    // host, and the shared session carries the `303` straight back.
    let bindings = state.host_bindings();
    if matches!(
        crate::vhosts::classify(uri, headers, &bindings),
        crate::vhosts::HostClass::Tenant(_)
    ) {
        return format!("/login?redirect={}", encode_uri_component(here));
    }
    let cms_origin = state.cms_origin();
    if cms_origin.is_empty() {
        return format!("/login?redirect={}", encode_uri_component(here));
    }
    let scheme = if state.config().tls.enabled {
        "https"
    } else {
        "http"
    };
    let here = match crate::vhosts::request_host(uri, headers) {
        Some(host) => format!("{scheme}://{host}{here}"),
        // A request without a Host header is no browser's navigation;
        // the local-path shape keeps the link working anyway.
        None => here.to_owned(),
    };
    format!(
        "{cms_origin}/login?redirect={}",
        encode_uri_component(&here)
    )
}

/// The `req` global (v0.9.0): an Express-shaped request object with a
/// sanitized header allowlist — see [`SAFE_REQ_HEADERS`].
fn req_global(headers: &HeaderMap, uri: &Uri, method: &Method) -> Value {
    let url = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or_else(|| uri.path());

    json!({
        "method": method.as_str(),
        "url": url,
        "path": uri.path(),
        "query": query_global(uri),
        "headers": safe_req_headers(headers),
    })
}

/// The allowlisted headers actually present on the request, as plain
/// strings. Anything not in [`SAFE_REQ_HEADERS`] is simply absent —
/// templates cannot even detect that a `cookie` header existed.
fn safe_req_headers(headers: &HeaderMap) -> Value {
    let mut map = Map::new();
    for name in SAFE_REQ_HEADERS {
        if let Some(value) = headers.get(name) {
            map.insert(
                name.to_owned(),
                Value::String(value.to_str().unwrap_or_default().to_owned()),
            );
        }
    }
    Value::Object(map)
}

/// The `query` global: first-value-wins object of the query string.
fn query_global(uri: &Uri) -> Value {
    let Some(query) = uri.query() else {
        return Value::Object(Map::new());
    };

    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap_or_default();
    let mut map = Map::new();
    for (key, value) in pairs {
        // First value wins, mirroring the common multi-map reading.
        map.entry(key).or_insert(Value::String(value));
    }
    Value::Object(map)
}

/// The `pages` global: the request's organization's published CMS
/// pages, newest first (empty while the CMS is disabled).
async fn pages_global(state: &AppState, organization: &str) -> Value {
    let Some(cms) = state.cms() else {
        return Value::Array(Vec::new());
    };

    match cms
        .pages
        .list(organization, false, PAGES_GLOBAL_LIMIT)
        .await
    {
        Ok(pages) => Value::Array(
            pages
                .into_iter()
                .map(|page| {
                    json!({
                        "id": page.id,
                        "slug": page.slug,
                        "title": page.title,
                        "updated_at": page.updated_at,
                        "updated_at_h": crate::util::format_timestamp(page.updated_at),
                    })
                })
                .collect(),
        ),
        Err(error) => {
            tracing::warn!(%error, "the pages template global could not be loaded");
            Value::Array(Vec::new())
        }
    }
}

/// The `menus` global (F7): the request's organization's named
/// navigation menus, each an array of `{ label, href }` items resolved
/// server-side — published pages and custom URLs only, so the public
/// navigation never links a 404. `{}` while the CMS is disabled;
/// templates read `menus.main`.
async fn menus_global(state: &AppState, organization: &str) -> Value {
    let Some(cms) = state.cms() else {
        return Value::Object(Map::new());
    };

    match cms.menus.resolved(organization).await {
        Ok(menus) => {
            let mut map = Map::new();
            for (menu, items) in menus {
                map.insert(
                    menu.name,
                    Value::Array(
                        items
                            .into_iter()
                            .map(|item| {
                                json!({
                                    "label": item.label,
                                    "href": item.url,
                                })
                            })
                            .collect(),
                    ),
                );
            }
            Value::Object(map)
        }
        Err(error) => {
            tracing::warn!(%error, "the menus template global could not be loaded");
            Value::Object(Map::new())
        }
    }
}

/// Renders `path` with `data` and builds the response with `status`
/// (or the JSON error envelope).
///
/// Crate-visible so the CMS handlers render through the exact same
/// pipeline (blocking pool, console capture, `no-store`, security
/// headers) as the auto-routed views. A `res.redirect()` issued
/// inside the template wins over the HTML: the response is the
/// redirect (with the template-chosen status) instead.
pub(crate) async fn render_response(
    engine: std::sync::Arc<dyn TemplateRenderer>,
    path: PathBuf,
    data: Map<String, Value>,
    request_id: Option<String>,
    method: &Method,
    status: StatusCode,
) -> Response {
    let label = path.display().to_string();
    let render = tokio::task::spawn_blocking(move || engine.render(&label, &data)).await;

    let output = match render {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return AppError::internal(error.to_string())
                .into_response_with_request_id(request_id.as_deref());
        }
        Err(join_error) => {
            tracing::error!(%join_error, "template rendering task failed");
            return AppError::internal("template rendering task failed".to_owned())
                .into_response_with_request_id(request_id.as_deref());
        }
    };

    log_console(&output);

    if let Some(redirect) = &output.redirect {
        return redirect_response(redirect, request_id.as_deref());
    }

    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(output.html)
    };

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_else(|error| {
            tracing::error!(%error, "failed to build the template response");
            AppError::internal("failed to build the template response".to_owned())
                .into_response_with_request_id(request_id.as_deref())
        })
}

/// Builds the HTTP response for a [`RedirectIntent`] recorded by
/// `res.redirect()`. Crate-visible so the CMS page handler can honour
/// redirects issued from inside stored page bodies.
pub(crate) fn redirect_response(redirect: &RedirectIntent, request_id: Option<&str>) -> Response {
    let status = StatusCode::from_u16(redirect.status).unwrap_or(StatusCode::FOUND);
    Response::builder()
        .status(status)
        .header(header::LOCATION, redirect.location.as_str())
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .unwrap_or_else(|error| {
            tracing::error!(%error, "failed to build the template redirect response");
            AppError::internal("failed to build the template redirect response".to_owned())
                .into_response_with_request_id(request_id)
        })
}

/// Routes captured `console.*` lines to `tracing` at the matching level
/// (`console.log` maps to `info`, mirroring Node's stdout/stderr split).
fn log_console(output: &RenderOutput) {
    for line in &output.console {
        match line.level.as_str() {
            "error" => tracing::error!(target: "wallermax::templates", "{}", line.message),
            "warn" => tracing::warn!(target: "wallermax::templates", "{}", line.message),
            "info" | "log" => tracing::info!(target: "wallermax::templates", "{}", line.message),
            "trace" => tracing::trace!(target: "wallermax::templates", "{}", line.message),
            _ => tracing::debug!(target: "wallermax::templates", "{}", line.message),
        }
    }
}

/// A path is safe to resolve against the configured roots when it has
/// no `..` segments and no backslashes (a Windows path separator, and
/// therefore a potential traversal vector there).
fn is_safe_relative(path: &str) -> bool {
    !path.contains('\\') && path.split('/').all(|segment| segment != "..")
}

/// Drops the leading slash so the path joins cleanly onto a root.
fn trim_leading_slash(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

/// First existing view candidate for a (safe, decoded) request path:
///
/// - `/` → `views/index.jhs`;
/// - `/x.jhs` → `views/x.jhs`;
/// - `/contact` → `views/contact.jhs`, then `views/contact/index.jhs`;
/// - `/blog/` → the same two shapes as `/blog`.
///
/// `/admin/*` never auto-routes (F20): those views are the panel's
/// chrome, rendered by their handlers with their data — never
/// standalone. On the CMS organization's host the routes take
/// precedence anyway; on a tenant host the panel's platform surface
/// is not mounted at all, and auto-routing its views there (with no
/// handler data behind them) would render broken pages instead of
/// the honest 404.
fn view_candidate(views_dir: &Path, decoded: &str) -> Option<PathBuf> {
    if decoded == "/admin" || decoded.starts_with("/admin/") {
        return None;
    }
    let relative = decoded.trim_matches('/');
    let candidates: Vec<PathBuf> = if relative.is_empty() {
        vec![views_dir.join("index.jhs")]
    } else if relative.ends_with(".jhs") {
        vec![views_dir.join(relative)]
    } else {
        vec![
            views_dir.join(format!("{relative}.jhs")),
            views_dir.join(relative).join("index.jhs"),
        ]
    };
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// The `index.jhs` candidate for a directory request (`/` or a path
/// ending in `/`): `/` maps to the static root's own `index.jhs`,
/// `/docs/` to `docs/index.jhs` inside the root. It stays a
/// **candidate** — the caller renders it only when it exists, and
/// otherwise the static index answer flows on unchanged.
fn directory_index_jhs(static_root: &Path, decoded: &str) -> PathBuf {
    let relative = decoded.trim_matches('/');
    if relative.is_empty() {
        static_root.join("index.jhs")
    } else {
        static_root.join(relative).join("index.jhs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_paths_are_accepted() {
        assert!(is_safe_relative("/hello.jhs"));
        assert!(is_safe_relative("/blog/post.jhs"));
        assert!(is_safe_relative("/"));
    }

    #[test]
    fn traversal_and_backslash_paths_are_rejected() {
        assert!(!is_safe_relative("/../secret.jhs"));
        assert!(!is_safe_relative("/a/../../etc/passwd"));
        assert!(!is_safe_relative(
            "/..%2f..%2fsecret.jhs".replace("%2f", "/").as_str()
        ));
        assert!(!is_safe_relative("/a\\..\\..\\secret.jhs"));
    }

    #[test]
    fn view_candidates_prefer_the_flat_view_then_the_index() {
        let dir = std::env::temp_dir().join(format!(
            "wallermax-jhs-mw-{}-candidates",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("blog")).expect("fixture dirs");
        std::fs::write(dir.join("index.jhs"), "x").expect("write");
        std::fs::write(dir.join("blog").join("index.jhs"), "x").expect("write");
        std::fs::write(dir.join("flat.jhs"), "x").expect("write");

        assert_eq!(view_candidate(&dir, "/"), Some(dir.join("index.jhs")));
        assert_eq!(
            view_candidate(&dir, "/blog"),
            Some(dir.join("blog").join("index.jhs"))
        );
        assert_eq!(
            view_candidate(&dir, "/admin/users"),
            None,
            "panel views never auto-route (F20)"
        );
        assert_eq!(
            view_candidate(&dir, "/admin"),
            None,
            "the panel root neither"
        );
        assert_eq!(view_candidate(&dir, "/flat"), Some(dir.join("flat.jhs")));
        assert_eq!(
            view_candidate(&dir, "/flat.jhs"),
            Some(dir.join("flat.jhs"))
        );
        assert_eq!(view_candidate(&dir, "/missing"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_index_candidates_map_request_shapes_onto_the_root() {
        let root = Path::new("/srv/public");
        assert_eq!(directory_index_jhs(root, "/"), root.join("index.jhs"));
        assert_eq!(
            directory_index_jhs(root, "/docs/"),
            root.join("docs").join("index.jhs")
        );
        assert_eq!(
            directory_index_jhs(root, "/deep/tree/"),
            root.join("deep").join("tree").join("index.jhs")
        );
    }
}
