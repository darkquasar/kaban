//! The one schema, v14 (spec 05 OWN-P; spec 11 CMP-01, KER-05, PC-1): an empty directory
//! opens as a v14 project whose version is in the main file as soon as it is created, in
//! incremental auto-vacuum mode, with a `node.rid` that is never handed out twice; a database
//! at any other version, older, newer or an unversioned spike file, is refused at the open
//! with a typed error and the way out, and nothing in it is touched (there is no migration);
//! the tenant-scoped tables of the created schema carry `workspace_id NOT NULL` (F17).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{ActorId, ForgetMode, MemoryStore, StoreError};
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, MigrationReport, SqliteStore};
use std::path::Path;

/// A read of the main database file alone, as the open's refusal probe reads it: an immutable
/// read-only connection takes no lock, creates no `-wal` or `-shm` and never sees the log.
fn main_file(db: &Path) -> rusqlite::Connection {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
    rusqlite::Connection::open_with_flags(
        format!("file:{}?mode=ro&immutable=1", db.display()),
        flags,
    )
    .unwrap()
}

/// `index_meta.schema_version` as the main file records it.
fn main_file_version(db: &Path) -> Option<String> {
    main_file(db)
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .ok()
}

/// `PRAGMA auto_vacuum` as the main file's header records it (2 = incremental).
fn main_file_auto_vacuum(db: &Path) -> i64 {
    main_file(db)
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap()
}

/// The schema version as an ordinary connection reads it (main file and log).
fn schema_version(db: &Path) -> Option<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// The `rid` of the row with node id `id`.
fn rid_of(db: &Path, id: &str) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT rid FROM node WHERE id = ?1", [id], |r| r.get(0))
        .unwrap()
}

/// What a refused open must leave as it found it: every object of `sqlite_master` with its
/// DDL, and every row of `node` and of `index_meta` (when the table exists).
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

/// CMP-01: an empty directory opens as a v14 project, and the version is in the main file
/// while the creating store is still open (the open ends with a TRUNCATE checkpoint), so a
/// second opener's probe, which reads the main file alone, always sees it.
#[test]
fn an_empty_directory_opens_as_a_v14_project_recorded_in_the_main_file() {
    let s = Scratch::new("kaban-migrate", "fresh");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(LATEST_SCHEMA_VERSION, 14);
    assert_eq!(
        (store.migration().from, store.migration().to),
        (0, 14),
        "created at v14 from nothing"
    );
    assert_eq!(
        main_file_version(&s.db()).as_deref(),
        Some("14"),
        "the main file holds the version right after creation"
    );
    drop(store);
    let reopened = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(*reopened.migration(), MigrationReport::unchanged(14));
}

/// KER-05: a fresh project is created in incremental auto-vacuum mode, so an ordinary compact
/// can release wholly free pages, and the mode survives a close and a reopen.
#[test]
fn a_fresh_project_reports_incremental_auto_vacuum_before_and_after_reopen() {
    let s = Scratch::new("kaban-migrate", "auto-vacuum");
    {
        let _store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        assert_eq!(main_file_auto_vacuum(&s.db()), 2, "fresh: incremental");
    }
    let _reopened = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(main_file_auto_vacuum(&s.db()), 2, "reopened: incremental");
}

/// PC-1: `node.rid` is the graph's local id, so the rid of a purged highest row is never
/// handed to a later insert. A purge leaves the row as a ghost, which alone keeps its rid
/// taken; the ghost row is then removed outright, so only the schema (AUTOINCREMENT) stands
/// between the next insert and the old rid.
#[test]
fn a_purged_highest_rid_is_never_reused() {
    let s = Scratch::new("kaban-migrate", "rid");
    let purged = {
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        created(&store, "Fact", "the first row");
        let highest = created(&store, "Fact", "the highest row, purged");
        store
            .forget(&nid(&highest.id), ForgetMode::Purge, &ctx())
            .unwrap();
        let after_ghost = created(&store, "Fact", "inserted beside the ghost");
        drop(store);
        let purged = rid_of(&s.db(), &highest.id);
        assert!(rid_of(&s.db(), &after_ghost.id) > purged);
        purged
    };
    {
        let raw = rusqlite::Connection::open(s.db()).unwrap();
        // the change-log triggers stamp every node write with the connection's writer
        raw.create_scalar_function(
            "kaban_writer",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| Ok(ActorId::default().as_str().to_owned()),
        )
        .unwrap();
        // the later row first, so the purged row is the table's highest when it leaves
        raw.execute("DELETE FROM node WHERE rid > ?1", [purged])
            .unwrap();
        raw.execute("DELETE FROM node WHERE rid = ?1", [purged])
            .unwrap();
    }
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let next = created(&store, "Fact", "inserted after the purged row left");
    drop(store);
    let next = rid_of(&s.db(), &next.id);
    assert!(next > purged, "rid {purged} reused as {next}");
}

/// OWN-P, CMP-01: a database at another schema version, older (the last ladder release) or
/// newer, is refused by every open that reaches the schema, with both versions and the way
/// out named, and every object and row of the file is left as it was.
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
                    && msg.contains("no upgrade path exists; create a fresh project and re-ingest"),
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
        // `(notnull, pk)`: a `workspace_id` that is the table's PRIMARY KEY is the tenant key
        // by construction
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

/// F17 over the v14 table set (`schema.sql`): every table but SQLite's own, listed exactly.
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
            "changes",
            "derived_artifact",
            "edge",
            "ingest_jobs",
            "kind_stats",
            "node",
            "node_anchor"
        ],
        "F17: tenant-scoped tables (engine-spec §3.2)"
    );
    assert_eq!(
        absent,
        [
            "cat_claim",
            "cat_extent",
            "cat_generation",
            "cat_meta",
            "cat_sediment",
            "cat_view",
            "cat_view_extent",
            "index_meta",
            "leases",
            "projections",
            "rollup",
            "rollup_input",
            "schema_type",
            "session_edits",
            "writer"
        ],
        "F17 (PR-6): DB-scoped bookkeeping tables carry no workspace column"
    );
}

/// Seed the node row-id sequence so the next insert gets `next`.
fn seed_next_rid(db: &Path, next: i64) {
    let raw = rusqlite::Connection::open(db).unwrap();
    let updated = raw
        .execute(
            "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'node'",
            [next - 1],
        )
        .unwrap();
    assert_eq!(updated, 1, "the node table has a sequence row");
}

/// The node row-id sequence and the number of node rows.
fn sequence_and_rows(db: &Path) -> (i64, i64) {
    let raw = rusqlite::Connection::open(db).unwrap();
    let seq = raw
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'node'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let rows = raw
        .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
        .unwrap();
    (seq, rows)
}

/// A store whose next node insert gets row id `u32::MAX`, the last one the graph can address.
fn at_the_ceiling(s: &Scratch) {
    {
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        created(&store, "Fact", "the row that starts the sequence");
    }
    seed_next_rid(&s.db(), i64::from(u32::MAX));
}

fn note(text: &str) -> kaban_core::NewNode {
    kaban_core::NewNode {
        kind: "Fact".into(),
        text: text.into(),
        ..Default::default()
    }
}

/// PC-1: the last addressable row id is handed out; the next `remember` is refused by name
/// and rolls back, leaving the sequence and every row as they were.
#[test]
fn a_remember_past_the_local_id_ceiling_is_refused_and_rolled_back() {
    let s = Scratch::new("kaban-migrate", "ceiling-remember");
    at_the_ceiling(&s);
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let last = created(&store, "Fact", "the last addressable row");
    assert_eq!(rid_of(&s.db(), &last.id), i64::from(u32::MAX));
    let before = sequence_and_rows(&s.db());

    let err = store
        .remember(note("one row too many"), &[], &ctx())
        .unwrap_err();

    assert!(
        matches!(err, StoreError::NodeIdCeiling { max } if max == u32::MAX),
        "{err:?}"
    );
    assert!(err.to_string().contains(&u32::MAX.to_string()), "{err}");
    assert_eq!(sequence_and_rows(&s.db()), before);
}

/// PC-1: a `remember` whose attached reference would cross the ceiling is refused and the
/// note it carried is rolled back with it.
#[test]
fn a_remember_whose_reference_crosses_the_ceiling_rolls_back_the_note_too() {
    let s = Scratch::new("kaban-migrate", "ceiling-refs");
    at_the_ceiling(&s);
    let before = sequence_and_rows(&s.db());
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let code_ref = kaban_core::NewNode {
        kind: "CodeRef".into(),
        text: "src/past_the_ceiling.rs".into(),
        ..Default::default()
    };

    let err = store
        .remember_with(note("a note at the ceiling"), &[], &[code_ref], &ctx())
        .unwrap_err();

    assert!(matches!(err, StoreError::NodeIdCeiling { .. }), "{err:?}");
    assert_eq!(sequence_and_rows(&s.db()), before);
}

/// PC-1: a batch whose second insert would cross the ceiling is refused whole: its first
/// insert, which got the last addressable id, is rolled back too.
#[test]
fn a_batch_crossing_the_ceiling_is_refused_whole() {
    use kaban_core::{IngestStore, NodeId, Op, WorkspaceId, WriteBatch};
    let s = Scratch::new("kaban-migrate", "ceiling-batch");
    at_the_ceiling(&s);
    let before = sequence_and_rows(&s.db());
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ops = ["first in the batch", "second in the batch"]
        .into_iter()
        .map(|text| Op::UpsertNode {
            id: NodeId::new(),
            node: note(text),
        })
        .collect();

    let err = store
        .apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
        .unwrap_err();

    assert!(matches!(err, StoreError::NodeIdCeiling { .. }), "{err:?}");
    assert_eq!(sequence_and_rows(&s.db()), before);
}
