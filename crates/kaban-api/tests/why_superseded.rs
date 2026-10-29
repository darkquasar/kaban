//! The `why` extension of v4 CG6.1 over the engine's `symbol` target (stage1-spec §6, G1-F11
//! extension, G1-F13, G1-S7; CG-JOIN v1 §7, §11): `include_superseded` reaches the archived
//! predecessors of each current card through its `SUPERSEDES` chain, every card carries
//! `superseded_by`, both under the request's resolved workspace (RS-9), and the `MatchedBy`
//! wire enum serialises all four legs. The fixture follows `why_symbol.rs` (no snapshot
//! provider: `code: snapshot_absent`, the anchor leg still joins). Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/cg_fixture.rs"]
mod cg_fixture;
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use cg_fixture::AFERO_KEY;
use common::Scratch;
use kaban_api::{
    AcceptRequest, ApiConfig, Caller, KabanApi, LinkRequest, MatchedBy, RememberRequest, WhyCard,
    WhyRequest, WhyTarget,
};
use kaban_core::ActorId;
use kaban_core::{
    Actor, Governance, MemoryStore, NewNode, RememberOutcome, WorkspaceId, WriteCtx, new_id,
    now_iso,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// The process repository identity (`ApiConfig.repo` = `CodeRef.repo`).
const REPO: &str = "kaban";
/// The fixture's defining file, as in `why_symbol.rs`.
const AFERO_PATH: &str = "memmap.go";

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

/// A one-commit repository (the currency anchor needs a head).
fn repo(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "0").unwrap();
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "c0"]);
    root
}

/// The shipped builtin policy.
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
    /// A service bound to [`REPO`] over a fresh store and a one-commit repository, no
    /// snapshot provider.
    fn new(name: &str) -> Self {
        let s = Scratch::new("cg61-why-superseded", name);
        let root = repo(&s.0);
        let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
        let api = KabanApi::with_config(
            store.clone(),
            builtin_policy(),
            store.signer(),
            None,
            ApiConfig {
                repo: Some(REPO.into()),
                repo_root: Some(root),
                ..Default::default()
            },
        );
        Self { s, store, api }
    }

    /// A proposed Decision in the bound workspace, or in `workspace`.
    fn decision_in(&self, text: &str, workspace: Option<&str>) -> String {
        self.api
            .remember(RememberRequest {
                kind: "Decision".into(),
                text: text.into(),
                props: Some(json!({"rationale": "because"})),
                workspace_id: workspace.map(str::to_owned),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    /// An accepted Decision linked `GOVERNS` to `coderef`.
    fn governing(&self, text: &str, coderef: &str) -> String {
        let id = self.decision_in(text, None);
        self.link(&id, "GOVERNS", coderef);
        self.api
            .accept(
                AcceptRequest {
                    id: id.clone(),
                    reason: Some("reviewed".into()),
                    path_scope: vec!["src/**".into()],
                    ..Default::default()
                },
                &human(),
            )
            .unwrap();
        id
    }

    /// A `CodeRef` of this process's repository through the public API, unscoped.
    fn coderef(&self, key: &str) -> String {
        self.api
            .remember(RememberRequest {
                kind: "CodeRef".into(),
                text: format!("{REPO} {AFERO_PATH} {key}"),
                props: Some(coderef_props(key)),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    /// A `CodeRef` written at the store seam with an explicit `repo` and workspace (the
    /// cross-repository row the API would never write for this process), unscoped.
    fn foreign_coderef(&self, key: &str, repo: &str, workspace: &str) -> String {
        let ctx = WriteCtx::new("pv", WorkspaceId::new(workspace), Actor::Agent)
            .with_governed(self.api.governed());
        match self
            .store
            .remember(
                NewNode {
                    kind: "CodeRef".into(),
                    text: format!("{repo} {workspace} {key}"),
                    props: Some(coderef_props(key)),
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

    /// One raw `SUPERSEDES` edge row `new -> old` on a second connection: the cross-workspace
    /// supersession the API refuses (as `kaban-store-sqlite/tests/anchors_v4.rs` seeds it).
    /// `old` keeps its governance: only the chain sees the edge.
    fn raw_supersedes(&self, new: &str, old: &str) {
        let conn = Connection::open(self.s.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        let rid = |id: &str| -> i64 {
            conn.query_row("SELECT rid FROM node WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        conn.execute(
            "INSERT INTO edge(id, src, dst, rel, created_at, props) VALUES (?1, ?2, ?3, 'SUPERSEDES', ?4, '{}')",
            params![new_id(), rid(new), rid(old), now_iso()],
        )
        .unwrap();
    }

    /// `why(symbol)` with the extension's two knobs; `partial` beside the cards.
    fn why(
        &self,
        key: &str,
        workspace_id: Option<&str>,
        include_superseded: bool,
    ) -> (Vec<WhyCard>, bool) {
        let r = self
            .api
            .why(WhyRequest {
                targets: vec![WhyTarget::Symbol(key.into())],
                workspace_id: workspace_id.map(str::to_owned),
                include_superseded,
                ..Default::default()
            })
            .unwrap();
        (r.results, r.partial)
    }
}

fn coderef_props(key: &str) -> Value {
    json!({
        "repo": REPO, "commit": "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1", "tool": "fixture",
        "relative_path": AFERO_PATH, "name_path": "MemMapFs/Create", "kind": "Method", "line": 7,
        "symbol": key,
    })
}

fn ids(cards: &[WhyCard]) -> Vec<&str> {
    cards.iter().map(|c| c.node.id.as_str()).collect()
}

fn card<'a>(cards: &'a [WhyCard], id: &str) -> &'a WhyCard {
    cards
        .iter()
        .find(|c| c.node.id == id)
        .unwrap_or_else(|| panic!("{id} not in {cards:?}"))
}

/// CG-JOIN v1 §11 `why_include_superseded_returns_superseded_with_superseded_by`: D1 accepted,
/// then superseded by D2 (`api.supersede`, the store archives D1). The default call returns D2
/// alone with `superseded_by: []` (negative: D1 is not listed); `include_superseded: true`
/// returns both, D1 after D2 with `superseded_by: [D2]`, D1's `matched_by` and `code` equal to
/// D2's, D1's governance `superseded`.
#[test]
fn why_include_superseded_returns_superseded_with_superseded_by() {
    let f = Fixture::new("include");
    let coderef = f.coderef(AFERO_KEY);
    let d1 = f.governing("D1: MemMapFs.Create allocates once", &coderef);
    let d2 = f.governing("D2: MemMapFs.Create stays allocation-free", &coderef);
    f.api.supersede(&d2, &d1, &human()).unwrap();

    let (current, partial) = f.why(AFERO_KEY, None, false);
    assert_eq!(ids(&current), vec![d2.as_str()], "{current:?}");
    assert!(!partial);
    assert!(current[0].superseded_by.is_empty(), "{current:?}");

    let (all, partial) = f.why(AFERO_KEY, None, true);
    assert_eq!(ids(&all), vec![d2.as_str(), d1.as_str()], "{all:?}");
    assert!(!partial);
    let (c2, c1) = (card(&all, &d2), card(&all, &d1));
    assert!(c2.superseded_by.is_empty(), "{c2:?}");
    assert_eq!(c1.superseded_by, vec![d2.clone()], "{c1:?}");
    assert_eq!(c1.governance, Governance::Superseded);
    assert_eq!(c1.matched_by, c2.matched_by);
    assert_eq!(c1.matched_by, MatchedBy::Symbol);
    assert_eq!(c1.code, c2.code);
    assert_eq!(c1.matched_target, c2.matched_target);
}

/// RS-9 `why_superseded_by_respects_workspace`: D1 accepted in the bound workspace governs the
/// fixture `CodeRef`; A, a Decision in `ws-b`, supersedes D1 through one raw `SUPERSEDES` edge
/// row (a cross-workspace supersession the API refuses). Under the bound workspace the
/// successor is in an unselected workspace and `superseded_by` is `[]` (the chain neither
/// leaks nor bridges, S6); under `"*"` it is `[A]`. A itself never governs the key and is not
/// a card in either read.
#[test]
fn why_superseded_by_respects_workspace() {
    let f = Fixture::new("workspace");
    let coderef = f.coderef(AFERO_KEY);
    let d1 = f.governing("D1: deploy pipeline uses blue green", &coderef);
    let a = f.decision_in("A: deploy pipeline uses canary now", Some("ws-b"));
    f.raw_supersedes(&a, &d1);

    let (bound, _) = f.why(AFERO_KEY, None, false);
    assert_eq!(ids(&bound), vec![d1.as_str()], "{bound:?}");
    assert!(
        bound[0].superseded_by.is_empty(),
        "a successor in an unselected workspace must not be listed: {bound:?}"
    );

    let (all, _) = f.why(AFERO_KEY, Some("*"), false);
    assert_eq!(ids(&all), vec![d1.as_str()], "{all:?}");
    assert_eq!(all[0].superseded_by, vec![a.clone()], "{all:?}");
}

/// G1-S7 `governance_join_workspace_filter`: under `workspace_id: "*"` the code side stays
/// fixed to the current repository. R2 governs a `CodeRef` of another repository with the same
/// key and is never returned, with or without `include_superseded`; the current-repository
/// record R1 is, and so is its superseded predecessor D0 (reached through R1's chain, with
/// `superseded_by: [R1]`), never through R2's.
#[test]
fn governance_join_workspace_filter() {
    let f = Fixture::new("join");
    let mine = f.coderef(AFERO_KEY);
    let d0 = f.governing("D0: the old rule", &mine);
    let r1 = f.governing("R1: the current rule", &mine);
    f.api.supersede(&r1, &d0, &human()).unwrap();
    let theirs = f.foreign_coderef(AFERO_KEY, "other-repo", WorkspaceId::default().as_str());
    let r2 = f.governing("R2: the other repository's rule", &theirs);

    // the predicate half (engine E2.6, green at the base): R2 is excluded under "*"
    let (current, _) = f.why(AFERO_KEY, Some("*"), false);
    assert_eq!(ids(&current), vec![r1.as_str()], "{current:?}");

    // the include_superseded half: D0 behind R1, still never R2
    let (all, partial) = f.why(AFERO_KEY, Some("*"), true);
    assert_eq!(ids(&all), vec![r1.as_str(), d0.as_str()], "{all:?}");
    assert!(!partial);
    assert!(!ids(&all).contains(&r2.as_str()), "{all:?}");
    assert_eq!(card(&all, &d0).superseded_by, vec![r1.clone()]);
    assert!(card(&all, &r1).superseded_by.is_empty(), "{all:?}");
}

/// `limit` counts the chain-reached cards: with `limit: 1` the superseded predecessor is cut
/// and `partial` says so; the current card stays first.
#[test]
fn why_include_superseded_counts_against_limit() {
    let f = Fixture::new("limit");
    let coderef = f.coderef(AFERO_KEY);
    let d1 = f.governing("D1: first", &coderef);
    let d2 = f.governing("D2: second", &coderef);
    f.api.supersede(&d2, &d1, &human()).unwrap();

    let r = f
        .api
        .why(WhyRequest {
            targets: vec![WhyTarget::Symbol(AFERO_KEY.into())],
            limit: Some(1),
            include_superseded: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ids(&r.results), vec![d2.as_str()], "{r:?}");
    assert!(r.partial, "{r:?}");
}

/// stage1-spec §6 `MatchedBy` wire completeness: the four legs round-trip through
/// `serde_json` as `path_scope`, `query`, `symbol`, `evidence`; the JSON schema lists exactly
/// those four; the Rust spelling `"Symbol"` is not a wire value.
#[test]
fn matched_by_serialises_all_four_legs() {
    let legs = [
        (MatchedBy::PathScope, "path_scope"),
        (MatchedBy::Query, "query"),
        (MatchedBy::Symbol, "symbol"),
        (MatchedBy::Evidence, "evidence"),
    ];
    for (leg, wire) in legs {
        let v = serde_json::to_value(leg).unwrap();
        assert_eq!(v, json!(wire));
        let back: MatchedBy = serde_json::from_value(v).unwrap();
        assert_eq!(back, leg);
    }
    let schema = serde_json::to_value(schemars::schema_for!(MatchedBy)).unwrap();
    let listed: Vec<&str> = schema["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("{schema}"))
        .iter()
        .map(|o| o["const"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["path_scope", "query", "symbol", "evidence"]);
    assert!(serde_json::from_value::<MatchedBy>(json!("Symbol")).is_err());
    assert!(serde_json::from_value::<MatchedBy>(json!("PathScope")).is_err());
}
