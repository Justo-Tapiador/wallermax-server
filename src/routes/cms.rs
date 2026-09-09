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
//!
//! **Admin panel** (`admin` and `editor` roles; browser-friendly
//! guards: unauthenticated visitors are redirected to `/login`, and
//! the forms re-render with their values and the error on failure so
//! nothing typed is ever lost):
//!
//! - `GET  /admin` — dashboard (page and user counters);
//! - `GET  /admin/pages` — every page, drafts included;
//! - `GET  /admin/pages/new` / `POST /admin/pages` — create;
//! - `GET  /admin/pages/{id}/edit` / `POST /admin/pages/{id}` — edit;
//! - `POST /admin/pages/{id}/delete` — delete;
//! - `GET  /admin/pages/import` / `POST` — copy a file from `public/`
//!   into a new draft page (read-only on the static tree: the CMS
//!   never writes into `public/`).
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

use std::path::PathBuf;

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::auth::{hash_password, validate_password, validate_username, verify_password};
use crate::db::{NewPage, PageUpdate, RepositoryError, User, UserRole};
use crate::error::AppError;
use crate::extractors::AuthUser;
use crate::middleware::request_id::RequestId;
use crate::middleware::templates::{base_data, redirect_response, render_response};
use crate::state::{AppState, CmsContext};
use crate::util::{format_timestamp, read_form};

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
fn html_error_page(status: StatusCode, title: &str, message: &str) -> Response {
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
}

/// Renders `views/<view>.jhs` with the base globals plus `extra`,
/// answering with `status`.
async fn render_view(
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
#[allow(clippy::result_large_err)] // the Err is a one-shot browser response
fn cms_context(state: &AppState) -> Result<&CmsContext, Response> {
    state
        .cms()
        .ok_or_else(|| AppError::internal("the CMS is not initialized").into_response())
}

/// A bare `303 See Other`.
fn see_other(location: &str) -> Response {
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

    // Render the page body (template source) with the same globals.
    let body_html = {
        let Some(templates) = state.templates() else {
            return PublicPageOutcome::Served(
                AppError::internal("templates are not initialized").into_response(),
            );
        };
        let engine = templates.engine();
        let content = page.content.clone();
        let globals = data.clone();
        match tokio::task::spawn_blocking(move || engine.render_string(&content, &globals)).await {
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
    };

    let author: Value = match (page.created_by, state.auth_context()) {
        (Some(user_id), Some(auth)) => match auth.repository.find_by_id(user_id).await {
            Ok(Some(author)) => Value::String(author.username),
            _ => Value::Null,
        },
        _ => Value::Null,
    };

    let page_json = json!({
        "id": page.id,
        "slug": page.slug,
        "title": page.title,
        "is_published": page.is_published,
        "created_at_h": format_timestamp(page.created_at),
        "updated_at_h": format_timestamp(page.updated_at),
        "author": author,
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

    let stats = json!({
        "pages": total,
        "published": published,
        "drafts": total - published,
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
    /// HTML checkboxes post `on` when checked and nothing when not.
    is_published: Option<String>,
}

impl PageForm {
    fn published(&self) -> bool {
        matches!(
            self.is_published.as_deref(),
            Some("on") | Some("true") | Some("1")
        )
    }

    /// The form as template data (`id` and `is_new` added by callers).
    fn form_data(&self, id: Option<i64>, error: Option<&str>) -> (Value, Value) {
        let form = json!({
            "id": id,
            "slug": self.slug,
            "title": self.title,
            "content": self.content,
            "is_published": self.published(),
            "is_new": id.is_none(),
        });
        (
            form,
            error
                .map(|message| Value::String(message.to_owned()))
                .unwrap_or(Value::Null),
        )
    }
}

/// `GET /admin/pages`: every page, drafts included.
async fn list_pages(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let pages = match cms.pages.list(true, MAX_LISTED).await {
        Ok(pages) => pages,
        Err(error) => {
            tracing::error!(%error, "page listing failed");
            return AppError::internal("storage failure").into_response();
        }
    };

    let admin_pages: Vec<Value> = pages
        .into_iter()
        .map(|page| {
            json!({
                "id": page.id,
                "slug": page.slug,
                "title": page.title,
                "is_published": page.is_published,
                "updated_at_h": format_timestamp(page.updated_at),
            })
        })
        .collect();

    render_view(
        &state,
        &parts,
        "admin/pages.jhs",
        vec![("admin_pages", Value::Array(admin_pages))],
        StatusCode::OK,
    )
    .await
}

/// `GET /admin/pages/new`: the empty creation form.
async fn new_page_form(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let form = PageForm::default();
    let (form, error) = form.form_data(None, None);
    render_view(
        &state,
        &parts,
        "admin/page_form.jhs",
        vec![("form", form), ("form_error", error)],
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

    let new_page = NewPage {
        slug: form.slug.clone(),
        title: form.title.clone(),
        content: form.content.clone(),
        is_published: form.published(),
        created_by: Some(editor.user.user_id),
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
        is_published: page.is_published.then(|| "on".to_owned()),
    };
    let (form, error) = form.form_data(Some(id), None);
    render_view(
        &state,
        &parts,
        "admin/page_form.jhs",
        vec![("form", form), ("form_error", error)],
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

    let update = PageUpdate {
        slug: Some(form.slug.clone()),
        title: form.title.clone(),
        content: form.content.clone(),
        is_published: form.published(),
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
    let (form, error) = form.form_data(id, Some(error));
    render_view(
        state,
        parts,
        "admin/page_form.jhs",
        vec![("form", form), ("form_error", error)],
        StatusCode::OK,
    )
    .await
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
        is_published: false,
        created_by: Some(editor.user.user_id),
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
        .route("/p/{slug}", get(public_page))
        .route("/perfil/password", post(change_password))
        .route("/admin", get(dashboard))
        .route("/admin/pages", get(list_pages).post(create_page))
        .route("/admin/pages/new", get(new_page_form))
        .route("/admin/pages/import", get(import_form).post(import_page))
        .route("/admin/pages/{id}/edit", get(edit_page_form))
        .route("/admin/pages/{id}", post(update_page))
        .route("/admin/pages/{id}/delete", post(delete_page))
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
