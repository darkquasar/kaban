//! The `governance_history` tool of v4 CG6.2 (stage1-spec G1-F13, §6, G1-S7): the `SUPERSEDES`
//! chain of each reached record in both directions, its events newest first, the workspace
//! filter on the seed and on every hop (RS-9, S6), the bounds and the selector rule. The
//! fixture follows `why_superseded.rs` (no snapshot provider). Invented data.

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
    AcceptRequest, ApiConfig, ApiError, Caller, GovernanceHistoryRequest, HistoryRecord, KabanApi,
    LinkRequest, RememberRequest,
};
use kaban_core::ActorId;
use kaban_core::{Actor, new_id, now_iso};
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
    api: KabanApi,
}

impl Fixture {
    /// A service bound to [`REPO`] over a fresh store and a one-commit repository, no
    /// snapshot provider.
    fn new(name: &str) -> Self {
        let s = Scratch::new("cg62-governance-history", name);
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
        Self { s, api }
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

    /// An accepted Decision linked `GOVERNS` to `coderef`, scoped `src/**`.
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

    /// `governance_history(id)` under `workspace_id`.
    fn by_id(
        &self,
        id: &str,
        workspace_id: Option<&str>,
    ) -> Result<(Vec<HistoryRecord>, bool), ApiError> {
        self.api
            .governance_history(GovernanceHistoryRequest {
                id: Some(id.into()),
                workspace_id: workspace_id.map(str::to_owned),
                ..Default::default()
            })
            .map(|r| (r.records, r.partial))
    }
}

fn coderef_props(key: &str) -> Value {
    json!({
        "repo": REPO, "commit": "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1", "tool": "fixture",
        "relative_path": AFERO_PATH, "name_path": "MemMapFs/Create", "kind": "Method", "line": 7,
        "symbol": key,
    })
}

fn ids(records: &[HistoryRecord]) -> Vec<&str> {
    records.iter().map(|r| r.card.id.as_str()).collect()
}

/// S6 (plan CG6.2 negative path): D lives in `ws-b`. The bound read of `governance_history(id:
/// D)` is `NotFound` (no existence leak, as `traverse` and `events` answer today); under `"*"`
/// D is the one record.
#[test]
fn history_id_in_other_workspace_is_not_found() {
    let f = Fixture::new("other-workspace");
    let _ = f.coderef(AFERO_KEY);
    let d = f.decision_in("D: the ws-b rule", Some("ws-b"));

    let bound = f.by_id(&d, None);
    assert!(
        matches!(bound, Err(ApiError::NotFound(_))),
        "a record in an unselected workspace must be NotFound on the bound read: {bound:?}"
    );

    let (all, partial) = f.by_id(&d, Some("*")).unwrap();
    assert_eq!(ids(&all), vec![d.as_str()], "{all:?}");
    assert!(!partial);
}

/// Plan CG6.2 negative path: a `path` selector with a `..` component (or a leading `/`) is
/// `selector_invalid` before any store read; a repository-relative path is accepted (negative:
/// `Ok`, no record in an empty store).
#[test]
fn history_path_dot_dot_is_selector_invalid() {
    let f = Fixture::new("dotdot");
    for bad in ["src/../etc/passwd", "/etc/passwd"] {
        let r = f.api.governance_history(GovernanceHistoryRequest {
            path: Some(bad.into()),
            ..Default::default()
        });
        match r {
            Err(ApiError::Invalid(text)) => {
                assert!(text.starts_with("selector_invalid"), "{bad}: {text}");
            }
            other => {
                panic!("{bad}: a non repository-relative path must be selector_invalid: {other:?}")
            }
        }
    }
    let ok = f
        .api
        .governance_history(GovernanceHistoryRequest {
            path: Some("src/a.rs".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(ok.records.is_empty() && !ok.partial, "{ok:?}");
}

fn record<'a>(records: &'a [HistoryRecord], id: &str) -> &'a HistoryRecord {
    records
        .iter()
        .find(|r| r.card.id == id)
        .unwrap_or_else(|| panic!("{id} not in {records:?}"))
}

fn kinds(r: &HistoryRecord) -> Vec<&str> {
    r.events.iter().map(|e| e.kind.as_str()).collect()
}

/// G1-F13 `history_chain_both_directions`: D1 <- D2 <- D3 (D2 supersedes D1, D3 supersedes
/// D2). Seeding at D2 returns the seed, then its predecessors, then its successors: D2, D1,
/// D3. Each chain is whole and nearest first: D1 `superseded_by: [D2, D3]`, D3 `supersedes:
/// [D2, D1]`, D2 both one deep. Events are newest first: D1's `superseded` before its
/// `accepted`. Negative: a lone accepted record has both lists empty and no cut.
#[test]
fn history_chain_both_directions() {
    let f = Fixture::new("chain");
    let coderef = f.coderef(AFERO_KEY);
    let d1 = f.governing("D1: MemMapFs.Create allocates once", &coderef);
    let d2 = f.governing("D2: MemMapFs.Create stays allocation-free", &coderef);
    let d3 = f.governing("D3: MemMapFs.Create pools its buffers", &coderef);
    let lone = f.governing("L: a rule nothing supersedes", &coderef);
    f.api.supersede(&d2, &d1, &human()).unwrap();
    f.api.supersede(&d3, &d2, &human()).unwrap();

    let (records, partial) = f.by_id(&d2, None).unwrap();
    assert_eq!(
        ids(&records),
        vec![d2.as_str(), d1.as_str(), d3.as_str()],
        "{records:?}"
    );
    assert!(!partial, "{records:?}");
    let (r1, r2, r3) = (
        record(&records, &d1),
        record(&records, &d2),
        record(&records, &d3),
    );
    assert_eq!(r1.superseded_by, vec![d2.clone(), d3.clone()], "{r1:?}");
    assert!(r1.supersedes.is_empty(), "{r1:?}");
    assert_eq!(r2.supersedes, vec![d1.clone()], "{r2:?}");
    assert_eq!(r2.superseded_by, vec![d3.clone()], "{r2:?}");
    assert_eq!(r3.supersedes, vec![d2.clone(), d1.clone()], "{r3:?}");
    assert!(r3.superseded_by.is_empty(), "{r3:?}");
    assert!(records.iter().all(|r| !r.chain_cut), "{records:?}");
    assert_eq!(r1.governance, kaban_core::Governance::Superseded);
    assert_eq!(r3.governance, kaban_core::Governance::Accepted);
    // newest first: the supersession after the acceptance, seq descending
    assert_eq!(kinds(r1), vec!["superseded", "accepted"], "{r1:?}");
    assert!(r1.events.windows(2).all(|w| w[0].seq > w[1].seq), "{r1:?}");

    let (alone, partial) = f.by_id(&lone, None).unwrap();
    assert_eq!(ids(&alone), vec![lone.as_str()], "{alone:?}");
    assert!(!partial);
    assert!(
        alone[0].supersedes.is_empty() && alone[0].superseded_by.is_empty(),
        "{alone:?}"
    );
    assert!(!alone[0].chain_cut);
    assert_eq!(kinds(&alone[0]), vec!["accepted"], "{alone:?}");
}

/// G1-F13, RS-9 `history_hop_in_other_workspace_cut`: A, a Decision in `ws-b`, supersedes D1
/// through one raw `SUPERSEDES` edge row. On the bound read the successor hop lies in an
/// unselected workspace: it ends the chain (`chain_cut: true`, `partial: true`), is never
/// listed in `superseded_by` and is not a record. Under `"*"` A is the hop and the record.
#[test]
fn history_hop_in_other_workspace_cut() {
    let f = Fixture::new("hop-workspace");
    let coderef = f.coderef(AFERO_KEY);
    let d1 = f.governing("D1: deploy pipeline uses blue green", &coderef);
    let a = f.decision_in("A: deploy pipeline uses canary now", Some("ws-b"));
    f.raw_supersedes(&a, &d1);

    let (bound, partial) = f.by_id(&d1, None).unwrap();
    assert_eq!(
        ids(&bound),
        vec![d1.as_str()],
        "A must not be a record: {bound:?}"
    );
    assert!(bound[0].chain_cut, "{bound:?}");
    assert!(partial, "{bound:?}");
    assert!(
        bound[0].superseded_by.is_empty(),
        "a successor in an unselected workspace must not be listed: {bound:?}"
    );

    let (all, partial) = f.by_id(&d1, Some("*")).unwrap();
    assert_eq!(ids(&all), vec![d1.as_str(), a.as_str()], "{all:?}");
    assert!(!partial, "{all:?}");
    assert_eq!(all[0].superseded_by, vec![a.clone()], "{all:?}");
    assert!(!all[0].chain_cut, "{all:?}");
    assert_eq!(record(&all, &a).supersedes, vec![d1.clone()], "{all:?}");
}

/// G1-F13 `history_limits_partial`: 21 accepted Decisions govern `src/a.rs`; the `path` seed
/// at the default limit returns 20 records and `partial: true`; `limit: 100` returns all 21
/// and `false`; a `limit` under 1 is clamped to 1.
#[test]
fn history_limits_partial() {
    let f = Fixture::new("limits");
    let coderef = f.coderef(AFERO_KEY);
    let all_ids: Vec<String> = (0..21)
        .map(|i| f.governing(&format!("D{i}: rule number {i} for src"), &coderef))
        .collect();
    let by_path = |limit: Option<usize>| {
        f.api
            .governance_history(GovernanceHistoryRequest {
                path: Some("src/a.rs".into()),
                limit,
                ..Default::default()
            })
            .unwrap()
    };

    let page = by_path(None);
    assert_eq!(page.records.len(), 20, "{:?}", ids(&page.records));
    assert!(page.partial);
    assert!(page.records.iter().all(|r| all_ids.contains(&r.card.id)));

    let whole = by_path(Some(100));
    assert_eq!(whole.records.len(), 21, "{:?}", ids(&whole.records));
    assert!(!whole.partial);
    let mut listed: Vec<&str> = ids(&whole.records);
    listed.sort_unstable();
    let mut expected: Vec<&str> = all_ids.iter().map(String::as_str).collect();
    expected.sort_unstable();
    assert_eq!(listed, expected);

    let one = by_path(Some(0));
    assert_eq!(one.records.len(), 1, "{:?}", ids(&one.records));
    assert!(one.partial);
}

/// RW3-4 (G1-F13, CG6.2): the seed cut is a truncation of the answer. 101 accepted Decisions
/// govern `src/a.rs`; the `path` seeds come from `why` at its limit of 100, so `limit: 100`
/// returns 100 records and `partial: true` although the record loop itself never overflows.
/// Negative: `history_limits_partial` above, 21 records at `limit: 100` are `partial: false`.
#[test]
fn history_seed_cut_is_partial() {
    let f = Fixture::new("seed-cut");
    let coderef = f.coderef(AFERO_KEY);
    for i in 0..101 {
        f.governing(&format!("D{i}: rule number {i} for src"), &coderef);
    }
    let page = f
        .api
        .governance_history(GovernanceHistoryRequest {
            path: Some("src/a.rs".into()),
            limit: Some(100),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.records.len(), 100, "{}", page.records.len());
    assert!(page.partial, "the seed cut at 100 of 101 is reported");
}

/// G1-F13 `history_selector_one_of`: two selectors set, or none, is `selector_invalid`
/// before any store read; negative: one selector is answered.
#[test]
fn history_selector_one_of() {
    let f = Fixture::new("selector");
    let coderef = f.coderef(AFERO_KEY);
    let d = f.governing("D: one rule", &coderef);
    let invalid = |req: GovernanceHistoryRequest| match f.api.governance_history(req) {
        Err(ApiError::Invalid(text)) => assert!(text.starts_with("selector_invalid"), "{text}"),
        other => panic!("not exactly one selector must be selector_invalid: {other:?}"),
    };
    invalid(GovernanceHistoryRequest {
        id: Some(d.clone()),
        path: Some("src/a.rs".into()),
        ..Default::default()
    });
    invalid(GovernanceHistoryRequest::default());
    invalid(GovernanceHistoryRequest {
        id: Some(d.clone()),
        symbol: Some(AFERO_KEY.into()),
        path: Some("src/a.rs".into()),
        ..Default::default()
    });

    let (one, _) = f.by_id(&d, None).unwrap();
    assert_eq!(ids(&one), vec![d.as_str()], "{one:?}");
    let by_symbol = f
        .api
        .governance_history(GovernanceHistoryRequest {
            symbol: Some(AFERO_KEY.into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ids(&by_symbol.records), vec![d.as_str()], "{by_symbol:?}");
}
