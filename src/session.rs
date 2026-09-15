//! Browser session cookie (the v0.7.0 `[auth]` session layer).
//!
//! The API contract is unchanged: clients keep exchanging
//! `Authorization: Bearer <token>` headers. On top of that, `login` and
//! `refresh` attach the freshly issued access token as an HTTP-only
//! cookie so **browsers** stay authenticated across normal navigation
//! (plain links, address bar, reloads) — the one thing a Bearer header
//! could never give a page visit.
//!
//! The cookie is a mirror of the access token, not a second credential:
//!
//! - name `wallermax_session`, value = the access token verbatim;
//! - `Path=/`, `Max-Age` = `[auth] token_ttl_secs`, `HttpOnly`,
//!   `SameSite=Strict`;
//! - `Domain` — only while `[auth] cookie_domain` names a parent
//!   domain (F19, the **shared session**): the cookie then travels
//!   across every host under it (the main site, the CMS host and the
//!   tenant sites of `app.localhost`/`example.com` alike), so one
//!   sign-in personalises them all — `public/index.jhs` greets
//!   `user.username` without a line of template code. While unset
//!   (the default) the cookie stays **host-only**, exactly as every
//!   phase before F19: whatever host set it is the only host that
//!   sees it. The value must be a parent domain of at least two
//!   labels covering **every** serving host — browsers refuse a
//!   `Domain` the serving host does not belong to, and a one-label
//!   domain is a public suffix to them (`Domain=localhost` from
//!   `cms.localhost` is silently refused — the failure mode that
//!   motivated F19's startup validation);
//! - `Secure` follows `[auth] secure_cookies` (v0.8.0): `auto` (the
//!   default) sets it only while `[tls]` is enabled. An unconditional
//!   `Secure` silently broke browser logins on plain-HTTP servers —
//!   browsers refuse to store `Secure` cookies on insecure origins,
//!   while curl and PowerShell are lax about it, which made the failure
//!   look like a server bug;
//! - verified with the exact same `JwtService::verify_token` path as
//!   the Bearer header (signature, expiry, issuer), and accepted
//!   wherever the header is — the `AuthUser`/`AdminUser` extractors and
//!   the `user` template global;
//! - `logout`/`logout_all` clear it, and `login`/`refresh` overwrite it
//!   (rotation included).
//!
//! `HttpOnly` keeps `document.cookie` away from the value,
//! `SameSite=Strict` stops the cookie from being attached to
//! cross-site requests (the CSRF posture — see `README.md`).
//!
//! The Bearer header, when present, always wins over the cookie, so
//! API-first clients and mixed tooling keep working unchanged.

use axum::http::{header, HeaderMap};

/// Name of the session cookie set by `login`/`refresh`.
pub const COOKIE_NAME: &str = "wallermax_session";

/// Builds the `Set-Cookie` value attaching `token` as the session.
///
/// `ttl_secs` mirrors `[auth] token_ttl_secs`: the cookie dies with the
/// access token it carries. `secure` controls the `Secure` attribute
/// (see `[auth] secure_cookies`). `domain` adds the `Domain` attribute
/// — the shared session of F19 — or keeps the cookie host-only while
/// `None`.
pub fn set_cookie_value(token: &str, ttl_secs: u64, secure: bool, domain: Option<&str>) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age={ttl_secs}; HttpOnly; SameSite=Strict{}{}",
        secure_part(secure),
        domain_part(domain),
    )
}

/// Builds the `Set-Cookie` value expiring the session cookie.
///
/// The `Domain` must match the one the cookie was set with — a browser
/// only overwrites a cookie when the replacement addresses it the same
/// way — so the clear carries whatever `[auth] cookie_domain`
/// configured, exactly like the set.
pub fn clear_cookie_value(secure: bool, domain: Option<&str>) -> String {
    format!(
        "{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}{}",
        secure_part(secure),
        domain_part(domain),
    )
}

/// The `Secure` fragment, with its leading separator, or nothing.
fn secure_part(secure: bool) -> &'static str {
    if secure {
        "; Secure"
    } else {
        ""
    }
}

/// The `Domain` fragment, with its leading separator, or nothing.
///
/// An empty or absent domain stays host-only — every phase before F19
/// behaved exactly so, and the empty string must not silently change
/// that.
fn domain_part(domain: Option<&str>) -> String {
    match domain.filter(|domain| !domain.is_empty()) {
        Some(domain) => format!("; Domain={domain}"),
        None => String::new(),
    }
}

/// Extracts the session token from a `Cookie` request header, if any.
///
/// Cookie parsing is deliberately minimal: the header is split on `;`,
/// pairs are trimmed, and the pair named exactly `wallermax_session`
/// with a non-empty value wins (first match across all `Cookie`
/// headers). No cookie jar, no attributes: wallermax only ever reads
/// this one name, and a full parser would be attack surface without
/// behaviour.
pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(raw) = value.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some((name, token)) = pair.split_once('=') {
                if name == COOKIE_NAME && !token.is_empty() {
                    return Some(token);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(cookie).expect("valid cookie header"),
        );
        headers
    }

    #[test]
    fn set_and_clear_cookie_values() {
        assert_eq!(
            set_cookie_value("tok", 3600, true, None),
            "wallermax_session=tok; Path=/; Max-Age=3600; HttpOnly; SameSite=Strict; Secure"
        );
        assert_eq!(
            set_cookie_value("tok", 3600, false, None),
            "wallermax_session=tok; Path=/; Max-Age=3600; HttpOnly; SameSite=Strict"
        );
        assert_eq!(
            clear_cookie_value(true, None),
            "wallermax_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict; Secure"
        );
        assert_eq!(
            clear_cookie_value(false, None),
            "wallermax_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict"
        );
    }

    #[test]
    fn a_shared_domain_rides_along_set_and_clear() {
        // The F19 shared session: the Domain attribute makes one
        // sign-in travel across every host under it.
        assert_eq!(
            set_cookie_value("tok", 3600, false, Some("localhost")),
            "wallermax_session=tok; Path=/; Max-Age=3600; HttpOnly; SameSite=Strict; Domain=localhost"
        );
        assert_eq!(
            set_cookie_value("tok", 3600, true, Some("example.com")),
            "wallermax_session=tok; Path=/; Max-Age=3600; HttpOnly; SameSite=Strict; Secure; Domain=example.com"
        );
        // The clear must address the cookie the same way, or the
        // browser keeps the old one.
        assert_eq!(
            clear_cookie_value(false, Some("localhost")),
            "wallermax_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict; Domain=localhost"
        );
    }

    #[test]
    fn an_empty_domain_stays_host_only() {
        // The empty string must behave exactly like None: host-only,
        // the pre-F19 default.
        assert_eq!(
            set_cookie_value("tok", 60, false, Some("")),
            set_cookie_value("tok", 60, false, None),
        );
        assert_eq!(
            clear_cookie_value(false, Some("")),
            clear_cookie_value(false, None),
        );
    }

    #[test]
    fn reads_the_session_cookie() {
        let headers = headers_with("theme=dark; wallermax_session=abc.def.ghi; other=1");
        assert_eq!(session_token(&headers), Some("abc.def.ghi"));
    }

    #[test]
    fn reads_the_only_cookie() {
        let headers = headers_with("wallermax_session=t");
        assert_eq!(session_token(&headers), Some("t"));
    }

    #[test]
    fn ignores_empty_values_and_prefix_traps() {
        let headers = headers_with("wallermax_session=; wallermax_sessionx=nope");
        assert_eq!(session_token(&headers), None);
    }

    #[test]
    fn missing_cookie_header_yields_none() {
        assert_eq!(session_token(&HeaderMap::new()), None);
    }

    #[test]
    fn scans_every_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("first=1; wallermax_session="),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("wallermax_session=second"),
        );
        assert_eq!(session_token(&headers), Some("second"));
    }
}
