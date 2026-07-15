//! Snapshot marketplace — read-side MVP.
//!
//! A **curated public registry** of pre-built snapshot images that any
//! caller can list (unauthenticated) via `GET /v1/marketplace/snapshots`.
//!
//! This is the wedge feature vs AWS Lambda MicroVMs: their model is
//! "you write a Dockerfile, they snapshot at image-build-time, you fork
//! that." Ours is "here's a public library of pre-warmed sandbox
//! environments — pick one and fork in ~12 ms." A follow-up PR adds the
//! `POST /v1/marketplace/snapshots/:name/fork` flow (pull the tarball
//! into the tenant's snapshot store, then fork). This PR ships only
//! the read side so the shape lands without operational surface for
//! marketplace hosting.
//!
//! ## Config
//!
//! `NANOVM_MARKETPLACE_CONFIG=/path/to/marketplace.json` — unset →
//! empty registry, endpoint returns `{"snapshots": []}`.
//!
//! File shape:
//!
//! ```json
//! {
//!   "snapshots": [
//!     {
//!       "name": "python-3.12-ds",
//!       "description": "Python 3.12 + pandas + numpy + scikit-learn",
//!       "size_bytes": 52428800,
//!       "kernel_url": "https://cdn.example/marketplace/python-3.12-ds/vmlinux",
//!       "rootfs_url": "https://cdn.example/marketplace/python-3.12-ds/rootfs.ext4",
//!       "cmdline": "console=ttyS0 root=/dev/vda rw",
//!       "labels": ["python", "data-science"],
//!       "maintainer": "nanovm-marketplace"
//!     }
//!   ]
//! }
//! ```
//!
//! Malformed entries are logged at `warn` and skipped — same
//! don't-crash-on-typo posture as `NANOVM_PLAN_TIERS`.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Env var pointing at the marketplace JSON config file.
pub const CONFIG_PATH_ENV: &str = "NANOVM_MARKETPLACE_CONFIG";

/// One entry in the curated marketplace. Rich enough for the dashboard
/// / CLI to render a decent picker; keep everything client-safe (no
/// admin tokens, no S3 credentials, only URLs that would be public
/// anyway).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MarketplaceSnapshot {
    /// Unique, human-friendly id like `python-3.12-ds`. Used as the
    /// URL segment for the (future) fork endpoint. Must match
    /// `[a-z0-9][a-z0-9.-]*` (no trailing `-` or `.`) — see
    /// [`is_valid_name`].
    pub name: String,
    /// One-line description shown next to the name in a picker.
    pub description: String,
    /// Approximate uncompressed size of `rootfs_url`, in bytes. Lets
    /// the dashboard render "~50 MB" without HEAD-ing the URL.
    pub size_bytes: u64,
    /// Public URL to the kernel binary (vmlinux or bzImage).
    pub kernel_url: String,
    /// Public URL to the rootfs image (ext4, squashfs, etc.).
    pub rootfs_url: String,
    /// Kernel cmdline the snapshot was captured with. Consumers
    /// forking a marketplace snapshot should pass this through
    /// verbatim unless they know what they're doing.
    pub cmdline: String,
    /// Free-form tags for filtering in a UI: `["python", "ml"]`,
    /// `["node", "playwright"]`, etc.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Who publishes / maintains this snapshot. `"nanovm-marketplace"`
    /// for first-party entries; will grow to include community
    /// publishers when the write-side of the marketplace ships.
    pub maintainer: String,
}

/// Wire response for `GET /v1/marketplace/snapshots`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceListResponse {
    /// The catalogue's snapshot entries in the order the operator
    /// listed them. The list is intentionally not sorted server-side —
    /// clients that want a specific order should sort themselves.
    pub snapshots: Vec<MarketplaceSnapshot>,
}

/// In-memory catalogue — loaded once at startup, cheap to clone into
/// per-request handlers. If the operator wants to reload without a
/// restart, that's a follow-up (SIGHUP / file-watcher).
#[derive(Debug, Clone, Default)]
pub struct Marketplace {
    snapshots: Vec<MarketplaceSnapshot>,
}

impl Marketplace {
    /// Read the config file named by `NANOVM_MARKETPLACE_CONFIG`.
    /// Unset / unreadable / malformed → empty catalogue (endpoint
    /// returns `{"snapshots": []}`). Every path except "unset" is
    /// logged so an operator can tell the difference.
    pub fn from_env() -> Self {
        let Some(path) = std::env::var(CONFIG_PATH_ENV)
            .ok()
            .filter(|s| !s.is_empty())
        else {
            tracing::debug!(
                "marketplace: {CONFIG_PATH_ENV} unset — the /v1/marketplace/snapshots \
                 endpoint will return an empty list"
            );
            return Self::default();
        };
        match Self::load_from_file(&path) {
            Ok(m) => {
                tracing::info!(
                    path,
                    count = m.snapshots.len(),
                    "marketplace: loaded snapshot catalogue"
                );
                m
            }
            Err(e) => {
                tracing::error!(path, error = %e, "marketplace: config load failed");
                Self::default()
            }
        }
    }

    /// Same as [`from_env`](Self::from_env) but takes an explicit
    /// path — useful in tests and for callers that already parsed
    /// their own config.
    pub fn load_from_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        Ok(Self::parse(&raw))
    }

    /// Parse the JSON payload directly. Malformed top-level shape
    /// (not a JSON object with a `snapshots` array) yields an empty
    /// catalogue (logged). Malformed *individual* entries — missing
    /// required fields, wrong types, invalid names, empty URLs — are
    /// skipped with a per-entry warn so a typo doesn't take down the
    /// whole registry.
    ///
    /// We deserialize the outer envelope as `Value` and each entry
    /// individually rather than the whole `MarketplaceListResponse`
    /// at once — serde's default all-or-nothing behavior on a
    /// `Vec<MarketplaceSnapshot>` would fail the whole parse on one
    /// bad entry, defeating the graceful-degradation goal.
    pub fn parse(raw: &str) -> Self {
        let Ok(root) = serde_json::from_str::<serde_json::Value>(raw) else {
            tracing::warn!("marketplace: config is not valid JSON; treating as empty");
            return Self::default();
        };
        let Some(arr) = root.get("snapshots").and_then(|v| v.as_array()) else {
            tracing::warn!(
                "marketplace: config is missing the top-level `snapshots` array; treating as empty"
            );
            return Self::default();
        };
        let mut out = Vec::new();
        for (idx, raw_entry) in arr.iter().enumerate() {
            let entry: MarketplaceSnapshot = match serde_json::from_value(raw_entry.clone()) {
                Ok(e) => e,
                Err(e) => {
                    let name_hint = raw_entry
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    tracing::warn!(
                        index = idx,
                        name = name_hint,
                        error = %e,
                        "marketplace: skipping malformed entry"
                    );
                    continue;
                }
            };
            if !is_valid_name(&entry.name) {
                tracing::warn!(
                    name = %entry.name,
                    "marketplace: skipping entry with invalid name (must match [a-z0-9][a-z0-9.-]*, no trailing - or .)"
                );
                continue;
            }
            if entry.kernel_url.is_empty() || entry.rootfs_url.is_empty() {
                tracing::warn!(
                    name = %entry.name,
                    "marketplace: skipping entry with empty kernel_url or rootfs_url"
                );
                continue;
            }
            out.push(entry);
        }
        Self { snapshots: out }
    }

    /// True when the operator hasn't configured any marketplace
    /// entries. Handlers can use this to decide whether to advertise
    /// the endpoint on `/openapi.json` (future).
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// Number of entries. Cheap.
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// Clone the full list — meant for the list handler, which
    /// returns the whole catalogue as JSON. Marketplaces are small
    /// enough that a full copy per request is fine; if we ever
    /// grow past a few hundred entries this becomes a
    /// `&[MarketplaceSnapshot]` accessor + `Cow`.
    pub fn all(&self) -> Vec<MarketplaceSnapshot> {
        self.snapshots.clone()
    }

    /// Look up a single entry by name. Returns `None` for unknown
    /// names. Used by the (future) fork endpoint.
    pub fn get(&self, name: &str) -> Option<&MarketplaceSnapshot> {
        self.snapshots.iter().find(|s| s.name == name)
    }
}

/// Names must be URL-safe (no `/`, no `?`, no whitespace) because
/// they become the path segment in `POST /v1/marketplace/snapshots/:name/fork`.
/// Regex: `[a-z0-9][a-z0-9.-]*` with no trailing `-` or `.` — allows
/// natural versioned names like `python-3.12-ds` while keeping
/// underscores and uppercase out (visually consistent, forces
/// publishers into the marketplace convention).
fn is_valid_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    // First char must be [a-z0-9].
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    // Trailing chars must not be `-` or `.` — avoids `python-` or
    // `python.` which look like typos.
    let last = *bytes.last().unwrap();
    if last == b'-' || last == b'.' {
        return false;
    }
    for &c in &bytes[1..] {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.';
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"
        {
          "snapshots": [
            {
              "name": "python-3.12-ds",
              "description": "Python 3.12 + pandas + numpy",
              "size_bytes": 52428800,
              "kernel_url": "https://cdn.example/py/vmlinux",
              "rootfs_url": "https://cdn.example/py/rootfs.ext4",
              "cmdline": "console=ttyS0 root=/dev/vda rw",
              "labels": ["python", "ml"],
              "maintainer": "nanovm-marketplace"
            },
            {
              "name": "node-20-playwright",
              "description": "Node 20 + Playwright + Chromium",
              "size_bytes": 209715200,
              "kernel_url": "https://cdn.example/node/vmlinux",
              "rootfs_url": "https://cdn.example/node/rootfs.ext4",
              "cmdline": "console=ttyS0 root=/dev/vda rw",
              "maintainer": "nanovm-marketplace"
            }
          ]
        }
        "#
    }

    #[test]
    fn parse_happy_path() {
        let m = Marketplace::parse(sample_json());
        assert_eq!(m.len(), 2);
        let py = m.get("python-3.12-ds").expect("python entry present");
        assert_eq!(py.size_bytes, 52428800);
        assert_eq!(py.labels, vec!["python", "ml"]);
        let node = m.get("node-20-playwright").expect("node entry present");
        assert!(node.labels.is_empty(), "no labels → serde default = []");
    }

    #[test]
    fn parse_empty_root_yields_empty_catalogue() {
        let m = Marketplace::parse(r#"{"snapshots":[]}"#);
        assert!(m.is_empty());
    }

    #[test]
    fn parse_malformed_top_level_yields_empty_catalogue() {
        let m = Marketplace::parse("not json at all");
        assert!(m.is_empty());
    }

    #[test]
    fn parse_skips_entries_with_invalid_name() {
        let raw = r#"
        {
          "snapshots": [
            {
              "name": "Bad_Name/../etc",
              "description": "x", "size_bytes": 1,
              "kernel_url": "https://k", "rootfs_url": "https://r",
              "cmdline": "", "maintainer": "m"
            },
            {
              "name": "good-one",
              "description": "x", "size_bytes": 1,
              "kernel_url": "https://k", "rootfs_url": "https://r",
              "cmdline": "", "maintainer": "m"
            }
          ]
        }
        "#;
        let m = Marketplace::parse(raw);
        assert_eq!(m.len(), 1);
        assert!(m.get("good-one").is_some());
        assert!(m.get("Bad_Name/../etc").is_none());
    }

    #[test]
    fn parse_skips_individual_malformed_entries_without_killing_the_catalogue() {
        // One entry is missing `cmdline` (required, no serde default) —
        // the whole-document deserialize used to fail on this and drop
        // every entry. New parser skips the bad entry and keeps the
        // rest.
        let raw = r#"
        {"snapshots": [
          {"name":"good-one","description":"x","size_bytes":1,
           "kernel_url":"https://k","rootfs_url":"https://r",
           "cmdline":"","labels":[],"maintainer":"m"},
          {"name":"missing-cmdline","description":"x","size_bytes":1,
           "kernel_url":"https://k","rootfs_url":"https://r",
           "labels":[],"maintainer":"m"}
        ]}
        "#;
        let m = Marketplace::parse(raw);
        assert_eq!(m.len(), 1, "the well-formed entry must survive");
        assert!(m.get("good-one").is_some());
    }

    #[test]
    fn parse_skips_entries_missing_urls() {
        let raw = r#"
        {"snapshots": [
          {"name":"missing-kernel","description":"x","size_bytes":1,
           "kernel_url":"","rootfs_url":"https://r","cmdline":"","maintainer":"m"},
          {"name":"missing-rootfs","description":"x","size_bytes":1,
           "kernel_url":"https://k","rootfs_url":"","cmdline":"","maintainer":"m"}
        ]}
        "#;
        assert!(Marketplace::parse(raw).is_empty());
    }

    #[test]
    fn valid_names() {
        for good in [
            "python-3.12-ds",
            "a",
            "1",
            "a1",
            "node-20-playwright",
            "python-3.12", // dot in the middle is fine
            "abc.def",
        ] {
            assert!(is_valid_name(good), "want {good:?} to be valid");
        }
        for bad in [
            "",
            "Uppercase",
            "under_score",
            "-leading-dash",
            ".leading-dot",
            "trailing-",
            "trailing.",
            "has/slash",
            "has space",
            "has?query",
        ] {
            assert!(!is_valid_name(bad), "want {bad:?} to be invalid");
        }
    }

    #[test]
    fn load_from_file_roundtrips() {
        use tempfile::NamedTempFile;
        let mut tmp = NamedTempFile::new().unwrap();
        std::io::Write::write_all(tmp.as_file_mut(), sample_json().as_bytes()).unwrap();
        let m = Marketplace::load_from_file(tmp.path()).unwrap();
        assert_eq!(m.len(), 2);
    }
}
