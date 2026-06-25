//! Durable snapshot store wiring for the control plane.
//!
//! At process start the control plane reads
//! [`ENV_STORE_URI`] (`NANOVM_SNAPSHOT_STORE`) and constructs a
//! [`SnapshotStore`] for the operator's chosen backend:
//!
//! - Unset → no durable store; `/v1/snapshots/:id/export` and
//!   `/v1/snapshots/import` return `503 storage_unsupported`.
//! - `file:///abs/path` → [`FsSnapshotStore`]: local directory,
//!   always available, useful for single-host deployments and
//!   integration testing.
//! - `s3://bucket[/prefix]` → [`snapshot::S3SnapshotStore`] when the
//!   binary was built with the `s3` feature; otherwise a clear
//!   error at startup.
//!
//! The store sits in [`AppState`](crate::AppState) wrapped in
//! `Arc<dyn SnapshotStore>` so handler threads share one client
//! (and one connection pool, for S3).

use std::sync::Arc;

use snapshot::{FsSnapshotStore, SnapshotStore, StoreLocation};

/// Environment variable read at process start to select the
/// snapshot-store backend. Examples:
///
/// - `file:///var/lib/nanovm/snapshots`
/// - `s3://nanovm-snapshots/`
/// - `s3://nanovm/prod/eu-west-1`
pub const ENV_STORE_URI: &str = "NANOVM_SNAPSHOT_STORE";

/// Construct a [`SnapshotStore`] from `NANOVM_SNAPSHOT_STORE`.
/// Returns `Ok(None)` when the env var is unset — the control plane
/// then runs without a durable store and the export/import
/// endpoints return `503`.
///
/// Errors:
/// - Bad URI scheme / malformed value → returned to the binary so
///   startup fails loud rather than the operator wondering why
///   exports silently no-op.
/// - `s3://` URI when the binary was built without the `s3` feature
///   → returned with a hint that the operator wants a
///   `cargo build --features s3` binary.
pub fn from_env() -> Result<Option<Arc<dyn SnapshotStore>>, String> {
    let Ok(uri) = std::env::var(ENV_STORE_URI) else {
        return Ok(None);
    };
    if uri.is_empty() {
        return Ok(None);
    }
    let location = snapshot::parse_store_uri(&uri).map_err(|e| format!("{ENV_STORE_URI}: {e}"))?;
    let store = construct(location, &uri)?;
    Ok(Some(store))
}

#[cfg(not(feature = "s3"))]
fn construct(location: StoreLocation, uri: &str) -> Result<Arc<dyn SnapshotStore>, String> {
    match location {
        StoreLocation::File { root } => Ok(Arc::new(FsSnapshotStore::new(root))),
        StoreLocation::S3 { .. } => Err(format!(
            "{uri}: s3:// requires a binary built with `--features s3` \
             (nanovm-control-plane was built without it)"
        )),
    }
}

#[cfg(feature = "s3")]
fn construct(location: StoreLocation, uri: &str) -> Result<Arc<dyn SnapshotStore>, String> {
    match location {
        StoreLocation::File { root } => Ok(Arc::new(FsSnapshotStore::new(root))),
        StoreLocation::S3 { .. } => {
            let store = snapshot::S3SnapshotStore::from_location(&location)
                .map_err(|e| format!("{uri}: {e}"))?;
            Ok(Arc::new(store))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pure logic — URI parsing + construction — is covered in
    // the `snapshot` crate's tests. Here we just smoke-test that
    // `construct` returns the right concrete type for a known file
    // URI without touching the real environment (env mutation in
    // tests is racy and clashes with `#![forbid(unsafe_code)]`).
    #[test]
    fn construct_builds_filesystem_store_from_file_location() {
        let store = construct(
            StoreLocation::File {
                root: "/tmp/nanovm-snapshots-test".into(),
            },
            "file:///tmp/nanovm-snapshots-test",
        )
        .expect("construct");
        assert!(store.display().starts_with("file:///"));
    }

    #[cfg(not(feature = "s3"))]
    #[test]
    fn construct_rejects_s3_when_feature_disabled() {
        let err = construct(
            StoreLocation::S3 {
                bucket: "b".into(),
                prefix: "".into(),
            },
            "s3://b",
        )
        .unwrap_err();
        assert!(err.contains("--features s3"), "got {err:?}");
    }
}
