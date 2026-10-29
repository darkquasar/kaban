//! Spec 05 SS-07, feature `text-index-override`: a store built with `with_text_index` seeds its
//! ordinary `recall` from the injected adapter, in the adapter's order, and FTS5 is not
//! consulted; the expansion, fusion and materialization stay the store's.

#![cfg(feature = "text-index-override")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created};
use kaban_core::text_index::TextHit;
use kaban_core::text_index::fake::FakeIndex;
use kaban_core::{MemoryStore, Node, RecallQuery};
use kaban_store_sqlite::SqliteStore;

/// `(rid, rev)` of `node`, read beside the store.
fn row(conn: &rusqlite::Connection, node: &Node) -> (i64, i64) {
    conn.query_row("SELECT rid, rev FROM node WHERE id = ?1", [&node.id], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .unwrap()
}

#[test]
fn recall_returns_the_injected_index_hits_in_its_order() {
    // Arrange: four notes FTS5 would all match; the fake lists three of them, last first
    let s = Scratch::new("ss07-override", "order");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let notes = [
        "quokka one by the shore",
        "quokka two under the trees",
        "quokka three at the ferry",
        "quokka four near the settlement",
    ]
    .map(|text| created(&store, "Fact", text));
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let scripted = [2, 0, 1].map(|i| {
        let (rid, revision) = row(&conn, &notes[i]);
        TextHit {
            rid,
            revision,
            score: 1.0,
            kind: "Fact".into(),
        }
    });
    let store = store.with_text_index(Box::new(FakeIndex {
        lexical: Some(scripted.to_vec()),
        ..Default::default()
    }));

    // Act
    let hits = store
        .recall(&RecallQuery {
            query: "quokka".into(),
            hops: 0,
            ..Default::default()
        })
        .unwrap();

    // Assert: the fake's three, in its order; the fourth (an FTS5 match) is not a hit
    let ids: Vec<&str> = hits.iter().map(|h| h.node.id.as_str()).collect();
    assert_eq!(ids, [2, 0, 1].map(|i| notes[i].id.as_str()), "{hits:?}");
    assert!(
        hits.iter()
            .all(|h| h.why.iter().any(|w| w.starts_with("bm25:"))),
        "seeded by the injected index: {hits:?}"
    );
}
