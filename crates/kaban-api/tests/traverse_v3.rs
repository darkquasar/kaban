//! Public-behaviour tests of `traverse` v3 (core-spec F6; core-plan C3.6) through `KabanApi`:
//! EN-F46 (engine-plan E2.8): the ontology filters are validated and the edge filter runs on
//! the stored scoring columns. The removed `kind_set`, `governance` and `follow_anchors` keys
//! are refused at the request parse (`crates/kaban/tests/traverse_removed_keys.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{ApiError, KabanApi, RememberRequest};
use kaban_core::{
    Actor, EdgeDiff, GraphStore, IngestStore, NewNode, NodeId, Op, Producer, StoreError,
    WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

// ---- EN-F46 (engine-plan E2.8): ontology filters, validated; the edge filter ----

/// A request built from its wire JSON, the shape the MCP router hands the service.
fn traverse_json(
    api: &KabanApi,
    v: serde_json::Value,
) -> Result<kaban_api::TraverseResponse, ApiError> {
    api.traverse(serde_json::from_value(v).unwrap())
}

/// `(src, dst)` pairs of the returned edges, sorted.
fn pairs(r: &kaban_api::TraverseResponse) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = r
        .edges
        .iter()
        .map(|e| (e.from.clone(), e.to.clone()))
        .collect();
    p.sort();
    p
}

/// Three edges out of `a`, one per provenance the stored producer and scoring columns
/// distinguish: an authored `remember` link (`authored`, `score = 1.0`), a structural ingest
/// edge (`Owned { document_ingest }`, no method, no score) and an extractor's `gliner` edge
/// (`extractor:gliner`, `method = gliner`, `score = 0.6`).
struct Scored {
    api: KabanApi,
    a: String,
    human: String,
    ingest: String,
    claimed: String,
}

fn scored() -> Scored {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    let api = KabanApi::new(Arc::clone(&store) as Arc<dyn GraphStore>, policy, signer);
    let a = api.remember(fact("alpha, the start")).unwrap().id;
    let human = api
        .remember(RememberRequest {
            links: vec![kaban_api::LinkSpec {
                rel: "RELATES_TO".into(),
                from: Some(a.clone()),
                ..Default::default()
            }],
            ..fact("beta, linked by a human")
        })
        .unwrap()
        .id;
    let (ingest, claimed) = (NodeId::new(), NodeId::new());
    let a_id = NodeId::parse(&a).unwrap();
    let mut b = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            Op::UpsertNode {
                id: ingest.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: "gamma, found by the ingest layer".into(),
                    ..Default::default()
                },
            },
            Op::UpsertNode {
                id: claimed.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: "delta, claimed by an extractor".into(),
                    ..Default::default()
                },
            },
            Op::UpsertEdge {
                src: a_id.clone(),
                dst: ingest.clone(),
                rel: "RELATES_TO".into(),
                props: None,
            },
            // an extractor's edge, its scoring in the props (written directly, T3)
            Op::UpsertEdge {
                src: a_id,
                dst: claimed.clone(),
                rel: "RELATES_TO".into(),
                props: Some(
                    json!({"producer": "extractor:gliner", "method": "gliner", "score": 6000}),
                ),
            },
        ],
    );
    b.edge_diff = EdgeDiff::Owned {
        producer: Producer::DocumentIngest,
    };
    store
        .apply_batch(b, &WriteCtx::new(WorkspaceId::default(), Actor::Agent))
        .unwrap();
    Scored {
        api,
        a,
        human,
        ingest: ingest.to_string(),
        claimed: claimed.to_string(),
    }
}

fn fact(text: &str) -> RememberRequest {
    RememberRequest {
        kind: "Fact".into(),
        text: text.into(),
        ..Default::default()
    }
}

/// EN-F46: a `node_kinds` entry the loaded ontology does not declare is refused as
/// `unknown_kind` (exit 4), not filtered silently.
#[test]
fn an_unknown_node_kind_is_refused_as_unknown_kind() {
    let s = scored();
    // RW3-6: the probe carries a marker; the refusal is the class, and neither `Display`
    // nor `Debug` renders a byte of it (the diagnostics print neither)
    let probe = "Nonexistent-ZXQV9";
    let err = traverse_json(&s.api, json!({"start_ids": [s.a], "node_kinds": [probe]}))
        .expect_err("an undeclared kind must be refused, not filtered silently");
    assert!(
        matches!(&err, ApiError::Store(StoreError::UnknownKind(_))),
        "the refusal class is unknown_kind"
    );
    assert_no_probe(&err, "ZXQV9", "unknown_kind");
}

/// RW3-6: `Display` is the fixed class and `Debug` (plain and alternate) carries no byte of the
/// probe; the failure names the property, never the rendering.
fn assert_no_probe(err: &ApiError, marker: &str, class: &str) {
    // not `assert_eq!`: a failure would print the rendering under test (RW3-7)
    assert!(
        err.to_string() == class,
        "Display is the fixed class {class}"
    );
    assert!(
        !format!("{err:?}").contains(marker),
        "{class}: Debug renders the probe"
    );
    assert!(
        !format!("{err:#?}").contains(marker),
        "{class}: alternate Debug renders the probe"
    );
}

/// EN-F46: a `rels` entry the loaded ontology does not declare is refused as `unknown_rel`.
#[test]
fn an_unknown_rel_is_refused_as_unknown_rel() {
    let s = scored();
    let probe = "NO_SUCH_REL_QWJ7X";
    let err = traverse_json(&s.api, json!({"start_ids": [s.a], "rels": [probe]}))
        .expect_err("an undeclared rel must be refused, not filtered silently");
    assert!(
        matches!(&err, ApiError::Store(StoreError::UnknownRel(_))),
        "the refusal class is unknown_rel"
    );
    assert_no_probe(&err, "QWJ7X", "unknown_rel");
}

/// EN-F46 `edge_filter.producers` (PC-16): `["authored"]` keeps the authored link and drops
/// the ingest and extractor edges; the nodes the dropped edges led to are not reached.
#[test]
fn edge_filter_producers_authored_keeps_the_link_and_drops_the_ingest_edge() {
    let s = scored();
    let all = traverse_json(&s.api, json!({"start_ids": [s.a]})).unwrap();
    assert_eq!(all.edges.len(), 3, "the fixture: {all:?}");
    let human = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"producers": ["authored"]}}),
    )
    .unwrap();
    assert_eq!(
        pairs(&human),
        vec![(s.a.clone(), s.human.clone())],
        "{human:?}"
    );
    let mut ids: Vec<String> = human.nodes.iter().map(|n| n.id.clone()).collect();
    ids.sort();
    let mut want = vec![s.a.clone(), s.human.clone()];
    want.sort();
    assert_eq!(ids, want, "the ingest edge's far endpoint is not reached");
}

/// EN-F46 `edge_filter.methods`: `["gliner"]` keeps the projected claim edge alone.
#[test]
fn edge_filter_methods_keeps_the_matching_claim_edge_alone() {
    let s = scored();
    let gliner = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"methods": ["gliner"]}}),
    )
    .unwrap();
    assert_eq!(
        pairs(&gliner),
        vec![(s.a.clone(), s.claimed.clone())],
        "{gliner:?}"
    );
}

/// EN-F46 `edge_filter.min_score`: `0.7` keeps the human link (1.0) and drops the claim edge
/// (0.6); `0.5` keeps both; the ingest edge, with no score at all, is dropped by either.
#[test]
fn edge_filter_min_score_drops_edges_below_it_and_edges_without_a_score() {
    let s = scored();
    let high = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"min_score": 0.7}}),
    )
    .unwrap();
    assert_eq!(
        pairs(&high),
        vec![(s.a.clone(), s.human.clone())],
        "{high:?}"
    );
    let low = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"min_score": 0.5}}),
    )
    .unwrap();
    let mut want = vec![
        (s.a.clone(), s.human.clone()),
        (s.a.clone(), s.claimed.clone()),
    ];
    want.sort();
    assert_eq!(pairs(&low), want, "{low:?}");
    assert!(
        !low.nodes.iter().any(|n| n.id == s.ingest),
        "the unscored ingest edge is dropped by any score bound: {low:?}"
    );
}

/// EN-F46 `edge_filter`: an unknown producer or method is refused with a fixed word, never
/// echoed; a score outside 0..1 is refused; the removed `layers` key is refused.
#[test]
fn edge_filter_refuses_an_unknown_producer_method_or_score() {
    let s = scored();
    let err = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"producers": ["carrier-pigeon-ZXQV9"]}}),
    )
    .expect_err("an unknown producer");
    assert!(
        matches!(&err, ApiError::Invalid(m) if m == "unknown_producer"),
        "{err:?}"
    );
    assert!(
        serde_json::from_value::<kaban_api::TraverseRequest>(
            json!({"start_ids": [s.a], "edge_filter": {"layers": ["gazetteer"]}})
        )
        .is_err(),
        "the removed `layers` key is refused"
    );
    let err = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"methods": ["guesswork-ZXQV9"]}}),
    )
    .expect_err("an unknown method");
    assert!(
        matches!(&err, ApiError::Invalid(m) if m == "unknown_method"),
        "{err:?}"
    );
    let err = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"min_score": 1.5}}),
    )
    .expect_err("a score outside 0..1");
    assert!(matches!(&err, ApiError::Invalid(_)), "{err:?}");
}
