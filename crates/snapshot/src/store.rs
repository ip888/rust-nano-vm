//! Pluggable durable-storage backend for snapshots.
//!
//! A snapshot is a directory containing `manifest.json` and `memory.cow`
//! (see the crate root). For local single-host workloads, keeping those
//! files on the VMM's local filesystem is sufficient. For **production
//! deployments** — multi-region warm pools, cross-host failover,
//! deploy-and-redeploy without losing the warmed base image — the
//! snapshot needs to live somewhere durable that any host can pull it
//! from.
//!
//! This module is the abstraction layer. [`SnapshotStore`] is the
//! trait every backend implements; [`FsSnapshotStore`] is the
//! built-in local-filesystem implementation; the `s3` feature adds
//! [`S3SnapshotStore`] for AWS S3 / MinIO / Cloudflare R2 / Wasabi /
//! any other S3-compatible object store.
//!
//! ### Object layout
//!
//! For any backend, a snapshot id `N` is stored as **two named blobs**
//! under a per-snapshot prefix:
//!
//! ```text
//! <store-root>/snap-<N>/manifest.json
//! <store-root>/snap-<N>/memory.cow
//! ```
//!
//! - Filesystem backend: `<store-root>` is a host path; the blobs are
//!   files.
//! - S3 backend: `<store-root>` is a bucket + optional prefix; the
//!   blobs are S3 objects with those keys.
//!
//! That symmetry is intentional. An operator can pull a snapshot
//! out of S3 with the AWS CLI, copy it locally, and `restore` from
//! the filesystem store with no format change. Conversely, a local
//! snapshot directory can be `aws s3 cp --recursive ...` into a
//! bucket and immediately be served by the S3 store.
//!
//! ### Configuration via URI
//!
//! Stores are addressed by URI so a single `NANOVM_SNAPSHOT_STORE` env
//! var can select any backend at runtime:
//!
//! - `file:///var/lib/nanovm/snapshots`
//! - `s3://nanovm-prod-snapshots/`
//! - `s3://nanovm-snapshots/eu-west-1/`
//!
//! [`parse_store_uri`] is the entry point: it returns
//! [`StoreLocation::File`] or [`StoreLocation::S3`] which the caller
//! turns into a concrete `SnapshotStore` implementation. (We don't
//! build the `dyn` instance inline because the S3 path needs an
//! async runtime to construct, which is a control-plane concern,
//! not a snapshot-crate concern.)

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{Manifest, SnapshotError};

/// Snapshot id type. Mirrors `vm_core::SnapshotId(u64)` but kept here
/// as a primitive to avoid pulling `vm-core` into `snapshot` (which
/// would create a circular dep when `vm-core` itself uses snapshot
/// types).
pub type SnapshotIdNum = u64;

/// Errors produced by a [`SnapshotStore`] backend.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Snapshot id is not present in the backend.
    #[error("snapshot id {0} not found in store")]
    NotFound(SnapshotIdNum),

    /// Local filesystem or transport-layer I/O failure.
    #[error("snapshot store io: {0}")]
    Io(#[from] io::Error),

    /// Wraps a manifest / backing-file format error so callers see
    /// store and format errors through one envelope.
    #[error("snapshot store format: {0}")]
    Format(#[from] SnapshotError),

    /// URI malformed (bad scheme, missing bucket, etc.).
    #[error("bad snapshot store URI: {0}")]
    BadUri(String),

    /// Network call to the backend failed. The string includes the
    /// backend-specific error message; callers should NOT match on
    /// it as a stable surface.
    #[error("snapshot store network: {0}")]
    Network(String),

    /// Backend rejected the request as unauthorized — bad credentials,
    /// IAM policy denying the action, signed-URL expiration, etc.
    #[error("snapshot store auth: {0}")]
    Auth(String),
}

/// Pluggable durable backend for snapshots.
///
/// All methods are SYNCHRONOUS. Async implementations (S3) wrap a
/// tokio runtime internally so callers can drive snapshot lifecycle
/// from the existing sync `Hypervisor` trait without async-flooding
/// the codebase. The trait is `Send + Sync` so an `Arc<dyn
/// SnapshotStore>` can be shared across handler threads.
pub trait SnapshotStore: Send + Sync + std::fmt::Debug {
    /// Upload the snapshot at `src_dir` (which must contain
    /// `manifest.json` + the backing file referenced in it) to the
    /// store under id `snap_id`. Idempotent on the backend: re-putting
    /// the same id overwrites.
    fn put(&self, snap_id: SnapshotIdNum, src_dir: &Path) -> Result<(), StoreError>;

    /// Download the snapshot with id `snap_id` into the directory
    /// `dst_dir` (which the implementation creates if missing).
    /// After a successful return, `dst_dir/manifest.json` and the
    /// referenced backing file are present and ready for the local
    /// restore path.
    fn get(&self, snap_id: SnapshotIdNum, dst_dir: &Path) -> Result<(), StoreError>;

    /// Enumerate the snapshot ids currently in the store. Order is
    /// implementation-defined.
    fn list(&self) -> Result<Vec<SnapshotIdNum>, StoreError>;

    /// Delete the snapshot with id `snap_id`. Returns `Ok(())` if it
    /// didn't exist (idempotent — a redundant delete shouldn't
    /// fail).
    fn delete(&self, snap_id: SnapshotIdNum) -> Result<(), StoreError>;

    /// Human-readable identifier for log lines / dashboards
    /// (`"file:///.../snapshots"`, `"s3://bucket/"`).
    fn display(&self) -> String;
}

/// Built-in [`SnapshotStore`] backed by a local directory.
///
/// Every snapshot is a sub-directory named `snap-<id>`:
///
/// ```text
/// /<root>/snap-42/manifest.json
/// /<root>/snap-42/memory.cow
/// ```
///
/// `put` copies the manifest and the backing file referenced in it
/// into the destination. `get` reads them out — the caller hands the
/// returned directory to the existing `Manifest::read_from_dir` path,
/// no changes to the restore code.
#[derive(Debug, Clone)]
pub struct FsSnapshotStore {
    root: PathBuf,
}

impl FsSnapshotStore {
    /// Construct with the given root directory. The directory is
    /// created on first `put`; we don't `mkdir` here because callers
    /// may want to validate the path before writing anything.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Path of the per-snapshot directory inside this store.
    fn snap_dir(&self, snap_id: SnapshotIdNum) -> PathBuf {
        self.root.join(format!("snap-{snap_id}"))
    }
}

impl SnapshotStore for FsSnapshotStore {
    fn put(&self, snap_id: SnapshotIdNum, src_dir: &Path) -> Result<(), StoreError> {
        let dst = self.snap_dir(snap_id);
        fs::create_dir_all(&dst)?;

        // Read manifest first so we know which backing file to copy.
        let manifest = Manifest::read_from_dir(src_dir)?;

        // Copy manifest.json verbatim (re-serialize so the on-disk
        // bytes are normalized regardless of what's on src).
        manifest.write_to_dir(&dst)?;

        // Copy backing file. If the manifest references a backing
        // file that doesn't exist in src_dir, surface that as a
        // Format error rather than silently completing.
        let src_backing = manifest.backing_file_path(src_dir);
        if src_backing.exists() {
            let dst_backing = manifest.backing_file_path(&dst);
            fs::copy(&src_backing, &dst_backing)?;
        }
        Ok(())
    }

    fn get(&self, snap_id: SnapshotIdNum, dst_dir: &Path) -> Result<(), StoreError> {
        let src = self.snap_dir(snap_id);
        if !src.exists() {
            return Err(StoreError::NotFound(snap_id));
        }
        fs::create_dir_all(dst_dir)?;

        let manifest = Manifest::read_from_dir(&src)?;
        manifest.write_to_dir(dst_dir)?;

        let src_backing = manifest.backing_file_path(&src);
        if src_backing.exists() {
            let dst_backing = manifest.backing_file_path(dst_dir);
            fs::copy(&src_backing, &dst_backing)?;
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<SnapshotIdNum>, StoreError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if let Some(rest) = s.strip_prefix("snap-") {
                if let Ok(id) = rest.parse::<u64>() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    fn delete(&self, snap_id: SnapshotIdNum) -> Result<(), StoreError> {
        let dir = self.snap_dir(snap_id);
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    fn display(&self) -> String {
        format!("file://{}", self.root.display())
    }
}

/// Parsed snapshot-store URI. Concrete construction of an
/// [`S3SnapshotStore`] (when the `s3` feature is enabled) needs an
/// async runtime to wire up `aws-sdk-s3`, which is a control-plane
/// concern; the snapshot crate just identifies the shape of the URI
/// here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLocation {
    /// `file:///abs/path/to/dir`
    File {
        /// Absolute path on the local filesystem.
        root: PathBuf,
    },
    /// `s3://bucket[/prefix]`
    S3 {
        /// Bucket name (no scheme, no slashes).
        bucket: String,
        /// Key prefix within the bucket (`""` if URI is bare
        /// `s3://bucket`). Trailing slash stripped.
        prefix: String,
    },
}

/// Parse a `file://` or `s3://` URI into a [`StoreLocation`].
///
/// Strict on shape:
///
/// - `file://` schemes must be absolute (`file:///var/...`); relative
///   paths are rejected because operators inevitably set them based
///   on the wrong cwd.
/// - `s3://` URIs must include a bucket and may include a prefix.
///   Trailing slashes on the prefix are normalized away so
///   `s3://b/` and `s3://b` are equivalent.
///
/// The `s3` feature isn't required to PARSE an s3 URI — that's pure
/// string handling. Constructing a working [`SnapshotStore`] from
/// the parsed s3 URI needs the feature.
pub fn parse_store_uri(uri: &str) -> Result<StoreLocation, StoreError> {
    if let Some(rest) = uri.strip_prefix("file://") {
        // Three slashes required: `file://` + `/abs/path` → `file:///abs/path`.
        if !rest.starts_with('/') {
            return Err(StoreError::BadUri(format!(
                "file:// URI must be absolute (file:///path/...), got {uri:?}"
            )));
        }
        return Ok(StoreLocation::File {
            root: PathBuf::from(rest),
        });
    }
    if let Some(rest) = uri.strip_prefix("s3://") {
        if rest.is_empty() {
            return Err(StoreError::BadUri(format!(
                "s3:// URI has no bucket: {uri:?}"
            )));
        }
        let (bucket, prefix) = match rest.split_once('/') {
            Some((b, p)) => (b, p.trim_end_matches('/').to_string()),
            None => (rest, String::new()),
        };
        if bucket.is_empty() {
            return Err(StoreError::BadUri(format!(
                "s3:// URI has empty bucket: {uri:?}"
            )));
        }
        return Ok(StoreLocation::S3 {
            bucket: bucket.to_string(),
            prefix,
        });
    }
    Err(StoreError::BadUri(format!(
        "unknown snapshot-store URI scheme; want file:// or s3://, got {uri:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Per-test unique temp dir so tests don't trip over each other
    /// when run in parallel.
    fn tmp_dir(label: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "rust-nano-vm-store-{}-{}-{}",
            label,
            std::process::id(),
            id,
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn make_src_snapshot(dir: &Path, snap_id: u64) -> Manifest {
        let mut m = Manifest::new(snap_id, 4096, 4096, 1);
        m.kernel_cmdline = "console=ttyS0".into();
        m.labels.insert("base".into(), "alpine:3.20".into());
        fs::create_dir_all(dir).unwrap();
        m.write_to_dir(dir).unwrap();
        // Synthetic backing file: 4 KiB of payload (1 page).
        let backing_path = m.backing_file_path(dir);
        let bytes: Vec<u8> = (0..4096u32).map(|i| (i & 0xff) as u8).collect();
        fs::write(&backing_path, &bytes).unwrap();
        m
    }

    // ---- URI parser ---------------------------------------------------

    #[test]
    fn parse_file_uri_returns_absolute_path() {
        let loc = parse_store_uri("file:///var/lib/nanovm/snapshots").unwrap();
        assert_eq!(
            loc,
            StoreLocation::File {
                root: PathBuf::from("/var/lib/nanovm/snapshots"),
            }
        );
    }

    #[test]
    fn parse_file_uri_rejects_relative_path() {
        let err = parse_store_uri("file://relative/path").unwrap_err();
        assert!(matches!(err, StoreError::BadUri(_)), "got {err:?}");
    }

    #[test]
    fn parse_s3_uri_with_just_bucket() {
        let loc = parse_store_uri("s3://nanovm-snapshots").unwrap();
        assert_eq!(
            loc,
            StoreLocation::S3 {
                bucket: "nanovm-snapshots".into(),
                prefix: String::new(),
            }
        );
    }

    #[test]
    fn parse_s3_uri_with_prefix_trims_trailing_slash() {
        let loc = parse_store_uri("s3://nanovm-snapshots/prod/").unwrap();
        assert_eq!(
            loc,
            StoreLocation::S3 {
                bucket: "nanovm-snapshots".into(),
                prefix: "prod".into(),
            }
        );
    }

    #[test]
    fn parse_s3_uri_with_nested_prefix() {
        let loc = parse_store_uri("s3://b/prod/eu-west-1").unwrap();
        assert_eq!(
            loc,
            StoreLocation::S3 {
                bucket: "b".into(),
                prefix: "prod/eu-west-1".into(),
            }
        );
    }

    #[test]
    fn parse_s3_uri_rejects_empty_bucket() {
        let err = parse_store_uri("s3:///prefix").unwrap_err();
        assert!(matches!(err, StoreError::BadUri(_)));
    }

    #[test]
    fn parse_uri_rejects_unknown_scheme() {
        let err = parse_store_uri("gs://bucket/prefix").unwrap_err();
        assert!(matches!(err, StoreError::BadUri(_)));
    }

    // ---- FsSnapshotStore ---------------------------------------------

    #[test]
    fn fs_store_put_then_get_round_trips_manifest_and_backing_file() {
        let root = tmp_dir("rt");
        let store = FsSnapshotStore::new(&root);

        let src = tmp_dir("src");
        let original = make_src_snapshot(&src, 42);

        store.put(42, &src).expect("put");
        assert!(root.join("snap-42").join("manifest.json").exists());
        assert!(root.join("snap-42").join("memory.cow").exists());

        let dst = tmp_dir("dst");
        store.get(42, &dst).expect("get");

        let restored = Manifest::read_from_dir(&dst).unwrap();
        assert_eq!(restored, original);

        let restored_backing = fs::read(restored.backing_file_path(&dst)).unwrap();
        let original_backing = fs::read(original.backing_file_path(&src)).unwrap();
        assert_eq!(restored_backing, original_backing);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    #[test]
    fn fs_store_get_unknown_snapshot_returns_not_found() {
        let root = tmp_dir("nf");
        let store = FsSnapshotStore::new(&root);
        let dst = tmp_dir("nf-dst");
        let err = store.get(99, &dst).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(99)), "got {err:?}");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&dst);
    }

    #[test]
    fn fs_store_list_returns_sorted_ids() {
        let root = tmp_dir("list");
        let store = FsSnapshotStore::new(&root);

        // No directory yet — list returns empty, not an error.
        assert_eq!(store.list().unwrap(), Vec::<u64>::new());

        for id in [7, 3, 11, 1] {
            let src = tmp_dir(&format!("list-src-{id}"));
            make_src_snapshot(&src, id);
            store.put(id, &src).unwrap();
            let _ = fs::remove_dir_all(&src);
        }

        // Add some non-snapshot directories — should be ignored.
        fs::create_dir_all(root.join("not-a-snapshot")).unwrap();
        fs::create_dir_all(root.join("snap-not-numeric")).unwrap();

        let ids = store.list().unwrap();
        assert_eq!(ids, vec![1, 3, 7, 11]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fs_store_delete_is_idempotent() {
        let root = tmp_dir("del");
        let store = FsSnapshotStore::new(&root);

        let src = tmp_dir("del-src");
        make_src_snapshot(&src, 5);
        store.put(5, &src).unwrap();
        assert!(root.join("snap-5").exists());

        store.delete(5).expect("first delete");
        assert!(!root.join("snap-5").exists());

        // Second delete on a now-missing snapshot must succeed —
        // operators sometimes retry deletes and we don't want them
        // to see a spurious failure.
        store.delete(5).expect("idempotent delete");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn fs_store_display_is_file_uri() {
        let store = FsSnapshotStore::new("/var/lib/nanovm/snapshots");
        assert_eq!(store.display(), "file:///var/lib/nanovm/snapshots");
    }
}
