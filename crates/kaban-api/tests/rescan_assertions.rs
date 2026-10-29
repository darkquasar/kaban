//! The rescan job over `assertion.value` through the service (engine-spec EN-S9; engine-plan
//! E2.2 item 11): one policy bump re-judges stored nodes and stored claim values in the same
//! `rescan_if_policy_changed` step; a claim value carrying a term the new policy redacts is
//! masked with the policy's own mask text, set `rejected`, and audited under the `rescan`
//! tool; a second start under the same policy touches nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, fact};
use kaban_api::{KabanApi, RescanStatus};
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, Fixed, GraphStore, IngestStore, Layer, Method,
    NewNode, NodeId, Op, Rank, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Policy v1: builtin rules only. Policy v2: one redact term.
const V1: &str = "";
const V2: &str = "words:\n  - { term: hunter2, action: redact, id: old-password }\n";
const MASK: &str = "[REDACTED:old-password]";

fn api_with(store: &Arc<SqliteStore>, policy: &str) -> KabanApi {
    let policy = Policy::from_file(Policy::parse(policy.as_bytes()).unwrap(), "t".into()).unwrap();
    KabanApi::new(
        Arc::clone(store) as Arc<dyn GraphStore>,
        policy,
        store.signer(),
        None,
    )
}

fn claim_value_and_status(s: &Scratch) -> (String, String) {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row(
        "SELECT a.value, s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// `(tool, field, rules)` of every audit row, oldest first.
fn audit_rows(s: &Scratch) -> Vec<(String, String, String)> {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let mut st = conn
        .prepare("SELECT tool, field, rules FROM audit ORDER BY seq")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn a_policy_bump_masks_a_stored_claim_value_and_a_second_start_touches_nothing() {
    // Arrange: a node and an edge claim whose value v2 will redact, both stored under v1
    let s = Scratch::new("kaban-api-rescan-claims", "mask");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let v1 = api_with(&store, V1);
    v1.remember(fact("a plain note about cargo")).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            Op::UpsertNode {
                id: a.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: "node a".into(),
                    ..Default::default()
                },
            },
            Op::UpsertNode {
                id: b.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: "node b".into(),
                    ..Default::default()
                },
            },
        ],
    );
    batch.assertions = vec![BatchAssertion {
        subject: BatchSubject::Edge {
            src: a,
            rel: "RELATES_TO".into(),
            dst: b,
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({"note": "the old hunter2 password"}),
        layer: Layer::Llm,
        method: Method::Llm,
        extractor_id: Some("fixture".into()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: "l1".into(),
        raw_score: Fixed::new(9000).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }];
    store
        .apply_batch(
            batch,
            &WriteCtx::new(v1.policy_version(), WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    assert_eq!(claim_value_and_status(&s).1, "winner");
    let audit_before = audit_rows(&s).len();

    // Act: the server starts under v2
    let v2 = api_with(&store, V2);
    let outcome = v2
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();

    // Assert: nodes rescanned as before; the claim value masked with the policy's mask text,
    // the claim rejected (its edge, the group's only supporter, removed), one audit row
    assert_eq!(outcome.status, RescanStatus::Completed);
    assert_eq!(outcome.scanned, 3, "the node figures stay the node figures");
    assert_eq!(
        claim_value_and_status(&s),
        (
            format!("{{\"note\":\"the old {MASK} password\"}}"),
            "rejected".to_owned()
        )
    );
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let edges: i64 = conn
        .query_row("SELECT count(*) FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(edges, 0, "a rejected sole supporter projects nothing");
    let rows = audit_rows(&s);
    assert_eq!(rows.len(), audit_before + 1, "{rows:?}");
    let last = rows.last().unwrap();
    assert_eq!(
        (last.0.as_str(), last.1.as_str()),
        ("rescan", "assertion.value")
    );
    assert!(last.2.contains("old-password"), "{last:?}");
    assert_eq!(
        store.index_meta().unwrap().get("assertion_policy_hash"),
        Some(v2.policy_version().as_str())
    );

    // a second start under v2: nothing stale, nothing touched
    let again = api_with(&store, V2)
        .rescan_if_policy_changed(&CancellationToken::new())
        .unwrap();
    assert_eq!(again.status, RescanStatus::UpToDate);
    assert_eq!(audit_rows(&s).len(), audit_before + 1);
    assert_eq!(claim_value_and_status(&s).1, "rejected");
}
