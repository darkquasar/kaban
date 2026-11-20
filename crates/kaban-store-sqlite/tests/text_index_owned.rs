//! The store owns its text index (spec 11 T7): every open composes it over the catalog, a
//! commit is searchable as soon as it returns, and recall never holds the store's lock while
//! the adapter runs.

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
    use kaban_core::text_index::{Searched, TextHit, TextIndex, TextQuery, TextSnapshot};
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
        MatchMode, Searched, TextHit, TextIndex, TextQuery, TextSnapshot,
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
                MatchMode::Lexical | MatchMode::Exact => vec![self.hit.clone()],
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

#[test]
fn stale_existing_packs_are_rebuilt_and_a_failed_rebuild_is_retried() {
    let scratch = Scratch::new("text-format", "retry");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let note = created(&store, "Fact", "original quokka");
    assert_eq!(recalled(&store, "quokka"), std::slice::from_ref(&note.id));
    drop(store);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &kaban_core::ActorId::default()).unwrap();
    // Model an obsolete projection: this canonical text was never present in the pack.
    conn.execute("UPDATE node SET text = 'wombat' WHERE id=?1", [&note.id])
        .unwrap();
    conn.execute_batch(
        "UPDATE index_meta SET value='old' WHERE key='ident_terms_version';
        CREATE TRIGGER refuse_rebuild BEFORE INSERT ON cat_extent BEGIN
          SELECT RAISE(ABORT,'simulated rebuild failure'); END;",
    )
    .unwrap();
    assert!(
        SqliteStore::open(&scratch.db(), &scratch.keys()).is_err(),
        "publishing the replacement tier must fail"
    );
    conn.execute_batch("DROP TRIGGER refuse_rebuild").unwrap();
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    assert_eq!(recalled(&store, "wombat"), [note.id]);
    assert_eq!(recalled(&store, "quokka"), Vec::<String>::new());
}

#[test]
fn declared_body_and_code_fields_keep_their_ranked_hits_through_reindex() {
    use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextQuery};
    use kaban_core::{IngestStore, NewNode};
    let s = Scratch::new("code-text", "reindex");
    let mut ontology = kaban_core::shipped::resolved().unwrap();
    let fact = ontology
        .kinds
        .iter_mut()
        .find(|(name, _)| kaban_core::stored_name(name) == "Fact")
        .unwrap()
        .1;
    fact.fts_fields = vec!["props.body".into()];
    fact.ident_fields = vec!["props.name".into(), "props.qualified".into()];
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_ontology(ontology.into())
        .unwrap();
    for (name, owner, body) in [
        (
            "snapshot_symbols",
            "CodeGraphReader",
            "quokka quokka quokka",
        ),
        ("clone", "SnapshotSymbol", "quokka wanders slowly"),
        ("configure_connection", "schema", "shore birds"),
        ("snapshot_status", "CodeGraphReader", "birds at the shore"),
    ] {
        store.remember(NewNode {
            kind: "Fact".into(), text: format!("record {name}"),
            props: Some(serde_json::json!({"name":name, "qualified":format!("{owner}::{name}"), "body":body})),
            ..Default::default()
        }, &[], &common::ctx()).unwrap();
    }
    let search = |text: &str, mode| {
        let recall = RecallQuery::default();
        let q = TextQuery {
            text,
            mode,
            filter: SeedFilter::of(&recall),
            limit: 50,
            sufficient: 10,
            skip: 0,
        };
        let Searched::Hits { hits, .. } = store.search_text(&q).unwrap() else {
            panic!("unavailable")
        };
        hits.into_iter().map(|h| h.rid).collect::<Vec<_>>()
    };
    let names: Vec<_> =
        include_str!("../../kaban-index-tantivy/tests/fixtures/s4-parity-queries.jsonl")
            .lines()
            .filter_map(|line| {
                let case: serde_json::Value = serde_json::from_str(line).unwrap();
                Some((
                    case["selector"]["name"].as_str()?.to_owned(),
                    if case["category"] == "fragment" {
                        MatchMode::Lexical
                    } else {
                        MatchMode::Exact
                    },
                ))
            })
            .collect();
    let parity = || {
        names
            .iter()
            .map(|(name, mode)| search(name, *mode))
            .collect::<Vec<_>>()
    };
    let before = parity();
    assert_eq!(search("snapshotSymbols", MatchMode::Lexical), [1]);
    assert_eq!(search("configure_conn", MatchMode::Lexical), [3]);
    assert_eq!(search("quokka", MatchMode::Lexical), [1, 2]);
    assert_eq!(search("\"shore birds\"", MatchMode::Lexical), [3]);
    assert_eq!(store.reindex_text().unwrap(), 4);
    assert_eq!(parity(), before);
    assert_eq!(search("quokka", MatchMode::Lexical), [1, 2]);
    assert_eq!(search("\"shore birds\"", MatchMode::Lexical), [3]);
}

#[test]
fn invalid_catalog_settings_are_refused_before_creating_project_files() {
    let scratch = Scratch::new("text-format", "invalid-catalog");
    let settings = kaban_core::config::CatalogCfg {
        maintenance_memory_mib: 0,
        ..Default::default()
    };
    let opened = SqliteStore::open_as(
        &scratch.db(),
        &scratch.keys(),
        &kaban_core::ActorId::default(),
        settings,
    );
    assert!(
        matches!(opened, Err(kaban_core::StoreError::InvalidName(key, "invalid_configuration")) if key == "catalog.maintenance_memory_mib")
    );
    assert_eq!(
        std::fs::read_dir(&scratch.0).unwrap().count(),
        0,
        "invalid settings must leave the project directory untouched"
    );
}
