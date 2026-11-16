//! The one commit entry (spec 11 T5, KER-07, REC-06, INV-3): a write through any writer, here
//! `SqliteStore::link` (not `apply_batch`), stages every index class inside its own transaction,
//! so a pin another process takes after the write returns sees the staged sediment at once.
//!
//! Runs with the `failpoints` feature (the test class lives behind it).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::MemoryStore;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing::{self, ObservedOp};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

/// The database the pinning role opens (set only for the child process).
const PIN_DB: &str = "KABAN_T5_PIN_DB";
/// The file the pinning role writes the sediment it saw to, one payload per line.
const PINNED: &str = "pinned";

/// The pinning role of the two-process case: in a child process (`KABAN_T5_PIN_DB` set) open
/// the store, pin its catalog at once and write the test class's sediment payloads to
/// `<dir>/pinned`. A no-op elsewhere.
#[test]
#[ignore = "a role of the two-process visibility case"]
fn multi_process_pin_role() {
    let Ok(db) = std::env::var(PIN_DB) else {
        return;
    };
    let db = PathBuf::from(db);
    let dir = db.parent().unwrap().to_path_buf();
    let store = SqliteStore::open(&db, &dir.join("keys")).unwrap();
    let rows = catalog_testing::pin_sediment(&store).unwrap();
    std::fs::write(dir.join(PINNED), rows.join("\n")).unwrap();
}

fn rid(conn: &Connection, id: &str) -> i64 {
    conn.query_row("SELECT rid FROM node WHERE id = ?1", [id], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_link_is_in_the_sediment_of_a_pin_another_process_takes_after_it_returns() {
    // Arrange: process A's store with the test class installed, two nodes
    let s = Scratch::new("kaban-t5-commit", "link-visibility");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let _observed = catalog_testing::install_edge_sediment_class(&store);
    let from = created(&store, "Fact", "the harbour opens at dawn");
    let to = created(&store, "Fact", "the ferry leaves the harbour at noon");

    // Act: A links (a non-`apply_batch` writer); after it returns, process B pins
    store
        .link(
            &nid(&from.id),
            &nid(&to.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "multi_process_pin_role", "--ignored"])
        .env(PIN_DB, s.db())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the pinning role failed: {status:?}");

    // Assert: B's pin holds exactly the one row A's link staged
    let raw = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let expected = catalog_testing::edge_payload(
        ObservedOp::Insert,
        rid(&raw, &from.id),
        rid(&raw, &to.id),
        "RELATES_TO",
    );
    let pinned = std::fs::read_to_string(s.0.join(PINNED)).unwrap();
    assert_eq!(
        pinned.lines().collect::<Vec<_>>(),
        vec![expected.as_str()],
        "process B's pin must hold the sediment row of A's link"
    );
}

/// Every entry of `dir` by name, with a regular file's bytes (`None` for anything else).
fn listing(dir: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let bytes = entry
                .file_type()
                .unwrap()
                .is_file()
                .then(|| std::fs::read(entry.path()).unwrap());
            (entry.file_name().into_string().unwrap(), bytes)
        })
        .collect()
}

/// The catalog's artefacts beside a database.
const CATALOG_ARTEFACTS: [&str; 3] = ["kaban.kbx", "kaban.kbx.alloc", "kaban.kbx.pins"];

#[test]
fn a_fresh_project_opens_with_its_catalog_beside_the_database() {
    // Arrange
    let s = Scratch::new("kaban-t5-commit", "fresh-catalog");

    // Act
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    // Assert: the container and the allocation lock are files, the pins a directory
    let entries = listing(&s.0);
    for name in CATALOG_ARTEFACTS {
        assert!(
            entries.contains_key(name),
            "{name} is missing: {:?}",
            entries.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        s.0.join("kaban.kbx.pins").is_dir(),
        "the pins are a directory"
    );
    drop(store);
}

#[test]
fn a_refused_older_format_project_gains_no_catalog_artefact() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    for fixture in ["v3.0.0", "v3.0.0-wal"] {
        // Arrange: a copy of the 3.0.0 project (never opened in place)
        let s = Scratch::new("kaban-t5-commit", &format!("refused-{fixture}"));
        for entry in std::fs::read_dir(fixtures.join(fixture)).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), s.0.join(entry.file_name())).unwrap();
        }
        let before = listing(&s.0);

        // Act: every open refuses it
        let actor = kaban_core::ActorId::default();
        let refusals = [
            SqliteStore::open(&s.db(), &s.keys()).map(drop),
            SqliteStore::open_existing_key(&s.db(), &s.keys(), &actor).map(drop),
            SqliteStore::open_diagnostic(&s.db(), &s.keys(), &actor).map(drop),
            SqliteStore::open_for_hook(&s.db(), &s.keys(), &actor).map(drop),
        ];

        // Assert: refused, and the directory is as it was: no catalog artefact, no byte changed
        for refused in refusals {
            let err = refused.unwrap_err();
            assert!(
                matches!(
                    err,
                    kaban_core::StoreError::SchemaUnsupported { found: 13, .. }
                ),
                "{fixture}: {err:?}"
            );
        }
        let after = listing(&s.0);
        for name in CATALOG_ARTEFACTS {
            assert!(!after.contains_key(name), "{fixture}: {name} was created");
        }
        assert_eq!(after, before, "{fixture}: the project was changed");
    }
}

/// The transaction openers the structural scan looks for (the task's inventory grep).
const OPENERS: [&str; 5] = [
    "TransactionBehavior::Immediate",
    "TransactionBehavior::Exclusive",
    "unchecked_transaction",
    ".transaction()",
    "\"BEGIN",
];

/// The sites that may open a transaction outside the catalog, by file (relative to `src/`)
/// and enclosing function, each with its reason. Every entry must still match a site.
const OPENER_ALLOWLIST: [(&str, &str, &str); 5] = [
    ("catalog/commit.rs", "commit_in", "the one writer entry"),
    ("catalog/commit.rs", "read", "the one read entry"),
    (
        "schema.rs",
        "ensure_current",
        "open-time schema creation, before the catalog exists",
    ),
    (
        "sqlite.rs",
        "init_schema",
        "open-time seed and identity rows, before admission and version publication: the \
         catalog opens only after publication (CMP-01) and the seed writes no node or edge row",
    ),
    (
        "projector_thread.rs",
        "immediate",
        "the projector's cursor on its own connection; removed by T7",
    ),
];

/// The test-only source files the scan skips: each must be declared `#[cfg(test)]` by its
/// parent module (`(file, parent, declaration)`), so production code is never skipped.
const TEST_ONLY_FILES: [(&str, &str, &str); 1] = [("sqlite/tests.rs", "sqlite.rs", "mod tests;")];

/// The `.rs` files under `dir`, as paths relative to `root` with `/` separators.
fn rust_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let rel = path.strip_prefix(root).unwrap();
            let parts: Vec<_> = rel.iter().map(|p| p.to_str().unwrap()).collect();
            out.push(parts.join("/"));
        }
    }
}

/// The name of the function a line declares, if it declares one (`fn name`, any visibility).
fn declared_fn(line: &str) -> Option<String> {
    let code = line.trim_start();
    if code.starts_with("//") {
        return None;
    }
    let at = code.find("fn ")?;
    // `fn` as a keyword: at the start or after a space (`pub fn`, `pub(crate) fn`)
    if at > 0 && !code[..at].ends_with(' ') {
        return None;
    }
    let name: String = code[at + 3..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Every transaction opener in the store's source as `(file, enclosing fn, line number)`.
fn opener_sites(src: &std::path::Path) -> Vec<(String, String, usize)> {
    let mut files = Vec::new();
    rust_files(src, src, &mut files);
    files.sort();
    let mut sites = Vec::new();
    for file in files {
        if TEST_ONLY_FILES.iter().any(|(skip, ..)| *skip == file) {
            continue;
        }
        let text = std::fs::read_to_string(src.join(&file)).unwrap();
        let mut current = String::new();
        for (n, line) in text.lines().enumerate() {
            if let Some(name) = declared_fn(line) {
                current = name;
            }
            if OPENERS.iter().any(|o| line.contains(o)) {
                sites.push((file.clone(), current.clone(), n + 1));
            }
        }
    }
    sites
}

#[test]
fn every_writer_transaction_of_the_store_opens_in_the_catalog() {
    // Arrange: the store's production source; each skipped test file is test-only
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (file, parent, declaration) in TEST_ONLY_FILES {
        let text = std::fs::read_to_string(src.join(parent)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.trim() == declaration)
            .unwrap_or_else(|| panic!("{parent} declares no `{declaration}` for {file}"));
        assert!(
            lines[at.saturating_sub(3)..at]
                .iter()
                .any(|l| l.trim() == "#[cfg(test)]"),
            "{file} is skipped as test-only, but {parent} does not declare it #[cfg(test)]"
        );
    }

    // Act
    let sites = opener_sites(&src);

    // Assert: no opener outside the allowlist, and no stale allowlist entry
    let unrouted: Vec<String> = sites
        .iter()
        .filter(|(file, function, _)| {
            !OPENER_ALLOWLIST
                .iter()
                .any(|(f, func, _)| f == file && func == function)
        })
        .map(|(file, function, line)| format!("{file}:{line} ({function})"))
        .collect();
    assert!(
        unrouted.is_empty(),
        "transactions opened outside Catalog::commit / Catalog::read:\n{}",
        unrouted.join("\n")
    );
    for (file, function, reason) in OPENER_ALLOWLIST {
        assert!(
            sites
                .iter()
                .any(|(f, func, _)| f == file && func == function),
            "stale allowlist entry {file} ({function}): {reason}"
        );
    }
}

/// One store write the autocommit case drives.
type Writer<'a> = Box<dyn Fn() -> kaban_core::Result<()> + 'a>;

#[test]
fn every_autocommit_writer_commits_once_through_the_catalog() {
    use kaban_core::{
        IngestStore, JobId, Producer, SchemaStore, SessionEdit, SessionEditKind, SessionStore,
        TypeDef, WorkspaceId,
    };
    use kaban_store_sqlite::ProducerRunOutcome;

    // Arrange: a store with the test class, which records one change set per commit
    let s = Scratch::new("kaban-t5-commit", "autocommit-writers");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let observed = catalog_testing::install_edge_sediment_class(&store);
    let workspace = WorkspaceId::new("default");
    let label = TypeDef {
        kind: "label".into(),
        name: "Widget".into(),
        ns: None,
        description: "a widget".into(),
        from_kinds: vec![],
        to_kinds: vec![],
        directed: true,
        props_schema: None,
        builtin: false,
        declaration: kaban_core::TypeDeclaration::default(),
    };
    let edit = SessionEdit {
        session_id: "s1".into(),
        kind: SessionEditKind::Edit,
        path: "p".into(),
        ts: 10,
    };
    let writers: Vec<(&str, Writer<'_>)> = vec![
        (
            "set_index_meta",
            Box::new(|| store.set_index_meta("t5c", "1")),
        ),
        (
            "record_producer_run",
            Box::new(|| store.record_producer_run(Producer::Gazetteer, 1, ProducerRunOutcome::Ok)),
        ),
        (
            "schema_extend",
            Box::new(|| store.schema_extend(label.clone()).map(drop)),
        ),
        (
            "session_edit_record",
            Box::new(|| store.session_edit_record(&edit)),
        ),
        (
            "session_edits_prune",
            Box::new(|| store.session_edits_prune(20, 10).map(drop)),
        ),
        (
            "jobs_prune",
            Box::new(|| store.jobs_prune(&workspace, 0, 10).map(drop)),
        ),
        (
            "job_mark_stale",
            Box::new(|| {
                store
                    .job_mark_stale(&JobId::from_digest(&[7; 32]), &workspace)
                    .map(drop)
            }),
        ),
    ];

    for (name, write) in writers {
        // Act
        let before = observed.change_sets().len();
        write().unwrap_or_else(|e| panic!("{name}: {e:?}"));

        // Assert: one commit through the catalog, which staged the class once
        assert_eq!(
            observed.change_sets().len(),
            before + 1,
            "{name} did not commit through Catalog::commit"
        );
    }
}

/// SQLite's default automatic checkpoint threshold, in WAL frames (pages).
const DEFAULT_AUTOCHECKPOINT_FRAMES: u64 = 1000;

#[test]
fn the_writer_never_checkpoints_on_its_own_and_its_close_folds_the_wal() {
    // Arrange: a store and the WAL size an automatic checkpoint would never let it reach (a
    // restarted WAL is rewritten from its start, so the file stays near the threshold)
    let s = Scratch::new("kaban-t5-commit", "no-autocheckpoint");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let page: u64 = {
        let raw = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let page: i64 = raw.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
        u64::try_from(page).unwrap()
    };
    let past = 3 * DEFAULT_AUTOCHECKPOINT_FRAMES * (page + 24);
    let wal = s.0.join("kaban.db-wal");
    let wal_len = || std::fs::metadata(&wal).map_or(0, |m| m.len());

    // Act: write until the WAL is past that size, at most a bounded number of writes
    let mut writes = 0;
    while wal_len() <= past && writes < 6000 {
        created(
            &store,
            "Fact",
            &format!("note {writes} on the harbour tide tables"),
        );
        writes += 1;
    }

    // Assert: no automatic checkpoint restarted the WAL (today a writer's WAL is bounded only
    // by an explicit checkpoint or the last close) ...
    assert!(
        wal_len() > past,
        "after {writes} writes the WAL is {} bytes, not past {past}: a checkpoint restarted it",
        wal_len()
    );
    // ... and the last connection's close folds it into the main file
    drop(store);
    assert_eq!(wal_len(), 0, "the close left the WAL behind");
}
