//! Change capture without writer cooperation (spec 11 T5, KER-03, INV-5): a purge through
//! `forget(Purge)` removes the node's edges, and the change set every index class is handed
//! lists the node and each removed edge, with the values gone rows had before they went.
//!
//! Runs with the `failpoints` feature (the test class lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{ForgetMode, MemoryStore};
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing::{self, ObservedChange, ObservedOp};
use rusqlite::{Connection, OpenFlags};

fn rid(conn: &Connection, id: &str) -> i64 {
    conn.query_row("SELECT rid FROM node WHERE id = ?1", [id], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_purge_lists_the_node_and_all_three_of_its_edges() {
    // Arrange: a hub with three edges (two out, one in)
    let s = Scratch::new("kaban-t5-capture", "purge");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let observed = catalog_testing::install_edge_sediment_class(&store);
    let hub = created(&store, "Fact", "the lighthouse keeper logs every ship");
    let north = created(&store, "Fact", "a trawler passed the north buoy");
    let south = created(&store, "Fact", "a yacht anchored off the south pier");
    let east = created(&store, "Fact", "the east channel was dredged in spring");
    for (src, dst) in [(&hub, &north), (&south, &hub), (&hub, &east)] {
        store
            .link(
                &nid(&src.id),
                &nid(&dst.id),
                "RELATES_TO",
                None,
                None,
                &ctx(),
            )
            .unwrap();
    }
    let raw = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let (h, n, so, e) = (
        rid(&raw, &hub.id),
        rid(&raw, &north.id),
        rid(&raw, &south.id),
        rid(&raw, &east.id),
    );

    // Act: purge the hub
    store
        .forget(&nid(&hub.id), ForgetMode::Purge, &ctx())
        .unwrap();

    // Assert: the edges are gone, and the purge's change set lists the node and all three
    let left: i64 = raw
        .query_row(
            "SELECT count(*) FROM edge WHERE src = ?1 OR dst = ?1",
            [h],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "the purge removes the hub's edges");
    // the purge's own commit: the catalog purge after it commits again, changing no row
    let purge = observed
        .change_sets()
        .into_iter()
        .rev()
        .find(|set| !set.is_empty())
        .unwrap_or_default();
    let node = ObservedChange::Node {
        rid: h,
        op: ObservedOp::Update,
        lifecycle_before: Some("active".into()),
        lifecycle_after: Some("purged".into()),
    };
    assert!(
        purge.contains(&node),
        "the purged node is captured: {purge:?}"
    );
    for (src, dst) in [(h, n), (so, h), (h, e)] {
        let edge = ObservedChange::Edge {
            src,
            dst,
            rel: "RELATES_TO".into(),
            op: ObservedOp::Delete,
        };
        assert!(
            purge.contains(&edge),
            "edge {src}->{dst} is captured: {purge:?}"
        );
    }
}
