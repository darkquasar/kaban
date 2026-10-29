//! `KabanApi::deprecate` behind `kaban explain <key> --deprecate <reason>` (engine-spec
//! EN-F12b; engine-plan E2.3 item 5): a human sets a claim's rank to `deprecated` with a
//! content-gated reason and the claim's group is re-decided without its vote; an agent actor
//! is refused with a fixed text; a reason carrying a canary is rejected before any write.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{ApiConfig, ApiError, DeprecateRequest, KabanApi};
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, Fixed, GraphStore, IngestStore, Layer,
    MemoryStore, Method, NewNode, NodeId, Op, Rank, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view; built
/// at run time so the literal never sits in a source file.
fn canary() -> String {
    format!("AKIA{}", "QWERTYUIOPASDFGH")
}

fn api_on(store: &Arc<SqliteStore>) -> KabanApi {
    KabanApi::new(
        Arc::clone(store) as Arc<dyn GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
    )
}

fn edge_claim(
    src: &NodeId,
    dst: &NodeId,
    layer: Layer,
    method: Method,
    raw: u32,
    lineage: &str,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: "RELATES_TO".into(),
            dst: dst.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer,
        method,
        extractor_id: Some(lineage.to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// Two machine claims on one triple (gliner 5400 wins over llm 4500); returns the store and
/// the `(winner key, other key)`.
fn two_claims(s: &Scratch) -> (Arc<SqliteStore>, (String, String)) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let (a, b) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        [(&a, "node a"), (&b, "node b")]
            .into_iter()
            .map(|(id, text)| Op::UpsertNode {
                id: id.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    ..Default::default()
                },
            })
            .collect(),
    );
    batch.assertions = vec![
        edge_claim(&a, &b, Layer::Gliner, Method::Gliner, 9000, "g1"),
        edge_claim(&a, &b, Layer::Llm, Method::Llm, 6000, "l1"),
    ];
    store
        .apply_batch(
            batch,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let key = |lineage: &str| -> String {
        conn.query_row(
            "SELECT key FROM assertion WHERE lineage_id = ?1",
            [lineage],
            |r| r.get(0),
        )
        .unwrap()
    };
    let keys = (key("g1"), key("l1"));
    (store, keys)
}

fn rank_and_status(s: &Scratch, key: &str) -> (String, Option<String>, String) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT a.rank, a.rank_reason, s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id WHERE a.key = ?1",
        [key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// Slice: a human deprecates the winning claim; its rank and reason are stored, the group is
/// re-decided in the same call and the other supporter wins; the deprecated claim is dissent.
#[test]
fn a_human_deprecation_stores_the_rank_and_redecides_the_group() {
    let s = Scratch::new("e23-deprecate", "human");
    let (store, (winner, other)) = two_claims(&s);
    let api = api_on(&store);
    let r = api
        .deprecate(
            DeprecateRequest {
                key: winner.clone(),
                reason: "the gazetteer alias is wrong here".into(),
            },
            Actor::HumanCli,
        )
        .unwrap();
    assert_eq!(r.key, winner);
    assert_eq!(r.disposition, "dissent");
    assert_eq!(
        rank_and_status(&s, &winner),
        (
            "deprecated".to_owned(),
            Some("the gazetteer alias is wrong here".to_owned()),
            "dissent".to_owned()
        )
    );
    assert_eq!(rank_and_status(&s, &other).2, "winner");
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let confidence: f64 = conn
        .query_row("SELECT confidence FROM edge", [], |r| r.get(0))
        .unwrap();
    assert!((confidence - 0.45).abs() < 1e-9, "{confidence}");
}

/// Negative: an agent actor is refused with a fixed text and nothing changes.
#[test]
fn an_agent_actor_is_refused_with_a_fixed_text() {
    let s = Scratch::new("e23-deprecate", "agent");
    let (store, (winner, _)) = two_claims(&s);
    let api = api_on(&store);
    let err = api
        .deprecate(
            DeprecateRequest {
                key: winner.clone(),
                reason: "no".into(),
            },
            Actor::Agent,
        )
        .expect_err("agents never deprecate");
    assert!(
        matches!(&err, ApiError::Invalid(text) if text == kaban_api::DEPRECATE_HUMAN_ONLY),
        "{err:?}"
    );
    assert_eq!(rank_and_status(&s, &winner).0, "normal");
}

/// Negative (S6; E2 interim review RE4-4): a process bound to workspace B that knows a claim
/// key of workspace A cannot deprecate it. The refusal is the same fixed `NotFound` an unknown
/// key gets (no existence leak), the rank is unchanged and the group is not re-decided.
#[test]
fn a_key_of_another_workspace_is_not_found_and_its_rank_is_unchanged() {
    let s = Scratch::new("e23-deprecate", "foreign-ws");
    let (store, (winner, other)) = two_claims(&s);
    let api = KabanApi::with_config(
        Arc::clone(&store) as Arc<dyn GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new("ws-b"),
            ..Default::default()
        },
    );
    let foreign = api
        .deprecate(
            DeprecateRequest {
                key: winner.clone(),
                reason: "not mine to retire".into(),
            },
            Actor::HumanCli,
        )
        .expect_err("a claim of another workspace is not found");
    let unknown = api
        .deprecate(
            DeprecateRequest {
                key: "0".repeat(64),
                reason: "not mine to retire".into(),
            },
            Actor::HumanCli,
        )
        .expect_err("an unknown key is not found");
    assert!(
        matches!(&foreign, ApiError::NotFound(k) if *k == winner),
        "{foreign:?}"
    );
    assert!(
        matches!(&unknown, ApiError::NotFound(k) if *k == "0".repeat(64)),
        "{unknown:?}"
    );
    assert_eq!(
        rank_and_status(&s, &winner),
        ("normal".to_owned(), None, "winner".to_owned())
    );
    assert_eq!(rank_and_status(&s, &other).2, "dissent");
}

/// Two nodes and a human `link` between them (a `preferred` human claim that wins its group
/// and owns the materialised edge); returns the store, the nodes and the human claim's key.
fn human_link(s: &Scratch) -> (Arc<SqliteStore>, (NodeId, NodeId), String) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let (a, b) = (NodeId::new(), NodeId::new());
    let batch = WriteBatch::new(
        WorkspaceId::default(),
        [(&a, "node a"), (&b, "node b")]
            .into_iter()
            .map(|(id, text)| Op::UpsertNode {
                id: id.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    ..Default::default()
                },
            })
            .collect(),
    );
    store
        .apply_batch(
            batch,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    store
        .link(
            &a,
            &b,
            "RELATES_TO",
            Some(0.5),
            None,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let key: String = conn
        .query_row("SELECT key FROM assertion WHERE layer = 'human'", [], |r| {
            r.get(0)
        })
        .unwrap();
    (store, (a, b), key)
}

/// `(count, layer, confidence)` of the `edge` rows: the one projected row, or none.
fn edge_state(s: &Scratch) -> (i64, Option<String>, Option<f64>) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT count(*), min(layer), min(confidence) FROM edge",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// E2 interim review RE4-7 (EN-F12b, EN-F12c): a human winner a human deprecates stops
/// protecting its group in the same call. Without another supporter nothing votes, so the
/// edge the human's claim materialised is dropped (its bytes were the human's) and the claim
/// is `rejected`, not left `winner` behind a row that outlives it.
#[test]
fn deprecating_the_human_winner_without_a_supporter_drops_its_edge() {
    let s = Scratch::new("e23-deprecate", "human-winner-alone");
    let (store, _, human) = human_link(&s);
    assert_eq!(edge_state(&s).1.as_deref(), Some("human"));
    let api = api_on(&store);
    let r = api
        .deprecate(
            DeprecateRequest {
                key: human.clone(),
                reason: "linked the wrong note".into(),
            },
            Actor::HumanCli,
        )
        .unwrap();
    assert_eq!(r.disposition, "rejected", "{r:?}");
    assert_eq!(
        rank_and_status(&s, &human),
        (
            "deprecated".to_owned(),
            Some("linked the wrong note".to_owned()),
            "rejected".to_owned()
        )
    );
    assert_eq!(edge_state(&s), (0, None, None), "no voter, no edge");
}

/// E2 interim review RE4-7: with a machine supporter left, deprecating the human winner
/// re-decides the group to the machine value: the machine claim wins, the deprecated human is
/// `dissent`, and the edge row carries the machine's layer and confidence (gliner 0.6 x 0.9),
/// not the deprecated human's.
#[test]
fn deprecating_the_human_winner_with_a_machine_supporter_redecides_to_the_machine_value() {
    let s = Scratch::new("e23-deprecate", "human-winner-supported");
    let (store, (a, b), human) = human_link(&s);
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions = vec![edge_claim(
        &a,
        &b,
        Layer::Gliner,
        Method::Gliner,
        9000,
        "g1",
    )];
    store
        .apply_batch(
            batch,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let machine: String = conn
        .query_row(
            "SELECT key FROM assertion WHERE lineage_id = 'g1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rank_and_status(&s, &machine).2, "dissent", "protected");
    assert_eq!(edge_state(&s).1.as_deref(), Some("human"));
    let api = api_on(&store);
    let r = api
        .deprecate(
            DeprecateRequest {
                key: human.clone(),
                reason: "the gazetteer was right".into(),
            },
            Actor::HumanCli,
        )
        .unwrap();
    assert_eq!(r.disposition, "dissent", "{r:?}");
    assert_eq!(rank_and_status(&s, &human).2, "dissent");
    assert_eq!(rank_and_status(&s, &machine).2, "winner");
    let (rows, layer, confidence) = edge_state(&s);
    assert_eq!((rows, layer.as_deref()), (1, Some("gliner")));
    let confidence = confidence.unwrap();
    assert!((confidence - 0.54).abs() < 1e-9, "{confidence}");
}

/// Negative: a `rank_reason` carrying a canary is content and is rejected before any write.
#[test]
fn a_reason_carrying_a_canary_is_rejected_before_any_write() {
    let s = Scratch::new("e23-deprecate", "canary");
    let (store, (winner, _)) = two_claims(&s);
    let api = api_on(&store);
    let canary = canary();
    let err = api
        .deprecate(
            DeprecateRequest {
                key: winner.clone(),
                reason: format!("leaked {canary}"),
            },
            Actor::HumanCli,
        )
        .expect_err("a reject hit fails closed");
    assert!(matches!(err, ApiError::Rejected { .. }), "{err:?}");
    assert_eq!(rank_and_status(&s, &winner).0, "normal");
    let bytes = std::fs::read(s.db()).unwrap();
    assert!(
        !bytes.windows(canary.len()).any(|w| w == canary.as_bytes()),
        "the canary never reaches the database file"
    );
}
