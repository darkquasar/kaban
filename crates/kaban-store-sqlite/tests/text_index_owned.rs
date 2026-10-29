//! Spec 05 OWN-M (lane T8a): the store owns its text index. Opening a store yields one whose
//! Tantivy index exists, admits or was rebuilt from SQLite under the exclusive hold; every
//! commit is projected by the writing process after the transaction committed, with no
//! connection guard held; recall never holds the store's lock while the adapter runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created};
use kaban_core::text_index::{Visibility, index_dir};
use kaban_core::{ChangeLogStore, LeaseStore, MemoryStore, NodeId, RecallQuery, StoreError};
use kaban_store_sqlite::SqliteStore;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn recalled(store: &SqliteStore, query: &str) -> Vec<String> {
    store
        .recall(&RecallQuery {
            query: query.into(),
            hops: 0,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect()
}

/// After the barrier's publication (a write marks its commit pending; the barrier publishes
/// it within its deadline) the index serves exactly the canonical head (a lagging or absent
/// index is anything else).
fn assert_current(store: &SqliteStore) {
    store
        .text_publish(u64::MAX, Instant::now() + Duration::from_secs(5))
        .unwrap();
    let head = i64::try_from(store.changes_head().unwrap()).unwrap();
    assert_eq!(
        store.text_visible().unwrap(),
        Visibility::Through(head),
        "the index serves the head"
    );
}

/// A fresh in-memory store answers a recall through its RAM index right after `remember`:
/// the watermark is `Through(head)` (never FTS5's `Current`), and a second write moves it.
#[test]
fn a_fresh_in_memory_store_recalls_through_its_own_index_right_after_remember() {
    let store = SqliteStore::open_memory().unwrap();
    assert_eq!(store.text_visible().unwrap(), Visibility::Through(0));
    let note = created(&store, "Fact", "quokka by the shore");

    assert_eq!(recalled(&store, "quokka"), std::slice::from_ref(&note.id));
    assert_current(&store);

    let second = created(&store, "Fact", "quokka under the trees");
    let mut hits = recalled(&store, "quokka");
    hits.sort();
    let mut expected = [note.id, second.id];
    expected.sort();
    assert_eq!(hits, expected);
    assert_current(&store);
}

/// A fresh file store does the same and leaves its index directory beside the database
/// (`<stem>.index/tantivy/`), committed (a `meta.json` with a payload) before the open returns.
#[test]
fn a_fresh_file_store_recalls_and_keeps_its_index_beside_the_database() {
    let s = Scratch::new("t8a-owned", "file");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let dir = index_dir(&s.db());
    assert!(dir.join("meta.json").is_file(), "built at open: {dir:?}");
    assert_eq!(store.text_visible().unwrap(), Visibility::Through(0));
    let note = created(&store, "Fact", "quokka by the shore");

    assert_eq!(recalled(&store, "quokka"), std::slice::from_ref(&note.id));
    assert_current(&store);

    // a reopen admits the index as it is and serves at once
    drop(store);
    let reopened = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(recalled(&reopened, "quokka"), [note.id]);
    assert_current(&reopened);
}

/// A damaged index directory (its `meta.json` is not Tantivy's) is rebuilt from SQLite at the
/// next open, under the exclusive hold, and serves every row at the head.
#[test]
fn a_damaged_index_directory_is_rebuilt_on_open() {
    let s = Scratch::new("t8a-owned", "damaged");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    drop(store);
    let meta = index_dir(&s.db()).join("meta.json");
    std::fs::write(&meta, b"{not an index").unwrap();

    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();

    assert_eq!(recalled(&store, "quokka"), [note.id]);
    assert_current(&store);
    assert_ne!(std::fs::read(&meta).unwrap(), b"{not an index");
}

const HOLDER_DIR: &str = "KABAN_T8A_HOLDER_DIR";
const HOLDER_TEST: &str =
    "an_open_that_must_rebuild_refuses_with_a_reason_while_another_process_holds_the_database";

/// The other process: opens the store and keeps it open until its `held` marker is removed.
fn hold_database(dir: &std::path::Path) {
    let db = dir.join("kaban.db");
    let _store = SqliteStore::open(&db, &dir.join("keys")).unwrap();
    std::fs::write(dir.join("held"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while dir.join("held").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// While another process holds the database, an open that finds its index damaged cannot
/// take the exclusive hold a rebuild needs: it is refused with a reason naming the other
/// session, never opened over an index that does not admit. Once the other process is gone
/// the same open rebuilds and serves.
#[test]
fn an_open_that_must_rebuild_refuses_with_a_reason_while_another_process_holds_the_database() {
    if let Some(dir) = std::env::var_os(HOLDER_DIR) {
        hold_database(std::path::Path::new(&dir));
        return;
    }
    let s = Scratch::new("t8a-owned", "held");
    let note = {
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        created(&store, "Fact", "quokka by the shore")
    };
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HOLDER_TEST, "--nocapture"])
        .env(HOLDER_DIR, &s.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !s.0.join("held").exists() {
        assert!(
            Instant::now() < deadline,
            "the holder did not open the store"
        );
        assert!(
            holder.try_wait().unwrap().is_none(),
            "the holder exited early"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(index_dir(&s.db()).join("meta.json"), b"{not an index").unwrap();

    let refused = SqliteStore::open(&s.db(), &s.keys());

    let Err(err) = refused else {
        std::fs::remove_file(s.0.join("held")).unwrap();
        let _ = holder.wait();
        panic!("opened over a damaged index while another process holds the database");
    };
    assert!(
        matches!(err, StoreError::TextIndexRebuildRefused(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("another session"), "{err}");
    std::fs::remove_file(s.0.join("held")).unwrap();
    let out = holder.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(recalled(&store, "quokka"), [note.id]);
    assert_current(&store);
}

/// A projection that cannot publish after the commit (another holder owns the `text-index`
/// lease, as a serving process would) never turns the committed write into a failure: the row
/// is there, the watermark lags, and the next publication catches up.
#[test]
fn a_post_commit_projection_that_cannot_publish_leaves_the_row_committed() {
    let s = Scratch::new("t8a-owned", "lease-held");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let first = created(&store, "Fact", "quokka by the shore");
    assert_current(&store);
    let before = store.text_visible().unwrap();
    let lease = store
        .lease_acquire("text-index", "another:holder", 60)
        .unwrap()
        .unwrap();

    let second = created(&store, "Fact", "quokka under the trees");

    assert!(
        store
            .get(&NodeId::parse(&second.id).unwrap(), false)
            .unwrap()
            .is_some(),
        "committed"
    );
    assert_eq!(store.text_visible().unwrap(), before, "not yet searchable");
    assert_eq!(recalled(&store, "quokka"), std::slice::from_ref(&first.id));
    store.lease_release(&lease).unwrap();

    let third = created(&store, "Fact", "quokka at the ferry");
    assert_current(&store);
    let mut hits = recalled(&store, "quokka");
    hits.sort();
    let mut expected = [first.id, second.id, third.id];
    expected.sort();
    assert_eq!(hits, expected);
}

/// Constraint 1 (lock ordering): recall never holds the store's connection lock while the
/// adapter runs. An adapter whose `search` re-enters the store (reads the change-log head,
/// which takes the lock) completes; with the lock held across the search it would deadlock.
#[cfg(feature = "text-index-override")]
#[test]
fn recall_never_holds_the_store_lock_while_the_adapter_runs() {
    use kaban_core::Change;
    use kaban_core::text_index::{
        PurgeOutcome, Searched, TextHit, TextIndex, TextQuery, Visibility,
    };
    use std::sync::{Arc, OnceLock, Weak};

    /// Scripted hits, read back through the store on every search.
    struct Reentrant {
        store: OnceLock<Weak<SqliteStore>>,
        hit: TextHit,
    }

    impl TextIndex for Reentrant {
        fn search(&self, _query: &TextQuery<'_>) -> kaban_core::Result<Searched> {
            let store = self.store.get().and_then(Weak::upgrade).unwrap();
            // re-enters the store: a lock held by the recall in progress would deadlock here
            let _head = store.changes_head()?;
            Ok(Searched::Hits {
                hits: vec![self.hit.clone()],
                exhausted: true,
            })
        }
        fn visible(&self) -> kaban_core::Result<Visibility> {
            Ok(Visibility::Current)
        }
        fn project(&self, _changes: &[Change]) -> kaban_core::Result<()> {
            Ok(())
        }
        fn rebuild(&self) -> kaban_core::Result<()> {
            Ok(())
        }
        fn purge(&self, _rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
            Ok(PurgeOutcome {
                cleaned: true,
                detail: None,
            })
        }
        fn pin(&self) -> kaban_core::Result<Box<dyn TextIndex + '_>> {
            let store = self.store.get().and_then(Weak::upgrade).unwrap();
            let _head = store.changes_head()?;
            Ok(Box::new(self))
        }
    }

    let store = SqliteStore::open_memory().unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    // the first row of a fresh store: rid 1 at the revision the store returned
    let (rid, revision) = (1, note.rev);
    let adapter = Arc::new(Reentrant {
        store: OnceLock::new(),
        hit: TextHit {
            rid,
            revision,
            score: 1.0,
            kind: "Fact".into(),
        },
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

/// Review t8a-2: a crashed projector left an unexpired `text-index` lease and no index. An
/// open never returns a store whose index silently answers empty: it waits for the lease
/// (bounded) and completes the index to the head, or refuses with a typed error.
#[test]
fn an_open_behind_a_crashed_projectors_lease_completes_or_refuses_never_serves_empty() {
    let s = Scratch::new("t8a-owned", "crashed-lease");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    // the crash: the lease row outlives its process, the index does not
    store
        .lease_acquire("text-index", "crashed:projector", 2)
        .unwrap()
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(index_dir(&s.db())).unwrap();

    let t0 = Instant::now();
    match SqliteStore::open(&s.db(), &s.keys()) {
        Ok(store) => {
            assert!(
                t0.elapsed() >= Duration::from_millis(500),
                "the open waited for the lease: {:?}",
                t0.elapsed()
            );
            assert_current(&store);
            assert_eq!(recalled(&store, "quokka"), [note.id]);
        }
        Err(StoreError::TextIndexUnavailable) => {}
        Err(other) => panic!("a typed refusal, not {other:?}"),
    }
}

/// Review t8a-6: a diagnostic open reads the index as it stands and never repairs its
/// permissions; only a writable attach tightens modes.
#[test]
fn a_diagnostic_open_leaves_the_index_modes_untouched() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("t8a-owned", "diagnostic-modes");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    created(&store, "Fact", "quokka by the shore");
    drop(store);
    let dir = index_dir(&s.db());
    let meta = dir.join("meta.json");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::set_permissions(&meta, std::fs::Permissions::from_mode(0o640)).unwrap();

    let (store, _) =
        SqliteStore::open_diagnostic(&s.db(), &s.keys(), &kaban_core::ActorId::default()).unwrap();

    assert!(
        store.text_visible().is_ok(),
        "the index is read as it stands"
    );
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o750, "the directory mode stands");
    assert_eq!(mode(&meta), 0o640, "the file mode stands");
}

/// Rows of the large ingest below: enough that a Tantivy commit per write (7 ms in RAM, 13 ms
/// and more on disk, fix15) would take minutes, while the transactions alone take seconds.
const LARGE_INGEST_ROWS: usize = 20_000;

/// A write never commits the index itself (fix16): a large ingest costs its SQLite
/// transactions only and leaves the index lagging; the recall right after projects what is
/// pending in this process before it seeds, within the publication budget.
#[test]
fn a_large_ingest_commits_without_projecting_each_write_and_a_recall_right_after_converges() {
    let s = Scratch::new("t8a-owned", "large-ingest");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let t0 = Instant::now();
    let mut last = None;
    for i in 0..LARGE_INGEST_ROWS {
        last = Some(created(
            &store,
            "Fact",
            &format!("quokka row{i} by the shore"),
        ));
    }
    let ingest = t0.elapsed();
    // seconds, not minutes: 7-8 s alone, 10-11 s under the suite's load (a commit per write
    // ran past six minutes)
    assert!(
        ingest < Duration::from_secs(30),
        "{LARGE_INGEST_ROWS} rows committed in {ingest:?}"
    );
    let last = last.unwrap();

    let t1 = Instant::now();
    let hits = recalled(&store, &format!("row{}", LARGE_INGEST_ROWS - 1));
    let recall = t1.elapsed();

    assert_eq!(hits, [last.id]);
    assert!(
        recall < Duration::from_secs(5),
        "the recall converged within the budget: {recall:?}"
    );
    assert_current(&store);
}

/// Review t8a-3 (SS-11): a seed revalidated after its search can still change before the
/// fusion (here: during the fuzzy retrieval that follows). The materialization compares the
/// row's revision with the hit's under the one lock it holds and drops the mismatch, so new
/// text is never returned under an obsolete match and score.
#[cfg(feature = "text-index-override")]
#[test]
fn a_seed_whose_row_changed_between_revalidation_and_fusion_is_dropped() {
    use kaban_core::Change;
    use kaban_core::text_index::{
        MatchMode, PurgeOutcome, Searched, TextHit, TextIndex, TextQuery, Visibility,
    };
    use std::sync::Mutex;

    /// The lexical seed, then a write to its row during the fuzzy search.
    struct Editing {
        conn: Mutex<rusqlite::Connection>,
        hit: TextHit,
    }

    impl TextIndex for Editing {
        fn search(&self, query: &TextQuery<'_>) -> kaban_core::Result<Searched> {
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
        fn visible(&self) -> kaban_core::Result<Visibility> {
            Ok(Visibility::Current)
        }
        fn project(&self, _changes: &[Change]) -> kaban_core::Result<()> {
            Ok(())
        }
        fn rebuild(&self) -> kaban_core::Result<()> {
            Ok(())
        }
        fn purge(&self, _rids: &[i64]) -> kaban_core::Result<PurgeOutcome> {
            Ok(PurgeOutcome {
                cleaned: true,
                detail: None,
            })
        }
        fn pin(&self) -> kaban_core::Result<Box<dyn TextIndex + '_>> {
            Ok(Box::new(self))
        }
    }

    let s = Scratch::new("t8a-owned", "stale-seed");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    SqliteStore::bind_writer(&conn, &kaban_core::ActorId::default()).unwrap();
    let store = store.with_text_index(Box::new(Editing {
        conn: Mutex::new(conn),
        hit: TextHit {
            rid: 1,
            revision: note.rev,
            score: 1.0,
            kind: "Fact".into(),
        },
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

/// Review t8a-1 (SS-09, SS-12): a rebuild never unlinks the lock files another adapter holds.
/// While an adapter of this directory lives (its readers' mark, in this or another process),
/// an open that must rebuild waits for it, bounded, instead of deleting the directory under
/// it; once the adapter is gone the same open rebuilds and serves.
#[test]
fn a_rebuild_waits_for_an_adapter_that_holds_the_index_directory() {
    let s = Scratch::new("t8a-owned", "rebuild-waits");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let note = created(&store, "Fact", "quokka by the shore");
    drop(store);
    let dir = index_dir(&s.db());
    // another adapter of the directory (an adapter-only process: no database hold)
    let holder = kaban_index_tantivy::TantivyIndex::open(
        &dir,
        &kaban_core::shipped::resolved().unwrap(),
        &kaban_core::config::Recall::default(),
        Box::new(kaban_store_sqlite::index_rows::IndexRows::open(&s.db()).unwrap()),
    )
    .unwrap();
    let meta = dir.join("meta.json");
    std::fs::write(&meta, b"{not an index").unwrap();
    let hold = Duration::from_secs(1);

    let t0 = Instant::now();
    let (store, elapsed) = std::thread::scope(|sc| {
        let opener = sc.spawn(|| {
            let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
            (store, t0.elapsed())
        });
        std::thread::sleep(hold);
        drop(holder);
        opener.join().unwrap()
    });

    assert!(
        elapsed >= hold,
        "the rebuild waited for the adapter holding the directory: {elapsed:?}"
    );
    assert_eq!(recalled(&store, "quokka"), [note.id]);
    assert_current(&store);
}

/// Constraint 1 for the purge (review t8a-5 leftover): `forget(Purge)` runs the adapter's
/// erasure after its transaction committed and with the connection lock released. An adapter
/// whose `purge` re-enters the store completes; with the lock held across the erasure it
/// would deadlock.
#[cfg(feature = "text-index-override")]
#[test]
fn purge_never_holds_the_store_lock_while_the_adapter_runs() {
    use kaban_core::text_index::{PurgeOutcome, Searched, TextIndex, TextQuery, Visibility};
    use kaban_core::{Change, ForgetMode};
    use std::sync::{Arc, OnceLock, Weak};

    struct Reentrant(OnceLock<Weak<SqliteStore>>);

    impl TextIndex for Reentrant {
        fn search(&self, _query: &TextQuery<'_>) -> kaban_core::Result<Searched> {
            Ok(Searched::Hits {
                hits: vec![],
                exhausted: true,
            })
        }
        fn visible(&self) -> kaban_core::Result<Visibility> {
            Ok(Visibility::Current)
        }
        fn project(&self, _changes: &[Change]) -> kaban_core::Result<()> {
            Ok(())
        }
        fn rebuild(&self) -> kaban_core::Result<()> {
            Ok(())
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
        fn pin(&self) -> kaban_core::Result<Box<dyn TextIndex + '_>> {
            Ok(Box::new(self))
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

/// The live `text-index` lease holder, as the lease table names it.
fn lease_owner(db: &std::path::Path) -> Option<String> {
    use rusqlite::OptionalExtension;
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT owner FROM leases WHERE name = 'text-index' AND expires_at > unixepoch()",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

/// A serving store (lane T8b, OWN-M): one persistent projector thread of the store holds the
/// `text-index` lease under the serving owner name and publishes every commit as it lands, so
/// a write is searchable with no caller draining anything; the barrier routes through that
/// thread (its publication, never a halt), and the store's close stops it and releases the
/// lease. A store that does not serve holds no lease between calls.
#[test]
fn a_serving_store_publishes_through_its_own_projector_thread() {
    let s = Scratch::new("t8b-owned", "serving");
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .serving(true)
        .unwrap();
    let serve_owner = format!("kaban:{}:serve", std::process::id());
    let deadline = Instant::now() + Duration::from_secs(5);
    while lease_owner(&s.db()).as_deref() != Some(&serve_owner) {
        assert!(Instant::now() < deadline, "the thread never took the lease");
        std::thread::sleep(Duration::from_millis(20));
    }
    let note = created(&store, "Fact", "quokka by the shore");
    let head = i64::try_from(store.changes_head().unwrap()).unwrap();
    // the barrier's path: covered by the thread within the deadline
    store
        .text_publish(
            store.changes_head().unwrap(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(store.text_visible().unwrap(), Visibility::Through(head));
    assert_eq!(recalled(&store, "quokka"), std::slice::from_ref(&note.id));
    assert_eq!(
        lease_owner(&s.db()).as_deref(),
        Some(serve_owner.as_str()),
        "the thread keeps the lease between writes"
    );
    drop(store);
    assert_eq!(lease_owner(&s.db()), None, "the close released the lease");

    let plain = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .serving(false)
        .unwrap();
    let second = created(&plain, "Fact", "quokka under the trees");
    assert_current(&plain);
    assert_eq!(
        lease_owner(&s.db()),
        None,
        "a one-shot store holds no lease between calls"
    );
    let mut hits = recalled(&plain, "quokka");
    hits.sort();
    let mut expected = [note.id, second.id];
    expected.sort();
    assert_eq!(hits, expected);
}

/// Lane T8d.2 (astra, session positions): a read drains only toward the position of its own
/// store's last receipt. A second store on the same file that wrote nothing recalls the index
/// as it stands: the first store's commit, still pending, is not published by the second's
/// read (the watermark does not move; before, "index behind head" made every read drain), and
/// the first store's own read publishes it.
#[test]
fn a_read_with_no_uncovered_receipt_position_does_not_publish() {
    let s = Scratch::new("t8d-owned", "read-session-position");
    let writer = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let before = writer.text_visible().unwrap();
    let note = created(&writer, "Fact", "quokka pending in the writer's session");
    let head = i64::try_from(writer.changes_head().unwrap()).unwrap();
    assert_ne!(before, Visibility::Through(head));

    assert_eq!(recalled(&reader, "quokka"), Vec::<String>::new());

    assert_eq!(
        reader.text_visible().unwrap(),
        before,
        "the reader's session has no uncovered position: nothing published"
    );
    assert_eq!(recalled(&writer, "quokka"), vec![note.id]);
    assert_eq!(
        writer.text_visible().unwrap(),
        Visibility::Through(head),
        "the writer's own read publishes toward its receipt"
    );
}

/// T8b review t8b-3 (closed by the receipt position of T8d.2): a barrier that covered an
/// older position never clears newer pending work. Write A and publish it, write B, close the
/// store: B is published at the close, as another handle on the same file sees without
/// draining anything itself.
#[test]
fn a_covered_older_barrier_does_not_lose_a_later_commits_close_publication() {
    let s = Scratch::new("t8d-owned", "close-publishes-later-commit");
    let writer = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let reader = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    created(&writer, "Fact", "quokka a published by its barrier");
    let a = writer.changes_head().unwrap();
    writer
        .text_publish(a, Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        writer.text_visible().unwrap(),
        Visibility::Through(i64::try_from(a).unwrap())
    );
    let b = created(&writer, "Fact", "wombat b pending at the close");
    let head = i64::try_from(writer.changes_head().unwrap()).unwrap();

    drop(writer);

    assert_eq!(
        reader.text_visible().unwrap(),
        Visibility::Through(head),
        "the close published the later commit"
    );
    assert_eq!(recalled(&reader, "wombat"), vec![b.id]);
}
