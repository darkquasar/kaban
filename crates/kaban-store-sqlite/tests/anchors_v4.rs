//! The store half of v4 stage1-plan CG5.3 (cg-join-v1 §3, §5, §7; G1-F10 verify; RS-9): the
//! engine's `coderefs_by_symbol` predicates read through the trait, the `node_anchor_key`
//! index named by the query plan, and the `supersession_chain` walk (the workspace cut and the
//! `limit` cut). The API half (`anchor_conformance_v4`) lives in
//! `crates/kaban-api/tests/anchors_v4.rs`. Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, ctx, nid};
use kaban_core::ActorId;
use kaban_core::{
    Actor, AnchorPredicates, Chain, ChainDirection, Governance, GovernanceStore, LinkReq,
    MemoryStore, NewEvent, NewNode, Node, RememberOutcome, SignerKind, StoreError, WorkspaceFilter,
    WorkspaceId, WriteCtx, new_id, now_iso,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::json;

/// A normalised key (stage1-spec §4.2).
const KEY: &str = "scip-go gomod example.com/m . store/Store#Open().";
/// Two 40-hex snapshot commits.
const COMMIT_X: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMIT_Y: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn open(s: &Scratch) -> SqliteStore {
    SqliteStore::open(&s.db(), &s.keys()).unwrap()
}

fn read_only(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// A `CodeRef` with `symbol = key` in `workspace` and `repo`, scoped to `scope` (the
/// `tests/attach.rs` fixture); its id.
fn coderef(
    store: &SqliteStore,
    workspace: &str,
    repo: &str,
    key: &str,
    scope: Option<&str>,
) -> String {
    let mut props = json!({
        "repo": repo, "commit": COMMIT_X, "tool": "fixture", "relative_path": "store.go",
        "name_path": "Store/Open", "kind": "Method", "line": 7, "symbol": key,
    });
    if let Some(scope) = scope {
        props["snapshot_commit"] = json!(scope);
    }
    let ctx = WriteCtx::new("pv", WorkspaceId::new(workspace), Actor::Agent);
    match store
        .remember(
            NewNode {
                kind: "CodeRef".into(),
                text: format!("{workspace} {repo} {key} {scope:?}"),
                props: Some(props),
                repo: Some(repo.into()),
                workspace_id: Some(workspace.into()),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(n) | RememberOutcome::Existing { node: n, .. } => n.id,
    }
}

fn ids(store: &SqliteStore, p: &AnchorPredicates<'_>) -> Vec<String> {
    let mut v: Vec<String> = store
        .coderefs_by_symbol(p, KEY)
        .unwrap()
        .into_iter()
        .map(|n| n.id)
        .collect();
    v.sort();
    v
}

/// CG-JOIN v1 §3 and §7 through `MemoryStore::coderefs_by_symbol` (the engine's E2.6 method,
/// expected green at the base; brief default 4): another repository and another workspace are
/// out under `Only(default)`, a ref scoped to commit X is not returned for resolved commit Y,
/// a `NULL`-scoped ref is returned for any resolved commit, and `commit: None` returns every
/// scope (cg-join §3; OWN-83 (1), E2.7b: the latest decision rather than nothing).
#[test]
fn coderefs_by_symbol_predicates_v4() {
    let s = Scratch::new("cg53-anchors", "predicates");
    let store = open(&s);
    let ws = WorkspaceId::default();
    let unscoped = coderef(&store, ws.as_str(), "kaban", KEY, None);
    let scoped_x = coderef(&store, ws.as_str(), "kaban", KEY, Some(COMMIT_X));
    let _other_repo = coderef(&store, ws.as_str(), "other", KEY, None);
    let other_ws = coderef(&store, "ws-b", "kaban", KEY, None);
    let only = WorkspaceFilter::Only(ws.as_str().to_owned());

    // resolved Y: the NULL row only (X is another snapshot's scope)
    let for_y = AnchorPredicates {
        workspace: &only,
        repo: "kaban",
        commit: Some(COMMIT_Y),
    };
    assert_eq!(ids(&store, &for_y), vec![unscoped.clone()]);
    // resolved X: the NULL row and the row scoped to X
    let for_x = AnchorPredicates {
        workspace: &only,
        repo: "kaban",
        commit: Some(COMMIT_X),
    };
    let mut want = vec![unscoped.clone(), scoped_x.clone()];
    want.sort();
    assert_eq!(ids(&store, &for_x), want);
    // no resolved commit: every scope (OWN-83 (1)), the scoped row beside the NULL one
    let none = AnchorPredicates {
        workspace: &only,
        repo: "kaban",
        commit: None,
    };
    let mut want = vec![unscoped.clone(), scoped_x.clone()];
    want.sort();
    assert_eq!(ids(&store, &none), want);
    // every workspace: the ws-b row joins; the other repository never does
    let all = AnchorPredicates {
        workspace: &WorkspaceFilter::All,
        repo: "kaban",
        commit: None,
    };
    let mut want = vec![unscoped, scoped_x, other_ws];
    want.sort();
    assert_eq!(ids(&store, &all), want);
}

/// The `coderefs_by_symbol` shape after its `SELECT` list (the `commit: None`, `Only(ws)`
/// case: no scope clause, OWN-83 (1)), mirrored from `anchors.rs` `coderefs_by_symbol` (the `format!` at the `let sql`;
/// the comment above it names this constant). The production statement is `pub(crate)` and
/// builds its clauses from the predicates, so an integration test cannot share it; the two
/// change together.
const RECALL_REF_SHAPE: &str = "FROM node_anchor a CROSS JOIN node c ON c.rid = a.rid \
     WHERE a.anchor_kind = 'symbol' AND a.key = ?1 AND a.repo = ?2 \
     AND a.workspace_id = ?3 AND c.kind = 'CodeRef' AND c.lifecycle = 'active' ORDER BY c.rid";

/// The `EXPLAIN QUERY PLAN` detail rows of `SELECT c.id {shape}` on a read-only second
/// connection (the projection does not move the plan: `node` is reached by rowid either way).
fn plan_of(conn: &Connection, shape: &str, ws: &str) -> Vec<String> {
    let mut st = conn
        .prepare(&format!("EXPLAIN QUERY PLAN SELECT c.id {shape}"))
        .unwrap();
    st.query_map(params![KEY, "kaban", ws], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// G1-F10 verify (E2.6c, the CG5.3 stop closed): the plan of the engine's `coderefs_by_symbol`
/// shape names the `node_anchor_key` index (`migrations/0004_engine.sql:111`) on an empty
/// store and on one holding 100 symbol anchors; the assertion is on the plan, not on the SQL
/// text. The store never runs `ANALYZE`, so the planner has no `sqlite_stat1` to go on and
/// the statement itself has to drive from `node_anchor` (`CROSS JOIN`).
///
/// The negative case is the pre-E2.6c shape, a plain `JOIN`: on the same store the planner
/// drives it from `node_kind` and never reaches `node_anchor_key`, which is the defect the
/// `CROSS JOIN` removes. Should a later bundled SQLite plan the plain `JOIN` from the index
/// too, that assertion is the one to drop: the pin would then be belt and braces.
///
/// The rows themselves are unchanged by the join order: the fixture's `KEY` row is the one
/// the store returns among the 100 (the predicate set is `coderefs_by_symbol_predicates_v4`).
#[test]
fn recall_ref_uses_index() {
    let s = Scratch::new("cg53-anchors", "index");
    let store = open(&s);
    let ws = WorkspaceId::default();
    let conn = read_only(&s);
    let old_shape = RECALL_REF_SHAPE.replacen("CROSS JOIN", "JOIN", 1);

    // 0 rows: the empty store is where a plain `JOIN` has nothing to guide it
    let plan = plan_of(&conn, RECALL_REF_SHAPE, ws.as_str());
    assert!(
        plan.iter().any(|row| row.contains("node_anchor_key")),
        "empty store: the plan does not use the anchor index: {plan:?}"
    );
    let old = plan_of(&conn, &old_shape, ws.as_str());
    assert!(
        !old.iter().any(|row| row.contains("node_anchor_key")),
        "empty store: the pre-E2.6c JOIN shape now uses the anchor index; the CROSS JOIN pin can be revisited: {old:?}"
    );

    // 100 rows: 99 other symbols and the one under test; still no statistics
    for i in 0..99 {
        coderef(&store, ws.as_str(), "kaban", &format!("{KEY}{i}"), None);
    }
    let wanted = coderef(&store, ws.as_str(), "kaban", KEY, None);
    let plan = plan_of(&conn, RECALL_REF_SHAPE, ws.as_str());
    assert!(
        plan.iter().any(|row| row.contains("node_anchor_key")),
        "100 rows: the plan does not use the anchor index: {plan:?}"
    );
    let old = plan_of(&conn, &old_shape, ws.as_str());
    assert!(
        !old.iter().any(|row| row.contains("node_anchor_key")),
        "100 rows: the pre-E2.6c JOIN shape now uses the anchor index; the CROSS JOIN pin can be revisited: {old:?}"
    );

    // the same rows through the store
    let only = WorkspaceFilter::Only(ws.as_str().to_owned());
    let p = AnchorPredicates {
        workspace: &only,
        repo: "kaban",
        commit: None,
    };
    assert_eq!(ids(&store, &p), vec![wanted]);
}

fn event() -> NewEvent {
    NewEvent {
        reason: "seeded".into(),
        signer_kind: SignerKind::System,
        ..Default::default()
    }
}

/// A Decision in `workspace` (the default one when `None`).
fn decision(store: &SqliteStore, workspace: Option<&str>, text: &str) -> Node {
    match store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: text.into(),
                workspace_id: workspace.map(str::to_owned),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// Accept `prev`, then remember a Decision `text` that `SUPERSEDES` it (the
/// `src/sqlite/tests.rs` pattern: only an accepted Decision can be superseded); the new node.
fn supersede(store: &SqliteStore, prev: &str, text: &str) -> Node {
    store
        .transition(&nid(prev), Governance::Accepted, event(), &ctx())
        .unwrap();
    match store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: text.into(),
                ..Default::default()
            },
            &[LinkReq {
                rel: "SUPERSEDES".into(),
                to: Some(nid(prev)),
                ..Default::default()
            }],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh text: {node:?}"),
    }
}

fn chain(
    store: &SqliteStore,
    seed: &str,
    direction: ChainDirection,
    limit: usize,
    ws: &WorkspaceFilter,
) -> Chain {
    store
        .supersession_chain(&nid(seed), direction, limit, ws)
        .unwrap()
}

/// RS-9, S6 (the brief's `history_hop_in_other_workspace_cut`, security tier): A in `ws-b`
/// supersedes B in `default`. The store refuses a cross-workspace link through every write
/// path (R3-3), so the fixture writes the one edge row the walk must defend against on a
/// second connection. From B under `Only(default)` the successor is cut and counted, never
/// returned; from A under `Only(ws-b)` the predecessor is cut the same way; under `All` both
/// are returned; a seed outside the filter is `NotFound`.
#[test]
fn history_hop_in_other_workspace_cut() {
    let s = Scratch::new("cg53-anchors", "history-hop");
    let store = open(&s);
    let b = decision(&store, None, "B: deploy pipeline uses blue green");
    let a = decision(&store, Some("ws-b"), "A: deploy pipeline uses canary now");
    {
        let conn = Connection::open(s.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        let rid = |id: &str| -> i64 {
            conn.query_row("SELECT rid FROM node WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        conn.execute(
            "INSERT INTO edge(id, src, dst, rel, created_at, props) VALUES (?1, ?2, ?3, 'SUPERSEDES', ?4, '{}')",
            params![new_id(), rid(&a.id), rid(&b.id), now_iso()],
        )
        .unwrap();
    }
    let default = WorkspaceFilter::Only(WorkspaceId::default().as_str().to_owned());
    let ws_b = WorkspaceFilter::Only("ws-b".into());

    let got = chain(&store, &b.id, ChainDirection::Successors, 10, &default);
    assert_eq!(
        (got.ids.as_slice(), got.cut, got.cut_by_workspace),
        (&[][..], true, 1),
        "the successor in ws-b is cut and counted, never returned: {got:?}"
    );
    let got = chain(&store, &a.id, ChainDirection::Predecessors, 10, &ws_b);
    assert_eq!(
        (got.ids.as_slice(), got.cut, got.cut_by_workspace),
        (&[][..], true, 1),
        "the predecessor in default is cut the same way: {got:?}"
    );
    // every workspace: the chain is whole in both directions
    let got = chain(
        &store,
        &a.id,
        ChainDirection::Predecessors,
        10,
        &WorkspaceFilter::All,
    );
    assert_eq!(got.ids, vec![nid(&b.id)]);
    assert!(!got.cut && got.cut_by_workspace == 0, "{got:?}");
    let got = chain(
        &store,
        &b.id,
        ChainDirection::Successors,
        10,
        &WorkspaceFilter::All,
    );
    assert_eq!(got.ids, vec![nid(&a.id)]);
    // a seed outside the filter is as good as absent (S6), unknown is `NotFound` too
    assert!(matches!(
        store.supersession_chain(&nid(&a.id), ChainDirection::Predecessors, 10, &default),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.supersession_chain(
            &kaban_core::NodeId::new(),
            ChainDirection::Successors,
            10,
            &default
        ),
        Err(StoreError::NotFound(_))
    ));
}

/// The `limit` cut (feature tier): 52 Decisions chained by `SUPERSEDES` (51 hops). From the
/// newest, `Predecessors` with `limit: 50` returns 50 ids nearest first, `cut: true`,
/// `cut_by_workspace: 0`; a limit above the chain returns every hop with `cut: false`; the
/// archived hops are returned (they are the chain), and `Successors` from the oldest is the
/// mirror.
#[test]
fn chain_limit_cuts() {
    let s = Scratch::new("cg53-anchors", "limit");
    let store = open(&s);
    let mut ids: Vec<String> = vec![decision(&store, None, "chain step 0").id];
    for i in 1..52 {
        let prev = ids[i - 1].clone();
        ids.push(supersede(&store, &prev, &format!("chain step {i}")).id);
    }
    let newest = ids.last().unwrap();
    let default = WorkspaceFilter::Only(WorkspaceId::default().as_str().to_owned());

    let got = chain(&store, newest, ChainDirection::Predecessors, 50, &default);
    let want: Vec<_> = ids[1..51].iter().rev().map(|id| nid(id)).collect();
    assert_eq!(got.ids.len(), 50);
    assert_eq!(got.ids, want, "nearest first, the oldest step cut");
    assert!(got.cut, "{got:?}");
    assert_eq!(got.cut_by_workspace, 0);

    let got = chain(&store, newest, ChainDirection::Predecessors, 100, &default);
    let want: Vec<_> = ids[..51].iter().rev().map(|id| nid(id)).collect();
    assert_eq!(got.ids, want, "every hop, the archived ones included");
    assert!(!got.cut, "{got:?}");

    let got = chain(&store, &ids[0], ChainDirection::Successors, 100, &default);
    let want: Vec<_> = ids[1..].iter().map(|id| nid(id)).collect();
    assert_eq!(got.ids, want);
    assert!(!got.cut);
    // the archived seed is allowed: a superseded record is the usual seed of its history
    let got = chain(&store, &ids[25], ChainDirection::Successors, 1, &default);
    assert_eq!((got.ids.len(), got.cut), (1, true));
}
