//! The commit entry's phase contract (spec 11 T5 midpoint review MAJ-01, MED-01, LOW-01): after
//! the capture freeze no canonical row may change, the staging phase writes sediment only and
//! cannot outlive its own generation, and an unwound writer leaves the connection as it found
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::{
    Actor, Durability, MemoryStore, NewNode, NodeId, RememberOutcome, Result, StoreError,
    WorkspaceId, WriteCtx,
};
use rusqlite::Connection;

use super::capture::{BatchChanges, CaptureOp};
use super::class::{ExtentClass, IndexClass, SealOutput};
use super::commit::Stage;
use super::pin::Pin;
use super::Catalog;
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
    store.link(
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
        source.driver().to_string().starts_with("catalog_commit_refused"),
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
        assert_eq!(lifecycle(&conn, ra), "active", "cached {cached}: the archive committed");
        assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 0, "the link committed");
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
                let _ = txn
                    .conn_for_test()
                    .execute("UPDATE node SET lifecycle = 'archived' WHERE rid = ?1", [ra]);
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
                conn.prepare_cached(sql).and_then(|mut s| s.execute([])).map(drop)
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
        assert_eq!(stray_sediment(&conn), 0, "{end} (cached {cached}): orphan sediment");
        match out {
            Err(e) => {
                assert_refused(&e, end);
                assert_eq!(count(&conn, "SELECT count(*) FROM cat_sediment"), 0);
                assert_eq!(count(&conn, "SELECT count(*) FROM cat_generation"), 0);
            }
            Ok(()) => {
                let latest = count(&conn, "SELECT max(generation) FROM cat_generation");
                let off = count(
                    &conn,
                    &format!("SELECT count(*) FROM cat_sediment WHERE generation <> {latest}"),
                );
                assert_eq!(off, 0, "{end}: sediment beside the pinned generation");
            }
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
    assert_eq!(count(&conn, "SELECT max(generation) FROM cat_generation"), 1);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM cat_sediment WHERE generation = 1"),
        1
    );
}
