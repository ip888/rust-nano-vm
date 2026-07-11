//! Token-bucket quotas for `POST /v1/snapshots/:id/fork`.
//!
//! Fork is the expensive op — it spins up a fresh VM behind the scenes,
//! and (with the KVM backend) costs ~7-15 ms of CPU + a few KiB of
//! per-fork private memory. A misbehaving or runaway customer must not
//! be able to monopolise the host, so we gate `/fork` separately from
//! the cheap CRUD endpoints with two independent token buckets:
//!
//!  1. **Per-token** — cheap safety net against a runaway CI script
//!     hammering a single API key. Keyed by bearer, sized by env
//!     defaults, always on.
//!  2. **Per-org** — the billing-tier enforcement. Keyed by `OrgId`,
//!     sized by the caller's Stripe plan (`PlanTier.rps` from
//!     [`crate::billing::PlanTiers`], resolved per-request via
//!     [`crate::billing::resolve_plan`]). Without the `billing`
//!     feature, or when the org has no mapped tier, this falls back to
//!     the env default — semantically equivalent to the pre-tier
//!     behaviour.
//!
//! Both must pass for a fork to proceed. Either failing throttles the
//! request with 429 + `Retry-After`.
//!
//! Defaults are taken from env vars at startup:
//!
//! - `NANOVM_FORK_RPS`   — sustained forks per second per token (default `10`).
//! - `NANOVM_FORK_BURST` — bucket capacity in forks (default = `RPS`).
//!
//! Setting `NANOVM_FORK_RPS=0` disables both quotas (every request
//! passes through); the binary logs a `WARN` so operators notice.
//! Quotas are process-local; a multi-replica deployment should put a
//! shared rate-limiter in front (Redis / Envoy / etc.).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Default sustained fork-rate per token in forks-per-second.
pub const DEFAULT_FORK_RPS: f64 = 10.0;

#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl Bucket {
    fn new(burst: f64) -> Self {
        Self {
            tokens: burst,
            last_refill: Instant::now(),
        }
    }
}

/// Per-token + per-org fork quota. Shared across handlers via
/// `Arc<ForkQuota>`. Both buckets are optional per call: a caller
/// without a bearer skips the per-token check; a caller without an org
/// (unauthenticated / auth-off) skips the per-org check.
#[derive(Debug)]
pub struct ForkQuota {
    rps: f64,
    burst: f64,
    /// Buckets keyed by bearer token — protects against a runaway key.
    buckets: Mutex<HashMap<String, Bucket>>,
    /// Buckets keyed by org id — the tier-enforcement dimension.
    /// Sizing is per-call so a Pro org's bucket capacity can grow
    /// mid-flight when their subscription upgrades without a restart.
    org_buckets: Mutex<HashMap<String, Bucket>>,
}

impl ForkQuota {
    /// Construct a new quota with the given sustained rate and burst
    /// capacity. `rps <= 0.0` disables the quota.
    pub fn new(rps: f64, burst: u32) -> Self {
        Self {
            rps,
            burst: f64::from(burst),
            buckets: Mutex::new(HashMap::new()),
            org_buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Build from env vars (`NANOVM_FORK_RPS`, `NANOVM_FORK_BURST`),
    /// falling back to the defaults documented in the module header.
    pub fn from_env() -> Self {
        let rps = std::env::var("NANOVM_FORK_RPS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| *v >= 0.0)
            .unwrap_or(DEFAULT_FORK_RPS);
        let burst = std::env::var("NANOVM_FORK_BURST")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_else(|| rps.ceil().max(1.0) as u32);
        Self::new(rps, burst)
    }

    /// `true` when the quota is disabled (rps = 0).
    pub fn is_disabled(&self) -> bool {
        self.rps <= 0.0
    }

    /// Try to consume one fork token for `bearer`. Returns
    /// `Err(retry_after_secs)` (rounded up, minimum 1) when the bucket
    /// is empty. `bearer == None` (auth-off mode) passes through —
    /// there's no identity to key the bucket on.
    pub fn try_acquire(&self, bearer: Option<&str>) -> Result<(), u64> {
        if self.is_disabled() {
            return Ok(());
        }
        let Some(token) = bearer else {
            return Ok(());
        };
        let mut buckets = self.buckets.lock().expect("fork-quota mutex poisoned");
        take_one(&mut buckets, token, self.rps, self.burst)
    }

    /// Try to consume one fork token from the *per-org* bucket, with
    /// tier-overridden rate + capacity.
    ///
    /// `rps_override` / `burst_override` are the caller's plan-tier
    /// values (from [`crate::billing::PlanTier`]); `None` on either
    /// falls back to the env default. Combines with
    /// [`try_acquire`](Self::try_acquire) — the handler must call both.
    ///
    /// This is the enforcement dimension that scales with the
    /// customer's Stripe plan: a Pro org's bucket refills faster than
    /// a Free org's, and a customer that mints extra API tokens still
    /// hits the same org-level ceiling.
    pub fn try_acquire_org(
        &self,
        org: &str,
        rps_override: Option<f64>,
        burst_override: Option<u32>,
    ) -> Result<(), u64> {
        if self.is_disabled() {
            return Ok(());
        }
        // A mis-configured tier (rps <= 0 or NaN, or burst = 0) must not
        // divide by zero or permanently deny the caller — ignore such
        // overrides and fall back to the env default. `self.rps` is
        // guaranteed > 0 here because `is_disabled()` above already
        // short-circuited the zero case.
        let rps = rps_override
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(self.rps);
        let burst = burst_override
            .filter(|b| *b > 0)
            .map(f64::from)
            .unwrap_or(self.burst);
        let mut buckets = self.org_buckets.lock().expect("fork-quota mutex poisoned");
        take_one(&mut buckets, org, rps, burst)
    }
}

/// Refill and consume one token from `buckets[key]`, creating the
/// bucket at full capacity on first touch. Shared between the per-token
/// and per-org paths.
fn take_one(
    buckets: &mut HashMap<String, Bucket>,
    key: &str,
    rps: f64,
    burst: f64,
) -> Result<(), u64> {
    let bucket = buckets
        .entry(key.to_owned())
        .or_insert_with(|| Bucket::new(burst));
    let now = Instant::now();
    let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
    bucket.tokens = (bucket.tokens + elapsed * rps).min(burst);
    bucket.last_refill = now;
    if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        Ok(())
    } else {
        // Round up so Retry-After is never zero — clients that obey it
        // mustn't loop instantly and hammer us.
        let deficit = 1.0 - bucket.tokens;
        let secs = (deficit / rps).ceil().max(1.0) as u64;
        Err(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn disabled_quota_passes_everything() {
        let q = ForkQuota::new(0.0, 0);
        for _ in 0..1000 {
            assert!(q.try_acquire(Some("alpha")).is_ok());
        }
    }

    #[test]
    fn no_bearer_means_quota_skipped() {
        let q = ForkQuota::new(1.0, 1);
        for _ in 0..1000 {
            assert!(q.try_acquire(None).is_ok());
        }
    }

    #[test]
    fn burst_then_throttle() {
        let q = ForkQuota::new(1.0, 3);
        assert!(q.try_acquire(Some("a")).is_ok());
        assert!(q.try_acquire(Some("a")).is_ok());
        assert!(q.try_acquire(Some("a")).is_ok());
        let retry = q.try_acquire(Some("a")).unwrap_err();
        assert!(retry >= 1, "Retry-After should be at least 1 second");
    }

    #[test]
    fn buckets_are_isolated_between_tokens() {
        let q = ForkQuota::new(1.0, 1);
        assert!(q.try_acquire(Some("alpha")).is_ok());
        assert!(q.try_acquire(Some("beta")).is_ok());
        assert!(q.try_acquire(Some("alpha")).is_err());
    }

    #[test]
    fn refill_replenishes_the_bucket() {
        let q = ForkQuota::new(50.0, 1);
        assert!(q.try_acquire(Some("a")).is_ok());
        assert!(q.try_acquire(Some("a")).is_err());
        sleep(Duration::from_millis(50));
        assert!(q.try_acquire(Some("a")).is_ok());
    }

    #[test]
    fn org_bucket_uses_env_default_when_no_override() {
        let q = ForkQuota::new(1.0, 2);
        // Two forks fit the burst, third throttles.
        assert!(q.try_acquire_org("acme", None, None).is_ok());
        assert!(q.try_acquire_org("acme", None, None).is_ok());
        assert!(q.try_acquire_org("acme", None, None).is_err());
    }

    #[test]
    fn org_bucket_uses_tier_override_when_provided() {
        let q = ForkQuota::new(1.0, 1);
        // Pro tier: 100 rps burst=5 → 5 forks fit before throttle.
        for _ in 0..5 {
            assert!(q.try_acquire_org("acme", Some(100.0), Some(5)).is_ok());
        }
        assert!(q.try_acquire_org("acme", Some(100.0), Some(5)).is_err());
    }

    #[test]
    fn org_buckets_are_isolated_between_orgs() {
        let q = ForkQuota::new(1.0, 1);
        assert!(q.try_acquire_org("acme", None, None).is_ok());
        assert!(q.try_acquire_org("globex", None, None).is_ok());
        // acme is empty, globex is empty — but each org is its own bucket.
        assert!(q.try_acquire_org("acme", None, None).is_err());
        assert!(q.try_acquire_org("globex", None, None).is_err());
    }

    #[test]
    fn org_and_token_buckets_are_independent() {
        // Token bucket: 3 burst; org bucket: 1 burst. Handler is
        // expected to call both; the tighter one throttles first.
        let q = ForkQuota::new(1.0, 3);
        // Per-token has plenty of headroom, but per-org burst=1 caps
        // this org at one fork.
        assert!(q.try_acquire(Some("t")).is_ok());
        assert!(q.try_acquire_org("acme", None, Some(1)).is_ok());
        assert!(q.try_acquire(Some("t")).is_ok()); // token still ok
        assert!(q.try_acquire_org("acme", None, Some(1)).is_err()); // org empty
    }

    #[test]
    fn org_zero_rps_override_falls_back_to_env_default() {
        // A mis-configured tier (rps = 0) mustn't hard-lock the caller —
        // the override is ignored and the bucket uses the env default
        // sustained rate. With env rps=1.0, burst=1, we can fork once
        // then throttle — same shape as any normal bucket, no permanent
        // deny.
        let q = ForkQuota::new(1.0, 1);
        assert!(q.try_acquire_org("acme", Some(0.0), None).is_ok());
        assert!(q.try_acquire_org("acme", Some(0.0), None).is_err());
    }

    #[test]
    fn org_disabled_when_env_rps_is_zero() {
        let q = ForkQuota::new(0.0, 0);
        for _ in 0..1000 {
            assert!(q.try_acquire_org("acme", None, None).is_ok());
        }
    }
}
