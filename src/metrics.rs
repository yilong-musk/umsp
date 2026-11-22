//! In-memory Prometheus metrics (spec 6.2).
//!
//! Everything is aggregated from the same [`RequestRecord`] that the ledger
//! writes, in [`crate::server::AppState::finish_request`]: one ledger row and
//! one metrics observation per finished request, so `/metrics` counters and
//! `umsp usage` can never drift apart.
//!
//! Labels are deliberately low-cardinality: `provider`, upstream `model`,
//! `protocol`, HTTP `status`, `stream`, token `direction`, error `code` and
//! circuit `provider`. No request id, client key or client name is exported.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::usage::RequestRecord;

/// Latency buckets in seconds (spec 6.2 example, `+Inf` implied).
pub const DURATION_BUCKETS: [f64; 11] =
    [0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0];

type RequestKey = (String, String, String, u16, bool);
type SeriesKey = (String, String, String);

#[derive(Debug, Clone)]
struct Histogram {
    buckets: &'static [f64],
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn new(buckets: &'static [f64]) -> Self {
        Self {
            buckets,
            counts: vec![0; buckets.len()],
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        for (index, bound) in self.buckets.iter().enumerate() {
            if value <= *bound {
                self.counts[index] += 1;
            }
        }
    }
}

#[derive(Debug, Default)]
struct Data {
    requests: BTreeMap<RequestKey, u64>,
    durations: BTreeMap<SeriesKey, Histogram>,
    ttft: BTreeMap<SeriesKey, Histogram>,
    /// `(provider, model, direction)` → tokens.
    tokens: BTreeMap<(String, String, String), u64>,
    /// `(provider, error code)` → failures.
    upstream_errors: BTreeMap<(String, String), u64>,
    inflight: BTreeMap<String, i64>,
}

/// Shared metrics registry. Cloning shares the same data.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    inner: Arc<Mutex<Data>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Account one finished request. `model` is the logical model the client
    /// asked for; `provider` is the backend that actually served it.
    pub fn observe(&self, record: &RequestRecord) {
        let provider = label_or_dash(&record.provider);
        let model = label_or_dash(&record.requested_model);
        let protocol = label_or_dash(&record.protocol);
        let series = (provider.clone(), model.clone(), protocol.clone());

        let mut data = self.inner.lock().expect("metrics lock poisoned");
        *data
            .requests
            .entry((
                provider.clone(),
                model.clone(),
                protocol.clone(),
                record.status,
                record.stream,
            ))
            .or_default() += 1;

        data.durations
            .entry(series.clone())
            .or_insert_with(|| Histogram::new(&DURATION_BUCKETS))
            .observe(record.duration_ms as f64 / 1000.0);

        if let Some(ttft_ms) = record.ttft_ms {
            data.ttft
                .entry(series.clone())
                .or_insert_with(|| Histogram::new(&DURATION_BUCKETS))
                .observe(ttft_ms as f64 / 1000.0);
        }

        for (direction, value) in [
            ("input", record.prompt_tokens),
            ("output", record.completion_tokens),
            ("cached", record.cached_tokens),
            ("reasoning", record.reasoning_tokens),
        ] {
            if value > 0 {
                *data
                    .tokens
                    .entry((provider.clone(), model.clone(), direction.to_string()))
                    .or_default() += value;
            }
        }

        // Upstream faults are counted per attempt by the proxy loops
        // (`note_upstream_error`), so a fallback that absorbs a failed primary
        // still shows the primary's error without double-counting here.
    }

    /// Count one failed upstream attempt. Called for every attempt, including
    /// the ones a retry or a fallback absorbed, so this counter can exceed the
    /// finished-request count in `umsp_requests_total`.
    pub fn note_upstream_error(&self, provider: &str, code: &str) {
        let mut data = self.inner.lock().expect("metrics lock poisoned");
        *data
            .upstream_errors
            .entry((label_or_dash(provider), code.to_string()))
            .or_default() += 1;
    }

    /// Track an in-flight request for `provider` until the guard is dropped.
    pub fn inflight(&self, provider: &str) -> Inflight {
        let provider = label_or_dash(provider);
        {
            let mut data = self.inner.lock().expect("metrics lock poisoned");
            *data.inflight.entry(provider.clone()).or_default() += 1;
        }
        Inflight {
            metrics: self.clone(),
            provider,
        }
    }

    fn inflight_done(&self, provider: &str) {
        let mut data = self.inner.lock().expect("metrics lock poisoned");
        if let Some(value) = data.inflight.get_mut(provider) {
            *value -= 1;
            if *value <= 0 {
                data.inflight.remove(provider);
            }
        }
    }

    /// Render the Prometheus text exposition format. `circuits` carries the
    /// configured providers and whether their breaker is open right now.
    pub fn render(&self, circuits: &[(String, bool)]) -> String {
        let data = self.inner.lock().expect("metrics lock poisoned");
        let mut out = String::with_capacity(4096);

        out.push_str(
            "# HELP umsp_requests_total Requests handled, by final provider/model/protocol.\n\
             # TYPE umsp_requests_total counter\n",
        );
        for ((provider, model, protocol, status, stream), value) in &data.requests {
            out.push_str(&format!(
                "umsp_requests_total{{provider=\"{}\",model=\"{}\",protocol=\"{}\",status=\"{}\",stream=\"{}\"}} {}\n",
                escape(provider),
                escape(model),
                escape(protocol),
                status,
                stream,
                value
            ));
        }

        out.push_str(
            "# HELP umsp_request_duration_seconds End-to-end request duration in seconds.\n\
             # TYPE umsp_request_duration_seconds histogram\n",
        );
        for ((provider, model, protocol), histogram) in &data.durations {
            render_histogram(
                &mut out,
                "umsp_request_duration_seconds",
                provider,
                model,
                protocol,
                histogram,
            );
        }

        out.push_str(
            "# HELP umsp_ttft_seconds Time to first streamed content byte in seconds.\n\
             # TYPE umsp_ttft_seconds histogram\n",
        );
        for ((provider, model, protocol), histogram) in &data.ttft {
            render_histogram(
                &mut out,
                "umsp_ttft_seconds",
                provider,
                model,
                protocol,
                histogram,
            );
        }

        out.push_str(
            "# HELP umsp_tokens_total Tokens reported by the upstream, by direction.\n\
             # TYPE umsp_tokens_total counter\n",
        );
        for ((provider, model, direction), value) in &data.tokens {
            out.push_str(&format!(
                "umsp_tokens_total{{provider=\"{}\",model=\"{}\",direction=\"{}\"}} {}\n",
                escape(provider),
                escape(model),
                escape(direction),
                value
            ));
        }

        out.push_str(
            "# HELP umsp_upstream_errors_total Upstream failures, by provider and error code.\n\
             # TYPE umsp_upstream_errors_total counter\n",
        );
        for ((provider, code), value) in &data.upstream_errors {
            out.push_str(&format!(
                "umsp_upstream_errors_total{{provider=\"{}\",code=\"{}\"}} {}\n",
                escape(provider),
                escape(code),
                value
            ));
        }

        out.push_str(
            "# HELP umsp_circuit_open Circuit breaker state: 1 = open or half-open.\n\
             # TYPE umsp_circuit_open gauge\n",
        );
        for (provider, open) in circuits {
            out.push_str(&format!(
                "umsp_circuit_open{{provider=\"{}\"}} {}\n",
                escape(provider),
                u8::from(*open)
            ));
        }

        out.push_str(
            "# HELP umsp_inflight_requests Requests currently waiting on a provider.\n\
             # TYPE umsp_inflight_requests gauge\n",
        );
        for (provider, value) in &data.inflight {
            out.push_str(&format!(
                "umsp_inflight_requests{{provider=\"{}\"}} {}\n",
                escape(provider),
                value
            ));
        }

        out
    }
}

fn render_histogram(
    out: &mut String,
    name: &str,
    provider: &str,
    model: &str,
    protocol: &str,
    histogram: &Histogram,
) {
    let labels = format!(
        "provider=\"{}\",model=\"{}\",protocol=\"{}\"",
        escape(provider),
        escape(model),
        escape(protocol)
    );
    for (index, bound) in histogram.buckets.iter().enumerate() {
        out.push_str(&format!(
            "{name}_bucket{{{labels},le=\"{}\"}} {}\n",
            format_bound(*bound),
            histogram.counts[index]
        ));
    }
    out.push_str(&format!(
        "{name}_bucket{{{labels},le=\"+Inf\"}} {}\n",
        histogram.count
    ));
    out.push_str(&format!(
        "{name}_sum{{{labels}}} {}\n",
        format_value(histogram.sum)
    ));
    out.push_str(&format!("{name}_count{{{labels}}} {}\n", histogram.count));
}

fn format_bound(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        // Prometheus prints `0.05`, `0.1`, … without trailing zeros.
        let mut text = format!("{value}");
        if text.contains('.') {
            while text.ends_with('0') {
                text.pop();
            }
        }
        text
    }
}

fn format_value(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// Escape a Prometheus label value (spec: `\\`, `"`, `\n`).
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

fn label_or_dash(value: &str) -> String {
    if value.is_empty() {
        "-".to_string()
    } else {
        value.to_string()
    }
}

/// Keeps `umsp_inflight_requests` balanced even when a request future is
/// cancelled mid-flight.
#[derive(Debug)]
pub struct Inflight {
    metrics: Metrics,
    provider: String,
}

impl Drop for Inflight {
    fn drop(&mut self) {
        self.metrics.inflight_done(&self.provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(provider: &str, model: &str, status: u16, stream: bool) -> RequestRecord {
        RequestRecord {
            ts: "2026-11-19T00:00:00Z".to_string(),
            request_id: "req_1".to_string(),
            client: "secret-client".to_string(),
            client_ip: None,
            protocol: "chat".to_string(),
            requested_model: model.to_string(),
            provider: provider.to_string(),
            upstream_model: model.to_string(),
            stream,
            status,
            upstream_status: Some(status),
            upstream_body: None,
            prompt_tokens: 12,
            completion_tokens: 34,
            cached_tokens: 0,
            reasoning_tokens: 5,
            ttft_ms: stream.then_some(120),
            duration_ms: 1500,
            bytes_in: 10,
            bytes_out: 20,
            error_code: None,
            upstream_request_id: None,
            client_disconnected: false,
            fallbacks: 0,
            quirks: Vec::new(),
        }
    }

    #[test]
    fn renders_counters_histograms_and_gauges() {
        let metrics = Metrics::new();
        metrics.observe(&record("od", "deepseek-v4.1-flash", 200, true));
        let text = metrics.render(&[("od".to_string(), false)]);
        assert!(text.contains(
            "umsp_requests_total{provider=\"od\",model=\"deepseek-v4.1-flash\",protocol=\"chat\",status=\"200\",stream=\"true\"} 1"
        ));
        assert!(text.contains("umsp_request_duration_seconds_bucket{provider=\"od\""));
        assert!(text.contains("umsp_request_duration_seconds_count{provider=\"od\",model=\"deepseek-v4.1-flash\",protocol=\"chat\"} 1"));
        assert!(text.contains("le=\"0.05\"} 0"));
        assert!(text.contains("le=\"+Inf\"} 1"));
        assert!(text.contains("umsp_ttft_seconds_count"));
        assert!(text.contains(
            "umsp_tokens_total{provider=\"od\",model=\"deepseek-v4.1-flash\",direction=\"input\"} 12"
        ));
        assert!(text.contains("direction=\"reasoning\"} 5"));
        // Zero token counters do not create series.
        assert!(!text.contains("direction=\"cached\""));
        assert!(text.contains("umsp_circuit_open{provider=\"od\"} 0"));
        // No client identity leaks into labels.
        assert!(!text.contains("secret-client"));
    }

    #[test]
    fn upstream_errors_are_counted_per_attempt() {
        let metrics = Metrics::new();
        // Two failed primary attempts that a fallback absorbed: counted, even
        // though no ledger row carries these codes.
        metrics.note_upstream_error("od", "upstream_502");
        metrics.note_upstream_error("od", "upstream_502");
        let text = metrics.render(&[("deepseek".to_string(), true)]);
        assert!(
            text.contains("umsp_upstream_errors_total{provider=\"od\",code=\"upstream_502\"} 2")
        );
        assert!(text.contains("umsp_circuit_open{provider=\"deepseek\"} 1"));
        assert!(text.contains("# TYPE umsp_upstream_errors_total counter"));
    }

    #[test]
    fn inflight_gauge_returns_to_zero() {
        let metrics = Metrics::new();
        {
            let _guard = metrics.inflight("od");
            let text = metrics.render(&[]);
            assert!(text.contains("umsp_inflight_requests{provider=\"od\"} 1"));
        }
        let text = metrics.render(&[]);
        assert!(!text.contains("umsp_inflight_requests{provider=\"od\"}"));
    }

    #[test]
    fn labels_are_escaped() {
        let metrics = Metrics::new();
        let mut weird = record("od", "weird\"name\\", 200, false);
        weird.protocol = "chat".to_string();
        metrics.observe(&weird);
        let text = metrics.render(&[]);
        assert!(text.contains("model=\"weird\\\"name\\\\\""));
    }

    #[test]
    fn bucket_bounds_render_without_trailing_zeros() {
        assert_eq!(format_bound(0.05), "0.05");
        assert_eq!(format_bound(1.0), "1");
        assert_eq!(format_bound(300.0), "300");
    }
}
