//! The one schema and its creation (spec 05 OWN-P; formerly the forward-only migrator of
//! core-spec §7).
//!
//! kaban serves exactly one schema version, [`LATEST_SCHEMA_VERSION`]: a fresh database gets
//! the whole DDL of `schema.sql` (embedded with `include_str!`) in one transaction, with the
//! version record and the body codec record beside it; a database already at that version is
//! served as it is; a database at any other version, older or newer, is refused at open with
//! [`StoreError::SchemaUnsupported`] and the way out (export with the kaban that wrote it and
//! re-ingest, or start a new database). No backup, no upgrade step, no exclusive admission
//! for a migration: a breaking schema change is a breaking release. The schema version is
//! `index_meta(key='schema_version')`; a database with the `node` table and no such key is an
//! unversioned spike file, refused like any other version. Connection PRAGMAs are not part of
//! the schema: they are applied per connection by [`configure_connection`] (MD-2).
//!
//! [`Migrator`] is kept as the thin "create at the latest, refuse the rest" step so a future
//! schema change has a place to live.

use crate::sql_error::{SqlResultExt, backend_error};
use kaban_core::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension};
use std::time::{Duration, Instant};

/// The whole schema at [`LATEST_SCHEMA_VERSION`], created on a fresh database.
const SCHEMA: &str = include_str!("../schema.sql");

/// Per-connection settings applied after WAL is established (not DDL, not versioned): durability
/// level, foreign keys and secure delete (core-spec S5).
const CONNECTION_PRAGMAS: &str = "PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA secure_delete = FAST;";

/// How long a connection waits for a lock (core-spec N4), both as SQLite's busy handler and as
/// the deadline of [`ensure_wal`].
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Apply the per-connection settings on every open, before anything else: the busy timeout,
/// then WAL (with the retry SQLite omits, see [`ensure_wal`]), then [`CONNECTION_PRAGMAS`].
///
/// # Errors
/// SQLite errors; `SQLITE_BUSY` only after [`BUSY_TIMEOUT`] of retrying the WAL conversion.
pub(crate) fn configure_connection(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT).sql()?;
    ensure_wal(conn)?;
    conn.execute_batch(CONNECTION_PRAGMAS).sql()?;
    crate::body_codec::register(conn)?;
    // asserted, not assumed (engine-spec EN-F8a): the cascades of 0004 depend on it
    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .sql()?;
    if foreign_keys != 1 {
        return Err(StoreError::Backend {
            source: kaban_core::BackendSource::new(ForeignKeysOff),
        });
    }
    Ok(())
}

/// `PRAGMA foreign_keys` did not take effect on a connection (a build without foreign-key
/// support, or a connection that was already mid-transaction): the store refuses to run
/// without the cascades migration 0004 relies on.
#[derive(Debug, Clone, Copy)]
struct ForeignKeysOff;

impl std::fmt::Display for ForeignKeysOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PRAGMA foreign_keys is not ON after open")
    }
}

impl std::error::Error for ForeignKeysOff {}

/// Put the database in WAL mode (core-spec F15). `PRAGMA journal_mode = WAL` opens a read
/// transaction and upgrades it to a write transaction (`sqlite3BtreeSetVersion`); on that
/// read-to-write upgrade SQLite returns plain `SQLITE_BUSY` without invoking the busy handler
/// (deadlock avoidance), so two processes converting, or one converting while another holds a
/// rollback-journal RESERVED lock, fail at once. This retries exactly that case with a short
/// backoff until [`BUSY_TIMEOUT`] has elapsed (M2 review R2-5, peer review
/// `reviews/m2-flake-busy-open-review-codex.md`). Every other error returns immediately.
///
/// # Errors
/// SQLite errors; `SQLITE_BUSY` once the deadline has passed.
pub(crate) fn ensure_wal(conn: &Connection) -> Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    let mut backoff = Duration::from_millis(2);
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
            // in-memory databases report `memory`: there is no WAL to establish
            Ok(_) => return Ok(()),
            Err(e) if is_busy(&e) && Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            Err(e) => return Err(backend_error(e)),
        }
    }
}

/// Whether `e` is `SQLITE_BUSY` (any extended form).
pub(crate) fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                ..
            },
            _
        )
    )
}

/// The one schema version this binary serves: created on a fresh database, required of an
/// existing one (spec 05 OWN-P). The number continues the former ladder (13: the Tantivy
/// index beside the database is the only text index and every body is encoded, OWN-M) so a
/// database of the last ladder release is told apart from a spike file by its version.
pub const LATEST_SCHEMA_VERSION: u32 = 13;

/// The `index_meta.schema_version` as recorded, 0 when the key is absent (a fresh or spike
/// database): the cheap read of code that behaves by version on a live connection.
pub(crate) fn recorded_version(conn: &Connection) -> Result<u32> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Result of [`Migrator::ensure_current`]: the version found and the version the database is
/// at afterwards (`from` 0 and `to` [`LATEST_SCHEMA_VERSION`] when this open created the
/// schema; equal otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MigrationReport {
    /// Schema version detected before the open (0 = fresh database).
    pub from: u32,
    /// Schema version after the open (always [`LATEST_SCHEMA_VERSION`] on success).
    pub to: u32,
}

impl MigrationReport {
    /// The report of an open that found the database at `version` and left it there
    /// (verification, diagnostic and hook opens).
    #[must_use]
    pub const fn unchanged(version: u32) -> Self {
        Self {
            from: version,
            to: version,
        }
    }
}

/// Creates the schema on a fresh database and refuses every other version (module doc).
#[derive(Debug, Clone, Copy)]
pub struct Migrator;

impl Migrator {
    /// Detect the schema version of the database behind `conn`: `None` when the key is
    /// absent but the spike `node` table exists (an unversioned spike file), `Some(0)` for a
    /// fresh database, `Some(v)` when `index_meta.schema_version` is recorded.
    fn detect(conn: &Connection) -> Result<Option<u32>> {
        let has = |name: &str| -> Result<bool> {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                    [name],
                    |_| Ok(()),
                )
                .optional()
                .sql()?
                .is_some())
        };
        if !has("node")? {
            return Ok(Some(0));
        }
        if !has("index_meta")? {
            return Ok(None);
        }
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .sql()?;
        match v {
            None => Ok(None),
            Some(s) => s
                .parse::<u32>()
                .map(Some)
                .map_err(|e| StoreError::Migration {
                    from: 0,
                    to: 0,
                    source: Box::new(e),
                }),
        }
    }

    /// The refusal of a database at `found` (an unversioned spike file is reported as 1).
    const fn unsupported(found: Option<u32>) -> StoreError {
        StoreError::SchemaUnsupported {
            found: match found {
                Some(v) => v,
                None => 1,
            },
            supported: LATEST_SCHEMA_VERSION,
        }
    }

    /// Read the schema version of an existing database without creating anything: the one
    /// version this binary serves, or the refusal. Used by the opens that must not mutate
    /// the database (`kaban verify-audit`, MD-28; `doctor`; the hook; a pack source).
    ///
    /// # Errors
    /// `StoreError::SchemaUnsupported` for any version but [`LATEST_SCHEMA_VERSION`] (a
    /// fresh, empty database included: those opens create nothing); SQLite errors.
    pub fn check(conn: &Connection) -> Result<u32> {
        match Self::detect(conn)? {
            Some(LATEST_SCHEMA_VERSION) => Ok(LATEST_SCHEMA_VERSION),
            other => Err(Self::unsupported(other)),
        }
    }

    /// Whether a database at `version` is current for this binary: at
    /// [`LATEST_SCHEMA_VERSION`], the one version every open serves.
    #[must_use]
    pub const fn is_current(version: u32) -> bool {
        version == LATEST_SCHEMA_VERSION
    }

    /// Bring the database to the one schema: create it whole on a fresh database (version
    /// 0), under one `BEGIN IMMEDIATE` transaction so of any number of concurrent first
    /// openers exactly one creates it and the others find it; serve a database already at
    /// [`LATEST_SCHEMA_VERSION`] as it is; refuse any other version untouched.
    ///
    /// # Errors
    /// `StoreError::SchemaUnsupported` for a database at another version; SQLite errors.
    pub(crate) fn ensure_current(conn: &mut Connection) -> Result<MigrationReport> {
        // a cheap read first: an existing database never takes the write lock
        match Self::detect(conn)? {
            Some(LATEST_SCHEMA_VERSION) => {
                return Ok(MigrationReport::unchanged(LATEST_SCHEMA_VERSION));
            }
            Some(0) => {}
            other => return Err(Self::unsupported(other)),
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        // re-detect under the lock: another first opener may have created it meanwhile
        match Self::detect(&tx)? {
            Some(LATEST_SCHEMA_VERSION) => {
                return Ok(MigrationReport::unchanged(LATEST_SCHEMA_VERSION));
            }
            Some(0) => {}
            other => return Err(Self::unsupported(other)),
        }
        tx.execute_batch(SCHEMA).sql()?;
        tx.execute(
            "INSERT INTO index_meta(key, value) VALUES ('schema_version', ?1)",
            [LATEST_SCHEMA_VERSION.to_string()],
        )
        .sql()?;
        crate::body_codec::record(&tx)?;
        tx.commit().sql()?;
        Ok(MigrationReport {
            from: 0,
            to: LATEST_SCHEMA_VERSION,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// A fresh database gets the whole schema and the version record in one step; a second
    /// pass finds it current and changes nothing.
    #[test]
    fn a_fresh_database_is_created_whole_and_then_served_as_it_is() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        let report = Migrator::ensure_current(&mut conn).unwrap();
        assert_eq!((report.from, report.to), (0, LATEST_SCHEMA_VERSION));
        for table in ["node", "edge", "changes", "node_anchor", "index_meta"] {
            assert!(table_exists(&conn, table), "{table}");
        }
        assert!(!table_exists(&conn, "node_fts"), "no FTS5 object");
        assert_eq!(recorded_version(&conn).unwrap(), LATEST_SCHEMA_VERSION);
        assert_eq!(
            Migrator::ensure_current(&mut conn).unwrap(),
            MigrationReport::unchanged(LATEST_SCHEMA_VERSION)
        );
        assert_eq!(Migrator::check(&conn).unwrap(), LATEST_SCHEMA_VERSION);
    }

    /// OWN-P: a database at another version, older or newer, and an unversioned spike file
    /// are refused by name with the way out, and nothing in them is touched.
    #[test]
    fn a_database_at_another_version_is_refused_untouched_with_guidance() {
        for found in [LATEST_SCHEMA_VERSION - 1, LATEST_SCHEMA_VERSION + 1] {
            let mut conn = Connection::open_in_memory().unwrap();
            configure_connection(&conn).unwrap();
            Migrator::ensure_current(&mut conn).unwrap();
            conn.execute(
                "UPDATE index_meta SET value = ?1 WHERE key = 'schema_version'",
                [found.to_string()],
            )
            .unwrap();
            let err = Migrator::ensure_current(&mut conn).unwrap_err();
            assert!(
                matches!(err, StoreError::SchemaUnsupported { found: f, supported } if f == found && supported == LATEST_SCHEMA_VERSION),
                "{err:?}"
            );
            let text = err.to_string();
            assert!(
                text.contains(&found.to_string()) && text.contains("re-ingest"),
                "{text}"
            );
            assert_eq!(recorded_version(&conn).unwrap(), found, "untouched");
            assert!(matches!(
                Migrator::check(&conn),
                Err(StoreError::SchemaUnsupported { .. })
            ));
        }
        let mut spike = Connection::open_in_memory().unwrap();
        configure_connection(&spike).unwrap();
        spike
            .execute_batch("CREATE TABLE node(rid INTEGER PRIMARY KEY, text TEXT);")
            .unwrap();
        let err = Migrator::ensure_current(&mut spike).unwrap_err();
        assert!(
            matches!(err, StoreError::SchemaUnsupported { found: 1, .. }),
            "{err:?}"
        );
        assert!(!table_exists(&spike, "index_meta"), "nothing created");
    }
}
