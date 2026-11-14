//! Commit stamps on write (engine-plan E2.0 item 5, the survivors): a record written without a
//! repository gets `commit_sha` NULL, and an ordinary `remember` never probes Git. The ontology
//! `index.stamp` knob, the registry's stamping set and the `unstamped` reason events went with
//! governance (T3).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{ApiConfig, GitRunner, KabanApi, RememberRequest};
use kaban_core::{MemoryStore, NodeId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::sync::Arc;

fn api_on(s: &Scratch, config: ApiConfig) -> (KabanApi, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = KabanApi::with_config(
        Arc::clone(&store) as Arc<dyn kaban_core::GraphStore>,
        Policy::empty(),
        store.signer(),
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
