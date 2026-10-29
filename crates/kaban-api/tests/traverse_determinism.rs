//! EN-F46 determinism (engine-plan E2.8; feature tier, proptest): the same `TraverseRequest`
//! twice is byte-identical JSON, and the fixture's six edges inserted in any order give the
//! same response once the store-assigned edge ids are set aside. The ids are `UUIDv7`, minted
//! at insertion, and `traverse` orders edges by id (N5: a column), so the ids and the edge
//! order follow the insertion order by construction; the nodes, their order (by row id, fixed
//! here by inserting the nodes first), the edge set and every card field other than the id
//! are what any insertion order must agree on. The negative case: two different edge filters
//! give different JSON.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{KabanApi, LinkRequest, TraverseRequest, TraverseResponse};
use kaban_core::{
    Actor, EdgeDiff, GraphStore, IngestStore, NewNode, NodeId, Op, Owner, WorkspaceId, WriteBatch,
    WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use proptest::prelude::*;
use serde_json::{Value, json};
use std::sync::Arc;

/// Seven fixed node ids (`UUIDv7` shaped), `a` the start.
fn ids() -> Vec<NodeId> {
    (1..=7)
        .map(|i| NodeId::parse(&format!("01a17a03-5344-7000-8000-00000000000{i}")).unwrap())
        .collect()
}

/// The six edges out of `a` and one hop beyond: three human links and three structural ingest
/// edges, so a layer filter has something to keep and something to drop.
const EDGES: [(usize, usize, bool); 6] = [
    (0, 1, true),
    (0, 2, false),
    (0, 3, true),
    (1, 4, false),
    (2, 5, true),
    (3, 6, false),
];

/// A store with the seven nodes written first (their row order fixed), then the edges in
/// `order`, each in its own transaction.
fn fixture(order: &[usize]) -> KabanApi {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    let api = KabanApi::new(
        Arc::clone(&store) as Arc<dyn GraphStore>,
        policy,
        signer,
        None,
    );
    let ids = ids();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let nodes = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: format!("determinism node {i}"),
                ..Default::default()
            },
        })
        .collect();
    store
        .apply_batch(WriteBatch::new(WorkspaceId::default(), nodes), &ctx)
        .unwrap();
    for &i in order {
        let (src, dst, human) = EDGES[i];
        if human {
            api.link(LinkRequest {
                from: ids[src].to_string(),
                rel: "RELATES_TO".into(),
                to: ids[dst].to_string(),
                confidence: None,
                props: None,
            })
            .unwrap();
        } else {
            let mut b = WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::UpsertEdge {
                    src: ids[src].clone(),
                    dst: ids[dst].clone(),
                    rel: "RELATES_TO".into(),
                    props: None,
                }],
            );
            b.edge_diff = EdgeDiff::Owned {
                owner: Owner::KabanIngest,
            };
            store.apply_batch(b, &ctx).unwrap();
        }
    }
    api
}

fn request(edge_filter: Option<&Value>) -> TraverseRequest {
    serde_json::from_value(json!({
        "start_ids": [ids()[0].to_string()],
        "max_depth": 2,
        "edge_filter": edge_filter,
    }))
    .unwrap()
}

/// The response with what two stores built at different instants cannot share set aside:
/// the store-assigned edge ids (edges keyed by `(from, rel, to)` in that order) and the
/// nodes' `created_at` stamps; every other field as returned.
fn canonical(r: &TraverseResponse) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    for n in v["nodes"].as_array_mut().unwrap() {
        n.as_object_mut().unwrap().remove("created_at");
    }
    let edges = v["edges"].as_array_mut().unwrap();
    for e in edges.iter_mut() {
        e.as_object_mut().unwrap().remove("id");
    }
    edges.sort_by_key(|e| {
        (
            e["from"].as_str().unwrap().to_owned(),
            e["rel"].as_str().unwrap().to_owned(),
            e["to"].as_str().unwrap().to_owned(),
        )
    });
    v
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Positive: any insertion order of the six edges gives the reference response once the
    /// edge ids are set aside, and the same request twice on one store is byte-identical.
    #[test]
    fn any_insertion_order_gives_the_same_response(order in Just((0..EDGES.len()).collect::<Vec<usize>>()).prop_shuffle()) {
        let reference = fixture(&(0..EDGES.len()).collect::<Vec<usize>>());
        let permuted = fixture(&order);
        for filter in [None, Some(json!({"layers": ["human"]})), Some(json!({"min_score": 0.5}))] {
            let want = canonical(&reference.traverse(request(filter.as_ref())).unwrap());
            let first = permuted.traverse(request(filter.as_ref())).unwrap();
            let second = permuted.traverse(request(filter.as_ref())).unwrap();
            prop_assert_eq!(
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap(),
                "the same request twice: {:?}", filter
            );
            prop_assert_eq!(canonical(&first), want, "order {:?}, filter {:?}", order, filter);
        }
    }
}

/// Negative: two different edge filters give different JSON (the human layer keeps the two
/// human edges out of `a`; `c -> f` is human too but `c` is reached only through the ingest
/// edge the filter drops; an absent layer keeps none), and either differs from the
/// unfiltered response.
#[test]
fn different_edge_filters_give_different_responses() {
    let api = fixture(&(0..EDGES.len()).collect::<Vec<usize>>());
    let all = api.traverse(request(None)).unwrap();
    let human = api
        .traverse(request(Some(&json!({"layers": ["human"]}))))
        .unwrap();
    let v4 = api
        .traverse(request(Some(&json!({"layers": ["v4"]}))))
        .unwrap();
    assert_eq!(all.edges.len(), 6, "{all:?}");
    assert_eq!(human.edges.len(), 2, "{human:?}");
    assert!(v4.edges.is_empty(), "{v4:?}");
    let s = |r: &TraverseResponse| serde_json::to_string(r).unwrap();
    assert_ne!(s(&human), s(&v4));
    assert_ne!(s(&all), s(&human));
}
