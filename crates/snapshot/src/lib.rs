//! Snapshot format and CoW backing-file layout.
//!
//! This is the M5 wedge — "snapshot once, fork many" — but the runtime
//! pieces (userfaultfd, CoW memory sharing) land in a later PR. What this
//! crate ships today is the **on-disk file format** so that:
//!
//! - CLI / control-plane can already serialize and enumerate snapshots.
//! - The snapshot-writer and snapshot-reader can be unit-tested on any
//!   machine (no KVM / no userfaultfd needed).
//! - Future format evolution is version-gated from day one.
//!
//! # On-disk layout
//!
//! A snapshot is a directory:
//!
//! ```text
//! <snap-id>/
//! ├── manifest.json       — [`Manifest`], serde-JSON, human-inspectable
//! └── memory.cow          — header + page data
//!                           (optional for mock; required for real backends)
//! ```
//!
//! `manifest.json` holds metadata (size, page count, labels). The memory
//! image itself starts with a [`BackingFileHeader`] — a 64-byte fixed
//! binary record whose magic bytes `NANOVMS1` identify the file to tooling
//! and whose `page_size` / `page_count` tell a reader how to stride through
//! the subsequent page data. Everything is little-endian and pinned by
//! dedicated byte-offset tests.
//!
//! Device state and vCPU register dumps (TSC, MSRs, IDT, ...) follow in a
//! later PR alongside the real vm-kvm snapshot implementation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current snapshot format version. Bump on any backwards-incompatible
/// change (field rename, layout change, new required field). Readers MUST
/// refuse files whose `format_version` doesn't match a version they know.
pub const FORMAT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Manifest (manifest.json)
// ---------------------------------------------------------------------------

/// Top-level per-snapshot metadata. Serialized as JSON so it's
/// human-inspectable and trivially introspectable by operators via `cat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version. Must equal [`FORMAT_VERSION`] when the reader loads
    /// a manifest; otherwise [`SnapshotError::VersionMismatch`].
    pub format_version: u32,
    /// Opaque snapshot id, matches `SnapshotId(u64)` in vm-core.
    pub snapshot_id: u64,
    /// Milliseconds since the UNIX epoch at which the snapshot was taken.
    pub created_at_unix_ms: u64,
    /// Guest memory size in bytes (the whole RAM slab, not just dirty pages).
    pub memory_bytes: u64,
    /// Guest page size in bytes. Virtually always 4096 on x86_64, but
    /// pinned here so a reader on a host with a different page size can
    /// detect the mismatch instead of silently striding wrong.
    pub page_size: u32,
    /// Number of vCPUs captured.
    pub vcpu_count: u32,
    /// Kernel cmdline the guest was booted with. Captured so an operator
    /// can see what the base image was configured for.
    #[serde(default)]
    pub kernel_cmdline: String,
    /// Path to the CoW backing file, relative to the manifest's directory.
    /// Defaults to `"memory.cow"`.
    #[serde(default = "default_backing_file")]
    pub backing_file: String,
    /// Free-form labels an operator / orchestrator can attach. Sorted map
    /// so JSON output is deterministic (good for diffing).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

fn default_backing_file() -> String {
    "memory.cow".to_owned()
}

impl Manifest {
    /// Fresh manifest with [`FORMAT_VERSION`] and default backing file.
    pub fn new(snapshot_id: u64, memory_bytes: u64, page_size: u32, vcpu_count: u32) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            snapshot_id,
            created_at_unix_ms: 0,
            memory_bytes,
            page_size,
            vcpu_count,
            kernel_cmdline: String::new(),
            backing_file: default_backing_file(),
            labels: BTreeMap::new(),
        }
    }

    /// Decode from a JSON byte slice. Rejects files whose `format_version`
    /// we don't understand.
    pub fn from_json(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let parsed: Self = serde_json::from_slice(bytes)?;
        if parsed.format_version != FORMAT_VERSION {
            return Err(SnapshotError::VersionMismatch {
                found: parsed.format_version,
                expected: FORMAT_VERSION,
            });
        }
        Ok(parsed)
    }

    /// Encode to pretty JSON (2-space indent) so `cat manifest.json` is
    /// readable. If you need compact output, use `serde_json::to_vec`
    /// directly.
    pub fn to_json_pretty(&self) -> Result<Vec<u8>, SnapshotError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

// ---------------------------------------------------------------------------
// Backing-file header (memory.cow)
// ---------------------------------------------------------------------------

/// On-the-wire size of the backing-file header in bytes.
pub const BACKING_HDR_LEN: usize = 64;

/// Magic bytes that identify a rust-nano-vm snapshot backing file. Readers
/// MUST verify these before parsing any further; anything else is a
/// foreign file.
pub const BACKING_MAGIC: [u8; 8] = *b"NANOVMS1";

/// Fixed 64-byte binary header at the start of `memory.cow`.
///
/// Layout (all little-endian):
///
/// | offset | size | field         |
/// |--------|------|---------------|
/// | 0      | 8    | magic = b"NANOVMS1" |
/// | 8      | 4    | format_version |
/// | 12     | 4    | page_size     |
/// | 16     | 8    | page_count    |
/// | 24     | 8    | memory_bytes  |
/// | 32     | 4    | flags         |
/// | 36     | 28   | reserved (zero) |
///
/// Page data follows the header immediately. There are `page_count`
/// records each `page_size` bytes; total `page_count * page_size ==
/// memory_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackingFileHeader {
    /// Format version. Must equal [`FORMAT_VERSION`] on read.
    pub format_version: u32,
    /// Page size used when the snapshot was captured.
    pub page_size: u32,
    /// Number of pages stored after the header.
    pub page_count: u64,
    /// `page_count * page_size`. Stored redundantly as a sanity check
    /// against silent corruption.
    pub memory_bytes: u64,
    /// Bit-field reserved for future layout flags (e.g. compression,
    /// zero-page elision). No bits defined today.
    pub flags: u32,
}

impl BackingFileHeader {
    /// Construct a new header for the given geometry. Asserts
    /// `page_count * page_size == memory_bytes` — if you need to encode a
    /// mismatch (don't), do it manually.
    pub fn new(page_size: u32, page_count: u64) -> Self {
        let memory_bytes = page_size as u64 * page_count;
        Self {
            format_version: FORMAT_VERSION,
            page_size,
            page_count,
            memory_bytes,
            flags: 0,
        }
    }

    /// Parse the first [`BACKING_HDR_LEN`] bytes of `buf` as a header.
    /// `buf` must be **at least** that long; trailing bytes (the page
    /// data) are ignored.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, SnapshotError> {
        if buf.len() < BACKING_HDR_LEN {
            return Err(SnapshotError::ShortHeader {
                have: buf.len(),
                need: BACKING_HDR_LEN,
            });
        }
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[0..8]);
        if magic != BACKING_MAGIC {
            return Err(SnapshotError::BadMagic { found: magic });
        }
        let format_version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(SnapshotError::VersionMismatch {
                found: format_version,
                expected: FORMAT_VERSION,
            });
        }
        let page_size = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        let page_count = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let memory_bytes = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let flags = u32::from_le_bytes(buf[32..36].try_into().unwrap());
        if page_size as u64 * page_count != memory_bytes {
            return Err(SnapshotError::Inconsistent {
                page_size,
                page_count,
                memory_bytes,
            });
        }
        Ok(Self {
            format_version,
            page_size,
            page_count,
            memory_bytes,
            flags,
        })
    }

    /// Serialize into `buf`, which must be at least [`BACKING_HDR_LEN`]
    /// bytes. Writes exactly [`BACKING_HDR_LEN`] bytes (including zeroing
    /// the reserved tail) and returns that count.
    pub fn write_to(&self, buf: &mut [u8]) -> Result<usize, SnapshotError> {
        if buf.len() < BACKING_HDR_LEN {
            return Err(SnapshotError::ShortBuffer {
                have: buf.len(),
                need: BACKING_HDR_LEN,
            });
        }
        buf[0..8].copy_from_slice(&BACKING_MAGIC);
        buf[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        buf[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        buf[16..24].copy_from_slice(&self.page_count.to_le_bytes());
        buf[24..32].copy_from_slice(&self.memory_bytes.to_le_bytes());
        buf[32..36].copy_from_slice(&self.flags.to_le_bytes());
        // Reserved tail zeroed so two equivalent headers byte-compare
        // equal (easier round-trip testing).
        for byte in &mut buf[36..BACKING_HDR_LEN] {
            *byte = 0;
        }
        Ok(BACKING_HDR_LEN)
    }

    /// Serialize into a fresh fixed-size byte array.
    pub fn to_bytes(&self) -> [u8; BACKING_HDR_LEN] {
        let mut out = [0u8; BACKING_HDR_LEN];
        self.write_to(&mut out)
            .expect("serializing BackingFileHeader into a fixed-size buffer must succeed");
        out
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors produced while reading / writing snapshot artefacts.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// Input byte slice is smaller than the backing-file header.
    #[error("backing-file header too short: have {have} bytes, need {need}")]
    ShortHeader {
        /// Bytes we were handed.
        have: usize,
        /// Bytes the header requires.
        need: usize,
    },
    /// Output byte slice is smaller than the backing-file header.
    #[error("backing-file output buffer too small: have {have} bytes, need {need}")]
    ShortBuffer {
        /// Bytes in the output buffer.
        have: usize,
        /// Bytes the serialized header requires.
        need: usize,
    },
    /// Backing file doesn't start with [`BACKING_MAGIC`]. Almost certainly
    /// a foreign file, not a bugged rust-nano-vm snapshot.
    #[error(
        "bad backing-file magic: found {found:02x?}, expected {:02x?}",
        BACKING_MAGIC
    )]
    BadMagic {
        /// Magic bytes we found.
        found: [u8; 8],
    },
    /// Manifest or backing file advertises a format version this reader
    /// doesn't understand.
    #[error("snapshot format version mismatch: found {found}, expected {expected}")]
    VersionMismatch {
        /// Version embedded in the file.
        found: u32,
        /// Version this build supports.
        expected: u32,
    },
    /// `page_size * page_count` doesn't match `memory_bytes`. The file is
    /// corrupt or was written by a buggy producer.
    #[error(
        "inconsistent backing-file geometry: page_size={page_size}, \
         page_count={page_count}, memory_bytes={memory_bytes}"
    )]
    Inconsistent {
        /// Page size as recorded in the header.
        page_size: u32,
        /// Page count as recorded in the header.
        page_count: u64,
        /// Memory size as recorded in the header.
        memory_bytes: u64,
    },
    /// The JSON manifest failed to serialize or deserialize.
    #[error("manifest json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Manifest ------------------------------------------------------

    #[test]
    fn manifest_roundtrips_through_json() {
        let mut m = Manifest::new(0x42, 256 * 1024 * 1024, 4096, 2);
        m.created_at_unix_ms = 1_700_000_000_000;
        m.kernel_cmdline = "console=ttyS0 panic=1".into();
        m.labels.insert("base".into(), "python3.12".into());
        m.labels.insert("tool".into(), "uv".into());

        let bytes = m.to_json_pretty().unwrap();
        let back = Manifest::from_json(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn manifest_backing_file_defaults_to_memory_cow_when_absent() {
        let json = br#"{
            "format_version": 1,
            "snapshot_id": 1,
            "created_at_unix_ms": 0,
            "memory_bytes": 4096,
            "page_size": 4096,
            "vcpu_count": 1
        }"#;
        let m = Manifest::from_json(json).unwrap();
        assert_eq!(m.backing_file, "memory.cow");
        assert!(m.labels.is_empty());
        assert_eq!(m.kernel_cmdline, "");
    }

    #[test]
    fn manifest_rejects_unknown_format_version() {
        let json = br#"{
            "format_version": 9999,
            "snapshot_id": 1,
            "created_at_unix_ms": 0,
            "memory_bytes": 0,
            "page_size": 4096,
            "vcpu_count": 0
        }"#;
        let err = Manifest::from_json(json).unwrap_err();
        assert!(matches!(
            err,
            SnapshotError::VersionMismatch {
                found: 9999,
                expected: 1
            }
        ));
    }

    #[test]
    fn manifest_labels_are_sorted_for_deterministic_output() {
        let mut m = Manifest::new(1, 4096, 4096, 1);
        // Insert in reverse order — BTreeMap sorts on iteration.
        m.labels.insert("z".into(), "last".into());
        m.labels.insert("a".into(), "first".into());
        let json = String::from_utf8(m.to_json_pretty().unwrap()).unwrap();
        let a_pos = json.find("\"a\"").unwrap();
        let z_pos = json.find("\"z\"").unwrap();
        assert!(a_pos < z_pos, "labels must be emitted in sorted order");
    }

    // ---- BackingFileHeader --------------------------------------------

    fn sample_header() -> BackingFileHeader {
        BackingFileHeader::new(4096, 65536) // 256 MiB
    }

    #[test]
    fn backing_header_length_is_exactly_64() {
        assert_eq!(BACKING_HDR_LEN, 64);
    }

    #[test]
    fn backing_magic_is_nanovm_s1_ascii() {
        assert_eq!(&BACKING_MAGIC, b"NANOVMS1");
    }

    #[test]
    fn backing_header_roundtrips_every_field() {
        let h = sample_header();
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), BACKING_HDR_LEN);
        let back = BackingFileHeader::from_bytes(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn backing_header_field_offsets_match_spec() {
        let h = BackingFileHeader {
            format_version: 0x0202_0202,
            page_size: 0x0303_0303,
            page_count: 0x0404_0404_0404_0404,
            memory_bytes: 0x0404_0404_0404_0404u64.wrapping_mul(0x0303_0303), // don't care
            flags: 0x0606_0606,
        };
        // Override the new-computed memory_bytes so we can inspect raw
        // bytes deterministically.
        let h = BackingFileHeader {
            memory_bytes: 0x0505_0505_0505_0505,
            ..h
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..8], b"NANOVMS1");
        assert_eq!(&b[8..12], &[0x02; 4]);
        assert_eq!(&b[12..16], &[0x03; 4]);
        assert_eq!(&b[16..24], &[0x04; 8]);
        assert_eq!(&b[24..32], &[0x05; 8]);
        assert_eq!(&b[32..36], &[0x06; 4]);
        // Reserved tail must be zeroed.
        assert_eq!(&b[36..BACKING_HDR_LEN], &[0u8; 28]);
    }

    #[test]
    fn backing_header_accepts_longer_buffer_and_ignores_trailing_bytes() {
        // Simulates reading the first slab of memory.cow: header || page data.
        let h = sample_header();
        let mut packet = Vec::with_capacity(BACKING_HDR_LEN + 4096);
        packet.extend_from_slice(&h.to_bytes());
        packet.extend_from_slice(&[0x42; 4096]);
        let back = BackingFileHeader::from_bytes(&packet).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn backing_header_rejects_short_input() {
        let short = [0u8; BACKING_HDR_LEN - 1];
        let err = BackingFileHeader::from_bytes(&short).unwrap_err();
        assert!(matches!(
            err,
            SnapshotError::ShortHeader { have: 63, need: 64 }
        ));
    }

    #[test]
    fn backing_header_rejects_foreign_magic() {
        // A buffer that's long enough but doesn't start with NANOVMS1.
        let mut buf = [0u8; BACKING_HDR_LEN];
        buf[0..8].copy_from_slice(b"QCOW2\0\0\0");
        let err = BackingFileHeader::from_bytes(&buf).unwrap_err();
        assert!(matches!(
            err,
            SnapshotError::BadMagic { found } if &found == b"QCOW2\0\0\0"
        ));
    }

    #[test]
    fn backing_header_rejects_unknown_format_version() {
        let mut buf = sample_header().to_bytes();
        buf[8..12].copy_from_slice(&9999u32.to_le_bytes());
        let err = BackingFileHeader::from_bytes(&buf).unwrap_err();
        assert!(matches!(
            err,
            SnapshotError::VersionMismatch {
                found: 9999,
                expected: 1
            }
        ));
    }

    #[test]
    fn backing_header_rejects_inconsistent_geometry() {
        // Manually write a header whose page_size * page_count != memory_bytes.
        let mut buf = [0u8; BACKING_HDR_LEN];
        buf[0..8].copy_from_slice(&BACKING_MAGIC);
        buf[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&4096u32.to_le_bytes()); // page_size
        buf[16..24].copy_from_slice(&10u64.to_le_bytes()); // page_count
        buf[24..32].copy_from_slice(&999_999u64.to_le_bytes()); // memory_bytes (wrong)
        let err = BackingFileHeader::from_bytes(&buf).unwrap_err();
        assert!(matches!(
            err,
            SnapshotError::Inconsistent {
                page_size: 4096,
                page_count: 10,
                memory_bytes: 999_999,
            }
        ));
    }

    #[test]
    fn write_to_rejects_short_output_buffer() {
        let h = sample_header();
        let mut buf = [0u8; 8];
        let err = h.write_to(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            SnapshotError::ShortBuffer { have: 8, need: 64 }
        ));
    }

    #[test]
    fn new_computes_memory_bytes_consistently() {
        let h = BackingFileHeader::new(4096, 1024);
        assert_eq!(h.memory_bytes, 4096 * 1024);
        let h = BackingFileHeader::new(4096, 0);
        assert_eq!(h.memory_bytes, 0);
    }
}
