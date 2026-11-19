//! The extractor trust rule (spec 11 PC-20 item 5): the per-rel `governed_semantics` flag is
//! gone and the structured-submission gate always refuses a lifecycle-affecting rel in
//! extractor output: `SUPERSEDES`, `INVALIDATES`, `CONTRADICTS`, and any loaded rel whose
//! `on_link` archives its target. Every other declared rel passes the gate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_api::{ApiConfig, ApiError, KabanApi, OntologyRegistry};
use kaban_core::{NodeId, Op};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

fn edge(rel: &str) -> Op {
    Op::UpsertEdge {
        src: NodeId::new(),
        dst: NodeId::new(),
        rel: rel.into(),
        props: None,
    }
}

#[test]
fn the_submit_gate_refuses_every_lifecycle_affecting_rel() {
    // Arrange: a service holding the `incidents` ontology (`incidents:REPLACES` archives)
    let scratch = common::Scratch::new("kaban-extractor-trust", "gate");
    std::fs::create_dir_all(scratch.0.join(".kaban/ontology")).unwrap();
    let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
    let registry = Arc::new(OntologyRegistry::new(Some(scratch.0.clone())).unwrap());
    let api = KabanApi::with_config(
        Arc::clone(&store) as Arc<dyn kaban_core::GraphStore>,
        Policy::empty(),
        store.signer(),
        ApiConfig::default(),
    )
    .with_registry(Arc::clone(&registry));
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/incidents.yaml");
    let path = scratch.0.join(".kaban/ontology/incidents.yaml");
    std::fs::copy(fixture, &path).unwrap();
    registry.load_file(&api, &path).unwrap();

    // Act + Assert
    let mut gate = api.begin_ingest_submit();
    for rel in [
        "SUPERSEDES",
        "INVALIDATES",
        "CONTRADICTS",
        "incidents:REPLACES",
    ] {
        match gate.edge(edge(rel)) {
            Err(ApiError::Invalid(text)) => assert_eq!(text, "edge_not_allowed", "{rel}"),
            other => panic!("{rel}: {other:?}"),
        }
    }
    for rel in ["RELATES_TO", "incidents:MENTIONED_IN"] {
        assert!(gate.edge(edge(rel)).is_ok(), "{rel}");
    }
}
