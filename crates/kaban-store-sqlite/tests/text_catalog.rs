//! Text on the catalog (spec 11 T7, KER-01, TXT-06): a commit's text is searchable by every
//! store of the database as soon as the commit returns, and a record deleted while a merge is
//! in flight never comes back once the merge publishes.
//!
//! Runs with the `failpoints` feature (the compaction window hooks live behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created};
use kaban_core::{MemoryStore, RecallQuery};
use kaban_store_sqlite::SqliteStore;

#[test]
fn a_remember_is_found_by_a_second_store_of_the_database_at_once() {
    // Arrange: two stores of one database, the second opened before the write
    let s = Scratch::new("kaban-t7-text", "visible");
    let writer = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    // Act: remember through the first, recall through the second right after it returns
    let node = created(&writer, "Fact", "zebra-sentinel");
    let hits = reader
        .recall(&RecallQuery {
            query: "zebra".into(),
            ..Default::default()
        })
        .unwrap();

    // Assert: no wait, no poll, no barrier
    assert!(
        hits.iter().any(|h| h.node.id == node.id),
        "the second store did not find the record its peer just committed: {hits:?}"
    );
}
