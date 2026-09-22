//! Prometheus exposition parsing — the dashboard's data plane.
//!
//! Parsing keeps every family the endpoint exposes (including the ones
//! today's dashboard does not show yet); the typed accessors simply answer
//! `None` for families the server does not emit — **unknown families are
//! ignored**, never fatal. Fetching lives next door in [`crate::http`]:
//! the tiny client that speaks both of the local server's dialects,
//! plain HTTP and TLS.

use std::collections::BTreeMap;
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

/// Fetches and parses `url` in one step.
///
/// # Errors
///
/// See [`crate::http::http_get_text`].
pub fn fetch_metrics(url: &str, timeout: Duration) -> Result<MetricsSnapshot, String> {
    let text = crate::http::http_get_text(url, timeout)?;
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
}
