//! The buyer's durable application state: `$MOBEE_HOME/buyer.sqlite`.
//!
//! Opened only by the daemon (guaranteed single-owner by the home lock). This is
//! the state home the later phases build on — the reservation ledger, payment
//! attempts, and lifecycle tables all land here. Step 1 ships the minimal shell:
//! a `buyer_meta` schema-version row and a `jobs` stub table, in WAL mode with
//! foreign keys and `synchronous=FULL` so the money-adjacent state that follows
//! inherits crash-safe defaults from day one.
//!
//! `rusqlite`'s [`Connection`] is `Send` but not `Sync`; the store keeps it behind
//! a mutex and callers reach it from the async runtime via `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// Current on-disk schema version. Bumped when a later phase migrates the schema.
pub const SCHEMA_VERSION: i64 = 1;

/// A cloneable handle to the daemon-owned SQLite state.
#[derive(Clone)]
pub struct BuyerStore {
    conn: Arc<Mutex<Connection>>,
}

/// Store open / query failure.
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "buyer store error: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self(value.to_string())
    }
}

/// A point-in-time view of the store for `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub schema_version: i64,
    pub started_at_unix: i64,
    pub jobs: i64,
}

impl BuyerStore {
    /// Open (creating if absent) the state DB at `path` with WAL + crash-safe pragmas
    /// and ensure the schema is present.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path.as_ref())?;
        // WAL for concurrent reads alongside the single writer; FULL sync + FK
        // enforcement because this DB will hold money-adjacent ledger state. A
        // bounded busy timeout avoids an immediate SQLITE_BUSY under contention.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS buyer_meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- Lifecycle table stub. Later phases add reservation/attempt columns
             -- and the state machine; step 1 only proves the DB is the daemon's.
             CREATE TABLE IF NOT EXISTS jobs (
                 job_id          TEXT PRIMARY KEY,
                 status          TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL
             );",
        )?;
        conn.execute(
            "INSERT INTO buyer_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO NOTHING",
            [SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Record (idempotently overwrite) the daemon's most recent start time.
    pub fn record_start(&self, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO buyer_meta (key, value) VALUES ('started_at_unix', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now_unix.to_string()],
        )?;
        Ok(())
    }

    /// Read the current health view for `status`.
    pub fn health(&self) -> Result<HealthSnapshot, StoreError> {
        let conn = self.lock()?;
        let schema_version = read_meta_i64(&conn, "schema_version")?.unwrap_or(0);
        let started_at_unix = read_meta_i64(&conn, "started_at_unix")?.unwrap_or(0);
        let jobs = conn.query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get::<_, i64>(0))?;
        Ok(HealthSnapshot {
            schema_version,
            started_at_unix,
            jobs,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError("state DB mutex poisoned".into()))
    }
}

fn read_meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>, StoreError> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM buyer_meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok();
    match value {
        Some(text) => text
            .parse::<i64>()
            .map(Some)
            .map_err(|error| StoreError(format!("buyer_meta.{key} not an integer: {error}"))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-buyer-store-{label}-{}-{id}.sqlite", std::process::id()))
    }

    #[test]
    fn open_is_wal_and_carries_schema_and_start() {
        let path = temp_db("wal");
        let _ = std::fs::remove_file(&path);
        let store = BuyerStore::open(&path).expect("open");
        store.record_start(1234).expect("record start");

        let health = store.health().expect("health");
        assert_eq!(health.schema_version, SCHEMA_VERSION);
        assert_eq!(health.started_at_unix, 1234);
        assert_eq!(health.jobs, 0);

        // WAL mode leaves a -wal sidecar once written.
        let conn = Connection::open(&path).expect("reopen");
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");

        let _ = std::fs::remove_file(&path);
    }
}
