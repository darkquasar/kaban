//! The dedicated read connection and the `cg` attach (CG-JOIN v1 §2 SO-4, §7 SO-10, §11;
//! engine-plan E2.6): `attach_is_read_only`, `attach_switches_on_commit_change`,
//! `snapshot_symbols_pages_non_external` and the eight-process concurrent attach (E2.6b,
//! `#[ignore]`, run by the clean-room's `--run-ignored only -E 'test(multi_process)'` step).
//! The fixture is the §10 builder, shared with the `kaban-api` suites through `#[path]`;
//! admission is `validate_for_attach` (v4's), the store only ever sees the core-owned
//! `SnapshotAttach`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "../../kaban-api/tests/common/cg_fixture.rs"]
mod cg_fixture;
mod common;

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cg_fixture::{
    AFERO_COMMIT, AFERO_KEY, AFERO_RENAMED_KEY, MANY_PATH, OTHER_COMMIT, REPO_ID, Variant,
};
use common::{Scratch, created, ctx};
use kaban_codegraph::{SnapshotError, validate_for_attach};
use kaban_core::{
    Actor, AnchorPredicates, GovernedSet, IngestStore, MemoryStore, NewNode, NodeId, Op,
    RememberOutcome, SNAPSHOT_SYMBOL_PAGE, SnapshotAttach, StoreError, SymbolPage, WorkspaceFilter,
    WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

/// The admitted attach of `path` (the test-side twin of `kaban-api/src/snapshot.rs`: the
/// store crate has no `Resolved`), with the generation the admission itself read, rendered
/// as `kaban-api/src/snapshot.rs` renders it: the production path end to end. The store
/// checks the file against that generation before every attach (SS-25), so a made-up label
/// would be refused.
fn admitted(path: &Path) -> SnapshotAttach {
    let uri = validate_for_attach(path).unwrap();
    SnapshotAttach::from_admitted(
        uri.as_sql_param().to_owned(),
        commit_of(path),
        uri.schema_version(),
        format!("{:?}", uri.generation()),
    )
}

/// The commit a final snapshot name carries.
fn commit_of(path: &Path) -> String {
    path.file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_suffix(".cg.db")
        .unwrap()
        .to_owned()
}

/// `(dev, ino)` of `path`.
fn ident(path: &Path) -> (u64, u64) {
    let m = std::fs::metadata(path).unwrap();
    (m.dev(), m.ino())
}

/// `(dev, ino)` of every file this process holds open (`/proc/self/fd`, each link followed to
/// its open file, a deleted one included).
fn open_files() -> BTreeSet<(u64, u64)> {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::metadata(e.path()).ok())
        .map(|m| (m.dev(), m.ino()))
        .collect()
}

/// The keys of every page of the attached snapshot under `commit`.
fn all_keys(store: &SqliteStore, commit: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut page = SymbolPage {
        after_id: 0,
        limit: SNAPSHOT_SYMBOL_PAGE,
    };
    loop {
        let (rows, next) = store.snapshot_symbols(commit, page).unwrap();
        out.extend(rows.into_iter().map(|r| r.key));
        match next {
            Some(after_id) => page.after_id = after_id,
            None => return out,
        }
    }
}

/// Contract §11 `attach_is_read_only`: through the dedicated read connection an `INSERT INTO
/// cg.symbols` fails; a `remember` and an `apply_batch` on the same store instance succeed
/// after the attach; the main connection has no `cg` schema (`PRAGMA database_list`).
#[test]
fn attach_is_read_only() {
    let s = Scratch::new("e26-attach", "read-only");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let built = cg_fixture::build(&s.0, Variant::Afero, AFERO_COMMIT);
    store.attach_snapshot(&admitted(&built.path)).unwrap();

    let write = store.cg_read_execute(
        "INSERT INTO cg.symbols(id, kind, ref, label, name, external, is_test, ident_terms) \
         VALUES (99, 'symbol', 'scip:x', 'Method', 'x', 0, 0, 'x')",
    );
    assert!(
        matches!(write, Err(StoreError::Backend { .. })),
        "a write through the read connection must fail: {write:?}"
    );
    let read_back = store
        .snapshot_symbols(
            AFERO_COMMIT,
            SymbolPage {
                after_id: 0,
                limit: SNAPSHOT_SYMBOL_PAGE,
            },
        )
        .unwrap()
        .0;
    assert!(
        read_back.iter().all(|r| r.key != "scip:x"),
        "nothing was written: {read_back:?}"
    );

    // the memory database is still writable on its own connection
    let note = created(&store, "Fact", "written after the attach");
    let other = NodeId::new();
    let report = store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::UpsertNode {
                    id: other.clone(),
                    node: NewNode {
                        kind: "Fact".into(),
                        text: "batched after the attach".into(),
                        ..Default::default()
                    },
                }],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.inserted, 1, "{report:?}");
    assert!(
        store
            .get(&NodeId::parse(&note.id).unwrap(), false)
            .unwrap()
            .is_some()
    );

    let (main, read) = store.attached_schemas().unwrap();
    assert!(
        !main.iter().any(|n| n == "cg"),
        "the main connection never attaches: {main:?}"
    );
    assert!(read.iter().any(|n| n == "cg"), "{read:?}");

    store.detach_snapshot().unwrap();
    let (_, read) = store.attached_schemas().unwrap();
    assert_eq!(read, Vec::<String>::new(), "detached and closed");
    assert!(matches!(
        store.snapshot_symbols(
            AFERO_COMMIT,
            SymbolPage {
                after_id: 0,
                limit: 10
            }
        ),
        Err(StoreError::SnapshotMismatch)
    ));
}

/// An in-memory store has no file to open a second connection on: the attach is refused with
/// the fixed `Backend` class and nothing is attached.
#[test]
fn a_memory_store_refuses_attach() {
    let s = Scratch::new("e26-attach", "memory");
    let built = cg_fixture::build(&s.0, Variant::Afero, AFERO_COMMIT);
    let store = SqliteStore::open_memory().unwrap();
    let r = store.attach_snapshot(&admitted(&built.path));
    assert!(matches!(r, Err(StoreError::Backend { .. })), "{r:?}");
    let (_, read) = store.attached_schemas().unwrap();
    assert_eq!(read, Vec::<String>::new());
}

/// `attach_switches_on_commit_change` (engine-plan E2.6 slice; RU3-2): a second attach with
/// another commit re-attaches and the first file is closed (its `(dev, ino)` leaves
/// `/proc/self/fd`); a same-commit republication (another generation of the same name)
/// re-attaches too, so the read connection never serves a file the provider no longer has.
#[test]
fn attach_switches_on_commit_change() {
    let s = Scratch::new("e26-attach", "switch");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = cg_fixture::build(&s.0, Variant::Afero, AFERO_COMMIT);
    let b = cg_fixture::build(&s.0, Variant::AferoRenamed, OTHER_COMMIT);
    let (a_id, b_id) = (ident(&a.path), ident(&b.path));
    assert!(
        !open_files().contains(&a_id),
        "nothing open before the attach"
    );

    store.attach_snapshot(&admitted(&a.path)).unwrap();
    assert!(open_files().contains(&a_id), "the attach opened the file");
    assert!(all_keys(&store, AFERO_COMMIT).contains(&AFERO_KEY.to_owned()));

    // another commit: re-attached, the first file closed
    store.attach_snapshot(&admitted(&b.path)).unwrap();
    let open = open_files();
    assert!(open.contains(&b_id), "the second file is open");
    assert!(!open.contains(&a_id), "the first file left /proc/self/fd");
    assert!(all_keys(&store, OTHER_COMMIT).contains(&AFERO_RENAMED_KEY.to_owned()));
    assert!(matches!(
        store.snapshot_symbols(
            AFERO_COMMIT,
            SymbolPage {
                after_id: 0,
                limit: 10
            }
        ),
        Err(StoreError::SnapshotMismatch)
    ));

    // a same-commit republication: the same name, another inode and content
    store.attach_snapshot(&admitted(&a.path)).unwrap();
    assert!(open_files().contains(&a_id));
    let a2 = cg_fixture::build(&s.0, Variant::AferoRenamed, AFERO_COMMIT);
    let republished = ident(&a2.path);
    assert_ne!(republished, a_id, "the rebuild is a new inode");
    store.attach_snapshot(&admitted(&a2.path)).unwrap();
    let open = open_files();
    assert!(
        open.contains(&republished),
        "the republished file is the attached one"
    );
    assert!(!open.contains(&a_id), "the stale generation was closed");
    let keys = all_keys(&store, AFERO_COMMIT);
    assert!(keys.contains(&AFERO_RENAMED_KEY.to_owned()), "{keys:?}");
    assert!(!keys.contains(&AFERO_KEY.to_owned()), "{keys:?}");

    // the same generation again: no re-attach needed, still served
    store.attach_snapshot(&admitted(&a2.path)).unwrap();
    assert!(open_files().contains(&republished));
}

/// Spec 05 SS-25 on the production path (admit, then mint, then attach): a handle exists only
/// inside the `SnapshotAttach` an admission's evidence builds, and it binds the generation that
/// admission read. (a) After the file is replaced under its name and re-admitted, the retained
/// handle is stale: presented after the current attach, it is refused (the file no longer has
/// its generation), the replaced generation's rows are not served again and its inode is
/// never reopened. (b) A file
/// admission refuses mints no handle at all, so nothing of it is ever attached.
#[test]
fn a_stale_handle_never_serves_the_replaced_generation_and_a_refused_file_never_attaches() {
    // Arrange: admit `a` and attach the handle its admission minted
    let s = Scratch::new("ss25-attach", "stale-handle");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = cg_fixture::build(&s.0, Variant::Afero, AFERO_COMMIT);
    let a_id = ident(&a.path);
    let stale = admitted(&a.path);
    let rendered = format!("{stale:?}");
    assert!(
        !rendered.contains(a.path.to_str().unwrap()) && !rendered.contains("file:"),
        "the handle is opaque: {rendered}"
    );
    store.attach_snapshot(&stale).unwrap();
    assert!(all_keys(&store, AFERO_COMMIT).contains(&AFERO_KEY.to_owned()));

    // Act (a): the file is replaced under the same name and admitted again; the current
    // handle attaches, then the stale one is presented
    let a2 = cg_fixture::build(&s.0, Variant::AferoRenamed, AFERO_COMMIT);
    let current = admitted(&a2.path);
    assert_ne!(
        stale.generation, current.generation,
        "the admission read the replacement's generation"
    );
    store.attach_snapshot(&current).unwrap();
    assert!(
        !open_files().contains(&a_id),
        "the replaced generation closed"
    );
    let presented = store.attach_snapshot(&stale);

    // Assert (a): the stale handle is refused and serves nothing of the replaced generation:
    // its rows are gone for good, its inode is never attached again, and the current
    // generation's attachment stands untouched
    assert!(
        matches!(presented, Err(StoreError::SnapshotMismatch)),
        "{presented:?}"
    );
    let keys = all_keys(&store, AFERO_COMMIT);
    assert!(!keys.contains(&AFERO_KEY.to_owned()), "{keys:?}");
    assert!(keys.contains(&AFERO_RENAMED_KEY.to_owned()), "{keys:?}");
    assert!(
        !open_files().contains(&a_id),
        "the replaced inode is never attached again"
    );
    assert!(open_files().contains(&ident(&a2.path)));

    // Act (b): a file admission refuses (its schema major is above the reader's)
    let too_new = cg_fixture::build(&s.0, Variant::AferoTooNew, OTHER_COMMIT);
    let refused = validate_for_attach(&too_new.path);

    // Assert (b): no handle, so nothing of it is attached and its commit is not served
    assert!(
        matches!(refused, Err(SnapshotError::SnapshotTooNew { .. })),
        "{refused:?}"
    );
    assert!(!open_files().contains(&ident(&too_new.path)));
    assert!(matches!(
        store.snapshot_symbols(
            OTHER_COMMIT,
            SymbolPage {
                after_id: 0,
                limit: 10
            }
        ),
        Err(StoreError::SnapshotMismatch)
    ));
    let (_, read) = store.attached_schemas().unwrap();
    assert_eq!(read.iter().filter(|n| *n == "cg").count(), 1, "{read:?}");
}

/// Spec 05 SS-25 at attach time (review c-2b): a handle binds the generation its admission
/// read, and the store checks the file under the admitted path against it before the
/// `ATTACH`. A stale handle presented after the file was replaced under its name is refused
/// with the fixed class; nothing is attached (the read side carries no `cg`, the replacement's
/// inode is never opened through it) and the replacement's rows are never served under the
/// stale label. The replacement's own admission still attaches.
#[test]
fn a_stale_handle_is_refused_before_the_attach_and_the_fresh_admission_attaches() {
    // Arrange: admit `a`, keep its handle, replace the file under the same name
    let s = Scratch::new("ss25-attach", "stale-refused");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = cg_fixture::build(&s.0, Variant::Afero, AFERO_COMMIT);
    let stale = admitted(&a.path);
    let a2 = cg_fixture::build(&s.0, Variant::AferoRenamed, AFERO_COMMIT);
    let replaced = ident(&a2.path);

    // Act: the stale handle
    let refused = store.attach_snapshot(&stale);

    // Assert: refused, nothing attached, the replacement neither opened nor served
    assert!(
        matches!(refused, Err(StoreError::SnapshotMismatch)),
        "{refused:?}"
    );
    let (_, read) = store.attached_schemas().unwrap();
    assert!(!read.iter().any(|n| n == "cg"), "{read:?}");
    assert!(!open_files().contains(&replaced));
    assert!(matches!(
        store.snapshot_symbols(
            AFERO_COMMIT,
            SymbolPage {
                after_id: 0,
                limit: 10
            }
        ),
        Err(StoreError::SnapshotMismatch)
    ));

    // Act and assert: the replacement's own admission attaches and serves its rows
    store.attach_snapshot(&admitted(&a2.path)).unwrap();
    let keys = all_keys(&store, AFERO_COMMIT);
    assert!(keys.contains(&AFERO_RENAMED_KEY.to_owned()), "{keys:?}");
    assert!(!keys.contains(&AFERO_KEY.to_owned()), "{keys:?}");
    assert!(open_files().contains(&replaced));
}

/// A `CodeRef` with `symbol = key` in `workspace` and `repo`, scoped to `scope`; its id.
fn coderef(
    store: &SqliteStore,
    workspace: &str,
    repo: &str,
    key: &str,
    scope: Option<&str>,
) -> String {
    let mut props = json!({
        "repo": repo, "commit": AFERO_COMMIT, "tool": "fixture", "relative_path": "memmap.go",
        "name_path": "MemMapFs/Create", "kind": "Method", "line": 7, "symbol": key,
    });
    if let Some(scope) = scope {
        props["snapshot_commit"] = json!(scope);
    }
    let ctx = WriteCtx::new("pv", WorkspaceId::new(workspace), Actor::Agent)
        .with_governed(GovernedSet::shipped());
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

/// CG-JOIN v1 §3 and §5 at the store seam: `coderefs_by_symbol` applies every predicate of
/// the set (workspace, `repo`, the scope half) and compares the key byte for byte.
#[test]
fn coderefs_by_symbol_applies_every_predicate() {
    let s = Scratch::new("e26-attach", "predicates");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = WorkspaceId::default();
    let mine = coderef(&store, ws.as_str(), "kaban", AFERO_KEY, None);
    let scoped = coderef(&store, ws.as_str(), "kaban", AFERO_KEY, Some(AFERO_COMMIT));
    let other_scope = coderef(&store, ws.as_str(), "kaban", AFERO_KEY, Some(OTHER_COMMIT));
    let other_repo = coderef(&store, ws.as_str(), "other", AFERO_KEY, None);
    let other_ws = coderef(&store, "ws2", "kaban", AFERO_KEY, None);
    let off_by_case = coderef(
        &store,
        ws.as_str(),
        "kaban",
        &AFERO_KEY.replace("MemMapFs#", "memmapfs#"),
        None,
    );
    let ids = |p: &AnchorPredicates<'_>, key: &str| -> Vec<String> {
        store
            .coderefs_by_symbol(p, key)
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect()
    };
    let only = WorkspaceFilter::Only(ws.as_str().to_owned());
    // resolved commit: the NULL rows and the rows scoped to it
    let with_commit = AnchorPredicates {
        workspace: &only,
        repo: "kaban",
        commit: Some(AFERO_COMMIT),
    };
    let mut got = ids(&with_commit, AFERO_KEY);
    got.sort();
    let mut want = vec![mine.clone(), scoped.clone()];
    want.sort();
    assert_eq!(
        got, want,
        "other scope, other repo, other workspace, other bytes are out"
    );
    // no snapshot: every scope (OWN-83 (1), cg-join §3; the card signals `anchor_commit`)
    let without = AnchorPredicates {
        workspace: &only,
        repo: "kaban",
        commit: None,
    };
    let mut got = ids(&without, AFERO_KEY);
    got.sort();
    let mut want = vec![mine.clone(), scoped.clone(), other_scope.clone()];
    want.sort();
    assert_eq!(got, want);
    // every workspace: the ws2 row joins too, the other repository still does not
    let all = AnchorPredicates {
        workspace: &WorkspaceFilter::All,
        repo: "kaban",
        commit: None,
    };
    let mut got = ids(&all, AFERO_KEY);
    got.sort();
    let mut want = vec![
        mine.clone(),
        scoped.clone(),
        other_scope.clone(),
        other_ws.clone(),
    ];
    want.sort();
    assert_eq!(got, want);
    // the other repository's identity selects its own row only
    assert_eq!(
        ids(
            &AnchorPredicates {
                workspace: &only,
                repo: "other",
                commit: None
            },
            AFERO_KEY
        ),
        vec![other_repo.clone()]
    );
    // byte equality: the case-different key finds its own row and nothing else
    let lower = AFERO_KEY.replace("MemMapFs#", "memmapfs#");
    assert_eq!(ids(&without, &lower), vec![off_by_case.clone()]);
    let _ = other_scope;
}

/// Contract §11 `snapshot_symbols_pages_non_external` (SO-10): over 10,000 symbols of which
/// 1,000 are `external = 1` and 50 are `kind = 'local'`, paging with the structural limit
/// returns 8,950 rows in three pages ordered by id, each with `path` joined and `ident_terms`
/// filled; a `commit` that differs from the attached one is `SnapshotMismatch` with no rows.
#[test]
fn snapshot_symbols_pages_non_external() {
    let s = Scratch::new("e26-attach", "pages");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let built = cg_fixture::build(
        &s.0,
        Variant::Many {
            total: 10_000,
            external: 1_000,
            local: 50,
        },
        AFERO_COMMIT,
    );
    store.attach_snapshot(&admitted(&built.path)).unwrap();

    let mut pages = Vec::new();
    let mut page = SymbolPage {
        after_id: 0,
        limit: SNAPSHOT_SYMBOL_PAGE,
    };
    loop {
        let (rows, next) = store.snapshot_symbols(AFERO_COMMIT, page).unwrap();
        pages.push(rows);
        match next {
            Some(after_id) => page.after_id = after_id,
            None => break,
        }
    }
    let sizes: Vec<usize> = pages.iter().map(Vec::len).collect();
    assert_eq!(sizes, vec![4096, 4096, 758], "three pages of 8,950 rows");
    let rows: Vec<_> = pages.into_iter().flatten().collect();
    assert!(rows.iter().all(|r| r.path == MANY_PATH), "path joined");
    assert!(
        rows.iter().all(|r| !r.ident_terms.is_empty()),
        "ident_terms filled"
    );
    assert!(rows.iter().all(|r| r.label == "Method" && r.name == "M"));
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        keys, sorted,
        "ordered by id, which is key order here, no duplicate"
    );
    assert!(
        rows.iter()
            .all(|r| r.key.starts_with("scip-go gomod example.com/many . p/S")),
        "no local row (keyless) and no external row"
    );
    // the externals are the last 1,000 scip rows: the highest surviving index is 8,949
    assert!(keys.last().unwrap().contains("S08949#"));

    let other = store.snapshot_symbols(
        OTHER_COMMIT,
        SymbolPage {
            after_id: 0,
            limit: SNAPSHOT_SYMBOL_PAGE,
        },
    );
    assert!(
        matches!(other, Err(StoreError::SnapshotMismatch)),
        "{other:?}"
    );
}

// ---- E2.6b: the eight-process concurrent attach (engine-plan E2.6 slice "8 processes
// attach concurrently") ----

/// Set in a reader process: its index. The test binary re-executes itself for the readers
/// (the pattern of `kaban-ingest/tests/panic_hook.rs`), so no second binary is needed.
const READER_INDEX: &str = "KABAN_E26B_ATTACH_READER";
/// Set in a reader process: the parent's scratch directory (db, keys and the snapshot).
const READER_DIR: &str = "KABAN_E26B_ATTACH_DIR";
const READERS: usize = 8;
/// Attach rounds per reader; every other round detaches first, so a fresh attach, a no-op
/// re-presentation of the attached generation and a detach all race across the processes.
const ROUNDS: usize = 24;
const MP_TEST: &str = "multi_process_attach_eight_readers_share_one_store_and_snapshot";

/// One reader: open the parent's store file, then `ROUNDS` times attach the snapshot on the
/// dedicated read connection, page its symbols, join the `CodeRef` by key over `node_anchor`
/// (the main connection), and detach now and then. Nothing is written; the main connection
/// never carries `cg`.
fn attach_reader(index: usize) {
    let dir = PathBuf::from(std::env::var_os(READER_DIR).expect("the parent sets the dir"));
    let store = SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap();
    let path = dir
        .join("codegraph")
        .join(REPO_ID)
        .join(format!("{AFERO_COMMIT}.cg.db"));
    let only = WorkspaceFilter::Only(WorkspaceId::default().as_str().to_owned());
    for round in 0..ROUNDS {
        store
            .attach_snapshot(&admitted(&path))
            .unwrap_or_else(|e| panic!("reader {index} round {round}: attach: {e:?}"));
        let keys = all_keys(&store, AFERO_COMMIT);
        assert!(
            keys.contains(&AFERO_KEY.to_owned()),
            "reader {index} round {round}: {keys:?}"
        );
        let rows = store
            .coderefs_by_symbol(
                &AnchorPredicates {
                    workspace: &only,
                    repo: "kaban",
                    commit: Some(AFERO_COMMIT),
                },
                AFERO_KEY,
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "reader {index} round {round}: {rows:?}");
        if round % 2 == 1 {
            store.detach_snapshot().unwrap();
        }
    }
    let (main, _) = store.attached_schemas().unwrap();
    assert!(!main.iter().any(|n| n == "cg"), "reader {index}: {main:?}");
}

/// Engine-plan E2.6 "8 processes attach concurrently" (SO-4): eight processes open one store
/// file and attach the same snapshot on their own read connections while the parent keeps
/// writing to the main database. Every reader must exit clean (its attach, page, join and
/// detach rounds all passed), the snapshot file's bytes are unchanged (read-only attach), the
/// store is intact afterwards (the pointer reads back, a `remember` lands) and the parent's main
/// connection has no `cg` schema.
#[test]
#[ignore = "expensive 8-process run; cargo nextest run --run-ignored only -E 'test(multi_process)'"]
fn multi_process_attach_eight_readers_share_one_store_and_snapshot() {
    if let Some(index) = std::env::var_os(READER_INDEX) {
        attach_reader(index.to_str().unwrap().parse().unwrap());
        return;
    }
    let s = Scratch::new("e26b-attach", "multi-process");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let built = cg_fixture::build(&s.0, Variant::Afero, AFERO_COMMIT);
    let pointer = coderef(
        &store,
        WorkspaceId::default().as_str(),
        "kaban",
        AFERO_KEY,
        None,
    );
    let bytes_before = std::fs::read(&built.path).unwrap();
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let mut readers: Vec<_> = (0..READERS)
        .map(|i| {
            Command::new(&exe)
                .args(["--exact", MP_TEST, "--ignored", "--nocapture"])
                .env(READER_INDEX, i.to_string())
                .env(READER_DIR, &s.0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    // the parent writes beside the readers: the main database stays writable under attach
    for i in 0..ROUNDS {
        created(&store, "Fact", &format!("written beside the readers {i}"));
    }
    let deadline = started + Duration::from_secs(180);
    loop {
        let mut running = 0;
        for c in &mut readers {
            if c.try_wait().unwrap().is_none() {
                running += 1;
            }
        }
        if running == 0 {
            break;
        }
        if Instant::now() > deadline {
            for c in &mut readers {
                let _ = c.kill();
            }
            panic!("{running} readers still running after 180s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for (i, reader) in readers.into_iter().enumerate() {
        let out = reader.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "reader {i} failed ({}):\n{stdout}\n{stderr}",
            out.status
        );
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "reader {i} did not run the reader branch:\n{stdout}\n{stderr}"
        );
    }
    assert_eq!(
        std::fs::read(&built.path).unwrap(),
        bytes_before,
        "the snapshot file is untouched by eight read-only attaches"
    );
    assert!(
        store
            .get(&NodeId::parse(&pointer).unwrap(), false)
            .unwrap()
            .is_some(),
        "the store is intact"
    );
    created(&store, "Fact", "written after the readers left");
    let (main, _) = store.attached_schemas().unwrap();
    assert!(
        !main.iter().any(|n| n == "cg"),
        "the parent's main connection never attaches: {main:?}"
    );
    let wall = started.elapsed();
    assert!(wall < Duration::from_secs(180), "{wall:?}");
}
