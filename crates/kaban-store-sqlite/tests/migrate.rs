//! The one schema (spec 05 OWN-P; formerly the forward-only migrations of core-spec §7): a
//! fresh database is created whole at `LATEST_SCHEMA_VERSION` with no backup; a database at
//! any other version, older, newer or an unversioned spike file, is refused at the open with
//! a typed error and the way out, and nothing in it is touched; the tenant-scoped tables of
//! the created schema carry `workspace_id NOT NULL` (F17).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{ActorId, StoreError};
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use std::path::{Path, PathBuf};

/// Every `kaban.db.bak-*` file in the scratch directory (the former migrator's backups).
fn backups(s: &Scratch) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(&s.0)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("kaban.db.bak-"))
        })
        .collect();
    v.sort();
    v
}

fn schema_version(db: &Path) -> Option<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// What a refused open must leave as it found it: every object of `sqlite_master` with its
/// DDL, and every row of `node` and of `index_meta` (when the table exists). The file header
/// itself is not compared: the connection settings put a file in WAL mode before the schema
/// is read, the one change an open makes to a file it then refuses.
type Content = (Vec<(String, String, Option<String>)>, Vec<String>);

fn content(db: &Path) -> Content {
    let conn = rusqlite::Connection::open(db).unwrap();
    let objects = conn
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut rows = Vec::new();
    for table in ["node", "index_meta"] {
        let Ok(mut st) = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1")) else {
            continue;
        };
        let n = st.column_count();
        let mut it = st.query([]).unwrap();
        while let Some(r) = it.next().unwrap() {
            rows.push(
                (0..n)
                    .map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
    }
    (objects, rows)
}

#[test]
fn fresh_database_opens_at_the_latest_schema_version_without_a_backup() {
    let s = Scratch::new("kaban-migrate", "fresh");
    {
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        let report = store.migration();
        assert_eq!((report.from, report.to), (0, LATEST_SCHEMA_VERSION));
    }
    assert_eq!(
        schema_version(&s.db()).as_deref(),
        Some(LATEST_SCHEMA_VERSION.to_string().as_str()),
        "a fresh database records the one version"
    );
    assert!(
        backups(&s).is_empty(),
        "nothing to back up on a fresh database: {:?}",
        backups(&s)
    );
    let reopened = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(
        *reopened.migration(),
        kaban_store_sqlite::MigrationReport::unchanged(LATEST_SCHEMA_VERSION)
    );
}

/// OWN-P: a database at another schema version, older (the last ladder release) or newer, is
/// refused by every open that reaches the schema, with the way out named, and the file is
/// left byte for byte as it was: no migration, no backup, no re-encode.
#[test]
fn a_database_at_another_schema_version_is_refused_untouched_with_guidance() {
    for found in [LATEST_SCHEMA_VERSION - 1, 999] {
        let s = Scratch::new("kaban-migrate", &format!("version-{found}"));
        {
            let _store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        }
        let raw = rusqlite::Connection::open(s.db()).unwrap();
        raw.execute(
            "UPDATE index_meta SET value = ?1 WHERE key = 'schema_version'",
            [found.to_string()],
        )
        .unwrap();
        drop(raw);
        let before = content(&s.db());

        let actor = ActorId::default();
        let refusals = [
            ("open", SqliteStore::open(&s.db(), &s.keys()).map(drop)),
            (
                "open_existing_key",
                SqliteStore::open_existing_key(&s.db(), &s.keys(), &actor).map(drop),
            ),
            (
                "open_diagnostic",
                SqliteStore::open_diagnostic(&s.db(), &s.keys(), &actor).map(drop),
            ),
            (
                "open_for_hook",
                SqliteStore::open_for_hook(&s.db(), &s.keys(), &actor).map(drop),
            ),
        ];
        for (constructor, outcome) in refusals {
            let err = outcome.expect_err(constructor);
            assert!(
                matches!(
                    err,
                    StoreError::SchemaUnsupported { found: f, supported }
                        if f == found && supported == LATEST_SCHEMA_VERSION
                ),
                "{constructor} at {found}: {err:?}"
            );
            let msg = err.to_string();
            assert!(
                msg.contains(&found.to_string())
                    && msg.contains(&LATEST_SCHEMA_VERSION.to_string())
                    && msg.contains("re-ingest")
                    && msg.contains("new database"),
                "{constructor}: the refusal names both versions and the way out: {msg}"
            );
        }
        assert_eq!(
            schema_version(&s.db()).as_deref(),
            Some(found.to_string().as_str()),
            "the version record is not touched"
        );
        assert_eq!(
            content(&s.db()),
            before,
            "version {found}: every object and row is as it was"
        );
        assert!(backups(&s).is_empty(), "no backup of a refused database");
    }
}

/// An unversioned spike-era file (the `node` table, no `index_meta`) is refused as version 1
/// and nothing is created in it.
#[test]
fn an_unversioned_spike_file_is_refused_and_left_alone() {
    let s = Scratch::new("kaban-migrate", "spike");
    {
        let conn = rusqlite::Connection::open(s.db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE node(rid INTEGER PRIMARY KEY, id TEXT NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL);
             INSERT INTO node(id, kind, text) VALUES ('n1', 'Fact', 'spike era');",
        )
        .unwrap();
    }
    let before = content(&s.db());
    let err = SqliteStore::open(&s.db(), &s.keys()).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::SchemaUnsupported {
                found: 1,
                supported: LATEST_SCHEMA_VERSION
            }
        ),
        "{err:?}"
    );
    assert_eq!(content(&s.db()), before, "nothing created, the row intact");
    assert!(backups(&s).is_empty(), "no backup: {:?}", backups(&s));
}

/// Tables whose `workspace_id` column is `NOT NULL`, and tables that have no such column.
fn workspace_columns(conn: &rusqlite::Connection) -> (Vec<String>, Vec<String>) {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = st
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut not_null = Vec::new();
    let mut absent = Vec::new();
    for t in tables {
        let mut cols = conn.prepare(&format!("PRAGMA table_info({t})")).unwrap();
        // `(notnull, pk)`: a `workspace_id` that is the table's PRIMARY KEY
        // (`discovered_overflow`, engine-spec §3.2 verbatim) is the tenant key by construction
        let ws: Option<(i64, i64)> = cols
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .find(|(name, _, _)| name == "workspace_id")
            .map(|(_, notnull, pk)| (notnull, pk));
        match ws {
            Some((1, _) | (_, 1..)) => not_null.push(t),
            Some(_) => panic!("{t}.workspace_id is nullable"),
            None => absent.push(t),
        }
    }
    (not_null, absent)
}

#[test]
fn workspace_id_is_not_null_on_exactly_the_tenant_scoped_tables() {
    let s = Scratch::new("kaban-migrate", "f17");
    {
        let _store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    }
    let c = rusqlite::Connection::open(s.db()).unwrap();
    let (not_null, absent) = workspace_columns(&c);
    assert_eq!(
        not_null,
        [
            "assertion",
            "audit",
            "changes",
            "derived_artifact",
            "discovered_overflow",
            "edge",
            "events",
            "ingest_jobs",
            "kind_stats",
            "node",
            "node_anchor",
            "node_candidate",
            "pruning_protection"
        ],
        "F17: tenant-scoped tables (engine-spec §3.2)"
    );
    assert_eq!(
        absent,
        [
            "assertion_status",
            "confirmations",
            "conflicts",
            "index_meta",
            "leases",
            "node_identity",
            "projections",
            "read_audit_seen",
            "schema_type",
            "session_edits"
        ],
        "F17 (PR-6): DB-scoped bookkeeping tables and per-row child tables carry no workspace column"
    );
}
