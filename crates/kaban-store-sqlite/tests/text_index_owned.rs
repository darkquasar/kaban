//! The store owns its text index (spec 11 T7): every open composes it over the catalog, a
//! commit is searchable as soon as it returns, and recall and purge never hold the store's
//! lock while the adapter runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created};
use kaban_core::{ChangeLogStore, MemoryStore, NodeId, RecallQuery};
use kaban_store_sqlite::SqliteStore;
use std::time::{Duration, Instant};

fn recalled(store: &SqliteStore, query: &str) -> Vec<String> {
    let mut ids: Vec<String> = store
        .recall(&RecallQuery {
            query: query.into(),
            hops: 0,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect();
    ids.sort();
    ids
}

/// A fresh in-memory store answers a recall right after `remember`, and a second write is
/// found by the next recall.
#[test]
fn a_fresh_in_memory_store_recalls_through_its_own_index_right_after_remember() {
    let store = SqliteStore::open_memory().unwrap();
    let note = created(&store, "Fact", "quokka by the shore");

    assert_eq!(recalled(&store, "quokka"), std::slice::from_ref(&note.id));

    let second = created(&store, "Fact", "quokka under the trees");
    let mut expected = vec![note.id, second.id];
    expected.sort();
    assert_eq!(recalled(&store, "quokka"), expected);
}

/// A fresh file store does the same, and a reopen serves what it wrote at once.
#[test]
fn a_fresh_file_store_recalls_and_a_reopen_serves_at_once() {
    let s = Scratch::new("t7-owned", "file");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let note = created(&store, "Fact", "quokka by the shore");

    assert_eq!(recalled(&store, "quokka"), std::slice::from_ref(&note.id));

    drop(store);
    let reopened = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(recalled(&reopened, "quokka"), [note.id]);
}

/// Constraint 1 (lock ordering): recall never holds the store's connection lock while the
/// adapter runs. An adapter whose `search` re-enters the store (reads the change-log head,
/// which takes the lock) completes; with the lock held across the search it would deadlock.
#[cfg(feature = "text-index-override")]
#[test]
fn recall_never_holds_the_store_lock_while_the_adapter_runs() {
    use kaban_core::query_budget::QueryBudget;
    use kaban_core::text_index::{
        PurgeOutcome, Searched, TextHit, TextIndex, TextQuery, TextSnapshot,
    };
    use std::sync::{Arc, OnceLock, Weak};

    /// Scripted hits, read back through the store on every search.
    struct Reentrant {
        store: OnceLock<Weak<SqliteStore>>,
        hit: TextHit,
    }

    impl TextIndex for Reentrant {
        fn search(
            &self,
            _at: &TextSnapshot<'_>,
            _query: &TextQuery<'_>,
            _budget: &QueryBudget,
        ) -> kaban_core::Result<Searched> {
            let store = self.store.get().and_then(Weak::upgrade).unwrap();
            // re-enters the store: a lock held by the recall in progress would deadlock here
            let _head = store.changes_head()?;
            Ok(Searched::Hits {
                hits: vec![self.hit.clone()],
                exhausted: true,
            })
        }
        fn purge(&self, _rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
            Ok(PurgeOutcome {
                cleaned: true,
                detail: None,
            })
        }
    }

    let store = SqliteStore::open_memory().unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    // the first row of a fresh store: rid 1
    let adapter = Arc::new(Reentrant {
        store: OnceLock::new(),
        hit: TextHit { rid: 1, score: 1.0 },
    });
    let store = Arc::new(store.with_text_index(Box::new(Arc::clone(&adapter))));
    adapter.store.set(Arc::downgrade(&store)).ok().unwrap();

    let worker = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || recalled(&store, "quokka"))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() {
        assert!(
            Instant::now() < deadline,
            "recall holds the store lock while the adapter runs: deadlock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(worker.join().unwrap(), [note.id]);
}

/// Review t8a-3 (SS-11): a seed revalidated after its search can still change before the
/// fusion (here: during the fuzzy retrieval that follows). The materialization compares the
/// row's revision with the one the seed was revalidated at, under the one lock it holds, and
/// drops the mismatch, so new text is never returned under an obsolete match and score.
#[cfg(feature = "text-index-override")]
#[test]
fn a_seed_whose_row_changed_between_revalidation_and_fusion_is_dropped() {
    use kaban_core::query_budget::QueryBudget;
    use kaban_core::text_index::{
        MatchMode, PurgeOutcome, Searched, TextHit, TextIndex, TextQuery, TextSnapshot,
    };
    use std::sync::Mutex;

    /// The lexical seed, then a write to its row during the fuzzy search.
    struct Editing {
        conn: Mutex<rusqlite::Connection>,
        hit: TextHit,
    }

    impl TextIndex for Editing {
        fn search(
            &self,
            _at: &TextSnapshot<'_>,
            query: &TextQuery<'_>,
            _budget: &QueryBudget,
        ) -> kaban_core::Result<Searched> {
            let hits = match query.mode {
                MatchMode::Lexical => vec![self.hit.clone()],
                MatchMode::Fuzzy => {
                    self.conn
                        .lock()
                        .unwrap()
                        .execute(
                            "UPDATE node SET text = 'wombat instead', rev = rev + 1 WHERE rid = ?1",
                            [self.hit.rid],
                        )
                        .unwrap();
                    vec![]
                }
            };
            Ok(Searched::Hits {
                hits,
                exhausted: true,
            })
        }
        fn purge(&self, _rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
            Ok(PurgeOutcome {
                cleaned: true,
                detail: None,
            })
        }
    }

    let s = Scratch::new("t8a-owned", "stale-seed");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    SqliteStore::bind_writer(&conn, &kaban_core::ActorId::default()).unwrap();
    let store = store.with_text_index(Box::new(Editing {
        conn: Mutex::new(conn),
        hit: TextHit { rid: 1, score: 1.0 },
    }));

    let hits = store
        .recall(&RecallQuery {
            query: "quokka".into(),
            hops: 0,
            fuzzy: true,
            ..Default::default()
        })
        .unwrap();

    assert!(hits.is_empty(), "the stale seed is rejected: {hits:?}");
    let now = store
        .get(&NodeId::parse(&note.id).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        (now.text.as_str(), now.rev),
        ("wombat instead", note.rev + 1)
    );
}

/// Constraint 1 for the purge (review t8a-5 leftover): `forget(Purge)` runs the adapter's
/// hygiene after its transaction committed and with the connection lock released. An adapter
/// whose `purge` re-enters the store completes; with the lock held across it it would
/// deadlock.
#[cfg(feature = "text-index-override")]
#[test]
fn purge_never_holds_the_store_lock_while_the_adapter_runs() {
    use kaban_core::ForgetMode;
    use kaban_core::query_budget::QueryBudget;
    use kaban_core::text_index::{PurgeOutcome, Searched, TextIndex, TextQuery, TextSnapshot};
    use std::sync::{Arc, OnceLock, Weak};

    struct Reentrant(OnceLock<Weak<SqliteStore>>);

    impl TextIndex for Reentrant {
        fn search(
            &self,
            _at: &TextSnapshot<'_>,
            _query: &TextQuery<'_>,
            _budget: &QueryBudget,
        ) -> kaban_core::Result<Searched> {
            Ok(Searched::Hits {
                hits: vec![],
                exhausted: true,
            })
        }
        fn purge(&self, _rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
            let store = self.0.get().and_then(Weak::upgrade).unwrap();
            // re-enters the store: a lock held across the erasure would deadlock here
            let _head = store.changes_head()?;
            Ok(PurgeOutcome {
                cleaned: true,
                detail: None,
            })
        }
    }

    let store = SqliteStore::open_memory().unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    let adapter = Arc::new(Reentrant(OnceLock::new()));
    let store = Arc::new(store.with_text_index(Box::new(Arc::clone(&adapter))));
    adapter.0.set(Arc::downgrade(&store)).ok().unwrap();

    let worker = {
        let store = Arc::clone(&store);
        let id = NodeId::parse(&note.id).unwrap();
        std::thread::spawn(move || store.forget(&id, ForgetMode::Purge, &common::ctx()))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() {
        assert!(
            Instant::now() < deadline,
            "forget(Purge) holds the store lock while the adapter runs: deadlock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let outcome = worker.join().unwrap().unwrap();
    assert!(outcome.revoked, "{outcome:?}");
}
