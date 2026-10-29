//! The API half of v4 stage1-plan CG5.3: `anchor_conformance_v4` (cg-join-v1 §5, §11; SO-2) reads
//! the `node_anchor` rows the engine writes for a `CodeRef` remembered through `remember`'s
//! `refs` on a second read-only connection and asserts them byte for byte, including the SO-2
//! canonicalisation of a short `snapshot_commit`; and the RU3-7 same-key cases of
//! `derive_symbol` (two rows of one file sharing a key; one row whose key another file also
//! defines). The snapshot is a `FixedSnapshot` over a file this suite writes itself (the
//! `remember_symbol.rs` pattern, smaller). Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use common::Scratch;
use kaban_api::{ApiConfig, ApiError, CodeRefWire, KabanApi, RememberRequest};
use kaban_codegraph::testing::FixedSnapshot;
use kaban_codegraph::write::{FileRow, SymbolRow};
use kaban_codegraph::{FileId, NodeId, NodeKind, Range, SCHEMA_VERSION, SnapshotProvider, write};
use kaban_core::{MemoryStore, TraverseQuery};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;

/// The fixture snapshot's commit (40 hex).
const SNAPSHOT: &str = "abababababababababababababababababababab";
/// The join's repository identity (`ApiConfig.repo`).
const REPO: &str = "kaban";
/// Keys of the fixture (stage1-spec §4.2 normalised form).
const OPEN_KEY: &str = "scip-go gomod example.com/m . store/Store#Open().";
const CLOSE_KEY: &str = "scip-go gomod example.com/m . store/Pool#Close().";
const UNIQUE_KEY: &str = "scip-go gomod example.com/m . store/Pool#Reset().";
/// Keys a conforming indexer never stores (RW1-3: the snapshot's admission checks file and
/// schema integrity, not key form): a versioned form and a document-local form, each the one
/// key of its name in the snapshot.
const VERSIONED_KEY: &str = "scip-go gomod example.com/m v1.0.0 store/Pool#Flush().";
const LOCAL_KEY: &str = "local 3";

/// `(anchor_kind, key, repo, snapshot_scope)` of one `node_anchor` row.
type AnchorRow = (String, String, Option<String>, Option<String>);

fn symbol_row(file: i64, scip: &str, key: &str, name: &str) -> SymbolRow {
    SymbolRow {
        id: NodeId(0),
        kind: NodeKind::Symbol,
        r#ref: format!("scip:{scip}"),
        scip: Some(scip.to_owned()),
        key: Some(key.to_owned()),
        label: "Method".into(),
        name: name.into(),
        file_id: Some(FileId(file)),
        def: Some(Range {
            sl: 1,
            sc: 0,
            el: 1,
            ec: 4,
        }),
        enc: None,
        signature: None,
        doc: None,
        external: false,
        is_test: false,
        ident_terms: name.to_lowercase(),
    }
}

/// A snapshot with `src/store.go` (file 2: `Open` in two package versions sharing `OPEN_KEY`,
/// `Close` with `CLOSE_KEY`, `Reset` with `UNIQUE_KEY`, `Flush` with the nonconforming
/// `VERSIONED_KEY` and `Drain` with the nonconforming `LOCAL_KEY`) and `src/other.go` (file 1:
/// a second `Close` row with the same `CLOSE_KEY`); files sorted by path as the writer requires.
fn write_snapshot(dir: &Path) -> PathBuf {
    let path = dir.join(format!("{SNAPSHOT}.cg.db"));
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap(),
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    write::begin(&conn).unwrap();
    let file = |id: i64, path: &str| FileRow {
        id: FileId(id),
        path: path.into(),
        language: "go".into(),
        is_test: false,
        lines: 40,
    };
    write::insert_files(&conn, &[file(1, "src/other.go"), file(2, "src/store.go")]).unwrap();
    let mut rows = vec![
        symbol_row(
            2,
            "scip-go gomod example.com/m v1.0.0 store/Store#Open().",
            OPEN_KEY,
            "Open",
        ),
        symbol_row(
            2,
            "scip-go gomod example.com/m v1.1.0 store/Store#Open().",
            OPEN_KEY,
            "Open",
        ),
        symbol_row(
            2,
            "scip-go gomod example.com/m v1.0.0 store/Pool#Close().",
            CLOSE_KEY,
            "Close",
        ),
        symbol_row(
            2,
            "scip-go gomod example.com/m v1.0.0 store/Pool#Reset().",
            UNIQUE_KEY,
            "Reset",
        ),
        symbol_row(
            1,
            "scip-go gomod example.com/m v1.2.0 store/Pool#Close().",
            CLOSE_KEY,
            "Close",
        ),
        symbol_row(2, VERSIONED_KEY, VERSIONED_KEY, "Flush"),
        symbol_row(2, LOCAL_KEY, LOCAL_KEY, "Drain"),
    ];
    rows.sort_by(|a, b| a.r#ref.as_bytes().cmp(b.r#ref.as_bytes()));
    for (i, r) in rows.iter_mut().enumerate() {
        r.id = NodeId(i64::try_from(i + 1).unwrap());
    }
    write::insert_symbols(&conn, &rows).unwrap();
    write::insert_edges(&conn, &[]).unwrap();
    write::insert_occurrences(&conn, &[]).unwrap();
    write::insert_snippets(&conn, &[]).unwrap();
    write::insert_edge_types(&conn, &[]).unwrap();
    let schema = SCHEMA_VERSION.to_string();
    write::insert_meta(
        &conn,
        &[
            ("commit", SNAPSHOT),
            ("isolation", "none"),
            ("policy_version", "pv-cg53"),
            ("repo_id", "0123456789abcdef0123456789abcdef"),
            ("schema_version", schema.as_str()),
            ("symbols_fts_version", "1"),
        ],
    )
    .unwrap();
    write::rebuild_fts(&conn).unwrap();
    write::finish(&conn).unwrap();
    path
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
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

/// A repository with one commit (the head the derivation fetches); its root.
fn repo_with_commit(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "one").unwrap();
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "one"]);
    root
}

/// The service bound to repository `kaban`; with `snapshot`, the fixture provider and a head
/// repository so refs resolve. The shipped builtin policy.
fn service(s: &Scratch, snapshot: bool) -> (Arc<SqliteStore>, KabanApi) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
    let repo_root = snapshot.then(|| repo_with_commit(&s.0));
    let mut api = KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            repo: Some(REPO.into()),
            repo_root,
            ..Default::default()
        },
    );
    if snapshot {
        let provider =
            Arc::new(FixedSnapshot::new(write_snapshot(&s.0))) as Arc<dyn SnapshotProvider>;
        api = api.with_snapshot_provider(s.0.join("codegraph"), provider);
    }
    (store, api)
}

/// A ref in `src/store.go` with no `symbol`, no `name_path` and no `snapshot_commit`.
fn wire() -> CodeRefWire {
    CodeRefWire {
        repo: REPO.into(),
        commit: "8d10f88".into(),
        tool: "manual".into(),
        relative_path: "src/store.go".into(),
        kind: Some("Method".into()),
        ..Default::default()
    }
}

/// A `Fact` with a text of its own, so each call links its refs from a new node.
fn note(refs: Vec<CodeRefWire>) -> RememberRequest {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    RememberRequest {
        kind: "Fact".into(),
        text: format!("note {n} about the store"),
        refs,
        ..Default::default()
    }
}

/// The one `CodeRef` linked ABOUT from the note that remembered `w`.
fn one_ref(store: &SqliteStore, api: &KabanApi, w: CodeRefWire) -> kaban_core::Node {
    let r = api.remember(note(vec![w])).unwrap();
    let t = store
        .traverse(&TraverseQuery {
            start_ids: vec![kaban_core::NodeId::parse(&r.id).unwrap()],
            rels: vec!["ABOUT".into()],
            direction: kaban_core::TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec!["CodeRef".into()],
            governance: vec![],
            limit: 100,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    let refs: Vec<_> = t
        .nodes
        .into_iter()
        .filter(|n| n.kind == "CodeRef")
        .collect();
    assert_eq!(refs.len(), 1);
    refs.into_iter().next().unwrap()
}

fn invalid_text(r: Result<kaban_api::RememberResponse, ApiError>) -> String {
    match r {
        Err(ApiError::Invalid(msg)) => msg,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

/// `(anchor_kind, key, repo, snapshot_scope)` of the node's rows, on a second read-only
/// connection, by kind then key.
fn anchors(s: &Scratch, id: &str) -> Vec<AnchorRow> {
    let conn = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.prepare(
        "SELECT a.anchor_kind, a.key, a.repo, a.snapshot_scope FROM node_anchor a
         JOIN node n ON n.rid = a.rid WHERE n.id = ?1 ORDER BY a.anchor_kind, a.key",
    )
    .unwrap()
    .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn anchor_rows(s: &Scratch) -> i64 {
    let conn = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT count(*) FROM node_anchor", [], |r| r.get(0))
        .unwrap()
}

/// cg-join-v1 §5, §11 (SO-2; brief default 5: the API half lives here because the store crate
/// has no `kaban-api` dev-dependency): a `CodeRef` with `symbol` and `snapshot_commit`
/// remembered through `refs` has a `symbol` row whose `key` is byte-equal to the stored
/// `props.symbol` and a `path` row keyed by `relative_path`, both with `repo = ApiConfig.repo`
/// and `snapshot_scope = props.snapshot_commit`; without `symbol` the `path` row stands alone
/// with the scope kept; a short `snapshot_commit` arrives as the resolved 40 hex in
/// `snapshot_scope`; a non-matching short value is refused before any row.
#[test]
fn anchor_conformance_v4() {
    // --- explicit symbol and 40-hex commit, no snapshot needed
    let s = Scratch::new("kaban-cg53", "conformance");
    let (store, api) = service(&s, false);
    let full = "cd".repeat(20);
    let mut w = wire();
    w.symbol = Some(OPEN_KEY.into());
    w.snapshot_commit = Some(full.clone());
    let n = one_ref(&store, &api, w);
    let stored_symbol = n.props["symbol"].as_str().unwrap().to_owned();
    let stored_commit = n.props["snapshot_commit"].as_str().unwrap().to_owned();
    assert_eq!(stored_commit, full);
    let rows = anchors(&s, &n.id);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let path = &rows[0];
    let symbol = &rows[1];
    assert_eq!(symbol.0, "symbol");
    assert_eq!(
        symbol.1.as_bytes(),
        stored_symbol.as_bytes(),
        "key byte-equal to props.symbol"
    );
    assert_eq!((path.0.as_str(), path.1.as_str()), ("path", "src/store.go"));
    for row in &rows {
        assert_eq!(
            row.2.as_deref(),
            Some(REPO),
            "repo = ApiConfig.repo: {row:?}"
        );
        assert_eq!(
            row.3.as_deref(),
            Some(stored_commit.as_str()),
            "scope on both rows: {row:?}"
        );
    }
    // --- no symbol: the path row only, the scope kept
    let mut w = wire();
    w.relative_path = "src/other.go".into();
    w.snapshot_commit = Some(full.clone());
    let n = one_ref(&store, &api, w);
    assert!(n.props.get("symbol").is_none());
    assert_eq!(
        anchors(&s, &n.id),
        vec![(
            "path".to_owned(),
            "src/other.go".to_owned(),
            Some(REPO.to_owned()),
            Some(full.clone())
        )]
    );
    // --- no snapshot_commit either: NULL scope on the row
    let n = one_ref(&store, &api, wire());
    assert_eq!(
        anchors(&s, &n.id),
        vec![(
            "path".to_owned(),
            "src/store.go".to_owned(),
            Some(REPO.to_owned()),
            None
        )]
    );

    // --- a short prefix through the resolved snapshot: 40 hex in snapshot_scope (SO-2)
    let s2 = Scratch::new("kaban-cg53", "conformance-short");
    let (store2, api2) = service(&s2, true);
    let mut w = wire();
    w.snapshot_commit = Some(SNAPSHOT[..7].into());
    let n = one_ref(&store2, &api2, w);
    assert_eq!(n.props["snapshot_commit"], json!(SNAPSHOT));
    assert_eq!(
        anchors(&s2, &n.id),
        vec![(
            "path".to_owned(),
            "src/store.go".to_owned(),
            Some(REPO.to_owned()),
            Some(SNAPSHOT.to_owned())
        )]
    );
    // --- a non-matching short value is refused before any row
    let before = (
        anchor_rows(&s2),
        store2.stats(None).unwrap()["nodes"].clone(),
    );
    let mut w = wire();
    w.snapshot_commit = Some("0000000".into());
    assert_eq!(
        invalid_text(api2.remember(note(vec![w]))),
        "refs[].snapshot_commit"
    );
    let after = (
        anchor_rows(&s2),
        store2.stats(None).unwrap()["nodes"].clone(),
    );
    assert_eq!(after, before, "no row and no node from a refused ref");
}

/// RU3-7 (cg-join-v1 §6): exactly one eligible row of the file is the candidate, then
/// `symbol_unique` confirms the key snapshot-wide. Two rows of one file sharing a key are
/// `ambiguous_symbol` (the base deduplicated them to one key); one row whose key another file
/// also defines is `ambiguous_symbol` through the confirmation; a key defined once is derived
/// with the resolved commit.
#[test]
fn remember_symbol_two_rows_one_key_ambiguous() {
    let s = Scratch::new("kaban-cg53", "two-rows-one-key");
    let (store, api) = service(&s, true);
    // two rows in src/store.go share OPEN_KEY
    let mut w = wire();
    w.name_path = Some("Store/Open".into());
    assert_eq!(
        invalid_text(api.remember(note(vec![w]))),
        "refs[].name_path: ambiguous_symbol"
    );
    // one row in src/store.go, but src/other.go defines the same key
    let mut w = wire();
    w.name_path = Some("Pool/Close".into());
    assert_eq!(
        invalid_text(api.remember(note(vec![w]))),
        "refs[].name_path: ambiguous_symbol"
    );
    assert_eq!(
        store.stats(None).unwrap()["nodes"],
        json!(0),
        "nothing stored"
    );
    // a key defined once in the snapshot is derived
    let mut w = wire();
    w.name_path = Some("Pool/Reset".into());
    let n = one_ref(&store, &api, w);
    assert_eq!(n.props["symbol"], json!(UNIQUE_KEY));
    assert_eq!(n.props["snapshot_commit"], json!(SNAPSHOT));
    let rows = anchors(&s, &n.id);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].1, UNIQUE_KEY);
}

/// RW1-3 (stage1-spec §4.2; RU3-6; the kept red `e25d-red-derived-form.log`): a key derived
/// from an admitted snapshot passes the key-form rule an explicit `symbol` passes. The unique
/// row for `Pool/Flush` carries a versioned key and the one for `Pool/Drain` a document-local
/// key: `remember` without an explicit `symbol` is refused with the fixed class
/// `symbol_invalid` (the key never echoed), and no node and no anchor row is stored.
#[test]
fn remember_derived_symbol_of_a_nonconforming_key_form_refused() {
    // Arrange
    let s = Scratch::new("kaban-cg53", "derived-form");
    let (store, api) = service(&s, true);

    for name_path in ["Pool/Flush", "Pool/Drain"] {
        let mut w = wire();
        w.name_path = Some(name_path.into());

        // Act
        let r = api.remember(note(vec![w]));

        // Assert
        assert!(
            matches!(&r, Err(ApiError::Invalid(msg)) if msg == "refs[].symbol: symbol_invalid"),
            "{name_path}: a derived key of a nonconforming form is refused with the fixed class, got {r:?}"
        );
    }
    assert_eq!(anchor_rows(&s), 0, "no anchor row from a refused ref");
    assert_eq!(
        store.stats(None).unwrap()["nodes"],
        json!(0),
        "no node from a refused ref"
    );
}
