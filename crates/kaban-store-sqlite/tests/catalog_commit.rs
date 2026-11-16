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
