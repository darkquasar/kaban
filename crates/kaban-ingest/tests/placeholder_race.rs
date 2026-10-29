//! The four-worker placeholder race (DI1 milestone review residual; DI2.0 (a)): eight documents
//! that all link to one missing target are prepared by four workers at once; the single
//! committer applies them in path order, and the deterministic placeholder id makes every
//! batch's `missing` Document the same row: one placeholder, eight edges into it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use common::real::doc_id;
use kaban_api::KabanApi;
use kaban_core::{IngestStore, MemoryStore, TraverseDirection, TraverseQuery, WorkspaceId};
use kaban_ingest::{ChunkConfig, DocumentPreparer, IngestConfig, Ingestor, WalkConfig};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::fs;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[test]
fn eight_documents_racing_on_four_workers_create_one_placeholder_for_a_shared_missing_target() {
    // Arrange: the real pipeline with four workers and a window of eight
    let root = Scratch::new("kaban-ingest-race", "placeholder");
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = Arc::new(KabanApi::new(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
    ));
    let cfg = IngestConfig {
        workers: 4,
        queue_depth: 8,
        ..IngestConfig::default()
    };
    let preparer = Arc::new(DocumentPreparer::new(
        &api,
        store.clone(),
        store.signer(),
        ChunkConfig::default(),
        cfg.clone(),
    ));
    let ingestor = Ingestor::new(api, store.clone(), store.signer(), preparer, cfg);
    for i in 0..8 {
        fs::write(
            root.join(format!("doc-{i}.md")),
            format!("# Doc {i}\n\nSee [[ghost]] for the shared target.\n"),
        )
        .unwrap();
    }
    let walk = WalkConfig::new(root.to_path_buf());

    // Act
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let report = rt
        .block_on(ingestor.run(&walk, CancellationToken::new()))
        .unwrap();

    // Assert: eight documents, one placeholder row, eight LINKS_TO edges into it
    assert_eq!((report.documents, report.failed), (8, 0), "{report:?}");
    let ws = WorkspaceId::default();
    let ghosts: Vec<_> = store
        .list_nodes("Document", ws.as_str(), 0, 500)
        .unwrap()
        .into_iter()
        .filter(|n| n.props["path"] == "ghost.md")
        .collect();
    assert_eq!(ghosts.len(), 1, "exactly one placeholder: {ghosts:?}");
    assert_eq!(ghosts[0].id, doc_id("ghost.md").to_string());
    assert_eq!(ghosts[0].props["status"], "missing");
    let r = store
        .traverse(&TraverseQuery {
            start_ids: vec![doc_id("ghost.md")],
            rels: vec!["LINKS_TO".to_owned()],
            direction: TraverseDirection::Both,
            max_depth: 1,
            node_kinds: Vec::new(),
            governance: Vec::new(),
            limit: 500,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    let incoming: Vec<_> = r
        .edges
        .iter()
        .filter(|e| e.rel == "LINKS_TO" && e.dst == doc_id("ghost.md").to_string())
        .collect();
    assert_eq!(incoming.len(), 8, "{incoming:?}");
}
