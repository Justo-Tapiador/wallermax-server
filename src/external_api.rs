//! Server-side proxy for external APIs (the `[external_api]` section,
//! v0.12.0).
//!
//! Browsers cannot keep a secret: any API key shipped to a page is public
//! the moment the HTML arrives, and cross-origin `fetch` calls are also
//! bound by the *target's* CORS policy. The proxy solves both problems at
//! once: pages call a **named** endpoint on this server
//! (`GET/POST /api/ext/{name}`, see [`crate::routes::external_api`]) and
//! this module forwards the request upstream, injecting the configured
//! secret headers that never leave the server.
//!
//! | Responsibility                              | Where                        |
//! |---------------------------------------------|------------------------------|
//! | Static validation (names, URLs, limits)     | [`crate::config`] `validate` |
//! | `${ENV}` interpolation + header resolution  | [`ExternalApi::resolve`]     |
//! | Request forwarding + response caps          | [`crate::routes::external_api`] |
//!
//! Security shape, deliberately narrow:
//!
//! - **SSRF-safe by construction**: the client picks an endpoint *name*,
//!   never a URL — every upstream is an operator-configured address.
//! - **Header hygiene**: nothing from the incoming request (cookies,
//!   authorization, arbitrary headers) is forwarded upstream; only the
//!   configured headers plus a `User-Agent` and `Accept` travel.
//! - **Secrets stay in strings only as long as needed**: values are
//!   resolved once at startup and never logged.
//!
//! Header values and query parameter values may reference the
//! environment with `${VAR_NAME}` (the committed `wallermax.toml` can
//! therefore ship placeholders; real keys live in the git-ignored
//! `wallermax.local.toml` or in the process environment). A missing or
//! invalid variable is a startup error, not a runtime surprise.
//!
//! Query parameters (the `query` map) are the `keyParam` pattern for
//! upstreams that want the key in the URL instead of a header (OMDb's
//! `apikey`, Google's `key`, ...). They are resolved once at startup
//! like header values, and a configured name always replaces the same
//! name arriving from the browser (see
//! [`crate::routes::external_api`]).

use std::collections::HashMap;

use axum::http::{HeaderName, HeaderValue, Method};

use crate::config::ExternalApiConfig;

/// Header names the operator cannot set, because the HTTP client owns
/// them (or forwarding them would leak request-scoped state upstream).
/// Shared with `config` so startup validation and runtime resolution
/// enforce the exact same list.
pub(crate) const RESERVED_HEADER_NAMES: [&str; 5] = [
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "cookie",
];

/// A fully resolved upstream: configuration plus startup-expanded header
/// and query values, ready to serve requests without touching the
/// environment again.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    /// Upstream URL exactly as configured (query forwarding is appended
    /// per request).
    pub url: String,
    /// Whether requests require an authenticated user (Bearer token or
    /// session cookie).
    pub auth_required: bool,
    /// Headers to attach to every upstream call, in configuration order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Fixed query parameters appended to every upstream call, in
    /// configuration (alphabetical) order. Names here win over the same
    /// names arriving from the browser.
    pub query: Vec<(String, String)>,
}

/// The shared proxy state: one connection-pooled client and the endpoint
/// table, built once at startup (see [`ExternalApi::resolve`]).
///
/// An instance with no endpoints is "disabled": the route family is not
/// even mounted, and lookups answer `None`.
#[derive(Debug, Clone)]
pub struct ExternalApi {
    /// Connection-pooled client (`None` only while disabled — kept out of
    /// `disabled()` instances so an unused proxy costs no runtime).
    client: Option<reqwest::Client>,
    /// Endpoint table keyed by configured name.
    endpoints: HashMap<String, ResolvedEndpoint>,
    /// Maximum forwarded upstream body size in bytes.
    response_limit_bytes: usize,
}

impl ExternalApi {
    /// Resolves the whole `[external_api]` section: expands `${ENV}`
    /// references in header and query values, validates the expanded
    /// header values as HTTP header values and builds the shared client.
    ///
    /// # Errors
    ///
    /// Fails on the first header value whose `${VAR}` is not set, whose
    /// expansion is not a valid header value, or whose name is reserved;
    /// and on the first query value whose `${VAR}` is not set. Startup
    /// treats this as fatal; [`crate::state`] treats it as "feature
    /// disabled" with an error log for programmatically built states that
    /// skipped [`crate::config::AppConfig::load`] validation.
    pub fn resolve(config: &ExternalApiConfig) -> Result<Self, String> {
        if config.endpoints.is_empty() {
            return Ok(Self::disabled());
        }

        let client = reqwest::Client::builder()
            .user_agent(format!("wallermax-server/{}", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|error| format!("external API proxy client could not be built: {error}"))?;

        let mut endpoints = HashMap::with_capacity(config.endpoints.len());
        for endpoint in &config.endpoints {
            let mut headers = Vec::with_capacity(endpoint.headers.len());
            for (name, value) in &endpoint.headers {
                let lower = name.to_ascii_lowercase();
                if RESERVED_HEADER_NAMES.contains(&lower.as_str()) {
                    return Err(format!(
                        "endpoint `{}` sets reserved header `{name}`; the HTTP client owns it",
                        endpoint.name
                    ));
                }
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    format!(
                        "endpoint `{}` has invalid header name `{name}`",
                        endpoint.name
                    )
                })?;
                let expanded = expand_env(value).map_err(|error| {
                    format!("endpoint `{}` header `{name}`: {error}", endpoint.name)
                })?;
                let value = HeaderValue::from_str(&expanded).map_err(|_| {
                    format!(
                        "endpoint `{}` header `{name}` expands to an invalid header value \
                         (control characters or newlines are not allowed)",
                        endpoint.name
                    )
                })?;
                headers.push((name, value));
            }
            let mut query = Vec::with_capacity(endpoint.query.len());
            for (name, value) in &endpoint.query {
                let expanded = expand_env(value).map_err(|error| {
                    format!(
                        "endpoint `{}` query parameter `{name}`: {error}",
                        endpoint.name
                    )
                })?;
                // No further validation: values are percent-encoded when
                // the query string is serialized per request, so any
                // string is safe to carry (there is no header-injection
                // surface to guard).
                query.push((name.clone(), expanded));
            }
            endpoints.insert(
                endpoint.name.clone(),
                ResolvedEndpoint {
                    url: endpoint.url.clone(),
                    auth_required: endpoint.auth_required,
                    headers,
                    query,
                },
            );
        }

        Ok(Self {
            client: Some(client),
            endpoints,
            response_limit_bytes: config.response_limit_bytes,
        })
    }

    /// An empty, route-less instance (nothing mounted, lookups miss).
    pub fn disabled() -> Self {
        Self {
            client: None,
            endpoints: HashMap::new(),
            response_limit_bytes: 0,
        }
    }

    /// Whether any endpoint is configured (routes mount on `true`).
    pub fn enabled(&self) -> bool {
        !self.endpoints.is_empty()
    }

    /// The endpoint configured under `name`, if any.
    pub fn endpoint(&self, name: &str) -> Option<&ResolvedEndpoint> {
        self.endpoints.get(name)
    }

    /// The shared client (`None` only while disabled).
    pub fn client(&self) -> Option<&reqwest::Client> {
        self.client.as_ref()
    }

    /// Maximum forwarded upstream body size in bytes.
    pub fn response_limit_bytes(&self) -> usize {
        self.response_limit_bytes
    }

    /// The upstream HTTP method for an incoming request (the proxy is
    /// pass-through: browsers `GET`/`POST`, the upstream sees the same
    /// method).
    pub fn upstream_method(&self, incoming: &Method) -> Method {
        incoming.clone()
    }
}

/// Expands every `${VAR_NAME}` reference in `value` from the environment.
///
/// `${` without a closing `}`, an empty or non-identifier variable name,
/// or a variable that is not set are all errors — the caller embeds the
/// endpoint name in the message, so a broken secret fails startup with a
/// precise pointer instead of failing requests at runtime.
fn expand_env(value: &str) -> Result<String, String> {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("`{value}` opens `${{'` without a closing `}}`"))?;
        let name = &after[..end];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "`{value}` references `{name}`, which is not a valid environment variable name"
            ));
        }
        let resolved = std::env::var(name).map_err(|_| {
            format!("environment variable `{name}` is not set (referenced by `{value}`)")
        })?;
        expanded.push_str(&resolved);
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint_config(name: &str, headers: &[(&str, &str)]) -> ExternalApiConfig {
        endpoint_config_with_query(name, headers, &[])
    }

    fn endpoint_config_with_query(
        name: &str,
        headers: &[(&str, &str)],
        query: &[(&str, &str)],
    ) -> ExternalApiConfig {
        let mut config = ExternalApiConfig::default();
        config
            .endpoints
            .push(crate::config::ExternalEndpointConfig {
                name: name.to_owned(),
                url: String::from("https://api.example.com/v1"),
                auth_required: false,
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                query: query
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            });
        config
    }

    fn set_var(name: &str, value: &str) {
        // SAFETY-equivalent for tests: single-threaded within each test;
        // distinct variable names per test avoid cross-test interference.
        std::env::set_var(name, value);
    }

    fn remove_var(name: &str) {
        std::env::remove_var(name);
    }

    #[test]
    fn expand_env_passes_literals_through() {
        assert_eq!(expand_env("Bearer abc123").unwrap(), "Bearer abc123");
        assert_eq!(expand_env("").unwrap(), "");
    }

    #[test]
    fn expand_env_substitutes_variables() {
        set_var("WMS_F6_PLAIN", "tok-42");
        assert_eq!(
            expand_env("Bearer ${WMS_F6_PLAIN}").unwrap(),
            "Bearer tok-42"
        );
        remove_var("WMS_F6_PLAIN");
    }

    #[test]
    fn expand_env_rejects_missing_variable() {
        remove_var("WMS_F6_MISSING");
        let error = expand_env("${WMS_F6_MISSING}").unwrap_err();
        assert!(
            error.contains("WMS_F6_MISSING"),
            "error names the variable: {error}"
        );
    }

    #[test]
    fn expand_env_rejects_unterminated_reference() {
        assert!(expand_env("${WMS_F6_OPEN").is_err());
    }

    #[test]
    fn expand_env_rejects_invalid_names() {
        assert!(expand_env("${}").is_err());
        assert!(expand_env("${bad-name}").is_err());
    }

    #[test]
    fn resolve_builds_endpoints_and_expands_values() {
        set_var("WMS_F6_RESOLVE", "secret-value");
        let config = endpoint_config("svc", &[("X-Api-Key", "${WMS_F6_RESOLVE}")]);
        let api = ExternalApi::resolve(&config).expect("resolves");
        assert!(api.enabled());
        assert!(api.client().is_some());
        let endpoint = api.endpoint("svc").expect("endpoint present");
        assert_eq!(endpoint.url, "https://api.example.com/v1");
        assert_eq!(endpoint.headers.len(), 1);
        assert_eq!(endpoint.headers[0].0.as_str(), "x-api-key");
        assert_eq!(endpoint.headers[0].1.to_str().unwrap(), "secret-value");
        remove_var("WMS_F6_RESOLVE");
    }

    #[test]
    fn resolve_fails_on_missing_env_variable() {
        remove_var("WMS_F6_ABSENT");
        let config = endpoint_config("svc", &[("X-Api-Key", "${WMS_F6_ABSENT}")]);
        let error = ExternalApi::resolve(&config).unwrap_err();
        assert!(
            error.contains("WMS_F6_ABSENT"),
            "error names the variable: {error}"
        );
    }

    #[test]
    fn resolve_fails_on_reserved_header() {
        let config = endpoint_config("svc", &[("Cookie", "session=1")]);
        assert!(ExternalApi::resolve(&config)
            .unwrap_err()
            .contains("reserved"));
    }

    #[test]
    fn resolve_fails_on_invalid_expanded_value() {
        set_var("WMS_F6_NEWLINE", "line-one\nline-two");
        let config = endpoint_config("svc", &[("X-Api-Key", "${WMS_F6_NEWLINE}")]);
        assert!(ExternalApi::resolve(&config)
            .unwrap_err()
            .contains("invalid header value"));
        remove_var("WMS_F6_NEWLINE");
    }

    #[test]
    fn disabled_instance_is_inert() {
        let api = ExternalApi::disabled();
        assert!(!api.enabled());
        assert!(api.endpoint("anything").is_none());
        assert!(api.client().is_none());
        let empty = ExternalApiConfig::default();
        assert!(!ExternalApi::resolve(&empty).unwrap().enabled());
    }

    #[test]
    fn resolve_expands_query_values_in_keyparam_shape() {
        set_var("WMS_F6_QKEY", "omdb-secret-42");
        let config = endpoint_config_with_query("omdb", &[], &[("apikey", "${WMS_F6_QKEY}")]);
        let api = ExternalApi::resolve(&config).expect("resolves");
        let endpoint = api.endpoint("omdb").expect("endpoint present");
        assert_eq!(
            endpoint.query,
            vec![(String::from("apikey"), String::from("omdb-secret-42"))]
        );
        remove_var("WMS_F6_QKEY");
    }

    #[test]
    fn resolve_fails_on_missing_query_env_variable() {
        remove_var("WMS_F6_QMISSING");
        let config = endpoint_config_with_query("svc", &[], &[("key", "${WMS_F6_QMISSING}")]);
        let error = ExternalApi::resolve(&config).unwrap_err();
        assert!(
            error.contains("WMS_F6_QMISSING") && error.contains("query parameter"),
            "error names the variable and the parameter: {error}"
        );
    }

    #[test]
    fn endpoints_without_query_resolve_to_an_empty_map() {
        let config = endpoint_config("svc", &[("X-Api-Key", "literal")]);
        let api = ExternalApi::resolve(&config).expect("resolves");
        let endpoint = api.endpoint("svc").expect("endpoint present");
        assert!(endpoint.query.is_empty());
    }
}
