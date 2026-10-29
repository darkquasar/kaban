//! The evaluation counters behind `kaban doctor --priors` (engine-spec EN-F16; engine-plan E2.4
//! item 5): a human `accept` of a record whose `type` claims sit below the margin (a
//! `node_candidate` row) counts one `accepted` per reconciled candidate claim of its layer
//! triple and changes no edge row; `forget` counts `rejected` the same way (item 10).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{AcceptRequest, AcceptResponse, Caller, ForgetRequest, KabanApi};
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, Fixed, GraphStore, IngestStore, Layer, Method,
    NewNode, NodeId, Op, PriorsObs, Rank, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

fn api_on(store: &Arc<SqliteStore>) -> KabanApi {
    KabanApi::new(
        Arc::clone(store) as Arc<dyn GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
    )
}

/// A `type` claim about `node` from one lineage.
fn type_claim(
    node: &NodeId,
    value: &str,
    layer: Layer,
    method: Method,
    raw: u32,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Node(node.clone()),
        claim_class: ClaimClass::Type,
        predicate: None,
        object: None,
        value: json!(value),
        layer,
        method,
        extractor_id: Some(format!("{}-1", layer.as_str())),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: format!("{}-1", layer.as_str()),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// A proposed Convention with a structural edge to a Fact and two `type` claims about it
/// below the margin (gliner `Fact` 3600 against llm `Idea` 3750: shares `0.4898`/`0.5102`,
/// margin `0.0204 < 0.3000`): the Convention gets a `node_candidate` row, both claims are
/// `candidate`. Returns the store and the Convention's id.
fn convention_with_candidate_claims(s: &Scratch) -> (Arc<SqliteStore>, NodeId) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let (conv, fact) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            Op::UpsertNode {
                id: conv.clone(),
                node: NewNode {
                    kind: "Convention".into(),
                    text: "run the linter before every commit".into(),
                    ..Default::default()
                },
            },
            Op::UpsertNode {
                id: fact.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: "the linter is clippy".into(),
                    ..Default::default()
                },
            },
            Op::UpsertEdge {
                src: conv.clone(),
                dst: fact.clone(),
                rel: "RELATES_TO".into(),
                props: None,
            },
        ],
    );
    batch.assertions = vec![
        type_claim(&conv, "Fact", Layer::Gliner, Method::Gliner, 6000),
        type_claim(&conv, "Idea", Layer::Llm, Method::Llm, 5000),
    ];
    store
        .apply_batch(
            batch,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    (store, conv)
}

/// `SELECT * FROM edge ORDER BY id`, every column as text.
fn edge_rows(s: &Scratch) -> String {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let mut st = conn.prepare("SELECT * FROM edge ORDER BY id").unwrap();
    let n = st.column_count();
    let rows: Vec<Vec<String>> = st
        .query_map([], |r| {
            (0..n)
                .map(|i| {
                    Ok(match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => "NULL".to_owned(),
                        rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                        rusqlite::types::ValueRef::Real(v) => v.to_string(),
                        rusqlite::types::ValueRef::Text(t) => {
                            String::from_utf8_lossy(t).into_owned()
                        }
                        rusqlite::types::ValueRef::Blob(b) => format!("{b:?}"),
                    })
                })
                .collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!rows.is_empty(), "the fixture holds an edge row");
    serde_json::to_string(&rows).unwrap()
}

fn candidate_rows(s: &Scratch) -> i64 {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row("SELECT count(*) FROM node_candidate", [], |r| r.get(0))
        .unwrap()
}

/// The counters of the two layer triples the fixture's claims belong to.
fn counters(rows: &[PriorsObs]) -> Vec<(&'static str, u64, u64)> {
    [(Layer::Gliner, Method::Gliner), (Layer::Llm, Method::Llm)]
        .into_iter()
        .map(|(layer, method)| {
            let row = rows
                .iter()
                .find(|r| {
                    r.layer == layer && r.method == method && r.claim_class == ClaimClass::Type
                })
                .unwrap();
            (layer.as_str(), row.accepted, row.rejected)
        })
        .collect()
}

/// Slice (EN-F16 "accept a candidate => counters change; no edge changes from counters
/// alone"): the human acceptance of the Convention counts `accepted = 1` for the gliner and
/// the llm triple, the human's own `kind_of_record` claim counts nothing, and the edge table
/// is byte-identical before and after.
#[test]
fn accepting_a_record_with_candidate_claims_counts_each_layer_once_and_moves_no_edge() {
    let s = Scratch::new("e24-priors", "accept");
    let (store, conv) = convention_with_candidate_claims(&s);
    assert_eq!(candidate_rows(&s), 1, "the type claims left a review row");
    let before = edge_rows(&s);
    let fresh = store.priors_obs().unwrap();
    assert!(
        fresh.iter().all(|r| r.accepted == 0 && r.rejected == 0),
        "a fresh store has counted nothing"
    );
    assert!(
        fresh.len() > 100,
        "every configured prior is listed: {}",
        fresh.len()
    );

    let api = api_on(&store);
    let response = api
        .accept(
            AcceptRequest {
                id: conv.to_string(),
                reason: Some("reviewed".into()),
                path_scope: vec!["src/".into()],
                signer: None,
                confirmation_token: None,
                confirmed_by_user: false,
            },
            &Caller {
                kind: Actor::HumanCli,
                host: None,
                session: None,
            },
        )
        .unwrap();
    assert!(
        matches!(response, AcceptResponse::Accepted { .. }),
        "{response:?}"
    );

    let after = store.priors_obs().unwrap();
    assert_eq!(
        counters(&after),
        vec![("gliner", 1, 0), ("llm", 1, 0)],
        "one accepted per candidate claim"
    );
    assert!(
        after
            .iter()
            .filter(|r| r.layer == Layer::Human)
            .all(|r| r.accepted == 0 && r.rejected == 0),
        "the human's own claim is no evidence about a layer"
    );
    assert_eq!(edge_rows(&s), before, "no edge row changes from counters");
}

/// Item 10: `forget` of the same record counts `rejected = 1` per candidate claim, read
/// before the tombstone rejects the claims' statuses.
#[test]
fn forgetting_a_record_with_candidate_claims_counts_rejected_once_per_layer() {
    let s = Scratch::new("e24-priors", "forget");
    let (store, conv) = convention_with_candidate_claims(&s);
    let api = api_on(&store);
    api.forget(ForgetRequest {
        id: conv.to_string(),
        mode: None,
    })
    .unwrap();
    assert_eq!(
        counters(&store.priors_obs().unwrap()),
        vec![("gliner", 0, 1), ("llm", 0, 1)]
    );
}
