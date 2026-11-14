//! `traverse` hub protection in the store (core-plan C3.6; DEL-08 survivors of the claim-era
//! file): each node contributes at most 200 edges per direction, and the relationship filter
//! applies before that cut.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{ctx, nid};
use kaban_core::{MemoryStore, NewNode, RememberOutcome, TraverseDirection, TraverseQuery};
use kaban_store_sqlite::SqliteStore;

fn fact(store: &SqliteStore, text: &str) -> String {
    match store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: text.into(),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n.id,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// Hub protection (core-plan C3.6; m1 Me5 follow-up): a node with 1,000 outgoing edges
/// contributes at most 200 of them (`LIMIT 200` inside the SQL, deterministic by edge id), and
/// the traversal answers in under 50 ms.
#[test]
fn a_hub_with_a_thousand_edges_yields_two_hundred_in_under_fifty_ms() {
    let store = SqliteStore::open_memory().unwrap();
    let hub = fact(&store, "quokka hub");
    let mut edge_ids = Vec::with_capacity(1000);
    for i in 0..1000 {
        let leaf = fact(&store, &format!("quokka leaf {i}"));
        let e = store
            .link(&nid(&hub), &nid(&leaf), "RELATES_TO", None, None, &ctx())
            .unwrap();
        edge_ids.push(e.id);
    }
    let started = std::time::Instant::now();
    let r = store
        .traverse(&TraverseQuery {
            start_ids: vec![nid(&hub)],
            rels: vec![],
            direction: TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec![],

            limit: 500,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r.edges.len(), 200, "per-node edge cap");
    assert_eq!(r.nodes.len(), 201, "hub plus 200 leaves");
    edge_ids.sort();
    let returned: Vec<&str> = r.edges.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        returned,
        edge_ids[..200]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "the first 200 edges by id, deterministic (N5)"
    );
    assert!(elapsed.as_millis() < 50, "traverse took {elapsed:?}");
}

/// M3 review R3-7: the relationship filter applies before the per-node hub cut, so a hub whose
/// 200 oldest edges use another relationship still yields its matching neighbours (and only
/// those); the cut itself stays deterministic by edge id.
#[test]
fn a_relationship_filter_sees_past_two_hundred_older_edges_of_another_relationship() {
    let store = SqliteStore::open_memory().unwrap();
    let hub = fact(&store, "wombat hub");
    for i in 0..200 {
        let leaf = fact(&store, &format!("wombat related leaf {i}"));
        store
            .link(&nid(&hub), &nid(&leaf), "RELATES_TO", None, None, &ctx())
            .unwrap();
    }
    // two rels, one filtered (the hub-cut intent): the filtered rel is the unconstrained core
    // `LINKS_TO`, since `FIXED_BY` is `from: [Failure]` and enforced as data since E1.8 (E1.7 R1)
    let mut fixes = Vec::new();
    for i in 0..3 {
        let fix = fact(&store, &format!("wombat fix {i}"));
        store
            .link(&nid(&hub), &nid(&fix), "LINKS_TO", None, None, &ctx())
            .unwrap();
        fixes.push(fix);
    }
    let r = store
        .traverse(&TraverseQuery {
            start_ids: vec![nid(&hub)],
            rels: vec!["LINKS_TO".into()],
            direction: TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec![],

            limit: 500,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    let mut reached: Vec<String> = r
        .nodes
        .iter()
        .filter(|n| n.id != hub)
        .map(|n| n.id.clone())
        .collect();
    reached.sort();
    fixes.sort();
    assert_eq!(
        reached, fixes,
        "the LINKS_TO neighbours beyond the 200 older edges"
    );
    assert_eq!(r.edges.len(), 3, "{:?}", r.edges);
    assert!(r.edges.iter().all(|e| e.rel == "LINKS_TO"), "{:?}", r.edges);
}
