//! The workspace/path lookup index `node_chunk_path` is part of the one schema (formerly
//! migration 0006): a fresh store has it from its first open.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test assertions

mod common;

use common::Scratch;
use kaban_store_sqlite::SqliteStore;
use rusqlite::Connection;

/// The one schema version every open serves (spec 05 OWN-P).
const PLAIN_OPEN: u32 = kaban_store_sqlite::LATEST_SCHEMA_VERSION;

const INDEX: &str = "node_chunk_path";

fn has_index(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name=?1",
        [INDEX],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn fresh_store_has_path_index_without_backup() {
    let s = Scratch::new("kaban-path-index", "fresh");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert!(has_index(&Connection::open(s.db()).unwrap()));
    assert_eq!(
        (store.migration().from, store.migration().to),
        (0, PLAIN_OPEN)
    );
}
