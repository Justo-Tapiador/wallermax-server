//! External API proxy (the `[external_api]` section, v0.12.0).
//!
//! `GET/POST /api/ext/{name}` — and, when the page needs a deeper route,
//! `GET/POST /api/ext/{name}/{subpath...}` — forwards to a **named,
//! operator-configured** upstream (see [`crate::external_api`]) and passes
//! the answer back to the browser. The point is secret hygiene: pages never see the API
//! keys, and the upstream's CORS policy stops mattering because the
//! browser only ever talks to this origin.
//!
//! Rules the handler enforces on every call:
//!
//! - the client picks a *name*, never a URL — no SSRF surface;
//! - a client subpath (`/api/ext/{name}/{subpath...}`) is appended after
//!   the configured URL with exactly one `/`: it is percent-encoded
//!   back to a canonical form segment by segment, `.` and `..` segments
//!   answer `400`, and duplicate `/` collapse — the client may steer the
//!   upstream *path*, never its host or scheme;
//! - nothing from the incoming request is forwarded upstream (no
//!   cookies, no `Authorization`, no arbitrary headers): only the
//!   configured headers, `User-Agent` and `Accept`;
//! - `auth_required = true` endpoints answer 401 without a valid Bearer
//!   token or session cookie;
//! - the incoming query string is appended to the configured URL; keys
//!   belong in configured headers or in fixed query parameters (the
//!   `keyParam` pattern), never in client-visible URLs — and a fixed
//!   parameter name always replaces the same name arriving from the
//!   browser, so the page can neither read nor shadow the injected
//!   secret;
//! - upstream bodies are capped by `external_api.response_limit_bytes`
//!   and only text-ish media types (`application/json`, `*+json`,
//!   `text/*`) are forwarded — this is a JSON proxy, not a media one;
//! - transport failures and unusable answers render as 502 envelopes;
//!   the exact cause is logged server-side with the endpoint name.
//!
//! Global middlewares apply as usual: rate limiting, the request
//! timeout, body limits, request-id and the security headers (the CSP
//! of a proxied page still governs what the browser may do with the
//! answer).

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::error::AppError;
use crate::extractors::{AuthUser, Rejection};
use crate::state::AppState;

/// Route fragment for this module.
///
/// Mounted only while at least one `[[external_api.endpoints]]` entry is
/// configured (see [`crate::routes`]).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ext/{name}", get(forward).post(forward))
        .route(
            "/api/ext/{name}/{*subpath}",
            get(forward_subpath).post(forward_subpath),
        )
}

/// Hex digits for percent-encoding (uppercase, the canonical form).
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Whether `c` may travel verbatim in a path segment: the RFC 3986
/// `pchar` set minus `%` (the extractor already decoded the client's
/// `%XX`, so a literal `%` must be re-encoded — never passed through).
fn is_pchar(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
        )
}

/// Validates and canonically re-encodes a client-provided subpath.
///
/// Axum percent-**decodes** path captures, so the raw string is untrusted
/// twice over: `..` could try to rewrite the upstream path and a decoded
/// `?`/`#`/control character could try to escape the path entirely. This
/// function turns the subpath back into a canonical, injection-proof
/// path: `.`/`..` segments and control characters are refused, empty
/// segments (from duplicate `/`) collapse, and every character outside
/// `pchar` is percent-encoded again — so a `?` becomes `%3F` and stays
/// part of the path, never a query separator.
///
/// # Errors
///
/// `Err` carries a client-safe reason (`.`/`..` segments, control
/// characters) — the handler answers it as a `400` verbatim.
fn encode_subpath(subpath: &str) -> Result<String, &'static str> {
    let mut segments: Vec<String> = Vec::new();
    for segment in subpath.split('/') {
        if segment.is_empty() {
            continue; // duplicate `/` collapse; the path stays canonical
        }
        if segment == "." || segment == ".." {
            return Err("subpath segments `.` and `..` are not allowed");
        }
        if segment.chars().any(char::is_control) {
            return Err("subpath may not contain control characters");
        }
        let mut encoded = String::with_capacity(segment.len());
        for c in segment.chars() {
            if is_pchar(c) {
                encoded.push(c);
            } else {
                let mut utf8 = [0u8; 4];
                for byte in c.encode_utf8(&mut utf8).as_bytes() {
                    encoded.push('%');
                    encoded.push(HEX[(byte >> 4) as usize] as char);
                    encoded.push(HEX[(byte & 0x0f) as usize] as char);
                }
            }
        }
        segments.push(encoded);
    }
    Ok(segments.join("/"))
}

/// Joins the (already encoded) subpath after the configured base URL:
/// exactly one `/` separates them, whatever the base's trailing slashes,
/// and a query the base already carries stays at the end, after the
/// joined path, where it belongs.
fn join_subpath(base: &str, subpath: &str) -> String {
    if subpath.is_empty() {
        return base.to_owned();
    }
    match base.split_once('?') {
        Some((path, query)) => {
            format!("{}/{}?{query}", path.trim_end_matches('/'), subpath)
        }
        None => format!("{}/{}", base.trim_end_matches('/'), subpath),
    }
}

/// Composes the upstream URL: the configured address plus the incoming
/// query string, with the endpoint's fixed query parameters appended.
///
/// Endpoints without fixed parameters keep the v0.12.0 contract: the
/// incoming query string travels upstream untouched, byte for byte.
/// Endpoints **with** fixed parameters (the `keyParam` pattern) parse
/// the incoming pairs, drop any pair whose name the operator configured
/// and re-serialize — the server-side value always wins, so a browser
/// cannot shadow the injected key with one of its own.
fn upstream_url(base: &str, query: Option<&str>, fixed: &[(String, String)]) -> String {
    if fixed.is_empty() {
        return match query {
            Some(query) if !query.is_empty() => {
                if base.contains('?') {
                    format!("{base}&{query}")
                } else {
                    format!("{base}?{query}")
                }
            }
            _ => base.to_owned(),
        };
    }
    let mut pairs: Vec<(String, String)> = query
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    pairs.retain(|(name, _)| !fixed.iter().any(|(fixed, _)| fixed == name));
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(&name, &value);
    }
    for (name, value) in fixed {
        serializer.append_pair(name, value);
    }
    let composed = serializer.finish();
    if base.contains('?') {
        format!("{base}&{composed}")
    } else {
        format!("{base}?{composed}")
    }
}

/// Whether an upstream media type may be forwarded to the browser.
///
/// JSON (including `application/*+json` profiles) and `text/*` pass;
/// anything binary is refused with a 502 — the proxy is for APIs, not
/// for media files.
fn forwardable_content_type(content_type: Option<&str>) -> bool {
    match content_type {
        None => true,
        Some(value) => {
            let lowered = value.to_ascii_lowercase();
            lowered.starts_with("application/json")
                || lowered.starts_with("text/")
                || (lowered.starts_with("application/") && lowered.contains("+json"))
        }
    }
}

/// Reads the whole upstream body, refusing anything above `limit`.
///
/// Reading stops as soon as the cap is crossed — an upstream answering
/// with gigabytes costs at most `limit + one chunk` of memory.
async fn read_capped(mut upstream: reqwest::Response, limit: usize) -> Result<Bytes, AppError> {
    let mut body = Vec::new();
    while let Some(chunk) = upstream
        .chunk()
        .await
        .map_err(|error| transport_error("reading the upstream body", &error))?
    {
        if body.len() + chunk.len() > limit {
            return Err(AppError::bad_gateway(format!(
                "upstream answer exceeds the configured {limit}-byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// Maps a reqwest transport error to a client-safe 502 (the details are
/// logged by the caller; the client only learns *that* the upstream
/// failed, never where or why).
fn transport_error(stage: &str, error: &reqwest::Error) -> AppError {
    if error.is_timeout() {
        AppError::bad_gateway(format!(
            "upstream did not answer within the configured timeout while {stage}"
        ))
    } else {
        AppError::bad_gateway(format!("upstream is unreachable while {stage}"))
    }
}

/// `GET/POST /api/ext/{name}`: forwards to the configured upstream.
///
/// Thin wrapper — see [`proxy`] for the actual rule list.
async fn forward(
    State(state): State<AppState>,
    Path(name): Path<String>,
    user: Result<AuthUser, Rejection>,
    request: Request,
) -> Response {
    proxy(state, &name, None, user, request).await
}

/// `GET/POST /api/ext/{name}/{subpath}`: same, with the validated and
/// percent-encoded subpath joined after the configured base URL.
///
/// Thin wrapper — see [`proxy`] for the actual rule list.
async fn forward_subpath(
    State(state): State<AppState>,
    Path((name, subpath)): Path<(String, String)>,
    user: Result<AuthUser, Rejection>,
    request: Request,
) -> Response {
    proxy(state, &name, Some(&subpath), user, request).await
}

/// The proxy core both routes share: endpoint lookup, auth, URL
/// composition (optional subpath + query string + fixed parameters) and
/// the upstream call.
///
/// See the module docs for the full rule list; the method is passed
/// through as-is (browsers `GET` or `POST`, the upstream sees the same).
#[allow(clippy::too_many_lines)]
async fn proxy(
    state: AppState,
    name: &str,
    subpath: Option<&str>,
    user: Result<AuthUser, Rejection>,
    request: Request,
) -> Response {
    let api = state.external_api();
    let Some(endpoint) = api.endpoint(name) else {
        return AppError::not_found(request.method().as_str(), request.uri().path())
            .into_response();
    };
    let Some(client) = api.client() else {
        // Unreachable while any endpoint exists; the map and the client
        // are built together.
        return AppError::not_found(request.method().as_str(), request.uri().path())
            .into_response();
    };

    // Authentication is opt-in per endpoint; public pages need to reach
    // their data, and the global rate limiter keeps open endpoints from
    // being hammered.
    if endpoint.auth_required {
        if let Err(rejection) = user {
            return rejection.into_response();
        }
    }

    let method = request.method().clone();
    let query = request.uri().query().map(str::to_owned);
    let timeout = Duration::from_secs(state.config().external_api.timeout_secs);
    let limit = api.response_limit_bytes();

    // Subpath first (validated + re-encoded + joined), query second: the
    // composed base then flows through the regular query rules above.
    let base = match subpath.filter(|subpath| !subpath.is_empty()) {
        None => endpoint.url.clone(),
        Some(raw) => match encode_subpath(raw) {
            Ok(encoded) => join_subpath(&endpoint.url, &encoded),
            Err(reason) => {
                tracing::warn!(
                    endpoint = %name,
                    "external API proxy refused an unsafe subpath"
                );
                return AppError::bad_request(reason).into_response();
            }
        },
    };
    let url = upstream_url(&base, query.as_deref(), &endpoint.query);
    // `axum::http::Method` and `reqwest::Method` are the same `http`
    // type — no conversion needed. The client-level timeout already
    // bounds the whole call; the per-request timeout is belt and braces.
    let mut upstream = client
        .request(method.clone(), &url)
        .timeout(timeout)
        .header(header::ACCEPT, "application/json");
    for (name, value) in &endpoint.headers {
        upstream = upstream.header(name.clone(), value.clone());
    }

    // POST bodies are forwarded as-is (already capped by the global body
    // limit middleware); only text media types travel, mirroring the
    // response side.
    let upstream =
        if method == axum::http::Method::POST {
            let content_type = request
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body_limit = state.config().server.max_body_size_bytes;
            let body =
                match axum::body::to_bytes(request.into_body(), body_limit).await {
                    Ok(bytes) => bytes,
                    Err(_) => return AppError::bad_request(
                        "request body could not be read; only bounded JSON bodies are forwarded",
                    )
                    .into_response(),
                };
            match content_type.as_deref() {
                None => upstream.body(body),
                Some(kind) if forwardable_content_type(Some(kind)) => {
                    upstream.header(header::CONTENT_TYPE, kind).body(body)
                }
                Some(_) => return AppError::bad_request(
                    "only text media types (application/json, text/*) can be forwarded upstream",
                )
                .into_response(),
            }
        } else {
            upstream
        };

    let response = match upstream.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                endpoint = %name,
                %error,
                "external API proxy transport failure"
            );
            return transport_error("calling the upstream", &error).into_response();
        }
    };

    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    if !forwardable_content_type(content_type.as_ref().and_then(|value| value.to_str().ok())) {
        tracing::warn!(
            endpoint = %name,
            status = status.as_u16(),
            "external API proxy refused a non-text upstream media type"
        );
        return AppError::bad_gateway(
            "upstream answered with a media type this proxy does not forward",
        )
        .into_response();
    }

    let body = match read_capped(response, limit).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(
                endpoint = %name,
                status = status.as_u16(),
                "external API proxy refused an oversized or unreadable upstream answer"
            );
            return error.into_response();
        }
    };

    tracing::debug!(
        endpoint = %name,
        status = status.as_u16(),
        "external API proxy call"
    );

    // Pass-through: the browser sees the upstream's status and body,
    // while the outer middleware layers still add the security headers,
    // request id and metrics to this response.
    let mut response = Response::new(Body::from(body));
    *response.status_mut() =
        StatusCode::from_u16(status.as_u16()).expect("reqwest statuses are valid HTTP statuses");
    if let Some(kind) = content_type {
        response.headers_mut().insert(header::CONTENT_TYPE, kind);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::{encode_subpath, join_subpath, upstream_url};

    fn fixed(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn without_fixed_parameters_the_query_travels_untouched() {
        // The v0.12.0 contract, byte for byte.
        assert_eq!(
            upstream_url(
                "https://api.example.com/v1",
                Some("city=Madrid&units=metric"),
                &[],
            ),
            "https://api.example.com/v1?city=Madrid&units=metric"
        );
        assert_eq!(
            upstream_url("https://api.example.com/v1", None, &[]),
            "https://api.example.com/v1"
        );
        assert_eq!(
            upstream_url("https://api.example.com/v1", Some(""), &[]),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn fixed_parameters_are_appended_after_the_incoming_ones() {
        let key = fixed(&[("apikey", "omdb-secret-42")]);
        assert_eq!(
            upstream_url("https://www.omdbapi.com/", Some("t=Inception"), &key),
            "https://www.omdbapi.com/?t=Inception&apikey=omdb-secret-42"
        );
    }

    #[test]
    fn a_client_pair_cannot_shadow_a_fixed_one() {
        // The browser sends its own `apikey`; the proxy drops it and the
        // configured value is the only one upstream sees (the
        // `params.set()` semantics of the keyParam pattern).
        let key = fixed(&[("apikey", "omdb-secret-42")]);
        assert_eq!(
            upstream_url(
                "https://www.omdbapi.com/",
                Some("apikey=spoof&t=Inception"),
                &key,
            ),
            "https://www.omdbapi.com/?t=Inception&apikey=omdb-secret-42"
        );
    }

    #[test]
    fn fixed_parameters_travel_without_any_incoming_query() {
        let key = fixed(&[("key", "google-key")]);
        assert_eq!(
            upstream_url(
                "https://translation.googleapis.com/language/translate/v2",
                None,
                &key,
            ),
            "https://translation.googleapis.com/language/translate/v2?key=google-key"
        );
    }

    #[test]
    fn base_urls_that_already_carry_a_query_join_with_ampersand() {
        let fixed_pairs = fixed(&[("apikey", "k")]);
        assert_eq!(
            upstream_url(
                "https://api.example.com/v1?format=json",
                Some("q=x"),
                &fixed_pairs
            ),
            "https://api.example.com/v1?format=json&q=x&apikey=k"
        );
        assert_eq!(
            upstream_url("https://api.example.com/v1?format=json", None, &fixed_pairs),
            "https://api.example.com/v1?format=json&apikey=k"
        );
    }

    #[test]
    fn incoming_pairs_are_percent_encoded_on_the_way_out() {
        // Re-encoding is part of the deal for endpoints with fixed
        // parameters: parse-in, serialize-out keeps every pair
        // well-formed even when the browser sends raw characters.
        let key = fixed(&[("apikey", "k-42")]);
        assert_eq!(
            upstream_url("https://api.example.com/", Some("q=sea of monsters"), &key),
            "https://api.example.com/?q=sea+of+monsters&apikey=k-42"
        );
        // Secret values with URL metacharacters survive the round trip.
        let tricky = fixed(&[("token", "a&b=c d")]);
        assert_eq!(
            upstream_url("https://api.example.com/", None, &tricky),
            "https://api.example.com/?token=a%26b%3Dc+d"
        );
    }

    #[test]
    fn subpaths_join_with_exactly_one_slash() {
        // Whatever trailing slashes the base carries, the join inserts
        // exactly one `/` — the upstream never sees `//`.
        assert_eq!(
            join_subpath("https://api.example.com/v1", "search/deep"),
            "https://api.example.com/v1/search/deep"
        );
        assert_eq!(
            join_subpath("https://www.omdbapi.com/", "search"),
            "https://www.omdbapi.com/search"
        );
        assert_eq!(
            join_subpath("https://api.example.com/v1//", "a"),
            "https://api.example.com/v1/a"
        );
        // A query the base already carries stays at the end, after the
        // joined path.
        assert_eq!(
            join_subpath("https://api.example.com/v1?format=json", "data"),
            "https://api.example.com/v1/data?format=json"
        );
        // An empty subpath (a bare trailing `/`) is no subpath at all.
        assert_eq!(
            join_subpath("https://api.example.com/v1", ""),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn dot_segments_are_refused() {
        // `..` (and `.`) could try to rewrite the upstream path; the
        // proxy answers 400 instead of guessing the intent.
        assert!(encode_subpath("ok/..").is_err());
        assert!(encode_subpath("../ok").is_err());
        assert!(encode_subpath(".").is_err());
        assert!(encode_subpath("ok/./x").is_err());
    }

    #[test]
    fn subpaths_are_reencoded_to_a_canonical_form() {
        // The path extractor already decoded the client's `%XX`, so the
        // proxy encodes everything outside pchar back — a decoded `?`
        // stays part of the path (`%3F`), spaces and non-ASCII travel
        // percent-encoded, and a literal `%` becomes `%25` (never a
        // double-encoding pass-through).
        assert_eq!(encode_subpath("search").unwrap(), "search");
        assert_eq!(encode_subpath("a/b/c").unwrap(), "a/b/c");
        assert_eq!(encode_subpath("a//b").unwrap(), "a/b");
        assert_eq!(encode_subpath("term?x=1").unwrap(), "term%3Fx=1");
        assert_eq!(
            encode_subpath("caf\u{e9} au lait").unwrap(),
            "caf%C3%A9%20au%20lait"
        );
        assert_eq!(encode_subpath("50%").unwrap(), "50%25");
        assert_eq!(encode_subpath("a&b=c").unwrap(), "a&b=c");
    }

    #[test]
    fn control_characters_are_refused() {
        assert!(encode_subpath("bad\u{7}segment").is_err());
        assert!(encode_subpath("line\nbreak").is_err());
    }

    #[test]
    fn fixed_parameters_travel_with_a_joined_subpath() {
        // Subpath first, keyParam second: the pair composes.
        let key = fixed(&[("apikey", "omdb-secret-42")]);
        let base = join_subpath("https://www.omdbapi.com/", "search");
        assert_eq!(
            upstream_url(&base, Some("t=Alien"), &key),
            "https://www.omdbapi.com/search?t=Alien&apikey=omdb-secret-42"
        );
    }
}
