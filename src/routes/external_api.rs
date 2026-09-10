//! External API proxy (the `[external_api]` section, v0.12.0).
//!
//! `GET/POST /api/ext/{name}` forwards to a **named, operator-configured**
//! upstream (see [`crate::external_api`]) and passes the answer back to
//! the browser. The point is secret hygiene: pages never see the API
//! keys, and the upstream's CORS policy stops mattering because the
//! browser only ever talks to this origin.
//!
//! Rules the handler enforces on every call:
//!
//! - the client picks a *name*, never a URL — no SSRF surface;
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
    Router::new().route("/api/ext/{name}", get(forward).post(forward))
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
/// See the module docs for the full rule list; the method is passed
/// through as-is (browsers `GET` or `POST`, the upstream sees the same).
#[allow(clippy::too_many_lines)]
async fn forward(
    State(state): State<AppState>,
    Path(name): Path<String>,
    user: Result<AuthUser, Rejection>,
    request: Request,
) -> Response {
    let api = state.external_api();
    let Some(endpoint) = api.endpoint(&name) else {
        let path = format!("/api/ext/{name}");
        return AppError::not_found(request.method().as_str(), &path).into_response();
    };
    let Some(client) = api.client() else {
        // Unreachable while any endpoint exists; the map and the client
        // are built together.
        let path = format!("/api/ext/{name}");
        return AppError::not_found(request.method().as_str(), &path).into_response();
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

    let url = upstream_url(&endpoint.url, query.as_deref(), &endpoint.query);
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
    use super::upstream_url;

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
}
