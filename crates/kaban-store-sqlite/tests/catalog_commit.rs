//! The one commit entry (spec 11 T5, KER-07, REC-06, INV-3): a write through any writer, here
//! `SqliteStore::link` (not `apply_batch`), stages every index class inside its own transaction,
//! so a pin another process takes after the write returns sees the staged sediment at once.
//!
//! Runs with the `failpoints` feature (the test class lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::MemoryStore;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing::{self, ObservedOp};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

/// The database the pinning role opens (set only for the child process).
const PIN_DB: &str = "KABAN_T5_PIN_DB";
/// The file the pinning role writes the sediment it saw to, one payload per line.
const PINNED: &str = "pinned";

/// The pinning role of the two-process case: in a child process (`KABAN_T5_PIN_DB` set) open
/// the store, pin its catalog at once and write the test class's sediment payloads to
/// `<dir>/pinned`. A no-op elsewhere.
#[test]
#[ignore = "a role of the two-process visibility case"]
fn multi_process_pin_role() {
    let Ok(db) = std::env::var(PIN_DB) else {
        return;
    };
    let db = PathBuf::from(db);
    let dir = db.parent().unwrap().to_path_buf();
    let store = SqliteStore::open(&db, &dir.join("keys")).unwrap();
    let rows = catalog_testing::pin_sediment(&store).unwrap();
    std::fs::write(dir.join(PINNED), rows.join("\n")).unwrap();
}

fn rid(conn: &Connection, id: &str) -> i64 {
    conn.query_row("SELECT rid FROM node WHERE id = ?1", [id], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_link_is_in_the_sediment_of_a_pin_another_process_takes_after_it_returns() {
    // Arrange: process A's store with the test class installed, two nodes
    let s = Scratch::new("kaban-t5-commit", "link-visibility");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let _observed = catalog_testing::install_edge_sediment_class(&store);
    let from = created(&store, "Fact", "the harbour opens at dawn");
    let to = created(&store, "Fact", "the ferry leaves the harbour at noon");

    // Act: A links (a non-`apply_batch` writer); after it returns, process B pins
    store
        .link(
            &nid(&from.id),
            &nid(&to.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "multi_process_pin_role", "--ignored"])
        .env(PIN_DB, s.db())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the pinning role failed: {status:?}");

    // Assert: B's pin holds exactly the one row A's link staged
    let raw = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let expected = catalog_testing::edge_payload(
        ObservedOp::Insert,
        rid(&raw, &from.id),
        rid(&raw, &to.id),
        "RELATES_TO",
    );
    let pinned = std::fs::read_to_string(s.0.join(PINNED)).unwrap();
    assert_eq!(
        pinned.lines().collect::<Vec<_>>(),
        vec![expected.as_str()],
        "process B's pin must hold the sediment row of A's link"
    );
}

/// Every entry of `dir` by name, with a regular file's bytes (`None` for anything else).
fn listing(dir: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let bytes = entry
                .file_type()
                .unwrap()
                .is_file()
                .then(|| std::fs::read(entry.path()).unwrap());
            (entry.file_name().into_string().unwrap(), bytes)
        })
        .collect()
}

/// The catalog's artefacts beside a database.
const CATALOG_ARTEFACTS: [&str; 3] = ["kaban.kbx", "kaban.kbx.alloc", "kaban.kbx.pins"];

#[test]
fn a_fresh_project_opens_with_its_catalog_beside_the_database() {
    // Arrange
    let s = Scratch::new("kaban-t5-commit", "fresh-catalog");

    // Act
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    // Assert: the container and the allocation lock are files, the pins a directory
    let entries = listing(&s.0);
    for name in CATALOG_ARTEFACTS {
        assert!(
            entries.contains_key(name),
            "{name} is missing: {:?}",
            entries.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        s.0.join("kaban.kbx.pins").is_dir(),
        "the pins are a directory"
    );
    drop(store);
}

#[test]
fn a_refused_older_format_project_gains_no_catalog_artefact() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    for fixture in ["v3.0.0", "v3.0.0-wal"] {
        // Arrange: a copy of the 3.0.0 project (never opened in place)
        let s = Scratch::new("kaban-t5-commit", &format!("refused-{fixture}"));
        for entry in std::fs::read_dir(fixtures.join(fixture)).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), s.0.join(entry.file_name())).unwrap();
        }
        let before = listing(&s.0);

        // Act: every open refuses it
        let actor = kaban_core::ActorId::default();
        let refusals = [
            SqliteStore::open(&s.db(), &s.keys()).map(drop),
            SqliteStore::open_existing_key(&s.db(), &s.keys(), &actor).map(drop),
            SqliteStore::open_diagnostic(&s.db(), &s.keys(), &actor).map(drop),
            SqliteStore::open_for_hook(&s.db(), &s.keys(), &actor).map(drop),
        ];

        // Assert: refused, and the directory is as it was: no catalog artefact, no byte changed
        for refused in refusals {
            let err = refused.unwrap_err();
            assert!(
                matches!(
                    err,
                    kaban_core::StoreError::SchemaUnsupported { found: 13, .. }
                ),
                "{fixture}: {err:?}"
            );
        }
        let after = listing(&s.0);
        for name in CATALOG_ARTEFACTS {
            assert!(!after.contains_key(name), "{fixture}: {name} was created");
        }
        assert_eq!(after, before, "{fixture}: the project was changed");
    }
}
