//! Name-based virtual hosts (F14): one server, one IP, one port —
//! two names.
//!
//! While [`crate::config::CmsConfig::hosts`] is empty every surface
//! (static site, CMS, API) shares one host, exactly as before. While
//! it lists hostnames, the request's `Host` header decides which route
//! tree serves it: the listed names get the CMS — public pages, panel,
//! media, feeds, search and the `/api/auth/*` family the no-JS forms
//! post to — and every other host gets the static site under the
//! `[static]` root plus the operator machinery (`/api`, `/health`,
//! `/metrics`, the external proxy).
//!
//! The classification lives here as pure functions of the request so
//! the two places that need to agree — the templates middleware
//! (which decides whether `.jhs` files under the static root render,
//! whether the CMS default page takes over `/` and whether 404s
//! auto-route to `views/`) and the dispatcher in
//! [`crate::routes::vhost_routes`] (which picks the tree) — classify
//! every request identically by construction.
//!
//! Matching rules, kept deliberately boring:
//!
//! - HTTP/2 carries the host in the URI authority, HTTP/1.1 in the
//!   `Host` header; the authority wins when present.
//! - The port is stripped (`cms.example.com:8080` is
//!   `cms.example.com`), and so is any whitespace around the value.
//! - Comparison ignores case, the way DNS does; configured entries
//!   are lowercased at load time.
//! - A missing `Host` (an HTTP/1.0 relic) or a **duplicated** one
//!   (a request shaped like header smuggling) classifies as the main
//!   host — fail safe, never the CMS.
//! - An unknown host is not an error: it gets the main host, the way
//!   a friendly shared server answers any name that reaches it.
//!
//! The Host header is attacker-controlled input, but it only ever
//! *routes* here: no redirect, cache key or log line is built from it
//! verbatim, and the one redirect the panel owns (`GET /admin/theme`)
//! already validates its own `back` parameter.

use crate::config::CmsConfig;
use axum::http::{header, HeaderMap, Uri};

/// The bare host name inside a raw `Host`-header value (or URI
/// authority): lowercase-equivalent, port stripped.
///
/// IPv6 literals arrive bracketed (`[::1]:8080`) — the name keeps the
/// brackets, which simply never matches a configured CMS host.
pub(crate) fn host_name(raw: &str) -> &str {
    let raw = raw.trim();
    if let Some(end) = raw.find(']') {
        // `[::1]:8080` or `[::1]` — the name is the bracketed literal.
        &raw[..=end]
    } else {
        match raw.rsplit_once(':') {
            Some((name, _)) => name,
            None => raw,
        }
    }
}

/// The host a request arrived for: the URI authority (HTTP/2) when
/// present, else the single `Host` header (HTTP/1.1).
///
/// `None` means "classify as the main host": no header at all, a
/// non-UTF-8 value, or a duplicated `Host`.
pub(crate) fn request_host<'a>(uri: &'a Uri, headers: &'a HeaderMap) -> Option<&'a str> {
    if let Some(authority) = uri.authority() {
        return Some(authority.host());
    }
    let mut hosts = headers.get_all(header::HOST).iter();
    let (first, second) = (hosts.next(), hosts.next());
    match (first, second) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    }
}

/// Whether the request belongs to a configured CMS host.
pub(crate) fn is_cms_request(uri: &Uri, headers: &HeaderMap, cms_hosts: &[String]) -> bool {
    let Some(host) = request_host(uri, headers) else {
        return false;
    };
    let name = host_name(host);
    cms_hosts
        .iter()
        .any(|configured| configured.eq_ignore_ascii_case(name))
}

/// The CMS surface's origin for template links (F14 follow-up): the
/// value of the templates' `cms_origin` global.
///
/// Empty while `cms.hosts` is empty — the single-host server needs no
/// cross-host links, and `<?= cms_origin ?>/login` degrades to the
/// relative `/login` on the host the browser is already on. While
/// virtual hosts are on, the operator's `cms.site_url` wins when set
/// (the same key the sitemap and feeds trust for absolute URLs:
/// behind a reverse proxy the derived origin would be wrong, and
/// `site_url` is where the public truth is already declared), else
/// the origin is derived:
///
/// - the scheme comes from `[tls] enabled` (`https` while TLS serves
///   the socket, `http` otherwise);
/// - the host is the **first** `cms.hosts` entry — the canonical CMS
///   name, the one the operator lists first;
/// - the port is `[server] port`, and only when it is neither the
///   scheme's default (80/443) nor `0` (the OS-picked ephemeral port
///   of test boots — nobody publishes a link to it).
pub(crate) fn cms_origin(cms: &CmsConfig, tls_enabled: bool, port: u16) -> String {
    let Some(host) = cms.hosts.first() else {
        return String::new();
    };
    if let Some(site_url) = cms.site_url.as_deref() {
        return site_url.to_owned();
    }
    let scheme = if tls_enabled { "https" } else { "http" };
    let default_port = if tls_enabled { 443 } else { 80 };
    if port != 0 && port != default_port {
        format!("{scheme}://{host}:{port}")
    } else {
        format!("{scheme}://{host}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CmsConfig;
    use axum::http::HeaderValue;

    /// A `CmsConfig` with the given `site_url` and `hosts`.
    fn cms(site_url: Option<&str>, hosts: &[&str]) -> CmsConfig {
        CmsConfig {
            hosts: hosts.iter().map(|host| String::from(*host)).collect(),
            site_url: site_url.map(String::from),
            ..CmsConfig::default()
        }
    }

    fn request(uri: &str, host: Option<&str>) -> (Uri, HeaderMap) {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert(
                header::HOST,
                HeaderValue::from_str(host).expect("test host value"),
            );
        }
        (uri.parse().expect("test uri"), headers)
    }

    fn hosts() -> Vec<String> {
        vec![String::from("cms.test"), String::from("cms.example.com")]
    }

    #[test]
    fn host_names_lose_their_port() {
        assert_eq!(host_name("cms.test"), "cms.test");
        assert_eq!(host_name("cms.test:8080"), "cms.test");
        assert_eq!(host_name("cms.test:"), "cms.test");
        assert_eq!(host_name("  cms.test:8443  "), "cms.test");
    }

    #[test]
    fn ipv6_literals_stay_bracketed() {
        assert_eq!(host_name("[::1]:8080"), "[::1]");
        assert_eq!(host_name("[::1]"), "[::1]");
    }

    #[test]
    fn matching_ignores_case_and_port() {
        let (uri, headers) = request("/p", Some("CMS.TEST"));
        assert!(is_cms_request(&uri, &headers, &hosts()));
        let (uri, headers) = request("/p", Some("cms.test:9999"));
        assert!(is_cms_request(&uri, &headers, &hosts()));
        let (uri, headers) = request("/p", Some("Cms.Example.Com"));
        assert!(is_cms_request(&uri, &headers, &hosts()));
    }

    #[test]
    fn other_hosts_do_not_match() {
        let (uri, headers) = request("/p", Some("localhost"));
        assert!(!is_cms_request(&uri, &headers, &hosts()));
        let (uri, headers) = request("/p", Some("cms.test.evil.com"));
        assert!(!is_cms_request(&uri, &headers, &hosts()));
        let (uri, headers) = request("/p", Some("cms.test.."));
        assert!(!is_cms_request(&uri, &headers, &hosts()));
    }

    #[test]
    fn missing_host_is_the_main_host() {
        let (uri, headers) = request("/p", None);
        assert!(!is_cms_request(&uri, &headers, &hosts()));
    }

    #[test]
    fn duplicated_host_is_the_main_host() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::HOST,
            HeaderValue::from_str("cms.test").expect("first host"),
        );
        headers.append(
            header::HOST,
            HeaderValue::from_str("evil.test").expect("second host"),
        );
        let uri: Uri = "/p".parse().expect("test uri");
        assert!(!is_cms_request(&uri, &headers, &hosts()));
    }

    #[test]
    fn the_single_host_has_no_origin() {
        assert_eq!(cms_origin(&cms(None, &[]), false, 8080), "");
        // `site_url` is the feeds' key, not a link target, while every
        // surface shares one host — relative links are always right.
        assert_eq!(
            cms_origin(&cms(Some("https://www.example.com"), &[]), false, 8080),
            ""
        );
    }

    #[test]
    fn the_first_host_is_the_canonical_origin() {
        let config = cms(None, &["cms.example.com", "cms.example.net"]);
        assert_eq!(cms_origin(&config, false, 80), "http://cms.example.com");
        assert_eq!(cms_origin(&config, true, 443), "https://cms.example.com");
    }

    #[test]
    fn non_default_ports_tag_along() {
        let config = cms(None, &["cms.test"]);
        assert_eq!(cms_origin(&config, false, 8080), "http://cms.test:8080");
        assert_eq!(cms_origin(&config, true, 8443), "https://cms.test:8443");
    }

    #[test]
    fn the_ephemeral_port_is_omitted() {
        let config = cms(None, &["cms.test"]);
        assert_eq!(cms_origin(&config, false, 0), "http://cms.test");
        assert_eq!(cms_origin(&config, true, 0), "https://cms.test");
    }

    #[test]
    fn site_url_overrides_the_derived_origin() {
        let config = cms(
            Some("https://cms.example.com"),
            &["cms.example.com", "cms.example.net"],
        );
        assert_eq!(cms_origin(&config, false, 8080), "https://cms.example.com");
    }
}
