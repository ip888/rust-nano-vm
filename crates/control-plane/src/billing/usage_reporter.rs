//! Metered-billing reporter — flushes per-org fork deltas to Stripe.
//!
//! This closes the "customer actually pays for usage" loop. Every
//! `NANOVM_BILLING_REPORT_SECS`, the reporter:
//!
//! 1. Snapshots `nanovm_forks_total_by_org` from [`crate::Metrics`].
//! 2. Diffs against the prior tick's snapshot → per-org delta.
//! 3. For each org with a positive delta, looks up the customer's
//!    `subscription_item_id` (persisted by the webhook handler on
//!    `customer.subscription.*` events; see `parse_subscription_object`).
//! 4. POSTs `usage_records` to Stripe with `action=increment`.
//!
//! ## Design decisions
//!
//! **Off by default.** Set `NANOVM_BILLING_REPORT_SECS=60` to enable.
//! An unset var → no background task → identical shape to today's
//! behaviour, and no accidental Stripe traffic in dev.
//!
//! **No persistent cursor.** The reporter keeps the last-seen
//! snapshot in memory and diffs against it. On restart, the first
//! tick's delta is 0 (we didn't see the counter climb across the
//! restart), which means at-most-once reporting per tick and
//! occasional "lost" forks equal to `(counter_at_shutdown -
//! counter_at_previous_tick)`. Correctness argument: the counter is
//! a Prometheus monotonic counter, but the whole `Metrics` struct is
//! process-local; restarts zero it. So the counter's "true" value
//! at restart is 0. First tick after restart correctly reports 0.
//!
//! **Idempotency key** = `(subscription_item_id, timestamp_secs)`.
//! Stripe deduplicates on this key for 24 h, so a retry after a
//! transient 429 or 5xx can't double-bill.
//!
//! **Failure handling.** Errors on individual customers log at
//! `warn` and don't block other customers; the next tick retries.
//! A total Stripe outage translates to at most one billing-period's
//! worth of delayed reports.

#![cfg(feature = "billing")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::{BillingStore, StripeClient};
use crate::Metrics;

/// Default reporter tick interval. 60 seconds keeps Stripe's rate
/// limits comfortable (~1 req/customer/min) while giving customers
/// near-real-time usage in their dashboard.
pub const DEFAULT_REPORT_SECS: u64 = 60;

/// Env var that enables the reporter. When unset (or `0`), no
/// background task is spawned.
pub const REPORT_INTERVAL_ENV: &str = "NANOVM_BILLING_REPORT_SECS";

/// Config for the reporter, filled at binary startup.
#[derive(Debug, Clone)]
pub struct UsageReporterConfig {
    /// How often to snapshot + flush.
    pub interval: Duration,
}

impl UsageReporterConfig {
    /// Read `NANOVM_BILLING_REPORT_SECS`. See [`parse`](Self::parse)
    /// for the exact semantics — this is a thin env-var wrapper.
    pub fn from_env() -> Option<Self> {
        Self::parse(std::env::var(REPORT_INTERVAL_ENV).ok().as_deref())
    }

    /// Parse a raw interval value. Unset (`None`) / empty / `"0"` →
    /// `None` (reporter disabled). Any positive integer → `Some`.
    /// Malformed / negative → warn + `None` so a typo doesn't run
    /// the reporter at some accidental cadence.
    ///
    /// Split out from `from_env` so tests can drive the parser
    /// without mutating process env — the workspace `forbid(unsafe)`
    /// posture rules out `std::env::set_var` in tests.
    pub fn parse(raw: Option<&str>) -> Option<Self> {
        let raw = raw?;
        if raw.is_empty() || raw == "0" {
            return None;
        }
        match raw.parse::<u64>() {
            Ok(n) if n > 0 => Some(Self {
                interval: Duration::from_secs(n),
            }),
            _ => {
                tracing::warn!(
                    value = %raw,
                    "{REPORT_INTERVAL_ENV}: not a positive integer; reporter disabled"
                );
                None
            }
        }
    }
}

/// Handle to a running reporter task. Drop it to keep the task
/// running for the process lifetime; call [`shutdown`] before drop
/// if you want the loop to exit cleanly.
#[derive(Debug)]
pub struct UsageReporterHandle {
    _stop: tokio::sync::oneshot::Sender<()>,
    _task: tokio::task::JoinHandle<()>,
}

impl UsageReporterHandle {
    /// Signal the reporter task to stop after the current tick
    /// completes. The handle drops after this; the task detaches.
    pub fn shutdown(self) {
        let _ = self._stop.send(());
    }
}

/// Spawn the reporter as a background tokio task. Returns a handle
/// that keeps the task alive until dropped.
///
/// `metrics` is the process-wide `Arc<Metrics>` (same one the
/// `/metrics` endpoint reads). `store` and `stripe` come from
/// [`crate::billing::BillingCtx`].
pub fn spawn(
    config: UsageReporterConfig,
    metrics: Arc<Metrics>,
    store: Arc<dyn BillingStore>,
    stripe: Arc<StripeClient>,
) -> UsageReporterHandle {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.interval);
        // First tick fires immediately; discard so the initial state
        // becomes the baseline instead of getting reported as one
        // huge delta.
        interval.tick().await;
        let mut prior_snapshot: HashMap<String, u64> = HashMap::new();
        loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    tracing::info!("usage reporter: shutdown");
                    return;
                }
                _ = interval.tick() => {
                    let now_snapshot: HashMap<String, u64> = metrics
                        .forks_by_org_snapshot()
                        .into_iter()
                        .map(|(org, count, _sum_ms)| (org, count))
                        .collect();
                    let deltas = compute_deltas(&prior_snapshot, &now_snapshot);
                    prior_snapshot = now_snapshot;
                    if deltas.is_empty() {
                        continue;
                    }
                    let ts_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    for (org, delta) in deltas {
                        flush_one(store.as_ref(), &stripe, &org, delta, ts_secs).await;
                    }
                }
            }
        }
    });
    UsageReporterHandle {
        _stop: stop_tx,
        _task: task,
    }
}

/// Pure delta calculation, separated from IO so tests can pin it.
/// Returns only orgs whose delta is > 0 (drops zeros and negatives —
/// a negative would mean the counter went backward, which happens
/// on restart and mustn't produce a "negative usage" Stripe report).
fn compute_deltas(prior: &HashMap<String, u64>, now: &HashMap<String, u64>) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for (org, current) in now {
        let previous = prior.get(org).copied().unwrap_or(0);
        if *current > previous {
            out.push((org.clone(), current - previous));
        }
    }
    // Deterministic ordering makes tests + logs stable.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Flush one org's delta. Failures land in tracing but don't propagate
/// — the loop must survive a bad customer to keep flushing the rest.
async fn flush_one(
    store: &dyn BillingStore,
    stripe: &StripeClient,
    org: &str,
    delta: u64,
    timestamp_secs: i64,
) {
    let org_id = crate::auth::OrgId::new(org);
    let customer_id = match store.get_customer(&org_id) {
        Some(c) => c,
        None => {
            // Org has forks but never signed up for Stripe billing —
            // typically a self-hosted or invited-manually org. Debug
            // log, no error.
            tracing::debug!(
                org,
                delta,
                "usage reporter: skipping org with no stripe customer"
            );
            return;
        }
    };
    let sub = match store.get_subscription(&customer_id) {
        Some(s) => s,
        None => {
            tracing::debug!(
                org,
                customer_id,
                delta,
                "usage reporter: skipping customer with no subscription"
            );
            return;
        }
    };
    let item_id = match sub.subscription_item_id.as_deref() {
        Some(id) => id,
        None => {
            // Migrated in from schema v3 (no item id) OR Stripe
            // hasn't sent a `customer.subscription.updated` yet.
            // Warn so ops can investigate if it persists.
            tracing::warn!(
                org,
                customer_id,
                delta,
                "usage reporter: subscription has no item id; skipping"
            );
            return;
        }
    };
    let key = format!("{item_id}-{timestamp_secs}");
    match stripe
        .report_usage_record(item_id, delta, timestamp_secs, &key)
        .await
    {
        Ok(rec) => tracing::info!(
            org,
            customer_id,
            subscription_item_id = item_id,
            usage_record_id = %rec.id,
            delta,
            "usage reporter: reported delta"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            org,
            customer_id,
            subscription_item_id = item_id,
            delta,
            "usage reporter: report failed; will retry next tick"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn delta_is_current_minus_prior_for_present_orgs() {
        let prior = snap(&[("acme", 10), ("globex", 5)]);
        let now = snap(&[("acme", 15), ("globex", 7)]);
        let d = compute_deltas(&prior, &now);
        assert_eq!(d, vec![("acme".into(), 5), ("globex".into(), 2)]);
    }

    #[test]
    fn delta_treats_new_org_as_all_current() {
        let prior = snap(&[]);
        let now = snap(&[("acme", 12)]);
        let d = compute_deltas(&prior, &now);
        assert_eq!(d, vec![("acme".into(), 12)]);
    }

    #[test]
    fn delta_drops_zero_and_negative() {
        // Zero delta.
        let d = compute_deltas(&snap(&[("acme", 10)]), &snap(&[("acme", 10)]));
        assert!(d.is_empty());
        // Backward — counter reset (e.g. process restart). Must not
        // produce a negative usage report.
        let d = compute_deltas(&snap(&[("acme", 10)]), &snap(&[("acme", 3)]));
        assert!(d.is_empty());
    }

    #[test]
    fn delta_is_sorted_by_org() {
        let now = snap(&[("z", 1), ("a", 1), ("m", 1)]);
        let d = compute_deltas(&snap(&[]), &now);
        let names: Vec<&str> = d.iter().map(|(o, _)| o.as_str()).collect();
        assert_eq!(names, vec!["a", "m", "z"]);
    }

    #[test]
    fn config_parse_disabled_when_unset_zero_or_malformed() {
        assert!(UsageReporterConfig::parse(None).is_none());
        assert!(UsageReporterConfig::parse(Some("")).is_none());
        assert!(UsageReporterConfig::parse(Some("0")).is_none());
        assert!(UsageReporterConfig::parse(Some("not-a-number")).is_none());
        assert!(UsageReporterConfig::parse(Some("-5")).is_none());
    }

    #[test]
    fn config_parse_accepts_positive_integer() {
        let c = UsageReporterConfig::parse(Some("30")).expect("30 is a valid interval");
        assert_eq!(c.interval, Duration::from_secs(30));
        let c = UsageReporterConfig::parse(Some("300")).expect("300 is a valid interval");
        assert_eq!(c.interval, Duration::from_secs(300));
    }
}
