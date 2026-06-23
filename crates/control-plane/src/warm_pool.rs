//! Pre-warmed VM pool for snapshot forks.
//!
//! `POST /v1/snapshots/:id/fork` is the headline product op — on real KVM
//! a cold restore costs ~7-15 ms (memory map + vCPU setup). For
//! eval-style workloads that fan a single snapshot out to thousands of
//! children, we can hide that latency behind a background pool of
//! already-restored VMs and hand one out on each request, dropping the
//! customer-visible fork to sub-millisecond.
//!
//! Sizing:
//!
//! - `NANOVM_WARM_POOL_PER_SNAPSHOT` — target depth maintained per
//!   snapshot. Default `0` (disabled). Real workloads pick `4`-`16`.
//!
//! Lifecycle:
//!
//! - The pool warms lazily: the first miss against a snapshot kicks the
//!   first refill, so subsequent forks hit the warm path. (Pre-warming
//!   on snapshot creation would be cheaper for steady-state, but the
//!   first fork still tells us we *will* be forking — no allocations
//!   for snapshots no one is actively forking.)
//! - When the caller deletes a snapshot, [`WarmPool::drain`] destroys
//!   every pre-restored VM for that snapshot before the underlying
//!   `delete_snapshot` runs. Without this, the parent snapshot would
//!   vanish out from under the warm children.
//! - On process shutdown the OS reclaims everything. We deliberately
//!   don't run a synchronous drain on signal — graceful shutdown is
//!   already best-effort here, and a fast restart matters more than
//!   tidy queue accounting.
//!
//! Refill is bounded: each take inspects `(depth + in-flight)` against
//! `per_snapshot` and only spawns enough tasks to close the gap. Refill
//! failures (e.g. snapshot deleted out from under us) are logged at
//! `debug` and the in-flight counter is released; the next take retries.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tracing::{debug, warn};
use vm_core::{Hypervisor, SnapshotId, VmHandle};

/// Default target depth per snapshot. `0` keeps the pool disabled — no
/// background tasks, every fork takes the cold path.
pub const DEFAULT_WARM_PER_SNAPSHOT: usize = 0;

/// Pre-warmed VM pool. Shared across handlers via `Arc<WarmPool>`.
pub struct WarmPool {
    hypervisor: Arc<dyn Hypervisor>,
    per_snapshot: usize,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for WarmPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WarmPool")
            .field("per_snapshot", &self.per_snapshot)
            .field("hypervisor", &"<dyn Hypervisor>")
            .finish()
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// Pre-restored VMs ready to hand out, keyed by source snapshot.
    queues: HashMap<SnapshotId, VecDeque<VmHandle>>,
    /// Refill tasks currently in flight per snapshot. Caps spawn so a
    /// burst of takes can't oversaturate the backend.
    in_flight: HashMap<SnapshotId, usize>,
}

impl WarmPool {
    /// Construct a pool that maintains `per_snapshot` pre-restored VMs
    /// per source snapshot. `per_snapshot == 0` returns a disabled pool
    /// that always misses.
    pub fn new(hypervisor: Arc<dyn Hypervisor>, per_snapshot: usize) -> Arc<Self> {
        Arc::new(Self {
            hypervisor,
            per_snapshot,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Convenience: a disabled pool. Equivalent to `new(hv, 0)`. Used
    /// as the default in `AppState`.
    pub fn disabled(hypervisor: Arc<dyn Hypervisor>) -> Arc<Self> {
        Self::new(hypervisor, 0)
    }

    /// Build from env (`NANOVM_WARM_POOL_PER_SNAPSHOT`), falling back
    /// to [`DEFAULT_WARM_PER_SNAPSHOT`] on parse failure.
    pub fn from_env(hypervisor: Arc<dyn Hypervisor>) -> Arc<Self> {
        let n = std::env::var("NANOVM_WARM_POOL_PER_SNAPSHOT")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_WARM_PER_SNAPSHOT);
        Self::new(hypervisor, n)
    }

    /// `true` when the pool is disabled (target depth = 0).
    pub fn is_disabled(&self) -> bool {
        self.per_snapshot == 0
    }

    /// Target depth maintained per snapshot.
    pub fn per_snapshot(&self) -> usize {
        self.per_snapshot
    }

    /// Pop a pre-warmed VM for `snap`. `None` means either the pool is
    /// disabled, the snapshot has no warm entries yet, or the queue
    /// drained faster than refill could keep up. Whether or not the
    /// take hit, the call also kicks a refill toward `per_snapshot`.
    pub fn take(self: &Arc<Self>, snap: SnapshotId) -> Option<VmHandle> {
        if self.is_disabled() {
            return None;
        }
        let handle = {
            let mut inner = self.inner.lock().ok()?;
            inner.queues.get_mut(&snap).and_then(|q| q.pop_front())
        };
        // Top up regardless of hit/miss so a stream of takes keeps the
        // queue at depth instead of starving on the slow path.
        self.kick_refill(snap);
        handle
    }

    /// Destroy every pre-warmed VM for `snap` and forget the queue.
    /// Idempotent — calling for an unknown snapshot is a no-op. Call
    /// before the underlying `Hypervisor::delete_snapshot` so warm
    /// children are reaped before their parent disappears.
    pub fn drain(&self, snap: SnapshotId) {
        let queue: VecDeque<VmHandle> = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            inner.in_flight.remove(&snap);
            inner.queues.remove(&snap).unwrap_or_default()
        };
        for h in queue {
            if let Err(e) = self.hypervisor.destroy(h.id) {
                debug!(snap = snap.0, vm = h.id.0, error = %e, "warm-pool drain destroy");
            }
        }
    }

    /// Current ready-to-hand-out depth for `snap`. Observability /
    /// test helper; not used on the hot path.
    pub fn depth(&self, snap: SnapshotId) -> usize {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.queues.get(&snap).map(|q| q.len()))
            .unwrap_or(0)
    }

    fn kick_refill(self: &Arc<Self>, snap: SnapshotId) {
        if self.is_disabled() {
            return;
        }
        let target = self.per_snapshot;
        let needed = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            let have = inner.queues.get(&snap).map(|q| q.len()).unwrap_or(0);
            let in_flight = inner.in_flight.get(&snap).copied().unwrap_or(0);
            let need = target.saturating_sub(have + in_flight);
            if need > 0 {
                *inner.in_flight.entry(snap).or_insert(0) += need;
            }
            need
        };
        for _ in 0..needed {
            let this = Arc::clone(self);
            tokio::spawn(async move {
                let hv = Arc::clone(&this.hypervisor);
                // `restore` is blocking on real backends (memory map +
                // vCPU setup). Push it onto the blocking pool so we
                // don't stall the reactor for ~10 ms a pop.
                let result = tokio::task::spawn_blocking(move || hv.restore(snap)).await;
                if let Ok(mut inner) = this.inner.lock() {
                    let count = inner.in_flight.entry(snap).or_insert(0);
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        inner.in_flight.remove(&snap);
                    }
                }
                match result {
                    Ok(Ok(handle)) => {
                        if let Ok(mut inner) = this.inner.lock() {
                            inner.queues.entry(snap).or_default().push_back(handle);
                        }
                    }
                    Ok(Err(e)) => {
                        // Common case: caller deleted the snapshot
                        // between the take that kicked us and the
                        // restore. Not noisy enough for `warn`.
                        debug!(snap = snap.0, error = %e, "warm-pool refill failed");
                    }
                    Err(join_err) => {
                        warn!(snap = snap.0, error = %join_err, "warm-pool refill panicked");
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use vm_core::VmConfig;
    use vm_mock::MockHypervisor;

    /// Spin until `cond` returns `true` or `timeout` elapses.
    async fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        cond()
    }

    fn fresh_snapshot(hv: &dyn Hypervisor) -> SnapshotId {
        let h = hv.create_vm(&VmConfig::default()).unwrap();
        hv.start(h.id).unwrap();
        hv.snapshot(h.id).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disabled_pool_always_misses() {
        let hv: Arc<dyn Hypervisor> = Arc::new(MockHypervisor::new());
        let snap = fresh_snapshot(&*hv);
        let pool = WarmPool::disabled(Arc::clone(&hv));
        assert!(pool.is_disabled());
        for _ in 0..5 {
            assert!(pool.take(snap).is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_take_misses_then_pool_warms() {
        let hv: Arc<dyn Hypervisor> = Arc::new(MockHypervisor::new());
        let snap = fresh_snapshot(&*hv);
        let pool = WarmPool::new(Arc::clone(&hv), 2);

        // Cold: queue empty, take returns None, refill kicks.
        assert!(pool.take(snap).is_none());

        // Refill should drive depth toward target (= 2).
        let warmed = wait_until(|| pool.depth(snap) == 2, Duration::from_secs(1)).await;
        assert!(warmed, "pool didn't reach target depth in time");

        // Hot: two takes should hit.
        assert!(pool.take(snap).is_some());
        assert!(pool.take(snap).is_some());

        // Each take re-kicks refill; the queue should return to depth.
        let refilled = wait_until(|| pool.depth(snap) == 2, Duration::from_secs(1)).await;
        assert!(refilled, "pool didn't refill after takes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_destroys_warm_children_and_zeroes_depth() {
        let hv: Arc<dyn Hypervisor> = Arc::new(MockHypervisor::new());
        let snap = fresh_snapshot(&*hv);
        let pool = WarmPool::new(Arc::clone(&hv), 3);

        // Prime the pool.
        let _ = pool.take(snap);
        wait_until(|| pool.depth(snap) == 3, Duration::from_secs(1)).await;

        let pre = hv.list_vms().unwrap().len();
        assert!(pre >= 3, "expected at least the warm children + base VM");

        pool.drain(snap);
        assert_eq!(pool.depth(snap), 0);

        let post = hv.list_vms().unwrap().len();
        assert!(
            post + 3 <= pre,
            "drain didn't destroy warm children: pre={pre} post={post}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refill_concurrency_is_bounded_by_target_depth() {
        let hv: Arc<dyn Hypervisor> = Arc::new(MockHypervisor::new());
        let snap = fresh_snapshot(&*hv);
        let pool = WarmPool::new(Arc::clone(&hv), 4);

        // Fire many takes in quick succession. Even though every take
        // kicks refill, the (depth + in-flight) accounting must keep
        // total restores bounded to (takes + target).
        for _ in 0..16 {
            let _ = pool.take(snap);
        }
        // Let the system quiesce.
        wait_until(|| pool.depth(snap) == 4, Duration::from_secs(2)).await;
        assert_eq!(pool.depth(snap), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refill_against_deleted_snapshot_does_not_panic() {
        let hv: Arc<dyn Hypervisor> = Arc::new(MockHypervisor::new());
        let snap = fresh_snapshot(&*hv);
        let pool = WarmPool::new(Arc::clone(&hv), 2);

        // Kick refill, then yank the snapshot.
        let _ = pool.take(snap);
        hv.delete_snapshot(snap).unwrap();

        // Give the (now-doomed) refill tasks time to settle. They
        // should fail gracefully — no panic, no stuck in-flight.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.depth(snap), 0);
    }
}
