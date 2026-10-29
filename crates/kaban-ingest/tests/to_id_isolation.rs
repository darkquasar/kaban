//! Positive same-workspace links and indistinguishable foreign/unknown targets.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::{
    Actor, IngestStore, MemoryStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_ingest::jobs::{brief::brief, submit::submit, types::IngestOpts};
use serde_json::json;

#[tokio::test]
async fn links_resolve_only_active_same_workspace_nodes() {
    let mut rig = common::real::rig("kaban-e39-targets", "isolation");
    let mut ontology = kaban_core::shipped::resolved().unwrap();
    ontology
        .priors
        .insert("script.ingest_submit.edge".into(), 1.0);
    ontology
        .priors
        .insert("script.ingest_submit.kind_of_record".into(), 1.0);
    rig.store = std::sync::Arc::new(
        kaban_store_sqlite::SqliteStore::open_memory()
            .unwrap()
            .with_ontology(std::sync::Arc::new(ontology))
            .unwrap(),
    );
    std::fs::write(rig.root.join("a.md"), "# Source\n\nHarmless body.\n").unwrap();
    let api = KabanApi::new(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
    );
    let same = NodeId::new();
    let foreign = NodeId::new();
    let missing = NodeId::new();
    for (id, ws) in [
        (&same, WorkspaceId::default()),
        (&foreign, WorkspaceId::new("foreign")),
    ] {
        rig.store
            .apply_batch(
                WriteBatch::new(
                    ws.clone(),
                    vec![Op::UpsertNode {
                        id: id.clone(),
                        node: NewNode {
                            kind: "Fact".into(),
                            text: id.to_string(),
                            ..Default::default()
                        },
                    }],
                ),
                &WriteCtx::new("policy", ws, Actor::Agent),
            )
            .unwrap();
    }
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let result = submit(&api,&rig.root,json!({"job":prepared.job,"records":[{"local_id":"r","kind":"Fact","text":"Harmless","confidence":0.8}],"links":[
        {"from_local_id":"r","rel":"RELATES_TO","to_id":same},
        {"from_local_id":"r","rel":"RELATES_TO","to_id":foreign},
        {"from_local_id":"r","rel":"RELATES_TO","to_id":missing}
    ]})).await.unwrap();
    assert_eq!(result["skipped"][0]["reason"], "unknown_target");
    assert_eq!(result["skipped"][1]["reason"], "unknown_target");
    let id = NodeId::parse(result["records"][0]["id"].as_str().unwrap()).unwrap();
    let edges = rig
        .store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![id],
            rels: vec!["RELATES_TO".into()],
            filters: kaban_core::ReadFilters {
                include_candidates: true,
            },
            ..Default::default()
        })
        .unwrap()
        .edges;
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].dst, same.as_str());
    assert_eq!(edges[0].props["owner"], "kaban-parse");
    let mut scores = Vec::new();
    rig.store
        .rescan_assertions("confidence-proof", &mut |claim, _| {
            scores.push((
                claim.method,
                claim.rank,
                claim.raw_score,
                claim.lineage_id.clone(),
            ));
            Some(claim.value.as_value().clone())
        })
        .unwrap();
    assert_eq!(
        scores.len(),
        3,
        "record, provenance and valid link assertions only"
    );
    assert!(scores.iter().all(|(method, rank, score, _)| *method
        == kaban_core::Method::IngestSubmit
        && *rank == kaban_core::Rank::Normal
        && score.0 == 8000));
    assert!(
        scores
            .iter()
            .all(|(_, _, _, lineage)| lineage == &scores[0].3)
    );
}
