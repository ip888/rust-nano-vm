//! Per-token token-bucket quota for `POST /v1/snapshots/:id/fork`.
//!
//! Fork is the expensive op — it spins up a fresh VM behind the scenes,
//! and (with the KVM backend) costs ~7-15 ms of CPU + a few KiB of
//! per-fork private memory. A misbehaving or runaway customer must not
//! be able to monopolise the host, so we gate `/fork` separately from
//! the cheap CRUD endpoints with a per-bearer-token token bucket.
//!
//! Defaults are taken from env vars at startup:
//!
//! - `NANOVM_FORK_RPS`   — sustained forks per second per token (default `10`).
//! - `NANOVM_FORK_BURST` — bucket capacity in forks (default = `RPS`).
//!
//! Setting `NANOVM_FORK_RPS=0` disables the quota (every request passes
//! through); the binary logs a `WARN` so operators notice. Quotas are
//! process-local; a multi-replica deployment should put a shared
//! rate-limiter in front (Redis / Envoy / etc.).

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

/// Per-token fork quota. Shared across handlers via `Arc<ForkQuota>`.
#[derive(Debug)]
pub struct ForkQuota {
    rps: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl ForkQuota {
    /// Construct a new quota with the given sustained rate and burst
    /// capacity. `rps <= 0.0` disables the quota.
    pub fn new(rps: f64, burst: u32) -> Self {
        Self {
            rps,
            burst: f64::from(burst),
            buckets: Mutex::new(HashMap::new()),
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
        let bucket = buckets
            .entry(token.to_owned())
            .or_insert_with(|| Bucket::new(self.burst));
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rps).min(self.burst);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            // Round up so Retry-After is never zero — clients that obey it
            // mustn't loop instantly and hammer us.
            let deficit = 1.0 - bucket.tokens;
            let secs = (deficit / self.rps).ceil().max(1.0) as u64;
            Err(secs)
        }
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
}
