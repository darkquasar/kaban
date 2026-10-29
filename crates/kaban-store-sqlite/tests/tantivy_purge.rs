//! Physical erasure after a purge (spec 05 SS-12) through the store's own `forget(Purge)` with
//! the adapter attached: the canary leaves the database, the WAL and the index files, the
//! revoked content never returns through recall (a reader pinned before the purge included),
//! and the report keeps the database/WAL, index and temporary-output scopes apart while never
//! claiming an unqualified physical cleanup in a shared directory (other processes' mappings
//! are outstanding or unknown).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "tantivy_common/mod.rs"]
mod common;

use common::{Scratch, encoded, note, seed};
use kaban_core::text_index::{MatchMode, Searched, TextIndex};
use kaban_core::{ForgetMode, MemoryStore, NodeId, RecallQuery, SyncStore};
use kaban_index_tantivy::projector::Projector;
use std::path::Path;
use std::sync::Arc;

/// A token no other row carries; checked as raw bytes in every file.
const CANARY: &str = "zqxcanary7731";

/// The doomed body: the canary inside enough prose for the body codec to compress it.
fn doomed_body() -> String {
    format!(
        "the {CANARY} note: a token no other record carries, kept in the shore log beside \
         the decision record so the whole team can read it"
    )
}

/// Whether the file `path` holds `needle` as raw bytes.
fn holds(path: &Path, needle: &[u8]) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
}

/// Whether any searchable segment's term dictionary of any text field names `term` (the
/// dictionaries are FSTs, so a byte search of the files cannot stand in for this; a merged
/// segment writes alive documents only, so a purged row's terms leave the dictionary with the
/// segments that held them).
fn index_names(dir: &Path, term: &str) -> bool {
    use tantivy::schema::FieldType;
    let index = tantivy::Index::open_in_dir(dir).unwrap();
    let searcher = index.reader().unwrap().searcher();
    let schema = index.schema();
    searcher.segment_readers().iter().any(|segment| {
        schema.fields().any(|(field, entry)| {
            matches!(entry.field_type(), FieldType::Str(_))
                && entry.is_indexed()
                && segment
                    .inverted_index(field)
                    .unwrap()
                    .terms()
                    .get(term.as_bytes())
                    .unwrap()
                    .is_some()
        })
    })
}

fn query(text: &str) -> RecallQuery {
    RecallQuery {
        query: text.into(),
        hops: 0,
        ..Default::default()
    }
}

/// The canary over an encoded body (spec 05 SS-17 reuses the SS-12 canaries): the old reader
/// is pinned before the purge and revalidated after it, the scopes are reported, and no byte of
/// the canary survives in the database, the WAL or a term dictionary.
#[test]
fn a_purge_scrubs_the_index_files_and_reports_each_scope_without_an_unqualified_claim() {
    let s = Scratch::new("purge-canary");
    let store = s.store(); // the database exists before the adapter's read-only connection opens
    let index = Arc::new(s.adapter());
    let store = store.with_text_index(Box::new(Arc::clone(&index)));
    let doomed = note(&store, "default", "Fact", &doomed_body(), &[]);
    assert!(encoded(&s.db(), &doomed.id), "the body's shape");
    note(&store, "default", "Fact", "a neighbour that stays", &[]);
    let projector = Projector::new(index.as_ref(), &store, &store, "test:purge", 2);
    assert_eq!(projector.catch_up().unwrap().halt, None);
    let dir = s.index_dir();
    assert!(index_names(&dir, CANARY), "the canary is indexed first");
    assert_eq!(store.recall(&query(CANARY)).unwrap().len(), 1);
    // a reader pinned before the purge
    let pinned = index.pin().unwrap();

    let outcome = store
        .forget(
            &NodeId::parse(&doomed.id).unwrap(),
            ForgetMode::Purge,
            &common::ctx("default"),
        )
        .unwrap();

    assert!(outcome.revoked);
    assert!(
        !outcome.physically_cleaned,
        "other processes' mappings are unknown: never an unqualified claim"
    );
    let cleanup = outcome.cleanup.clone().unwrap_or_default();
    for scope in ["database/WAL:", "index:", "temporary outputs:"] {
        assert!(cleanup.contains(scope), "{cleanup}");
    }
    assert!(
        cleanup.contains("other readers outstanding or unknown"),
        "{cleanup}"
    );
    assert!(
        !index_names(&dir, CANARY),
        "the canary left every term dictionary"
    );
    assert!(!holds(&s.db(), CANARY.as_bytes()), "the database");
    let wal = s.db().with_extension("db-wal");
    assert!(!wal.exists() || !holds(&wal, CANARY.as_bytes()), "the WAL");
    // revoked content never returns: a fresh recall, and a recall over the old pinned reader
    assert!(store.recall(&query(CANARY)).unwrap().is_empty());
    let hits = match pinned
        .search(&seed(&query(CANARY), MatchMode::Lexical, 10, 0))
        .unwrap()
    {
        Searched::Hits { hits, .. } => hits,
        Searched::Unavailable => vec![],
    };
    for hit in hits {
        let row: Option<String> = rusqlite::Connection::open(s.db())
            .unwrap()
            .query_row(
                "SELECT lifecycle FROM node WHERE rid = ?1 AND lifecycle = 'active'",
                [hit.rid],
                |r| r.get(0),
            )
            .ok();
        assert_eq!(row, None, "an old reader's hit is revalidated away");
    }
    assert_eq!(store.recall(&query("neighbour stays")).unwrap().len(), 1);
}

/// An imported purge (a tombstone synced in, `SyncStore::import_purged`) is the other
/// canonical purge path (T2 review t2-7, SS-12): it runs the same post-commit index erase as
/// `forget(Purge)` and reports the index scope beside the database's. With an old reader
/// held, the canary leaves every term dictionary and the revoked content never returns.
#[test]
fn an_imported_purge_scrubs_the_index_like_forget_and_reports_the_index_scope() {
    let s = Scratch::new("purge-imported");
    let store = s.store();
    let index = Arc::new(s.adapter());
    let store = store.with_text_index(Box::new(Arc::clone(&index)));
    let doomed = note(&store, "default", "Fact", &doomed_body(), &[]);
    assert!(encoded(&s.db(), &doomed.id), "the body's shape");
    let projector = Projector::new(index.as_ref(), &store, &store, "test:purge", 2);
    assert_eq!(projector.catch_up().unwrap().halt, None);
    let dir = s.index_dir();
    assert!(index_names(&dir, CANARY), "the canary is indexed first");
    let pinned = index.pin().unwrap();

    let imported = store
        .import_purged(
            &NodeId::parse(&doomed.id).unwrap(),
            "Fact",
            5,
            &common::ctx("default"),
        )
        .unwrap();

    let cleanup = imported.cleanup.clone().unwrap_or_default();
    for scope in ["database/WAL:", "index:", "temporary outputs:"] {
        assert!(cleanup.contains(scope), "{imported:?}");
    }
    assert!(
        cleanup.contains("other readers outstanding or unknown"),
        "{cleanup}"
    );
    assert!(
        !index_names(&dir, CANARY),
        "the canary left every term dictionary"
    );
    assert!(store.recall(&query(CANARY)).unwrap().is_empty());
    let hits = match pinned
        .search(&seed(&query(CANARY), MatchMode::Lexical, 10, 0))
        .unwrap()
    {
        Searched::Hits { hits, .. } => hits,
        Searched::Unavailable => vec![],
    };
    for hit in hits {
        let row: Option<String> = rusqlite::Connection::open(s.db())
            .unwrap()
            .query_row(
                "SELECT lifecycle FROM node WHERE rid = ?1 AND lifecycle = 'active'",
                [hit.rid],
                |r| r.get(0),
            )
            .ok();
        assert_eq!(row, None, "an old reader's hit is revalidated away");
    }
}
