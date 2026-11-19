//! The memory graph on the catalog (spec 11 T11 Step 1; GRA-02, GRA-03, KER-01): an edge is
//! traversable from every store of the database once its commit returns, with its row's
//! attributes, and a row that maps to no graph attribute refuses its write by name.
//!
//! Runs with the `failpoints` feature (the graph's test access lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{MemoryStore, StoreError};
use kaban_core::query_budget::QueryBudget;
use kaban_index_graph::{Caps, Direction, RelSelection, expand};
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing;
use std::time::Duration;

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(Duration::from_secs(10)))
}

/// The driver's text of a backend error.
fn driver_text(e: &StoreError) -> String {
    let StoreError::Backend { source } = e else {
        panic!("not a backend error: {e:?}");
    };
    source.driver().to_string()
}

const CAPS: Caps = Caps {
    depth: 2,
    rows: 50,
};

#[test]
fn a_link_committed_in_one_store_is_traversable_from_another_at_once() {
    // Arrange: two stores of one database
    let s = Scratch::new("kaban-t11-main", "visible");
    let writer = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = nid(&created(&writer, "Fact", "a").id);
    let b = nid(&created(&writer, "Fact", "b").id);
    let lone = nid(&created(&writer, "Fact", "isolated").id);

    // Act: the link returns in the writer; the reader loads `main` right after
    writer
        .link(&a, &b, "RELATES_TO", None, None, &ctx())
        .unwrap();
    let (reached, isolated, unused_rel) =
        catalog_testing::with_main_graph(&reader, &budget(), |g| {
            let la = g.local(&a)?.expect("a has a local id");
            let r = expand(g.view(), &[la], &RelSelection::All, Direction::Out, CAPS, &budget())
                .unwrap();
            let reached: Vec<_> = r
                .nodes
                .iter()
                .map(|&(n, _)| g.node_id(n).unwrap().unwrap())
                .collect();
            let lone = g.local(&lone)?.expect("the isolated node has a local id");
            Ok((reached, g.view().contains(lone), g.rel("SUPERSEDES")))
        })
        .unwrap();

    // Assert: b reached with no wait; the isolated node present; a declared relation that no
    // edge uses has an id
    assert_eq!(reached, vec![b.clone()]);
    assert!(isolated, "an isolated node is present");
    assert!(unused_rel.is_some(), "a declared, unused relation has an id");
}

#[test]
fn an_edge_carries_its_rows_weight_and_time_and_unmappable_values_refuse_the_write() {
    // Arrange
    let s = Scratch::new("kaban-t11-main", "attrs");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = nid(&created(&store, "Fact", "a").id);
    let b = nid(&created(&store, "Fact", "b").id);
    store
        .link(&a, &b, "RELATES_TO", Some(0.25), None, &ctx())
        .unwrap();
    catalog_testing::corrupt(
        &store,
        "UPDATE edge SET created_at = '2024-02-29T12:00:00.123+02:00'",
    )
    .unwrap();

    // Act
    let attrs = catalog_testing::with_main_graph(&store, &budget(), |g| {
        let la = g.local(&a)?.unwrap();
        let rels = RelSelection::All;
        let b2 = budget();
        Ok(g.view()
            .neighbours(la, &rels, Direction::Out, &b2)
            .map(|n| n.unwrap().2)
            .collect::<Vec<_>>())
    })
    .unwrap();
    let negative = catalog_testing::corrupt(&store, "UPDATE edge SET confidence = -0.5");
    let untimed = catalog_testing::corrupt(&store, "UPDATE edge SET created_at = 'yesterday'");

    // Assert: weight in millionths, time in epoch milliseconds; both refusals name the
    // attribute and roll their write back
    assert_eq!(attrs.len(), 1);
    assert_eq!(
        (attrs[0].weight, attrs[0].ts),
        (250_000, 1_709_200_800_123)
    );
    let negative = driver_text(&negative.expect_err("a negative weight was admitted"));
    assert!(
        negative.contains("graph_weight_refused") && negative.contains("confidence"),
        "{negative}"
    );
    let untimed = driver_text(&untimed.expect_err("an unparsable time was admitted"));
    assert!(
        untimed.contains("graph_timestamp_refused") && untimed.contains("created_at"),
        "{untimed}"
    );
}
