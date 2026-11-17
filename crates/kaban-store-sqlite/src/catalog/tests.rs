//! The commit entry's phase contract (spec 11 T5 midpoint review MAJ-01, MED-01, LOW-01): after
//! the capture freeze no canonical row may change, the staging phase writes sediment only and
//! cannot outlive its own generation, and an unwound writer leaves the connection as it found
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::{
    Actor, Durability, MemoryStore, NewNode, NodeId, RememberOutcome, Result, StoreError,
    WorkspaceId, WriteCtx,
};
use rusqlite::Connection;

use super::Catalog;
use super::capture::{BatchChanges, CaptureOp};
use super::class::{ExtentClass, IndexClass, SealOutput};
use super::commit::Stage;
use super::pin::Pin;
use crate::SqliteStore;

fn ctx() -> WriteCtx {
    WriteCtx::new(WorkspaceId::default(), Actor::Agent)
}

/// A fresh in-memory store with two nodes; their ids and `rid`s.
fn store_with_two_nodes() -> (SqliteStore, [(String, i64); 2]) {
    let store = SqliteStore::open_memory().unwrap();
    let mut made = Vec::new();
    for text in ["the tide turns at six", "the harbour gate closes at nine"] {
        let node = match store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    ..Default::default()
                },
                &[],
                &ctx(),
            )
            .unwrap()
        {
            RememberOutcome::Created(n) => n,
            RememberOutcome::Existing { .. } => panic!("a fresh store has no {text:?}"),
        };
        let rid = rid(&store.lock(), &node.id);
        made.push((node.id, rid));
    }
    let [a, b] = <[_; 2]>::try_from(made).unwrap();
    (store, [a, b])
}

fn rid(conn: &Connection, id: &str) -> i64 {
    conn.query_row("SELECT rid FROM node WHERE id = ?1", [id], |r| r.get(0))
        .unwrap()
}

fn lifecycle(conn: &Connection, rid: i64) -> String {
    conn.query_row("SELECT lifecycle FROM node WHERE rid = ?1", [rid], |r| {
        r.get(0)
    })
    .unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn link(store: &SqliteStore, from: &str, to: &str) -> Result<()> {
    store
        .link(
            &NodeId::parse(from).unwrap(),
            &NodeId::parse(to).unwrap(),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .map(drop)
}

/// What a scripted class does in `stage`.
type Script = Box<dyn Fn(&mut Stage<'_>, &BatchChanges) -> Result<()> + Send + Sync>;

/// A test class: runs its script, and records every change set it was handed.
struct Scripted {
    script: Script,
    seen: Arc<Mutex<Vec<BatchChanges>>>,
}

impl Scripted {
    fn install(catalog: &Catalog, script: Script) -> Arc<Mutex<Vec<BatchChanges>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        catalog.register(Arc::new(Self {
            script,
            seen: Arc::clone(&seen),
        }));
        seen
    }
}

impl IndexClass for Scripted {
    fn class(&self) -> ExtentClass {
        ExtentClass::Test
    }
    fn stage(&self, txn: &mut Stage<'_>, changes: &BatchChanges) -> Result<()> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(changes.clone());
        (self.script)(txn, changes)
    }
    fn seal(&self, _pin: &Pin) -> Result<Option<SealOutput>> {
        Ok(None)
    }
    fn fold(&self, _pin: &Pin) -> Result<Option<SealOutput>> {
        Ok(None)
    }
    fn purge(&self, _pin: &Pin, _rids: &[i64]) -> Result<Option<SealOutput>> {
        Ok(None)
    }
    fn rebuild(&self, _catalog: &Catalog, _rows: &Connection) -> Result<()> {
        Ok(())
    }
}

/// The named refusal of a commit that broke the phase contract.
fn assert_refused(err: &StoreError, what: &str) {
    let StoreError::Backend { source } = err else {
        panic!("{what}: not a refused commit: {err:?}");
    };
    assert!(
        source
            .driver()
            .to_string()
            .starts_with("catalog_commit_refused"),
        "{what}: not the named refusal: {}",
        source.driver()
    );
}

/// The edge changes of a change set as `(op, src, dst)`.
fn edge_ops(changes: &BatchChanges) -> Vec<(CaptureOp, i64, i64)> {
    changes
        .edges()
        .iter()
        .map(|e| (e.op, e.src, e.dst))
        .collect()
}

/// A class that archives node `rid` through the connection while staging: a canonical write
/// after the freeze. `cached` runs a statement the writer phase prepared (no fresh prepare).
fn archiving(rid: i64, cached: bool) -> Script {
    Box::new(move |txn, _| {
        let sql = "UPDATE node SET lifecycle = 'archived' WHERE rid = ?1";
        let conn = txn.conn_for_test();
        let done = if cached {
            conn.prepare_cached(sql).and_then(|mut s| s.execute([rid]))
        } else {
            conn.execute(sql, [rid])
        };
        // a class that swallows the failure must not slip through either
        let _ = done;
        Ok(())
    })
}

#[test]
fn a_class_that_writes_a_canonical_row_while_staging_is_refused_and_rolled_back() {
    for cached in [false, true] {
        // Arrange: two classes, the second archives node a while staging; `cached`: with a
        // statement prepared before the commit (no fresh prepare in the staging phase)
        let (store, [(a, ra), (b, _)]) = store_with_two_nodes();
        if cached {
            store
                .lock()
                .prepare_cached("UPDATE node SET lifecycle = 'archived' WHERE rid = ?1")
                .map(drop)
                .unwrap();
        }
        let catalog = store.catalog();
        Scripted::install(
            catalog,
            Box::new(|txn, changes| {
                for _ in changes.edges() {
                    txn.put_sediment(ExtentClass::Test, b"edge")?;
                }
                Ok(())
            }),
        );
        Scripted::install(catalog, archiving(ra, cached));

        // Act
        let refused = link(&store, &a, &b).unwrap_err();

        // Assert: refused by name, nothing of it committed
        assert_refused(&refused, "a canonical write while staging");
        let conn = store.lock();
        assert_eq!(
            lifecycle(&conn, ra),
            "active",
            "cached {cached}: the archive committed"
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM edge"),
            0,
            "the link committed"
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM cat_sediment"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM temp.cat_capture"), 0);
    }
}

#[test]
fn the_next_commit_after_a_refused_one_holds_only_its_own_changes() {
    // Arrange: a class that archives node a once, on its first commit only
    let (store, [(a, ra), (b, rb)]) = store_with_two_nodes();
    let once = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = Arc::clone(&once);
    let seen = Scripted::install(
        store.catalog(),
        Box::new(move |txn, _| {
            if flag.swap(false, Ordering::SeqCst) {
                let _ = txn.conn_for_test().execute(
                    "UPDATE node SET lifecycle = 'archived' WHERE rid = ?1",
                    [ra],
                );
            }
            Ok(())
        }),
    );

    // Act: the refused link, then the same link again
    let refused = link(&store, &a, &b).unwrap_err();
    link(&store, &a, &b).unwrap();

    // Assert
    assert_refused(&refused, "a canonical write while staging");
    let sets = seen.lock().unwrap().clone();
    let last = sets.last().unwrap();
    assert!(
        last.nodes().is_empty(),
        "the stray archive leaked into the next commit: {:?}",
        last.nodes()
    );
    assert_eq!(edge_ops(last), [(CaptureOp::Insert, ra, rb)]);
    assert_eq!(lifecycle(&store.lock(), ra), "active");
}

#[test]
fn a_derived_row_hook_that_writes_a_canonical_row_is_refused_and_rolled_back() {
    for cached in [false, true] {
        // Arrange: a hook that archives node a (a canonical row, not a derived one)
        let (store, [(a, ra), (b, _)]) = store_with_two_nodes();
        let sql = "UPDATE node SET lifecycle = 'archived' WHERE rid = ?1";
        if cached {
            store.lock().prepare_cached(sql).map(drop).unwrap();
        }
        store
            .catalog()
            .set_test_derived_rows(Box::new(move |derived, _| {
                let conn = derived.conn();
                let _ = if cached {
                    conn.prepare_cached(sql).and_then(|mut s| s.execute([ra]))
                } else {
                    conn.execute(sql, [ra])
                };
                Ok(())
            }));

        // Act
        let refused = link(&store, &a, &b).unwrap_err();

        // Assert
        assert_refused(&refused, "a canonical write in the derived-row hook");
        let conn = store.lock();
        assert_eq!(lifecycle(&conn, ra), "active", "cached {cached}");
        assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM temp.cat_capture"), 0);
    }
}

/// The sediment rows whose generation is not a committed generation, or not the latest.
fn stray_sediment(conn: &Connection) -> i64 {
    count(
        conn,
        "SELECT count(*) FROM cat_sediment s
         WHERE NOT EXISTS (SELECT 1 FROM cat_generation g WHERE g.generation = s.generation)",
    )
}

#[test]
fn staging_cannot_outlive_its_generation_through_a_savepoint() {
    for (end, cached) in [
        ("ROLLBACK TO s", false),
        ("RELEASE s", false),
        ("ROLLBACK TO s", true),
    ] {
        // Arrange: a class that stages, rolls the staging back under a savepoint, stages again
        let (store, [(a, _), (b, _)]) = store_with_two_nodes();
        let statements = ["SAVEPOINT s", "ROLLBACK TO s", "RELEASE s"];
        if cached {
            let conn = store.lock();
            for sql in statements {
                conn.prepare_cached(sql).map(drop).unwrap();
            }
        }
        let run = move |conn: &Connection, sql: &str| {
            if cached {
                conn.prepare_cached(sql)
                    .and_then(|mut s| s.execute([]))
                    .map(drop)
            } else {
                conn.execute_batch(sql)
            }
        };
        Scripted::install(
            store.catalog(),
            Box::new(move |txn, _| {
                let _ = run(txn.conn_for_test(), "SAVEPOINT s");
                txn.put_sediment(ExtentClass::Test, b"first")?;
                let _ = run(txn.conn_for_test(), end);
                if end.starts_with("ROLLBACK") {
                    let _ = run(txn.conn_for_test(), "RELEASE s");
                }
                txn.put_sediment(ExtentClass::Test, b"second")?;
                Ok(())
            }),
        );

        // Act
        let out = link(&store, &a, &b);

        // Assert: refused, or every committed sediment row names the pinned generation
        let conn = store.lock();
        assert_eq!(
            stray_sediment(&conn),
            0,
            "{end} (cached {cached}): orphan sediment"
        );
        if let Err(e) = out {
            assert_refused(&e, end);
            assert_eq!(count(&conn, "SELECT count(*) FROM cat_sediment"), 0);
            assert_eq!(count(&conn, "SELECT count(*) FROM cat_generation"), 0);
        } else {
            let latest = count(&conn, "SELECT max(generation) FROM cat_generation");
            let off = count(
                &conn,
                &format!("SELECT count(*) FROM cat_sediment WHERE generation <> {latest}"),
            );
            assert_eq!(off, 0, "{end}: sediment beside the pinned generation");
        }
    }
}

#[test]
fn a_later_failing_class_rolls_back_an_earlier_class_s_staging_and_its_generation() {
    // Arrange: the first class stages, the second fails
    let (store, [(a, _), (b, _)]) = store_with_two_nodes();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = Arc::clone(&fail);
    Scripted::install(
        store.catalog(),
        Box::new(|txn, _| txn.put_sediment(ExtentClass::Test, b"staged").map(drop)),
    );
    Scripted::install(
        store.catalog(),
        Box::new(move |_, _| {
            if flag.load(Ordering::SeqCst) {
                return Err(StoreError::Builtin("a failing class".into()));
            }
            Ok(())
        }),
    );

    // Act: the failing commit, then a successful one
    assert!(link(&store, &a, &b).is_err());
    {
        let conn = store.lock();
        assert_eq!(count(&conn, "SELECT count(*) FROM cat_sediment"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM cat_generation"), 0);
    }
    fail.store(false, Ordering::SeqCst);
    link(&store, &a, &b).unwrap();

    // Assert: the successful commit publishes generation 1 with its one row
    let conn = store.lock();
    assert_eq!(
        count(&conn, "SELECT max(generation) FROM cat_generation"),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM cat_sediment WHERE generation = 1"
        ),
        1
    );
}

#[test]
fn a_panicking_writer_leaves_no_row_no_capture_and_the_original_durability() {
    // Arrange: an ordinary safety level the panicking commit raises to FULL
    let (store, [(a, ra), (b, _)]) = store_with_two_nodes();
    store
        .lock()
        .execute_batch("PRAGMA synchronous = NORMAL")
        .unwrap();
    let level = |store: &SqliteStore| -> i64 {
        store
            .lock()
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap()
    };
    let before = level(&store);
    assert_ne!(before, 2, "the arrangement needs a level below FULL");

    // Act: a Full commit whose writer archives a node and then panics
    let unwound = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut conn = store.lock();
        let _ = store
            .catalog()
            .commit(&mut conn, Durability::Full, |txn| -> Result<()> {
                txn.conn()
                    .execute(
                        "UPDATE node SET lifecycle = 'archived' WHERE rid = ?1",
                        [ra],
                    )
                    .unwrap();
                panic!("an injected writer panic");
            });
    }));

    // Assert: rolled back, the queue empty, the level restored, the next commit succeeds
    assert!(unwound.is_err());
    {
        let conn = store.lock();
        assert_eq!(lifecycle(&conn, ra), "active");
        assert_eq!(count(&conn, "SELECT count(*) FROM temp.cat_capture"), 0);
        assert!(conn.is_autocommit(), "the transaction outlived the panic");
    }
    assert_eq!(
        level(&store),
        before,
        "the panic left synchronous=FULL behind"
    );
    link(&store, &a, &b).unwrap();
}

#[test]
fn a_panic_inside_an_outside_commit_withdraws_its_admission() {
    // Arrange
    let store = SqliteStore::open_memory().unwrap();
    let catalog = store.catalog();

    // Act
    let unwound = std::panic::catch_unwind(AssertUnwindSafe(|| {
        catalog.outside_commit(|| panic!("an injected maintenance panic"));
    }));

    // Assert: the writer guard no longer admits commits outside the catalog
    assert!(unwound.is_err());
    assert!(
        !catalog.committing.load(Ordering::Acquire),
        "the admission flag outlived the panic"
    );
}

/// Commit `rows` sediment rows of the test class at generation 1, as a writer would.
fn sediment_rows(store: &SqliteStore, rows: usize) {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let conn = txn.conn();
            let backend = |e| StoreError::Backend {
                source: kaban_core::BackendSource::new(e),
            };
            conn.execute("INSERT INTO cat_generation(generation) VALUES (1)", [])
                .map_err(backend)?;
            for i in 0..rows {
                conn.execute(
                    "INSERT INTO cat_sediment(class, generation, payload) VALUES ('test', 1, ?1)",
                    [format!("row {i}").into_bytes()],
                )
                .map_err(backend)?;
            }
            Ok(())
        })
        .unwrap();
}

/// The budget limit `err` reports, if it is a budget error.
fn budget_limit(err: &StoreError) -> Option<kaban_core::query_budget::Limit> {
    let StoreError::Backend { source } = err else {
        return None;
    };
    source
        .driver()
        .downcast_ref::<kaban_core::query_budget::BudgetExceeded>()
        .map(|e| e.limit)
}

#[test]
fn a_pin_cancelled_while_copying_returns_the_budget_error_and_releases_everything() {
    use kaban_core::query_budget::{Limit, QueryBudget};

    // Arrange: enough rows for several check intervals; the budget is cancelled at row 10
    let store = SqliteStore::open_memory().unwrap();
    sediment_rows(&store, 3000);
    let budget = QueryBudget::new(u64::MAX, None);
    let cancel = budget.clone();
    store.catalog().set_test_copy_hook(Box::new(move |n| {
        if n == 10 {
            cancel.cancel();
        }
    }));

    // Act
    let out = store.catalog().pin(&mut store.lock(), &budget);

    // Assert
    let err = out.expect_err("a cancelled copy produced a pin");
    assert_eq!(budget_limit(&err), Some(Limit::Cancelled), "{err:?}");
    assert_eq!(budget.live_bytes(), 0, "the copy's reservation outlived it");
    assert!(
        store.lock().is_autocommit(),
        "the pin's transaction outlived it"
    );
}

#[test]
fn a_snapshot_whose_callback_cancels_the_budget_returns_the_budget_error() {
    use kaban_core::query_budget::{Limit, QueryBudget};

    // Arrange
    let store = SqliteStore::open_memory().unwrap();
    sediment_rows(&store, 3);
    let budget = QueryBudget::new(u64::MAX, None);

    // Act: the callback cancels and still returns Ok
    let out = store
        .catalog()
        .with_snapshot(&mut store.lock(), &budget, |_, _| {
            budget.cancel();
            Ok(7)
        });

    // Assert
    let err = out.expect_err("the cancelled snapshot reported success");
    assert_eq!(budget_limit(&err), Some(Limit::Cancelled), "{err:?}");
    assert_eq!(budget.live_bytes(), 0);
    assert!(store.lock().is_autocommit());
}

#[test]
fn a_long_statement_in_a_cancelled_snapshot_is_interrupted() {
    use kaban_core::query_budget::{Limit, QueryBudget};

    // Arrange: a statement that runs for seconds and returns one row
    let store = SqliteStore::open_memory().unwrap();
    let budget = QueryBudget::new(u64::MAX, None);
    let started = std::time::Instant::now();

    // Act: cancel, then run it inside the snapshot
    let out = store
        .catalog()
        .with_snapshot(&mut store.lock(), &budget, |tx, _| {
            budget.cancel();
            tx.query_row(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 20000000)
                 SELECT count(*) FROM c",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| StoreError::Backend {
                source: kaban_core::BackendSource::new(e),
            })
        });

    // Assert: the budget error, well before the statement could finish
    let elapsed = started.elapsed();
    let err = out.expect_err("the cancelled statement ran to completion");
    assert_eq!(budget_limit(&err), Some(Limit::Cancelled), "{err:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "the statement was not interrupted: {elapsed:?}"
    );
}

/// A file-backed store in a fresh scratch directory (the pin clones its container path).
struct FileStore {
    dir: std::path::PathBuf,
    store: SqliteStore,
}

impl FileStore {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("kaban-t5d-unit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap();
        Self { dir, store }
    }
}

impl Drop for FileStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Commit `rows` sediment rows of each of `classes` with payloads of `len` bytes, and
/// `files` cold extents of `class`, all at generation 1.
fn catalog_rows(store: &SqliteStore, classes: &[&str], rows: usize, len: usize, files: usize) {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let conn = txn.conn();
            let backend = |e| StoreError::Backend {
                source: kaban_core::BackendSource::new(e),
            };
            conn.execute("INSERT INTO cat_generation(generation) VALUES (1)", [])
                .map_err(backend)?;
            for class in classes {
                for _ in 0..rows {
                    conn.execute(
                        "INSERT INTO cat_sediment(class, generation, payload) VALUES (?1, 1, ?2)",
                        rusqlite::params![class, vec![7_u8; len]],
                    )
                    .map_err(backend)?;
                }
                for i in 0..files {
                    conn.execute(
                        "INSERT INTO cat_extent(class, generation, relpath) VALUES (?1, 1, ?2)",
                        rusqlite::params![class, format!("cold/{class}-{i}.seg")],
                    )
                    .map_err(backend)?;
                }
            }
            Ok(())
        })
        .unwrap();
}

/// What the pin retains, charged by the budget's own rule for one allocation.
fn retained_charge(pin: &Pin) -> u64 {
    pin.retained_allocations()
        .into_iter()
        .map(|(bytes, align)| {
            kaban_core::query_budget::QueryBudget::allocation_charge(bytes as u64, align as u64)
                .unwrap()
        })
        .sum()
}

#[test]
fn a_pin_reserves_every_allocation_it_retains_before_it_copies() {
    use kaban_core::query_budget::{Limit, QueryBudget};

    let cases: Vec<(&str, Vec<&str>, usize, usize, usize)> = vec![
        ("empty", vec![], 0, 0, 0),
        ("one small row", vec!["test"], 1, 1, 0),
        (
            "several classes",
            vec!["test", "text_tier", "graph_base"],
            3,
            2,
            1,
        ),
        ("growth 4", vec!["test"], 4, 1, 0),
        ("growth 5", vec!["test"], 5, 1, 0),
        ("growth 9", vec!["code_base", "test"], 9, 0, 5),
        ("growth 17", vec!["test"], 17, 3, 0),
    ];
    for (name, classes, rows, len, files) in cases {
        // Arrange
        let fs = FileStore::new(&name.replace(' ', "-"));
        let store = &fs.store;
        catalog_rows(store, &classes, rows, len, files);
        let copied = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&copied);
        store.catalog().set_test_copy_hook(Box::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        }));

        // Act: an unbounded pin tells the charge, held while it is measured
        let open = QueryBudget::new(u64::MAX, None);
        let pin = store.catalog().pin(&mut store.lock(), &open).unwrap();
        let charge = open.live_bytes();

        // Assert: every retained allocation is paid for, and the charge returns on drop
        assert!(
            charge >= retained_charge(&pin),
            "{name}: charged {charge}, retains {}",
            retained_charge(&pin)
        );
        drop(pin);
        assert_eq!(open.live_bytes(), 0, "{name}: the charge outlived the pin");

        // Act: exactly the charge admits, one byte less refuses before any row is copied
        let at = QueryBudget::new(charge, None);
        let pinned = store.catalog().pin(&mut store.lock(), &at);
        assert!(pinned.is_ok(), "{name}: refused at its own charge {charge}");
        drop(pinned);
        assert_eq!(at.live_bytes(), 0);
        if charge > 0 {
            copied.store(0, Ordering::SeqCst);
            let below = QueryBudget::new(charge - 1, None);
            let err = store.catalog().pin(&mut store.lock(), &below).unwrap_err();
            assert_eq!(budget_limit(&err), Some(Limit::Memory), "{name}: {err:?}");
            assert_eq!(
                copied.load(Ordering::SeqCst),
                0,
                "{name}: copied before refusing"
            );
            assert_eq!(below.live_bytes(), 0, "{name}: a refusal kept a charge");
        } else {
            panic!("{name}: a file-backed pin retains its container path, yet charged nothing");
        }
        assert!(store.lock().is_autocommit());
    }
}

#[test]
fn an_extent_keeps_a_stable_id_that_is_never_reused() {
    // Arrange: two cold extents, the second deleted, then a third
    let store = SqliteStore::open_memory().unwrap();
    let ids = |store: &SqliteStore| -> Vec<i64> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM cat_extent ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect()
    };
    let write = |sql: &'static str| {
        store
            .catalog()
            .commit(&mut store.lock(), Durability::Normal, |txn| {
                txn.conn()
                    .execute_batch(sql)
                    .map_err(|e| StoreError::Backend {
                        source: kaban_core::BackendSource::new(e),
                    })
            })
            .unwrap();
    };
    write(
        "INSERT INTO cat_generation(generation) VALUES (1);
         INSERT INTO cat_extent(class, generation, relpath) VALUES ('test', 1, 'a.seg');
         INSERT INTO cat_extent(class, generation, relpath) VALUES ('test', 1, 'b.seg');",
    );
    write("DELETE FROM cat_extent WHERE relpath = 'b.seg'");

    // Act
    write("INSERT INTO cat_extent(class, generation, relpath) VALUES ('test', 1, 'c.seg')");

    // Assert: the third extent does not take the deleted one's id, and a pin carries the ids
    assert_eq!(ids(&store), [1, 3], "a deleted extent's id was reused");
    let budget = kaban_core::query_budget::QueryBudget::new(u64::MAX, None);
    let pin = store.catalog().pin(&mut store.lock(), &budget).unwrap();
    let pinned: Vec<u64> = pin
        .extents(ExtentClass::Test)
        .iter()
        .map(|e| e.id.0)
        .collect();
    assert_eq!(pinned, [1, 3]);
}

#[test]
fn a_dead_extent_is_not_pinned() {
    // Arrange: a live and a dead extent
    let store = SqliteStore::open_memory().unwrap();
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.conn()
                .execute_batch(
                    "INSERT INTO cat_generation(generation) VALUES (1), (2);
                     INSERT INTO cat_extent(class, generation, relpath) VALUES ('test', 1, 'live.seg');
                     INSERT INTO cat_extent(class, generation, dead_since, relpath)
                       VALUES ('test', 1, 2, 'dead.seg');",
                )
                .map_err(|e| StoreError::Backend {
                    source: kaban_core::BackendSource::new(e),
                })
        })
        .unwrap();

    // Act
    let budget = kaban_core::query_budget::QueryBudget::new(u64::MAX, None);
    let pin = store.catalog().pin(&mut store.lock(), &budget).unwrap();

    // Assert
    let paths: Vec<_> = pin
        .extents(ExtentClass::Test)
        .iter()
        .map(|e| e.location.clone())
        .collect();
    assert_eq!(
        paths,
        [super::class::ExtentLocation::File {
            relpath: "live.seg".into()
        }]
    );
}

#[test]
fn a_span_beyond_the_container_is_refused_and_a_span_inside_it_reads() {
    use super::kbx::ContainerError;

    // Arrange: a 16-byte container, its one extent published, and two fabricated ranges past
    // its end (what a corrupt catalog row would name)
    let fs = FileStore::new("span-bounds");
    let store = &fs.store;
    let inside = store
        .catalog()
        .append_extent(ExtentClass::Test, &[1_u8; 16])
        .unwrap();
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(inside)?;
            txn.conn()
                .execute_batch(
                    "INSERT INTO cat_extent(class, generation, kbx_offset, kbx_len)
                       VALUES ('test', 1, 8, 9), ('test', 1, 9223372036854775807, 1);",
                )
                .map_err(|e| StoreError::Backend {
                    source: kaban_core::BackendSource::new(e),
                })
        })
        .unwrap();

    // Act
    let budget = kaban_core::query_budget::QueryBudget::new(u64::MAX, None);
    let pin = store.catalog().pin(&mut store.lock(), &budget).unwrap();
    let extents = pin.extents(ExtentClass::Test);

    // Assert: the range inside reads; one byte past the end and an offset past it refuse
    assert_eq!(extents.len(), 3);
    assert_eq!(pin.bytes(&extents[0]).unwrap().as_slice(), &[1_u8; 16]);
    for e in &extents[1..] {
        let err = pin
            .bytes(e)
            .expect_err("a span beyond the container was read");
        let StoreError::Backend { source } = &err else {
            panic!("{e:?}: {err:?}");
        };
        let refused = source
            .driver()
            .downcast_ref::<ContainerError>()
            .unwrap_or_else(|| panic!("{e:?}: {err:?}"));
        assert_eq!(
            refused.source.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "{e:?}: {refused}"
        );
    }
}

#[test]
fn a_pin_on_a_connection_with_an_open_transaction_is_refused_by_name() {
    use super::pin::OuterTransaction;
    use kaban_core::query_budget::QueryBudget;

    // Arrange: the connection is inside a transaction the catalog did not open
    let store = SqliteStore::open_memory().unwrap();
    let budget = QueryBudget::new(u64::MAX, None);
    let mut conn = store.lock();
    conn.execute_batch("SAVEPOINT outer").unwrap();

    // Act
    let pinned = store.catalog().pin(&mut conn, &budget).map(|_| ());
    let snapshot = store
        .catalog()
        .with_snapshot(&mut conn, &budget, |_, _| Ok(()));

    // Assert: both refuse with the named error, and the outer transaction is left as it was
    for out in [pinned, snapshot] {
        let err = out.expect_err("a pin ran inside an outer transaction");
        let StoreError::Backend { source } = &err else {
            panic!("{err:?}");
        };
        assert!(
            source.driver().downcast_ref::<OuterTransaction>().is_some(),
            "{err:?}"
        );
    }
    assert!(!conn.is_autocommit());
    conn.execute_batch("ROLLBACK").unwrap();
}
