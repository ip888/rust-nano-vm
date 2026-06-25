//! S3-backed [`SnapshotStore`].
//!
//! Built on `aws-sdk-s3`, the official AWS Rust SDK. Production
//! deployments inherit its credential chain: environment variables,
//! shared credentials files, IAM roles for EC2 / ECS / EKS, web
//! identity tokens (for IRSA), and SSO. That matters for paying
//! customers — they can deploy `nanovm-control-plane` on AWS,
//! attach an IAM role with `s3:PutObject` / `s3:GetObject` on one
//! bucket, and the snapshot lifecycle Just Works without any
//! long-lived credentials sitting on the host.
//!
//! Any S3-compatible service works via the
//! [`S3SnapshotStore::with_endpoint`] / `NANOVM_S3_ENDPOINT` knob:
//! MinIO for on-prem, Cloudflare R2 for cheap egress, Wasabi for
//! cold storage, etc.
//!
//! ### Object layout
//!
//! ```text
//! s3://<bucket>/<prefix>/snap-<id>/manifest.json
//! s3://<bucket>/<prefix>/snap-<id>/memory.cow
//! ```
//!
//! When `prefix` is empty the keys collapse to `snap-<id>/...`.
//!
//! ### Threading model
//!
//! The [`SnapshotStore`] trait is sync. The SDK is async. To bridge
//! them this store owns a dedicated multi-thread tokio runtime and
//! `block_on`s every SDK call. Per-call cost is one thread context
//! switch — negligible next to S3's network latency.
//!
//! Callers driving the store from inside a tokio reactor (e.g. an
//! axum handler) MUST do it from a `spawn_blocking` context;
//! `block_on` inside the reactor would deadlock. The control-plane
//! integration handles this.

use std::fs;
use std::path::Path;

use aws_config::BehaviorVersion;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use tokio::runtime::Runtime;

use crate::store::{SnapshotIdNum, SnapshotStore, StoreError, StoreLocation};
use crate::Manifest;

/// Conventional environment variable for overriding the S3 endpoint
/// — used by MinIO / R2 / Wasabi / etc. Empty / unset means "use the
/// SDK default" (AWS regional endpoint).
pub const ENV_ENDPOINT: &str = "NANOVM_S3_ENDPOINT";

/// [`SnapshotStore`] backed by an S3-compatible object store.
pub struct S3SnapshotStore {
    client: Client,
    bucket: String,
    prefix: String,
    runtime: Runtime,
}

impl std::fmt::Debug for S3SnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3SnapshotStore")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl S3SnapshotStore {
    /// Build from a parsed [`StoreLocation::S3`]. Uses the SDK
    /// credential chain plus `NANOVM_S3_ENDPOINT` for a custom
    /// endpoint when set.
    pub fn from_location(loc: &StoreLocation) -> Result<Self, StoreError> {
        let StoreLocation::S3 { bucket, prefix } = loc else {
            return Err(StoreError::BadUri(format!(
                "S3SnapshotStore requires an s3:// URI, got {loc:?}"
            )));
        };
        let endpoint = std::env::var(ENV_ENDPOINT).ok().filter(|s| !s.is_empty());
        Self::new(bucket.clone(), prefix.clone(), endpoint)
    }

    /// Parse `uri` and construct directly. Convenience for callers
    /// that have a raw URI string.
    pub fn from_uri(uri: &str) -> Result<Self, StoreError> {
        let loc = crate::parse_store_uri(uri)?;
        Self::from_location(&loc)
    }

    /// Build with explicit bucket / prefix and an optional custom
    /// endpoint. Constructs the SDK client from the AWS credential
    /// chain.
    pub fn new(
        bucket: String,
        prefix: String,
        endpoint: Option<String>,
    ) -> Result<Self, StoreError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(StoreError::Io)?;

        let client = runtime.block_on(async {
            let mut loader = aws_config::defaults(BehaviorVersion::latest());
            if let Some(ref ep) = endpoint {
                loader = loader.endpoint_url(ep);
            }
            let config = loader.load().await;
            // Path-style addressing is the safe default for
            // S3-compatible services: AWS S3 supports both
            // virtual-hosted and path style, but MinIO / R2 /
            // Wasabi only reliably support path style.
            let mut s3_config = aws_sdk_s3::config::Builder::from(&config);
            if endpoint.is_some() {
                s3_config = s3_config.force_path_style(true);
            }
            Client::from_conf(s3_config.build())
        });

        Ok(Self {
            client,
            bucket,
            prefix,
            runtime,
        })
    }

    /// Build the S3 key for the manifest of `snap_id`.
    fn manifest_key(&self, snap_id: SnapshotIdNum) -> String {
        self.join_key(&format!("snap-{snap_id}/manifest.json"))
    }

    /// Build the S3 key for `relative` (e.g. `"snap-42/memory.cow"`)
    /// under this store's prefix. Handles the empty-prefix case so
    /// keys never start with `/`.
    fn join_key(&self, relative: &str) -> String {
        if self.prefix.is_empty() {
            relative.to_string()
        } else {
            format!("{}/{}", self.prefix, relative)
        }
    }

    /// Build the key prefix for `snap_id`'s sub-directory (used by
    /// list enumeration + delete).
    fn snap_prefix(&self, snap_id: SnapshotIdNum) -> String {
        self.join_key(&format!("snap-{snap_id}/"))
    }
}

impl SnapshotStore for S3SnapshotStore {
    fn put(&self, snap_id: SnapshotIdNum, src_dir: &Path) -> Result<(), StoreError> {
        let manifest = Manifest::read_from_dir(src_dir)?;
        let manifest_bytes = manifest.to_json_pretty()?;
        let backing_path = manifest.backing_file_path(src_dir);
        let backing_key = self.join_key(&format!("snap-{snap_id}/{}", manifest.backing_file));
        let manifest_key = self.manifest_key(snap_id);

        self.runtime.block_on(async {
            // Manifest is small; ship as a single PutObject from
            // memory. Backing file may be hundreds of MiB; stream
            // it from disk via `ByteStream::from_path`. The SDK
            // handles multipart upload internally for objects
            // above the 5 MiB threshold.
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&manifest_key)
                .body(ByteStream::from(manifest_bytes))
                .content_type("application/json")
                .send()
                .await
                .map_err(|e| sdk_to_store_err("put manifest", e))?;

            if backing_path.exists() {
                let body = ByteStream::from_path(&backing_path)
                    .await
                    .map_err(|e| StoreError::Io(std::io::Error::other(e)))?;
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(&backing_key)
                    .body(body)
                    .content_type("application/octet-stream")
                    .send()
                    .await
                    .map_err(|e| sdk_to_store_err("put backing", e))?;
            }
            Ok::<_, StoreError>(())
        })?;
        Ok(())
    }

    fn get(&self, snap_id: SnapshotIdNum, dst_dir: &Path) -> Result<(), StoreError> {
        fs::create_dir_all(dst_dir)?;
        let manifest_key = self.manifest_key(snap_id);

        self.runtime.block_on(async {
            // 1. Pull manifest
            let manifest_resp = match self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&manifest_key)
                .send()
                .await
            {
                Ok(r) => r,
                Err(err) if is_no_such_key(&err) => {
                    return Err(StoreError::NotFound(snap_id));
                }
                Err(err) => return Err(sdk_to_store_err("get manifest", err)),
            };
            let manifest_bytes = manifest_resp
                .body
                .collect()
                .await
                .map_err(|e| StoreError::Network(format!("manifest body: {e}")))?
                .into_bytes();
            let manifest = Manifest::from_json(&manifest_bytes)?;
            manifest.write_to_dir(dst_dir)?;

            // 2. Pull backing file (if the manifest references one)
            let backing_key = self.join_key(&format!("snap-{snap_id}/{}", manifest.backing_file));
            let backing_resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&backing_key)
                .send()
                .await;
            let backing_resp = match backing_resp {
                Ok(r) => r,
                // A snapshot with manifest-but-no-backing is legal
                // (mock backend snapshots don't ship one). Don't
                // surface that as NotFound.
                Err(err) if is_no_such_key(&err) => return Ok(()),
                Err(err) => return Err(sdk_to_store_err("get backing", err)),
            };
            let dst_backing = manifest.backing_file_path(dst_dir);
            let mut byte_stream = backing_resp.body;
            let mut out = std::fs::File::create(&dst_backing)?;
            while let Some(chunk) = byte_stream
                .try_next()
                .await
                .map_err(|e| StoreError::Network(format!("backing chunk: {e}")))?
            {
                std::io::Write::write_all(&mut out, &chunk)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<SnapshotIdNum>, StoreError> {
        let prefix = if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        };

        self.runtime.block_on(async {
            let mut ids = Vec::new();
            // Use delimiter "/" so S3 collapses everything under
            // each snap-<id>/ into a single CommonPrefix — that's
            // O(snapshots) listings rather than O(objects).
            let mut continuation: Option<String> = None;
            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&self.bucket)
                    .delimiter("/");
                if !prefix.is_empty() {
                    req = req.prefix(&prefix);
                }
                if let Some(token) = continuation {
                    req = req.continuation_token(token);
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| sdk_to_store_err("list snapshots", e))?;
                for cp in resp.common_prefixes() {
                    let Some(p) = cp.prefix() else { continue };
                    // p is e.g. "prod/snap-42/" or "snap-42/"
                    if let Some(rest) = p.strip_prefix(&prefix) {
                        let id_part = rest.trim_end_matches('/');
                        if let Some(num) = id_part.strip_prefix("snap-") {
                            if let Ok(id) = num.parse::<u64>() {
                                ids.push(id);
                            }
                        }
                    }
                }
                continuation = resp.next_continuation_token().map(|s| s.to_string());
                if continuation.is_none() {
                    break;
                }
            }
            ids.sort_unstable();
            Ok(ids)
        })
    }

    fn delete(&self, snap_id: SnapshotIdNum) -> Result<(), StoreError> {
        let prefix = self.snap_prefix(snap_id);

        self.runtime.block_on(async {
            // Enumerate everything under the snap-<id>/ prefix (we
            // know only manifest + backing file are there today but
            // future versions may add device-state sidecars), then
            // delete them in one batch.
            let resp = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix)
                .send()
                .await
                .map_err(|e| sdk_to_store_err("list for delete", e))?;
            let mut to_delete = Vec::new();
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    to_delete.push(k.to_string());
                }
            }
            if to_delete.is_empty() {
                // Idempotent: nothing there, that's a success.
                return Ok(());
            }
            // delete_objects takes up to 1000 keys; we never have
            // anywhere near that, but cap defensively.
            for chunk in to_delete.chunks(1000) {
                let mut delete = aws_sdk_s3::types::Delete::builder();
                for key in chunk {
                    delete = delete.objects(
                        aws_sdk_s3::types::ObjectIdentifier::builder()
                            .key(key)
                            .build()
                            .map_err(|e| StoreError::Network(format!("oid build: {e}")))?,
                    );
                }
                let delete = delete
                    .build()
                    .map_err(|e| StoreError::Network(format!("delete build: {e}")))?;
                self.client
                    .delete_objects()
                    .bucket(&self.bucket)
                    .delete(delete)
                    .send()
                    .await
                    .map_err(|e| sdk_to_store_err("delete objects", e))?;
            }
            Ok(())
        })
    }

    fn display(&self) -> String {
        if self.prefix.is_empty() {
            format!("s3://{}", self.bucket)
        } else {
            format!("s3://{}/{}", self.bucket, self.prefix)
        }
    }
}

/// Translate an SDK error into a [`StoreError`]. Today we
/// distinguish auth from generic network failures by inspecting the
/// HTTP status; everything else goes under `Network`.
fn sdk_to_store_err<E: std::fmt::Display>(op: &'static str, err: E) -> StoreError {
    let msg = format!("{op}: {err}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("forbidden")
        || lower.contains("access denied")
        || lower.contains("invalidaccesskeyid")
        || lower.contains("signaturedoesnotmatch")
    {
        StoreError::Auth(msg)
    } else {
        StoreError::Network(msg)
    }
}

/// Heuristic for "object missing" responses. The SDK exposes this
/// through `NoSuchKey` on `GetObject` and a generic service error
/// otherwise; we inspect the rendered error string because the
/// typed variants are quite verbose and easy to get wrong.
fn is_no_such_key<E: std::fmt::Debug + std::fmt::Display>(err: &E) -> bool {
    let s = format!("{err:?} {err}");
    let l = s.to_ascii_lowercase();
    l.contains("nosuchkey") || l.contains("not found") || l.contains("404")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::parse_store_uri;

    // Network-touching round-trips are integration tests gated on
    // MinIO env vars. The unit suite covers the pure logic only:
    // construction does not fail, key-building stays consistent,
    // display() is correct.

    fn s3_loc(uri: &str) -> StoreLocation {
        parse_store_uri(uri).expect("parse")
    }

    #[test]
    fn construction_with_only_bucket_succeeds() {
        let store =
            S3SnapshotStore::from_location(&s3_loc("s3://nanovm-snapshots")).expect("construct");
        assert_eq!(store.display(), "s3://nanovm-snapshots");
    }

    #[test]
    fn construction_with_prefix_succeeds() {
        let store = S3SnapshotStore::from_location(&s3_loc("s3://nanovm/prod/eu-west-1"))
            .expect("construct");
        assert_eq!(store.display(), "s3://nanovm/prod/eu-west-1");
    }

    #[test]
    fn construction_from_uri_round_trips() {
        let store = S3SnapshotStore::from_uri("s3://nanovm-snapshots/prod").expect("construct");
        assert_eq!(store.display(), "s3://nanovm-snapshots/prod");
    }

    #[test]
    fn construction_rejects_file_location() {
        let err = S3SnapshotStore::from_location(&StoreLocation::File {
            root: "/tmp/x".into(),
        })
        .unwrap_err();
        assert!(matches!(err, StoreError::BadUri(_)), "got {err:?}");
    }

    #[test]
    fn key_layout_without_prefix() {
        let store = S3SnapshotStore::from_uri("s3://b").unwrap();
        assert_eq!(store.manifest_key(42), "snap-42/manifest.json");
        assert_eq!(store.snap_prefix(42), "snap-42/");
        assert_eq!(store.join_key("snap-42/memory.cow"), "snap-42/memory.cow");
    }

    #[test]
    fn key_layout_with_prefix() {
        let store = S3SnapshotStore::from_uri("s3://b/prod/eu-west-1").unwrap();
        assert_eq!(
            store.manifest_key(42),
            "prod/eu-west-1/snap-42/manifest.json"
        );
        assert_eq!(store.snap_prefix(42), "prod/eu-west-1/snap-42/");
    }

    #[test]
    fn sdk_to_store_err_classifies_auth_paths() {
        let err = sdk_to_store_err("op", "Forbidden: access denied");
        assert!(matches!(err, StoreError::Auth(_)), "got {err:?}");
        let err = sdk_to_store_err("op", "SignatureDoesNotMatch");
        assert!(matches!(err, StoreError::Auth(_)), "got {err:?}");
        let err = sdk_to_store_err("op", "503 Service Unavailable");
        assert!(matches!(err, StoreError::Network(_)), "got {err:?}");
    }
}
