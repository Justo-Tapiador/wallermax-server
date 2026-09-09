//! Prometheus metrics (the `[metrics]` section).
//!
//! One [`Metrics`] instance lives in the application state while
//! enabled. The request logging middleware feeds it
//! ([`Metrics::record_request`]); the rate limiter counts rejections
//! ([`Metrics::record_rate_limited`]); the exposition handler renders
//! everything ([`Metrics::render`]) at the configured path
//! (`GET /metrics` by default).
//!
//! Metric families:
//!
//! | Metric                                     | Type      | Labels          |
//! |--------------------------------------------|-----------|-----------------|
//! | `wallermax_requests_total`                 | counter   | `method`, `code`|
//! | `wallermax_request_duration_seconds`       | histogram | `method`        |
//! | `wallermax_rate_limited_requests_total`    | counter   | —               |
//! | `wallermax_uptime_seconds`                 | gauge     | —               |
//! | `wallermax_registered_users`               | gauge     | — (auth only)   |
//! | `wallermax_template_backend`               | gauge     | `backend`       |
//!
//! `wallermax_template_backend` (v0.10.1) is a 0/1 pair over the
//! `boa`/`sidecar` labels: the backend currently serving renders is
//! `1`, the other `0` (both `0` while `[templates]` is disabled), so
//! either half can be alerted on.
//!
//! On Linux the standard `process_*` collectors (CPU, memory, file
//! descriptors, ...) are registered as well; they are unavailable on
//! other platforms and simply absent from the output.
//!
//! Counters are maintained by the logging middleware, so they only
//! advance while `[middleware] logging = true` — the same caveat as
//! `GET /api/stats`.

use prometheus::{
    Encoder, Gauge, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec,
    Opts, Registry, TextEncoder,
};

/// Content type of the exposition payload (Prometheus text format).
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Registered metric families shared by the whole application.
#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    requests_total: IntCounterVec,
    request_duration: HistogramVec,
    rate_limited_total: IntCounter,
    uptime_seconds: Gauge,
    registered_users: Option<IntGauge>,
    template_backend: IntGaugeVec,
}

impl Metrics {
    /// Builds and registers every metric family.
    ///
    /// `auth_enabled` adds `wallermax_registered_users`. Registration
    /// failures are programming errors (duplicate names, label
    /// mismatches) surfaced as `Err` so the caller can disable metrics
    /// instead of panicking at startup.
    ///
    /// # Errors
    ///
    /// Returns the `prometheus::Error` of the first failed
    /// registration.
    pub fn new(auth_enabled: bool) -> Result<Self, prometheus::Error> {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new(
                "wallermax_requests_total",
                "Requests served, by HTTP method and response status code.",
            ),
            &["method", "code"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        let request_duration = HistogramVec::new(
            HistogramOpts::new(
                "wallermax_request_duration_seconds",
                "Request handling time in seconds, by HTTP method.",
            ),
            &["method"],
        )?;
        registry.register(Box::new(request_duration.clone()))?;

        let rate_limited_total = IntCounter::new(
            "wallermax_rate_limited_requests_total",
            "Requests rejected by the rate limiter.",
        )?;
        registry.register(Box::new(rate_limited_total.clone()))?;

        let uptime_seconds = Gauge::new(
            "wallermax_uptime_seconds",
            "Seconds since the server started.",
        )?;
        registry.register(Box::new(uptime_seconds.clone()))?;

        let registered_users = auth_enabled
            .then(|| IntGauge::new("wallermax_registered_users", "Registered user accounts."))
            .transpose()?;
        if let Some(gauge) = &registered_users {
            registry.register(Box::new(gauge.clone()))?;
        }

        let template_backend = IntGaugeVec::new(
            Opts::new(
                "wallermax_template_backend",
                "Whether the .jhs rendering backend is live: 1 on the backend currently serving renders, 0 on the other (both 0 while templates are disabled).",
            ),
            &["backend"],
        )?;
        registry.register(Box::new(template_backend.clone()))?;

        // Process collectors exist on Linux only; the feature is
        // enabled for the crate but the module is cfg-gated upstream.
        #[cfg(target_os = "linux")]
        if let Err(error) = registry.register(Box::new(
            prometheus::process_collector::ProcessCollector::for_self(),
        )) {
            // Best-effort: a locked-down /proc must not break startup.
            tracing::warn!(%error, "process metrics unavailable");
        }

        Ok(Self {
            registry,
            requests_total,
            request_duration,
            rate_limited_total,
            uptime_seconds,
            registered_users,
            template_backend,
        })
    }

    /// Records one served request: counter by method/status and latency
    /// observation by method.
    pub fn record_request(&self, method: &str, status: u16, duration_secs: f64) {
        // Label values are ASCII (HTTP tokens and formatted numbers),
        // always valid for the vec APIs.
        self.requests_total
            .with_label_values(&[method, &status.to_string()])
            .inc();
        self.request_duration
            .with_label_values(&[method])
            .observe(duration_secs);
    }

    /// Records one rate-limited rejection.
    pub fn record_rate_limited(&self) {
        self.rate_limited_total.inc();
    }

    /// Renders the exposition payload.
    ///
    /// `uptime_secs`, `registered_users` and `template_backend` are
    /// refreshed before gathering so the gauges are exact at scrape
    /// time.
    ///
    /// # Errors
    ///
    /// Returns a message when encoding fails (a broken collector).
    pub fn render(
        &self,
        uptime_secs: f64,
        registered_users: Option<i64>,
        template_backend: Option<&str>,
    ) -> Result<String, String> {
        self.uptime_seconds.set(uptime_secs);
        if let Some(count) = registered_users {
            if let Some(gauge) = &self.registered_users {
                gauge.set(count.max(0));
            }
        }

        // v0.10.1: the live template backend as a 0/1 pair, so either
        // half can be alerted on (sidecar down => boa fallback).
        // "unavailable" (strict spawn failed) and `None` ([templates]
        // disabled) both render as "no backend is serving".
        let (sidecar, boa) = match template_backend {
            Some("sidecar") => (1, 0),
            Some("boa") => (0, 1),
            _ => (0, 0),
        };
        self.template_backend
            .with_label_values(&["sidecar"])
            .set(sidecar);
        self.template_backend.with_label_values(&["boa"]).set(boa);

        let families = self.registry.gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        encoder
            .encode(&families, &mut buffer)
            .map_err(|error| error.to_string())?;

        let mut body = String::from_utf8(buffer)
            .map_err(|error| format!("metrics output is not valid UTF-8: {error}"))?;
        // Exposition format ends with a trailing newline; keep it
        // idempotent for append-style usage.
        if !body.ends_with('\n') {
            body.push('\n');
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_register_and_render() {
        let metrics = Metrics::new(false).expect("metrics build");

        metrics.record_request("GET", 200, 0.0015);
        metrics.record_request("POST", 404, 0.25);
        metrics.record_rate_limited();

        let body = metrics
            .render(12.5, None, Some("sidecar"))
            .expect("renders");
        assert!(body.contains("# HELP wallermax_requests_total"));
        assert!(body.contains("wallermax_requests_total{code=\"200\",method=\"GET\"} 1"));
        assert!(body.contains("wallermax_requests_total{code=\"404\",method=\"POST\"} 1"));
        assert!(body.contains("wallermax_rate_limited_requests_total 1"));
        assert!(body.contains("wallermax_uptime_seconds 12.5"));
        assert!(!body.contains("wallermax_registered_users"));
        assert!(body.contains("wallermax_template_backend{backend=\"sidecar\"} 1"));
        assert!(body.contains("wallermax_template_backend{backend=\"boa\"} 0"));
    }

    #[test]
    fn counters_accumulate() {
        let metrics = Metrics::new(false).expect("metrics build");

        metrics.record_request("GET", 200, 0.001);
        metrics.record_request("GET", 200, 0.002);
        metrics.record_request("GET", 500, 0.003);

        let body = metrics.render(1.0, None, None).expect("renders");
        assert!(body.contains("wallermax_requests_total{code=\"200\",method=\"GET\"} 2"));
        assert!(body.contains("wallermax_requests_total{code=\"500\",method=\"GET\"} 1"));
        assert!(body.contains("wallermax_request_duration_seconds_count{method=\"GET\"} 3"));
        assert!(body.contains("wallermax_template_backend{backend=\"sidecar\"} 0"));
        assert!(body.contains("wallermax_template_backend{backend=\"boa\"} 0"));
    }

    #[test]
    fn registered_users_gauge_tracks_auth() {
        let metrics = Metrics::new(true).expect("metrics build");

        let body = metrics.render(2.0, Some(7), Some("boa")).expect("renders");
        assert!(body.contains("wallermax_registered_users 7"));
        assert!(body.contains("wallermax_template_backend{backend=\"boa\"} 1"));

        let refreshed = metrics.render(3.0, Some(8), Some("boa")).expect("renders");
        assert!(refreshed.contains("wallermax_registered_users 8"));
    }

    #[test]
    fn template_backend_gauge_reports_the_live_backend() {
        let metrics = Metrics::new(false).expect("metrics build");

        let sidecar = metrics.render(1.0, None, Some("sidecar")).expect("renders");
        assert!(sidecar.contains("wallermax_template_backend{backend=\"sidecar\"} 1"));
        assert!(sidecar.contains("wallermax_template_backend{backend=\"boa\"} 0"));

        let fallback = metrics.render(2.0, None, Some("boa")).expect("renders");
        assert!(fallback.contains("wallermax_template_backend{backend=\"boa\"} 1"));
        assert!(fallback.contains("wallermax_template_backend{backend=\"sidecar\"} 0"));

        let disabled = metrics.render(3.0, None, None).expect("renders");
        assert!(disabled.contains("wallermax_template_backend{backend=\"boa\"} 0"));
        assert!(disabled.contains("wallermax_template_backend{backend=\"sidecar\"} 0"));
    }

    #[test]
    fn content_type_is_prometheus_text() {
        assert_eq!(CONTENT_TYPE, "text/plain; version=0.0.4");
    }
}
