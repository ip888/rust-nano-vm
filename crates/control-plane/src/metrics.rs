//! Prometheus exposition for the fork data plane.
//!
//! Hand-rolled to avoid pulling in the `prometheus` crate (~15 transitive
//! deps for what amounts to two counters and a sum/count pair). The text
//! format is stable since 2014 and the renderer here implements it
//! exactly.
//!
//! Exposed series:
//!
//! - `nanovm_forks_total{token="tok-…"}` — successful `POST
//!   /v1/snapshots/:id/fork` calls, labeled by token fingerprint
//!   (`tok-<first4>-<len>`; the raw bearer never leaves the request).
//! - `nanovm_fork_quota_throttled_total{token="…"}` — `/fork` attempts
//!   rejected by the per-token quota with `429`.
//! - `nanovm_fork_latency_ms_sum` / `nanovm_fork_latency_ms_count` —
//!   sum + count of fork wall-time in milliseconds (rate gives mean
//!   latency: `rate(sum) / rate(count)`).
//! - `nanovm_up 1` — heartbeat gauge so a stale process is detectable.
//!
//! Labels are deliberately limited to the token fingerprint. Per-route
//! / per-status labels would explode cardinality on user-controlled
//! inputs; if we add them later they should be on a separate name with
//! bounded cardinality.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Process-local metrics, shared via `Arc<Metrics>` in `AppState`.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Successful fork count, keyed by token fingerprint.
    forks_total: Mutex<HashMap<String, u64>>,
    /// Fork attempts rejected by the per-token quota (429).
    throttled_total: Mutex<HashMap<String, u64>>,
    /// Sum of fork wall-time in milliseconds.
    fork_latency_ms_sum: AtomicU64,
    /// Number of latency observations recorded.
    fork_latency_ms_count: AtomicU64,
}

impl Metrics {
    /// Fresh metrics, all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful fork: bump the per-token counter and the
    /// latency sum/count.
    pub fn record_fork(&self, token_fp: &str, latency_ms: u64) {
        if let Ok(mut map) = self.forks_total.lock() {
            *map.entry(token_fp.to_owned()).or_insert(0) += 1;
        }
        self.fork_latency_ms_sum
            .fetch_add(latency_ms, Ordering::Relaxed);
        self.fork_latency_ms_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a fork attempt that was rejected by the quota.
    pub fn record_throttled(&self, token_fp: &str) {
        if let Ok(mut map) = self.throttled_total.lock() {
            *map.entry(token_fp.to_owned()).or_insert(0) += 1;
        }
    }

    /// Render the Prometheus text exposition for these metrics.
    pub fn render_text(&self) -> String {
        let mut out = String::with_capacity(512);

        out.push_str("# HELP nanovm_up Always 1 — process is alive.\n");
        out.push_str("# TYPE nanovm_up gauge\n");
        out.push_str("nanovm_up 1\n");

        out.push_str("# HELP nanovm_forks_total Successful forks (POST /v1/snapshots/:id/fork).\n");
        out.push_str("# TYPE nanovm_forks_total counter\n");
        if let Ok(map) = self.forks_total.lock() {
            for (fp, n) in sorted_pairs(&map) {
                let _ = writeln!(out, "nanovm_forks_total{{token=\"{fp}\"}} {n}");
            }
        }

        out.push_str(
            "# HELP nanovm_fork_quota_throttled_total Fork attempts rejected by per-token quota.\n",
        );
        out.push_str("# TYPE nanovm_fork_quota_throttled_total counter\n");
        if let Ok(map) = self.throttled_total.lock() {
            for (fp, n) in sorted_pairs(&map) {
                let _ = writeln!(
                    out,
                    "nanovm_fork_quota_throttled_total{{token=\"{fp}\"}} {n}"
                );
            }
        }

        out.push_str("# HELP nanovm_fork_latency_ms_sum Sum of fork wall-time (ms).\n");
        out.push_str("# TYPE nanovm_fork_latency_ms_sum counter\n");
        let _ = writeln!(
            out,
            "nanovm_fork_latency_ms_sum {}",
            self.fork_latency_ms_sum.load(Ordering::Relaxed)
        );
        out.push_str("# HELP nanovm_fork_latency_ms_count Number of latency observations.\n");
        out.push_str("# TYPE nanovm_fork_latency_ms_count counter\n");
        let _ = writeln!(
            out,
            "nanovm_fork_latency_ms_count {}",
            self.fork_latency_ms_count.load(Ordering::Relaxed)
        );

        out
    }
}

/// Sort label values for deterministic exposition order (helps diffing
/// scrape outputs and asserting in tests).
fn sorted_pairs(map: &HashMap<String, u64>) -> Vec<(&String, u64)> {
    let mut v: Vec<_> = map.iter().map(|(k, n)| (k, *n)).collect();
    v.sort_by(|a, b| a.0.cmp(b.0));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_with_no_data_lists_only_help_lines_and_up_gauge() {
        let m = Metrics::new();
        let text = m.render_text();
        assert!(text.contains("nanovm_up 1"));
        assert!(text.contains("# TYPE nanovm_forks_total counter"));
        assert!(text.contains("nanovm_fork_latency_ms_sum 0"));
        assert!(text.contains("nanovm_fork_latency_ms_count 0"));
        // No labeled samples yet.
        assert!(!text.contains("nanovm_forks_total{"));
    }

    #[test]
    fn forks_accumulate_per_token_and_total_latency() {
        let m = Metrics::new();
        m.record_fork("tok-alpha-12", 7);
        m.record_fork("tok-alpha-12", 9);
        m.record_fork("tok-beta-12", 14);
        let text = m.render_text();
        assert!(text.contains("nanovm_forks_total{token=\"tok-alpha-12\"} 2"));
        assert!(text.contains("nanovm_forks_total{token=\"tok-beta-12\"} 1"));
        assert!(text.contains("nanovm_fork_latency_ms_sum 30"));
        assert!(text.contains("nanovm_fork_latency_ms_count 3"));
    }

    #[test]
    fn throttle_counters_are_separate_from_success() {
        let m = Metrics::new();
        m.record_fork("tok-alpha-12", 5);
        m.record_throttled("tok-alpha-12");
        m.record_throttled("tok-alpha-12");
        let text = m.render_text();
        assert!(text.contains("nanovm_forks_total{token=\"tok-alpha-12\"} 1"));
        assert!(
            text.contains("nanovm_fork_quota_throttled_total{token=\"tok-alpha-12\"} 2"),
            "throttle line missing:\n{text}"
        );
    }

    #[test]
    fn exposition_is_deterministically_sorted_by_label() {
        let m = Metrics::new();
        m.record_fork("tok-zzz-12", 1);
        m.record_fork("tok-aaa-12", 1);
        let text = m.render_text();
        let aaa = text.find("tok-aaa-12").unwrap();
        let zzz = text.find("tok-zzz-12").unwrap();
        assert!(aaa < zzz, "labels must be alphabetically sorted");
    }
}
