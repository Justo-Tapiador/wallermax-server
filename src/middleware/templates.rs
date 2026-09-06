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
//! `console.*` output inside templates is routed to `tracing` at the
//! matching level instead of the process stdout.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use percent_encoding::percent_decode_str;

use crate::error::AppError;
use crate::middleware::request_id::RequestId;
use crate::state::AppState;
use crate::template_engine::{JhsEngine, RenderOutput};

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

    // On-the-fly rendering of `.jhs` files under the static root.
    if let Some(static_root) = templates.static_root() {
        if decoded.ends_with(".jhs") {
            let candidate = static_root.join(trim_leading_slash(&decoded));
            if candidate.is_file() {
                return render_response(templates.engine(), candidate, request_id, &method).await;
            }
        }
    }

    // Normal pipeline (API routes, static files, JSON 404 fallback).
    let response = next.run(request).await;

    // Auto-route views for otherwise-unmatched paths.
    if response.status() == StatusCode::NOT_FOUND {
        if let Some(view) = view_candidate(templates.views_dir(), &decoded) {
            return render_response(templates.engine(), view, request_id, &method).await;
        }
    }

    response
}

/// Renders `path` and builds the response (or the JSON error envelope).
async fn render_response(
    engine: std::sync::Arc<JhsEngine>,
    path: PathBuf,
    request_id: Option<String>,
    method: &Method,
) -> Response {
    let label = path.display().to_string();
    let render = tokio::task::spawn_blocking(move || {
        let data = serde_json::Map::new();
        engine.render(&label, &data)
    })
    .await;

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

    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(output.html)
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_else(|error| {
            tracing::error!(%error, "failed to build the template response");
            AppError::internal("failed to build the template response".to_owned())
                .into_response_with_request_id(request_id.as_deref())
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
