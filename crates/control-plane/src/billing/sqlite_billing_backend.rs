//! SQLite-backed [`BillingStore`]. Compiled only under `--features billing`
//! (which implies `sqlite`).
//!
//! Uses the same SQLite file as the ownership store. Its own table
//! (`stripe_customers`) lives alongside `vms` / `snapshots`, migrated
//! independently via `PRAGMA application_id`. The ownership store
//! uses `PRAGMA user_version`; we deliberately use a different PRAGMA
//! slot so the two subsystems can evolve their schemas independently.
//!
//! ## Concurrency
//!
//! Same rusqlite reasoning as the ownership backend: `Connection` is
//! `!Sync`, wrap in a `Mutex`. Two SQLite connections against the
//! same file (one from the ownership store, one from here) are fine
//! — SQLite's own file locks handle it.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::auth::OrgId;

use super::{BillingStore, BillingStoreError};

/// Schema version for the billing table.
const SCHEMA_VERSION: u32 = 1;

/// SQLite-backed [`BillingStore`].
#[derive(Debug)]
pub struct SqliteBillingStore {
    conn: Mutex<Connection>,
}

impl From<rusqlite::Error> for BillingStoreError {
    fn from(e: rusqlite::Error) -> Self {
        BillingStoreError::Backend(format!("sqlite: {e}"))
    }
}

impl SqliteBillingStore {
    /// Open (or create) the billing table at `path`. Runs schema
    /// migration idempotently.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, BillingStoreError> {
        let conn = Connection::open(path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // 5 s busy_timeout — same reasoning as the ownership store.
        // Two SQLite connections against the same file (one from
        // OwnershipStore, one from BillingStore) can contend during
        // WAL checkpoint; without this a signup's Stripe customer
        // write can be silently dropped, stranding the customer's
        // subscription without a mapping.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), BillingStoreError> {
        let mut guard = self
            .conn
            .lock()
            .map_err(|e| BillingStoreError::Backend(format!("mutex poisoned: {e}")))?;
        // Distinct pragma from the ownership store: this file has TWO
        // conceptual schemas (ownership + billing). We keep them
        // separately versioned so migrating one doesn't require
        // bumping the other. Use `user_version` for ownership (owned
        // by `ownership::sqlite_backend`) and `application_id` for
        // billing. This isn't SQLite's intended semantic for
        // application_id, but it's a spare 32-bit slot we can
        // repurpose without colliding.
        //
        // Alternative would be a `_meta` table with a schema-name
        // primary key; kept as follow-up when we grow a third
        // subsystem.
        let current: u32 = guard.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        if current >= SCHEMA_VERSION {
            return Ok(());
        }
        let tx = guard.transaction()?;
        if current < 1 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS stripe_customers (
                    org         TEXT PRIMARY KEY,
                    customer_id TEXT NOT NULL,
                    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );",
            )?;
        }
        tx.execute_batch(&format!("PRAGMA application_id = {SCHEMA_VERSION}"))?;
        tx.commit()?;
        tracing::info!(
            from = current,
            to = SCHEMA_VERSION,
            "billing sqlite: migrated"
        );
        Ok(())
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, BillingStoreError> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| BillingStoreError::Backend(format!("mutex poisoned: {e}")))?;
        Ok(f(&guard)?)
    }
}

impl BillingStore for SqliteBillingStore {
    fn record_customer(&self, org: &OrgId, customer_id: &str) -> Result<(), BillingStoreError> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO stripe_customers (org, customer_id) VALUES (?1, ?2)
                 ON CONFLICT(org) DO UPDATE SET customer_id = excluded.customer_id",
                params![org.as_str(), customer_id],
            )?;
            Ok(())
        })
    }

    fn get_customer(&self, org: &OrgId) -> Option<String> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT customer_id FROM stripe_customers WHERE org = ?1",
                params![org.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .ok()
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn record_and_lookup_round_trip() {
        let dir = tempdir().unwrap();
        let store = SqliteBillingStore::open(dir.path().join("billing.sqlite")).unwrap();
        let acme = OrgId::new("acme");
        store.record_customer(&acme, "cus_ABC").unwrap();
        assert_eq!(store.get_customer(&acme).as_deref(), Some("cus_ABC"));
    }

    #[test]
    fn upsert_overwrites_prior_customer_id() {
        let dir = tempdir().unwrap();
        let store = SqliteBillingStore::open(dir.path().join("billing.sqlite")).unwrap();
        let acme = OrgId::new("acme");
        store.record_customer(&acme, "cus_OLD").unwrap();
        store.record_customer(&acme, "cus_NEW").unwrap();
        assert_eq!(store.get_customer(&acme).as_deref(), Some("cus_NEW"));
    }

    #[test]
    fn survives_reopening_the_same_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("billing.sqlite");
        {
            let s = SqliteBillingStore::open(&path).unwrap();
            s.record_customer(&OrgId::new("acme"), "cus_A").unwrap();
            s.record_customer(&OrgId::new("globex"), "cus_G").unwrap();
        }
        let reopened = SqliteBillingStore::open(&path).unwrap();
        assert_eq!(
            reopened.get_customer(&OrgId::new("acme")).as_deref(),
            Some("cus_A")
        );
        assert_eq!(
            reopened.get_customer(&OrgId::new("globex")).as_deref(),
            Some("cus_G")
        );
    }

    #[test]
    fn migrate_is_idempotent_across_reopens() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("billing.sqlite");
        let _ = SqliteBillingStore::open(&path).unwrap();
        let _ = SqliteBillingStore::open(&path).unwrap();
        let _ = SqliteBillingStore::open(&path).unwrap();
    }
}
