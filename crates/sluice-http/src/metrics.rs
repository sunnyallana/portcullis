//! Prometheus metrics.
//!
//! Hand-rolled rather than pulling in a registry crate: there are four series
//! and a histogram, and the exposition format is a dozen lines of printing.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

/// Upper bounds in seconds. The same set Prometheus clients use by default.
const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Debug, Default)]
struct Inner {
    http_requests: BTreeMap<(String, u16), u64>,
    tool_calls: BTreeMap<(String, &'static str), u64>,
    auth_failures: BTreeMap<&'static str, u64>,
    bucket_counts: [u64; BUCKETS.len()],
    over_last_bucket: u64,
    duration_sum: f64,
    duration_count: u64,
}

/// Counters and one histogram, safe to share.
#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    /// A fresh, empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one HTTP response.
    pub fn request(&self, route: &str, status: u16) {
        let mut inner = self.lock();
        *inner
            .http_requests
            .entry((route.to_owned(), status))
            .or_insert(0) += 1;
    }

    /// Record one tool call and how long it took.
    pub fn tool_call(&self, action: &str, outcome: &'static str, elapsed: Duration) {
        let mut inner = self.lock();
        *inner
            .tool_calls
            .entry((action.to_owned(), outcome))
            .or_insert(0) += 1;

        let seconds = elapsed.as_secs_f64();
        inner.duration_sum += seconds;
        inner.duration_count += 1;
        let mut placed = false;
        for (i, bound) in BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                inner.bucket_counts[i] += 1;
                placed = true;
                break;
            }
        }
        if !placed {
            inner.over_last_bucket += 1;
        }
    }

    /// Record a rejected credential.
    pub fn auth_failure(&self, code: &'static str) {
        let mut inner = self.lock();
        *inner.auth_failures.entry(code).or_insert(0) += 1;
    }

    /// Render the exposition format.
    pub fn render(&self) -> String {
        let inner = self.lock();
        let mut out = String::new();

        out.push_str("# HELP sluice_http_requests_total HTTP responses by route and status.\n");
        out.push_str("# TYPE sluice_http_requests_total counter\n");
        for ((route, status), n) in &inner.http_requests {
            let _ = writeln!(
                out,
                "sluice_http_requests_total{{route=\"{}\",status=\"{status}\"}} {n}",
                escape(route)
            );
        }

        out.push_str("# HELP sluice_tool_calls_total Tool calls by action and outcome.\n");
        out.push_str("# TYPE sluice_tool_calls_total counter\n");
        for ((action, outcome), n) in &inner.tool_calls {
            let _ = writeln!(
                out,
                "sluice_tool_calls_total{{action=\"{}\",outcome=\"{outcome}\"}} {n}",
                escape(action)
            );
        }

        out.push_str("# HELP sluice_auth_failures_total Rejected credentials by reason.\n");
        out.push_str("# TYPE sluice_auth_failures_total counter\n");
        for (code, n) in &inner.auth_failures {
            let _ = writeln!(out, "sluice_auth_failures_total{{reason=\"{code}\"}} {n}");
        }

        out.push_str("# HELP sluice_tool_call_duration_seconds Tool call wall time.\n");
        out.push_str("# TYPE sluice_tool_call_duration_seconds histogram\n");
        let mut cumulative = 0u64;
        for (i, bound) in BUCKETS.iter().enumerate() {
            cumulative += inner.bucket_counts[i];
            let _ = writeln!(
                out,
                "sluice_tool_call_duration_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
            );
        }
        cumulative += inner.over_last_bucket;
        let _ = writeln!(
            out,
            "sluice_tool_call_duration_seconds_bucket{{le=\"+Inf\"}} {cumulative}"
        );
        let _ = writeln!(
            out,
            "sluice_tool_call_duration_seconds_sum {}",
            inner.duration_sum
        );
        let _ = writeln!(
            out,
            "sluice_tool_call_duration_seconds_count {}",
            inner.duration_count
        );

        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned metrics mutex should not take the server down; the worst
        // case is a slightly wrong counter.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn escape(label: &str) -> String {
    label.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_the_histogram_render() {
        let m = Metrics::new();
        m.request("/mcp", 200);
        m.request("/mcp", 200);
        m.request("/mcp", 401);
        m.tool_call("find_order", "ok", Duration::from_millis(3));
        m.tool_call("find_order", "error", Duration::from_secs(30));
        m.auth_failure("credential_missing");

        let text = m.render();
        assert!(text.contains("sluice_http_requests_total{route=\"/mcp\",status=\"200\"} 2"));
        assert!(text.contains("sluice_http_requests_total{route=\"/mcp\",status=\"401\"} 1"));
        assert!(text.contains("sluice_tool_calls_total{action=\"find_order\",outcome=\"ok\"} 1"));
        assert!(text.contains("sluice_auth_failures_total{reason=\"credential_missing\"} 1"));
        // One call landed past the last bucket, so +Inf carries both.
        assert!(text.contains("sluice_tool_call_duration_seconds_bucket{le=\"+Inf\"} 2"));
        assert!(text.contains("sluice_tool_call_duration_seconds_count 2"));
    }

    #[test]
    fn buckets_are_cumulative() {
        let m = Metrics::new();
        m.tool_call("a", "ok", Duration::from_millis(1));
        let text = m.render();
        assert!(text.contains("_bucket{le=\"0.005\"} 1"));
        assert!(
            text.contains("_bucket{le=\"10\"} 1"),
            "later buckets include earlier ones"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        let m = Metrics::new();
        m.tool_call("we\"ird", "ok", Duration::from_millis(1));
        assert!(m.render().contains("action=\"we\\\"ird\""));
    }
}
