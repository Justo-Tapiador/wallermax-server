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
//! 4. Everything else runs the normal pipeline (API routes first, then
//!    static files).
//! 5. A pipeline `404` auto-routes to the views directory before the
//!    JSON envelope is returned: `GET /contacto` renders
//!    `views/contacto.jhs`, `GET /blog` renders `views/blog.jhs` or
//!    `views/blog/index.jhs`, and `GET /` falls back to
//!    `views/index.jhs` when the static index file is missing.
//!
//! Rendering happens on the blocking pool (`spawn_blocking`): the JS
//! engine is CPU-bound and the fresh-sandbox-per-render design keeps it
//! off the async workers. Template errors answer the standard JSON
//! error envelope (`500 INTERNAL_ERROR`) with the engine's message,
//! which is template-author-facing diagnostics rather than a leak:
//! template code never sees server internals.
//!
//! ## Template data: the `user`, `path`, `query`, `pages` and `req`
//! globals
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
//! - `req` — an Express-shaped request object (v0.9.0):
//!   `{ method, url, path, query, headers }`. Only a fixed allowlist
//!   of harmless headers (`accept`, `accept-language`, `content-type`,
//!   `host`, `referer`, `user-agent`) is exposed — CMS **editors**
//!   author page bodies, so `cookie`/`authorization` values must never
//!   reach template code (an editor page echoing the viewer's session
//!   cookie would leak it to the page author).
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
use crate::session;
use crate::state::AppState;
use crate::template_engine::{JhsEngine, RedirectIntent, RenderOutput};

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

    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());

    let data = base_data(&state, request.headers(), request.uri(), &method).await;

    // On-the-fly rendering of `.jhs` files under the static root.
    if let Some(static_root) = templates.static_root() {
        if decoded.ends_with(".jhs") {
            let candidate = static_root.join(trim_leading_slash(&decoded));
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

    // Auto-route views for otherwise-unmatched paths.
    if response.status() == StatusCode::NOT_FOUND {
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
    let user = state
        .auth_context()
        .filter(|_| state.config().templates.expose_user)
        .and_then(|auth| {
            bearer_token(headers)
                .or_else(|| session::session_token(headers))
                .and_then(|token| auth.jwt.verify_token(token).ok())
        })
        .and_then(|claims| AuthUser::from_claims(&claims).ok())
        .map(|user| {
            json!({
                "id": user.user_id,
                "username": user.username,
                "role": user.role.as_str(),
            })
        })
        .unwrap_or(Value::Null);

    let mut data = Map::new();
    data.insert(String::from("user"), user);
    data.insert(String::from("path"), Value::String(uri.path().to_owned()));
    data.insert(String::from("query"), query_global(uri));
    data.insert(String::from("pages"), pages_global(state).await);
    data.insert(String::from("req"), req_global(headers, uri, method));
    data
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

/// The `pages` global: published CMS pages, newest first (empty while
/// the CMS is disabled).
async fn pages_global(state: &AppState) -> Value {
    let Some(cms) = state.cms() else {
        return Value::Array(Vec::new());
    };

    match cms.pages.list(false, PAGES_GLOBAL_LIMIT).await {
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

/// Renders `path` with `data` and builds the response with `status`
/// (or the JSON error envelope).
///
/// Crate-visible so the CMS handlers render through the exact same
/// pipeline (blocking pool, console capture, `no-store`, security
/// headers) as the auto-routed views. A `res.redirect()` issued
/// inside the template wins over the HTML: the response is the
/// redirect (with the template-chosen status) instead.
pub(crate) async fn render_response(
    engine: std::sync::Arc<JhsEngine>,
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
/// - `/contacto` → `views/contacto.jhs`, then `views/contacto/index.jhs`;
/// - `/blog/` → the same two shapes as `/blog`.
fn view_candidate(views_dir: &Path, decoded: &str) -> Option<PathBuf> {
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
        assert_eq!(view_candidate(&dir, "/flat"), Some(dir.join("flat.jhs")));
        assert_eq!(
            view_candidate(&dir, "/flat.jhs"),
            Some(dir.join("flat.jhs"))
        );
        assert_eq!(view_candidate(&dir, "/missing"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
