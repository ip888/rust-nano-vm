//! Marketplace fork endpoint — `POST /v1/marketplace/snapshots/:name/fork`.
//!
//! Turns a listed marketplace entry into a **usable** VM. This is the
//! wedge feature vs AWS Lambda MicroVMs: the operator publishes a
//! `snapshot.tar.gz` (manifest.json + memory.cow) at some `snapshot_url`
//! in the marketplace catalogue, and any tenant can fork from it in
//! sub-second cold time (first fork per tenant) then ~12 ms after that.
//!
//! ## Flow
//!
//! 1. Auth (tenant bearer → [`OrgId`] in extensions).
//! 2. Look up the entry by URL segment `:name`. 404 if unknown; 501 if
//!    the entry has no `snapshot_url` (listing-only entry).
//! 3. Apply per-token + per-org fork quota. On 429, return `retry-after`.
//!    Enforced BEFORE the cold path so an exhausted-bucket caller can't
//!    trigger a full tarball download by hammering this endpoint.
//! 4. Cache lookup: `(org, name, url) → local SnapshotId`. On hit, skip
//!    to step 7.
//! 5. On miss, STREAM the tarball (no full in-memory buffer) through a
//!    capping reader (default 2 GiB, `NANOVM_MARKETPLACE_MAX_BYTES`
//!    override) into a fresh scratch dir. Extraction rejects
//!    path-traversal, tar-bomb entry counts, gzip-bomb size, symlinks
//!    / hardlinks / device entries, and duplicate paths under
//!    normalization. Then [`Hypervisor::snapshot_adopt`](vm_core::Hypervisor::snapshot_adopt)
//!    into the backend under a fresh local id.
//! 6. Reconcile with the cache under lock; on race-loss delete the
//!    freshly-adopted snapshot + forget its ownership so we don't
//!    accumulate duplicate-tenant rows.
//! 7. Restore the local snapshot (warm-pool if hot, cold otherwise);
//!    record fork metrics + usage counters exactly like `/v1/snapshots/:id/fork`.
//! 8. Return [`ForkResponseDto`](crate::api::ForkResponseDto).
//!
//! ## Safety posture on the tarball download
//!
//! - `Content-Length > MAX_TARBALL_BYTES` → reject before streaming a
//!   byte. Callers can override via `NANOVM_MARKETPLACE_MAX_BYTES`.
//! - No `Content-Length` header → still stream, but the [`CappedRead`]
//!   adapter aborts the moment bytes-read passes the cap (guards
//!   against chunked-encoding truncation attacks).
//! - **No full in-memory buffer**: the HTTP body flows straight through
//!   gz decode → tar reader → per-file unpack. Peak memory is bounded
//!   to a couple of chunk-size buffers regardless of tarball size —
//!   concurrent cold forks don't OOM the box.
//! - Extract dir created with `create_dir` (NOT `create_dir_all`) so a
//!   pre-placed symlink at the scratch path cannot redirect writes.
//! - Extraction rejects any entry whose relative path escapes the target
//!   dir (`..` components or absolute paths) — standard tar-slip
//!   protection.
//! - Duplicate detection is on the NORMALIZED path (`.` components
//!   collapsed), so `manifest.json` and `./manifest.json` collide
//!   rather than silently overwriting each other.
//! - Extraction caps entry count at [`MAX_TARBALL_ENTRIES`] and
//!   uncompressed-total at the same byte limit so a gzip bomb can't fill
//!   the disk.
//! - HTTP timeouts: 30 s connect, no read timeout (large downloads are
//!   slow). Redirects are followed up to 5 hops (reqwest default).
//!
//! Marketplace URLs are **operator-configured**, not tenant-controlled,
//! so SSRF isn't in scope — a tenant can only pull URLs the operator
//! has already vetted by putting them in `NANOVM_MARKETPLACE_CONFIG`.
//!
//! ## Cache is process-local
//!
//! After control-plane restart the first fork per (org, name, url) will
//! re-download. Persisting the mapping in the [`crate::ownership`] store
//! (with a `source: "marketplace:python-3.12-ds@<sha>"` tag) is a
//! follow-up; it saves one cold pull per restart per tenant, which
//! isn't the hot path.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{rejection::PathRejection, Extension, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use vm_core::{Hypervisor, SnapshotId};

use crate::api::ForkResponseDto;
use crate::auth::OrgId;
use crate::error::ApiError;
use crate::routes::{extract_bearer, resolve_tier_limits, token_fingerprint, AppState};

/// Default cap on both the compressed download size AND the sum of
/// extracted-entry sizes. Chosen so a rootfs the size of the largest
/// baked images the marketplace publishes today (~200 MB) fits
/// comfortably while keeping accidental gigabyte-scale downloads
/// containable. Operators can override via `NANOVM_MARKETPLACE_MAX_BYTES`.
const DEFAULT_MAX_TARBALL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Cap on the number of entries in a marketplace tarball. A well-formed
/// snapshot tarball has ~2-3 entries (`manifest.json` + `memory.cow`
/// + optional vCPU state). Anything past 64 is a strong tar-bomb signal;
///   reject rather than trying to extract.
const MAX_TARBALL_ENTRIES: usize = 64;

/// Env var override for the byte cap. Value parses as `u64`.
const MAX_BYTES_ENV: &str = "NANOVM_MARKETPLACE_MAX_BYTES";

/// Handler for `POST /v1/marketplace/snapshots/:name/fork`.
pub(crate) async fn fork_marketplace_snapshot(
    State(state): State<AppState>,
    Extension(org): Extension<OrgId>,
    name: Result<AxumPath<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<ForkResponseDto>), ApiError> {
    let AxumPath(name) = name?;

    // 1. Catalogue lookup.
    let entry = state
        .marketplace()
        .get(&name)
        .cloned()
        .ok_or_else(|| ApiError::NotFound {
            code: "marketplace_not_found",
            message: format!("no marketplace snapshot named {name:?}"),
        })?;
    let snapshot_url = entry
        .snapshot_url
        .clone()
        .ok_or_else(|| ApiError::Unsupported {
            code: "snapshot_not_forkable",
            message: format!(
                "marketplace entry {name:?} has no `snapshot_url` — the publisher \
             lists it for discovery but has not yet supplied a forkable tarball"
            ),
        })?;

    // 2. Enforce fork quota BEFORE the (potentially expensive) cold-path
    //    pull. Doing quota-after-pull meant a quota-exhausted token could
    //    still trigger a full tarball download + extract + adopt (seconds
    //    + up to the byte cap) before getting 429 — turning quota from a
    //    cost-control mechanism into a decorative rejection. Ordered
    //    per-token first, then per-org (matches /v1/snapshots/:id/fork).
    let bearer = extract_bearer(&headers);
    if let Err(retry_after_secs) = state.fork_quota().try_acquire(bearer.as_deref()) {
        let fp = bearer
            .as_deref()
            .map(token_fingerprint)
            .unwrap_or_else(|| "anonymous".to_owned());
        state.metrics().record_throttled(&fp, org.as_str());
        return Err(ApiError::TooManyRequests {
            code: "fork_quota_exceeded",
            message: format!("fork quota exceeded; retry after {retry_after_secs} second(s)"),
            retry_after_secs,
        });
    }
    let (tier_rps, tier_burst) = resolve_tier_limits(&state, &org);
    if let Err(retry_after_secs) =
        state
            .fork_quota()
            .try_acquire_org(org.as_str(), tier_rps, tier_burst)
    {
        let fp = bearer
            .as_deref()
            .map(token_fingerprint)
            .unwrap_or_else(|| "anonymous".to_owned());
        state.metrics().record_throttled(&fp, org.as_str());
        return Err(ApiError::TooManyRequests {
            code: "fork_quota_exceeded",
            message: format!("org fork quota exceeded; retry after {retry_after_secs} second(s)"),
            retry_after_secs,
        });
    }

    // 3. Cache lookup — skip download on hit.
    let cache_key = (org.clone(), name.clone(), snapshot_url.clone());
    let cached_id = {
        let cache = state
            .marketplace_fork_cache
            .lock()
            .map_err(|_| ApiError::Internal("marketplace_fork_cache mutex poisoned"))?;
        cache.get(&cache_key).copied()
    };

    let snap_id = if let Some(id) = cached_id {
        id
    } else {
        // Cold path: download, extract, adopt. Runs on the blocking pool
        // because tarball extraction + snapshot_adopt do plain sync I/O.
        let hv = state.hypervisor().clone();
        let ownership = state.ownership().clone();
        let org_for_task = org.clone();
        let url_for_task = snapshot_url.clone();
        let name_for_task = name.clone();
        let max_bytes = max_tarball_bytes();

        let adopted = tokio::task::spawn_blocking(move || {
            pull_and_adopt(
                hv,
                ownership,
                org_for_task,
                name_for_task,
                url_for_task,
                max_bytes,
            )
        })
        .await
        .map_err(|e| ApiError::InternalDyn(format!("marketplace pull task panicked: {e}")))??;

        // Reconcile with the cache: another request may have won the race
        // and already installed a different id for this (org, name, url).
        // If so, our freshly-adopted snapshot is dead weight — delete it
        // from the hypervisor and forget its ownership record so the
        // tenant's `/v1/snapshots` listing and quota don't accumulate
        // race-loser rows on every concurrent first fork.
        let winner_id = {
            let mut cache = state
                .marketplace_fork_cache
                .lock()
                .map_err(|_| ApiError::Internal("marketplace_fork_cache mutex poisoned"))?;
            *cache.entry(cache_key).or_insert(adopted)
        };
        if winner_id != adopted {
            // Truly detached: DROP the JoinHandle so the request path
            // never waits on `delete_snapshot`. The caller already has
            // `winner_id` in hand — a slow hypervisor delete or a
            // panicking cleanup task must not couple to response
            // latency or leak a 500 to the client. Worst case is a
            // stale snapshot lingering in the backend that a periodic
            // reconciler can reap later; that trade is well worth
            // shaving the extra deletion time off first-fork p99.
            let hv = state.hypervisor().clone();
            let ownership = state.ownership().clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = hv.delete_snapshot(adopted) {
                    tracing::warn!(
                        snapshot = adopted.0,
                        error = %e,
                        "marketplace: race-loser snapshot delete failed"
                    );
                }
                ownership.forget_snapshot(adopted);
            });
        }
        winner_id
    };

    // 4. Restore — warm-pool first, cold restore fallback.
    let started = Instant::now();
    let handle = if let Some(h) = state.warm_pool().take(snap_id) {
        state.metrics().record_warm_hit();
        h
    } else {
        state.metrics().record_warm_miss();
        state.hypervisor().restore(snap_id)?
    };
    state.ownership().record_vm(handle.id, org.clone());
    let fork_ms = started.elapsed().as_millis() as u64;

    let fp = bearer
        .as_deref()
        .map(token_fingerprint)
        .unwrap_or_else(|| "anonymous".to_owned());
    state.metrics().record_fork(&fp, org.as_str(), fork_ms);

    let (fork_count, fork_total_ms) = if let Some(token) = bearer {
        let mut usage = state
            .fork_usage_lock()
            .map_err(|_| ApiError::Internal("fork_usage mutex poisoned"))?;
        let entry = usage.entry(token).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.total_ms = entry.total_ms.saturating_add(fork_ms);
        (entry.count, entry.total_ms)
    } else {
        (1u64, fork_ms)
    };

    Ok((
        StatusCode::CREATED,
        Json(ForkResponseDto {
            vm: handle.into(),
            fork_ms,
            fork_count,
            fork_total_ms,
        }),
    ))
}

/// Blocking cold-path: fetch the tarball, extract it into a scratch
/// dir, hand it to the backend, record ownership. Returns the local
/// `SnapshotId` on success.
fn pull_and_adopt(
    hv: Arc<dyn Hypervisor>,
    ownership: Arc<crate::OwnershipMap>,
    org: OrgId,
    entry_name: String,
    url: String,
    max_bytes: u64,
) -> Result<SnapshotId, ApiError> {
    tracing::info!(
        org = %org.as_str(),
        entry = %entry_name,
        url = %url,
        max_bytes,
        "marketplace: pulling snapshot tarball"
    );
    let scratch = scratch_dir(&entry_name);
    // Stream the response directly through the gzip + tar readers.
    // Bounded at every stage: the HTTP body is wrapped in a capping
    // reader (byte cap), gz decode uses its own bounded output check via
    // extract_tar_gz, and the scratch dir is created fresh (not with
    // create_dir_all, which would follow a pre-placed symlink).
    let resp = http_get_streaming(&url, max_bytes)?;
    // Extract into a fresh dir. On any error, do our best to clean up so
    // we don't leave partial data behind.
    if let Err(e) = extract_tar_gz(resp, &scratch, max_bytes) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }

    let snap_id = hv.snapshot_adopt(&scratch).map_err(|e| {
        let _ = std::fs::remove_dir_all(&scratch);
        ApiError::from(e)
    })?;
    // Backend has copied what it needs — the scratch dir is safe to drop.
    let _ = std::fs::remove_dir_all(&scratch);

    ownership.record_snapshot(snap_id, org);
    tracing::info!(
        entry = %entry_name,
        snap_id = snap_id.0,
        "marketplace: snapshot adopted"
    );
    Ok(snap_id)
}

/// Open the tarball URL and return the response body as a `Read` that
/// enforces the byte cap streaming-side. NO full buffering — the
/// caller (gzip decoder → tar reader → per-file unpack) consumes bytes
/// as they arrive so peak memory is bounded to a couple of chunk-size
/// buffers regardless of the tarball's compressed size.
///
/// Uses a fresh `reqwest::blocking::Client` per call — marketplace pulls
/// are rare (first fork per tenant per entry) so the per-call setup cost
/// is negligible next to the network round-trip.
fn http_get_streaming(url: &str, max_bytes: u64) -> Result<CappedRead, ApiError> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| ApiError::InternalDyn(format!("http client build: {e}")))?;

    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(ApiError::Bad(format!(
            "marketplace snapshot_url must be http:// or https://, got {url:?}"
        )));
    }
    if url.starts_with("http://") {
        tracing::warn!(
            url,
            "marketplace: pulling snapshot over http:// — prefer https://"
        );
    }

    let resp = client
        .get(url)
        .send()
        .map_err(|e| ApiError::InternalDyn(format!("marketplace GET {url}: {e}")))?;

    if !resp.status().is_success() {
        return Err(ApiError::InternalDyn(format!(
            "marketplace GET {url}: HTTP {}",
            resp.status()
        )));
    }
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            return Err(ApiError::Bad(format!(
                "marketplace tarball {url}: Content-Length {len} exceeds cap {max_bytes} \
                 (set NANOVM_MARKETPLACE_MAX_BYTES to raise)"
            )));
        }
    }
    Ok(CappedRead::new(resp, max_bytes, url.to_string()))
}

/// `Read` adapter that enforces a running byte cap on the underlying
/// stream. Returns [`io::ErrorKind::InvalidData`] the moment total-read
/// crosses `max_bytes` — the gz + tar readers translate that into an
/// extraction error, which the handler surfaces as a client-safe 400.
pub(crate) struct CappedRead {
    inner: reqwest::blocking::Response,
    max_bytes: u64,
    seen: u64,
    url: String,
}

impl CappedRead {
    fn new(inner: reqwest::blocking::Response, max_bytes: u64, url: String) -> Self {
        Self {
            inner,
            max_bytes,
            seen: 0,
            url,
        }
    }
}

impl Read for CappedRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.seen = self.seen.saturating_add(n as u64);
        if self.seen > self.max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "marketplace tarball {}: body exceeded cap {} mid-stream",
                    self.url, self.max_bytes
                ),
            ));
        }
        Ok(n)
    }
}

/// Safely extract a gzipped tarball into `dst`. Rejects entries whose
/// path escapes `dst` (path-traversal), rejects archives with more than
/// [`MAX_TARBALL_ENTRIES`] entries, and rejects entries whose combined
/// uncompressed size exceeds `max_bytes`.
///
/// The extraction is deliberately restrictive: marketplace snapshot
/// tarballs are a well-defined format (a couple of files at the archive
/// root) and any weird shape is a security signal, not a novel layout
/// we should accommodate.
fn extract_tar_gz<R: Read>(source: R, dst: &Path, max_bytes: u64) -> Result<(), ApiError> {
    // `create_dir` (not `create_dir_all`) — the scratch path is
    // per-process nonce-tagged, so it MUST NOT already exist. If it
    // does, treat that as an attempt to pre-place a symlink and refuse
    // rather than following into whatever the attacker prepared. The
    // process-local `pid + atomic counter` naming makes a real
    // collision essentially impossible; a pre-existing path means
    // tampering.
    std::fs::create_dir(dst).map_err(|e| {
        ApiError::InternalDyn(format!(
            "marketplace: mkdir extract dir {} (must not pre-exist): {e}",
            dst.display()
        ))
    })?;

    let gz = flate2::read::GzDecoder::new(source);
    let mut archive = tar::Archive::new(gz);
    let mut count = 0usize;
    let mut total_bytes: u64 = 0;
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    for entry in archive
        .entries()
        .map_err(|e| ApiError::Bad(format!("marketplace tarball: not a valid tar.gz: {e}")))?
    {
        let mut entry = entry
            .map_err(|e| ApiError::Bad(format!("marketplace tarball: entry read error: {e}")))?;
        count += 1;
        if count > MAX_TARBALL_ENTRIES {
            return Err(ApiError::Bad(format!(
                "marketplace tarball has more than {MAX_TARBALL_ENTRIES} entries \
                 — refusing to extract (looks like a tar-bomb)"
            )));
        }

        let raw_path = entry
            .path()
            .map_err(|e| ApiError::Bad(format!("marketplace tarball: bad entry path: {e}")))?
            .into_owned();
        if !is_safe_relative(&raw_path) {
            return Err(ApiError::Bad(format!(
                "marketplace tarball: unsafe entry path {raw_path:?} — refusing to extract"
            )));
        }
        // Normalize before the dedup check: `manifest.json` and
        // `./manifest.json` (and `a/b` vs `a/./b`) unpack to the same
        // filesystem target, so they must collide in `seen_paths` too.
        // Without this, an archive could smuggle in two entries whose
        // second silently overwrote the first's extracted file.
        let normalized = normalize_relative(&raw_path);
        // Entries like `.` or `./` normalize to an empty path — that
        // would make the extract target `dst.join("")` == `dst` and
        // `tar::Entry::unpack` errors out with an opaque I/O failure
        // that renders as a 500. Reject up-front as a client-facing 400
        // with a clear reason.
        if normalized.as_os_str().is_empty() {
            return Err(ApiError::Bad(format!(
                "marketplace tarball: entry {raw_path:?} normalizes to an empty path \
                 (only `.` / `./` components) — refusing to extract"
            )));
        }
        if !seen_paths.insert(normalized.clone()) {
            return Err(ApiError::Bad(format!(
                "marketplace tarball: duplicate entry {raw_path:?} \
                 (normalizes to {normalized:?})"
            )));
        }

        let size = entry.size();
        total_bytes = total_bytes.saturating_add(size);
        if total_bytes > max_bytes {
            return Err(ApiError::Bad(format!(
                "marketplace tarball: uncompressed total exceeds cap {max_bytes} bytes"
            )));
        }

        // Only files and directories. Symlinks / hardlinks / device
        // files would be a snapshot bug and a potential escape.
        let hdr_type = entry.header().entry_type();
        if !hdr_type.is_file() && !hdr_type.is_dir() {
            return Err(ApiError::Bad(format!(
                "marketplace tarball: entry {raw_path:?} has unsupported type {hdr_type:?}"
            )));
        }

        let target = dst.join(&normalized);
        // Belt-and-braces: canonicalize check under dst. `is_safe_relative`
        // already rejects `..` and absolute paths so this is redundant on
        // sound inputs, but cheap.
        if !target.starts_with(dst) {
            return Err(ApiError::Bad(format!(
                "marketplace tarball: computed target {} escapes extract dir {}",
                target.display(),
                dst.display()
            )));
        }
        entry.unpack(&target).map_err(|e| {
            ApiError::InternalDyn(format!("marketplace tarball: unpack {raw_path:?}: {e}"))
        })?;
    }
    Ok(())
}

/// Strip `.` components from a relative path. Callers guarantee it's
/// already `is_safe_relative` (no absolute, no `..`), so nothing else
/// needs collapsing. May return an empty PathBuf when the input is
/// entirely `.` components (e.g. `.` or `./`) — the caller in
/// [`extract_tar_gz`] checks for that and rejects with a 400 rather
/// than trying to `dst.join("")` and confusing `tar::Entry::unpack`.
fn normalize_relative(p: &Path) -> PathBuf {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            Component::CurDir => None,
            // is_safe_relative caller-side rules these out; be defensive.
            _ => None,
        })
        .collect()
}

/// True iff `p` is safe to `dst.join(p)` — no absolute, no `..`
/// components, no `~`-tilde prefix, no `/` prefix.
fn is_safe_relative(p: &Path) -> bool {
    if p.is_absolute() {
        return false;
    }
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

/// Where to extract the marketplace tarball while we adopt it. Uses
/// the system temp dir + a per-process nonce to avoid collisions.
fn scratch_dir(entry_name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nonce = N.fetch_add(1, Ordering::Relaxed);
    // Strip characters that would confuse a shell / file browser looking
    // at the temp dir. Marketplace names already pass `is_valid_name`
    // so this is defensive rather than corrective.
    let safe: String = entry_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("nanovm-mkt-{pid}-{safe}-{nonce}"))
}

/// Read the tarball byte cap from env, defaulting to
/// [`DEFAULT_MAX_TARBALL_BYTES`]. Invalid values are logged + ignored
/// so a typo doesn't take down the endpoint.
fn max_tarball_bytes() -> u64 {
    match std::env::var(MAX_BYTES_ENV) {
        Ok(v) => match v.parse::<u64>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!("{MAX_BYTES_ENV}={v:?} is not a positive integer; using default");
                DEFAULT_MAX_TARBALL_BYTES
            }
        },
        Err(_) => DEFAULT_MAX_TARBALL_BYTES,
    }
}

// ------ Tests ------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use tar::Header;

    /// Build a minimal snapshot-shaped tar.gz in memory:
    /// `manifest.json` + `memory.cow`. Uses [`snapshot::Manifest`] so the
    /// bytes are what `Hypervisor::snapshot_adopt` expects.
    fn sample_snapshot_tarball() -> Vec<u8> {
        let mut mani = snapshot::Manifest::new(0, 4096, 4096, 1);
        mani.kernel_cmdline = "console=ttyS0".into();
        let mani_bytes = serde_json::to_vec_pretty(&mani).unwrap();
        // 4 KiB of payload — matches the mock hypervisor's expectations
        // for adopt() (backing_file_path).
        let backing_bytes: Vec<u8> = (0..4096u32).map(|i| (i & 0xff) as u8).collect();
        let backing_name = mani
            .backing_file_path(Path::new(""))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tarw = tar::Builder::new(&mut gz);
            let mut h = Header::new_gnu();
            h.set_size(mani_bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tarw.append_data(&mut h, "manifest.json", mani_bytes.as_slice())
                .unwrap();

            let mut h = Header::new_gnu();
            h.set_size(backing_bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tarw.append_data(&mut h, &backing_name, backing_bytes.as_slice())
                .unwrap();
            tarw.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    /// Per-test tmp path with a real monotonic counter so two calls in
    /// the same process (or two `#[test]` in the same binary) never
    /// collide. Callers `remove_dir_all` up front to ensure a clean
    /// slate for `create_dir` (which errors on pre-existing dirs, by
    /// design).
    fn fresh_tmp(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "nanovm-mkt-{}-{}-{}",
            label,
            std::process::id(),
            id,
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn is_safe_relative_accepts_normal() {
        assert!(is_safe_relative(Path::new("manifest.json")));
        assert!(is_safe_relative(Path::new("sub/dir/file.bin")));
        assert!(is_safe_relative(Path::new("./manifest.json")));
    }

    #[test]
    fn is_safe_relative_rejects_dangerous() {
        assert!(!is_safe_relative(Path::new("/etc/passwd")));
        assert!(!is_safe_relative(Path::new("../etc/passwd")));
        assert!(!is_safe_relative(Path::new("a/../b")));
    }

    #[test]
    fn extract_tar_gz_happy_path() {
        let tarball = sample_snapshot_tarball();
        let tmp = fresh_tmp("extract");
        extract_tar_gz(tarball.as_slice(), &tmp, DEFAULT_MAX_TARBALL_BYTES).expect("extract");
        assert!(
            tmp.join("manifest.json").exists(),
            "manifest.json extracted"
        );
        // Any *.cow file present is sufficient — the exact backing filename
        // is manifest-derived.
        let entries: Vec<_> = std::fs::read_dir(&tmp).unwrap().collect();
        assert_eq!(entries.len(), 2, "manifest + backing file");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_tar_gz_rejects_traversal() {
        // The tar crate's high-level `append_data` refuses to write a
        // `..` path in the first place (defense in depth on the
        // producer side). To exercise our extract-time check we craft
        // the tar bytes by hand: a 512-byte USTAR header + payload
        // padded to 512 bytes, with the malicious path directly in
        // the header's name field. We then gzip-wrap the whole thing.
        let payload: &[u8] = b"pwned\n";
        let bad_path: &[u8] = b"../evil.txt";
        let mut hdr = [0u8; 512];
        hdr[..bad_path.len()].copy_from_slice(bad_path);
        // File mode (octal ASCII, null-terminated) — 100644.
        hdr[100..107].copy_from_slice(b"0000644");
        // uid, gid.
        hdr[108..115].copy_from_slice(b"0000000");
        hdr[116..123].copy_from_slice(b"0000000");
        // Size (octal ASCII, 11 chars + null).
        let size_str = format!("{:011o}", payload.len());
        hdr[124..135].copy_from_slice(size_str.as_bytes());
        // mtime = 0.
        hdr[136..147].copy_from_slice(b"00000000000");
        // Placeholder checksum area = spaces, then compute + write.
        hdr[148..156].copy_from_slice(b"        ");
        // typeflag = '0' (regular file)
        hdr[156] = b'0';
        // ustar magic + version.
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263..265].copy_from_slice(b"00");
        // Checksum = sum of all header bytes with the checksum field as spaces.
        let cksum: u32 = hdr.iter().map(|&b| u32::from(b)).sum();
        let cksum_str = format!("{cksum:06o}\0 ");
        hdr[148..156].copy_from_slice(cksum_str.as_bytes());

        // Build the tar stream: header + padded payload + 2×512 zeros (end marker).
        let mut tarbytes = Vec::new();
        tarbytes.extend_from_slice(&hdr);
        tarbytes.extend_from_slice(payload);
        // Pad payload to 512-byte boundary.
        let pad = (512 - payload.len() % 512) % 512;
        tarbytes.extend(std::iter::repeat_n(0u8, pad));
        // End-of-archive marker.
        tarbytes.extend(std::iter::repeat_n(0u8, 1024));

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tarbytes).unwrap();
        let bytes = gz.finish().unwrap();

        let tmp = fresh_tmp("traverse");
        let err = extract_tar_gz(bytes.as_slice(), &tmp, DEFAULT_MAX_TARBALL_BYTES).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("unsafe entry path") || msg.contains("escapes extract dir"),
            "expected traversal rejection, got: {msg}"
        );
        assert!(
            !tmp.parent().unwrap().join("evil.txt").exists(),
            "traversal must not write outside dst"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_tar_gz_enforces_byte_cap() {
        let tarball = sample_snapshot_tarball();
        // Cap tiny — 100 bytes — so the ~4 KiB payload trips it.
        let tmp = fresh_tmp("cap");
        let err = extract_tar_gz(tarball.as_slice(), &tmp, 100).unwrap_err();
        assert!(
            format!("{err:?}").contains("uncompressed total exceeds cap"),
            "expected cap error, got: {err:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_tar_gz_refuses_preexisting_dst() {
        // Simulates the symlink pre-placement attack: attacker gets
        // `dst` to already exist (as a symlink or real dir) before we
        // extract. `create_dir` must fail rather than reuse.
        let tarball = sample_snapshot_tarball();
        let tmp = fresh_tmp("preexist");
        std::fs::create_dir(&tmp).unwrap();
        let err = extract_tar_gz(tarball.as_slice(), &tmp, DEFAULT_MAX_TARBALL_BYTES).unwrap_err();
        assert!(
            format!("{err:?}").contains("must not pre-exist"),
            "expected pre-exist refusal, got: {err:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn normalize_relative_strips_dot_components() {
        assert_eq!(
            normalize_relative(Path::new("./manifest.json")),
            PathBuf::from("manifest.json")
        );
        assert_eq!(
            normalize_relative(Path::new("a/./b/./c")),
            PathBuf::from("a/b/c")
        );
        assert_eq!(
            normalize_relative(Path::new("manifest.json")),
            PathBuf::from("manifest.json")
        );
    }

    #[test]
    fn normalize_relative_yields_empty_for_all_dot_components() {
        assert!(normalize_relative(Path::new("./")).as_os_str().is_empty());
        assert!(normalize_relative(Path::new(".")).as_os_str().is_empty());
        assert!(normalize_relative(Path::new("./././"))
            .as_os_str()
            .is_empty());
    }

    #[test]
    fn extract_tar_gz_rejects_all_dot_entry_with_client_error() {
        // Craft a tarball with a single `.` entry (all-CurDir path).
        // This normalizes to an empty path — before the fix, extract
        // fell through to `entry.unpack(dst)` which errored out as a
        // 500. After the fix, we return a client-safe 400.
        let payload: &[u8] = b"x";
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tarw = tar::Builder::new(&mut gz);
            let mut h = Header::new_gnu();
            h.set_size(payload.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            // `.` — a single CurDir component.
            tarw.append_data(&mut h, ".", payload).unwrap();
            tarw.finish().unwrap();
        }
        let bytes = gz.finish().unwrap();
        let tmp = fresh_tmp("empty-normalize");
        let err = extract_tar_gz(bytes.as_slice(), &tmp, DEFAULT_MAX_TARBALL_BYTES).unwrap_err();
        // Must be a Bad (400) with a clear reason, not an InternalDyn (500).
        assert!(
            matches!(err, ApiError::Bad(_)),
            "expected ApiError::Bad, got: {err:?}"
        );
        assert!(
            format!("{err:?}").contains("normalizes to an empty path"),
            "expected empty-path message, got: {err:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_tar_gz_dedup_collapses_dot_prefix() {
        // Craft a tarball with `manifest.json` and `./manifest.json` —
        // both entries extract to the same target. Without normalize
        // the dedup check on `raw_path` would let the second silently
        // overwrite the first; with normalize it must reject.
        let payload = b"x";
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tarw = tar::Builder::new(&mut gz);
            let mut h = Header::new_gnu();
            h.set_size(payload.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tarw.append_data(&mut h, "manifest.json", payload.as_slice())
                .unwrap();
            // Second entry with a `./` prefix. tar-rs's append_data
            // canonicalizes the input path, but the CurDir component
            // gets preserved because it's a leading `./`. If tar-rs
            // strips it on its side, this test degenerates into "two
            // identical entries" which the raw-path check would also
            // reject — either way, the collision is caught. We just
            // want to prove the extract-time normalize does its job.
            let mut h2 = Header::new_gnu();
            h2.set_size(payload.len() as u64);
            h2.set_mode(0o644);
            h2.set_cksum();
            tarw.append_data(&mut h2, "./manifest.json", payload.as_slice())
                .unwrap();
            tarw.finish().unwrap();
        }
        let bytes = gz.finish().unwrap();
        let tmp = fresh_tmp("dedup");
        let err = extract_tar_gz(bytes.as_slice(), &tmp, DEFAULT_MAX_TARBALL_BYTES).unwrap_err();
        assert!(
            format!("{err:?}").contains("duplicate entry"),
            "expected duplicate rejection, got: {err:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn max_tarball_bytes_defaults_when_env_unset_or_bad() {
        // We don't touch env (workspace forbids unsafe / set_var). The
        // default path is the only branch we can exercise here without
        // an env mutator; a real integration test in bin/server sets it.
        let n = max_tarball_bytes();
        // Env is not set in-process (or set to bad); the fn must return
        // a positive default.
        assert!(n > 0);
    }

    // --------- Integration test: end-to-end fork via a local HTTP fixture.

    /// Spawn a tiny HTTP server bound to 127.0.0.1:0 that serves the
    /// given bytes at `/snapshot.tar.gz`. Returns the URL. The server
    /// lives for the duration of the returned `TaskHandle` (drop it to
    /// stop the accept loop).
    async fn serve_bytes(bytes: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::get, Router};
        let payload = Arc::new(bytes);
        let app = Router::new().route(
            "/snapshot.tar.gz",
            get(move || {
                let payload = payload.clone();
                async move { axum::body::Bytes::from(payload.to_vec()) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/snapshot.tar.gz"), handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_fork_hits_backend_and_caches() {
        use crate::marketplace::{Marketplace, MarketplaceSnapshot};
        use crate::routes::{router, AppState};
        use crate::ApiTokens;
        use axum::{
            body::{to_bytes, Body},
            http::{Method, Request},
            Extension,
        };
        use tower::ServiceExt;
        use vm_mock::MockHypervisor;

        // 1. Local HTTP server serving the sample tarball.
        let tarball = sample_snapshot_tarball();
        let (url, _server) = serve_bytes(tarball).await;

        // 2. Marketplace with one entry pointing at the local URL.
        let raw_config = format!(
            r#"{{"snapshots":[{{
                "name":"python-3.12-ds",
                "description":"test",
                "size_bytes":0,
                "kernel_url":"http://cdn.example/vmlinux",
                "rootfs_url":"http://cdn.example/rootfs.ext4",
                "snapshot_url":{url:?},
                "cmdline":"console=ttyS0",
                "labels":["python"],
                "maintainer":"nanovm-test"
            }}]}}"#
        );
        let mkt = Arc::new(Marketplace::parse(&raw_config));
        assert_eq!(mkt.len(), 1);
        assert!(matches!(
            mkt.get("python-3.12-ds"),
            Some(MarketplaceSnapshot { .. })
        ));

        // 3. Build a control plane against a mock hypervisor.
        let hv = Arc::new(MockHypervisor::new());
        let tokens = Arc::new(ApiTokens::with_orgs([(
            "tok-a".to_string(),
            OrgId::new("org-alpha"),
        )]));
        let state = AppState::new(hv.clone()).with_marketplace(mkt);
        let app = router().layer(Extension(tokens)).with_state(state.clone());

        // 4. Fork twice — the second call must hit the cache (no download).
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/marketplace/snapshots/python-3.12-ds/fork")
                    .header("authorization", "Bearer tok-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            first.status(),
            StatusCode::CREATED,
            "first fork should succeed"
        );
        let bytes = to_bytes(first.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["vm"]["id"].is_number(), "response carries a vm id");
        assert_eq!(body["fork_count"].as_u64(), Some(1));

        // Assert exactly one snapshot was adopted into the backend.
        assert_eq!(
            hv.list_snapshots().unwrap().len(),
            1,
            "first fork adopted 1 snapshot"
        );

        let second = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/marketplace/snapshots/python-3.12-ds/fork")
                    .header("authorization", "Bearer tok-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::CREATED,
            "second fork should succeed"
        );
        let bytes = to_bytes(second.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["fork_count"].as_u64(),
            Some(2),
            "usage counter increments for the same token"
        );

        // Cache hit: still one snapshot in the backend, not two.
        assert_eq!(
            hv.list_snapshots().unwrap().len(),
            1,
            "second fork used the cached snapshot — no re-adopt"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_marketplace_name_returns_404() {
        use crate::marketplace::Marketplace;
        use crate::routes::{router, AppState};
        use crate::ApiTokens;
        use axum::{
            body::Body,
            http::{Method, Request},
            Extension,
        };
        use tower::ServiceExt;
        use vm_mock::MockHypervisor;

        let hv = Arc::new(MockHypervisor::new());
        let tokens = Arc::new(ApiTokens::with_orgs([(
            "tok-a".to_string(),
            OrgId::new("org-alpha"),
        )]));
        let state = AppState::new(hv).with_marketplace(Arc::new(Marketplace::default()));
        let app = router().layer(Extension(tokens)).with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/marketplace/snapshots/no-such/fork")
                    .header("authorization", "Bearer tok-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn entry_without_snapshot_url_returns_501() {
        use crate::marketplace::Marketplace;
        use crate::routes::{router, AppState};
        use crate::ApiTokens;
        use axum::{
            body::Body,
            http::{Method, Request},
            Extension,
        };
        use tower::ServiceExt;
        use vm_mock::MockHypervisor;

        let raw = r#"{"snapshots":[{
            "name":"listing-only",
            "description":"x","size_bytes":0,
            "kernel_url":"https://k","rootfs_url":"https://r",
            "cmdline":"","maintainer":"m"
        }]}"#;
        let mkt = Arc::new(Marketplace::parse(raw));
        assert_eq!(mkt.len(), 1);

        let hv = Arc::new(MockHypervisor::new());
        let tokens = Arc::new(ApiTokens::with_orgs([(
            "tok-a".to_string(),
            OrgId::new("org-alpha"),
        )]));
        let state = AppState::new(hv).with_marketplace(mkt);
        let app = router().layer(Extension(tokens)).with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/marketplace/snapshots/listing-only/fork")
                    .header("authorization", "Bearer tok-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
