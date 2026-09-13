//! Content-negotiated error pages (F13).
//!
//! The JSON error envelope is the right answer for API clients — and
//! the wrong answer for a browser: a person clicking the panel's theme
//! toggle that trips the rate limiter should never stare at
//! `{"error":{"code":"RATE_LIMITED",…}}`. This middleware watches every
//! response on its way out and, when **all** of the following hold:
//!
//! 1. the status is 4xx/5xx,
//! 2. the body is the standard error envelope (`application/json`),
//! 3. the request's `Accept` header prefers `text/html` (browser
//!    navigations and form posts; the site is script-free, so every
//!    HTML-preferring request *is* a human),
//!
//! it rebuilds the response with the shared, script-free English error
//! page ([`HtmlErrorPage`], styled by `public/assets/error.css`) while
//! keeping the status and the interesting headers (`Retry-After`,
//! `X-Request-Id`, the limiter's fields) intact. Everything else —
//! curls, health checks, Prometheus scrapes, JSON API clients — keeps
//! the byte-identical envelope, so every existing contract and test
//! holds.
//!
//! 429 answers gain a self-heal: the page carries
//! `<meta http-equiv="refresh" content="N">` with `N` taken from the
//! `Retry-After` header (capped), so the browser reloads on its own
//! once the token bucket refills. The theme toggle's 303 has already
//! pinned the `wm_theme` cookie by then, which is why a manual refresh
//! "fixed" the panel before this middleware existed.
//!
//! Execution order: second, right inside the security-headers layer —
//! the swapped page still receives CSP and friends on its way out, and
//! the inner logging middleware has already recorded the true status.
//! The layer also honors the visitor's pinned panel theme by reading
//! the `wm_theme` cookie (the same Rust-side read the admin shell
//! uses), so editors get dark error pages to match their dark panel.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use crate::error::HtmlErrorPage;

/// The swapped page's media type.
const HTML_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("text/html; charset=utf-8");

/// Envelope bodies are tiny; anything bigger is not an envelope and
/// passes through untouched.
const ENVELOPE_READ_LIMIT: usize = 32 * 1024;

/// Swaps envelope error responses for the shared HTML error page on
/// browser navigations (see the module docs for the negotiation
/// rules and the 429 self-heal).
pub async fn run(request: Request, next: Next) -> Response {
    let accept = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let method = request.method().as_str().to_owned();
    let target = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned());
    // Owned: the headers (and their borrow) move into `next.run`.
    let theme = crate::middleware::templates::pinned_theme(request.headers()).map(str::to_owned);

    let response = next.run(request).await;

    if !prefers_html(accept.as_deref()) {
        return response;
    }

    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let is_envelope_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_envelope_json {
        return Response::from_parts(parts, body);
    }

    // Reading consumes the body: on failure there is nothing sane to
    // re-serve, so the visitor gets the plain page instead.
    let Ok(bytes) = axum::body::to_bytes(body, ENVELOPE_READ_LIMIT).await else {
        return envelope_free_page(
            status,
            None,
            &method,
            target.as_deref(),
            theme.as_deref(),
            parts,
        );
    };
    let Some((code, message, request_id)) = parse_envelope(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    // The self-heal delay rides on the header the limiter already set.
    let retry_after_secs = parts
        .headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    let page = HtmlErrorPage {
        status,
        code: &code,
        message: &message,
        request_id: request_id.as_deref(),
        method: Some(&method),
        target: target.as_deref(),
        retry_after_secs,
        theme: theme.as_deref(),
    };
    let html = page.render();

    // The swapped page must not inherit the envelope's length (the
    // transport recalculates it) but keeps every other header —
    // `Retry-After`, `X-Request-Id`, the rate-limit fields — and the
    // outer layers still add CSP/HSTS on their way out.
    parts.headers.remove(header::CONTENT_LENGTH);
    parts
        .headers
        .insert(header::CONTENT_TYPE, HTML_CONTENT_TYPE);
    parts
        .headers
        .append(header::VARY, axum::http::HeaderValue::from_static("accept"));
    parts.headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Response::from_parts(parts, Body::from(html))
}

/// Serves a plain page when the envelope could not even be read.
fn envelope_free_page(
    status: StatusCode,
    request_id: Option<&str>,
    method: &str,
    target: Option<&str>,
    theme: Option<&str>,
    mut parts: axum::http::response::Parts,
) -> Response {
    let page = HtmlErrorPage {
        status,
        code: "REQUEST_FAILED",
        message: "The response could not be rendered.",
        request_id,
        method: Some(method),
        target,
        retry_after_secs: None,
        theme,
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts
        .headers
        .insert(header::CONTENT_TYPE, HTML_CONTENT_TYPE);
    Response::from_parts(parts, Body::from(page.render()))
}

/// Extracts `(code, message, request_id)` from an error envelope body.
/// Anything else (non-JSON, JSON without the `error` object) yields
/// `None` and the original bytes pass through.
fn parse_envelope(bytes: &[u8]) -> Option<(String, String, Option<String>)> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let error = value.get("error")?;
    let code = error.get("code")?.as_str()?.to_owned();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let request_id = error
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((code, message, request_id))
}

/// Whether an `Accept` header prefers HTML over JSON.
///
/// Modelled on the handful of shapes real clients send:
///
/// - browsers navigate with `text/html,application/xhtml+xml,…` → HTML;
/// - API clients ask for `application/json` → JSON;
/// - curl/tools send `*/*` (or nothing) → JSON, the historical answer;
/// - a stylesheet/image request never lists HTML → JSON (invisible).
///
/// Highest quality wins; ties go to whichever media type appeared
/// first (browsers list `text/html` first). Wildcards do not vote.
fn prefers_html(accept: Option<&str>) -> bool {
    let Some(accept) = accept else {
        return false;
    };

    #[derive(PartialEq)]
    enum Kind {
        Html,
        Json,
    }

    let mut best: Option<(f32, Kind)> = None;
    for entry in accept.split(',') {
        let mut parameters = entry.split(';');
        let Some(media) = parameters.next().map(str::trim) else {
            continue;
        };
        let media = media.to_ascii_lowercase();
        let kind = match media.as_str() {
            "text/html" | "application/xhtml+xml" => Kind::Html,
            "application/json" | "text/json" => Kind::Json,
            _ => continue,
        };
        let quality = parameters
            .map(str::trim)
            .find_map(|parameter| parameter.strip_prefix("q="))
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(1.0);
        if quality <= 0.0 {
            continue;
        }
        // Strictly better quality replaces the incumbent; equal
        // quality keeps the earlier entry (documented tie-break).
        let replace = match &best {
            Some((best_quality, _)) => quality > *best_quality,
            None => true,
        };
        if replace {
            best = Some((quality, kind));
        }
    }
    matches!(best, Some((_, Kind::Html)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_accepts_prefer_html() {
        assert!(prefers_html(Some(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
        )));
        assert!(prefers_html(Some("text/html")));
        assert!(prefers_html(Some("TEXT/HTML")));
    }

    #[test]
    fn api_accepts_prefer_json() {
        assert!(!prefers_html(Some("application/json")));
        assert!(!prefers_html(Some(
            "application/json;q=1.0, text/html;q=0.5"
        )));
        assert!(!prefers_html(Some("*/*")));
        assert!(!prefers_html(Some("image/avif,image/webp,*/*;q=0.8")));
        assert!(!prefers_html(None));
    }

    #[test]
    fn quality_overrides_order() {
        assert!(prefers_html(Some("application/json;q=0.9, text/html")));
    }

    #[test]
    fn envelope_parsing_reads_the_error_object() {
        let parsed = parse_envelope(
            b"{\"error\":{\"code\":\"NOT_FOUND\",\"message\":\"No route matches GET /nope\",\
              \"request_id\":\"req-17\"}}",
        );
        assert_eq!(
            parsed,
            Some((
                String::from("NOT_FOUND"),
                String::from("No route matches GET /nope"),
                Some(String::from("req-17"))
            ))
        );
        assert!(parse_envelope(br#"{"status":"ok"}"#).is_none());
        assert!(parse_envelope(b"not json").is_none());
    }
}
