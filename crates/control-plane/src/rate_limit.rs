//! Per-token rate limiter for `/v1/*`.
//!
//! Sits in the same `route_layer` stack as `auth::require_token`, just
//! after auth: each accepted bearer token gets its own token-bucket,
//! refilled at `rps` tokens/second up to `burst`. When the bucket is
//! empty, the request returns a [`ApiError::TooManyRequests`] (HTTP 429)
//! that renders through the same structured envelope as every other
//! error.
//!
//! When the auth middleware short-circuits (no `NANOVM_API_TOKENS` set —
//! "auth disabled" dev mode), this middleware also short-circuits.
//! Production deployments should set `NANOVM_API_TOKENS` *and* a
//! conservative `NANOVM_RATE_LIMIT_RPS`.
//!
//! Single-process only — backing store is an `Arc<Mutex<HashMap<...>>>`.
//! Multi-replica deployments will want an external (Redis-backed)
//! limiter; pluggable via a follow-up PR once we have a real prod
//! deployment.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use axum::{
    extract::{Extension, Request},
    middleware::Next,
    response::Response,
};

use crate::error::ApiError;

/// Default per-token rate when [`RateLimit::from_env`] sees no
/// `NANOVM_RATE_LIMIT_RPS`. Conservative — most agent workloads burst
/// snapshot-fork at 50–200 rps; raise as needed.
pub const DEFAULT_RATE_LIMIT_RPS: u32 = 100;

/// Default burst when [`RateLimit::from_env`] sees no
/// `NANOVM_RATE_LIMIT_BURST`. One second's worth of requests at the
/// default rps — enough to absorb a small fan-out without rejecting
/// the first request.
pub const DEFAULT_RATE_LIMIT_BURST: u32 = DEFAULT_RATE_LIMIT_RPS;

/// Configured rate limiter: token-bucket parameters plus the in-memory
/// state map. Cheap to clone via `Arc`.
#[derive(Debug)]
pub struct RateLimit {
    rps: u32,
    burst: u32,
    state: Mutex<HashMap<String, Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Current token count (fractional — refill rate is `rps/sec`).
    tokens: f64,
    /// Last refill instant.
    updated: Instant,
}

impl RateLimit {
    /// Construct with explicit `rps` (refill rate) and `burst` (bucket
    /// capacity). When `rps == 0` the limiter is **disabled** — every
    /// request passes. This matches the auth-disabled convention.
    pub fn new(rps: u32, burst: u32) -> Self {
        Self {
            rps,
            burst: burst.max(1),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Read `NANOVM_RATE_LIMIT_RPS` / `NANOVM_RATE_LIMIT_BURST` from
    /// the environment. Unset → defaults. Unparseable → defaults
    /// (operator should notice via the startup log line the binary
    /// emits).
    pub fn from_env() -> Self {
        let rps = std::env::var("NANOVM_RATE_LIMIT_RPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RATE_LIMIT_RPS);
        let burst = std::env::var("NANOVM_RATE_LIMIT_BURST")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RATE_LIMIT_BURST);
        Self::new(rps, burst)
    }

    /// `true` when this limiter passes every request through (rps == 0).
    pub fn is_disabled(&self) -> bool {
        self.rps == 0
    }

    /// Configured rps (refill rate).
    pub fn rps(&self) -> u32 {
        self.rps
    }

    /// Configured burst (bucket capacity).
    pub fn burst(&self) -> u32 {
        self.burst
    }

    /// Try to consume one token for `key`. Returns `Ok(())` if a token
    /// was available; `Err` carries the seconds-until-retry hint the
    /// caller surfaces in the response.
    fn try_acquire(&self, key: &str, now: Instant) -> Result<(), f64> {
        if self.rps == 0 {
            return Ok(());
        }
        let mut map = self.state.lock().expect("rate-limit state poisoned");
        let bucket = map.entry(key.to_owned()).or_insert(Bucket {
            tokens: self.burst as f64,
            updated: now,
        });
        // Refill since `updated`. f64 math is fine — millisecond
        // granularity is plenty for an HTTP rate limiter and avoids
        // integer-overflow corner cases on long-idle keys.
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rps as f64).min(self.burst as f64);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let need = 1.0 - bucket.tokens;
            let wait = need / self.rps as f64;
            Err(wait)
        }
    }
}

/// Axum middleware that consumes one token per request from the bucket
/// keyed by the request's bearer token. Install as a `route_layer` on
/// `/v1/*` after [`super::auth::require_token`] — that ordering
/// guarantees the bearer token is present and stripped of the
/// `"Bearer "` prefix before we key on it.
///
/// When the limiter is disabled (`rps == 0`), the request has no
/// `Authorization: Bearer ...` header (auth-disabled mode), or the
/// `RateLimit` extension was never installed (library consumers / tests
/// that don't care about throttling), the middleware passes through
/// unchanged. Mirrors the auth middleware's degrade-gracefully shape.
pub async fn require_rate(
    limiter: Option<Extension<std::sync::Arc<RateLimit>>>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(Extension(limiter)) = limiter else {
        return Ok(next.run(req).await);
    };
    if limiter.is_disabled() {
        return Ok(next.run(req).await);
    }
    let key = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or(""); // auth-disabled mode: every caller shares one bucket
    if key.is_empty() {
        // No token → either auth is off (in which case the auth
        // middleware would already have allowed the request) or the
        // request is malformed (auth middleware rejected it before us).
        // Either way, don't decrement.
        return Ok(next.run(req).await);
    }
    match limiter.try_acquire(key, Instant::now()) {
        Ok(()) => Ok(next.run(req).await),
        Err(retry_after) => Err(ApiError::TooManyRequests {
            retry_after_secs: retry_after.max(0.001),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn disabled_limiter_always_passes() {
        let rl = RateLimit::new(0, 1);
        assert!(rl.is_disabled());
        for _ in 0..1_000 {
            assert!(rl.try_acquire("k", Instant::now()).is_ok());
        }
    }

    #[test]
    fn burst_capped_then_refills_at_rps() {
        // 10 rps, burst 5 — first 5 requests pass immediately, sixth
        // fails, then after 100ms one more token has refilled.
        let rl = RateLimit::new(10, 5);
        let t0 = Instant::now();
        for i in 0..5 {
            assert!(rl.try_acquire("k", t0).is_ok(), "burst {i}");
        }
        let err = rl.try_acquire("k", t0).unwrap_err();
        // Retry hint is "1 token / 10 rps = 0.1 sec".
        assert!((err - 0.1).abs() < 1e-6, "retry hint was {err}");
        // 100ms later: one more token available.
        let t1 = t0 + Duration::from_millis(100);
        assert!(rl.try_acquire("k", t1).is_ok());
        // ... and the bucket is empty again.
        assert!(rl.try_acquire("k", t1).is_err());
    }

    #[test]
    fn different_keys_have_independent_buckets() {
        let rl = RateLimit::new(1, 1);
        let t0 = Instant::now();
        assert!(rl.try_acquire("alice", t0).is_ok());
        assert!(rl.try_acquire("alice", t0).is_err());
        // Bob's bucket is full even though Alice's is empty.
        assert!(rl.try_acquire("bob", t0).is_ok());
        assert!(rl.try_acquire("bob", t0).is_err());
    }

    #[test]
    fn long_idle_does_not_overfill_past_burst() {
        let rl = RateLimit::new(100, 5);
        let t0 = Instant::now();
        // Drain.
        for _ in 0..5 {
            assert!(rl.try_acquire("k", t0).is_ok());
        }
        // Sleep ~1 day. Bucket caps at burst, not 100*86400 tokens.
        let t1 = t0 + Duration::from_secs(86_400);
        for _ in 0..5 {
            assert!(rl.try_acquire("k", t1).is_ok());
        }
        // 6th must fail again.
        assert!(rl.try_acquire("k", t1).is_err());
    }
}
