//! Public-behaviour tests of `traverse` v3 (core-spec F6; core-plan C3.6) through `KabanApi`:
//! the governance filter keeps active nodes of the requested states only; EN-F46 (engine-plan
//! E2.8): the ontology filters are validated, the edge filter runs on the stored scoring
//! columns, `kind_set: governing` resolves to the governing kinds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{ApiError, KabanApi, RememberRequest};
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, EdgeDiff, Fixed, GraphStore, IngestStore,
    Layer, Method, NewNode, NodeId, Op, Owner, Rank, StoreError, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

fn api() -> KabanApi {
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    KabanApi::new(store, policy, signer, None)
}

// ---- EN-F46 (engine-plan E2.8): ontology filters, validated; the edge filter; `kind_set` ----

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

/// Three edges out of `a`, one per provenance the stored scoring columns distinguish
/// (migration 0004): a human `remember` link (`layer = human`, `score = 1.0`), a structural
/// ingest edge (`Owned { kaban-ingest }`, no layer, no score) and a projected `gliner` claim
/// (`layer = gliner`, `method = gliner`, `score = 0.6`).
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
    let api = KabanApi::new(
        Arc::clone(&store) as Arc<dyn GraphStore>,
        policy,
        signer,
        None,
    );
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
        ],
    );
    b.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    b.assertions = vec![BatchAssertion {
        subject: BatchSubject::Edge {
            src: a_id,
            rel: "RELATES_TO".into(),
            dst: claimed.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer: Layer::Gliner,
        method: Method::Gliner,
        extractor_id: Some("fixture".into()),
        extractor_version: Some("1".into()),
        source_id: None,
        lineage_id: "gliner:1".into(),
        raw_score: Fixed::new(6000).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }];
    store
        .apply_batch(
            b,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
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

/// EN-F46 `edge_filter.layers`: `["human"]` keeps the human link and drops the ingest edge,
/// whose `layer` column is empty; the node the dropped edge led to is not reached.
#[test]
fn edge_filter_layers_human_keeps_the_link_and_drops_the_ingest_edge() {
    let s = scored();
    let all = traverse_json(&s.api, json!({"start_ids": [s.a]})).unwrap();
    assert_eq!(all.edges.len(), 3, "the fixture: {all:?}");
    let human = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"layers": ["human"]}}),
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

/// EN-F46 `edge_filter`: an unknown layer or method is refused with a fixed word, never
/// echoed; a score outside 0..1 is refused.
#[test]
fn edge_filter_refuses_an_unknown_layer_method_or_score() {
    let s = scored();
    let err = traverse_json(
        &s.api,
        json!({"start_ids": [s.a], "edge_filter": {"layers": ["carrier-pigeon-ZXQV9"]}}),
    )
    .expect_err("an unknown layer");
    assert!(
        matches!(&err, ApiError::Invalid(m) if m == "unknown_layer"),
        "{err:?}"
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

/// EN-F46 `kind_set: governing` is `node_kinds: <the governing kinds>` (the union with the
/// caller's own `node_kinds`); any other set name is `unknown_kind_set`.
#[test]
fn kind_set_governing_is_the_governing_kinds_filter() {
    let api = api();
    let decision = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "quokka storage is sqlite".into(),
            props: Some(json!({"rationale": "one file"})),
            ..Default::default()
        })
        .unwrap()
        .id;
    let fact = api
        .remember(RememberRequest {
            links: vec![kaban_api::LinkSpec {
                rel: "RELATES_TO".into(),
                to: Some(decision.clone()),
                ..Default::default()
            }],
            ..fact("a fact beside the decision")
        })
        .unwrap()
        .id;
    let ids = |r: &kaban_api::TraverseResponse| {
        let mut v: Vec<String> = r.nodes.iter().map(|n| n.id.clone()).collect();
        v.sort();
        v
    };
    let all = traverse_json(&api, json!({"start_ids": [fact]})).unwrap();
    assert_eq!(ids(&all).len(), 2, "{all:?}");
    let governing = api.registry().governing_kinds();
    assert!(
        governing.iter().any(|k| k == "Decision") && !governing.iter().any(|k| k == "Fact"),
        "{governing:?}"
    );
    let by_set =
        traverse_json(&api, json!({"start_ids": [fact], "kind_set": "governing"})).unwrap();
    let by_kinds =
        traverse_json(&api, json!({"start_ids": [fact], "node_kinds": governing})).unwrap();
    assert_eq!(ids(&by_set), vec![decision.clone()], "{by_set:?}");
    assert_eq!(
        serde_json::to_value(&by_set).unwrap(),
        serde_json::to_value(&by_kinds).unwrap()
    );
    // the union with the caller's own kinds
    let union = traverse_json(
        &api,
        json!({"start_ids": [fact], "kind_set": "governing", "node_kinds": ["Fact"]}),
    )
    .unwrap();
    assert_eq!(ids(&union), ids(&all), "{union:?}");
    let err = traverse_json(
        &api,
        json!({"start_ids": [fact], "kind_set": "everything-ZXQV9"}),
    )
    .expect_err("an unknown set");
    assert!(
        matches!(&err, ApiError::Invalid(m) if m == "unknown_kind_set"),
        "{err:?}"
    );
}
