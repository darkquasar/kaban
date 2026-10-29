//! `why` with a `symbol` target over `node_anchor` (engine-spec EN-F17, EN-F18, EN-F18a;
//! CG-JOIN v1 §3, §5, §6, §11; engine-plan E2.6): the seven G1-F11 names, the grammar negatives
//! of SO-13, `why_symbol_too_new`, the SD-5 identity and SD-6 byte-equality slices, the
//! `policy: stale` projection and the plan :1071 golden parity over a view of the former column
//! design (E2.6b). The snapshot is the §10 fixture builder's file behind a `FixedSnapshot` (or
//! a `DiscoverProvider` where `exact: false` is the point); the head comes from a one-commit
//! scripted repository. Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/cg_fixture.rs"]
mod cg_fixture;
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use cg_fixture::{AFERO_COMMIT, AFERO_KEY, AFERO_PATH, OTHER_COMMIT, REPO_ID, Variant};
use common::Scratch;
use kaban_api::{
    AcceptRequest, ApiConfig, ApiError, Caller, CodeField, CodeSnapshot, KabanApi, LinkRequest,
    MatchedBy, RememberRequest, WhyCard, WhyRequest, WhyTarget,
};
use kaban_codegraph::testing::FixedSnapshot;
use kaban_codegraph::{DiscoverProvider, SnapshotProvider, symbol};
use kaban_core::{
    Actor, Governance, MemoryStore, NewNode, NodeId, RememberOutcome, WorkspaceId, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};

/// The process repository identity (`ApiConfig.repo` = `CodeRef.repo`), deliberately not the
/// fixture's keyed directory name (SD-5).
const REPO: &str = "kaban";

/// An AWS access key id shaped canary (the builtin `aws-access-key-id` rule), assembled at run
/// time so that no credential-shaped literal sits in the source.
fn canary() -> String {
    format!("{}{}", "AKIA", "QWERTYUIOPASDFGH")
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn rev_parse(root: &Path, what: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", what])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A repository with `commits` commits; `(root, head, ancestors nearest first)`.
fn repo(dir: &Path, commits: usize) -> (PathBuf, String, Vec<String>) {
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    let mut shas = Vec::new();
    for i in 0..commits {
        std::fs::write(root.join("a.txt"), format!("{i}")).unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", &format!("c{i}")]);
        shas.push(rev_parse(&root, "HEAD"));
    }
    let head = shas.pop().unwrap();
    shas.reverse();
    (root, head, shas)
}

/// The shipped builtin policy (the AWS key rule is live).
fn builtin_policy() -> Policy {
    Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap()
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

struct Fixture {
    s: Scratch,
    store: Arc<SqliteStore>,
    api: KabanApi,
}

impl Fixture {
    /// A service bound to [`REPO`] over a fresh store, with the scripted repository and
    /// `provider` when given.
    fn new(name: &str, provider: Option<Arc<dyn SnapshotProvider>>) -> Self {
        let s = Scratch::new("e26-why-symbol", name);
        let (root, _, _) = repo(&s.0, 1);
        let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
        let api = Self::service(&s, &store, builtin_policy(), &root, provider);
        Self { s, store, api }
    }

    fn service(
        s: &Scratch,
        store: &Arc<SqliteStore>,
        policy: Policy,
        root: &Path,
        provider: Option<Arc<dyn SnapshotProvider>>,
    ) -> KabanApi {
        let mut api = KabanApi::with_config(
            store.clone(),
            policy,
            store.signer(),
            None,
            ApiConfig {
                repo: Some(REPO.into()),
                repo_root: Some(root.to_path_buf()),
                ..Default::default()
            },
        );
        if let Some(provider) = provider {
            api = api.with_snapshot_provider(s.0.join("codegraph").join(REPO_ID), provider);
        }
        api
    }

    /// The fixture file of `variant` under `commit`, admitted by a `FixedSnapshot`, with
    /// `policy_version` equal to the live one unless `stale`.
    fn fixed(
        s: &Scratch,
        variant: Variant,
        commit: &str,
        policy_version: &str,
    ) -> Arc<dyn SnapshotProvider> {
        let built = cg_fixture::build_with_policy(&s.0, variant, commit, policy_version);
        Arc::new(FixedSnapshot::new(&built.path))
    }

    /// A proposed Decision.
    fn decision(&self, text: &str) -> String {
        self.api
            .remember(RememberRequest {
                kind: "Decision".into(),
                text: text.into(),
                props: Some(json!({"rationale": "because"})),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    /// Accept `id` with `scope` (a human action).
    fn accept(&self, id: &str, scope: &[&str]) {
        self.api
            .accept(
                AcceptRequest {
                    id: id.into(),
                    reason: Some("reviewed".into()),
                    path_scope: scope.iter().map(|s| (*s).to_owned()).collect(),
                    ..Default::default()
                },
                &human(),
            )
            .unwrap();
    }

    /// A `CodeRef` of this process's repository through the public API (`node.repo` is
    /// `ApiConfig.repo`); `scope` is `props.snapshot_commit`.
    fn coderef(&self, key: &str, scope: Option<&str>) -> String {
        self.api
            .remember(RememberRequest {
                kind: "CodeRef".into(),
                text: format!("{REPO} {AFERO_PATH} {key}"),
                props: Some(coderef_props(key, scope)),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    /// A `CodeRef` written at the store seam with an explicit `repo` and workspace (the
    /// cross-repository and cross-workspace rows the API would never write for this process),
    /// unscoped so that only the predicate under test excludes it.
    fn foreign_coderef(&self, key: &str, repo: &str, workspace: &str) -> String {
        let ctx = WriteCtx::new("pv", WorkspaceId::new(workspace), Actor::Agent)
            .with_governed(self.api.governed());
        match self
            .store
            .remember(
                NewNode {
                    kind: "CodeRef".into(),
                    text: format!("{repo} {workspace} {key}"),
                    props: Some(coderef_props(key, None)),
                    repo: Some(repo.into()),
                    workspace_id: Some(workspace.into()),
                    ..Default::default()
                },
                &[],
                &ctx,
            )
            .unwrap()
        {
            RememberOutcome::Created(n) => n.id,
            RememberOutcome::Existing { node, .. } => node.id,
        }
    }

    fn link(&self, from: &str, rel: &str, to: &str) {
        self.api
            .link(LinkRequest {
                from: from.into(),
                rel: rel.into(),
                to: to.into(),
                confidence: None,
                props: None,
            })
            .unwrap();
    }

    fn why(&self, key: &str) -> Vec<WhyCard> {
        self.why_in(key, None)
    }

    fn why_in(&self, key: &str, workspace_id: Option<&str>) -> Vec<WhyCard> {
        self.api
            .why(WhyRequest {
                targets: vec![WhyTarget::Symbol(key.into())],
                workspace_id: workspace_id.map(str::to_owned),
                ..Default::default()
            })
            .unwrap()
            .results
    }
}

fn coderef_props(key: &str, scope: Option<&str>) -> Value {
    let mut props = json!({
        "repo": REPO, "commit": AFERO_COMMIT, "tool": "fixture",
        "relative_path": AFERO_PATH, "name_path": "MemMapFs/Create", "kind": "Method", "line": 7,
        "symbol": key,
    });
    if let Some(scope) = scope {
        props["snapshot_commit"] = Value::String(scope.into());
    }
    props
}

fn ids(cards: &[WhyCard]) -> Vec<&str> {
    cards.iter().map(|c| c.node.id.as_str()).collect()
}

fn current(commit: &str) -> CodeSnapshot {
    CodeSnapshot {
        commit: commit.into(),
        exact: true,
        policy: "current".into(),
    }
}

/// G1-F11 `why_symbol_exact_match`: the record linked `GOVERNS` to the `CodeRef` with the key
/// comes back with `matched_by: symbol` and `code.snapshot = {commit, exact: true, policy:
/// current}` (the fixture's `policy_version` is the live one); another service over the same
/// store with another policy sees `policy: stale`.
#[test]
fn why_symbol_exact_match() {
    let s = Scratch::new("e26-why-symbol", "exact");
    let (root, _, _) = repo(&s.0, 1);
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let probe = Fixture::service(&s, &store, builtin_policy(), &root, None);
    let live = probe.policy_version();
    let provider = Fixture::fixed(&s, Variant::Afero, AFERO_COMMIT, &live);
    let api = Fixture::service(&s, &store, builtin_policy(), &root, Some(provider.clone()));
    let f = Fixture { s, store, api };

    let coderef = f.coderef(AFERO_KEY, Some(AFERO_COMMIT));
    let governing = f.decision("MemMapFs.Create must stay allocation-free");
    f.accept(&governing, &["src/**"]); // a scope that does not cover the defining file
    f.link(&governing, "GOVERNS", &coderef);
    let elsewhere = f.decision("the cli prints json");
    f.accept(&elsewhere, &["src/cli/**"]);

    let cards = f.why(AFERO_KEY);
    assert_eq!(ids(&cards), vec![governing.as_str()], "{cards:?}");
    let card = &cards[0];
    assert_eq!(card.matched_by, MatchedBy::Symbol);
    assert_eq!(card.matched_target, AFERO_KEY);
    assert_eq!(card.governance, Governance::Accepted);
    // negative (OWN-83 (1)): a resolved lookup never carries `anchor_commit`
    assert_eq!(card.anchor_commit, None, "{card:?}");
    assert!(
        serde_json::to_value(card)
            .unwrap()
            .get("anchor_commit")
            .is_none(),
        "{card:?}"
    );
    assert_eq!(card.code, Some(CodeField::Snapshot(current(AFERO_COMMIT))));
    let wire = serde_json::to_value(card).unwrap();
    assert_eq!(
        wire["code"],
        json!({"snapshot": {"commit": AFERO_COMMIT, "exact": true, "policy": "current"}}),
        "the contract §6 wire shape"
    );

    // a policy bump: the same store and repository under another live policy version (a user
    // rule changes the effective hash; the builtin set alone is what `Policy::empty()` hashes)
    let with_rule = Policy::from_file(
        Policy::parse(b"rules:\n  - id: numbat-reject\n    regex: numbats\n    action: reject\n")
            .unwrap(),
        "bumped".into(),
    )
    .unwrap();
    let bumped = Fixture::service(&f.s, &f.store, with_rule, &root, Some(provider));
    assert_ne!(
        bumped.policy_version(),
        live,
        "the fixture needs two distinct versions"
    );
    let cards = bumped
        .why(WhyRequest {
            targets: vec![WhyTarget::Symbol(AFERO_KEY.into())],
            ..Default::default()
        })
        .unwrap()
        .results;
    assert_eq!(
        cards[0].code,
        Some(CodeField::Snapshot(CodeSnapshot {
            commit: AFERO_COMMIT.into(),
            exact: true,
            policy: "stale".into(),
        }))
    );
    // a path target carries no `code` field at all
    let path_cards = f
        .api
        .why(WhyRequest {
            targets: vec![WhyTarget::Path("src/a.rs".into())],
            ..Default::default()
        })
        .unwrap()
        .results;
    assert!(path_cards.iter().all(|c| c.code.is_none()));
    assert!(
        serde_json::to_value(&path_cards[0])
            .unwrap()
            .get("code")
            .is_none()
    );
}

/// SD-5: the join compares `node_anchor.repo` with `ApiConfig.repo`; the fixture's keyed
/// `<repo-id>` directory name is something else and does not matter.
#[test]
fn why_symbol_identity_is_config_repo_not_repo_id() {
    let s = Scratch::new("e26-why-symbol", "identity");
    let provider = Fixture::fixed(&s, Variant::Afero, AFERO_COMMIT, cg_fixture::POLICY_VERSION);
    let (root, _, _) = repo(&s.0, 1);
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = Fixture::service(&s, &store, builtin_policy(), &root, Some(provider));
    let f = Fixture { s, store, api };
    assert_ne!(REPO_ID, REPO);
    let coderef = f.coderef(AFERO_KEY, Some(AFERO_COMMIT));
    let d = f.decision("identity is the process repository string");
    f.link(&d, "ABOUT", &coderef);
    let cards = f.why(AFERO_KEY);
    assert_eq!(ids(&cards), vec![d.as_str()]);
    assert_eq!(cards[0].matched_by, MatchedBy::Symbol);
    assert_eq!(cards[0].governance, Governance::Proposed);
}

/// G1-F11 `why_symbol_path_scope_fallback`: a record whose `path_scope` governs the key's
/// defining file in the resolved snapshot comes back with `matched_by: path_scope` even without
/// any `CodeRef`; an anchored record ranks before it and is never returned twice.
#[test]
fn why_symbol_path_scope_fallback() {
    let s = Scratch::new("e26-why-symbol", "fallback");
    let provider = Fixture::fixed(&s, Variant::Afero, AFERO_COMMIT, cg_fixture::POLICY_VERSION);
    let (root, _, _) = repo(&s.0, 1);
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = Fixture::service(&s, &store, builtin_policy(), &root, Some(provider));
    let f = Fixture { s, store, api };

    let by_scope = f.decision("go files keep the error wrapping convention");
    f.accept(&by_scope, &["*.go"]);
    let cards = f.why(AFERO_KEY);
    assert_eq!(ids(&cards), vec![by_scope.as_str()], "{cards:?}");
    assert_eq!(cards[0].matched_by, MatchedBy::PathScope);
    assert_eq!(cards[0].matched_target, AFERO_KEY);
    assert!(matches!(cards[0].code, Some(CodeField::Snapshot(_))));

    // anchored first, the fallback second; a record in both legs appears once as `symbol`
    let coderef = f.coderef(AFERO_KEY, None);
    let anchored = f.decision("MemMapFs.Create returns the opened handle");
    f.accept(&anchored, &["docs/**"]);
    f.link(&anchored, "GOVERNS", &coderef);
    f.link(&by_scope, "ABOUT", &coderef);
    let cards = f.why(AFERO_KEY);
    assert_eq!(
        ids(&cards),
        vec![anchored.as_str(), by_scope.as_str()],
        "{cards:?}"
    );
    assert!(cards.iter().all(|c| c.matched_by == MatchedBy::Symbol));
}

/// G1-F11 `why_symbol_other_repo_excluded`: the same key in two repositories returns only the
/// current one (`node_anchor.repo = ApiConfig.repo`).
#[test]
fn why_symbol_other_repo_excluded() {
    let f = Fixture::new("other-repo", None);
    let mine = f.coderef(AFERO_KEY, None);
    let d_mine = f.decision("ours");
    f.link(&d_mine, "ABOUT", &mine);
    let theirs = f.foreign_coderef(AFERO_KEY, "other-repo", WorkspaceId::default().as_str());
    let d_theirs = f.decision("theirs");
    f.link(&d_theirs, "ABOUT", &theirs);
    let cards = f.why(AFERO_KEY);
    assert_eq!(ids(&cards), vec![d_mine.as_str()], "{cards:?}");
}

/// G1-F11 `why_symbol_other_workspace_excluded`: an anchor row of another workspace never
/// joins a bound read (S6); the explicit `"*"` selector reads every workspace.
#[test]
fn why_symbol_other_workspace_excluded() {
    let f = Fixture::new("other-workspace", None);
    let mine = f.coderef(AFERO_KEY, None);
    let d_mine = f.decision("ours");
    f.link(&d_mine, "ABOUT", &mine);
    let theirs = f.foreign_coderef(AFERO_KEY, REPO, "ws2");
    let d_theirs = f
        .api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "theirs".into(),
            props: Some(json!({"rationale": "because"})),
            workspace_id: Some("ws2".into()),
            ..Default::default()
        })
        .unwrap()
        .id;
    let ctx =
        WriteCtx::new("pv", WorkspaceId::new("ws2"), Actor::Agent).with_governed(f.api.governed());
    f.store
        .link(
            &NodeId::parse(&d_theirs).unwrap(),
            &NodeId::parse(&theirs).unwrap(),
            "ABOUT",
            None,
            None,
            &ctx,
        )
        .unwrap();
    let cards = f.why(AFERO_KEY);
    assert_eq!(ids(&cards), vec![d_mine.as_str()], "{cards:?}");
    let all = f.why_in(AFERO_KEY, Some("*"));
    let mut got = ids(&all);
    got.sort_unstable();
    let mut want = vec![d_mine.as_str(), d_theirs.as_str()];
    want.sort_unstable();
    assert_eq!(got, want, "{all:?}");
}

/// G1-F11 `why_symbol_without_snapshot`: no provider is `code: snapshot_absent`, never an
/// error; the anchor leg runs with the scope predicate dropped (every scope matches, cg-join
/// §3; OWN-83 (1), E2.7b), the card reached through a commit-scoped row carries that commit as
/// `anchor_commit` (the signal of the missing index), the one through an unscoped row does
/// not, and nothing is attached.
#[test]
fn why_symbol_without_snapshot() {
    let f = Fixture::new("absent", None);
    let unscoped = f.coderef(AFERO_KEY, None);
    let d_unscoped = f.decision("unscoped pointer");
    f.link(&d_unscoped, "ABOUT", &unscoped);
    // a second CodeRef under the same key, scoped to a commit (another node: the text differs)
    let scoped = f
        .api
        .remember(RememberRequest {
            kind: "CodeRef".into(),
            text: "scoped pointer".into(),
            props: Some(coderef_props(AFERO_KEY, Some(AFERO_COMMIT))),
            ..Default::default()
        })
        .unwrap()
        .id;
    let d_scoped = f.decision("scoped pointer's record");
    f.link(&d_scoped, "ABOUT", &scoped);

    let cards = f.why(AFERO_KEY);
    let mut got = ids(&cards);
    got.sort_unstable();
    let mut want = vec![d_unscoped.as_str(), d_scoped.as_str()];
    want.sort_unstable();
    assert_eq!(got, want, "{cards:?}");
    for card in &cards {
        assert_eq!(card.code, Some(CodeField::SnapshotAbsent));
        let wire = serde_json::to_value(card).unwrap();
        assert_eq!(wire["code"], json!("snapshot_absent"));
        if card.node.id == d_scoped {
            // positive: the commit the anchor row was scoped to, on the wire
            assert_eq!(
                card.anchor_commit.as_deref(),
                Some(AFERO_COMMIT),
                "{card:?}"
            );
            assert_eq!(wire["anchor_commit"], json!(AFERO_COMMIT));
        } else {
            // the unscoped row has no commit to signal; the field is omitted
            assert_eq!(card.anchor_commit, None, "{card:?}");
            assert!(wire.get("anchor_commit").is_none(), "{wire}");
        }
    }
    let (_, read) = f.store.attached_schemas().unwrap();
    assert_eq!(
        read,
        Vec::<String>::new(),
        "nothing attached without a snapshot"
    );
}

/// G1-F11 `why_symbol_evidence_for_path`: a record linked only by `EVIDENCE_FOR` from the
/// `CodeRef` is returned with `matched_by: evidence`; one that is also anchored is `symbol`.
#[test]
fn why_symbol_evidence_for_path() {
    let f = Fixture::new("evidence", None);
    let coderef = f.coderef(AFERO_KEY, None);
    let evidenced = f.decision("the pointer is evidence for this record");
    f.link(&coderef, "EVIDENCE_FOR", &evidenced);
    let both = f.decision("anchored and evidenced");
    f.link(&both, "GOVERNS", &coderef);
    f.link(&coderef, "EVIDENCE_FOR", &both);
    let cards = f.why(AFERO_KEY);
    let by_id = |id: &str| cards.iter().find(|c| c.node.id == id).unwrap().matched_by;
    assert_eq!(cards.len(), 2, "{cards:?}");
    assert_eq!(by_id(&evidenced), MatchedBy::Evidence);
    assert_eq!(by_id(&both), MatchedBy::Symbol);
}

/// EN-F18a ambiguity: two non-external rows share the key in the snapshot that resolved as the
/// nearest ancestor (a `DiscoverProvider` over the fixture directory, `HEAD` itself has no
/// file): `code: ambiguous_symbol` with `exact: false`; the anchored record is still returned,
/// through the anchor leg only.
#[test]
fn why_symbol_ambiguous_symbol() {
    let s = Scratch::new("e26-why-symbol", "ambiguous");
    let (root, _head, ancestors) = repo(&s.0, 2);
    let ancestor = ancestors[0].clone();
    let built = cg_fixture::build(&s.0, Variant::AferoAmbiguous, &ancestor);
    let provider: Arc<dyn SnapshotProvider> = Arc::new(DiscoverProvider::new(built.dir.clone()));
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = Fixture::service(&s, &store, builtin_policy(), &root, Some(provider));
    let f = Fixture { s, store, api };

    let coderef = f.coderef(AFERO_KEY, Some(&ancestor));
    let anchored = f.decision("anchored on the ambiguous key");
    f.link(&anchored, "ABOUT", &coderef);
    let by_scope = f.decision("would govern the defining file");
    f.accept(&by_scope, &["*.go"]);

    let cards = f.why(AFERO_KEY);
    assert_eq!(
        ids(&cards),
        vec![anchored.as_str()],
        "no path leg: {cards:?}"
    );
    assert_eq!(
        cards[0].code,
        Some(CodeField::AmbiguousSymbol {
            snapshot: CodeSnapshot {
                commit: ancestor.clone(),
                exact: false,
                policy: "stale".into(),
            }
        })
    );
    assert_eq!(
        serde_json::to_value(&cards[0]).unwrap()["code"],
        json!({"ambiguous_symbol": {"snapshot": {"commit": ancestor, "exact": false, "policy": "stale"}}})
    );
}

/// A key present in `node_anchor` and absent from the resolved snapshot (`afero-renamed`) is
/// `code: snapshot_mismatch` with the `snapshot` object; the anchor leg still runs with the
/// resolved commit (a row scoped to it joins).
#[test]
fn why_symbol_snapshot_mismatch() {
    let s = Scratch::new("e26-why-symbol", "mismatch");
    let provider = Fixture::fixed(
        &s,
        Variant::AferoRenamed,
        AFERO_COMMIT,
        cg_fixture::POLICY_VERSION,
    );
    let (root, _, _) = repo(&s.0, 1);
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = Fixture::service(&s, &store, builtin_policy(), &root, Some(provider));
    let f = Fixture { s, store, api };

    let coderef = f.coderef(AFERO_KEY, Some(AFERO_COMMIT));
    let d = f.decision("renamed since");
    f.link(&d, "ABOUT", &coderef);
    let cards = f.why(AFERO_KEY);
    assert_eq!(ids(&cards), vec![d.as_str()], "{cards:?}");
    let snap = CodeSnapshot {
        commit: AFERO_COMMIT.into(),
        exact: true,
        policy: "stale".into(),
    };
    assert_eq!(
        cards[0].code,
        Some(CodeField::SnapshotMismatch { snapshot: snap })
    );
    let (_, read) = f.store.attached_schemas().unwrap();
    assert!(read.iter().any(|n| n == "cg"), "attached: {read:?}");
}

/// Contract §11 `why_symbol_too_new`: `afero-too-new` as the only candidate is
/// `NoSnapshot::TooNewOnly` from the provider, `code: snapshot_too_new` on the card, the
/// anchor leg only, and nothing attached (`PRAGMA database_list` on the read connection has
/// no `cg`).
#[test]
fn why_symbol_too_new() {
    let s = Scratch::new("e26-why-symbol", "too-new");
    let provider = Fixture::fixed(
        &s,
        Variant::AferoTooNew,
        AFERO_COMMIT,
        cg_fixture::POLICY_VERSION,
    );
    let (root, _, _) = repo(&s.0, 1);
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = Fixture::service(&s, &store, builtin_policy(), &root, Some(provider));
    let f = Fixture { s, store, api };

    let coderef = f.coderef(AFERO_KEY, None);
    let d = f.decision("anchored under a too-new snapshot");
    f.link(&d, "ABOUT", &coderef);
    let by_scope = f.decision("would govern the defining file");
    f.accept(&by_scope, &["*.go"]);
    let cards = f.why(AFERO_KEY);
    assert_eq!(ids(&cards), vec![d.as_str()], "anchor leg only: {cards:?}");
    assert_eq!(cards[0].code, Some(CodeField::SnapshotTooNew));
    assert_eq!(
        serde_json::to_value(&cards[0]).unwrap()["code"],
        json!("snapshot_too_new")
    );
    let (_, read) = f.store.attached_schemas().unwrap();
    assert!(
        !read.iter().any(|n| n == "cg"),
        "nothing attached: {read:?}"
    );
}

/// SO-13 `why_symbol_invalid_grammar`: a `symbol` target failing `kaban_codegraph::symbol::parse`
/// is `targets[].symbol: symbol_invalid`, a fixed class, the input never echoed.
#[test]
fn why_symbol_invalid_grammar() {
    let f = Fixture::new("grammar", None);
    for bad in [
        "MemMapFs/Create",
        "scip-go gomod",
        "not a symbol at all",
        "local ",
    ] {
        let err = f
            .api
            .why(WhyRequest {
                targets: vec![WhyTarget::Symbol(bad.into())],
                ..Default::default()
            })
            .unwrap_err();
        match err {
            ApiError::Invalid(msg) => {
                assert_eq!(msg, "targets[].symbol: symbol_invalid");
                assert!(!msg.contains(bad));
            }
            other => panic!("{bad:?}: {other:?}"),
        }
    }
}

/// SO-13 `why_symbol_canary_scanned_before_grammar`: a grammar-valid key with a credential
/// canary in a backtick descriptor is rejected by the pre-parse scan with the rule id; the
/// instrumented parser never saw the canary.
#[test]
fn why_symbol_canary_scanned_before_grammar() {
    let f = Fixture::new("canary", None);
    let canary = canary();
    // a backticked identifier must carry a non-simple character to be canonical (the space)
    let key = format!("scip-go gomod example.com/x . `{canary} key`/A#");
    assert!(symbol::parse(&key).is_ok(), "the probe is grammar-valid");
    let recorder = symbol::testing::record_inputs();
    let err = f
        .api
        .why(WhyRequest {
            targets: vec![WhyTarget::Symbol(key.clone())],
            ..Default::default()
        })
        .unwrap_err();
    match &err {
        ApiError::Rejected { field, rules } => {
            assert_eq!(field, "targets");
            assert!(rules.iter().any(|r| r == "aws-access-key-id"), "{rules:?}");
        }
        other => panic!("{other:?}"),
    }
    let seen = recorder.inputs();
    assert!(
        seen.iter().all(|s| !s.contains(&canary)),
        "the parser saw the canary: {seen:?}"
    );
}

/// SD-6 byte equality: a key one byte off (`memmapfs` for `MemMapFs`) does not join, and a
/// `CodeRef` that carries only a `name_path` has no `symbol` anchor to join on.
#[test]
fn why_symbol_key_byte_equality() {
    let f = Fixture::new("bytes", None);
    let coderef = f.coderef(AFERO_KEY, None);
    let d = f.decision("anchored on the exact bytes");
    f.link(&d, "ABOUT", &coderef);
    let name_path_only = f
        .api
        .remember(RememberRequest {
            kind: "CodeRef".into(),
            text: "name_path only".into(),
            props: Some(json!({
                "repo": REPO, "commit": AFERO_COMMIT, "tool": "fixture",
                "relative_path": AFERO_PATH, "name_path": "MemMapFs/Create", "kind": "Method", "line": 7,
            })),
            ..Default::default()
        })
        .unwrap()
        .id;
    let d_name_path = f.decision("linked to the name_path pointer");
    f.link(&d_name_path, "ABOUT", &name_path_only);

    assert_eq!(ids(&f.why(AFERO_KEY)), vec![d.as_str()]);
    let off_by_case = AFERO_KEY.replace("MemMapFs#", "memmapfs#");
    assert_ne!(off_by_case, AFERO_KEY);
    assert_eq!(
        ids(&f.why(&off_by_case)),
        Vec::<&str>::new(),
        "one byte off must not join"
    );
    let other_descriptor = AFERO_KEY.replace("Create().", "Create.");
    assert_eq!(ids(&f.why(&other_descriptor)), Vec::<&str>::new());
}

// ---- E2.6b: engine-plan :1071 golden parity over a view of the former column design ----

/// The cards of a `why(symbol)` as the comparable pairs `(record id, matched_by)`, sorted.
fn pairs(cards: &[WhyCard]) -> Vec<(String, MatchedBy)> {
    let mut out: Vec<_> = cards
        .iter()
        .map(|c| (c.node.id.clone(), c.matched_by))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The former column design, simulated: a view over the stored `CodeRef` columns (`node.repo`,
/// `props.symbol`, `props.snapshot_commit`) in place of the `node_anchor` rows, on a read-only
/// second connection to the store file.
fn column_view(db: &Path) -> rusqlite::Connection {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    conn.execute_batch(
        "CREATE TEMP VIEW coderef_columns AS \
         SELECT rid, workspace_id, repo, \
                json_extract(props, '$.symbol') AS symbol, \
                json_extract(props, '$.snapshot_commit') AS snapshot_commit \
         FROM node WHERE kind = 'CodeRef' AND lifecycle = 'active'",
    )
    .unwrap();
    conn
}

/// The G1-F11 query over the column view: the contract §3 predicates (`symbol = key`, `repo`,
/// the scope half, the workspace selector) on the pointer, then the record hop (`ABOUT` and
/// `GOVERNS` in as `symbol`, `EVIDENCE_FOR` out as `evidence`) and, when the key's defining file
/// is known, the path-scope leg over the records' own globs (`path_scope`); a record in two legs
/// takes the strongest match, as `why` does.
fn expected_over_view(
    conn: &rusqlite::Connection,
    key: &str,
    commit: Option<&str>,
    workspace: Option<&str>,
    defining_file: Option<&str>,
) -> Vec<(String, MatchedBy)> {
    // no resolved commit: every scope (OWN-83 (1)); `?3 IS NULL` keeps the parameter count
    let scope = if commit.is_some() {
        "(p.snapshot_commit IS NULL OR p.snapshot_commit = ?3)"
    } else {
        "?3 IS NULL"
    };
    let sql = format!(
        "WITH pointers AS ( \
             SELECT p.rid FROM coderef_columns p \
             WHERE p.symbol = ?1 AND p.repo = ?2 AND {scope} \
               AND (?4 IS NULL OR p.workspace_id = ?4)), \
         legs(rid, strength) AS ( \
             SELECT e.src, 3 FROM edge e JOIN pointers p ON p.rid = e.dst \
             WHERE e.rel IN ('ABOUT', 'GOVERNS') \
             UNION ALL \
             SELECT e.dst, 2 FROM edge e JOIN pointers p ON p.rid = e.src \
             WHERE e.rel = 'EVIDENCE_FOR' \
             UNION ALL \
             SELECT n.rid, 1 FROM node n, json_each(n.path_scope) g \
             WHERE ?5 IS NOT NULL AND ?5 GLOB g.value) \
         SELECT n.id, max(l.strength) FROM legs l JOIN node n ON n.rid = l.rid \
         WHERE n.kind = 'Decision' AND n.lifecycle = 'active' \
           AND (?4 IS NULL OR n.workspace_id = ?4) \
         GROUP BY n.id ORDER BY n.id"
    );
    let mut st = conn.prepare(&sql).unwrap();
    let mut rows: Vec<(String, MatchedBy)> = st
        .query_map(
            rusqlite::params![key, REPO, commit, workspace, defining_file],
            |r| {
                let strength: i64 = r.get(1)?;
                Ok((
                    r.get::<_, String>(0)?,
                    match strength {
                        3 => MatchedBy::Symbol,
                        2 => MatchedBy::Evidence,
                        _ => MatchedBy::PathScope,
                    },
                ))
            },
        )
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Engine-plan :1071 golden parity: over one store holding the data of the seven G1-F11 names
/// (an unscoped, a scoped, an other-scope, an other-repository, an other-workspace and a
/// case-different pointer, records on each by `ABOUT`, `GOVERNS` and `EVIDENCE_FOR`, a record
/// governing `*.go` and one governing `src/cli/**`), the query shapes return identical
/// `(record, matched_by)` cards over `node_anchor` (the API) as over the former column design
/// (the view built here from the fixture's own data): the bound workspace with the resolved
/// commit, every workspace (`"*"`), the key one byte off, the key with another descriptor, and
/// a service without a snapshot (the scope predicate reduced to its `NULL` half, no path leg).
#[test]
fn why_symbol_golden_parity_over_a_column_view() {
    let s = Scratch::new("e26-why-symbol", "parity");
    let provider = Fixture::fixed(&s, Variant::Afero, AFERO_COMMIT, cg_fixture::POLICY_VERSION);
    let (root, _, _) = repo(&s.0, 1);
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = Fixture::service(&s, &store, builtin_policy(), &root, Some(provider));
    let f = Fixture { s, store, api };
    let scoped_coderef = |text: &str, scope: &str, key: &str| -> String {
        f.api
            .remember(RememberRequest {
                kind: "CodeRef".into(),
                text: text.into(),
                props: Some(coderef_props(key, Some(scope))),
                ..Default::default()
            })
            .unwrap()
            .id
    };
    let ws = WorkspaceId::default();

    // the pointers of the seven names, each with a record of its own
    let mine = f.coderef(AFERO_KEY, None);
    let d_about = f.decision("about the pointer");
    f.link(&d_about, "ABOUT", &mine);
    let d_governs = f.decision("governs the pointer");
    f.accept(&d_governs, &["docs/**"]);
    f.link(&d_governs, "GOVERNS", &mine);
    let d_evidence = f.decision("the pointer is its evidence");
    f.link(&mine, "EVIDENCE_FOR", &d_evidence);
    let d_both = f.decision("anchored and evidenced");
    f.link(&d_both, "ABOUT", &mine);
    f.link(&mine, "EVIDENCE_FOR", &d_both);
    let scoped = scoped_coderef("scoped to the resolved commit", AFERO_COMMIT, AFERO_KEY);
    let d_scoped = f.decision("about the scoped pointer");
    f.link(&d_scoped, "ABOUT", &scoped);
    let other_scope = scoped_coderef("scoped to another commit", OTHER_COMMIT, AFERO_KEY);
    let d_other_scope = f.decision("about the other-scope pointer");
    f.link(&d_other_scope, "ABOUT", &other_scope);
    let other_repo = f.foreign_coderef(AFERO_KEY, "other-repo", ws.as_str());
    let d_other_repo = f.decision("about the other repository's pointer");
    f.link(&d_other_repo, "ABOUT", &other_repo);
    let other_ws = f.foreign_coderef(AFERO_KEY, REPO, "ws2");
    let d_other_ws = f
        .api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: "about the other workspace's pointer".into(),
            props: Some(json!({"rationale": "because"})),
            workspace_id: Some("ws2".into()),
            ..Default::default()
        })
        .unwrap()
        .id;
    let ws2 =
        WriteCtx::new("pv", WorkspaceId::new("ws2"), Actor::Agent).with_governed(f.api.governed());
    f.store
        .link(
            &NodeId::parse(&d_other_ws).unwrap(),
            &NodeId::parse(&other_ws).unwrap(),
            "ABOUT",
            None,
            None,
            &ws2,
        )
        .unwrap();
    let off_by_case = AFERO_KEY.replace("MemMapFs#", "memmapfs#");
    let case_pointer = f
        .api
        .remember(RememberRequest {
            kind: "CodeRef".into(),
            text: "one byte off".into(),
            props: Some(coderef_props(&off_by_case, None)),
            ..Default::default()
        })
        .unwrap()
        .id;
    let d_case = f.decision("about the case-different pointer");
    f.link(&d_case, "ABOUT", &case_pointer);
    let by_scope = f.decision("go files keep the error wrapping convention");
    f.accept(&by_scope, &["*.go"]);
    let elsewhere = f.decision("the cli prints json");
    f.accept(&elsewhere, &["src/cli/**"]);

    let view = column_view(&f.s.db());
    // the bound workspace with the resolved commit (the exact-match shape)
    let bound = f.why(AFERO_KEY);
    let want = expected_over_view(
        &view,
        AFERO_KEY,
        Some(AFERO_COMMIT),
        Some(ws.as_str()),
        Some(AFERO_PATH),
    );
    assert_eq!(pairs(&bound), want, "{bound:?}");
    assert!(
        want.iter()
            .any(|(id, m)| id == &d_scoped && *m == MatchedBy::Symbol),
        "the view side is not vacuous: {want:?}"
    );
    assert!(
        want.iter()
            .any(|(id, m)| id == &d_evidence && *m == MatchedBy::Evidence)
    );
    assert!(
        want.iter()
            .any(|(id, m)| id == &d_both && *m == MatchedBy::Symbol)
    );
    assert!(
        want.iter()
            .any(|(id, m)| id == &by_scope && *m == MatchedBy::PathScope)
    );
    for excluded in [
        &d_other_scope,
        &d_other_repo,
        &d_other_ws,
        &d_case,
        &elsewhere,
    ] {
        assert!(!want.iter().any(|(id, _)| id == excluded), "{want:?}");
    }
    // every workspace: the ws2 record joins on both sides
    let all = f.why_in(AFERO_KEY, Some("*"));
    let want = expected_over_view(&view, AFERO_KEY, Some(AFERO_COMMIT), None, Some(AFERO_PATH));
    assert_eq!(pairs(&all), want, "{all:?}");
    assert!(want.iter().any(|(id, _)| id == &d_other_ws));
    // one byte off (the key is absent from the snapshot: no defining file, no path leg)
    let case = f.why(&off_by_case);
    let want = expected_over_view(
        &view,
        &off_by_case,
        Some(AFERO_COMMIT),
        Some(ws.as_str()),
        None,
    );
    assert_eq!(pairs(&case), want, "{case:?}");
    assert_eq!(want, vec![(d_case.clone(), MatchedBy::Symbol)]);
    // another descriptor: empty on both sides
    let other_descriptor = AFERO_KEY.replace("Create().", "Create.");
    let none = f.why(&other_descriptor);
    let want = expected_over_view(
        &view,
        &other_descriptor,
        Some(AFERO_COMMIT),
        Some(ws.as_str()),
        None,
    );
    assert_eq!(pairs(&none), want);
    assert_eq!(want, Vec::new(), "another descriptor joins nothing");
    // no snapshot: the `NULL` half of the scope predicate only, no path leg
    let absent = Fixture::service(&f.s, &f.store, builtin_policy(), &root, None);
    let cards = absent
        .why(WhyRequest {
            targets: vec![WhyTarget::Symbol(AFERO_KEY.into())],
            ..Default::default()
        })
        .unwrap()
        .results;
    let want = expected_over_view(&view, AFERO_KEY, None, Some(ws.as_str()), None);
    assert_eq!(pairs(&cards), want, "{cards:?}");
    // OWN-83 (1): no resolved snapshot returns every scope, the scoped pointer included
    assert!(want.iter().any(|(id, _)| id == &d_scoped));
    assert!(want.iter().any(|(id, _)| id == &d_about));
}
