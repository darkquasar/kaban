//! CLI-only machine maintenance retains request content gates and fails closed.
#![allow(clippy::unwrap_used)]

use kaban_api::{ApiError, KabanApi};
use kaban_core::Layer;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
mod common;

fn maintenance_identity_case(field: &str, action: &str) {
    use kaban_core::{
        Actor, AuditStore, IngestStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch, WriteCtx,
    };
    let scratch = common::Scratch::new("maintenance-identity", &format!("{field}-{action}"));
    let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let raw = "historicalcanary7781";
    let masked = "[REDACTED:identity-mask]";
    for (key, identity) in [("raw-target", raw), ("masked-target", masked)] {
        let id = NodeId::new();
        let batch = WriteBatch::new(
            WorkspaceId::default(),
            vec![Op::UpsertNode {
                id: id.clone(),
                node: NewNode {
                    kind: "Document".into(),
                    text: "Historical document".into(),
                    props: Some(
                        serde_json::json!({"owner":"kaban-ingest", "source_id":key, "path":identity, "chunk_ids":[]}),
                    ),
                    ..Default::default()
                },
            }],
        );
        store
            .apply_batch(
                batch,
                &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
            )
            .unwrap();
        let producer = if field == "producer" {
            identity
        } else {
            "gazetteer@1"
        };
        conn.execute("INSERT INTO derived_artifact SELECT ?1,workspace_id,'gazetteer',?2,rid,json_object('content_hash',content_hash,'rev',rev),'fixture',policy_version,0 FROM node WHERE id=?3", rusqlite::params![key, producer, id.as_str()]).unwrap();
    }
    let policy_text = if action == "clean" {
        String::new()
    } else {
        format!("words:\n  - {{term: {raw}, action: {action}, id: identity-mask}}\n")
    };
    let policy = Policy::from_file(
        Policy::parse(policy_text.as_bytes()).unwrap(),
        "maintenance-policy".into(),
    )
    .unwrap();
    let api = KabanApi::new(store.clone(), policy, store.signer(), None);
    let outcome = if field == "producer" {
        api.retire_producer(Layer::Gazetteer, raw)
    } else {
        api.forget_layer(Layer::Gazetteer, Some(raw))
    };
    if action == "clean" {
        assert_eq!(outcome.unwrap().report.rows, 1);
    } else {
        assert!(
            outcome.is_err(),
            "a changed or rejected maintenance identity must refuse before destructive lookup: {outcome:?}"
        );
        let error = outcome.unwrap_err().to_string();
        assert!(
            !error.contains(raw) && !error.contains(masked),
            "refusal never echoes either identity"
        );
    }
    let remaining: i64 = conn
        .query_row("SELECT count(*) FROM derived_artifact", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        remaining,
        if action == "clean" { 1 } else { 2 },
        "neither raw nor masked alternate may be mutated on refusal"
    );
    let op = if action == "clean" { "forget" } else { action };
    let audits: i64 = conn
        .query_row("SELECT count(*) FROM audit WHERE op=?1", [op], |r| r.get(0))
        .unwrap();
    assert!(audits > 0, "maintenance verdict must persist its audit");
    if action == "redact" {
        let verdicts: i64 = conn
            .query_row(
                "SELECT count(*) FROM audit WHERE op='redact' AND field=?1",
                [field],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(verdicts, 1);
    }
    assert!(store.audit_verify().unwrap().ok);
}

#[test]
fn maintenance_producer_identity_redaction_refuses_destructive_lookup() {
    maintenance_identity_case("producer", "redact");
}

#[test]
fn maintenance_document_identity_redaction_refuses_destructive_lookup() {
    maintenance_identity_case("document", "redact");
}

#[test]
fn maintenance_clean_identities_mutate_and_reject_identities_preserve_targets() {
    for field in ["producer", "document"] {
        for action in ["clean", "reject"] {
            maintenance_identity_case(field, action);
        }
    }
}

#[test]
fn layer_maintenance_gates_document_before_lookup_and_refuses_unavailable_layers() {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let api = KabanApi::new(store.clone(), policy, store.signer(), None);
    assert_eq!(api.reindex_layer(Layer::Gazetteer).unwrap().report.rows, 0);
    assert_eq!(
        api.retire_producer(Layer::Gazetteer, "gazetteer@1")
            .unwrap()
            .report
            .refused,
        Some(kaban_core::MaintenanceRefusal::CurrentProducer)
    );
    assert!(matches!(
        api.retire_producer(Layer::Gazetteer, "AKIAQWERTYUIOPASDFGH"),
        Err(ApiError::Rejected { .. })
    ));
    assert!(
        matches!(
            api.forget_layer(Layer::Gazetteer, Some("AKIAQWERTYUIOPASDFGH")),
            Err(ApiError::Rejected { .. })
        ),
        "path CONTENT rejection must precede unknown-document lookup"
    );
    assert!(matches!(
        api.reindex_layer(Layer::Human),
        Err(ApiError::Invalid(_))
    ));
    assert!(matches!(
        api.reindex_layer(Layer::Embed),
        Err(ApiError::Invalid(_))
    ));
}
