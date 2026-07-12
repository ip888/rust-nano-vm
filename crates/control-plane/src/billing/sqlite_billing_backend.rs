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

use super::{BillingStore, BillingStoreError, PendingSignup, SubscriptionState};

/// Schema version for the billing table.
///
/// - v1 → v2 adds subscription columns to `stripe_customers` so the
///   webhook handler can persist `customer.subscription.*` state.
/// - v2 → v3 adds `pending_signups` so `POST /v1/signup/request` can
///   store a magic-link token (hashed) that `POST /v1/signup/verify`
///   later consumes atomically.
/// - v3 → v4 adds `subscription_item_id` to `stripe_customers` so the
///   metered-usage reporter can POST `usage_records` per customer.
const SCHEMA_VERSION: u32 = 4;

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
        if current < 2 {
            // v1 → v2: add subscription-state columns + a lookup index
            // on customer_id for the webhook handler's reverse
            // lookups. `ADD COLUMN` in SQLite requires each column in
            // its own statement.
            tx.execute_batch(
                "ALTER TABLE stripe_customers ADD COLUMN subscription_id TEXT;
                 ALTER TABLE stripe_customers ADD COLUMN subscription_status TEXT;
                 ALTER TABLE stripe_customers ADD COLUMN price_id TEXT;
                 ALTER TABLE stripe_customers ADD COLUMN updated_at TEXT;
                 CREATE UNIQUE INDEX IF NOT EXISTS stripe_customers_by_customer_id
                     ON stripe_customers(customer_id);",
            )?;
        }
        if current < 3 {
            // v2 → v3: `pending_signups` for self-serve magic-link flow.
            // token_hash is the SHA-256 hex of the token that was mailed;
            // the raw token never enters this table.
            //
            // UNIQUE on email so re-requesting a signup for the same
            // address replaces the prior row (via ON CONFLICT below),
            // never leaves two live tokens for one address.
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_signups (
                    token_hash TEXT PRIMARY KEY,
                    email      TEXT NOT NULL UNIQUE,
                    org_name   TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL
                );
                 CREATE INDEX IF NOT EXISTS pending_signups_by_expires_at
                     ON pending_signups(expires_at);",
            )?;
        }
        if current < 4 {
            // v3 → v4: add subscription_item_id column. Existing rows
            // land as NULL — the reporter treats that as "no meter
            // configured for this customer yet" and no-ops until
            // Stripe re-sends `customer.subscription.updated` (which
            // it does on every billing-period roll).
            tx.execute_batch("ALTER TABLE stripe_customers ADD COLUMN subscription_item_id TEXT;")?;
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

    fn record_subscription(
        &self,
        customer_id: &str,
        state: &SubscriptionState,
    ) -> Result<(), BillingStoreError> {
        let updated = self.with_conn(|c| {
            c.execute(
                "UPDATE stripe_customers
                    SET subscription_id      = ?1,
                        subscription_status  = ?2,
                        price_id             = ?3,
                        updated_at           = ?4,
                        subscription_item_id = ?5
                  WHERE customer_id = ?6",
                params![
                    state.subscription_id,
                    state.status,
                    state.price_id,
                    state.updated_at,
                    state.subscription_item_id,
                    customer_id,
                ],
            )
        })?;
        if updated == 0 {
            // Webhook arrived for a customer we never persisted (signup
            // crashed after Stripe returned, or the event was replayed
            // against a fresh SQLite file). Returning Ok would silently
            // drop the state and make the caller log "recorded" for a
            // no-op — surface the miss so ops sees it in tracing +
            // metrics rather than only in a future support ticket.
            return Err(BillingStoreError::Backend(format!(
                "record_subscription: no stripe_customers row for customer_id={customer_id:?}"
            )));
        }
        Ok(())
    }

    fn get_subscription(&self, customer_id: &str) -> Option<SubscriptionState> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT subscription_id, subscription_status, price_id,
                        updated_at, subscription_item_id
                   FROM stripe_customers
                  WHERE customer_id = ?1",
                params![customer_id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
        })
        .ok()
        .flatten()
        .and_then(
            |(sid, status, price_id, updated_at, subscription_item_id)| {
                // A row exists but no webhook event has updated it yet →
                // no subscription state.
                match (sid, status, updated_at) {
                    (Some(subscription_id), Some(status), Some(updated_at)) => {
                        Some(SubscriptionState {
                            subscription_id,
                            status,
                            price_id,
                            subscription_item_id,
                            updated_at,
                        })
                    }
                    _ => None,
                }
            },
        )
    }

    fn org_by_customer(&self, customer_id: &str) -> Option<OrgId> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT org FROM stripe_customers WHERE customer_id = ?1",
                params![customer_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .ok()
        .flatten()
        .map(|s| OrgId::new(&s))
    }

    fn record_pending_signup(&self, signup: &PendingSignup) -> Result<(), BillingStoreError> {
        // Two conflict paths, both need explicit UPSERT clauses because
        // SQLite requires the target of `ON CONFLICT` to be a UNIQUE /
        // PRIMARY KEY column:
        //
        //   * `email` (UNIQUE) — re-requesting a signup for the same
        //     address replaces the prior row so we never leave two
        //     live tokens for one email.
        //   * `token_hash` (PRIMARY KEY) — a fresh mint that collides
        //     with a still-live row from a different email (extremely
        //     unlikely for a 24-byte random token, but the trait
        //     contract promises the fresh row wins).
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO pending_signups
                    (token_hash, email, org_name, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(email) DO UPDATE SET
                    token_hash = excluded.token_hash,
                    org_name   = excluded.org_name,
                    created_at = excluded.created_at,
                    expires_at = excluded.expires_at
                 ON CONFLICT(token_hash) DO UPDATE SET
                    email      = excluded.email,
                    org_name   = excluded.org_name,
                    created_at = excluded.created_at,
                    expires_at = excluded.expires_at",
                params![
                    signup.token_hash,
                    signup.email,
                    signup.org_name,
                    signup.created_at,
                    signup.expires_at,
                ],
            )?;
            Ok(())
        })
    }

    fn take_pending_signup(&self, token_hash: &str, now: &str) -> Option<PendingSignup> {
        let mut guard = self.conn.lock().ok()?;
        let tx = guard.transaction().ok()?;
        // The single-redeem guarantee comes from the `Mutex<Connection>`
        // we hold across the whole SELECT + DELETE, NOT from SQLite's
        // transaction isolation — `rusqlite::Connection::transaction()`
        // starts a DEFERRED transaction by default, which does not
        // acquire a write lock until the DELETE. If a future refactor
        // moved this to a connection pool (multiple connections) the
        // race would reopen; either use `transaction_with_behavior`
        // (Immediate) or keep the single-connection-with-mutex shape.
        //
        // WHERE guards on `expires_at` so an expired row is treated as
        // absent even if GC hasn't run yet.
        let row = tx
            .query_row(
                "SELECT email, org_name, created_at, expires_at
                   FROM pending_signups
                  WHERE token_hash = ?1 AND expires_at >= ?2",
                params![token_hash, now],
                |r| {
                    Ok(PendingSignup {
                        token_hash: token_hash.to_owned(),
                        email: r.get::<_, String>(0)?,
                        org_name: r.get::<_, String>(1)?,
                        created_at: r.get::<_, String>(2)?,
                        expires_at: r.get::<_, String>(3)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()?;
        tx.execute(
            "DELETE FROM pending_signups WHERE token_hash = ?1",
            params![token_hash],
        )
        .ok()?;
        tx.commit().ok()?;
        Some(row)
    }

    fn gc_expired_signups(&self, now: &str) -> Result<u64, BillingStoreError> {
        let deleted = self.with_conn(|c| {
            c.execute(
                "DELETE FROM pending_signups WHERE expires_at < ?1",
                params![now],
            )
        })?;
        Ok(deleted as u64)
    }

    fn peek_pending_signup(&self, token_hash: &str, now: &str) -> Option<PendingSignup> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT email, org_name, created_at, expires_at
                   FROM pending_signups
                  WHERE token_hash = ?1 AND expires_at >= ?2",
                params![token_hash, now],
                |r| {
                    Ok(PendingSignup {
                        token_hash: token_hash.to_owned(),
                        email: r.get::<_, String>(0)?,
                        org_name: r.get::<_, String>(1)?,
                        created_at: r.get::<_, String>(2)?,
                        expires_at: r.get::<_, String>(3)?,
                    })
                },
            )
            .optional()
        })
        .ok()
        .flatten()
    }

    fn delete_pending_signup(&self, token_hash: &str) -> Result<(), BillingStoreError> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM pending_signups WHERE token_hash = ?1",
                params![token_hash],
            )?;
            Ok(())
        })
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

    #[test]
    fn subscription_round_trips_through_sqlite() {
        let dir = tempdir().unwrap();
        let store = SqliteBillingStore::open(dir.path().join("b.sqlite")).unwrap();
        store
            .record_customer(&OrgId::new("acme"), "cus_ACME")
            .unwrap();
        let sub = SubscriptionState {
            subscription_id: "sub_A".into(),
            status: "active".into(),
            price_id: Some("price_PRO".into()),
            subscription_item_id: None,
            updated_at: "2026-07-10T00:00:00Z".into(),
        };
        store.record_subscription("cus_ACME", &sub).unwrap();
        assert_eq!(store.get_subscription("cus_ACME"), Some(sub));
        assert_eq!(store.org_by_customer("cus_ACME"), Some(OrgId::new("acme")));
    }

    #[test]
    fn no_subscription_yet_returns_none() {
        let dir = tempdir().unwrap();
        let store = SqliteBillingStore::open(dir.path().join("b.sqlite")).unwrap();
        store
            .record_customer(&OrgId::new("acme"), "cus_ACME")
            .unwrap();
        assert!(store.get_subscription("cus_ACME").is_none());
    }

    #[test]
    fn pending_signup_round_trips_through_sqlite() {
        let dir = tempdir().unwrap();
        let store = SqliteBillingStore::open(dir.path().join("b.sqlite")).unwrap();
        let signup = PendingSignup {
            token_hash: "abc123".into(),
            email: "a@example.com".into(),
            org_name: "Acme".into(),
            created_at: "2026-07-10T00:00:00.000Z".into(),
            expires_at: "2999-01-01T00:00:00.000Z".into(),
        };
        store.record_pending_signup(&signup).unwrap();
        let taken = store
            .take_pending_signup(&signup.token_hash, "2026-07-11T00:00:00.000Z")
            .expect("row exists and not yet expired");
        assert_eq!(taken.email, "a@example.com");
        // Second take → consumed.
        assert!(store
            .take_pending_signup(&signup.token_hash, "2026-07-11T00:00:00.000Z")
            .is_none());
    }

    #[test]
    fn pending_signup_expiry_short_circuits_take() {
        let dir = tempdir().unwrap();
        let store = SqliteBillingStore::open(dir.path().join("b.sqlite")).unwrap();
        let signup = PendingSignup {
            token_hash: "abc123".into(),
            email: "a@example.com".into(),
            org_name: "Acme".into(),
            created_at: "2020-01-01T00:00:00.000Z".into(),
            expires_at: "2020-01-01T00:00:01.000Z".into(),
        };
        store.record_pending_signup(&signup).unwrap();
        // `now` past expiry: take returns None; row is NOT deleted
        // by take (leave that to GC).
        assert!(store
            .take_pending_signup(&signup.token_hash, "2020-01-01T00:00:02.000Z")
            .is_none());
        // GC picks it up.
        assert_eq!(
            store
                .gc_expired_signups("2020-01-01T00:00:02.000Z")
                .unwrap(),
            1
        );
    }

    #[test]
    fn re_request_replaces_prior_token_for_same_email_via_upsert() {
        let dir = tempdir().unwrap();
        let store = SqliteBillingStore::open(dir.path().join("b.sqlite")).unwrap();
        let a = PendingSignup {
            token_hash: "hash-a".into(),
            email: "same@example.com".into(),
            org_name: "Acme".into(),
            created_at: "2026-07-10T00:00:00.000Z".into(),
            expires_at: "2999-01-01T00:00:00.000Z".into(),
        };
        let b = PendingSignup {
            token_hash: "hash-b".into(),
            email: "same@example.com".into(),
            org_name: "Acme".into(),
            created_at: "2026-07-10T00:00:01.000Z".into(),
            expires_at: "2999-01-01T00:00:00.000Z".into(),
        };
        store.record_pending_signup(&a).unwrap();
        store.record_pending_signup(&b).unwrap();
        // Old token invalid — the UNIQUE(email) upsert replaced the row.
        assert!(store
            .take_pending_signup("hash-a", "2026-07-11T00:00:00.000Z")
            .is_none());
        // New token good.
        assert!(store
            .take_pending_signup("hash-b", "2026-07-11T00:00:00.000Z")
            .is_some());
    }

    #[test]
    fn subscription_survives_reopening_the_same_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("b.sqlite");
        {
            let s = SqliteBillingStore::open(&path).unwrap();
            s.record_customer(&OrgId::new("acme"), "cus_ACME").unwrap();
            s.record_subscription(
                "cus_ACME",
                &SubscriptionState {
                    subscription_id: "sub_A".into(),
                    status: "active".into(),
                    price_id: Some("price_PRO".into()),
                    subscription_item_id: None,
                    updated_at: "2026-07-10T00:00:00Z".into(),
                },
            )
            .unwrap();
        }
        let reopened = SqliteBillingStore::open(&path).unwrap();
        let sub = reopened.get_subscription("cus_ACME").unwrap();
        assert_eq!(sub.status, "active");
        assert_eq!(sub.subscription_id, "sub_A");
        assert_eq!(sub.price_id.as_deref(), Some("price_PRO"));
    }
}
