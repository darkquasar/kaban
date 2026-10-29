//! The exclusive maintenance condition (spec 05 SS-12, lane T4) through the store's own
//! `forget(Purge)` with the adapter attached: while a second process holds the database and
//! the index open, the erasure claims nothing about its mappings (the index scope stays
//! `outstanding or unknown`, never cleaned); once no other process holds them, the condition
//! is established without IPC (the database's owner lock plus the index directory's lock),
//! the affected segment files are gone and the index scope reports cleaned. The T2.4 canaries
//! (`purge.rs`) keep their qualified claim: nothing establishes the condition there.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "tantivy_common/mod.rs"]
mod common;

use common::{Scratch, encoded, note};
use kaban_core::{ForgetMode, MemoryStore, NodeId, RecallQuery};
use kaban_index_tantivy::TantivyIndex;
use kaban_index_tantivy::admission::DatabaseHold;
use kaban_index_tantivy::projector::Projector;
use kaban_store_sqlite::SqliteStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The database side of the condition as the binary wires it: the store crate's owner lock.
struct Owner(PathBuf);

impl DatabaseHold for Owner {
    fn try_exclusive(&self) -> kaban_core::Result<Option<Box<dyn std::any::Any + Send>>> {
        Ok(kaban_store_sqlite::exclusive_hold(&self.0)?
            .map(|hold| Box::new(hold) as Box<dyn std::any::Any + Send>))
    }
}

/// The segment files of `dir` whose `rid` dictionary names `rid` (the files a purge must
/// remove): every file of each affected segment, by the segment's uuid prefix.
fn segment_files_naming(dir: &Path, rid: i64) -> Vec<PathBuf> {
    use tantivy::schema::Term;
    let index = tantivy::Index::open_in_dir(dir).unwrap();
    let schema = index.schema();
    let field = schema.get_field("rid").unwrap();
    let searcher = index.reader().unwrap().searcher();
    let mut prefixes = Vec::new();
    for segment in searcher.segment_readers() {
        let inverted = segment.inverted_index(field).unwrap();
        if inverted
            .doc_freq(&Term::from_field_i64(field, rid))
            .unwrap()
            > 0
        {
            prefixes.push(segment.segment_id().uuid_string());
        }
    }
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            prefixes.iter().any(|prefix| name.starts_with(prefix))
        })
        .collect()
}

fn query(text: &str) -> RecallQuery {
    RecallQuery {
        query: text.into(),
        hops: 0,
        ..Default::default()
    }
}

/// The index scope of a purge's cleanup report.
fn index_scope(cleanup: &str) -> &str {
    let start = cleanup.find("index: ").expect(cleanup) + "index: ".len();
    let end = cleanup[start..]
        .find("; temporary outputs")
        .map_or(cleanup.len(), |i| start + i);
    &cleanup[start..end]
}

/// The reader role of the two-process cases: in a child process (`KABAN_T4_READER_DB` set)
/// hold the database (a store, unless `KABAN_T4_READER_STORE=0`) and the index (an adapter
/// with a pinned searcher) open, announce readiness with `<dir>/reader-ready`, and exit when
/// `<dir>/reader-release` appears or after 60 s. A no-op elsewhere.
#[test]
#[ignore = "a role of the two-process cases"]
fn multi_process_reader_role() {
    use kaban_core::text_index::TextIndex;
    let Ok(db) = std::env::var("KABAN_T4_READER_DB") else {
        return;
    };
    let db = PathBuf::from(db);
    let dir = db.parent().unwrap().to_path_buf();
    let store = (std::env::var("KABAN_T4_READER_STORE").as_deref() != Ok("0"))
        .then(|| SqliteStore::open(&db, &dir.join("keys")).unwrap());
    let index = TantivyIndex::open(
        &dir.join("adapter").join("tantivy"),
        &kaban_core::shipped::resolved().unwrap(),
        &kaban_core::config::Recall::default(),
        Box::new(kaban_store_sqlite::index_rows::IndexRows::open(&db).unwrap()),
    )
    .unwrap();
    let pinned = index.pin().unwrap();
    std::fs::write(dir.join("reader-ready"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !dir.join("reader-release").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(pinned);
    drop(store);
}

/// A reader in another process, started on `s` and released on drop.
struct Reader(std::process::Child, PathBuf);

impl Reader {
    fn start(s: &Scratch) -> Self {
        Self::start_with(s, true)
    }
    /// The role with (`store`) or without a store of its own on the database.
    fn start_with(s: &Scratch, store: bool) -> Self {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "multi_process_reader_role", "--ignored"])
            .env("KABAN_T4_READER_DB", s.db())
            .env("KABAN_T4_READER_STORE", if store { "1" } else { "0" })
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready = s.0.join("reader-ready");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the reader never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self(child, s.0.join("reader-release"))
    }
    fn release(mut self) {
        std::fs::write(&self.1, b"").unwrap();
        let status = self.0.wait().unwrap();
        assert!(status.success(), "{status:?}");
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.1, b"");
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A canary body long enough for the body codec to compress it.
fn canary_body(token: &str) -> String {
    format!(
        "the {token} note: a token no other record carries, kept in the shore log beside the \
         decision record so the whole team can read it"
    )
}

#[test]
fn erase_claims_cleaned_only_under_the_exclusive_maintenance_condition() {
    let s = Scratch::new("admission");
    let store = s.store();
    let index = Arc::new(s.adapter().with_database_hold(Box::new(Owner(s.db()))));
    let store = store.with_text_index(Box::new(Arc::clone(&index)));
    let dir = s.index_dir();
    let first = note(
        &store,
        "default",
        "Fact",
        &canary_body("zqxcanary7731"),
        &[],
    );
    let second = note(
        &store,
        "default",
        "Fact",
        &canary_body("zqxcanary8842"),
        &[],
    );
    for canary in [&first, &second] {
        assert!(encoded(&s.db(), &canary.id), "the body's shape");
    }
    note(&store, "default", "Fact", "a neighbour that stays", &[]);
    let projector = Projector::new(index.as_ref(), &store, &store, "test:admission", 2);
    assert_eq!(projector.catch_up().unwrap().halt, None);
    let (first_rid, _) = common::row(&s.db(), &first);
    let (second_rid, _) = common::row(&s.db(), &second);
    assert_ne!(segment_files_naming(&dir, first_rid), Vec::<PathBuf>::new());

    // 1. another process holds the database and the index: the condition cannot be
    // established, so the erasure reports the index scope as outstanding or unknown
    let reader = Reader::start(&s);
    let held = store
        .forget(
            &NodeId::parse(&first.id).unwrap(),
            ForgetMode::Purge,
            &common::ctx("default"),
        )
        .unwrap();
    assert!(held.revoked);
    assert!(!held.physically_cleaned);
    let cleanup = held.cleanup.clone().unwrap_or_default();
    let scope = index_scope(&cleanup);
    assert!(
        scope.contains("other readers outstanding or unknown"),
        "{cleanup}"
    );
    assert!(!scope.starts_with("cleaned"), "{cleanup}");
    assert!(store.recall(&query("zqxcanary7731")).unwrap().is_empty());
    reader.release();

    // 2. no other process holds them: the condition is established, the affected segment
    // files are gone from the directory and the index scope says so without qualification
    let affected = segment_files_naming(&dir, second_rid);
    assert!(!affected.is_empty(), "the second canary is indexed");
    let alone = store
        .forget(
            &NodeId::parse(&second.id).unwrap(),
            ForgetMode::Purge,
            &common::ctx("default"),
        )
        .unwrap();
    assert!(alone.revoked);
    let cleanup = alone.cleanup.clone().unwrap_or_default();
    let scope = index_scope(&cleanup);
    assert!(
        scope.starts_with("cleaned") || scope.contains("exclusive"),
        "the index scope is cleaned under exclusivity: {cleanup}"
    );
    assert!(
        !scope.contains("outstanding or unknown"),
        "no qualification under exclusivity: {cleanup}"
    );
    for file in &affected {
        assert!(!file.exists(), "{} is gone", file.display());
    }
    assert_eq!(
        segment_files_naming(&dir, second_rid),
        Vec::<PathBuf>::new()
    );
    assert!(store.recall(&query("zqxcanary8842")).unwrap().is_empty());
    assert_eq!(store.recall(&query("neighbour stays")).unwrap().len(), 1);
    // the condition is released afterwards: another process can open the database again
    let reader = Reader::start(&s);
    reader.release();
}

/// T4 review t4-2: the directory lock covers readers. A second adapter opened independently
/// in this process (the database mark is process-wide and cannot tell it apart) and a reader
/// process with no store of its own both map the index; while either lives the erasure
/// cannot claim the index artefacts cleaned, and once both are gone the condition is
/// established.
#[test]
fn an_independent_reader_blocks_exclusivity_until_it_drops() {
    use kaban_core::text_index::TextIndex;
    let s = Scratch::new("admission-readers");
    let store = s.store();
    let index = Arc::new(s.adapter().with_database_hold(Box::new(Owner(s.db()))));
    let store = store.with_text_index(Box::new(Arc::clone(&index)));
    let canaries: Vec<_> = ["zqxcanary1111", "zqxcanary2222", "zqxcanary3333"]
        .iter()
        .map(|word| note(&store, "default", "Fact", &format!("the {word} note"), &[]))
        .collect();
    note(&store, "default", "Fact", "a neighbour that stays", &[]);
    let projector = Projector::new(index.as_ref(), &store, &store, "test:readers", 2);
    assert_eq!(projector.catch_up().unwrap().halt, None);
    let purge = |node: &kaban_core::Node| {
        let outcome = store
            .forget(
                &NodeId::parse(&node.id).unwrap(),
                ForgetMode::Purge,
                &common::ctx("default"),
            )
            .unwrap();
        assert!(outcome.revoked);
        outcome.cleanup.unwrap_or_default()
    };

    // 1. a second adapter of this process, pinned on the current snapshot
    let other = s.adapter();
    let pinned = other.pin().unwrap();
    let cleanup = purge(&canaries[0]);
    assert!(
        index_scope(&cleanup).contains("other readers outstanding or unknown"),
        "a same-process reader is outstanding: {cleanup}"
    );
    assert!(store.recall(&query("zqxcanary1111")).unwrap().is_empty());
    drop(pinned);
    drop(other);

    // 2. a reader process without a store on the database
    let reader = Reader::start_with(&s, false);
    let cleanup = purge(&canaries[1]);
    assert!(
        index_scope(&cleanup).contains("other readers outstanding or unknown"),
        "a storeless reader process is outstanding: {cleanup}"
    );
    reader.release();

    // 3. alone: established
    let cleanup = purge(&canaries[2]);
    let scope = index_scope(&cleanup);
    assert!(scope.contains("exclusive"), "{cleanup}");
    assert!(!scope.contains("outstanding or unknown"), "{cleanup}");
    assert_eq!(store.recall(&query("neighbour stays")).unwrap().len(), 1);
}
