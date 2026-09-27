//! Prometheus metrics.
//!
//! Hand-rolled rather than pulling in a registry crate: there are four series
//! and a histogram, and the exposition format is a dozen lines of printing.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use portcullis_core::branding::metrics as series;

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

        header(
            &mut out,
            series::HTTP_REQUESTS,
            "counter",
            "HTTP responses by route and status.",
        );
        for ((route, status), n) in &inner.http_requests {
            let _ = writeln!(
                out,
                "{}{{route=\"{}\",status=\"{status}\"}} {n}",
                series::HTTP_REQUESTS,
                escape(route)
            );
        }

        header(
            &mut out,
            series::TOOL_CALLS,
            "counter",
            "Tool calls by action and outcome.",
        );
        for ((action, outcome), n) in &inner.tool_calls {
            let _ = writeln!(
                out,
                "{}{{action=\"{}\",outcome=\"{outcome}\"}} {n}",
                series::TOOL_CALLS,
                escape(action)
            );
        }

        header(
            &mut out,
            series::AUTH_FAILURES,
            "counter",
            "Rejected credentials by reason.",
        );
        for (code, n) in &inner.auth_failures {
            let _ = writeln!(out, "{}{{reason=\"{code}\"}} {n}", series::AUTH_FAILURES);
        }

        header(
            &mut out,
            series::TOOL_DURATION,
            "histogram",
            "Tool call wall time.",
        );
        let mut cumulative = 0u64;
        for (i, bound) in BUCKETS.iter().enumerate() {
            cumulative += inner.bucket_counts[i];
            let _ = writeln!(
                out,
                "{}_bucket{{le=\"{bound}\"}} {cumulative}",
                series::TOOL_DURATION
            );
        }
        cumulative += inner.over_last_bucket;
        let _ = writeln!(
            out,
            "{}_bucket{{le=\"+Inf\"}} {cumulative}",
            series::TOOL_DURATION
        );
        let _ = writeln!(out, "{}_sum {}", series::TOOL_DURATION, inner.duration_sum);
        let _ = writeln!(
            out,
            "{}_count {}",
            series::TOOL_DURATION,
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

/// Write the `# HELP` and `# TYPE` lines for one series.
///
/// These were literals while the sample lines used the constants. A prefix
/// change would then have produced an exposition whose comments named one
/// family and whose samples named another, which Prometheus rejects.
fn header(out: &mut String, series: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {series} {help}");
    let _ = writeln!(out, "# TYPE {series} {kind}");
}

fn escape(label: &str) -> String {
    label.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test that would have caught the HELP and TYPE lines being literals
    /// while the samples used the constants: it reads the rendered output and
    /// insists every name in it is one the branding module declares.
    #[test]
    fn the_rendered_output_names_only_declared_series() {
        let m = Metrics::new();
        m.request("/mcp", 200);
        m.tool_call("find_order", "ok", Duration::from_millis(3));
        m.auth_failure("credential_missing");
        let text = m.render();

        for s in series::ALL {
            assert!(
                text.contains(&format!("# HELP {s} ")),
                "no HELP line for `{s}`"
            );
            assert!(
                text.contains(&format!("# TYPE {s} ")),
                "no TYPE line for `{s}`"
            );
        }

        for line in text.lines() {
            let named = line
                .strip_prefix("# HELP ")
                .or_else(|| line.strip_prefix("# TYPE "))
                .map_or_else(
                    || line.split(['{', ' ']).next().unwrap_or(""),
                    |rest| rest.split_whitespace().next().unwrap_or(""),
                );
            if named.is_empty() {
                continue;
            }
            // Histogram samples carry _bucket, _sum and _count suffixes.
            let base = named
                .trim_end_matches("_bucket")
                .trim_end_matches("_sum")
                .trim_end_matches("_count");
            assert!(
                series::ALL.contains(&base) || series::ALL.contains(&named),
                "`{named}` is not a declared series; line: {line}"
            );
        }
    }

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
        assert!(text.contains("portcullis_http_requests_total{route=\"/mcp\",status=\"200\"} 2"));
        assert!(text.contains("portcullis_http_requests_total{route=\"/mcp\",status=\"401\"} 1"));
        assert!(
            text.contains("portcullis_tool_calls_total{action=\"find_order\",outcome=\"ok\"} 1")
        );
        assert!(text.contains("portcullis_auth_failures_total{reason=\"credential_missing\"} 1"));
        // One call landed past the last bucket, so +Inf carries both.
        assert!(text.contains("portcullis_tool_call_duration_seconds_bucket{le=\"+Inf\"} 2"));
        assert!(text.contains("portcullis_tool_call_duration_seconds_count 2"));
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
