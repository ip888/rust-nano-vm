//! SQLite-backed [`OwnershipStore`]. Only compiled with `--features sqlite`.
//!
//! ## Schema (version 1)
//!
//! ```sql
//! CREATE TABLE vms       (id INTEGER PRIMARY KEY, org TEXT NOT NULL);
//! CREATE TABLE snapshots (id INTEGER PRIMARY KEY, org TEXT NOT NULL);
//! CREATE INDEX vms_by_org        ON vms(org);
//! CREATE INDEX snapshots_by_org  ON snapshots(org);
//! ```
//!
//! `id` maps to the `u64` inside `VmId(u64)` / `SnapshotId(u64)`;
//! rusqlite carries the roundtrip via its `FromSql`/`ToSql` impls.
//!
//! ## Migration policy
//!
//! `PRAGMA user_version` starts at 0 on a fresh file. On open we read
//! it, run each pending migration in order (currently just "version 0
//! → 1: create tables"), then bump the pragma to the target. Migrations
//! are wrapped in a single transaction so a crash mid-migration leaves
//! the file at its old version rather than half-migrated.
//!
//! ## Concurrency
//!
//! Rusqlite's [`Connection`] is `!Sync`, so we wrap it in a `Mutex`.
//! SQLite's single-writer model means throughput is bounded by write
//! latency, but at the ownership layer we're doing one small INSERT
//! per API call — the ~µs scale is dwarfed by the KVM ioctls each
//! call triggers.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use vm_core::{SnapshotId, VmId};

use crate::auth::OrgId;

use super::{OwnershipStore, OwnershipStoreError};

/// Current schema version. Bump this + add a new migration branch in
/// [`SqliteStore::migrate`] when the shape changes.
const SCHEMA_VERSION: u32 = 1;

/// SQLite-backed [`OwnershipStore`]. Constructed via [`SqliteStore::open`],
/// which opens or creates the file and runs any pending migrations.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl From<rusqlite::Error> for OwnershipStoreError {
    fn from(e: rusqlite::Error) -> Self {
        OwnershipStoreError::Backend(format!("sqlite: {e}"))
    }
}

impl SqliteStore {
    /// Open (or create) a SQLite ownership store at `path`. Runs
    /// schema migrations as needed.
    ///
    /// Uses SQLite's WAL mode so a slow reader doesn't block writers,
    /// which matters when the same process is holding the connection
    /// under a `Mutex` and multiple axum handlers might contend on it.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, OwnershipStoreError> {
        let conn = Connection::open(path.as_ref())?;
        // WAL is friendlier to concurrent read+write than the default
        // journal mode; foreign_keys is off by default and we don't
        // rely on it.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Retry-on-busy for up to 5 s instead of the default (fail
        // immediately). Two connections against the same file — one
        // from OwnershipStore, one from the billing table — can race
        // on WAL checkpoint; without busy_timeout a concurrent writer
        // sees `SQLITE_BUSY` and the caller's ownership write is
        // silently dropped by the `warn!`-and-continue facade. Losing
        // an ownership write means a fresh redeploy could serve one
        // customer's VM to another org — the exact failure mode this
        // module exists to prevent.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Idempotent migration: reads `PRAGMA user_version`, applies any
    /// missing migrations in a single transaction, then bumps the
    /// pragma. Safe to call on every startup.
    fn migrate(&self) -> Result<(), OwnershipStoreError> {
        let mut guard = self
            .conn
            .lock()
            .map_err(|e| OwnershipStoreError::Backend(format!("mutex poisoned: {e}")))?;
        let current: u32 = guard.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current >= SCHEMA_VERSION {
            return Ok(());
        }
        let tx = guard.transaction()?;
        // v0 → v1: initial schema.
        if current < 1 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS vms (
                    id  INTEGER PRIMARY KEY,
                    org TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS snapshots (
                    id  INTEGER PRIMARY KEY,
                    org TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS vms_by_org       ON vms(org);
                CREATE INDEX IF NOT EXISTS snapshots_by_org ON snapshots(org);",
            )?;
        }
        // Bump user_version. `PRAGMA` doesn't accept bind parameters,
        // so we interpolate the trusted constant.
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        tx.commit()?;
        tracing::info!(
            from = current,
            to = SCHEMA_VERSION,
            "ownership sqlite: migrated"
        );
        Ok(())
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, OwnershipStoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| OwnershipStoreError::Backend(format!("mutex poisoned: {e}")))?;
        Ok(f(&guard)?)
    }
}

impl OwnershipStore for SqliteStore {
    fn record_vm(&self, vm: VmId, org: &OrgId) -> Result<(), OwnershipStoreError> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO vms (id, org) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET org = excluded.org",
                params![vm.0, org.as_str()],
            )?;
            Ok(())
        })
    }

    fn record_snapshot(&self, snap: SnapshotId, org: &OrgId) -> Result<(), OwnershipStoreError> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO snapshots (id, org) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET org = excluded.org",
                params![snap.0, org.as_str()],
            )?;
            Ok(())
        })
    }

    fn forget_vm(&self, vm: VmId) -> Result<(), OwnershipStoreError> {
        self.with_conn(|c| {
            c.execute("DELETE FROM vms WHERE id = ?1", params![vm.0])?;
            Ok(())
        })
    }

    fn forget_snapshot(&self, snap: SnapshotId) -> Result<(), OwnershipStoreError> {
        self.with_conn(|c| {
            c.execute("DELETE FROM snapshots WHERE id = ?1", params![snap.0])?;
            Ok(())
        })
    }

    fn vm_owner(&self, vm: VmId) -> Option<OrgId> {
        self.with_conn(|c| {
            c.query_row("SELECT org FROM vms WHERE id = ?1", params![vm.0], |r| {
                r.get::<_, String>(0)
            })
            .optional()
        })
        .ok()
        .flatten()
        .map(|s| OrgId::new(&s))
    }

    fn snapshot_owner(&self, snap: SnapshotId) -> Option<OrgId> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT org FROM snapshots WHERE id = ?1",
                params![snap.0],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .ok()
        .flatten()
        .map(|s| OrgId::new(&s))
    }

    fn vms_owned_by(&self, org: &OrgId) -> HashSet<VmId> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT id FROM vms WHERE org = ?1")?;
            let ids: Result<HashSet<VmId>, _> = stmt
                .query_map(params![org.as_str()], |r| r.get::<_, i64>(0))?
                .map(|r| r.map(|id| VmId(id as u64)))
                .collect();
            ids
        })
        .unwrap_or_default()
    }

    fn snapshots_owned_by(&self, org: &OrgId) -> HashSet<SnapshotId> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT id FROM snapshots WHERE org = ?1")?;
            let ids: Result<HashSet<SnapshotId>, _> = stmt
                .query_map(params![org.as_str()], |r| r.get::<_, i64>(0))?
                .map(|r| r.map(|id| SnapshotId(id as u64)))
                .collect();
            ids
        })
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn org(s: &str) -> OrgId {
        OrgId::new(s)
    }

    #[test]
    fn open_creates_file_and_runs_migration() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ownership.sqlite");
        assert!(!path.exists());
        let _store = SqliteStore::open(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn record_lookup_forget_round_trip() {
        let dir = tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("ownership.sqlite")).unwrap();
        store.record_vm(VmId(42), &org("acme")).unwrap();
        assert_eq!(store.vm_owner(VmId(42)), Some(org("acme")));
        store.forget_vm(VmId(42)).unwrap();
        assert_eq!(store.vm_owner(VmId(42)), None);
    }

    #[test]
    fn ownership_survives_reopening_the_same_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ownership.sqlite");
        {
            let store = SqliteStore::open(&path).unwrap();
            store.record_vm(VmId(1), &org("acme")).unwrap();
            store
                .record_snapshot(SnapshotId(9), &org("globex"))
                .unwrap();
        }
        // Drop → close. Re-open a fresh Store on the same file.
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.vm_owner(VmId(1)), Some(org("acme")));
        assert_eq!(reopened.snapshot_owner(SnapshotId(9)), Some(org("globex")));
    }

    #[test]
    fn record_overwrites_prior_owner() {
        let dir = tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("ownership.sqlite")).unwrap();
        store.record_vm(VmId(7), &org("acme")).unwrap();
        store.record_vm(VmId(7), &org("globex")).unwrap();
        assert_eq!(store.vm_owner(VmId(7)), Some(org("globex")));
    }

    #[test]
    fn vms_owned_by_returns_only_that_orgs_ids() {
        let dir = tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("ownership.sqlite")).unwrap();
        store.record_vm(VmId(1), &org("acme")).unwrap();
        store.record_vm(VmId(2), &org("acme")).unwrap();
        store.record_vm(VmId(3), &org("globex")).unwrap();
        let mut acme: Vec<_> = store.vms_owned_by(&org("acme")).into_iter().collect();
        acme.sort_by_key(|id| id.0);
        assert_eq!(acme, vec![VmId(1), VmId(2)]);
        let globex: Vec<_> = store.vms_owned_by(&org("globex")).into_iter().collect();
        assert_eq!(globex, vec![VmId(3)]);
    }

    #[test]
    fn migrate_is_idempotent_across_reopens() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ownership.sqlite");
        let _a = SqliteStore::open(&path).unwrap();
        let _b = SqliteStore::open(&path).unwrap();
        let _c = SqliteStore::open(&path).unwrap();
        // No panic == success. The pragma-guarded migration should
        // be a no-op after the first open.
    }
}
