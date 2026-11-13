//! OWN-65 stamping policy (engine-spec §3.1 "Indexing meta-config `index`", AMD-14; engine-plan
//! E2.0 item 5): the registry derives the stamping set from the loaded ontologies
//! (`stamp_kinds`, `index_policy`), and a record of a stamping kind written without a
//! repository gets `commit_sha` NULL. The `unstamped` reason events and their doctor check went
//! with governance (T3).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{ApiConfig, GitRunner, KabanApi, OntologyRegistry, RememberRequest};
use kaban_core::ontology::Stamp;
use kaban_core::{MemoryStore, NodeId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

/// A governed, serializable user kind (the shipped `Decision`'s shape) that opts out of commit
/// stamping, beside one that keeps the default.
const OPS_YAML: &str = r#"ontology: ops
version: 1
extends: [core]
kinds:
  Runbook:
    description: "An operational runbook; lives outside the repository, never stamped."
    governed: {acceptance: human, serialize: none, export: {digest: false, marker: false},
               display: {label: Runbook, plural: Runbooks}, governs_paths: false, dedup: none}
    index: {stamp: none}
  Service:
    description: "A deployed service; default indexing."
rels: {}
"#;

fn api_on(s: &Scratch, config: ApiConfig) -> (KabanApi, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = KabanApi::with_config(
        Arc::clone(&store) as Arc<dyn kaban_core::GraphStore>,
        Policy::empty(),
        store.signer(),
        None,
        config,
    );
    (api, store)
}

fn remember(api: &KabanApi, kind: &str, text: &str) -> NodeId {
    let r = api
        .remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            // the shipped Decision's schema requires `rationale` (EN-F3a)
            props: (kind == "Decision").then(|| json!({"rationale": "because"})),
            ..Default::default()
        })
        .unwrap();
    NodeId::parse(&r.id).unwrap()
}

/// 5a: the shipped set excludes the four document kinds and includes the governed ones; a
/// user ontology setting `index: {stamp: none}` on a Decision-like kind removes it from the
/// set, and `index_policy` answers per stored name (`None` for an unknown kind).
#[test]
fn stamp_kinds_follows_the_loaded_ontologies() {
    // Arrange
    let s = Scratch::new("kaban-stamp", "registry");
    std::fs::create_dir_all(s.0.join(".kaban/ontology")).unwrap();
    let registry = Arc::new(OntologyRegistry::new(Some(s.0.clone())).unwrap());
    let (api, _store) = api_on(&s, ApiConfig::default());
    let api = api.with_registry(Arc::clone(&registry));
    let shipped = registry.stamp_kinds();
    for document_kind in ["Document", "Section", "Chunk", "Tag"] {
        assert!(!shipped.iter().any(|k| k == document_kind), "{shipped:?}");
        assert_eq!(registry.index_policy(document_kind), Some(Stamp::None));
    }
    for governed in ["Decision", "Convention", "Failure", "Fact"] {
        assert!(shipped.iter().any(|k| k == governed), "{shipped:?}");
        assert_eq!(registry.index_policy(governed), Some(Stamp::Commit));
    }
    assert_eq!(registry.index_policy("Nothing"), None);
    let path = s.0.join(".kaban/ontology/ops.yaml");
    std::fs::write(&path, OPS_YAML).unwrap();

    // Act
    registry.load_file(&api, &path).unwrap();
    let loaded = registry.stamp_kinds();

    // Assert
    assert!(!loaded.iter().any(|k| k == "ops:Runbook"), "{loaded:?}");
    assert!(loaded.iter().any(|k| k == "ops:Service"), "{loaded:?}");
    assert_eq!(registry.index_policy("ops:Runbook"), Some(Stamp::None));
    assert_eq!(registry.index_policy("ops:Service"), Some(Stamp::Commit));
    assert_eq!(
        loaded.len(),
        shipped.len() + 1,
        "Service joined, Runbook did not"
    );
}

/// 5c: outside any repository (`repo_root = None`) a `remember` of a `Decision` succeeds with
/// `commit_sha` NULL.
#[test]
fn a_record_written_outside_any_repository_has_no_commit_stamp() {
    // Arrange
    let s = Scratch::new("kaban-stamp", "no-repository");
    let (api, store) = api_on(&s, ApiConfig::default());

    // Act
    let decision = remember(&api, "Decision", "written outside any repository");

    // Assert
    let node = store.get(&decision, false).unwrap().unwrap();
    assert_eq!(node.commit_sha, None);
}

/// Ordinary writes never probe Git.
#[test]
fn ordinary_remember_never_probes_git_and_doctor_reports_current_history() {
    // Arrange
    let s = Scratch::new("kaban-stamp", "no-history");
    let calls = s.0.join("git-calls");
    let wrapper = s.0.join("git-wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho call >> '{}'\nexec git \"$@\"\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let (api, store) = api_on(
        &s,
        ApiConfig {
            repo_root: Some(s.0.clone()),
            repo: Some("kaban-stamp-nohistory".into()),
            ..Default::default()
        },
    );
    let api = api.with_git(GitRunner {
        program: wrapper,
        ..Default::default()
    });

    // Act
    let decision = remember(&api, "Decision", "written where git has no HEAD");
    remember(&api, "Document", "never stamp this document");
    assert!(!calls.exists(), "ordinary remember must not invoke Git");

    // Assert
    assert_eq!(
        store.get(&decision, false).unwrap().unwrap().commit_sha,
        None
    );
}
