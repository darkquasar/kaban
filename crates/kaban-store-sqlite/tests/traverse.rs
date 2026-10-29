//! `traverse` v3 in the store (core-spec F6, §4; core-plan C3.6 as amended by M3-11): traversal
//! runs over `lifecycle = active` nodes of any governance; a superseded Decision (archived) is
//! reachable by id and by recall `include_superseded` but never by traverse; a tombstoned one is
//! reachable nowhere. Hub protection: each node contributes at most 200 edges per direction.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{ctx, nid};
use kaban_core::{
    ForgetMode, Governance, GovernanceStore, Lifecycle, MemoryStore, NewNode, RecallQuery,
    RememberOutcome, TraverseDirection, TraverseQuery,
};
use kaban_store_sqlite::SqliteStore;

fn decision(store: &SqliteStore, text: &str) -> String {
    match store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: text.into(),
                props: Some(serde_json::json!({"decision": text, "rationale": "because"})),
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

fn traverse_ids(store: &SqliteStore, start: &str) -> Vec<String> {
    let r = store
        .traverse(&TraverseQuery {
            start_ids: vec![nid(start)],
            rels: vec![],
            direction: TraverseDirection::Both,
            max_depth: 2,
            node_kinds: vec![],
            governance: vec![],
            limit: 100,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    assert!(
        r.edges
            .iter()
            .all(|e| r.nodes.iter().any(|n| n.id == e.src) && r.nodes.iter().any(|n| n.id == e.dst)),
        "edges among returned nodes only: {r:?}"
    );
    r.nodes.into_iter().map(|n| n.id).collect()
}

fn recall_ids(store: &SqliteStore, query: &str, include_superseded: bool) -> Vec<String> {
    store
        .recall(&RecallQuery {
            query: query.into(),
            hops: 1,
            limit: 10,
            include_superseded,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect()
}

/// M3-11 (binding): `new SUPERSEDES old` archives `old` with governance `superseded`.
#[test]
fn a_superseded_decision_is_reachable_by_id_and_opted_in_recall_but_not_by_traverse() {
    let store = SqliteStore::open_memory().unwrap();
    let old = decision(&store, "quokka builds run on fridays");
    let new = decision(&store, "quokka builds run on mondays");
    // accept the old one so the SUPERSEDES transition is legal (accepted -> superseded)
    store
        .transition(
            &nid(&old),
            Governance::Accepted,
            kaban_core::NewEvent {
                reason: "ship it".into(),
                path_scope: vec!["ci/**".into()],
                ..Default::default()
            },
            &ctx(),
        )
        .unwrap();
    store
        .link(&nid(&new), &nid(&old), "SUPERSEDES", None, None, &ctx())
        .unwrap();
    let archived = store
        .get(&nid(&old), false)
        .unwrap()
        .expect("reachable by id");
    assert_eq!(archived.lifecycle, Lifecycle::Archived);
    assert_eq!(archived.governance, Governance::Superseded);

    assert_eq!(
        traverse_ids(&store, &new),
        vec![new.clone()],
        "traverse never reaches the superseded Decision"
    );
    assert!(
        matches!(
            store.traverse(&TraverseQuery {
                start_ids: vec![nid(&old)],
                rels: vec![],
                direction: TraverseDirection::Both,
                max_depth: 1,
                node_kinds: vec![],
                governance: vec![],
                limit: 10,
                workspace_id: None,
                ..Default::default()
            }),
            Err(kaban_core::StoreError::NotFound(_))
        ),
        "nor does it start from one"
    );
    assert_eq!(
        recall_ids(&store, "quokka mondays", false),
        vec![new.clone()]
    );
    let opted = recall_ids(&store, "quokka mondays", true);
    assert_eq!(opted, vec![new.clone(), old.clone()], "{opted:?}");

    // tombstoned: reachable nowhere, whatever the flags
    store
        .forget(&nid(&old), ForgetMode::Tombstone, &ctx())
        .unwrap();
    assert!(store.get(&nid(&old), false).unwrap().is_none());
    assert_eq!(traverse_ids(&store, &new), vec![new.clone()]);
    for opt in [false, true] {
        assert_eq!(recall_ids(&store, "quokka mondays", opt), vec![new.clone()]);
    }
}

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
            governance: vec![],
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
            governance: vec![],
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
