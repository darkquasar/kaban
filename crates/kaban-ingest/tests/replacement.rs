//! Acceptance-safe structured revisions through the public submit surface.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::{
    Actor, Governance, GovernanceStore, IngestStore, MemoryStore, NewEvent, NodeId, WorkspaceId,
    WriteCtx,
};
use kaban_ingest::jobs::{brief::brief, submit::submit, types::IngestOpts};
use serde_json::json;

#[tokio::test]
async fn accepted_and_terminal_predecessors_are_immutable_and_revisions_repeat() {
    for (name, terminal) in [
        ("accepted", None),
        ("superseded", Some(Governance::Superseded)),
        ("invalidated", Some(Governance::Invalidated)),
    ] {
        let mut rig = common::real::rig("kaban-e39-revision", name);
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
        std::fs::write(
            rig.root.join("a.md"),
            "# Decision\n\nUse SQLite for a measured reason.\n",
        )
        .unwrap();
        let api = KabanApi::new(
            rig.store.clone(),
            kaban_policy::Policy::empty(),
            rig.store.signer(),
            None,
        );
        let prepared = brief(
            &api,
            &rig.root,
            "a.md",
            &IngestOpts {
                r#type: Some("adr".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let body = json!({"job":prepared.job,"records":[{"local_id":"stable","kind":"Decision","text":"Use SQLite","props":{"rationale":"measured reason"}}]});
        let first = submit(&api, &rig.root, body.clone()).await.unwrap();
        assert_eq!(first["created"], 1);
        let id = NodeId::parse(first["records"][0]["id"].as_str().unwrap()).unwrap();
        let ctx = WriteCtx::new("policy", WorkspaceId::default(), Actor::Agent);
        rig.store
            .transition(&id, Governance::Accepted, NewEvent::default(), &ctx)
            .unwrap();
        if let Some(governance) = terminal {
            rig.store
                .transition(&id, governance, NewEvent::default(), &ctx)
                .unwrap();
        }
        let before = serde_json::to_value(rig.store.get(&id, true).unwrap().unwrap()).unwrap();
        let events = rig.store.events(&id).unwrap();
        if terminal.is_none() {
            let unchanged = submit(&api, &rig.root, body.clone()).await.unwrap();
            assert_eq!(unchanged["unchanged"], 1);
        }
        let mut changed = body;
        changed["records"][0]["props"]["rationale"] = json!("different measured reason");
        let revision = submit(&api, &rig.root, changed.clone()).await.unwrap();
        assert_eq!(revision["created"], 1);
        assert_eq!(revision["records"][0]["outcome"], "revision_candidate");
        assert_eq!(revision["revision_candidates"][0]["of"], json!(id));
        assert_eq!(
            revision["predecessor_terminal"],
            usize::from(terminal.is_some())
        );
        let candidate = NodeId::parse(revision["records"][0]["id"].as_str().unwrap()).unwrap();
        assert_ne!(candidate, id);
        assert_eq!(
            rig.store
                .get(&candidate, false)
                .unwrap()
                .unwrap()
                .governance,
            Governance::Proposed
        );
        assert_eq!(
            serde_json::to_value(rig.store.get(&id, true).unwrap().unwrap()).unwrap(),
            before
        );
        assert_eq!(rig.store.events(&id).unwrap(), events);
        let edges = rig
            .store
            .traverse(&kaban_core::TraverseQuery {
                start_ids: vec![candidate.clone()],
                rels: vec!["RELATES_TO".into()],
                filters: kaban_core::ReadFilters {
                    include_candidates: true,
                },
                ..Default::default()
            })
            .unwrap()
            .edges;
        if terminal.is_none() {
            assert!(edges.iter().any(|edge| edge.src == candidate.as_str()
                && edge.dst == id.as_str()
                && edge.props["owner"] == "kaban-parse"
                && edge.props["role"] == "revision_candidate"));
        } else {
            assert!(
                edges.is_empty(),
                "hidden terminal predecessor remains excluded from traversal"
            );
            let mut keys = Vec::new();
            rig.store
                .rescan_assertions("terminal-proof", &mut |claim, _| {
                    if claim.value.as_value()["role"] == "revision_candidate" {
                        keys.push(claim.key.clone());
                    }
                    None
                })
                .unwrap();
            assert_eq!(keys.len(), 1);
            let explanation = api
                .explain(kaban_api::ExplainRequest {
                    key: keys[0].0.clone(),
                })
                .unwrap();
            assert_eq!(
                explanation.explanation.verdict,
                kaban_core::AssertionStatus::Winner,
                "terminal revision link remains a winning projection, not an automatic supersede"
            );
        }
        let last = rig.last_seq();
        let repeat = submit(&api, &rig.root, changed).await.unwrap();
        assert_eq!(repeat["unchanged"], 1);
        assert_eq!(rig.last_seq(), last);
    }
}
