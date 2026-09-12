//! The small built-in CMS (v0.8.0, the `[cms]` section).
//!
//! A database-backed content layer on top of the v0.7.0 session work:
//! the browser speaks plain HTML forms to the server, the server is the
//! only party that touches SQLite, and the session cookie stays
//! `HttpOnly` server-managed — exactly the "server as proxy" model.
//!
//! **Public site** (no role required):
//!
//! - `GET /p` — the published-pages index (auto-routed to
//!   `views/p.jhs`, which renders the `pages` global);
//! - `GET /p/{slug}` — renders one CMS page: the stored body is `.jhs`
//!   template source rendered through the same sandboxed engine (with
//!   the standard globals), wrapped in `views/cms_page.jhs`. Drafts
//!   answer 404 for the public and render with a banner for
//!   editors/admins.
//! - `GET /sitemap.xml` — the published pages (plus the canonical
//!   homepage while `default_page` names a published page) as a
//!   sitemap, behind `[cms] sitemap` (F7).
//!
//! **Corporate content model (F7)** — still zero JavaScript:
//!
//! - Pages are **hierarchical**: a parent and a sibling `position`, a
//!   cycle-proof move guard (a page can never hang under its own
//!   branch), breadcrumbs and `page.children` on the public render,
//!   and a depth-indented admin tree. Deleting a parent reparents its
//!   children to the top level — content is never lost with a branch.
//! - **Named menus** ("main", "footer"…) managed at `/admin/menus`:
//!   items link a page or a custom URL, and every template receives
//!   them resolved as the `menus` global. Items pointing at drafts
//!   are skipped publicly, so the navigation never links a 404.
//! - **SEO metadata** per page (`meta_title`, `meta_description`,
//!   `og_image`) rendered into the wrapper's `<head>`.
//!
//! **Admin panel** (`admin` and `editor` roles; browser-friendly
//! guards: unauthenticated visitors are redirected to `/login`, and
//! the forms re-render with their values and the error on failure so
//! nothing typed is ever lost):
//!
//! - `GET  /admin` — dashboard (page, menu and user counters);
//! - `GET  /admin/pages` — every page, drafts included, as a tree;
//!   create, edit (title/slug/body/parent/position/SEO/publish),
//!   delete;
//! - `GET  /admin/pages/new` / `POST /admin/pages` — create;
//! - `GET  /admin/pages/{id}/edit` / `POST /admin/pages/{id}` — edit;
//! - `POST /admin/pages/{id}/delete` — delete;
//! - `GET  /admin/pages/import` / `POST` — copy a file from `public/`
//!   into a new draft page (read-only on the static tree: the CMS
//!   never writes into `public/`);
//! - `GET/POST /admin/menus…` — the named navigation menus and their
//!   items (F7);
//! - `GET/POST /admin/media…` and the public `GET /media/{id}/{name}`
//!   family — the media library (F9, [`crate::routes::media`]): the
//!   same panel conventions with a `multipart/form-data` upload.
//!
//! **User management** (`admin` only, `/admin/users`): create, change
//! role, reset password, delete — with the last-admin and self-edit
//! guards so a panel mis-click can never lock the server out of its
//! own CMS.
//!
//! **Self-service**: `POST /perfil/password` lets any session change
//! its own password (the current one must be presented).
//!
//! Everything is JavaScript-free on purpose: the default CSP blocks
//! scripts entirely and the whole panel still works. Server
//! configuration (ports, TLS, secrets) is deliberately absent from the
//! panel — it lives in `wallermax.toml`, writable only with local
//! repository access: the CMS administrator manages content and users,
//! never the server.

use std::collections::HashMap;
use std::path::PathBuf;

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::auth::{hash_password, validate_password, validate_username, verify_password};
use crate::db::{
    BodyFormat, NewMenu, NewMenuItem, NewPage, PageSummary, PageUpdate, RepositoryError, User,
    UserRole,
};
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::middleware::request_id::RequestId;
use crate::middleware::templates::{base_data, redirect_response, render_response};
use crate::routes::search::fragment_segments;
use crate::state::{AppState, CmsContext};
use crate::util::{format_timestamp, iso_date, iso_datetime, read_form, rfc2822_date, unix_now};

/// Maximum listed pages / users in the admin panels.
const MAX_LISTED: i64 = 200;

/// Upper bound on a page body accepted from the admin form (characters).
/// The request-body limit caps the raw bytes; this keeps the stored
/// template source within a sane editing budget.
const MAX_PAGE_CONTENT: usize = 600_000;

/// Upper bound on a page title.
const MAX_TITLE: usize = 200;

/// Import caps: files listed per request, bytes per imported file, and
/// directory depth walked under the static root.
const MAX_IMPORT_LISTED: usize = 200;
const MAX_IMPORT_BYTES: u64 = 600_000;
const MAX_IMPORT_DEPTH: usize = 3;

/// Upper bound on a page's `meta_description` (F7): search engines
/// display roughly 160 characters; the stored budget is generous.
const MAX_META_DESCRIPTION: usize = 500;

/// Upper bound on a page's `og_image` and a menu item's URL (F7).
const MAX_URL_FIELD: usize = 500;

/// Upper bound on a menu item label (F7) — the custom-URL labels
/// share the page-title budget.
const MAX_MENU_LABEL: usize = 200;

/// Highest sibling position the forms accept (F7).
const MAX_POSITION: i64 = 99_999;

/// How many pages the sitemap lists (F7) — the protocol's own advised
/// per-file ceiling.
const SITEMAP_LIMIT: i64 = 50_000;

/// How many items `/feed.xml` and `/atom.xml` carry at most (F10). A
/// feed is a window over the site, not an archive: readers follow the
/// site for the rest.
const FEED_MAX_ITEMS: i64 = 20;

// ─── Browser-friendly guards ─────────────────────────────────────────

/// Extractor: an authenticated user allowed to manage CMS pages (the
/// `admin` and `editor` roles).
///
/// The rejection is browser-facing, not a JSON envelope: anonymous
/// visitors get a `303` to `/login?redirect=<this page>` and
/// authenticated-but-underprivileged users get a small HTML 403 page.
pub(crate) struct CmsEditor {
    pub(crate) user: AuthUser,
}

impl axum::extract::FromRequestParts<AppState> for CmsEditor {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state)
            .await
            .map_err(|rejection| {
                if rejection_is_unauthorized(&rejection) {
                    redirect_to_login(parts.uri.path())
                } else {
                    rejection.into_response()
                }
            })?;

        if !user.role.is_editor() {
            return Err(html_error_page(
                StatusCode::FORBIDDEN,
                "Se requiere el rol de editor o administrador",
                "Tu cuenta no puede gestionar el contenido del CMS.",
            ));
        }

        Ok(Self { user })
    }
}

/// Extractor: an authenticated `admin` (the user management and the
/// full panel).
pub(crate) struct CmsAdmin {
    pub(crate) user: AuthUser,
}

impl axum::extract::FromRequestParts<AppState> for CmsAdmin {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state)
            .await
            .map_err(|rejection| {
                if rejection_is_unauthorized(&rejection) {
                    redirect_to_login(parts.uri.path())
                } else {
                    rejection.into_response()
                }
            })?;

        if user.role != UserRole::Admin {
            return Err(html_error_page(
                StatusCode::FORBIDDEN,
                "Se requiere el rol de administrador",
                "Solo los administradores pueden gestionar las cuentas del CMS.",
            ));
        }

        Ok(Self { user })
    }
}

/// Whether the extractor rejection is an authentication failure (401)
/// rather than a privilege failure (403) — rejections carry the
/// [`AppError`] privately, so an accessor plus the status code is the
/// reliable signal.
fn rejection_is_unauthorized(rejection: &crate::extractors::Rejection) -> bool {
    rejection.error().status_code() == StatusCode::UNAUTHORIZED
}

/// A `303` to the login page carrying the current path, so the guard
/// sends browsers to a form instead of a JSON envelope.
fn redirect_to_login(path: &str) -> Response {
    let encoded = utf8_percent_encode(path);
    let location = format!("/login?redirect={encoded}");
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location)
        .body(axum::body::Body::empty())
        .expect("valid redirect")
}

/// Percent-encodes a path for use as a query-string value.
fn utf8_percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A minimal, script-free HTML error page (same style as the auth form
/// errors: browsers get pages, API clients get envelopes).
///
/// Crate-visible for the media routes (F9) — their public 404s match
/// the `/p/{slug}` behaviour by construction.
pub(crate) fn html_error_page(status: StatusCode, title: &str, message: &str) -> Response {
    let html = format!(
        "<!DOCTYPE html>\n<html lang=\"es\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n</head>\n<body>\n<h1>{title}</h1>\n\
         <p>{message}</p>\n<p><a href=\"/\">Volver al inicio</a></p>\n\
         </body>\n</html>\n",
        title = escape_html(title),
        message = escape_html(message),
    );

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(html))
        .expect("valid error page")
}

/// HTML-escapes a text for the server-built pages.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

// ─── Shared rendering plumbing ───────────────────────────────────────

/// The request facts the page flows need (method, headers, uri,
/// request id), captured once and threaded through the shared
/// renderers.
///
/// Crate-visible: the template middleware builds one for the v0.11.0
/// homepage takeover (`[cms] default_page`) so `GET /` renders through
/// the exact `GET /p/{slug}` pipeline.
pub(crate) struct PageParts {
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    request_id: Option<String>,
}

impl PageParts {
    pub(crate) fn of(request: &Request) -> Self {
        Self {
            method: request.method().clone(),
            headers: request.headers().clone(),
            uri: request.uri().clone(),
            request_id: request
                .extensions()
                .get::<RequestId>()
                .map(|id| id.0.clone()),
        }
    }

    /// The request URI — crate-visible so the F10 listing routes
    /// outside this module (`/admin/media`, `/buscar`) can read their
    /// `?q=`/`?page=` parameters the same way the handlers here do.
    pub(crate) fn uri(&self) -> &Uri {
        &self.uri
    }
}

/// Renders `views/<view>.jhs` with the base globals plus `extra`,
/// answering with `status`.
///
/// Crate-visible: the media library routes (F9) share the exact same
/// render pipeline for their admin views.
pub(crate) async fn render_view(
    state: &AppState,
    parts: &PageParts,
    view: &str,
    extra: Vec<(&str, Value)>,
    status: StatusCode,
) -> Response {
    let mut data = base_data(state, &parts.headers, &parts.uri, &parts.method).await;
    for (key, value) in extra {
        data.insert(key.to_owned(), value);
    }
    render_view_with_data(state, parts, data, view, status).await
}

/// Renders `views/<view>.jhs` with **precomputed** globals, answering
/// with `status`.
///
/// Split out of [`render_view`] so [`render_public_page`] can reuse
/// the `base_data` it already needs for the draft gate and the body
/// render instead of recomputing it (one `pages` query less per page
/// view).
async fn render_view_with_data(
    state: &AppState,
    parts: &PageParts,
    data: Map<String, Value>,
    view: &str,
    status: StatusCode,
) -> Response {
    let Some(templates) = state.templates() else {
        return AppError::internal("templates are not initialized").into_response();
    };

    let path = templates.views_dir().join(view);
    render_response(
        templates.engine(),
        path,
        data,
        parts.request_id.clone(),
        &axum::http::Method::GET,
        status,
    )
    .await
}

/// The CMS context (routes are only mounted while it exists).
/// Crate-visible for the media routes (F9), which ride the same mount.
#[allow(clippy::result_large_err)] // the Err is a one-shot browser response
pub(crate) fn cms_context(state: &AppState) -> Result<&CmsContext, Response> {
    state
        .cms()
        .ok_or_else(|| AppError::internal("the CMS is not initialized").into_response())
}

/// A bare `303 See Other`.
///
/// Crate-visible for the media routes (F9): same PRG convention.
pub(crate) fn see_other(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location)
        .body(axum::body::Body::empty())
        .expect("valid redirect")
}

// ─── Public: `GET /p/{slug}` (and the v0.11.0 homepage) ─────────────

/// What [`render_public_page`] decided for a slug.
///
/// Crate-visible so the template middleware can share the exact
/// `/p/{slug}` rendering for the v0.11.0 homepage takeover
/// (`[cms] default_page`): both routes behave identically by
/// construction.
pub(crate) enum PublicPageOutcome {
    /// A complete response: the rendered page, a `res.redirect()` issued
    /// by the page body, the hidden-draft 404 or an error envelope.
    Served(Response),
    /// No page row exists for the slug. `GET /p/{slug}` answers the
    /// HTML 404 view; the homepage instead falls back to its normal
    /// chain (static index file, then views auto-routing).
    Missing,
}

/// Renders one CMS page.
///
/// The stored body is `.jhs` source rendered with the standard globals
/// inside the same sandbox the views use — including the shared
/// partials, so page authors write `<?jhs include("partials/header") ?>`
/// exactly like the built-in views. The rendered HTML is embedded into
/// `views/cms_page.jhs` through `raw()` (it is server-generated output,
/// not user input; and the default CSP blocks scripts regardless).
///
/// Drafts are invisible to the public (same 404 as a missing slug) and
/// carry a preview banner for editors.
///
/// Crate-visible: the template middleware calls this for `GET /` while
/// `[cms] default_page` names a slug. `data` is the precomputed
/// `base_data` globals for the request (the caller needs them anyway);
/// the slug lookup happens **before** any cloning so a missing slug
/// costs one query and nothing else.
pub(crate) async fn render_public_page(
    state: &AppState,
    parts: &PageParts,
    data: &Map<String, Value>,
    slug: &str,
) -> PublicPageOutcome {
    let Ok(cms) = cms_context(state) else {
        return PublicPageOutcome::Served(
            AppError::internal("the CMS is not initialized").into_response(),
        );
    };

    let Some(page) = cms.pages.find_by_slug(slug).await.unwrap_or_else(|error| {
        tracing::error!(%error, "page lookup failed");
        None
    }) else {
        return PublicPageOutcome::Missing;
    };

    // Drafts: indistinguishable from missing pages unless the caller
    // may manage content.
    let viewer_is_editor = data
        .get("user")
        .and_then(|user| user.get("role"))
        .and_then(Value::as_str)
        .and_then(UserRole::parse)
        .is_some_and(|role| role.is_editor());

    if !page.is_published && !viewer_is_editor {
        return PublicPageOutcome::Served(
            render_view_with_data(state, parts, data.clone(), "404.jhs", StatusCode::NOT_FOUND)
                .await,
        );
    }

    // Render the page body with the mode the page states: `.jhs`
    // template source through the engine (the standard globals ride
    // along), Markdown through the safe renderer (F8).
    let body_html = match page.body_format {
        BodyFormat::Markdown => {
            let content = page.content.clone();
            match tokio::task::spawn_blocking(move || crate::markdown::render(&content)).await {
                Ok(html) => html,
                Err(join_error) => {
                    tracing::error!(%join_error, "page rendering task failed");
                    return PublicPageOutcome::Served(
                        AppError::internal("page rendering task failed".to_owned()).into_response(),
                    );
                }
            }
        }
        BodyFormat::Jhs => {
            let Some(templates) = state.templates() else {
                return PublicPageOutcome::Served(
                    AppError::internal("templates are not initialized").into_response(),
                );
            };
            let engine = templates.engine();
            let content = page.content.clone();
            let globals = data.clone();
            match tokio::task::spawn_blocking(move || engine.render_string(&content, &globals))
                .await
            {
                Ok(Ok(output)) => {
                    // A res.redirect() inside a stored page body redirects the
                    // whole page, exactly like it does inside a view.
                    if let Some(redirect) = &output.redirect {
                        return PublicPageOutcome::Served(redirect_response(
                            redirect,
                            parts.request_id.as_deref(),
                        ));
                    }
                    output.html
                }
                Ok(Err(error)) => {
                    // Template-author diagnostics, like every other render.
                    return PublicPageOutcome::Served(
                        AppError::internal(error.to_string()).into_response(),
                    );
                }
                Err(join_error) => {
                    tracing::error!(%join_error, "page rendering task failed");
                    return PublicPageOutcome::Served(
                        AppError::internal("page rendering task failed".to_owned()).into_response(),
                    );
                }
            }
        }
    };

    let author: Value = match (page.created_by, state.auth_context()) {
        (Some(user_id), Some(auth)) => match auth.repository.find_by_id(user_id).await {
            Ok(Some(author)) => Value::String(author.username),
            _ => Value::Null,
        },
        _ => Value::Null,
    };

    // F7: the hierarchy around the page — the ancestor chain (root
    // first, immediate parent last) feeds the breadcrumbs and the
    // `page.parent` link; the direct children (drafts only while the
    // viewer may manage content) feed `page.children`.
    let ancestors = match cms.pages.ancestors(page.id).await {
        Ok(chain) => chain,
        Err(error) => {
            tracing::error!(%error, "page hierarchy walk failed");
            Vec::new()
        }
    };
    let children = match cms.pages.children(Some(page.id), viewer_is_editor).await {
        Ok(children) => children,
        Err(error) => {
            tracing::error!(%error, "page children lookup failed");
            Vec::new()
        }
    };

    let breadcrumbs: Vec<Value> = ancestors
        .iter()
        .map(|ancestor| json!({ "slug": ancestor.slug, "title": ancestor.title }))
        .collect();
    let parent = ancestors
        .last()
        .map(|parent| json!({ "id": parent.id, "slug": parent.slug, "title": parent.title }))
        .unwrap_or(Value::Null);
    let children_json: Vec<Value> = children
        .iter()
        .map(
            |child| json!({ "slug": child.slug, "title": child.title, "position": child.position }),
        )
        .collect();

    let page_json = json!({
        "id": page.id,
        "slug": page.slug,
        "title": page.title,
        "is_published": page.is_published,
        "created_at_h": format_timestamp(page.created_at),
        "updated_at_h": format_timestamp(page.updated_at),
        "author": author,
        "parent": parent,
        "breadcrumbs": breadcrumbs,
        "children": children_json,
        "position": page.position,
        "meta_title": page.meta_title,
        "meta_description": page.meta_description,
        "og_image": page.og_image,
    });

    let mut wrapper_data = data.clone();
    wrapper_data.insert(String::from("page"), page_json);
    wrapper_data.insert(String::from("content"), Value::String(body_html));

    PublicPageOutcome::Served(
        render_view_with_data(state, parts, wrapper_data, "cms_page.jhs", StatusCode::OK).await,
    )
}

/// `GET /p/{slug}`: the route half of [`render_public_page`].
async fn public_page(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let data = base_data(&state, &parts.headers, &parts.uri, &parts.method).await;

    match render_public_page(&state, &parts, &data, &slug).await {
        PublicPageOutcome::Served(response) => response,
        // A slug with no page behind it answers the HTML 404 view —
        // the homepage is the caller that falls back instead.
        PublicPageOutcome::Missing => {
            render_view(&state, &parts, "404.jhs", Vec::new(), StatusCode::NOT_FOUND).await
        }
    }
}

// ─── Admin: dashboard ────────────────────────────────────────────────

/// `GET /admin`: counters and quick links.
async fn dashboard(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let total = match cms.pages.count().await {
        Ok(total) => total,
        Err(error) => {
            tracing::error!(%error, "cms dashboard counters failed");
            return AppError::internal("storage failure").into_response();
        }
    };
    let published = match cms.pages.count_published().await {
        Ok(published) => published,
        Err(error) => {
            tracing::error!(%error, "cms dashboard counters failed");
            return AppError::internal("storage failure").into_response();
        }
    };
    let users = match state.auth_context() {
        Some(auth) => match auth.repository.count().await {
            Ok(users) => users,
            Err(error) => {
                tracing::error!(%error, "cms dashboard counters failed");
                return AppError::internal("storage failure").into_response();
            }
        },
        None => 0,
    };
    let menus = match cms.menus.count().await {
        Ok(menus) => menus,
        Err(error) => {
            tracing::error!(%error, "cms dashboard counters failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let stats = json!({
        "pages": total,
        "published": published,
        "drafts": total - published,
        "menus": menus,
        "users": users,
    });

    render_view(
        &state,
        &parts,
        "admin/dashboard.jhs",
        vec![("stats", stats)],
        StatusCode::OK,
    )
    .await
}

// ─── Admin: pages ────────────────────────────────────────────────────

/// The admin page form payload.
#[derive(Deserialize, Default)]
struct PageForm {
    slug: String,
    title: String,
    content: String,
    /// How the body is interpreted: `jhs` (the default — the field is
    /// absent in pre-F8 clients) or `markdown` (F8).
    body_format: Option<String>,
    /// HTML checkboxes post `on` when checked and nothing when not.
    is_published: Option<String>,
    /// Parent page id as posted by the select: empty string = top
    /// level, digits = the parent (F7).
    parent_id: Option<String>,
    /// Sibling ordering as posted: empty or absent → 0, anything else
    /// must parse inside `0..=MAX_POSITION` (F7).
    position: Option<String>,
    /// The page being edited, posted by the hidden input the
    /// previsualización round-trip carries (F8): empty/absent = a new
    /// page. It is **round-trip data, not a command** — the preview
    /// never writes, and saves go to the routes that own the id.
    page_id: Option<String>,
    /// SEO overrides (F7): empty fields clear the stored value.
    meta_title: Option<String>,
    meta_description: Option<String>,
    og_image: Option<String>,
}

impl PageForm {
    fn published(&self) -> bool {
        matches!(
            self.is_published.as_deref(),
            Some("on") | Some("true") | Some("1")
        )
    }

    /// The parsed body format, defaulting to `.jhs` (F8). The shape
    /// itself is validated by [`validate_page_form`]; this accessor is
    /// for re-rendering after validation accepted the field.
    fn body_format(&self) -> BodyFormat {
        self.body_format
            .as_deref()
            .and_then(BodyFormat::parse)
            .unwrap_or(BodyFormat::Jhs)
    }

    /// The preview's round-trip page id (`None` = new page, F8).
    fn page_id(&self) -> Option<i64> {
        trimmed(&self.page_id).and_then(|raw| raw.parse().ok())
    }

    /// The parsed parent id (`None` = top level). Only meaningful
    /// after [`validate_page_form`] accepted the raw shape (F7).
    fn parent(&self) -> Option<i64> {
        trimmed(&self.parent_id).and_then(|raw| raw.parse().ok())
    }

    /// The parsed sibling position, defaulting to 0 (F7).
    fn position(&self) -> i64 {
        parse_position(self.position.as_deref())
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    /// The form as template data (`id` and `is_new` added by callers).
    /// `parents` feeds the parent `<select>` (F7).
    fn form_data(&self, id: Option<i64>, parents: &[Value], error: Option<&str>) -> (Value, Value) {
        let form = json!({
            "id": id,
            "slug": self.slug,
            "title": self.title,
            "content": self.content,
            "body_format": self.body_format().as_str(),
            "is_published": self.published(),
            "is_new": id.is_none(),
            "parent_id": self.parent(),
            "position": self.position(),
            "meta_title": trimmed(&self.meta_title).unwrap_or_default(),
            "meta_description": trimmed(&self.meta_description).unwrap_or_default(),
            "og_image": trimmed(&self.og_image).unwrap_or_default(),
            "parents": parents,
        });
        (
            form,
            error
                .map(|message| Value::String(message.to_owned()))
                .unwrap_or(Value::Null),
        )
    }
}

/// Trims an optional form field to `None` when empty — the shared
/// "empty means clear" reading of every optional CMS input (F7).
fn trimmed(field: &Option<String>) -> Option<String> {
    field
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Parses a form position: absent/empty → `Ok(None)` (the caller's
/// default applies), a number in `0..=MAX_POSITION` → `Ok(Some(n))`,
/// anything else → the Spanish form error (F7).
fn parse_position(raw: Option<&str>) -> Result<Option<i64>, &'static str> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    match raw.parse::<i64>() {
        Ok(value) if (0..=MAX_POSITION).contains(&value) => Ok(Some(value)),
        _ => Err("La posición debe ser un número entre 0 y 99 999."),
    }
}

/// `GET /admin/pages`: every page, drafts included — as the tree, or
/// as a flat ranked result list while `?q=` filters (F10).
async fn list_pages(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    // F10: with a filter the tree flattens into ranked hits — the
    // editor's way of finding content (and references: a media URL
    // inside a body is just words to FTS5). Without it, the full
    // hierarchy tree as always.
    let (filter, _) = listing_query(&parts.uri);
    if !filter.is_empty() {
        let hits = match cms.pages.search(&filter, true, MAX_LISTED, 0).await {
            Ok(hits) => hits,
            Err(error) => {
                tracing::error!(%error, "page search failed");
                return AppError::internal("storage failure").into_response();
            }
        };
        let admin_resultados: Vec<Value> = hits
            .iter()
            .map(|hit| {
                json!({
                    "id": hit.id,
                    "slug": hit.slug,
                    "title": hit.title,
                    "is_published": hit.is_published,
                    "updated_at_h": format_timestamp(hit.updated_at),
                    "fragmento": fragment_segments(&hit.fragment),
                })
            })
            .collect();
        return render_view(
            &state,
            &parts,
            "admin/pages.jhs",
            vec![
                ("admin_pages", Value::Array(Vec::new())),
                ("admin_resultados", Value::Array(admin_resultados)),
                ("filtro", Value::String(filter)),
            ],
            StatusCode::OK,
        )
        .await;
    }

    let pages = match cms.pages.list(true, MAX_LISTED).await {
        Ok(pages) => pages,
        Err(error) => {
            tracing::error!(%error, "page listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    // F7: the flat listing becomes a tree — siblings ordered by
    // `position` then id, children nested under their parent with a
    // `depth` the view turns into indentation.
    let mut sorted = pages;
    sorted.sort_by_key(|page| (page.position, page.id));
    let mut by_parent: HashMap<Option<i64>, Vec<&PageSummary>> = HashMap::new();
    for page in &sorted {
        by_parent.entry(page.parent_id).or_default().push(page);
    }
    let mut admin_pages: Vec<Value> = Vec::with_capacity(sorted.len());
    walk_page_tree(&by_parent, None, 0, &mut admin_pages);

    render_view(
        &state,
        &parts,
        "admin/pages.jhs",
        vec![
            ("admin_pages", Value::Array(admin_pages)),
            ("admin_resultados", Value::Null),
            ("filtro", Value::String(String::new())),
        ],
        StatusCode::OK,
    )
    .await
}

/// Flattens the page tree depth-first into listing rows (F7).
fn walk_page_tree(
    by_parent: &HashMap<Option<i64>, Vec<&PageSummary>>,
    parent: Option<i64>,
    depth: usize,
    rows: &mut Vec<Value>,
) {
    let Some(siblings) = by_parent.get(&parent) else {
        return;
    };
    for page in siblings {
        rows.push(json!({
            "id": page.id,
            "slug": page.slug,
            "title": page.title,
            "is_published": page.is_published,
            "updated_at_h": format_timestamp(page.updated_at),
            "depth": depth,
        }));
        walk_page_tree(by_parent, Some(page.id), depth + 1, rows);
    }
}

/// `GET /admin/pages/new`: the empty creation form.
async fn new_page_form(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = PageForm::default();
    let parents = parent_options(cms, None).await;
    let (form, error) = form.form_data(None, &parents, None);
    render_view(
        &state,
        &parts,
        "admin/page_form.jhs",
        // `preview_html` rides along as null: the view always reads it.
        vec![
            ("form", form),
            ("form_error", error),
            ("preview_html", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/pages`: creates a page.
///
/// Validation failures re-render the form **with the submitted values**
/// (a redirect would lose the editor's draft), success follows the
/// PRG pattern with a `303` to the edit view.
async fn create_page(
    State(state): State<AppState>,
    editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<PageForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_form_with_error(&state, &parts, None, PageForm::default(), &message)
                .await;
        }
    };

    if let Err(error) = validate_page_form(&form) {
        return render_form_with_error(&state, &parts, None, form, error).await;
    }

    // F7: a stated parent must exist before the insert (the select
    // only offers real pages, but the form is just HTTP).
    let parent = form.parent();
    if let Some(parent_id) = parent {
        if load_page(cms, parent_id).await.is_none() {
            return render_form_with_error(
                &state,
                &parts,
                None,
                form,
                "La página padre no existe.",
            )
            .await;
        }
    }

    let new_page = NewPage {
        slug: form.slug.clone(),
        title: form.title.clone(),
        content: form.content.clone(),
        body_format: form.body_format(),
        is_published: form.published(),
        created_by: Some(editor.user.user_id),
        parent_id: parent,
        position: form.position(),
        meta_title: trimmed(&form.meta_title),
        meta_description: trimmed(&form.meta_description),
        og_image: trimmed(&form.og_image),
    };

    match cms.pages.create(&new_page).await {
        Ok(page) => see_other(&format!("/admin/pages/{}/edit?ok=creada", page.id)),
        Err(RepositoryError::Duplicate) => {
            render_form_with_error(
                &state,
                &parts,
                None,
                form,
                "Ese slug ya existe: elige otro identificador de URL.",
            )
            .await
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "page creation failed");
            render_form_with_error(
                &state,
                &parts,
                None,
                form,
                "No se pudo guardar (error interno).",
            )
            .await
        }
    }
}

/// `GET /admin/pages/{id}/edit`: the edit form.
async fn edit_page_form(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let Some(page) = load_page(cms, id).await else {
        return see_other("/admin/pages");
    };

    let form = PageForm {
        slug: page.slug,
        title: page.title,
        content: page.content,
        body_format: Some(page.body_format.as_str().to_owned()),
        is_published: page.is_published.then(|| "on".to_owned()),
        parent_id: page.parent_id.map(|parent| parent.to_string()),
        position: Some(page.position.to_string()),
        page_id: Some(id.to_string()),
        meta_title: page.meta_title,
        meta_description: page.meta_description,
        og_image: page.og_image,
    };
    // The page itself and its whole branch are unavailable as parents.
    let parents = parent_options(cms, Some(id)).await;
    let (form, error) = form.form_data(Some(id), &parents, None);
    render_view(
        &state,
        &parts,
        "admin/page_form.jhs",
        // `preview_html` rides along as null: the view always reads it.
        vec![
            ("form", form),
            ("form_error", error),
            ("preview_html", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/pages/{id}`: applies an edit.
async fn update_page(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<PageForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_form_with_error(&state, &parts, Some(id), PageForm::default(), &message)
                .await;
        }
    };

    if let Err(error) = validate_page_form(&form) {
        return render_form_with_error(&state, &parts, Some(id), form, error).await;
    }

    // F7: the move guard. `None` (top level) is always legal; a
    // stated parent must exist, differ from the page, and not sit
    // below it — otherwise the tree would grow a cycle.
    let parent = form.parent();
    if let Some(parent_id) = parent {
        if parent_id == id {
            return render_form_with_error(
                &state,
                &parts,
                Some(id),
                form,
                "Una página no puede ser su propia padre.",
            )
            .await;
        }
        if load_page(cms, parent_id).await.is_none() {
            return render_form_with_error(
                &state,
                &parts,
                Some(id),
                form,
                "La página padre no existe.",
            )
            .await;
        }
        let creates_cycle = match cms.pages.ancestors(parent_id).await {
            Ok(chain) => chain.iter().any(|ancestor| ancestor.id == id),
            Err(error) => {
                tracing::error!(%error, "page hierarchy walk failed");
                return AppError::internal("storage failure").into_response();
            }
        };
        if creates_cycle {
            return render_form_with_error(
                &state,
                &parts,
                Some(id),
                form,
                "Ese padre crearía un ciclo: la página no puede colgar de su propia \
                 descendiente.",
            )
            .await;
        }
    }

    let update = PageUpdate {
        slug: Some(form.slug.clone()),
        title: form.title.clone(),
        content: form.content.clone(),
        body_format: form.body_format(),
        is_published: form.published(),
        parent_id: parent,
        position: form.position(),
        meta_title: trimmed(&form.meta_title),
        meta_description: trimmed(&form.meta_description),
        og_image: trimmed(&form.og_image),
    };

    match cms.pages.update(id, &update).await {
        Ok(Some(_)) => see_other(&format!("/admin/pages/{id}/edit?ok=guardada")),
        Ok(None) => see_other("/admin/pages"),
        Err(RepositoryError::Duplicate) => {
            render_form_with_error(
                &state,
                &parts,
                Some(id),
                form,
                "Ese slug ya existe: elige otro identificador de URL.",
            )
            .await
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "page update failed");
            render_form_with_error(
                &state,
                &parts,
                Some(id),
                form,
                "No se pudo guardar (error interno).",
            )
            .await
        }
    }
}

/// `POST /admin/pages/preview`: the server-side previsualización (F8).
///
/// The form's second submit button (`formaction`, pure HTML — no
/// JavaScript, CSP untouched) posts the same fields here, the server
/// renders the body with the mode it states **without writing
/// anything**, and the response re-renders the same form — values
/// kept, so the editor keeps writing — with the rendered body above
/// it. A `.jhs` preview runs through the engine with the live globals,
/// exactly the render the public page would get, so template errors
/// bounce back as `form_error` instead of a published 500.
///
/// `page_id` is round-trip data only: the re-rendered form's save
/// action (new page vs. edit) — the preview itself never persists.
async fn preview_page(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<PageForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_form_with_error(&state, &parts, None, PageForm::default(), &message)
                .await;
        }
    };

    if let Err(error) = validate_page_form(&form) {
        return render_form_with_error(&state, &parts, form.page_id(), form, error).await;
    }

    let preview_html = match form.body_format() {
        BodyFormat::Markdown => {
            let content = form.content.clone();
            match tokio::task::spawn_blocking(move || crate::markdown::render(&content)).await {
                Ok(rendered) => Value::String(rendered),
                Err(join_error) => {
                    tracing::error!(%join_error, "preview rendering task failed");
                    return AppError::internal("preview rendering failed".to_owned())
                        .into_response();
                }
            }
        }
        BodyFormat::Jhs => {
            let Some(templates) = state.templates() else {
                return AppError::internal("templates are not initialized").into_response();
            };
            let engine = templates.engine();
            let content = form.content.clone();
            let globals = base_data(&state, &parts.headers, &parts.uri, &parts.method).await;
            match tokio::task::spawn_blocking(move || engine.render_string(&content, &globals))
                .await
            {
                Ok(Ok(output)) => Value::String(output.html),
                Ok(Err(error)) => {
                    // The editor sees template diagnostics inline —
                    // that is the whole point of previewing.
                    return render_form_with_error(
                        &state,
                        &parts,
                        form.page_id(),
                        form,
                        &error.to_string(),
                    )
                    .await;
                }
                Err(join_error) => {
                    tracing::error!(%join_error, "preview rendering task failed");
                    return AppError::internal("preview rendering failed".to_owned())
                        .into_response();
                }
            }
        }
    };

    // Same form, same parent options, preview above.
    let id = form.page_id();
    let parents = parent_options(cms, id).await;
    let (form_json, form_error) = form.form_data(id, &parents, None);
    render_view(
        &state,
        &parts,
        "admin/page_form.jhs",
        vec![
            ("form", form_json),
            ("form_error", form_error),
            ("preview_html", preview_html),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/pages/{id}/delete`: removes a page (idempotent).
async fn delete_page(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    match cms.pages.delete(id).await {
        Ok(_) => see_other("/admin/pages?ok=eliminada"),
        Err(error) => {
            tracing::error!(%error, "page deletion failed");
            AppError::internal("storage failure").into_response()
        }
    }
}

/// Loads a page by id, logging storage failures as a missing page.
async fn load_page(cms: &CmsContext, id: i64) -> Option<crate::db::PageRecord> {
    match cms.pages.find_by_id(id).await {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "page lookup failed");
            None
        }
    }
}

/// Validates the admin page form; the error is already user-facing
/// Spanish.
fn validate_page_form(form: &PageForm) -> Result<(), &'static str> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err("El título no puede estar vacío.");
    }
    if form.title.len() > MAX_TITLE {
        return Err("El título es demasiado largo (máximo 200 caracteres).");
    }
    validate_slug(&form.slug)?;
    if form.content.len() > MAX_PAGE_CONTENT {
        return Err("El contenido es demasiado largo (máximo 600 000 caracteres).");
    }

    // F8: the body format must be one of the two modes the form
    // offers; a malformed (hand-crafted) POST bounces back.
    if trimmed(&form.body_format).is_some_and(|raw| BodyFormat::parse(&raw).is_none()) {
        return Err("El formato del contenido debe ser jhs o markdown.");
    }

    // F7 additions: hierarchy and SEO shapes.
    parse_position(form.position.as_deref())?;
    if let Some(raw_parent) = trimmed(&form.parent_id) {
        if raw_parent.parse::<i64>().map(|id| id > 0).unwrap_or(false) {
            // A positive integer: fine — existence is checked by the
            // handler, which can talk to the repository.
        } else {
            return Err(
                "El padre debe ser el identificador de una página (o vacío para el nivel \
                 superior).",
            );
        }
    }
    if trimmed(&form.meta_title).is_some_and(|meta_title| meta_title.len() > MAX_TITLE) {
        return Err("El título para buscadores es demasiado largo (máximo 200 caracteres).");
    }
    if trimmed(&form.meta_description)
        .is_some_and(|description| description.len() > MAX_META_DESCRIPTION)
    {
        return Err("La descripción es demasiado larga (máximo 500 caracteres).");
    }
    if let Some(og_image) = trimmed(&form.og_image) {
        if og_image.len() > MAX_URL_FIELD {
            return Err("La imagen social es demasiado larga (máximo 500 caracteres).");
        }
        if !(og_image.starts_with('/')
            || og_image.starts_with("http://")
            || og_image.starts_with("https://"))
        {
            return Err("La imagen social debe ser una ruta («/assets/…») o una URL absoluta.");
        }
    }
    Ok(())
}

/// A slug is valid when it has 1-64 characters of `[a-z0-9-]`, does not
/// start or end with a dash, and contains no double dash — the shared
/// [`crate::util::valid_slug`] rule, wrapped in the form's message.
fn validate_slug(slug: &str) -> Result<(), &'static str> {
    if crate::util::valid_slug(slug) {
        Ok(())
    } else {
        Err(
            "El slug debe tener 1-64 caracteres: minúsculas, números y guiones \
             (sin empezar ni terminar en guion).",
        )
    }
}

/// Re-renders the page form with the submitted values and an error.
async fn render_form_with_error(
    state: &AppState,
    parts: &PageParts,
    id: Option<i64>,
    form: PageForm,
    error: &str,
) -> Response {
    let parents = match state.cms() {
        Some(cms) => parent_options(cms, id).await,
        None => Vec::new(),
    };
    let (form, error) = form.form_data(id, &parents, Some(error));
    render_view(
        state,
        parts,
        "admin/page_form.jhs",
        // `preview_html` rides along as null: the view always reads it.
        vec![
            ("form", form),
            ("form_error", error),
            ("preview_html", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

/// The parent `<select>` options (F7): every page except `exclude`
/// and its whole branch — a page can never be moved under its own
/// subtree. Indented with non-breaking spaces so the tree shape
/// reads inside the dropdown, and ordered exactly like the admin
/// tree (position, then id, depth-first).
async fn parent_options(cms: &CmsContext, exclude: Option<i64>) -> Vec<Value> {
    let mut pages = match cms.pages.list(true, MAX_LISTED).await {
        Ok(pages) => pages,
        Err(error) => {
            tracing::error!(%error, "page listing failed");
            return Vec::new();
        }
    };
    pages.sort_by_key(|page| (page.position, page.id));

    let mut by_parent: HashMap<Option<i64>, Vec<&PageSummary>> = HashMap::new();
    for page in &pages {
        by_parent.entry(page.parent_id).or_default().push(page);
    }
    let mut options = Vec::new();
    walk_parent_options(&by_parent, None, exclude, 0, &mut options);
    options
}

/// Depth-first walk behind [`parent_options`] (F7).
fn walk_parent_options(
    by_parent: &HashMap<Option<i64>, Vec<&PageSummary>>,
    parent: Option<i64>,
    exclude: Option<i64>,
    depth: usize,
    options: &mut Vec<Value>,
) {
    let Some(siblings) = by_parent.get(&parent) else {
        return;
    };
    for page in siblings {
        // The excluded page and its whole subtree are unavailable.
        if Some(page.id) == exclude {
            continue;
        }
        options.push(json!({
            "id": page.id,
            "label": format!("{}{}", "\u{00a0}".repeat(depth * 3), page.title),
        }));
        walk_parent_options(by_parent, Some(page.id), exclude, depth + 1, options);
    }
}

// ─── Admin: import from `public/` ────────────────────────────────────

/// `GET /admin/pages/import`: lists the importable files under the
/// static root (read-only: the CMS never writes to `public/`).
async fn import_form(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let files = list_importable_files(&state);

    render_view(
        &state,
        &parts,
        "admin/import.jhs",
        vec![("files", Value::Array(files)), ("form_error", Value::Null)],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/pages/import`: copies one file from `public/` into a
/// new draft page. The slug is derived from the file name.
async fn import_page(
    State(state): State<AppState>,
    editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<ImportForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_import_error(&state, &parts, &message).await;
        }
    };

    // Resolve the file strictly inside the static root.
    if !state.config().static_files.enabled {
        return render_import_error(
            &state,
            &parts,
            "La importación requiere el servidor estático activo.",
        )
        .await;
    }
    let root = state.config().static_files.root_dir.clone();

    let root_path = std::path::Path::new(&root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&root));
    let candidate = root_path.join(&form.file);
    let canonical = candidate.canonicalize().ok();
    let inside_root = canonical
        .as_ref()
        .is_some_and(|path| path.starts_with(&root_path));
    let extension_ok = form.file.ends_with(".html") || form.file.ends_with(".jhs");

    let Some(path) = canonical.filter(|_| inside_root && extension_ok) else {
        return render_import_error(
            &state,
            &parts,
            "Ese archivo no existe o no es importable (solo .html/.jhs dentro de public/).",
        )
        .await;
    };

    let Ok(metadata) = std::fs::metadata(&path) else {
        return render_import_error(&state, &parts, "No se pudo leer el archivo.").await;
    };
    if metadata.len() > MAX_IMPORT_BYTES {
        return render_import_error(
            &state,
            &parts,
            "El archivo supera el límite de importación (600 KB).",
        )
        .await;
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        return render_import_error(&state, &parts, "No se pudo leer el archivo.").await;
    };

    let file_name = form.file.rsplit('/').next().unwrap_or("importada");
    let slug = slug_from_file_name(file_name);

    let title = file_name.to_owned();
    let new_page = NewPage {
        slug: slug.clone(),
        title,
        content,
        // Imported .html/.jhs files are template-shaped by definition.
        body_format: BodyFormat::Jhs,
        is_published: false,
        created_by: Some(editor.user.user_id),
        // Imported files land as plain top-level drafts: the editor
        // sets hierarchy and SEO in the follow-up edit (F7).
        parent_id: None,
        position: 0,
        meta_title: None,
        meta_description: None,
        og_image: None,
    };

    match cms.pages.create(&new_page).await {
        Ok(page) => see_other(&format!("/admin/pages/{}/edit?ok=importada", page.id)),
        Err(RepositoryError::Duplicate) => {
            render_import_error(
                &state,
                &parts,
                &format!("El slug `{slug}` ya existe; renombra la página antes de importar."),
            )
            .await
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "import failed");
            render_import_error(&state, &parts, "No se pudo importar (error interno).").await
        }
    }
}

/// The import form payload.
#[derive(Deserialize)]
struct ImportForm {
    file: String,
}

/// Re-renders the import view with an error message.
async fn render_import_error(state: &AppState, parts: &PageParts, error: &str) -> Response {
    let files = list_importable_files(state);
    render_view(
        state,
        parts,
        "admin/import.jhs",
        vec![
            ("files", Value::Array(files)),
            ("form_error", Value::String(error.to_owned())),
        ],
        StatusCode::OK,
    )
    .await
}

/// Walks the static root collecting importable `.html`/`.jhs` files
/// (bounded in count, size and depth; dot entries skipped).
fn list_importable_files(state: &AppState) -> Vec<Value> {
    let static_config = &state.config().static_files;
    if !static_config.enabled {
        return Vec::new();
    }

    let root = std::path::Path::new(&static_config.root_dir).to_path_buf();
    let mut files = Vec::new();
    collect_importable(&root, &root, 0, &mut files);
    files
}

/// Recursive collector for [`list_importable_files`].
fn collect_importable(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    files: &mut Vec<Value>,
) {
    if depth > MAX_IMPORT_DEPTH || files.len() >= MAX_IMPORT_LISTED {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_importable(root, &path, depth + 1, files);
        } else if name.ends_with(".html") || name.ends_with(".jhs") {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size > MAX_IMPORT_BYTES {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| name.to_string());
            files.push(json!({
                "path": relative,
                "bytes": size,
                "kind": if name.ends_with(".jhs") { "jhs" } else { "html" },
            }));
            if files.len() >= MAX_IMPORT_LISTED {
                return;
            }
        }
    }
}

/// Derives a slug from a file name (extension stripped): lowercase,
/// accents transliterated, `[a-z0-9]`, single dashes, bounded.
fn slug_from_file_name(name: &str) -> String {
    let base = name
        .strip_suffix(".html")
        .or_else(|| name.strip_suffix(".jhs"))
        .unwrap_or(name);

    // Spanish accents transliterate instead of becoming dashes — page
    // titles are Spanish far more often than not.
    let transliterated: String = base
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' => 'a',
            'é' | 'è' | 'ë' => 'e',
            'í' | 'ì' | 'ï' => 'i',
            'ó' | 'ò' | 'ö' => 'o',
            'ú' | 'ù' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            other => other,
        })
        .collect();
    let lower = transliterated.to_ascii_lowercase();
    let mut slug = String::with_capacity(lower.len());
    let mut dash = false;
    for character in lower.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
            dash = false;
        } else if !dash {
            slug.push('-');
            dash = true;
        }
    }
    while slug.starts_with('-') {
        slug.remove(0);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.truncate(64);
    if slug.is_empty() {
        slug.push_str("importada");
    }
    slug
}

// ─── Admin: menus (F7) ──────────────────────────────────────────────

/// The menu creation form payload.
#[derive(Deserialize, Default)]
struct MenuForm {
    name: String,
    title: String,
}

/// The menu title rename payload.
#[derive(Deserialize, Default)]
struct MenuTitleForm {
    title: String,
}

/// The menu item form payload: a page link or a custom URL — exactly
/// one of the two, plus an optional label override and position.
#[derive(Deserialize, Default)]
struct ItemForm {
    label: Option<String>,
    page_id: Option<String>,
    url: Option<String>,
    position: Option<String>,
}

impl ItemForm {
    /// The parsed page id (`None` = no page chosen). Only meaningful
    /// after [`validate_item_form`] accepted the shape.
    fn page(&self) -> Option<i64> {
        trimmed(&self.page_id).and_then(|raw| raw.parse().ok())
    }

    /// The parsed custom URL (`None` = none chosen).
    fn url(&self) -> Option<String> {
        trimmed(&self.url)
    }

    /// The parsed label override.
    fn label(&self) -> Option<String> {
        trimmed(&self.label)
    }

    /// The parsed position, defaulting to 0.
    fn position(&self) -> i64 {
        parse_position(self.position.as_deref())
            .ok()
            .flatten()
            .unwrap_or(0)
    }
}

/// `GET /admin/menus`: the menus plus the creation form.
async fn list_menus(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    render_menus_view(&state, &PageParts::of(&request), None).await
}

/// Renders the admin menus listing (with an optional form error).
async fn render_menus_view(state: &AppState, parts: &PageParts, error: Option<&str>) -> Response {
    let Ok(cms) = cms_context(state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let menus = match cms.menus.list().await {
        Ok(menus) => menus,
        Err(error) => {
            tracing::error!(%error, "menu listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let mut admin_menus = Vec::with_capacity(menus.len());
    for menu in menus {
        let items = cms.menus.count_items(menu.id).await.unwrap_or(0);
        admin_menus.push(json!({
            "id": menu.id,
            "name": menu.name,
            "title": menu.title,
            "items": items,
            "updated_at_h": format_timestamp(menu.updated_at),
        }));
    }

    render_view(
        state,
        parts,
        "admin/menus.jhs",
        vec![
            ("admin_menus", Value::Array(admin_menus)),
            (
                "form_error",
                error
                    .map(|message| Value::String(message.to_owned()))
                    .unwrap_or(Value::Null),
            ),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/menus`: creates a menu.
async fn create_menu(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<MenuForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_menus_view(&state, &parts, Some(&message)).await;
        }
    };

    let title = form.title.trim();
    if title.is_empty() {
        return render_menus_view(
            &state,
            &parts,
            Some("El título del menú no puede estar vacío."),
        )
        .await;
    }
    if form.title.len() > MAX_TITLE {
        return render_menus_view(
            &state,
            &parts,
            Some("El título del menú es demasiado largo (máximo 200 caracteres)."),
        )
        .await;
    }
    if !crate::util::valid_slug(&form.name) {
        return render_menus_view(
            &state,
            &parts,
            Some(
                "El nombre del menú debe tener 1-64 caracteres: minúsculas, números y guiones \
                 (es la clave que leen las plantillas: menus.<nombre>).",
            ),
        )
        .await;
    }

    let new_menu = NewMenu {
        name: form.name,
        title: form.title.trim().to_owned(),
    };

    match cms.menus.create(&new_menu).await {
        Ok(menu) => see_other(&format!("/admin/menus/{}?ok=creado", menu.id)),
        Err(RepositoryError::Duplicate) => {
            render_menus_view(
                &state,
                &parts,
                Some(
                    "Ese nombre de menú ya existe: es la clave que leen las plantillas \
                      (menus.<nombre>).",
                ),
            )
            .await
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "menu creation failed");
            render_menus_view(&state, &parts, Some("No se pudo guardar (error interno).")).await
        }
    }
}

/// `GET /admin/menus/{id}`: the menu detail — rename form, items, and
/// the add-item form.
async fn menu_detail(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    render_menu_detail(&state, &PageParts::of(&request), id, None).await
}

/// Renders the menu detail view (with an optional form error).
async fn render_menu_detail(
    state: &AppState,
    parts: &PageParts,
    id: i64,
    error: Option<&str>,
) -> Response {
    let Ok(cms) = cms_context(state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let Some(menu) = load_menu(cms, id).await else {
        return see_other("/admin/menus");
    };

    let items = match cms.menus.items_with_pages(id).await {
        Ok(items) => items,
        Err(error) => {
            tracing::error!(%error, "menu item listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let items_json: Vec<Value> = items
        .iter()
        .map(|entry| {
            let page = entry
                .page
                .as_ref()
                .map(|page| {
                    json!({
                        "slug": page.slug,
                        "title": page.title,
                        "is_published": page.is_published,
                    })
                })
                .unwrap_or(Value::Null);
            json!({
                "id": entry.item.id,
                "position": entry.item.position,
                "label": entry.item.label,
                "page": page,
                "url": entry.item.url,
                "is_page": entry.item.page_id.is_some(),
                "href": match (&entry.page, &entry.item.url) {
                    (Some(page), _) => format!("/p/{}", page.slug),
                    (None, Some(url)) => url.clone(),
                    (None, None) => String::new(),
                },
            })
        })
        .collect();

    let menu_json = json!({
        "id": menu.id,
        "name": menu.name,
        "title": menu.title,
    });

    render_view(
        state,
        parts,
        "admin/menu_detail.jhs",
        vec![
            ("menu", menu_json),
            ("items", Value::Array(items_json)),
            (
                "form_error",
                error
                    .map(|message| Value::String(message.to_owned()))
                    .unwrap_or(Value::Null),
            ),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/menus/{id}`: renames the menu's title (the name is
/// the template key and stays immutable).
async fn rename_menu(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<MenuTitleForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_menu_detail(&state, &parts, id, Some(&message)).await;
        }
    };

    let title = form.title.trim();
    if title.is_empty() {
        return render_menu_detail(
            &state,
            &parts,
            id,
            Some("El título del menú no puede estar vacío."),
        )
        .await;
    }
    if form.title.len() > MAX_TITLE {
        return render_menu_detail(
            &state,
            &parts,
            id,
            Some("El título del menú es demasiado largo (máximo 200 caracteres)."),
        )
        .await;
    }

    match cms.menus.update_title(id, title).await {
        Ok(Some(_)) => see_other(&format!("/admin/menus/{id}?ok=guardado")),
        Ok(None) => see_other("/admin/menus"),
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "menu rename failed");
            render_menu_detail(
                &state,
                &parts,
                id,
                Some("No se pudo guardar (error interno)."),
            )
            .await
        }
        Err(RepositoryError::Duplicate) => {
            render_menu_detail(
                &state,
                &parts,
                id,
                Some("No se pudo guardar (error interno)."),
            )
            .await
        }
    }
}

/// `POST /admin/menus/{id}/delete`: removes a menu and its items.
async fn delete_menu(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    match cms.menus.delete(id).await {
        Ok(_) => see_other("/admin/menus?ok=eliminado"),
        Err(error) => {
            tracing::error!(%error, "menu deletion failed");
            AppError::internal("storage failure").into_response()
        }
    }
}

/// `GET /admin/menus/{id}/items/new`: the add-item form.
async fn new_item_form(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    render_item_form_view(
        &state,
        &PageParts::of(&request),
        id,
        &ItemForm::default(),
        None,
        None,
    )
    .await
}

/// `POST /admin/menus/{id}/items`: adds an item to the menu.
async fn create_item(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<ItemForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_item_form_view(
                &state,
                &parts,
                id,
                &ItemForm::default(),
                None,
                Some(&message),
            )
            .await;
        }
    };

    if let Err(error) = validate_item_form(cms, &form).await {
        return render_item_form_view(&state, &parts, id, &form, None, Some(error)).await;
    }

    let new_item = NewMenuItem {
        menu_id: id,
        position: form.position(),
        label: form.label(),
        page_id: form.page(),
        url: form.url(),
    };

    match cms.menus.add_item(&new_item).await {
        Ok(_) => see_other(&format!("/admin/menus/{id}?ok=item-creado")),
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "menu item creation failed");
            render_item_form_view(
                &state,
                &parts,
                id,
                &form,
                None,
                Some("No se pudo guardar (error interno)."),
            )
            .await
        }
        Err(RepositoryError::Duplicate) => {
            render_item_form_view(
                &state,
                &parts,
                id,
                &form,
                None,
                Some("No se pudo guardar (error interno)."),
            )
            .await
        }
    }
}

/// `GET /admin/menus/{id}/items/{item_id}/edit`: the edit-item form.
async fn edit_item_form(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path((id, item_id)): Path<(i64, i64)>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let Some(item) = load_menu_item(cms, id, item_id).await else {
        return see_other(&format!("/admin/menus/{id}"));
    };

    let form = ItemForm {
        label: item.label,
        page_id: item.page_id.map(|page| page.to_string()),
        url: item.url,
        position: Some(item.position.to_string()),
    };

    render_item_form_view(&state, &parts, id, &form, Some(item_id), None).await
}

/// `POST /admin/menus/{id}/items/{item_id}`: applies an item edit.
async fn update_item(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path((id, item_id)): Path<(i64, i64)>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let form = match read_form::<ItemForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_item_form_view(
                &state,
                &parts,
                id,
                &ItemForm::default(),
                Some(item_id),
                Some(&message),
            )
            .await;
        }
    };

    if cms.menus.find_item(item_id).await.ok().flatten().is_none() {
        return see_other(&format!("/admin/menus/{id}"));
    }
    if let Err(error) = validate_item_form(cms, &form).await {
        return render_item_form_view(&state, &parts, id, &form, Some(item_id), Some(error)).await;
    }

    let new_item = NewMenuItem {
        menu_id: id,
        position: form.position(),
        label: form.label(),
        page_id: form.page(),
        url: form.url(),
    };

    match cms.menus.update_item(item_id, &new_item).await {
        Ok(Some(_)) => see_other(&format!("/admin/menus/{id}?ok=item-guardado")),
        Ok(None) => see_other(&format!("/admin/menus/{id}")),
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "menu item update failed");
            render_item_form_view(
                &state,
                &parts,
                id,
                &form,
                Some(item_id),
                Some("No se pudo guardar (error interno)."),
            )
            .await
        }
        Err(RepositoryError::Duplicate) => {
            render_item_form_view(
                &state,
                &parts,
                id,
                &form,
                Some(item_id),
                Some("No se pudo guardar (error interno)."),
            )
            .await
        }
    }
}

/// `POST /admin/menus/{id}/items/{item_id}/delete`: removes one item.
async fn delete_item(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path((id, item_id)): Path<(i64, i64)>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    // Only items that actually belong to this menu are touchable
    // through its URLs.
    if load_menu_item(cms, id, item_id).await.is_some() {
        if let Err(error) = cms.menus.delete_item(item_id).await {
            tracing::error!(%error, "menu item deletion failed");
            return AppError::internal("storage failure").into_response();
        }
    }

    see_other(&format!("/admin/menus/{id}?ok=item-eliminado"))
}

/// Renders the menu item form (create or edit) with the submitted
/// values and an optional error.
async fn render_item_form_view(
    state: &AppState,
    parts: &PageParts,
    menu_id: i64,
    form: &ItemForm,
    item_id: Option<i64>,
    error: Option<&str>,
) -> Response {
    let Ok(cms) = cms_context(state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let Some(menu) = load_menu(cms, menu_id).await else {
        return see_other("/admin/menus");
    };

    // Drafts are included on purpose: a menu may link a draft (the
    // public `menus` global skips it until the page is published).
    let page_options: Vec<Value> = match cms.pages.list(true, MAX_LISTED).await {
        Ok(pages) => pages
            .iter()
            .map(|page| {
                json!({
                    "id": page.id,
                    "title": page.title,
                    "is_published": page.is_published,
                })
            })
            .collect(),
        Err(error) => {
            tracing::error!(%error, "page listing failed");
            Vec::new()
        }
    };

    let form_json = json!({
        "menu_id": menu_id,
        "item_id": item_id,
        "is_new": item_id.is_none(),
        "label": form.label.as_deref().unwrap_or(""),
        "page_id": form.page(),
        "url": form.url.as_deref().unwrap_or(""),
        "position": form.position(),
    });
    let menu_json = json!({
        "id": menu.id,
        "name": menu.name,
        "title": menu.title,
    });

    render_view(
        state,
        parts,
        "admin/menu_item_form.jhs",
        vec![
            ("menu", menu_json),
            ("form", form_json),
            ("page_options", Value::Array(page_options)),
            (
                "form_error",
                error
                    .map(|message| Value::String(message.to_owned()))
                    .unwrap_or(Value::Null),
            ),
        ],
        StatusCode::OK,
    )
    .await
}

/// Validates the item form: exactly one destination (page or URL), a
/// mandatory label for custom links, and the shared shape rules.
async fn validate_item_form(cms: &CmsContext, form: &ItemForm) -> Result<(), &'static str> {
    parse_position(form.position.as_deref())?;
    let page = form.page();
    let url = form.url();
    let label = form.label();

    if label
        .as_ref()
        .is_some_and(|label| label.len() > MAX_MENU_LABEL)
    {
        return Err("La etiqueta es demasiado larga (máximo 200 caracteres).");
    }

    match (page, url) {
        (Some(_), Some(_)) => Err("Elige una página o escribe una URL — no ambas."),
        (None, None) => Err("El elemento necesita un destino: una página o una URL."),
        (Some(page_id), None) => {
            if load_page(cms, page_id).await.is_none() {
                return Err("La página elegida no existe.");
            }
            Ok(())
        }
        (None, Some(url)) => {
            if url.len() > MAX_URL_FIELD {
                return Err("La URL es demasiado larga (máximo 500 caracteres).");
            }
            if !(url.starts_with('/')
                || url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with('#'))
            {
                return Err("La URL debe empezar por «/», «http://», «https://» o «#».");
            }
            if label.is_none() {
                return Err("Los enlaces personalizados necesitan una etiqueta.");
            }
            Ok(())
        }
    }
}

/// Loads a menu by id, logging storage failures as a missing menu.
async fn load_menu(cms: &CmsContext, id: i64) -> Option<crate::db::MenuRecord> {
    match cms.menus.find_by_id(id).await {
        Ok(menu) => menu,
        Err(error) => {
            tracing::error!(%error, "menu lookup failed");
            None
        }
    }
}

/// Loads a menu item that must belong to `menu_id` — foreign or
/// missing items answer `None` (the URL named this menu).
async fn load_menu_item(
    cms: &CmsContext,
    menu_id: i64,
    item_id: i64,
) -> Option<crate::db::MenuItemRecord> {
    match cms.menus.find_item(item_id).await {
        Ok(Some(item)) if item.menu_id == menu_id => Some(item),
        Ok(_) => None,
        Err(error) => {
            tracing::error!(%error, "menu item lookup failed");
            None
        }
    }
}

// ─── Public: `GET /p` and the listings' shared scaffolding (F10) ────

/// The `q` and `page` query parameters of the listing routes (F10):
/// `q` arrives raw but trimmed and capped (the search box caps at the
/// browser too — this is the server-side backstop), and `page` is
/// forgiving: anything that is not an integer falls back to 1, and the
/// callers clamp the range.
pub(crate) fn listing_query(uri: &Uri) -> (String, i64) {
    let mut query = String::new();
    let mut page: i64 = 1;
    if let Some(pairs) = uri.query() {
        for (key, value) in url::form_urlencoded::parse(pairs.as_bytes()) {
            match key.as_ref() {
                "q" => query = value.as_ref().trim().chars().take(200).collect(),
                "page" => {
                    if let Ok(parsed) = value.parse::<i64>() {
                        page = parsed;
                    }
                }
                _ => {}
            }
        }
    }
    (query, page)
}

/// Ceiling page count without `div_ceil` (the signed flavor is still
/// unstable): exact for any `total >= 0` with `page_size >= 1`, and
/// the callers add the `.max(1)` edge for empty listings.
pub(crate) fn pages_for(total: i64, page_size: i64) -> i64 {
    (total / page_size) + i64::from(total % page_size != 0)
}

/// Clamps a requested page number into `1..=total_pages` (an empty
/// listing keeps page 1). Out-of-range numbers are not errors: the
/// visitor just lands on the nearest real page.
pub(crate) fn clamp_page(requested: i64, total_pages: i64) -> i64 {
    requested.clamp(1, total_pages.max(1))
}

/// `?page=N` link over a base that may already carry its own query
/// (`/buscar?q=hola`): the separator picks itself.
fn page_link(base: &str, page: i64) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}page={page}")
}

/// The `paginacion` global every paginated view renders (F10): the
/// current page, the totals, and ready-made prev/next links (`null` at
/// the edges) over `base`. One shape, three listings — `/p`, `/buscar`
/// and the admin media grid all include the same partial.
pub(crate) fn pagination_value(page: i64, total_items: i64, page_size: i64, base: &str) -> Value {
    let total_pages = pages_for(total_items, page_size).max(1);
    // `Option<String>` serializes to `null` at the edges — the view
    // simply checks truthiness.
    let anterior = (page > 1).then(|| page_link(base, page - 1));
    let siguiente = (page < total_pages).then(|| page_link(base, page + 1));
    json!({
        "pagina": page,
        "total_paginas": total_pages,
        "total_items": total_items,
        "anterior": anterior,
        "siguiente": siguiente,
        "base": base,
    })
}

/// `GET /p` — the published-pages index, one page at a time (F10).
///
/// Pre-F10 `/p` was auto-routed to `views/p.jhs` rendering the `pages`
/// global; the explicit route keeps the very same view but feeds it
/// one window of the listing plus `paginacion`. The `pages` global
/// stays in [`crate::middleware::templates::base_data`] for the other
/// templates, and the view falls back to it while the CMS is off (the
/// auto-route then serves `/p` again with an empty global).
async fn page_index(State(state): State<AppState>, request: Request) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };
    let page_size = i64::from(state.config().cms.index_page_size);

    let total = match cms.pages.count_published().await {
        Ok(total) => total,
        Err(error) => {
            tracing::error!(%error, "page count failed");
            return AppError::internal("storage failure").into_response();
        }
    };
    let (_, requested) = listing_query(&parts.uri);
    let page = clamp_page(requested, pages_for(total, page_size).max(1));

    let pages = match cms
        .pages
        .list_paged(false, page_size, (page - 1) * page_size)
        .await
    {
        Ok(pages) => pages,
        Err(error) => {
            tracing::error!(%error, "page listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let paginas: Vec<Value> = pages
        .iter()
        .map(|pagina| {
            json!({
                "slug": pagina.slug,
                "title": pagina.title,
                "updated_at_h": format_timestamp(pagina.updated_at),
            })
        })
        .collect();

    render_view(
        &state,
        &parts,
        "p.jhs",
        vec![
            ("paginas", Value::Array(paginas)),
            ("paginacion", pagination_value(page, total, page_size, "/p")),
        ],
        StatusCode::OK,
    )
    .await
}

// ─── Public: `GET /feed.xml` and `GET /atom.xml` (F10) ──────────────

/// The absolute origin the sitemap and the feeds (F10) build their
/// URLs from: `[cms] site_url` when configured (validated absolute
/// http(s) at load), the request's `Host` header with `http://`
/// otherwise — correct for plain-HTTP setups, wrong behind TLS or a
/// reverse proxy (the docs say so on the key).
fn absolute_origin(state: &AppState, request: &Request) -> String {
    state.config().cms.site_url.clone().unwrap_or_else(|| {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("127.0.0.1");
        format!("http://{host}")
    })
}

/// `GET /feed.xml`: the published pages as RSS 2.0 (F10), newest
/// first, capped at [`FEED_MAX_ITEMS`]. Item descriptions come from
/// each page's SEO `meta_description` — set when absent is the
/// editor's call, the element is simply omitted.
async fn rss_feed(State(state): State<AppState>, request: Request) -> Response {
    if !state.config().cms.feed {
        return AppError::not_found("GET", "/feed.xml").into_response();
    }
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };
    let entries = match cms.pages.feed_entries(FEED_MAX_ITEMS).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::error!(%error, "feed page listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let base = absolute_origin(&state, &request);
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\">\n  <channel>\n",
    );
    xml.push_str(&format!(
        "    <title>{}</title>\n    <link>{}/p</link>\n    \
         <description>Páginas publicadas del CMS de wallermax</description>\n    \
         <atom:link rel=\"self\" href=\"{}/feed.xml\" \
         type=\"application/rss+xml\"/>\n    <language>es</language>\n",
        xml_escape("Páginas — wallermax"),
        xml_escape(&base),
        xml_escape(&base)
    ));
    for entry in entries {
        let link = format!("{base}/p/{}", entry.slug);
        xml.push_str(&format!(
            "    <item>\n      <title>{}</title>\n      <link>{}</link>\n      \
             <guid isPermaLink=\"true\">{}</guid>\n      <pubDate>{}</pubDate>\n",
            xml_escape(&entry.title),
            xml_escape(&link),
            xml_escape(&link),
            rfc2822_date(entry.updated_at)
        ));
        if let Some(description) = entry
            .meta_description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            xml.push_str(&format!(
                "      <description>{}</description>\n",
                xml_escape(description)
            ));
        }
        xml.push_str("    </item>\n");
    }
    xml.push_str("  </channel>\n</rss>\n");

    xml_response(xml, "application/rss+xml; charset=utf-8")
}

/// `GET /atom.xml`: the same entries as Atom 1.0 (F10) — RFC 3339
/// timestamps, one `<entry>` per published page, `<summary>` only for
/// pages with an SEO description.
async fn atom_feed(State(state): State<AppState>, request: Request) -> Response {
    if !state.config().cms.feed {
        return AppError::not_found("GET", "/atom.xml").into_response();
    }
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };
    let entries = match cms.pages.feed_entries(FEED_MAX_ITEMS).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::error!(%error, "feed page listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let base = absolute_origin(&state, &request);
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <feed xmlns=\"http://www.w3.org/2005/Atom\">\n",
    );
    xml.push_str(&format!(
        "  <title>{}</title>\n  <id>{}/atom.xml</id>\n  \
         <link rel=\"alternate\" type=\"text/html\" href=\"{}/p\"/>\n  \
         <link rel=\"self\" href=\"{}/atom.xml\"/>\n  <updated>{}</updated>\n",
        xml_escape("Páginas — wallermax"),
        xml_escape(&base),
        xml_escape(&base),
        xml_escape(&base),
        iso_datetime(
            entries
                .first()
                .map_or_else(unix_now, |entry| entry.updated_at)
        )
    ));
    for entry in entries {
        let link = format!("{base}/p/{}", entry.slug);
        xml.push_str(&format!(
            "  <entry>\n    <title>{}</title>\n    <link rel=\"alternate\" \
             href=\"{}\"/>\n    <id>{}</id>\n    <updated>{}</updated>\n",
            xml_escape(&entry.title),
            xml_escape(&link),
            xml_escape(&link),
            iso_datetime(entry.updated_at)
        ));
        if let Some(summary) = entry
            .meta_description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            xml.push_str(&format!("    <summary>{}</summary>\n", xml_escape(summary)));
        }
        xml.push_str("  </entry>\n");
    }
    xml.push_str("</feed>\n");

    xml_response(xml, "application/atom+xml; charset=utf-8")
}

/// A finished feed document: its own content type, the same public
/// one-hour cache the sitemap gets.
fn xml_response(xml: String, content_type: &str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(axum::body::Body::from(xml))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "failed to build the feed response");
            AppError::internal("failed to build the feed response".to_owned()).into_response()
        })
}

// ─── Public: `GET /sitemap.xml` (F7) ────────────────────────────────

/// `GET /sitemap.xml`: the published CMS pages (and the canonical
/// homepage while `default_page` names a published page) as a
/// sitemap.
///
/// `<loc>` URLs must be absolute: `[cms] site_url` wins, and without
/// it the request's `Host` header carries the visible origin under
/// plain `http://` — correct for direct HTTP setups, wrong behind
/// TLS or a proxy (the docs say so). The response is cacheable: it
/// is public information, unlike every template render.
async fn sitemap(State(state): State<AppState>, request: Request) -> Response {
    if !state.config().cms.sitemap {
        return AppError::not_found("GET", "/sitemap.xml").into_response();
    }
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let pages = match cms.pages.list(false, SITEMAP_LIMIT).await {
        Ok(pages) => pages,
        Err(error) => {
            tracing::error!(%error, "sitemap page listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let base = absolute_origin(&state, &request);

    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");

    // The canonical homepage: only while the configured default page
    // is a published one (otherwise GET / is not CMS content).
    if let Some(slug) = state.config().cms.default_page.as_deref() {
        if let Some(home) = cms.pages.find_by_slug(slug).await.unwrap_or(None) {
            if home.is_published {
                xml.push_str(&sitemap_entry(&format!("{base}/"), home.updated_at));
            }
        }
    }
    for page in pages {
        xml.push_str(&sitemap_entry(
            &format!("{base}/p/{}", page.slug),
            page.updated_at,
        ));
    }
    xml.push_str("</urlset>\n");

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(axum::body::Body::from(xml))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "failed to build the sitemap response");
            AppError::internal("failed to build the sitemap response".to_owned()).into_response()
        })
}

/// One `<url>` entry with XML-escaped values and a W3C date.
fn sitemap_entry(loc: &str, lastmod: i64) -> String {
    format!(
        "  <url>\n    <loc>{}</loc>\n    <lastmod>{}</lastmod>\n  </url>\n",
        xml_escape(loc),
        iso_date(lastmod)
    )
}

/// Escapes the five characters XML requires in text and attributes.
fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

// ─── Admin: users ────────────────────────────────────────────────────

/// The admin user form payload (role always; password only for resets).
#[derive(Deserialize, Default)]
struct UserForm {
    #[serde(default)]
    role: String,
    #[serde(default)]
    password: String,
}

/// `GET /admin/users`: the account listing (admin only).
async fn list_users(State(state): State<AppState>, _admin: CmsAdmin, request: Request) -> Response {
    let parts = PageParts::of(&request);
    let Some(auth) = state.auth_context() else {
        return AppError::internal("authentication is not initialized").into_response();
    };

    let users = match auth.repository.list(MAX_LISTED).await {
        Ok(users) => users,
        Err(error) => {
            tracing::error!(%error, "user listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let admin_users: Vec<Value> = users
        .iter()
        .map(|user| {
            json!({
                "id": user.id,
                "username": user.username,
                "role": user.role.as_str(),
                "created_at_h": format_timestamp(user.created_at),
                "last_login_at_h": user.last_login_at.map(format_timestamp),
            })
        })
        .collect();

    render_view(
        &state,
        &parts,
        "admin/users.jhs",
        vec![
            ("admin_users", Value::Array(admin_users)),
            ("form_error", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

/// `GET /admin/users/new`: the account creation form.
async fn new_user_form(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    render_view(
        &state,
        &parts,
        "admin/user_form.jhs",
        vec![
            ("form", user_form_data(None, "", "user")),
            ("form_error", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/users`: creates an account with an explicit role.
async fn create_user(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Some(auth) = state.auth_context() else {
        return AppError::internal("authentication is not initialized").into_response();
    };

    let form = match read_form::<NewUserForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_user_error(&state, &parts, None, "", "user", &message).await;
        }
    };

    if let Err(error) = validate_username(&form.username) {
        return render_user_error(&state, &parts, None, &form.username, &form.role, &error).await;
    }
    if let Err(error) = validate_password(&form.password, auth.min_password_len) {
        return render_user_error(&state, &parts, None, &form.username, &form.role, &error).await;
    }
    let Some(role) = UserRole::parse(&form.role) else {
        return render_user_error(
            &state,
            &parts,
            None,
            &form.username,
            "user",
            "Rol desconocido.",
        )
        .await;
    };

    let password_hash = match hash_password(&form.password) {
        Ok(hash) => hash,
        Err(error) => {
            tracing::error!(%error, "password hashing failed");
            return render_user_error(
                &state,
                &parts,
                None,
                &form.username,
                &form.role,
                "No se pudo crear (error interno).",
            )
            .await;
        }
    };

    match auth
        .repository
        .create(&form.username, &password_hash, role)
        .await
    {
        Ok(_) => see_other("/admin/users?ok=creado"),
        Err(RepositoryError::Duplicate) => {
            render_user_error(
                &state,
                &parts,
                None,
                &form.username,
                &form.role,
                "Ese nombre de usuario ya está en uso.",
            )
            .await
        }
        Err(RepositoryError::Internal(message)) => {
            tracing::error!(%message, "user creation failed");
            render_user_error(
                &state,
                &parts,
                None,
                &form.username,
                &form.role,
                "No se pudo crear (error interno).",
            )
            .await
        }
    }
}

/// The account creation payload.
#[derive(Deserialize, Default)]
struct NewUserForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    role: String,
}

/// `GET /admin/users/{id}/edit`: the role / password-reset form.
async fn edit_user_form(
    State(state): State<AppState>,
    _admin: CmsAdmin,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Some(user) = load_user(&state, id).await else {
        return see_other("/admin/users");
    };

    render_view(
        &state,
        &parts,
        "admin/user_form.jhs",
        vec![
            (
                "form",
                json!({
                    "id": user.id,
                    "username": user.username,
                    "role": user.role.as_str(),
                    "is_new": false,
                }),
            ),
            ("form_error", Value::Null),
        ],
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/users/{id}`: changes the role and/or resets the
/// password, guarded against self-edits and the last-admin lockout.
async fn update_user(
    State(state): State<AppState>,
    admin: CmsAdmin,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Some(auth) = state.auth_context() else {
        return AppError::internal("authentication is not initialized").into_response();
    };

    let form = match read_form::<UserForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_user_error(&state, &parts, Some(id), "", "user", &message).await;
        }
    };

    let Some(user) = load_user(&state, id).await else {
        return see_other("/admin/users");
    };

    if user.id == admin.user.user_id {
        return render_user_error(
            &state,
            &parts,
            Some(id),
            &user.username,
            &form.role,
            "No puedes editar tu propia cuenta aquí: usa la página de perfil.",
        )
        .await;
    }

    let Some(new_role) = UserRole::parse(&form.role) else {
        return render_user_error(
            &state,
            &parts,
            Some(id),
            &user.username,
            "user",
            "Rol desconocido.",
        )
        .await;
    };

    // Validate the optional password reset before applying anything,
    // so a bad password does not leave a half-applied edit.
    let password_hash = if form.password.is_empty() {
        None
    } else {
        if let Err(error) = validate_password(&form.password, auth.min_password_len) {
            return render_user_error(&state, &parts, Some(id), &user.username, &form.role, &error)
                .await;
        }
        match hash_password(&form.password) {
            Ok(hash) => Some(hash),
            Err(error) => {
                tracing::error!(%error, "password hashing failed");
                return render_user_error(
                    &state,
                    &parts,
                    Some(id),
                    &user.username,
                    &form.role,
                    "No se pudo guardar la contraseña (error interno).",
                )
                .await;
            }
        }
    };

    // The last-admin lockout guard.
    if user.role == UserRole::Admin && new_role != UserRole::Admin {
        match auth.repository.count_with_role(UserRole::Admin).await {
            Ok(admins) if admins <= 1 => {
                return render_user_error(
                    &state,
                    &parts,
                    Some(id),
                    &user.username,
                    &form.role,
                    "Es el último administrador: no se puede quitar el rol.",
                )
                .await
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "admin count failed");
                return AppError::internal("storage failure").into_response();
            }
        }
    }

    if let Err(error) = auth.repository.update_role(id, new_role).await {
        tracing::error!(%error, "role update failed");
        return render_user_error(
            &state,
            &parts,
            Some(id),
            &user.username,
            &form.role,
            "No se pudo guardar (error interno).",
        )
        .await;
    }

    // The optional password reset.
    if let Some(password_hash) = password_hash {
        if let Err(error) = auth.repository.update_password(id, &password_hash).await {
            tracing::error!(%error, "password update failed");
            return render_user_error(
                &state,
                &parts,
                Some(id),
                &user.username,
                &form.role,
                "No se pudo guardar (error interno).",
            )
            .await;
        }
    }

    see_other("/admin/users?ok=guardado")
}

/// `POST /admin/users/{id}/delete`: removes an account (refresh tokens
/// cascade in SQL; access tokens die with their ≤`token_ttl_secs`
/// lifetime), guarded like the update.
async fn delete_user(
    State(state): State<AppState>,
    admin: CmsAdmin,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let _parts = PageParts::of(&request);
    let Some(auth) = state.auth_context() else {
        return AppError::internal("authentication is not initialized").into_response();
    };

    let Some(user) = load_user(&state, id).await else {
        return see_other("/admin/users");
    };

    if user.id == admin.user.user_id {
        return see_other("/admin/users?error=propia");
    }
    if user.role == UserRole::Admin {
        match auth.repository.count_with_role(UserRole::Admin).await {
            Ok(admins) if admins <= 1 => return see_other("/admin/users?error=ultimo-admin"),
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "admin count failed");
                return AppError::internal("storage failure").into_response();
            }
        }
    }

    match auth.repository.delete(id).await {
        Ok(()) => see_other("/admin/users?ok=eliminado"),
        Err(error) => {
            tracing::error!(%error, "user deletion failed");
            AppError::internal("storage failure").into_response()
        }
    }
}

/// Loads a user by id, mapping storage failures to "missing".
async fn load_user(state: &AppState, id: i64) -> Option<User> {
    let auth = state.auth_context()?;
    match auth.repository.find_by_id(id).await {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(%error, "user lookup failed");
            None
        }
    }
}

/// Template data for the user form (submitted values survive errors).
fn user_form_data(id: Option<i64>, username: &str, role: &str) -> Value {
    json!({
        "id": id,
        "username": username,
        "role": role,
        "is_new": id.is_none(),
    })
}

/// Re-renders the user form with an error, keeping the submitted role.
async fn render_user_error(
    state: &AppState,
    parts: &PageParts,
    id: Option<i64>,
    username: &str,
    role: &str,
    error: &str,
) -> Response {
    render_view(
        state,
        parts,
        "admin/user_form.jhs",
        vec![
            ("form", user_form_data(id, username, role)),
            ("form_error", Value::String(error.to_owned())),
        ],
        StatusCode::OK,
    )
    .await
}

// ─── Self-service: `POST /perfil/password` ───────────────────────────

/// `POST /perfil/password`: changes the caller's own password after
/// verifying the current one.
#[derive(Deserialize, Default)]
struct PasswordForm {
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    redirect: Option<String>,
}

async fn change_password(
    State(state): State<AppState>,
    user: AuthUser,
    request: Request,
) -> Response {
    let max_body = state.config().server.max_body_size_bytes;
    let Some(auth) = state.auth_context() else {
        return AppError::internal("authentication is not initialized").into_response();
    };

    let form = match read_form::<PasswordForm>(request, max_body).await {
        Ok(form) => form,
        Err(_) => return password_error("/perfil", "forma"),
    };

    // The form lives on `/perfil`; the optional field lets a future
    // embedding post it from anywhere local.
    let redirect = local_redirect(form.redirect.as_deref());

    let Some(record) = auth
        .repository
        .find_by_id(user.user_id)
        .await
        .ok()
        .flatten()
    else {
        return password_error(&redirect, "sesion");
    };

    if !verify_password(&form.current_password, &record.password_hash) {
        return password_error(&redirect, "actual");
    }
    if validate_password(&form.new_password, auth.min_password_len).is_err() {
        return password_error(&redirect, "nueva");
    }

    let password_hash = match hash_password(&form.new_password) {
        Ok(hash) => hash,
        Err(error) => {
            tracing::error!(%error, "password hashing failed");
            return password_error(&redirect, "error");
        }
    };

    if let Err(error) = auth
        .repository
        .update_password(user.user_id, &password_hash)
        .await
    {
        tracing::error!(%error, "password update failed");
        return password_error(&redirect, "error");
    }

    tracing::info!(user_id = user.user_id, "password changed by its owner");
    let separator = if redirect.contains('?') { '&' } else { '?' };
    see_other(&format!("{redirect}{separator}ok=contrasena"))
}

/// Where the password form lands after the `303` (a local path, or
/// `/perfil` — the page the form lives on).
fn local_redirect(target: Option<&str>) -> String {
    match target {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && !path.chars().any(char::is_control) =>
        {
            path.to_owned()
        }
        _ => String::from("/perfil"),
    }
}

/// `?pw_error=<code>` redirect for the password change.
fn password_error(redirect: &str, code: &str) -> Response {
    let separator = if redirect.contains('?') { '&' } else { '?' };
    see_other(&format!("{redirect}{separator}pw_error={code}"))
}

// ─── Route fragment ──────────────────────────────────────────────────

/// Route fragment for this module (merged while the CMS is enabled).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/p", get(page_index))
        .route("/p/{slug}", get(public_page))
        .route("/sitemap.xml", get(sitemap))
        .route("/feed.xml", get(rss_feed))
        .route("/atom.xml", get(atom_feed))
        .route("/perfil/password", post(change_password))
        .route("/admin", get(dashboard))
        .route("/admin/pages", get(list_pages).post(create_page))
        .route("/admin/pages/new", get(new_page_form))
        .route("/admin/pages/import", get(import_form).post(import_page))
        // Static segment first: axum's matcher prefers it over `{id}`,
        // so the preview never shadows an edit URL.
        .route("/admin/pages/preview", post(preview_page))
        .route("/admin/pages/{id}/edit", get(edit_page_form))
        .route("/admin/pages/{id}", post(update_page))
        .route("/admin/pages/{id}/delete", post(delete_page))
        .route("/admin/menus", get(list_menus).post(create_menu))
        .route("/admin/menus/{id}", get(menu_detail).post(rename_menu))
        .route("/admin/menus/{id}/delete", post(delete_menu))
        .route("/admin/menus/{id}/items/new", get(new_item_form))
        .route("/admin/menus/{id}/items", post(create_item))
        .route(
            "/admin/menus/{id}/items/{item_id}/edit",
            get(edit_item_form),
        )
        .route("/admin/menus/{id}/items/{item_id}", post(update_item))
        .route(
            "/admin/menus/{id}/items/{item_id}/delete",
            post(delete_item),
        )
        .route("/admin/users", get(list_users).post(create_user))
        .route("/admin/users/new", get(new_user_form))
        .route("/admin/users/{id}/edit", get(edit_user_form))
        .route("/admin/users/{id}", post(update_user))
        .route("/admin/users/{id}/delete", post(delete_user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_covers_the_five_required_characters() {
        assert_eq!(xml_escape("plain"), "plain");
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn sitemap_entries_carry_escaped_locs_and_iso_dates() {
        let entry = sitemap_entry("https://x.example/p/a&b", 0);
        assert!(
            entry.contains("<loc>https://x.example/p/a&amp;b</loc>"),
            "{entry}"
        );
        assert!(entry.contains("<lastmod>1970-01-01</lastmod>"), "{entry}");
    }

    #[test]
    fn positions_parse_with_bounds_and_defaults() {
        assert_eq!(parse_position(None), Ok(None));
        assert_eq!(parse_position(Some("")), Ok(None));
        assert_eq!(parse_position(Some("  ")), Ok(None));
        assert_eq!(parse_position(Some("7")), Ok(Some(7)));
        assert_eq!(parse_position(Some("0")), Ok(Some(0)));
        assert_eq!(parse_position(Some("99999")), Ok(Some(99_999)));
        assert!(parse_position(Some("abc")).is_err());
        assert!(parse_position(Some("-1")).is_err());
        assert!(parse_position(Some("100000")).is_err());
    }

    #[test]
    fn slugs_accept_the_documented_shape() {
        for slug in ["a", "hola", "sobre-nosotros", "p1", "a".repeat(64).as_str()] {
            assert!(validate_slug(slug).is_ok(), "{slug} should be valid");
        }
    }

    #[test]
    fn slugs_reject_everything_else() {
        for slug in [
            "",
            "-hola",
            "hola-",
            "hola--mundo",
            "Hola",
            "hola_mundo",
            "hola/mundo",
            "a".repeat(65).as_str(),
            "ñ",
        ] {
            assert!(validate_slug(slug).is_err(), "{slug} should be rejected");
        }
    }

    #[test]
    fn file_names_derive_slugs() {
        assert_eq!(slug_from_file_name("Sobre Nosotros.html"), "sobre-nosotros");
        assert_eq!(slug_from_file_name("hello.jhs"), "hello");
        assert_eq!(slug_from_file_name("Mi Página!!"), "mi-pagina");
        assert_eq!(slug_from_file_name("---"), "importada");
    }

    #[test]
    fn query_safe_paths_are_kept_as_redirects() {
        assert_eq!(local_redirect(Some("/perfil")), "/perfil");
        assert_eq!(local_redirect(Some("/p/x?a=1")), "/p/x?a=1");
        assert_eq!(local_redirect(Some("http://evil.example")), "/perfil");
        assert_eq!(local_redirect(Some("//evil.example")), "/perfil");
        assert_eq!(local_redirect(None), "/perfil");
    }
}
