//! OWN-65 stamping policy (engine-spec §3.1 "Indexing meta-config `index`", AMD-14; engine-plan
//! E2.0 item 5): the registry derives the stamping set from the loaded ontologies
//! (`stamp_kinds`, `index_policy`), `post-commit` stamps only that set, and a record of a
//! stamping kind written without a repository gets `commit_sha` NULL plus a known
//! `no_repository` event. Doctor separates historical reasons from current stamp eligibility.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{
    ApiConfig, DoctorContext, GitRunner, HookEvent, HookHost, KabanApi, OntologyRegistry,
    RememberRequest,
};
use kaban_core::ontology::Stamp;
use kaban_core::{
    Actor, DatabaseFileModes, ForgetMode, GovernanceStore, MemoryStore, NodeId, SyncStore,
    UnstampedReason, WorkspaceId, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use serde_json::json;
use std::path::Path;
use std::process::Command;
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

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_COMMITTER_DATE", "2026-10-26T08:16:02Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with one commit, so `git rev-parse HEAD` answers.
fn git_init_with_history(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "user.name", "fixture"]);
    std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial"]);
}

fn doctor_context(s: &Scratch) -> DoctorContext {
    DoctorContext {
        files: DatabaseFileModes {
            db: DatabaseFileModes::OWNER_ONLY,
            wal: None,
            shm: None,
        },
        schema_version: LATEST_SCHEMA_VERSION,
        keys_dir: s.keys(),
        vacuum: false,
        priors: false,
    }
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

/// 5b: `post-commit` passes the registry's stamping set: a `Document` beside a committed
/// `Decision` stays unstamped, and a user kind with `index: {stamp: none}` is left alone while
/// its default-indexed neighbour is stamped. Inside a repository with history no reason row is
/// written.
#[test]
fn post_commit_stamps_only_the_stamping_kinds() {
    // Arrange
    let s = Scratch::new("kaban-stamp", "post-commit");
    std::fs::create_dir_all(s.0.join(".kaban/ontology")).unwrap();
    git_init_with_history(&s.0);
    let registry = Arc::new(OntologyRegistry::new(Some(s.0.clone())).unwrap());
    let (api, store) = api_on(
        &s,
        ApiConfig {
            repo_root: Some(s.0.clone()),
            repo: Some("kaban-stamp".into()),
            ..Default::default()
        },
    );
    let api = api.with_registry(Arc::clone(&registry));
    let path = s.0.join(".kaban/ontology/ops.yaml");
    std::fs::write(&path, OPS_YAML).unwrap();
    registry.load_file(&api, &path).unwrap();
    let decision = remember(&api, "Decision", "stamped per commit");
    let document = remember(&api, "Document", "ingested document, never stamped");
    let runbook = remember(&api, "ops:Runbook", "outside the repository by policy");
    let service = remember(&api, "ops:Service", "default indexing");

    // Act
    let outcome = api.hook(HookHost::ClaudeCode, HookEvent::PostCommit, &json!({}));

    // Assert
    assert_eq!(outcome.output, json!({"stamped": 2}), "{outcome:?}");
    let sha_of = |id: &NodeId| store.get(id, false).unwrap().unwrap().commit_sha;
    assert!(sha_of(&decision).is_some());
    assert!(sha_of(&service).is_some());
    assert_eq!(
        sha_of(&document),
        None,
        "Document is `stamp: none` in the shipped file"
    );
    assert_eq!(
        sha_of(&runbook),
        None,
        "the user ontology turned stamping off"
    );
    for id in [&decision, &document, &runbook, &service] {
        assert!(
            store.events(id).unwrap().is_empty(),
            "inside a repository with history no reason is recorded"
        );
    }
}

/// 5c: outside any repository (`repo_root = None`) a `remember` of a `Decision` succeeds with
/// `commit_sha` NULL and one `unstamped`/`no_repository` events row; a `Document` in the same
/// situation gets no reason row; `doctor` lists the counts per reason.
#[test]
fn a_record_written_outside_any_repository_gets_a_no_repository_reason_row() {
    // Arrange
    let s = Scratch::new("kaban-stamp", "no-repository");
    let (api, store) = api_on(&s, ApiConfig::default());

    // Act
    let decision = remember(&api, "Decision", "written outside any repository");
    let document = remember(&api, "Document", "a document outside any repository");
    let doctor = api.doctor(&doctor_context(&s));

    // Assert
    let node = store.get(&decision, false).unwrap().unwrap();
    assert_eq!(node.commit_sha, None);
    let events = store.events(&decision).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        (events[0].kind.as_str(), events[0].reason.as_str()),
        ("unstamped", "no_repository")
    );
    assert!(
        store.events(&document).unwrap().is_empty(),
        "no reason row for a `stamp: none` kind"
    );
    let check = doctor
        .checks
        .iter()
        .find(|c| c.name == "unstamped")
        .unwrap_or_else(|| panic!("{doctor:?}"));
    assert!(check.ok, "{check:?}");
    assert_eq!(
        check.detail,
        "historical no_repository: 1, no_history: 0; current eligible: 1; status: no_repository"
    );
}

/// Ordinary writes never probe Git; doctor observes changing HEAD availability without
/// inventing historical reasons, and the hook still stamps eligible rows.
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
    let doctor = api.doctor(&doctor_context(&s));

    // Assert
    let events = store.events(&decision).unwrap();
    assert!(
        events.is_empty(),
        "HEAD availability is not a historical fact"
    );
    let check = doctor
        .checks
        .iter()
        .find(|c| c.name == "unstamped")
        .unwrap();
    let current = |report: kaban_api::DoctorReport| {
        report
            .checks
            .into_iter()
            .find(|c| c.name == "unstamped")
            .unwrap()
            .detail
    };
    assert_eq!(
        check.detail,
        "historical no_repository: 0, no_history: 0; current eligible: 1; status: unknown"
    );
    git(&s.0, &["init", "-q", "-b", "main"]);
    assert_eq!(
        current(api.doctor(&doctor_context(&s))),
        "historical no_repository: 0, no_history: 0; current eligible: 1; status: unknown"
    );
    git_init_with_history(&s.0);
    assert_eq!(
        current(api.doctor(&doctor_context(&s))),
        "historical no_repository: 0, no_history: 0; current eligible: 1; status: pending"
    );
    assert_eq!(
        api.hook(HookHost::ClaudeCode, HookEvent::PostCommit, &json!({}))
            .output,
        json!({"stamped": 1})
    );
    assert_eq!(
        current(api.doctor(&doctor_context(&s))),
        "historical no_repository: 0, no_history: 0; current eligible: 0; status: pending"
    );
    assert!(
        store.events(&decision).unwrap().is_empty(),
        "doctor must not manufacture history"
    );
}

#[test]
fn doctor_scopes_current_rows_but_preserves_historical_reasons() {
    let s = Scratch::new("kaban-stamp", "diagnostic-scope");
    let (api, store) = api_on(&s, ApiConfig::default());
    let kept = remember(&api, "Decision", "eligible unbound note");
    let purged = remember(&api, "Decision", "purged note");
    remember(&api, "Document", "excluded by policy");
    let ctx = WriteCtx::new("pv", WorkspaceId::new("default"), Actor::Agent);
    store
        .record_unstamped(&kept, "default", UnstampedReason::NoHistory)
        .unwrap();
    store.forget(&purged, ForgetMode::Purge, &ctx).unwrap();
    let (foreign, _) = api_on(
        &s,
        ApiConfig {
            repo: Some("other-repo".into()),
            ..Default::default()
        },
    );
    remember(&foreign, "Decision", "another repository");
    let (other, _) = api_on(
        &s,
        ApiConfig {
            workspace: WorkspaceId::new("other"),
            ..Default::default()
        },
    );
    remember(&other, "Decision", "another workspace");

    let report = api.doctor(&doctor_context(&s));
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "unstamped")
        .unwrap();
    assert_eq!(
        check.detail,
        "historical no_repository: 3, no_history: 1; current eligible: 1; status: no_repository"
    );
    assert_eq!(store.unstamped_counts("default").unwrap().no_history, 1);
}
