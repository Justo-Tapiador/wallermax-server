//! Name-based virtual hosts (F14), generalized per organization
//! (F17): one server, one IP, one port — as many names as the
//! `domains` table maps.
//!
//! F14 split the server in two by `Host` header (the static tree on
//! the main host, the CMS on its own names); F16 moved the split's
//! truth into the `domains` table. F17 completes the arc: every
//! mapped hostname resolves to its **organization**, and each
//! organization serves from its own `document_root`:
//!
//! - the **CMS organization** (`cms`) gets the visitor surface —
//!   public pages, `views/` auto-routing, the panel, the media
//!   library, search, feeds and the `/api/auth/*` family the no-JS
//!   forms post to;
//! - the **main organization** (`main`, and every host the table
//!   does not map — unknown or missing included, fail-safe) gets the
//!   static site under its document root plus the operator machinery
//!   (`/api`, `/health`, `/metrics`, the external proxy);
//! - **any other organization** gets a self-contained static site
//!   from its document root: the file tree, the directory indexes,
//!   the on-the-fly `.jhs` rendering — and nothing else.
//!
//! The classification lives here as pure functions of the request so
//! the two places that need to agree — the templates middleware
//! (which decides where `.jhs` files render from and whether 404s
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
//! - Comparison ignores case, the way DNS does; the table's rows are
//!   normalized to the canonical shape at load time.
//! - A missing `Host` (an HTTP/1.0 relic) or a **duplicated** one
//!   (a request shaped like header smuggling) classifies as the main
//!   host — fail safe, never a tenant.
//! - An unknown host is not an error: it gets the main host, the way
//!   a friendly shared server answers any name that reaches it.
//!
//! The Host header is attacker-controlled input, but it only ever
//! *routes* here: no redirect, cache key or log line is built from it
//! verbatim, and the one redirect the panel owns (`GET /admin/theme`)
//! already validates its own `back` parameter.

use crate::db::{CMS_ORGANIZATION_KEY, MAIN_ORGANIZATION_KEY};
use axum::http::{header, HeaderMap, Uri};

/// One row of the boot-time host resolution table (F17): a mapped
/// hostname, the organization it routes to, and that organization's
/// document root — the serving truth for whatever tree the host gets.
///
/// The rows load once at startup (the `domains` table joined to the
/// `organizations` rows on a database boot, the `[cms] hosts`
/// bootstrap otherwise) and are frozen afterwards, with the document
/// roots resolved to absolute paths — exactly the convention
/// `views_dir` and the static root already follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBinding {
    /// The mapped hostname in the table's canonical shape:
    /// lowercased, trimmed, without the DNS trailing dot — the same
    /// shape [`host_name`] extracts from a request, so the two
    /// compare equal no matter who wrote the row.
    pub hostname: String,
    /// The organization's stable key: `main`, `cms`, or any key a
    /// row in the `organizations` table carries.
    pub organization: String,
    /// The organization's document root, absolute (resolved once at
    /// boot).
    pub document_root: String,
}

impl HostBinding {
    /// The binding's document root as a path.
    pub fn root(&self) -> &std::path::Path {
        std::path::Path::new(&self.document_root)
    }
}

/// What a request's `Host` header resolves to (F17): the CMS tree,
/// the main tree, or a tenant organization's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostClass<'a> {
    /// A hostname mapped to the CMS organization: the visitor
    /// surface (public pages, panel, media, search, the auth forms
    /// and the shared stylesheets).
    Cms,
    /// The main tree: every host the table does not map — unknown or
    /// missing `Host` included, fail-safe — plus hostnames mapped to
    /// the `main` organization on purpose.
    Main,
    /// A hostname mapped to any other organization: that tenant's
    /// self-contained static tree, served from the organization's
    /// document root.
    Tenant(&'a HostBinding),
}

impl HostClass<'_> {
    /// The organization the class serves (F20): the tenant's key, or
    /// `None` for the two bootstrap classes (whose organizations the
    /// callers resolve by name — the CMS organization for content
    /// scope, the main organization for static roots).
    pub(crate) fn organization(&self) -> Option<&str> {
        match self {
            HostClass::Tenant(binding) => Some(&binding.organization),
            _ => None,
        }
    }
}

/// Resolves a request against the boot-time host bindings (F17).
///
/// This is the single pure function the dispatcher and the templates
/// middleware both classify with, so the two layers agree on every
/// request by construction — the F14 invariant, generalized from two
/// classes to one per organization.
pub(crate) fn classify<'a>(
    uri: &Uri,
    headers: &HeaderMap,
    bindings: &'a [HostBinding],
) -> HostClass<'a> {
    let Some(host) = request_host(uri, headers) else {
        return HostClass::Main;
    };
    let name = host_name(host);
    let Some(binding) = bindings
        .iter()
        .find(|binding| binding.hostname.eq_ignore_ascii_case(name))
    else {
        return HostClass::Main;
    };
    match binding.organization.as_str() {
        CMS_ORGANIZATION_KEY => HostClass::Cms,
        MAIN_ORGANIZATION_KEY => HostClass::Main,
        _ => HostClass::Tenant(binding),
    }
}

/// The organization a request's host resolves to, as a content scope
/// (F20): the mapped organization on a tenant host; the CMS
/// organization everywhere else — on the CMS host by definition, on
/// the single-host server because its one tree is the CMS surface,
/// and fail-safe on a main-host arrival, whose tree never mounts the
/// CMS family at all.
///
/// This is the one place the content plane learns which tenant a
/// request serves: the guards check the membership against it, the
/// handlers pass it to the repositories, and `base_data` scopes the
/// `pages`/`menus` template globals by it.
pub(crate) fn organization_key(uri: &Uri, headers: &HeaderMap, bindings: &[HostBinding]) -> String {
    match classify(uri, headers, bindings).organization() {
        Some(organization) => organization.to_owned(),
        None => CMS_ORGANIZATION_KEY.to_owned(),
    }
}

/// The bare host name inside a raw `Host`-header value (or URI
/// authority): lowercase-equivalent, port stripped.
///
/// IPv6 literals arrive bracketed (`[::1]:8080`) — the name keeps the
/// brackets, which simply never matches a configured host.
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

/// The CMS surface's origin for template links (F14 follow-up,
/// data-driven since F17): the value of the templates' `cms_origin`
/// global.
///
/// Empty while no CMS host is mapped — the single-host server needs
/// no cross-host links, and `<?= cms_origin ?>/login` degrades to the
/// relative `/login` on the host the browser is already on. While
/// hosts are mapped, the operator's `cms.site_url` wins when set
/// (the same key the sitemap and feeds trust for absolute URLs:
/// behind a reverse proxy the derived origin would be wrong, and
/// `site_url` is where the public truth is already declared), else
/// the origin is derived:
///
/// - the scheme comes from `[tls] enabled` (`https` while TLS serves
///   the socket, `http` otherwise);
/// - the host is the **first** mapped CMS host — the canonical CMS
///   name. On a database boot that is the domains table's insertion
///   order (the name the operator listed or inserted first); on the
///   configuration bootstrap, the first `[cms] hosts` entry;
/// - the port is `[server] port`, and only when it is neither the
///   scheme's default (80/443) nor `0` (the OS-picked ephemeral port
///   of test boots — nobody publishes a link to it).
///
/// `cms_hosts` is the mapped CMS host list in canonical order —
/// derived from the boot-time bindings, never read from the
/// configuration here.
pub(crate) fn cms_origin(
    site_url: Option<&str>,
    cms_hosts: &[&str],
    tls_enabled: bool,
    port: u16,
) -> String {
    let Some(&host) = cms_hosts.first() else {
        return String::new();
    };
    if let Some(site_url) = site_url {
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
    use axum::http::HeaderValue;

    /// A binding for `hostname` routed at `organization`, with a
    /// predictable document root.
    fn binding(hostname: &str, organization: &str) -> HostBinding {
        HostBinding {
            hostname: String::from(hostname),
            organization: String::from(organization),
            document_root: format!("sites/{organization}"),
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

    /// The boot table these tests classify against: two CMS hosts
    /// and one tenant organization's host.
    fn bindings() -> Vec<HostBinding> {
        vec![
            binding("cms.test", "cms"),
            binding("cms.example.com", "cms"),
            binding("shop.example.com", "shop"),
        ]
    }

    fn class_of<'a>(uri: &str, host: Option<&str>, bindings: &'a [HostBinding]) -> HostClass<'a> {
        let (uri, headers) = request(uri, host);
        classify(&uri, &headers, bindings)
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
        let bindings = bindings();
        assert_eq!(class_of("/p", Some("CMS.TEST"), &bindings), HostClass::Cms);
        assert_eq!(
            class_of("/p", Some("cms.test:9999"), &bindings),
            HostClass::Cms
        );
        assert_eq!(
            class_of("/p", Some("Cms.Example.Com"), &bindings),
            HostClass::Cms
        );
    }

    #[test]
    fn other_hosts_do_not_match() {
        let bindings = bindings();
        assert_eq!(
            class_of("/p", Some("localhost"), &bindings),
            HostClass::Main
        );
        assert_eq!(
            class_of("/p", Some("cms.test.evil.com"), &bindings),
            HostClass::Main
        );
        assert_eq!(
            class_of("/p", Some("cms.test.."), &bindings),
            HostClass::Main
        );
    }

    #[test]
    fn missing_host_is_the_main_host() {
        let bindings = bindings();
        assert_eq!(class_of("/p", None, &bindings), HostClass::Main);
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
        assert_eq!(classify(&uri, &headers, &bindings()), HostClass::Main);
    }

    #[test]
    fn a_host_mapped_to_the_main_organization_is_the_main_tree() {
        let bindings = vec![binding("explicit.test", "main")];
        assert_eq!(
            class_of("/", Some("explicit.test"), &bindings),
            HostClass::Main
        );
    }

    #[test]
    fn tenant_hosts_carry_their_binding() {
        let bindings = bindings();
        let shop = binding("shop.example.com", "shop");
        assert_eq!(
            class_of("/", Some("SHOP.example.com:8080"), &bindings),
            HostClass::Tenant(&shop),
            "the class carries the row that matched, root included"
        );
    }

    #[test]
    fn the_content_scope_follows_the_host() {
        // F20: the organization key a request's content belongs to —
        // the tenant's on a tenant host, the CMS organization
        // everywhere else (the CMS host, the single-host server, and
        // fail-safe on the main host).
        let bindings = bindings();
        let scope = |host: Option<&str>| {
            let (uri, headers) = request("/p", host);
            organization_key(&uri, &headers, &bindings)
        };
        assert_eq!(scope(Some("cms.test")), "cms");
        assert_eq!(scope(Some("cms.test:8443")), "cms");
        assert_eq!(scope(Some("shop.example.com")), "shop");
        assert_eq!(scope(Some("localhost")), "cms", "unknown hosts fail safe");
        assert_eq!(scope(None), "cms", "no Host at all fails safe");
    }

    #[test]
    fn the_single_host_server_scopes_to_the_cms_organization() {
        // No bindings mapped: the one tree is the whole server, and
        // its content is the CMS organization's — the pre-F20
        // behaviour, unchanged.
        let (uri, headers) = request("/p", Some("anything.test"));
        assert_eq!(organization_key(&uri, &headers, &[]), "cms");
    }

    #[test]
    fn the_single_host_has_no_origin() {
        assert_eq!(cms_origin(None, &[], false, 8080), "");
        // `site_url` is the feeds' key, not a link target, while every
        // surface shares one host — relative links are always right.
        assert_eq!(
            cms_origin(Some("https://www.example.com"), &[], false, 8080),
            ""
        );
    }

    #[test]
    fn the_first_host_is_the_canonical_origin() {
        let hosts = ["cms.example.com", "cms.example.net"];
        assert_eq!(
            cms_origin(None, &hosts, false, 80),
            "http://cms.example.com"
        );
        assert_eq!(
            cms_origin(None, &hosts, true, 443),
            "https://cms.example.com"
        );
    }

    #[test]
    fn non_default_ports_tag_along() {
        let hosts = ["cms.test"];
        assert_eq!(
            cms_origin(None, &hosts, false, 8080),
            "http://cms.test:8080"
        );
        assert_eq!(
            cms_origin(None, &hosts, true, 8443),
            "https://cms.test:8443"
        );
    }

    #[test]
    fn the_ephemeral_port_is_omitted() {
        let hosts = ["cms.test"];
        assert_eq!(cms_origin(None, &hosts, false, 0), "http://cms.test");
        assert_eq!(cms_origin(None, &hosts, true, 0), "https://cms.test");
    }

    #[test]
    fn site_url_overrides_the_derived_origin() {
        let hosts = ["cms.example.com", "cms.example.net"];
        assert_eq!(
            cms_origin(Some("https://cms.example.com"), &hosts, false, 8080),
            "https://cms.example.com"
        );
    }
}
