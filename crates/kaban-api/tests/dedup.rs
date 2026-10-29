//! Dedup semantics, self-links and the `GOVERNS` endpoints (core-plan C1.8; m2 N5, N11;
//! engine-plan E1.5): a retried `remember` that hits an existing node still attaches the
//! requested links and reports which metadata it ignored; a node can never link to itself; a
//! `GOVERNS` edge starts at a governing kind.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, fact};
use kaban_api::{
    ApiError, KabanApi, LinkRequest, LinkSpec, RememberRequest, RememberStatus, TraverseRequest,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

fn api() -> KabanApi {
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    KabanApi::new(store, Policy::empty(), signer, None)
}

#[test]
fn retrying_a_remember_with_new_links_attaches_them_and_reports_ignored_fields() {
    let api = api();
    let target = api.remember(fact("the retry target note")).unwrap().id;
    let first = api
        .remember(RememberRequest {
            summary: Some("first summary".into()),
            ..fact("cache invalidation runs on deploy")
        })
        .unwrap();
    assert_eq!(first.status, RememberStatus::Created);
    assert_eq!(first.links_created, 0);

    // same kind + text, new links, different metadata
    let retry = api
        .remember(RememberRequest {
            summary: Some("a different summary".into()),
            tags: vec!["ops".into()],
            links: vec![LinkSpec {
                rel: "RELATES_TO".into(),
                to: Some(target.clone()),
                ..Default::default()
            }],
            ..fact("cache invalidation runs on deploy")
        })
        .unwrap();
    assert_eq!(retry.status, RememberStatus::Existing);
    assert_eq!(retry.id, first.id, "dedup returns the existing node");
    assert_eq!(retry.links_created, 1);
    assert!(!retry.links_ignored);
    assert!(
        retry.fields_ignored.contains(&"summary".to_string())
            && retry.fields_ignored.contains(&"tags".to_string()),
        "{:?}",
        retry.fields_ignored
    );
    // the link really exists
    let t = api
        .traverse(TraverseRequest {
            start_ids: vec![first.id.clone()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(t.edges.len(), 1, "{t:?}");
    assert_eq!(t.edges[0].to, target);
    // and the first write's metadata was kept, not overwritten
    let card = t.nodes.iter().find(|n| n.id == first.id).unwrap();
    assert_eq!(card.summary.as_deref(), Some("first summary"));
    // an identical retry ignores nothing
    let same = api
        .remember(RememberRequest {
            summary: Some("first summary".into()),
            ..fact("cache invalidation runs on deploy")
        })
        .unwrap();
    assert_eq!(same.status, RememberStatus::Existing);
    assert!(same.fields_ignored.is_empty(), "{:?}", same.fields_ignored);
}

#[test]
fn a_node_cannot_supersede_itself_and_stays_active() {
    let api = api();
    let a = api
        .remember(fact("decision: use blue green deploys"))
        .unwrap()
        .id;
    let err = api
        .link(LinkRequest {
            from: a.clone(),
            to: a.clone(),
            rel: "SUPERSEDES".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert!(
        matches!(&err, ApiError::Store(kaban_core::StoreError::SelfLink(id)) if *id == a),
        "{err}"
    );
    // still active: recall finds it, traverse returns it alone with no edges
    let found = api
        .recall(kaban_api::RecallRequest {
            query: "blue green deploys".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.results.len(), 1, "{found:?}");
    let t = api
        .traverse(TraverseRequest {
            start_ids: vec![a.clone()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(t.nodes.len(), 1);
    assert!(t.edges.is_empty());
    // an inline self-link on a retry is rejected the same way
    let err = api
        .remember(RememberRequest {
            links: vec![LinkSpec {
                rel: "SUPERSEDES".into(),
                to: Some(a.clone()),
                ..Default::default()
            }],
            ..fact("decision: use blue green deploys")
        })
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Store(kaban_core::StoreError::SelfLink(_))),
        "{err}"
    );
}

/// Review R1-8: `links_created` counts edges that were actually inserted. Re-attaching a link
/// that already exists reports 0 (the edge is refreshed, not created), and the same link named
/// twice in one request counts once.
#[test]
fn links_created_counts_inserted_edges_not_refreshed_ones() {
    let api = api();
    let target = api.remember(fact("target of the links")).unwrap().id;
    let link_to_target = || LinkSpec {
        rel: "RELATES_TO".into(),
        to: Some(target.clone()),
        ..Default::default()
    };
    // duplicates inside one request: one edge, counted once
    let first = api
        .remember(RememberRequest {
            links: vec![link_to_target(), link_to_target()],
            ..fact("note that links twice to the same target")
        })
        .unwrap();
    assert_eq!(first.links_created, 1, "{first:?}");
    assert_eq!(first.links, Some(1), "{first:?}");
    // a retry re-attaching the existing link creates nothing
    let retry = api
        .remember(RememberRequest {
            links: vec![link_to_target()],
            ..fact("note that links twice to the same target")
        })
        .unwrap();
    assert_eq!(retry.status, kaban_api::RememberStatus::Existing);
    assert_eq!(retry.links_created, 0, "{retry:?}");
    // the edge count did not grow
    let t = api
        .traverse(TraverseRequest {
            start_ids: vec![first.id.clone()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(t.edges.len(), 1, "{t:?}");
}

/// The `runbooks` fixture ontology loaded into the service's registry (engine-plan E1.5): the
/// scratch directory is the containment root, the file is copied beneath it.
fn with_runbooks(s: &Scratch, api: KabanApi) -> KabanApi {
    let registry = Arc::new(kaban_api::OntologyRegistry::new(Some(s.0.clone())).unwrap());
    let api = api.with_registry(registry);
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/runbooks.yaml");
    let target = s.0.join("runbooks.yaml");
    std::fs::copy(fixture, &target).unwrap();
    api.registry().load_file(&api, &target).unwrap();
    api
}

/// E1.5 (core-spec §4, engine-spec EN-F4): a `GOVERNS` edge starts at a governing kind
/// (`governs_paths: true` in the registry: the shipped Decision and Convention, a loaded
/// `Runbook`); from a kind that does not govern paths it is `EndpointNotAllowed`, and a source
/// in another workspace stays `NotFound` (no existence leak, R3-3) whatever its kind.
#[test]
fn a_governs_edge_starts_at_a_governing_kind_and_a_fact_is_refused() {
    // Arrange
    let s = Scratch::new("kaban-dedup", "governs");
    let api = with_runbooks(&s, api());
    let remember = |kind: &str, text: &str, ws: Option<&str>| -> String {
        api.remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            props: (kind == "Decision").then(|| serde_json::json!({"rationale": "r"})),
            workspace_id: ws.map(str::to_owned),
            ..Default::default()
        })
        .unwrap()
        .id
    };
    let code = remember("CodeRef", "src/queue.rs", None);
    let runbook = remember("runbooks:Runbook", "rotate the queue", None);
    let decision = remember("Decision", "the queue stays on sqlite", None);
    let fact = remember("Fact", "the queue is a table", None);
    let elsewhere = remember("Fact", "a fact of another workspace", Some("other"));
    let governs = |from: &str| LinkRequest {
        from: from.into(),
        rel: "GOVERNS".into(),
        to: code.clone(),
        confidence: None,
        props: None,
    };

    // Act
    let from_runbook = api.link(governs(&runbook));
    let from_decision = api.link(governs(&decision));
    let from_fact = api.link(governs(&fact));
    let from_elsewhere = api.link(governs(&elsewhere));

    // Assert
    assert_eq!(from_runbook.unwrap().rel, "GOVERNS");
    assert_eq!(from_decision.unwrap().rel, "GOVERNS");
    match from_fact {
        Err(ApiError::Store(kaban_core::StoreError::EndpointNotAllowed { rel, from, to })) => {
            assert_eq!(
                (rel.as_str(), from.as_str(), to.as_str()),
                ("GOVERNS", "Fact", "CodeRef")
            );
        }
        other => panic!("expected EndpointNotAllowed, got {other:?}"),
    }
    assert!(
        matches!(from_elsewhere, Err(ApiError::NotFound(_))),
        "{from_elsewhere:?}"
    );
}
