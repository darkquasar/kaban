//! The transport-neutral write path (engine-spec EN-E10a; engine-plan E2.11): a `WriteBatch`
//! is plain serializable data. Every `Op` variant survives JSON and back equal;
//! the bounds a remote `GraphStore` needs hold at compile time; a batch written before
//! `edge_diff` existed still parses; an unknown `op` tag is refused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::{EdgeDiff, NewNode, NodeId, Op, Producer, WorkspaceId, WriteBatch};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

/// The bounds EN-E10a names for a batch that crosses a process boundary.
fn assert_bounds<T: Serialize + DeserializeOwned + Send>() {}

fn sample() -> WriteBatch {
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    WriteBatch {
        workspace_id: WorkspaceId::default(),
        ops: vec![
            Op::UpsertNode {
                id: a.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: "alpha".into(),
                    summary: Some("a".into()),
                    tags: vec!["t".into()],
                    confidence: Some(0.5),
                    props: Some(json!({"k": [1, "two", null]})),
                    path_scope: vec!["src/**".into()],
                    ..Default::default()
                },
            },
            Op::UpsertEdge {
                src: a.clone(),
                dst: b.clone(),
                rel: "RELATES_TO".into(),
                props: Some(json!({"owner": "kaban-ingest"})),
            },
            Op::Tombstone {
                id: b.clone(),
                reason: "gone from source".into(),
            },
            Op::Reactivate { id: c.clone() },
        ],
        edge_diff: EdgeDiff::Owned {
            producer: Producer::DocumentIngest,
        },
        artifacts: vec![],
        gazetteer_claim_sets: vec![],
    }
}

#[test]
fn a_batch_with_every_op_variant_survives_json_and_back_equal() {
    assert_bounds::<WriteBatch>();
    assert_bounds::<Op>();
    let batch = sample();
    let text = serde_json::to_string(&batch).unwrap();
    let back: WriteBatch = serde_json::from_str(&text).unwrap();
    assert_eq!(back, batch);
    // the four variants are each tagged by `op` on the wire, in order
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let tags: Vec<&str> = v["ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["op"].as_str().unwrap())
        .collect();
    assert_eq!(
        tags,
        ["upsert_node", "upsert_edge", "tombstone", "reactivate"]
    );
    assert_eq!(
        v["edge_diff"],
        json!({"mode": "owned", "producer": "document_ingest"})
    );
    // the pre-PC-16 `owner` spelling is gone (no alias)
    assert!(
        serde_json::from_value::<EdgeDiff>(json!({"mode": "owned", "owner": "kaban-ingest"}))
            .is_err()
    );
}

/// A batch serialized before `assertions` and `edge_diff` existed (or by a producer that never
/// opts in) parses to the `WriteBatch::new` defaults, and a `new` batch leaves them off the wire.
#[test]
fn a_batch_without_assertions_or_edge_diff_parses_to_the_defaults() {
    let id = NodeId::new();
    let old = json!({
        "workspace_id": "default",
        "ops": [{"op": "reactivate", "id": id.as_str()}],
    });
    let back: WriteBatch = serde_json::from_value(old).unwrap();
    assert_eq!(
        back,
        WriteBatch::new(WorkspaceId::default(), vec![Op::Reactivate { id }])
    );
    let wire = serde_json::to_value(&back).unwrap();
    assert!(wire.get("assertions").is_none(), "{wire}");
    assert_eq!(wire["edge_diff"], json!({"mode": "none"}));
}

/// Negative: an op tag outside the four variants, or a node id that is not a `UUIDv7`, is
/// refused at parse time, never applied.
#[test]
fn an_unknown_op_tag_or_a_malformed_id_is_refused() {
    let id = NodeId::new();
    let unknown = json!({
        "workspace_id": "default",
        "ops": [{"op": "purge", "id": id.as_str()}],
    });
    let err = serde_json::from_value::<WriteBatch>(unknown).unwrap_err();
    assert!(err.to_string().contains("purge"), "{err}");
    let malformed = json!({
        "workspace_id": "default",
        "ops": [{"op": "reactivate", "id": "not-a-node-id"}],
    });
    assert!(serde_json::from_value::<WriteBatch>(malformed).is_err());
}
