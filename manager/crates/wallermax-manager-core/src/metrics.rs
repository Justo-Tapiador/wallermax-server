//! Prometheus exposition parsing and a minimal blocking HTTP GET — the
//! dashboard's data plane.
//!
//! The manager only ever talks to the **local** server, so a hand-rolled
//! `HTTP/1.1` client over `TcpStream` is enough: no TLS stack, no async
//! runtime, no dependency to keep in sync with the server. What it lacks
//! in generality it repays in robustness — nothing here can fail on an
//! OpenSSL upgrade.
//!
//! Parsing keeps every family the endpoint exposes (including the ones
//! today's dashboard does not show yet); the typed accessors simply answer
//! `None` for families the server does not emit — **unknown families are
//! ignored**, never fatal.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::Serialize;

/// `wallermax_requests_total` — labelled `method`, `code`.
pub const REQUESTS_TOTAL: &str = "wallermax_requests_total";
/// `wallermax_rate_limited_requests_total`.
pub const RATE_LIMITED_TOTAL: &str = "wallermax_rate_limited_requests_total";
/// `wallermax_uptime_seconds`.
pub const UPTIME_SECONDS: &str = "wallermax_uptime_seconds";
/// `wallermax_registered_users` — present only while auth is enabled.
pub const REGISTERED_USERS: &str = "wallermax_registered_users";

/// One parsed sample line: `name{label="value"} 12.5`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// One metric family and the samples seen for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MetricFamily {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub samples: Vec<Sample>,
}

/// A whole `/metrics` answer, indexed by family name.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MetricsSnapshot {
    pub families: BTreeMap<String, MetricFamily>,
}

impl MetricsSnapshot {
    /// Parses the Prometheus text exposition format (the subset the
    /// server emits): `# HELP`/`# TYPE` comments, sample lines with
    /// optional label sets, `NaN`/`+Inf`/`-Inf` values. Histogram
    /// `_bucket`/`_sum`/`_count` lines are ordinary families; anything
    /// unparseable on a line is skipped, the way scrapers tolerate
    /// partial answers.
    pub fn parse(text: &str) -> Self {
        let mut snapshot = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("# ") {
                if let Some(help) = rest.strip_prefix("HELP ") {
                    if let Some((name, doc)) = help.split_once(' ') {
                        snapshot.families.entry(name.to_owned()).or_default().help =
                            Some(doc.trim().to_owned());
                    }
                }
                continue;
            }
            match split_sample(line) {
                Some((name, labels, raw_value)) => {
                    let Some(value) = raw_value.parse::<f64>().ok() else {
                        continue;
                    };
                    let family = snapshot.families.entry(name.to_owned()).or_default();
                    family.samples.push(Sample {
                        labels: parse_labels(labels),
                        value,
                    });
                }
                None => continue,
            }
        }
        snapshot
    }

    /// The value of the family's single unlabeled sample.
    pub fn value(&self, family: &str) -> Option<f64> {
        let samples = &self.families.get(family)?.samples;
        match samples.first() {
            Some(sample) if sample.labels.is_empty() => Some(sample.value),
            _ => None,
        }
    }

    /// The value of the sample whose `label` equals `wanted`.
    pub fn value_labeled(&self, family: &str, label: &str, wanted: &str) -> Option<f64> {
        self.families
            .get(family)?
            .samples
            .iter()
            .find_map(|sample| {
                (sample.labels.get(label).map(String::as_str) == Some(wanted))
                    .then_some(sample.value)
            })
    }

    /// The sum over every sample of the family — how labelled counters
    /// such as `wallermax_requests_total` are totalled.
    pub fn sum(&self, family: &str) -> Option<f64> {
        let samples = &self.families.get(family)?.samples;
        (!samples.is_empty()).then(|| samples.iter().map(|sample| sample.value).sum())
    }

    /// `wallermax_uptime_seconds`.
    pub fn uptime_seconds(&self) -> Option<f64> {
        self.value(UPTIME_SECONDS)
    }

    /// `wallermax_requests_total`, summed over methods and status codes.
    pub fn requests_total(&self) -> Option<f64> {
        self.sum(REQUESTS_TOTAL)
    }

    /// `wallermax_rate_limited_requests_total`.
    pub fn rate_limited_total(&self) -> Option<f64> {
        self.value(RATE_LIMITED_TOTAL)
    }

    /// `wallermax_registered_users`.
    pub fn registered_users(&self) -> Option<f64> {
        self.value(REGISTERED_USERS)
    }
}

/// Splits `name{labels} value` into its three parts. The label block is
/// scanned quote-aware so commas and spaces inside values do not confuse
/// the split; a trailing timestamp is dropped with the rest of the line.
fn split_sample(line: &str) -> Option<(&str, Option<&str>, &str)> {
    let bytes = line.as_bytes();
    let mut cursor = 0;
    // Metric name: [a-zA-Z_:][a-zA-Z0-9_:]*
    if bytes.is_empty() || !name_byte(bytes[0], true) {
        return None;
    }
    while cursor < bytes.len() && name_byte(bytes[cursor], false) {
        cursor += 1;
    }
    let name = &line[..cursor];

    let mut labels = None;
    if cursor < bytes.len() && bytes[cursor] == b'{' {
        let start = cursor + 1;
        let mut depth = cursor;
        let mut in_quotes = false;
        let mut escaped = false;
        while depth < bytes.len() {
            let byte = bytes[depth];
            if escaped {
                escaped = false;
            } else if byte == b'\\' && in_quotes {
                escaped = true;
            } else if byte == b'"' {
                in_quotes = !in_quotes;
            } else if byte == b'}' && !in_quotes {
                labels = Some(&line[start..depth]);
                depth += 1;
                cursor = depth;
                break;
            }
            depth += 1;
        }
        labels?; // unterminated label block
    }

    let rest = line[cursor..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let value = rest
        .split_ascii_whitespace()
        .next()
        .expect("a non-empty split always yields a first token");
    Some((name, labels, value))
}

/// Whether `byte` may appear at the given position of a metric name.
const fn name_byte(byte: u8, first: bool) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b':' || (!first && byte.is_ascii_digit())
}

/// Parses `key="value", other="v"` into a map, unescaping `\\`, `\"` and
/// `\n` inside values.
fn parse_labels(text: Option<&str>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    let Some(text) = text else {
        return labels;
    };
    for pair in text.split(',') {
        let Some((key, raw)) = pair.split_once('=') else {
            continue;
        };
        let raw = raw.trim();
        let Some(unquoted) = raw
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
        else {
            continue;
        };
        let mut value = String::with_capacity(unquoted.len());
        let mut chars = unquoted.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some(other) => value.push(other),
                    None => value.push('\\'),
                }
            } else {
                value.push(ch);
            }
        }
        labels.insert(key.trim().to_owned(), value);
    }
    labels
}

/// Performs a blocking `GET` against `url` with a hard `timeout`, and
/// returns the body of a 2xx answer. Only `http://` is supported — the
/// manager talks to the local server; an `https://` origin is refused
/// with a pointer at the limitation rather than failing mysteriously.
///
/// # Errors
///
/// A human-readable message for URL problems, connection failures,
/// timeouts, non-2xx statuses and truncated answers.
pub fn http_get_text(url: &str, timeout: Duration) -> Result<String, String> {
    let (authority, path) = split_url(url)?;
    let mut stream =
        connect(&authority).map_err(|error| format!("could not reach {url}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("could not arm the read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("could not arm the write timeout: {error}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: wallermax-manager\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("could not write to {url}: {error}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("could not read from {url}: {error}"))?;
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("truncated answer from {url}"))?;

    let status_line = head.lines().next().unwrap_or_default();
    let status = status_line
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line from {url}: {status_line:?}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("GET {url} answered {status}"));
    }

    let chunked = head.lines().skip(1).any(|header| {
        header
            .to_ascii_lowercase()
            .starts_with("transfer-encoding:")
            && header.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    };
    Ok(body)
}

/// Splits `http://host:port/path` into `(host:port, /path)`.
fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        format!(
            "the manager only speaks plain HTTP to the local server; `{url}` is not an `http://` URL"
        )
    })?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::from("/")),
    };
    if authority.is_empty() {
        return Err(format!("no host in `{url}`"));
    }
    Ok((authority.to_owned(), path))
}

/// Splits an authority into its host and port, defaulting the port to
/// 80.
///
/// Understands every shape an `http://` URL can carry: `host`,
/// `host:port`, bracketed IPv6 (`[::1]:8080`) and bare IPv6 (`::1` —
/// the colons belong to the address, they are not separators).
fn host_port(authority: &str) -> Result<(&str, u16), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: `[::1]` or `[::1]:8080`.
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("unterminated IPv6 address in `{authority}`"))?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port,
            None if tail.is_empty() => "",
            None => {
                return Err(format!(
                    "garbage after the IPv6 address in `{authority}`: `{tail}`"
                ))
            }
        };
        (host, port)
    } else if authority.matches(':').count() > 1 {
        // Bare IPv6: every colon belongs to the address itself.
        (authority, "")
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, port),
            None => (authority, ""),
        }
    };
    let port = if port.is_empty() {
        80
    } else {
        port.parse::<u16>()
            .map_err(|_| format!("invalid port in `{authority}`"))?
    };
    Ok((host, port))
}

/// Resolves `host:port` (defaulting port 80) to every address it names.
///
/// The host and the port are resolved **as a pair**: resolving the bare
/// string only works when the port is spelled out, so an origin like
/// `http://localhost` used to die with `invalid socket address` no
/// matter whether the server was running — the bug behind a dashboard
/// banner that no amount of Refresh ever cleared.
fn resolve_all(authority: &str) -> Result<Vec<SocketAddr>, String> {
    let (host, port) = host_port(authority)?;
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve `{authority}`: {error}"))?
        .collect();
    if addresses.is_empty() {
        return Err(format!("no address answered for `{authority}`"));
    }
    Ok(addresses)
}

/// Connects to the first address `authority` names that accepts the
/// connection. `localhost` can name both `::1` and `127.0.0.1`, and a
/// server that binds only one of them still answers.
fn connect(authority: &str) -> Result<TcpStream, String> {
    let addresses = resolve_all(authority)?;
    let mut last = String::new();
    for address in &addresses {
        match TcpStream::connect_timeout(address, Duration::from_secs(2)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = format!("{address}: {error}"),
        }
    }
    Err(format!(
        "every address of `{authority}` refused the connection — last tried {last}"
    ))
}

/// Reassembles a chunked body. Tolerates a trailing truncated chunk (the
/// connection closed early) by returning what was decoded so far.
fn dechunk(body: &str) -> String {
    let mut decoded = String::new();
    let mut rest = body;
    while let Some((size_line, remainder)) = rest.split_once('\n') {
        let Ok(size) = usize::from_str_radix(size_line.trim(), 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let cut = remainder.len().min(size);
        decoded.push_str(&remainder[..cut]);
        // Skip the chunk terminator: CRLF per the standard, a bare LF
        // tolerated; anything else means a truncated answer — keep what
        // we have.
        let tail = &remainder[cut..];
        rest = match tail
            .strip_prefix("\r\n")
            .or_else(|| tail.strip_prefix('\n'))
        {
            Some(tail) => tail,
            None => break,
        };
    }
    decoded
}

/// Fetches and parses `url` in one step.
///
/// # Errors
///
/// See [`http_get_text`].
pub fn fetch_metrics(url: &str, timeout: Duration) -> Result<MetricsSnapshot, String> {
    let text = http_get_text(url, timeout)?;
    Ok(MetricsSnapshot::parse(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_an_empty_snapshot() {
        let snapshot = MetricsSnapshot::parse("");
        assert!(snapshot.families.is_empty());
        assert_eq!(snapshot.uptime_seconds(), None);
        assert_eq!(snapshot.requests_total(), None);
    }

    #[test]
    fn parses_a_real_endpoints_answer() {
        let body = "\
# HELP wallermax_requests_total Requests handled, by method and status.
# TYPE wallermax_requests_total counter
wallermax_requests_total{code=\"200\",method=\"GET\"} 3
wallermax_requests_total{code=\"404\",method=\"POST\"} 1
# HELP wallermax_uptime_seconds Seconds since start.
# TYPE wallermax_uptime_seconds gauge
wallermax_uptime_seconds 12.5
# HELP wallermax_rate_limited_requests_total Rejected by the limiter.
# TYPE wallermax_rate_limited_requests_total counter
wallermax_rate_limited_requests_total 2
# HELP wallermax_registered_users Registered accounts.
# TYPE wallermax_registered_users gauge
wallermax_registered_users 4
";
        let snapshot = MetricsSnapshot::parse(body);
        assert_eq!(snapshot.uptime_seconds(), Some(12.5));
        assert_eq!(snapshot.requests_total(), Some(4.0));
        assert_eq!(
            snapshot.value_labeled(REQUESTS_TOTAL, "code", "404"),
            Some(1.0)
        );
        assert_eq!(snapshot.rate_limited_total(), Some(2.0));
        assert_eq!(snapshot.registered_users(), Some(4.0));
        let help = snapshot.families[REQUESTS_TOTAL]
            .help
            .as_deref()
            .expect("the HELP line is kept");
        assert!(help.starts_with("Requests handled"));
    }

    #[test]
    fn unknown_families_are_ignored() {
        let body = "\
some_future_family{outcome=\"great\"} 5
some_other_metric 42
";
        let snapshot = MetricsSnapshot::parse(body);
        // The parser keeps them, so a future dashboard can use them…
        assert_eq!(snapshot.value("some_other_metric"), Some(42.0));
        // …while today's typed accessors simply answer None.
        assert_eq!(snapshot.uptime_seconds(), None);
        assert_eq!(snapshot.requests_total(), None);
        assert_eq!(snapshot.registered_users(), None);
    }

    #[test]
    fn fetch_reports_a_dead_endpoint() {
        // Port 1 is privileged and unserved on every platform: the
        // connection is refused, never routed anywhere.
        let error = fetch_metrics("http://127.0.0.1:1/metrics", Duration::from_millis(500))
            .expect_err("a dead endpoint must be reported");
        assert!(!error.is_empty());
        assert!(error.contains("127.0.0.1:1"), "names the endpoint: {error}");
    }

    #[test]
    fn https_urls_are_refused_with_a_reason() {
        let error = http_get_text("https://127.0.0.1:8443/metrics", Duration::from_secs(1))
            .expect_err("https must be refused");
        assert!(
            error.contains("http://"),
            "explains the limitation: {error}"
        );
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        let dechunked = dechunk("4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n");
        assert_eq!(dechunked, "Wikipedia");
    }

    #[test]
    fn port_less_authorities_default_to_port_80() {
        // The regression behind the stubborn dashboard banner: an origin
        // like `http://localhost` used to fail with "invalid socket
        // address" regardless of the server's state.
        let (host, port) = host_port("localhost").expect("a port-less authority");
        assert_eq!(host, "localhost");
        assert_eq!(port, 80);
        let addresses = resolve_all("localhost").expect("localhost resolves");
        assert!(!addresses.is_empty());
        assert!(
            addresses.iter().all(|addr| addr.port() == 80),
            "every candidate carries the default port: {addresses:?}"
        );
    }

    #[test]
    fn ipv6_authorities_keep_their_colons() {
        assert_eq!(host_port("[::1]:9000").expect("bracketed"), ("::1", 9000));
        assert_eq!(
            host_port("[::1]").expect("bracketed, port-less"),
            ("::1", 80)
        );
        assert_eq!(host_port("::1").expect("bare ipv6"), ("::1", 80));
        assert_eq!(
            host_port("127.0.0.1:8123").expect("host and port"),
            ("127.0.0.1", 8123)
        );
        assert!(resolve_all("[::1]:9000").is_ok());
    }

    #[test]
    fn malformed_authorities_are_rejected_by_name() {
        for authority in ["localhost:http", "[::1", "[::1]junk"] {
            let error = resolve_all(authority).expect_err("must be rejected");
            assert!(
                error.contains(authority),
                "the complaint names `{authority}`: {error}"
            );
        }
    }
}
