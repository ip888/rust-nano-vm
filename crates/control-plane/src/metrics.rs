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
//! - `nanovm_warm_pool_hits_total` / `nanovm_warm_pool_misses_total` —
//!   `/fork` calls served from the pre-warmed pool vs. ones that fell
//!   through to a cold restore. Hit-rate is the warm-pool's headline
//!   number; misses are normal during cold-start or right after a
//!   burst that drains the queue.
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

/// Escape a string for use as a Prometheus label *value*. Spec requires
/// escaping `\`, `"`, and `\n`. We also escape `\r` defensively so a
/// stray carriage return can't corrupt the exposition. Order matters:
/// escape `\\` first so the new `\` characters introduced by the other
/// substitutions aren't doubled. Returns `Cow::Borrowed` on the common
/// path so the hot loop allocates nothing for well-formed inputs.
fn escape_label(v: &str) -> std::borrow::Cow<'_, str> {
    if v.bytes().any(|b| matches!(b, b'\\' | b'"' | b'\n' | b'\r')) {
        std::borrow::Cow::Owned(
            v.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r"),
        )
    } else {
        std::borrow::Cow::Borrowed(v)
    }
}

/// Process-local metrics, shared via `Arc<Metrics>` in `AppState`.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Successful fork count, keyed by token fingerprint.
    forks_total: Mutex<HashMap<String, u64>>,
    /// Successful fork count, keyed by org id. Same denominator the
    /// billing pipeline consumes: `rate(nanovm_forks_total_by_org{org="..."}[5m])`
    /// is the per-org fork rate, which Stripe Metering / Orb / etc.
    /// turn into the monthly bill. Token-level counters stay around
    /// for operator-side per-key triage.
    forks_total_by_org: Mutex<HashMap<String, u64>>,
    /// Cumulative fork wall-time (ms) by org. Lets the billing pipeline
    /// charge by compute-seconds instead of (or in addition to) fork
    /// count.
    fork_latency_ms_sum_by_org: Mutex<HashMap<String, u64>>,
    /// Fork attempts rejected by the per-token quota (429).
    throttled_total: Mutex<HashMap<String, u64>>,
    /// Fork attempts rejected by the per-token quota (429), keyed by
    /// org id. Useful for the noisy-neighbor dashboard.
    throttled_total_by_org: Mutex<HashMap<String, u64>>,
    /// Sum of fork wall-time in milliseconds.
    fork_latency_ms_sum: AtomicU64,
    /// Number of latency observations recorded.
    fork_latency_ms_count: AtomicU64,
    /// `/fork` calls served from the warm pool.
    warm_pool_hits: AtomicU64,
    /// `/fork` calls that fell through to a cold restore.
    warm_pool_misses: AtomicU64,
}

impl Metrics {
    /// Fresh metrics, all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful fork: bump the per-token and per-org
    /// counters and the global latency sum/count.
    ///
    /// `org` is the calling org's id (e.g. `"acme"`); `token_fp` is
    /// the non-cryptographic fingerprint of the bearer token. Both
    /// are needed because billing rolls up per org while operator
    /// triage often needs per-token breakdown.
    pub fn record_fork(&self, token_fp: &str, org: &str, latency_ms: u64) {
        if let Ok(mut map) = self.forks_total.lock() {
            *map.entry(token_fp.to_owned()).or_insert(0) += 1;
        }
        if let Ok(mut map) = self.forks_total_by_org.lock() {
            *map.entry(org.to_owned()).or_insert(0) += 1;
        }
        if let Ok(mut map) = self.fork_latency_ms_sum_by_org.lock() {
            *map.entry(org.to_owned()).or_insert(0) += latency_ms;
        }
        self.fork_latency_ms_sum
            .fetch_add(latency_ms, Ordering::Relaxed);
        self.fork_latency_ms_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a fork attempt that was rejected by the quota. Splits
    /// across the per-token and per-org throttle counters.
    pub fn record_throttled(&self, token_fp: &str, org: &str) {
        if let Ok(mut map) = self.throttled_total.lock() {
            *map.entry(token_fp.to_owned()).or_insert(0) += 1;
        }
        if let Ok(mut map) = self.throttled_total_by_org.lock() {
            *map.entry(org.to_owned()).or_insert(0) += 1;
        }
    }

    /// Snapshot the per-org fork counters as `(org, count, total_ms)`
    /// triples. Used by `GET /v1/usage/by-org` to render the
    /// caller's billing-relevant totals without scraping `/metrics`.
    pub fn forks_by_org_snapshot(&self) -> Vec<(String, u64, u64)> {
        let counts = self
            .forks_total_by_org
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let sums = self
            .fork_latency_ms_sum_by_org
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let mut keys: std::collections::BTreeSet<String> = counts.keys().cloned().collect();
        keys.extend(sums.keys().cloned());
        keys.into_iter()
            .map(|k| {
                let c = counts.get(&k).copied().unwrap_or(0);
                let s = sums.get(&k).copied().unwrap_or(0);
                (k, c, s)
            })
            .collect()
    }

    /// Record a fork served from the warm pool.
    pub fn record_warm_hit(&self) {
        self.warm_pool_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a fork that fell through to a cold restore.
    pub fn record_warm_miss(&self) {
        self.warm_pool_misses.fetch_add(1, Ordering::Relaxed);
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
                let fp = escape_label(fp.as_str());
                let _ = writeln!(out, "nanovm_forks_total{{token=\"{fp}\"}} {n}");
            }
        }

        // Per-org rollup — the series the billing pipeline consumes.
        out.push_str(
            "# HELP nanovm_forks_total_by_org Successful forks, rolled up per org (billing dimension).\n",
        );
        out.push_str("# TYPE nanovm_forks_total_by_org counter\n");
        if let Ok(map) = self.forks_total_by_org.lock() {
            for (org, n) in sorted_pairs(&map) {
                let org = escape_label(org.as_str());
                let _ = writeln!(out, "nanovm_forks_total_by_org{{org=\"{org}\"}} {n}");
            }
        }

        out.push_str(
            "# HELP nanovm_fork_latency_ms_sum_by_org Cumulative fork wall-time (ms), per org.\n",
        );
        out.push_str("# TYPE nanovm_fork_latency_ms_sum_by_org counter\n");
        if let Ok(map) = self.fork_latency_ms_sum_by_org.lock() {
            for (org, n) in sorted_pairs(&map) {
                let org = escape_label(org.as_str());
                let _ = writeln!(
                    out,
                    "nanovm_fork_latency_ms_sum_by_org{{org=\"{org}\"}} {n}"
                );
            }
        }

        out.push_str(
            "# HELP nanovm_fork_quota_throttled_total Fork attempts rejected by per-token quota.\n",
        );
        out.push_str("# TYPE nanovm_fork_quota_throttled_total counter\n");
        if let Ok(map) = self.throttled_total.lock() {
            for (fp, n) in sorted_pairs(&map) {
                let fp = escape_label(fp.as_str());
                let _ = writeln!(
                    out,
                    "nanovm_fork_quota_throttled_total{{token=\"{fp}\"}} {n}"
                );
            }
        }

        out.push_str(
            "# HELP nanovm_fork_quota_throttled_total_by_org Quota-rejected forks, per org.\n",
        );
        out.push_str("# TYPE nanovm_fork_quota_throttled_total_by_org counter\n");
        if let Ok(map) = self.throttled_total_by_org.lock() {
            for (org, n) in sorted_pairs(&map) {
                let org = escape_label(org.as_str());
                let _ = writeln!(
                    out,
                    "nanovm_fork_quota_throttled_total_by_org{{org=\"{org}\"}} {n}"
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

        out.push_str("# HELP nanovm_warm_pool_hits_total Forks served from the warm pool.\n");
        out.push_str("# TYPE nanovm_warm_pool_hits_total counter\n");
        let _ = writeln!(
            out,
            "nanovm_warm_pool_hits_total {}",
            self.warm_pool_hits.load(Ordering::Relaxed)
        );
        out.push_str(
            "# HELP nanovm_warm_pool_misses_total Forks that fell through to a cold restore.\n",
        );
        out.push_str("# TYPE nanovm_warm_pool_misses_total counter\n");
        let _ = writeln!(
            out,
            "nanovm_warm_pool_misses_total {}",
            self.warm_pool_misses.load(Ordering::Relaxed)
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
    fn escape_label_passes_through_ascii() {
        // Cheap path returns Borrowed; no allocation.
        let out = escape_label("acme");
        assert!(matches!(out, std::borrow::Cow::Borrowed("acme")));
    }

    #[test]
    fn escape_label_handles_quote_backslash_newline() {
        // The four characters the Prometheus exposition format actually
        // breaks on. `\r` is escaped defensively.
        assert_eq!(escape_label("a\"b").as_ref(), "a\\\"b");
        assert_eq!(escape_label("a\\b").as_ref(), "a\\\\b");
        assert_eq!(escape_label("a\nb").as_ref(), "a\\nb");
        assert_eq!(escape_label("a\rb").as_ref(), "a\\rb");
        // All four in one string, escape order matters.
        assert_eq!(escape_label("\\\"\n\r").as_ref(), "\\\\\\\"\\n\\r");
    }

    #[test]
    fn hostile_org_name_does_not_corrupt_exposition() {
        // Operator-controlled env value with a `"` and a `\n` should
        // round-trip into a single well-formed metric line.
        let m = Metrics::new();
        m.record_fork("tok-x-12", "ev\"il\nco", 1);
        let text = m.render_text();
        let line = text
            .lines()
            .find(|l| l.starts_with("nanovm_forks_total_by_org"))
            .expect("per-org line must exist");
        assert_eq!(line, "nanovm_forks_total_by_org{org=\"ev\\\"il\\nco\"} 1");
    }

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
        m.record_fork("tok-alpha-12", "acme", 7);
        m.record_fork("tok-alpha-12", "acme", 9);
        m.record_fork("tok-beta-12", "globex", 14);
        let text = m.render_text();
        assert!(text.contains("nanovm_forks_total{token=\"tok-alpha-12\"} 2"));
        assert!(text.contains("nanovm_forks_total{token=\"tok-beta-12\"} 1"));
        assert!(text.contains("nanovm_fork_latency_ms_sum 30"));
        assert!(text.contains("nanovm_fork_latency_ms_count 3"));
    }

    #[test]
    fn forks_roll_up_per_org_with_latency_sum() {
        let m = Metrics::new();
        m.record_fork("tok-alpha-12", "acme", 7);
        m.record_fork("tok-alpha-12", "acme", 9);
        m.record_fork("tok-beta-12", "globex", 14);
        let text = m.render_text();
        assert!(text.contains("nanovm_forks_total_by_org{org=\"acme\"} 2"));
        assert!(text.contains("nanovm_forks_total_by_org{org=\"globex\"} 1"));
        assert!(text.contains("nanovm_fork_latency_ms_sum_by_org{org=\"acme\"} 16"));
        assert!(text.contains("nanovm_fork_latency_ms_sum_by_org{org=\"globex\"} 14"));
    }

    #[test]
    fn throttle_counters_are_separate_from_success() {
        let m = Metrics::new();
        m.record_fork("tok-alpha-12", "acme", 5);
        m.record_throttled("tok-alpha-12", "acme");
        m.record_throttled("tok-alpha-12", "acme");
        let text = m.render_text();
        assert!(text.contains("nanovm_forks_total{token=\"tok-alpha-12\"} 1"));
        assert!(
            text.contains("nanovm_fork_quota_throttled_total{token=\"tok-alpha-12\"} 2"),
            "throttle line missing:\n{text}"
        );
        assert!(text.contains("nanovm_fork_quota_throttled_total_by_org{org=\"acme\"} 2"));
    }

    #[test]
    fn forks_by_org_snapshot_returns_count_and_total_ms_per_org() {
        let m = Metrics::new();
        m.record_fork("tok-alpha-12", "acme", 7);
        m.record_fork("tok-alpha-12", "acme", 9);
        m.record_fork("tok-beta-12", "globex", 14);
        let snap = m.forks_by_org_snapshot();
        // BTreeSet keys → sorted output.
        assert_eq!(
            snap,
            vec![("acme".to_owned(), 2, 16), ("globex".to_owned(), 1, 14),]
        );
    }

    #[test]
    fn exposition_is_deterministically_sorted_by_label() {
        let m = Metrics::new();
        m.record_fork("tok-zzz-12", "default", 1);
        m.record_fork("tok-aaa-12", "default", 1);
        let text = m.render_text();
        let aaa = text.find("tok-aaa-12").unwrap();
        let zzz = text.find("tok-zzz-12").unwrap();
        assert!(aaa < zzz, "labels must be alphabetically sorted");
    }
}
