//! The public search page (F10) — zero JavaScript, like everything
//! else in the CMS.
//!
//! `GET /search?q=...` (the pre-F13 Spanish spelling `/buscar` answers
//! a `301` that preserves the query) returns a plain HTML page: the
//! form (a twin of the one living in the shared header), the ranked
//! hits with highlighted snippets, and the same pagination the `/p`
//! index uses. The search runs on SQLite FTS5 through
//! [`crate::db::PageRepository::search`], and the visitor's words are
//! quoted into inert phrases first ([`crate::db::fts_match_query`]),
//! so FTS5's own query operators can never be typed in from a browser.
//!
//! Snippet highlighting is safe by construction: the repository wraps
//! the hits in `⟦ ⟧` markers and [`fragment_segments`] splits them
//! into `{ texto, hit }` segments; the view prints every segment
//! through the auto-escaping `<?= ?>` and wraps the hit ones in
//! `<mark>`. No `raw()`, no pre-escaped HTML strings — a page body can
//! never reach the browser as markup through a search result.
//!
//! Only published pages are ever visible here; drafts are the admin
//! filter's business (`GET /admin/pages?q=`, in
//! [`crate::routes::cms`]).

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::db::fts_match_query;
use crate::error::AppError;
use crate::routes::cms::{
    clamp_page, cms_context, listing_query, pages_for, pagination_value, render_view, PageParts,
};
use crate::state::AppState;
use crate::util::format_timestamp;

/// Route fragment for this module (merged while the CMS is enabled).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/search", get(search_page))
        // The F13 anglicization moved the search to /search; the old
        // Spanish spelling stays as a permanent redirect so existing
        // links and bookmarks keep working.
        .route("/buscar", get(legacy_search_redirect))
}

/// `GET /buscar` (pre-F13 spelling): `301` to `/search`, query
/// preserved. The raw query is echoed only while it stays printable
/// ASCII (the URI parser already rejects control bytes, so this is
/// belt-and-braces against a header-injection reflex).
async fn legacy_search_redirect(request: Request) -> Response {
    let target = request
        .uri()
        .query()
        .filter(|query| query.bytes().all(|byte| byte.is_ascii_graphic()))
        .map(|query| format!("/search?{query}"))
        .unwrap_or_else(|| String::from("/search"));
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, target)
        .body(Body::empty())
        .expect("valid redirect")
}

/// `GET /search`: the search form and its results, paginated with
/// `[cms] index_page_size`. An empty — or quotes-only, which sanitizes
/// to nothing — query renders the page with an invitation instead of
/// an error: a search box should never answer 4xx.
async fn search_page(State(state): State<AppState>, request: Request) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };
    let page_size = i64::from(state.config().cms.index_page_size);
    let (mut terms, requested) = listing_query(parts.uri());
    // F20: the visitor searches the organization their host serves.
    let organization = parts.organization(&state);

    let searched = fts_match_query(&terms).is_some();
    // Quotes-only input sanitizes to no query at all: the view then
    // shows the invitation instead of an empty result set for words
    // the visitor never really typed.
    if !searched {
        terms = String::new();
    }
    let total = if searched {
        match cms.pages.search_count(&organization, &terms, false).await {
            Ok(total) => total,
            Err(error) => {
                tracing::error!(%error, "page search count failed");
                return AppError::internal("storage failure").into_response();
            }
        }
    } else {
        0
    };
    // An out-of-range page number lands on the nearest real page; a
    // result-less search stays on page one.
    let page = clamp_page(requested, pages_for(total, page_size).max(1));
    let results = if searched {
        match cms
            .pages
            .search(
                &organization,
                &terms,
                false,
                page_size,
                (page - 1) * page_size,
            )
            .await
        {
            Ok(hits) => hits,
            Err(error) => {
                tracing::error!(%error, "page search failed");
                return AppError::internal("storage failure").into_response();
            }
        }
    } else {
        Vec::new()
    };

    let resultados: Vec<Value> = results
        .iter()
        .map(|hit| {
            json!({
                "slug": hit.slug,
                "title": hit.title,
                "updated_at_h": format_timestamp(hit.updated_at),
                "fragmento": fragment_segments(&hit.fragment),
            })
        })
        .collect();

    // The pagination links must carry the query along, re-encoded so
    // the visitor's words survive intact (spaces, accents, operators).
    let base = format!(
        "/search?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", &terms)
            .finish()
    );

    render_view(
        &state,
        &parts,
        "search.jhs",
        vec![
            ("busqueda", Value::String(terms)),
            ("resultados", Value::Array(resultados)),
            ("total_resultados", json!(total)),
            (
                "paginacion",
                pagination_value(page, total, page_size, &base),
            ),
        ],
        StatusCode::OK,
    )
    .await
}

/// Splits a snippet fragment (the hits wrapped in the `⟦`/`⟧` markers
/// by SQL `snippet(pages_fts, ...)`) into `{ texto, hit }` segments
/// the views loop over. Every segment prints through the
/// auto-escaping `<?= ?>` and the hit ones get a `<mark>` around them
/// — the highlighting is escaped by construction, and a stray marker
/// faked inside a page body can at worst split a segment, never
/// inject markup.
pub(crate) fn fragment_segments(fragment: &str) -> Vec<Value> {
    fn segment(text: &str, hit: bool) -> Value {
        json!({ "texto": text, "hit": hit })
    }

    let mut segments: Vec<Value> = Vec::new();
    let mut current = String::new();
    let mut in_hit = false;
    for character in fragment.chars() {
        match character {
            '⟦' => {
                if !current.is_empty() {
                    segments.push(segment(&current, in_hit));
                    current.clear();
                }
                in_hit = true;
            }
            '⟧' => {
                if !current.is_empty() {
                    segments.push(segment(&current, in_hit));
                    current.clear();
                }
                in_hit = false;
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        segments.push(segment(&current, in_hit));
    }
    segments
}
