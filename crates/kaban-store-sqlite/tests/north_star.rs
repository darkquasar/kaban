//! The north-star queries at the store seam (engine-spec EN-F21, EN-F10, EN-F19; engine-plan
//! E2.7): the text side ("which Decisions govern this path and are they superseded") over the
//! `node_anchor` path prefilter of `governing` and the `SUPERSEDES` chain, the prefilter's
//! identity with the `json_each` scan on the 0003 fixture, its plan, and `traverse` with
//! `follow_anchors` over a §10 fixture snapshot on the dedicated read connection. Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "../../kaban-api/tests/common/cg_fixture.rs"]
mod cg_fixture;
mod common;

use std::path::Path;

use cg_fixture::{AFERO_COMMIT, AFERO_KEY, AFERO_OTHER_PATH, AFERO_PATH, Variant};
use common::{Scratch, ctx, nid};
use kaban_codegraph::validate_for_attach;
use kaban_core::{
    AnchoredKind, ChainDirection, Governance, GovernanceStore, GoverningQuery, IngestStore,
    MemoryStore, NewEvent, NewNode, Node, RedactedStrings, RememberOutcome, RescanAction,
    SnapshotAttach, TraverseDirection, TraverseQuery, TraverseResult, WorkspaceFilter,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::json;

const REPO: &str = "kaban";

/// A proposed Decision scoped by `globs` in `workspace`; its id.
fn decision(store: &SqliteStore, text: &str, globs: &[&str], workspace: Option<&str>) -> String {
    match store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: text.into(),
                props: Some(json!({"decision": text, "rationale": "because"})),
                path_scope: globs.iter().map(|g| (*g).to_owned()).collect(),
                workspace_id: workspace.map(str::to_owned),
                repo: Some(REPO.into()),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n.id,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// Accept `id` with `globs` as its scope (the acceptance event carries the scope).
fn accept(store: &SqliteStore, id: &str, globs: &[&str]) {
    store
        .transition(
            &nid(id),
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                path_scope: globs.iter().map(|g| (*g).to_owned()).collect(),
                ..Default::default()
            },
            &ctx(),
        )
        .unwrap();
}

/// The target and its ancestor directories, as `kaban-api/src/why.rs` builds them.
fn path_candidates(target: &str) -> Vec<String> {
    let mut out = vec![target.to_owned()];
    let mut rest = target;
    while let Some(i) = rest.rfind('/') {
        rest = &rest[..i];
        if !rest.is_empty() {
            out.push(rest.to_owned());
        }
    }
    out
}

/// The ids `governing` returns for a path `target` (accepted and proposed Decisions), with
/// the `[why] anchor_prefilter` switch on.
fn governing_ids(store: &SqliteStore, target: &str, ws: &WorkspaceFilter) -> Vec<String> {
    governing_ids_with(store, target, ws, true)
}

/// [`governing_ids`] with the switch as given.
fn governing_ids_with(
    store: &SqliteStore,
    target: &str,
    ws: &WorkspaceFilter,
    anchor_prefilter: bool,
) -> Vec<String> {
    let mut ids: Vec<String> = store
        .governing(&GoverningQuery {
            kinds: vec!["Decision".into()],
            governance: vec![Governance::Accepted, Governance::Proposed],
            workspace: ws.clone(),
            path_candidates: path_candidates(target),
            query: None,
            limit: 1_000,
            offset: 0,
            filters: kaban_core::ReadFilters::default(),
            anchor_prefilter,
        })
        .unwrap()
        .into_iter()
        .map(|n| n.id)
        .collect();
    ids.sort();
    ids
}

fn read_only(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// EN-F10 / EN-F21 (the staged red of E2.7 item 3, default 4): a record scoped `src/api/**`
/// anchors the literal prefix `src/api/` and one scoped only by `**/*.rs` anchors the universal
/// key `""` (E1.7a RE2-2; plan :1187 names this row mandatory). Neither key is the target, an
/// ancestor, or a GLOB pattern the target matches, so the spec's literal form `key IN (<path
/// and ancestors>) OR ? GLOB key` loses both; the byte-prefix set keeps them, and keeps the
/// exact `docs/adr-1.md` row for its own target. `src/apix/**` never matches `src/api/...`
/// and a `ws-b` row never joins an `Only(default)` read.
#[test]
fn prefilter_keeps_prefix_universal_and_exact_rows() {
    let s = Scratch::new("e27-north-star", "prefilter");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let by_prefix = decision(
        &store,
        "api handlers gate every string",
        &["src/api/**"],
        None,
    );
    let universal = decision(&store, "rust files carry a module doc", &["**/*.rs"], None);
    let exact = decision(&store, "adr one is binding", &["docs/adr-1.md"], None);
    let _near_miss = decision(&store, "apix is another module", &["src/apix/**"], None);
    let _other_ws = decision(
        &store,
        "ws-b api handlers do something else",
        &["src/api/**"],
        Some("ws-b"),
    );
    let only_default = WorkspaceFilter::Only("default".into());

    let mut want = vec![by_prefix.clone(), universal.clone()];
    want.sort();
    assert_eq!(
        governing_ids(&store, "src/api/recall.rs", &only_default),
        want,
        "the prefix row and the universal row govern src/api/recall.rs"
    );
    assert_eq!(
        governing_ids(&store, "docs/adr-1.md", &only_default),
        vec![exact.clone()],
        "the exact row governs its own path and nothing else does"
    );
    // every workspace: the ws-b row joins for its target
    assert_eq!(
        governing_ids(&store, "src/api/recall.rs", &WorkspaceFilter::All).len(),
        3,
        "All admits the ws-b row"
    );
}

/// EN-F10 at the acceptance seam (the second red of E2.7 item 3, kept as
/// `e27-red-acceptance-scope-anchor.log`): a Decision remembered without a scope and scoped at
/// acceptance (`NewEvent.path_scope`, the `kaban accept` path) gets its `path` anchor rows in
/// the acceptance transaction, so the prefilter keeps it; a re-acceptance with another scope
/// moves them; its supersession keeps the rows (archived, reachable on request) while the
/// record itself leaves `governing` through its lifecycle.
#[test]
fn prefilter_keeps_a_record_scoped_at_acceptance() {
    let s = Scratch::new("e27-north-star", "accept-scope");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let only_default = WorkspaceFilter::Only("default".into());
    let d = decision(&store, "api handlers gate every string", &[], None);
    assert!(
        governing_ids(&store, "src/api/recall.rs", &only_default).is_empty(),
        "unscoped: governs nothing"
    );
    accept(&store, &d, &["src/api/**"]);
    assert_eq!(
        governing_ids(&store, "src/api/recall.rs", &only_default),
        vec![d.clone()],
        "scoped at acceptance: found through the prefilter"
    );
    assert_eq!(
        governing_ids(&store, "docs/adr-1.md", &only_default),
        Vec::<String>::new(),
        "another path: not governed"
    );

    let successor = decision(
        &store,
        "api handlers gate every string, typed",
        &["docs/**"],
        None,
    );
    store
        .link(&nid(&successor), &nid(&d), "SUPERSEDES", None, None, &ctx())
        .unwrap();
    assert!(
        governing_ids(&store, "src/api/recall.rs", &only_default).is_empty(),
        "superseded: archived, governs nothing"
    );
    assert_eq!(
        governing_ids(&store, "docs/adr-1.md", &only_default),
        vec![successor]
    );
}

/// EN-F10 "every node write" includes the policy rescan (E2.7b; the E2.7 residual): a
/// redaction that masks a `path_scope` glob rewrites the row, and the `node_anchor` path key
/// follows it in the same transaction. A stale key is a wrong `why` answer: the prefilter of
/// `governing` keeps the record for the old prefix only, so the masked prefix never finds it.
#[test]
fn rescan_redaction_moves_the_anchor_key_with_the_glob() {
    let s = Scratch::new("e27b-north-star", "rescan-rekey");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let d = decision(&store, "the api scope", &["src/api/**"], None);
    accept(&store, &d, &["src/api/**"]);
    let all = WorkspaceFilter::All;
    assert_eq!(
        governing_ids(&store, "src/api/recall.rs", &all),
        vec![d.clone()]
    );
    assert_eq!(
        governing_ids(&store, "src/masked/recall.rs", &all),
        Vec::<String>::new()
    );

    let report = store
        .rescan("pv-masked", 100, &mut |n: &Node| {
            if n.id != d {
                return RescanAction::Keep;
            }
            RescanAction::Redact {
                strings: Box::new(RedactedStrings {
                    text: n.text.clone(),
                    summary: n.summary.clone(),
                    tags: n.tags.clone(),
                    props: n.props.clone(),
                    path_scope: vec!["src/masked/**".into()],
                    session_id: n.session_id.clone(),
                    agent_id: n.agent_id.clone(),
                    repo: n.repo.clone(),
                    commit_sha: n.commit_sha.clone(),
                    accepted_commit: n.accepted_commit.clone(),
                    dup_key: n.dup_key.clone(),
                }),
                audit: Vec::new(),
            }
        })
        .unwrap();
    assert_eq!(report.redacted, 1, "{report:?}");
    // the key followed the glob: the masked prefix finds the record, the old one no longer
    assert_eq!(
        governing_ids(&store, "src/masked/recall.rs", &all),
        vec![d.clone()],
        "the anchor key did not follow the masked glob"
    );
    assert_eq!(
        governing_ids(&store, "src/api/recall.rs", &all),
        Vec::<String>::new()
    );
}

/// The `governing` path shape after its `SELECT` list under the prefilter (`Only(ws)`, the
/// shipped exclusion of candidates), mirrored from `sqlite/governance.rs` `governing_impl`
/// (`ANCHOR_PREFILTER`, `ANCHOR_PREFILTER_FALLTHROUGH` and the `json_each` EXISTS that follows them). The production statement
/// is built from the query, so an integration test cannot share it; the two change together.
const GOVERNING_PATH_SHAPE: &str = "FROM node n WHERE n.lifecycle = 'active' AND n.kind IN (?1) \
     AND n.governance IN (?2,?3) AND NOT EXISTS (SELECT 1 FROM node_candidate c WHERE c.rid = n.rid) \
     AND n.workspace_id = ?4 \
     AND (n.rid IN (SELECT a.rid FROM node_anchor a WHERE a.anchor_kind = 'path' \
         AND a.key IN (SELECT value FROM json_each(?5)) AND a.workspace_id = ?4) \
         OR NOT EXISTS (SELECT 1 FROM node_anchor a2 WHERE a2.rid = n.rid AND a2.anchor_kind = 'path')) \
     AND EXISTS (SELECT 1 FROM json_each(n.path_scope) ps WHERE ps.value IN (?6,?7,?8) \
     OR (instr(ps.value, '*') > 0 OR instr(ps.value, '?') > 0 OR instr(ps.value, '[') > 0 \
     OR instr(ps.value, '{') > 0) AND (?6 GLOB ps.value OR instr(ps.value, '{') > 0 \
     OR instr(ps.value, '[') > 0)) \
     ORDER BY CASE n.governance WHEN 'accepted' THEN 0 ELSE 1 END, COALESCE(n.importance, 0) DESC, \
     n.created_at DESC, n.id LIMIT ?9 OFFSET ?10";

/// The `EXPLAIN QUERY PLAN` detail rows of `SELECT n.id {shape}` on a read-only second
/// connection (the projection does not move the plan).
fn plan_of(conn: &Connection, shape: &str) -> Vec<String> {
    let mut st = conn
        .prepare(&format!("EXPLAIN QUERY PLAN SELECT n.id {shape}"))
        .unwrap();
    let keys = serde_json::to_string(&[
        "",
        "s",
        "sr",
        "src",
        "src/",
        "src/a",
        "src/ap",
        "src/api",
        "src/api/",
        "src/api/r",
        "src/api/re",
        "src/api/rec",
        "src/api/reca",
        "src/api/recal",
        "src/api/recall",
        "src/api/recall.",
        "src/api/recall.r",
        "src/api/recall.rs",
    ])
    .unwrap();
    st.query_map(
        params![
            "Decision",
            "accepted",
            "proposed",
            "default",
            keys,
            "src/api/recall.rs",
            "src/api",
            "src",
            1_000_i64,
            0_i64
        ],
        |r| r.get::<_, String>(3),
    )
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// E2.7 default 5: the prefilter is planned through `node_anchor_key`
/// (`migrations/0004_engine.sql:111`) and the `node` table is never scanned, at 0 rows and
/// after the 0003 fixture's backfill (the store never runs `ANALYZE`, so the planner has no
/// `sqlite_stat1` and the statement itself has to lead with the index).
#[test]
fn governing_prefilter_uses_index() {
    {
        // a store of its own: the 0003 fixture below cannot be built on a migrated file
        let s = Scratch::new("e27-north-star", "plan-empty");
        let _empty = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        let plan = plan_of(&read_only(&s), GOVERNING_PATH_SHAPE);
        assert!(
            plan.iter().any(|row| row.contains("node_anchor_key")),
            "empty store: the plan does not use the anchor index: {plan:?}"
        );
        assert!(
            !plan.iter().any(|row| row.starts_with("SCAN n")),
            "empty store: the node table is scanned: {plan:?}"
        );
    }
}

/// EN-F21 text side: D1 superseded by D2 superseded by D3, all scoped `src/api/**`. `governing`
/// for `src/api/recall.rs` answers D3 first and never D1 (a superseded record is archived,
/// core-spec §4 M3-11); the `SUPERSEDES` chain from D1 forwards is `[D2, D3]`.
#[test]
fn governing_src_api_answers_the_live_decision_and_the_chain_names_its_successors() {
    let s = Scratch::new("e27-north-star", "chain");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let d1 = decision(&store, "api handlers return json", &[], None);
    accept(&store, &d1, &["src/api/**"]);
    let d2 = decision(&store, "api handlers return typed json", &[], None);
    store
        .link(&nid(&d2), &nid(&d1), "SUPERSEDES", None, None, &ctx())
        .unwrap();
    accept(&store, &d2, &["src/api/**"]);
    let d3 = decision(
        &store,
        "api handlers return typed json with a schema",
        &["src/api/**"],
        None,
    );
    store
        .link(&nid(&d3), &nid(&d2), "SUPERSEDES", None, None, &ctx())
        .unwrap();
    let only_default = WorkspaceFilter::Only("default".into());

    let governing = store
        .governing(&GoverningQuery {
            kinds: vec!["Decision".into()],
            governance: vec![Governance::Accepted, Governance::Proposed],
            workspace: only_default.clone(),
            path_candidates: path_candidates("src/api/recall.rs"),
            query: None,
            limit: 50,
            offset: 0,
            filters: kaban_core::ReadFilters::default(),
            anchor_prefilter: true,
        })
        .unwrap();
    let ids: Vec<&str> = governing.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids.first(), Some(&d3.as_str()), "{ids:?}");
    assert!(
        !ids.contains(&d1.as_str()),
        "a superseded record governs nothing: {ids:?}"
    );
    assert!(!ids.contains(&d2.as_str()), "{ids:?}");

    let chain = store
        .supersession_chain(&nid(&d1), ChainDirection::Successors, 50, &only_default)
        .unwrap();
    assert_eq!(chain.ids, vec![nid(&d2), nid(&d3)]);
    assert!(!chain.cut);
}

/// The admitted attach of `path` (the test-side twin of `kaban-api/src/snapshot.rs`).
fn attach_of(path: &Path) -> SnapshotAttach {
    let uri = validate_for_attach(path).unwrap();
    SnapshotAttach::from_admitted(
        uri.as_sql_param().to_owned(),
        path.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_suffix(".cg.db")
            .unwrap()
            .to_owned(),
        uri.schema_version(),
        format!("{:?}", uri.generation()),
    )
}

/// A `CodeRef` of [`REPO`] anchored by `relative_path` and `symbol`, unscoped; its id.
fn coderef(store: &SqliteStore, relative_path: &str, symbol: Option<&str>) -> String {
    let mut props = json!({
        "repo": REPO, "commit": AFERO_COMMIT, "tool": "fixture",
        "relative_path": relative_path, "name_path": "x", "kind": "Method", "line": 1,
    });
    if let Some(symbol) = symbol {
        props["symbol"] = json!(symbol);
    }
    match store
        .remember(
            NewNode {
                kind: "CodeRef".into(),
                text: format!("{REPO} {relative_path} {}", symbol.unwrap_or("-")),
                props: Some(props),
                repo: Some(REPO.into()),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n.id,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// The node and edge ids of a result (`Node` and `Edge` carry no `PartialEq`).
fn memory_side(r: &TraverseResult) -> (Vec<&str>, Vec<&str>) {
    (
        r.nodes.iter().map(|n| n.id.as_str()).collect(),
        r.edges.iter().map(|e| e.id.as_str()).collect(),
    )
}

fn traverse_query(start: &str, follow_anchors: bool) -> TraverseQuery {
    TraverseQuery {
        start_ids: vec![nid(start)],
        rels: vec![],
        direction: TraverseDirection::Both,
        max_depth: 1,
        node_kinds: vec![],
        governance: vec![],
        limit: 100,
        workspace_id: Some("default".into()),
        follow_anchors,
        anchor_repo: Some(REPO.into()),
        ..Default::default()
    }
}

/// EN-F19 at the store seam (E2.7 item 2): a `path` anchor equal to a `cg.files.path` gives
/// the file card and the file's non-external `symbol` rows; a directory key (`mem/`, the
/// literal prefix of a `mem/**` scope) gives every file under it; a `symbol` key gives its
/// unique row; the universal key `""` and an ambiguous key give nothing; cards are ordered
/// `(path, ref)` and a row reached twice appears once. Flag off, or nothing attached, the
/// result is today's byte for byte.
#[test]
fn follow_anchors_expands_path_and_symbol_anchors_into_the_attached_snapshot() {
    let s = Scratch::new("e27-north-star", "follow");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let by_file = coderef(&store, AFERO_PATH, Some(AFERO_KEY));
    let by_dir = decision(&store, "mem files are tested", &["mem/**", "**/*.rs"], None);
    // the memory edge that makes the two reachable from one start
    store
        .link(&nid(&by_dir), &nid(&by_file), "GOVERNS", None, None, &ctx())
        .unwrap();

    // nothing attached: the flag changes nothing
    let off = store.traverse(&traverse_query(&by_file, false)).unwrap();
    let on_unattached = store.traverse(&traverse_query(&by_file, true)).unwrap();
    assert_eq!(memory_side(&off), memory_side(&on_unattached));
    assert!(off.anchored.is_empty() && on_unattached.anchored.is_empty());

    let built = cg_fixture::build(&s.0, Variant::Afero, AFERO_COMMIT);
    store.attach_snapshot(&attach_of(&built.path)).unwrap();
    let on = store.traverse(&traverse_query(&by_file, true)).unwrap();
    assert_eq!(
        memory_side(&on),
        memory_side(&off),
        "the memory side is unchanged"
    );
    let cards: Vec<(String, AnchoredKind, String, Option<String>)> = on
        .anchored
        .iter()
        .map(|a| (a.from.to_string(), a.kind, a.path.clone(), a.key.clone()))
        .collect();
    let pkg = "scip-go gomod github.com/spf13/afero . ";
    assert_eq!(
        cards,
        vec![
            // kept nodes come in row order: the CodeRef first. Its `memmap.go` key and its
            // `AFERO_KEY` symbol key give the file, its struct and the method, the method
            // once although two anchors reach it
            (by_file.clone(), AnchoredKind::File, AFERO_PATH.into(), None),
            (
                by_file.clone(),
                AnchoredKind::Symbol,
                AFERO_PATH.into(),
                Some(format!("{pkg}MemMapFs#")),
            ),
            (
                by_file.clone(),
                AnchoredKind::Symbol,
                AFERO_PATH.into(),
                Some(AFERO_KEY.into()),
            ),
            // the Decision's `mem/` key (the literal prefix of `mem/**`): the directory's one
            // file and its struct; its universal `""` key (`**/*.rs`) expands nothing
            (
                by_dir.clone(),
                AnchoredKind::File,
                AFERO_OTHER_PATH.into(),
                None
            ),
            (
                by_dir.clone(),
                AnchoredKind::Symbol,
                AFERO_OTHER_PATH.into(),
                Some(format!("{pkg}mem/File#")),
            ),
        ],
        "{:?}",
        on.anchored
    );
    assert!(
        on.anchored
            .iter()
            .filter(|a| a.kind == AnchoredKind::Symbol)
            .all(|a| a.r#ref.is_some() && a.name.is_some()),
        "symbol cards carry ref and name"
    );
    // the flag off against the attached snapshot: still today's result
    let off_attached = store.traverse(&traverse_query(&by_file, false)).unwrap();
    assert_eq!(
        off_attached.anchored.len(),
        0,
        "{:?}",
        off_attached.anchored
    );

    // an ambiguous symbol key gives no symbol card (its path key still gives the file's)
    store.detach_snapshot().unwrap();
    let ambiguous = cg_fixture::build(&s.0, Variant::AferoAmbiguous, AFERO_COMMIT);
    store.attach_snapshot(&attach_of(&ambiguous.path)).unwrap();
    let lone = coderef(&store, "nowhere.go", Some(AFERO_KEY));
    let r = store.traverse(&traverse_query(&lone, true)).unwrap();
    assert!(
        r.anchored.is_empty(),
        "an ambiguous key and an unknown path give nothing: {:?}",
        r.anchored
    );
}
