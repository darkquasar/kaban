//! The commit entry's phase contract (spec 11 T5 midpoint review MAJ-01, MED-01, LOW-01): after
//! the capture freeze no canonical row may change, the staging phase writes sediment only and
//! cannot outlive its own generation, and an unwound writer leaves the connection as it found
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::query_budget::QueryBudget;
use kaban_core::{
    Actor, Durability, MemoryStore, NewNode, NodeId, RememberOutcome, Result, StoreError, ViewName,
    WorkspaceId, WriteCtx,
};
use rusqlite::Connection;

use super::Catalog;
use super::capture::{BatchChanges, CaptureOp};
use super::class::{ExtentClass, IndexClass};
use super::commit::Stage;
use super::pin::{Pin, PinnedView};
use crate::SqliteStore;

fn ctx() -> WriteCtx {
    WriteCtx::new(WorkspaceId::default(), Actor::Agent)
}

#[test]
fn phase_close_refuses_a_rollup_contribution_after_the_freeze() {
    let store = SqliteStore::open_memory().unwrap();
    // Present the phase-close check with a queued contribution. It must detect it
    // independently of the authorizer that normally prevents the late write.
    for phase in [super::phase::Phase::Derived, super::phase::Phase::Stage] {
        let result = store.catalog().commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.conn().execute("INSERT INTO main.rollup_input(name,key,delta,real_sum) VALUES ('late',NULL,0,0)", []).unwrap();
            let phases = super::phase::Phases::default();
            let guard = phases.enter();
            guard.set(phase);
            guard.close(txn.conn(), Ok(()))
        });
        assert!(
            result.is_err(),
            "a contribution after freeze must refuse {phase:?}"
        );
        assert_eq!(count(&store.lock(), "SELECT count(*) FROM rollup_input"), 0);
    }
}

#[test]
fn the_scoped_body_reader_returns_the_hot_bytes_from_the_current_commit() {
    let (store, [(_, rid), _]) = store_with_two_nodes();
    let body = "A harbour ünicode body. ".repeat(256);
    let expected = body.clone();
    store
        .catalog()
        .set_test_derived_rows(Box::new(move |derived, _| {
            assert_eq!(
                crate::body_codec::read_body(&derived.reader(), rid)?,
                expected.as_bytes()
            );
            assert!(matches!(
                crate::body_codec::read_body(&derived.reader(), -1),
                Err(StoreError::NotFound(_))
            ));
            Ok(())
        }));
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let bytes = crate::body_codec::Writer::for_connection(txn.conn())?.stored(&body);
            txn.conn()
                .execute(
                    "UPDATE node SET text=? WHERE rid=?",
                    rusqlite::params![bytes, rid],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
}

#[test]
fn the_scoped_body_reader_distinguishes_cold_references_from_hot_and_corrupt_values() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE node(rid INTEGER PRIMARY KEY, text)", [])
        .unwrap();
    let phases = super::phase::Phases::default();
    let reader = super::phase::Reader::new(&conn, &phases);
    let read = |stored: &dyn rusqlite::ToSql| {
        conn.execute("INSERT OR REPLACE INTO node VALUES (1, ?)", [stored])
            .unwrap();
        crate::body_codec::read_body(&reader, 1)
    };
    let mut reference = vec![0, b'K', b'Z', 2, 0];
    reference.extend_from_slice(&42_i64.to_le_bytes());
    reference.extend_from_slice(&0_u32.to_le_bytes());
    assert!(matches!(read(&reference), Err(StoreError::ColdTierAbsent)));

    let compressed = "a body that benefits from compression ".repeat(50);
    for (body, format) in [("x", 0), (compressed.as_str(), 1)] {
        let blob = crate::body_codec::encode(body.as_bytes());
        assert_eq!(blob[3], format);
        assert_eq!(read(&blob).unwrap(), body.as_bytes());
    }
    let legacy = std::str::from_utf8(&reference).unwrap();
    assert_eq!(
        read(&legacy).unwrap(),
        reference,
        "TEXT is always plaintext"
    );

    let mut bad_header = reference.clone();
    bad_header[4] = 1;
    let mut unknown = reference.clone();
    unknown[3] = 9;
    let mut invalid_extent = reference.clone();
    invalid_extent[5..13].fill(0);
    let mut trailing = reference.clone();
    trailing.push(0);
    let mut bad_hot = crate::body_codec::encode(b"x");
    bad_hot.pop();
    for (blob, reason) in [
        (reference[..4].to_vec(), "corrupt body payload"),
        (
            reference[..reference.len() - 1].to_vec(),
            "corrupt body payload",
        ),
        (bad_header, "corrupt body payload"),
        (unknown, "unknown body format 9"),
        (invalid_extent, "corrupt body payload"),
        (trailing, "corrupt body payload"),
        (bad_hot, "corrupt body payload"),
    ] {
        let Err(StoreError::Backend { source }) = read(&blob) else {
            panic!("expected a conversion failure for {blob:?}");
        };
        assert!(source.driver().to_string().contains(reason), "{blob:?}");
    }
}

#[test]
fn get_and_sql_recognize_the_same_stored_cold_body() {
    let fixture = FileStore::new("cold-body");
    let store = &fixture.store;
    let RememberOutcome::Created(node) = store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "hot body".into(),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    else {
        panic!("fresh fact");
    };
    let id = NodeId::parse(&node.id).unwrap();
    let rid = rid(&store.lock(), &node.id);
    // Install the on-disk fixture without adding a producer for the deferred cold tier.
    let disk = Connection::open(fixture.dir.join("kaban.db")).unwrap();
    crate::stamp::bind(&disk, &kaban_core::ActorId::default()).unwrap();
    let mut reference = vec![0, b'K', b'Z', 2, 0];
    reference.extend_from_slice(&8_i64.to_le_bytes());
    reference.extend_from_slice(&3_u32.to_le_bytes());
    let put = |value: &[u8]| {
        disk.execute(
            "UPDATE node SET text=? WHERE rid=?",
            rusqlite::params![value, rid],
        )
        .unwrap();
    };
    let sql_body = || {
        store.lock().query_row(
            "SELECT kaban_body(text) FROM node WHERE rid=?",
            [rid],
            |row| row.get::<_, String>(0),
        )
    };
    put(&reference);
    assert!(matches!(
        store.get(&id, false),
        Err(StoreError::ColdTierAbsent)
    ));
    assert!(
        sql_body()
            .unwrap_err()
            .to_string()
            .contains("cold_tier_absent")
    );

    reference.pop();
    put(&reference);
    assert!(matches!(
        store.get(&id, false),
        Err(StoreError::Backend { .. })
    ));
    assert!(
        sql_body()
            .unwrap_err()
            .to_string()
            .contains("corrupt body payload")
    );

    put(&crate::body_codec::encode(b"x"));
    assert_eq!(store.get(&id, false).unwrap().unwrap().text, "x");
    assert_eq!(sql_body().unwrap(), "x");
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

/// The latest generation (0 before the first): what a test compares against, since the
/// registered classes' own commits publish generations too.
const GENERATION: &str = "SELECT COALESCE(max(generation), 0) FROM cat_generation";

/// [`GENERATION`] of `store`.
fn latest_generation(store: &SqliteStore) -> i64 {
    count(&store.lock(), GENERATION)
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

/// Every class, as a pin loads the sediment of all of them.
const EVERY: &[ExtentClass] = &[
    ExtentClass::TextTier,
    ExtentClass::GraphBase,
    ExtentClass::GraphLayer,
    ExtentClass::CodeLayer,
    ExtentClass::CodeBase,
    ExtentClass::Test,
];

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
    fn classes(&self) -> &[ExtentClass] {
        &[ExtentClass::Test]
    }
    fn stage(&self, txn: &mut Stage<'_>, changes: &BatchChanges) -> Result<()> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(changes.clone());
        (self.script)(txn, changes)
    }
    fn seal_due(&self, _rows: &[&super::tables::SedimentRow], _ops: u32) -> Result<bool> {
        Ok(false)
    }
    fn seal(
        &self,
        _pin: &Pin,
        _rows: &[&super::tables::SedimentRow],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn fold(
        &self,
        _pin: &Pin,
        _extents: &[super::tables::ExtentRef],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn purge(
        &self,
        _pin: &Pin,
        _extent: &super::tables::ExtentRef,
        _rids: &[i64],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn rebuild(&self, _catalog: &Catalog, _rows: &mut Connection) -> Result<()> {
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
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM cat_sediment WHERE class = 'test'"
            ),
            0
        );
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
                let conn = derived.conn_for_test();
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

        let before = latest_generation(&store);

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
            assert_eq!(
                count(
                    &conn,
                    "SELECT count(*) FROM cat_sediment WHERE class = 'test'"
                ),
                0
            );
            assert_eq!(count(&conn, GENERATION), before);
        } else {
            let latest = count(&conn, GENERATION);
            let off = count(
                &conn,
                &format!(
                    "SELECT count(*) FROM cat_sediment WHERE class = 'test' AND generation <> {latest}"
                ),
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

    let before = latest_generation(&store);

    // Act: the failing commit, then a successful one
    assert!(link(&store, &a, &b).is_err());
    {
        let conn = store.lock();
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM cat_sediment WHERE class = 'test'"
            ),
            0
        );
        assert_eq!(count(&conn, GENERATION), before);
    }
    fail.store(false, Ordering::SeqCst);
    link(&store, &a, &b).unwrap();

    // Assert: the successful commit publishes the next generation with its one row
    let conn = store.lock();
    assert_eq!(count(&conn, GENERATION), before + 1);
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM cat_sediment WHERE class = 'test' AND generation = {}",
                before + 1
            )
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

/// A changed edge's values a class reads while staging (T11's `Attr::weight` and `Attr::ts`).
const EDGE_VALUES: &str =
    "SELECT confidence, created_at FROM edge WHERE src = ?1 AND dst = ?2 AND rel = ?3";
/// A changed node's values a class reads while staging (T7's text segment).
const NODE_VALUES: &str = "SELECT summary, text FROM node WHERE rid = ?1";

/// What a reading class or hook saw of one commit, read in that commit's transaction: each
/// changed edge's `(confidence, created_at)` and each changed node's `(summary, body)`.
#[derive(Debug, Default, Clone, PartialEq)]
struct Seen {
    edges: Vec<(Option<f64>, String)>,
    nodes: Vec<(Option<String>, String)>,
}

/// Read the values of every change in `changes` through `reader`.
fn read_changes(reader: super::phase::Reader<'_>, changes: &BatchChanges) -> Result<Seen> {
    let mut edge = reader.prepare(EDGE_VALUES)?;
    let mut node = reader.prepare(NODE_VALUES)?;
    let mut seen = Seen::default();
    for e in changes.edges() {
        let values = edge.query_row(rusqlite::params![e.src, e.dst, e.rel], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        seen.edges.extend(values);
    }
    for n in changes.nodes() {
        let values = node.query_row([n.rid], |r| {
            Ok((r.get(0)?, crate::body_codec::read_column(r, 1)?))
        })?;
        seen.nodes.extend(values);
    }
    Ok(seen)
}

#[test]
fn a_class_and_the_derived_row_hook_read_the_rows_of_their_own_commit() {
    // Arrange: a class and a hook that read every changed row; an edge committed at 0.25
    let (store, [(a, ra), (b, rb)]) = store_with_two_nodes();
    let staged = Arc::new(Mutex::new(Vec::new()));
    let hooked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&staged);
    Scripted::install(
        store.catalog(),
        Box::new(move |txn, changes| {
            let seen = read_changes(txn.reader(), changes)?;
            // a class reads and stages in any order
            txn.put_sediment(ExtentClass::Test, b"read")?;
            log.lock().unwrap().push(seen);
            Ok(())
        }),
    );
    let log = Arc::clone(&hooked);
    store
        .catalog()
        .set_test_derived_rows(Box::new(move |derived, changes| {
            log.lock()
                .unwrap()
                .push(read_changes(derived.reader(), changes)?);
            Ok(())
        }));
    store
        .link(
            &NodeId::parse(&a).unwrap(),
            &NodeId::parse(&b).unwrap(),
            "RELATES_TO",
            Some(0.25),
            None,
            &ctx(),
        )
        .unwrap();
    let first_at: String = store
        .lock()
        .query_row("SELECT created_at FROM edge", [], |r| r.get(0))
        .unwrap();
    let later = "2031-02-03T04:05:06Z";

    // Act: one commit changes the edge's confidence and time and node a's summary and body
    let body = crate::body_codec::encode(b"the tide turns at seven");
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let conn = txn.conn();
            conn.execute(
                "UPDATE edge SET confidence = 0.75, created_at = ?1 WHERE src = ?2 AND dst = ?3",
                rusqlite::params![later, ra, rb],
            )
            .unwrap();
            conn.execute(
                "UPDATE node SET summary = 'at seven', text = ?1 WHERE rid = ?2",
                rusqlite::params![body, ra],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();

    // Assert: each commit's class and hook read that commit's values, never the previous ones
    let first = Seen {
        edges: vec![(Some(0.25), first_at)],
        nodes: Vec::new(),
    };
    let second = Seen {
        edges: vec![(Some(0.75), later.to_owned())],
        nodes: vec![(
            Some("at seven".to_owned()),
            "the tide turns at seven".to_owned(),
        )],
    };
    assert_eq!(
        *staged.lock().unwrap(),
        [first.clone(), second.clone()],
        "staged"
    );
    assert_eq!(*hooked.lock().unwrap(), [first, second], "hooked");
}

/// The statement a test sends through a reader or a restricted handle, by table: a canonical
/// row (`node`, captured), an uncaptured catalog row (`cat_extent`), a staging row.
fn writes(rid: i64, extent: u64) -> [(&'static str, String); 3] {
    [
        (
            "node",
            format!("UPDATE node SET lifecycle = 'archived' WHERE rid = {rid}"),
        ),
        (
            "cat_extent",
            format!("DELETE FROM cat_extent WHERE id = {extent}"),
        ),
        (
            "cat_sediment",
            "INSERT INTO cat_sediment(class, generation, payload) VALUES ('test', 1, x'00')".into(),
        ),
    ]
}

/// Publish one cold-file extent of the test class (a row the writer writes: no producer
/// appends cold files yet); its id.
fn published_extent(store: &SqliteStore) -> u64 {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let generation = i64::try_from(txn.generation()?.0).unwrap();
            txn.conn()
                .query_row(
                    "INSERT INTO cat_extent(class, generation, relpath)
                     VALUES ('test', ?1, 'cold/one') RETURNING id",
                    [generation],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(|e| StoreError::Backend {
                    source: kaban_core::BackendSource::new(e),
                })
        })
        .map(|id| u64::try_from(id).unwrap())
        .unwrap()
}

/// Nothing of a refused commit stayed: node `rid` active, extent `extent` published, no edge
/// and no sediment.
fn assert_rolled_back(store: &SqliteStore, rid: i64, extent: u64, what: &str) {
    let conn = store.lock();
    assert_eq!(
        lifecycle(&conn, rid),
        "active",
        "{what}: the archive committed"
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*) FROM cat_extent WHERE id = {extent}")
        ),
        1,
        "{what}: the extent's row was deleted"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM edge"),
        0,
        "{what}: the link committed"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM cat_sediment WHERE class = 'test'"
        ),
        0,
        "{what}: sediment committed"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM temp.cat_capture"), 0);
}

#[test]
fn a_write_through_the_read_capability_is_refused_and_rolls_the_commit_back() {
    for hook in [false, true] {
        for at in 0..3 {
            // Arrange: a class (or the hook) that sends a write through its reader and
            // swallows the outcome; the class stages one legitimate row first
            let (store, [(a, ra), (b, _)]) = store_with_two_nodes();
            let extent = published_extent(&store);
            let (table, sql) = writes(ra, extent)[at].clone();
            let what = format!(
                "{table} through the {} reader",
                if hook { "hook's" } else { "class's" }
            );
            let class_sql = (!hook).then(|| sql.clone());
            Scripted::install(
                store.catalog(),
                Box::new(move |txn, _| {
                    txn.put_sediment(ExtentClass::Test, b"legitimate")?;
                    if let Some(sql) = &class_sql {
                        let _ = txn
                            .reader()
                            .prepare(sql)
                            .and_then(|mut s| s.query_map([], |_| Ok(())));
                    }
                    Ok(())
                }),
            );
            if hook {
                store
                    .catalog()
                    .set_test_derived_rows(Box::new(move |derived, _| {
                        let _ = derived
                            .reader()
                            .prepare(&sql)
                            .and_then(|mut s| s.query_map([], |_| Ok(())));
                        Ok(())
                    }));
            }

            // Act
            let out = link(&store, &a, &b);

            // Assert
            let Err(refused) = out else {
                panic!("{what}: the commit was not refused");
            };
            assert_refused(&refused, &what);
            assert_rolled_back(&store, ra, extent, &what);
        }
    }
}

#[test]
fn a_statement_the_writer_phase_cached_is_refused_in_the_hook_and_in_staging() {
    for staging in [false, true] {
        for at in 0..2 {
            // Arrange: the writer phase prepares (and caches) a write of a canonical row or of
            // an uncaptured catalog row; the hook, or a class, then sends the same statement
            // through its handle and swallows the outcome (decision review C15)
            let (store, [(_, ra), _]) = store_with_two_nodes();
            let extent = published_extent(&store);
            let (table, sql) = writes(ra, extent)[at].clone();
            let what = format!(
                "{table} cached by the writer, run in the {}",
                if staging { "staging" } else { "hook" }
            );
            let class_sql = staging.then(|| sql.clone());
            Scripted::install(
                store.catalog(),
                Box::new(move |txn, _| {
                    if let Some(sql) = &class_sql {
                        let _ = txn
                            .reader()
                            .prepare(sql)
                            .and_then(|mut s| s.query_map([], |_| Ok(())));
                    }
                    Ok(())
                }),
            );
            let hook_sql = (!staging).then(|| sql.clone());
            store
                .catalog()
                .set_test_derived_rows(Box::new(move |derived, _| {
                    if let Some(sql) = &hook_sql {
                        let _ = derived.execute(sql, []);
                    }
                    Ok(())
                }));

            // Act: a commit whose writer only prepares the statement
            let out = store
                .catalog()
                .commit(&mut store.lock(), Durability::Normal, |txn| {
                    txn.conn().prepare_cached(&sql).map(drop).unwrap();
                    Ok(())
                });

            // Assert
            let Err(refused) = out else {
                panic!("{what}: the commit was not refused");
            };
            assert_refused(&refused, &what);
            assert_rolled_back(&store, ra, extent, &what);
        }
    }
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
    match err {
        StoreError::BudgetExceeded(e) => Some(e.limit),
        _ => None,
    }
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
    let out = store
        .catalog()
        .pin(&store.lock(), &budget, &[(ViewName::Main, EVERY)]);

    // Assert
    let err = out.expect_err("a cancelled copy produced a pin");
    assert_eq!(budget_limit(&err), Some(Limit::Cancelled), "{err:?}");
    // the refusal names the budget, not the redacted backend text
    assert!(
        err.to_string()
            .starts_with("query budget exceeded: Cancelled"),
        "{err}"
    );
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
        .with_snapshot(&store.lock(), &budget, |_, _| {
            budget.cancel();
            Ok(7)
        });

    // Assert
    let err = out.expect_err("the cancelled snapshot reported success");
    assert_eq!(budget_limit(&err), Some(Limit::Cancelled), "{err:?}");
    assert_eq!(budget.live_bytes(), 0);
    assert!(store.lock().is_autocommit());
}

/// A statement that runs for seconds unless interrupted, and returns one row.
const LONG_STATEMENT: &str = "WITH d(x) AS (VALUES (1), (2), (3), (4), (5), (6), (7), (8),
                                     (9), (10), (11), (12), (13), (14), (15), (16))
                 SELECT count(*) FROM d AS a, d AS b, d AS c, d AS e, d AS f, d AS g";

#[test]
fn a_long_statement_in_a_cancelled_snapshot_is_interrupted() {
    use kaban_core::query_budget::{Limit, QueryBudget};

    // Arrange: a statement that runs for seconds and returns one row, on a connection whose
    // own progress handler watches the budget (as a query connection's does)
    let store = SqliteStore::open_memory().unwrap();
    let budget = QueryBudget::new(u64::MAX, None);
    let watched = budget.clone();
    store
        .lock()
        .progress_handler(1000, Some(move || watched.check().is_err()))
        .unwrap();
    let mut statement = None;

    // Act: cancel, then run it inside the snapshot, keeping the statement's own outcome
    let out = store
        .catalog()
        .with_snapshot(&store.lock(), &budget, |tx, _| {
            budget.cancel();
            let row = tx.query_row(LONG_STATEMENT, [], |r| r.get::<_, i64>(0));
            statement = Some(
                row.as_ref()
                    .copied()
                    .map_err(rusqlite::Error::sqlite_error_code),
            );
            row.map_err(|e| StoreError::Backend {
                source: kaban_core::BackendSource::new(e),
            })
        });

    // Assert: SQLite interrupted the statement itself (not only the snapshot's final budget
    // check), and the snapshot returns the budget error
    assert_eq!(
        statement,
        Some(Err(Some(rusqlite::ErrorCode::OperationInterrupted))),
        "the statement was not interrupted"
    );
    let err = out.expect_err("the cancelled statement ran to completion");
    assert_eq!(budget_limit(&err), Some(Limit::Cancelled), "{err:?}");
}

#[test]
fn a_snapshot_leaves_the_connection_s_own_progress_handler_in_place() {
    use kaban_core::query_budget::QueryBudget;

    // Arrange: the connection's own handler (as a query connection installs one) interrupts
    // every statement once `own` is cancelled
    let store = SqliteStore::open_memory().unwrap();
    let own = QueryBudget::new(u64::MAX, None);
    let watched = own.clone();
    store
        .lock()
        .progress_handler(1000, Some(move || watched.check().is_err()))
        .unwrap();
    let budget = QueryBudget::new(u64::MAX, None);

    // Act: a snapshot, then the connection's own budget is cancelled
    store
        .catalog()
        .with_snapshot(&store.lock(), &budget, |_, _| Ok(()))
        .unwrap();
    own.cancel();
    let out = store
        .lock()
        .query_row(LONG_STATEMENT, [], |r| r.get::<_, i64>(0));

    // Assert: the connection's handler still interrupts its statements
    assert_eq!(
        out.map_err(|e| e.sqlite_error_code()),
        Err(Some(rusqlite::ErrorCode::OperationInterrupted)),
        "the snapshot removed the connection's own progress handler"
    );
}

#[test]
fn a_snapshot_opens_through_a_shared_borrow_while_another_one_is_alive() {
    use kaban_core::query_budget::QueryBudget;

    // Arrange: one committed generation; a second shared borrow of the connection, alive
    // across the snapshot (what a table function's execution context holds)
    let store = SqliteStore::open_memory().unwrap();
    sediment_rows(&store, 1);
    let conn = store.lock();
    let other: &Connection = &conn;
    let budget = QueryBudget::new(u64::MAX, None);

    // Act
    let seen = store
        .catalog()
        .with_snapshot(&conn, &budget, |_, pin| {
            Ok((pin.generation().0, other.is_autocommit()))
        })
        .unwrap();

    // Assert: the other borrow saw the snapshot's transaction open, and it ended with it
    assert_eq!(seen, (1, false));
    assert!(other.is_autocommit());
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
    chain_into_main(store);
}

/// Append every extent row that `main`'s chain does not name to it, in id order (what a
/// publication into `main` records), for rows a test inserted directly.
fn chain_into_main(store: &SqliteStore) {
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.conn()
                .execute(
                    "INSERT INTO cat_view_extent(view, position, extent_id)
                     SELECT 'main', (SELECT COALESCE(max(position), -1) FROM cat_view_extent
                                     WHERE view = 'main') + row_number() OVER (ORDER BY id), id
                     FROM cat_extent
                     WHERE id NOT IN (SELECT extent_id FROM cat_view_extent WHERE view = 'main')",
                    [],
                )
                .map_err(|e| StoreError::Backend {
                    source: kaban_core::BackendSource::new(e),
                })
        })
        .unwrap();
}

/// A detached pin of `store` with `main` loaded, and `main`'s inputs.
fn pin_main(store: &SqliteStore) -> (Pin, Arc<PinnedView>) {
    let budget = kaban_core::query_budget::QueryBudget::new(u64::MAX, None);
    let conn = store.lock();
    let pin = store
        .catalog()
        .pin(&conn, &budget, &[(ViewName::Main, EVERY)])
        .unwrap();
    let main = pin.load_view(&conn, &ViewName::Main, EVERY).unwrap();
    (pin, main)
}

/// The extents of the test class in `main`'s chain.
fn test_extents(main: &PinnedView) -> Vec<super::tables::ExtentRef> {
    main.chain()
        .iter()
        .filter(|e| e.class == ExtentClass::Test)
        .cloned()
        .collect()
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
        (
            "several classes",
            vec!["test", "text_tier", "graph_base"],
            3,
            2,
            1,
        ),
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
        let pin = store
            .catalog()
            .pin(&store.lock(), &open, &[(ViewName::Main, EVERY)])
            .unwrap();
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
        let pinned = store
            .catalog()
            .pin(&store.lock(), &at, &[(ViewName::Main, EVERY)]);
        assert!(pinned.is_ok(), "{name}: refused at its own charge {charge}");
        drop(pinned);
        assert_eq!(at.live_bytes(), 0);
        if charge > 0 {
            copied.store(0, Ordering::SeqCst);
            let below = QueryBudget::new(charge - 1, None);
            let err = store
                .catalog()
                .pin(&store.lock(), &below, &[(ViewName::Main, EVERY)])
                .unwrap_err();
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

/// One snapshot of `store` that loads `main` for the graph classes, then for the text class,
/// then for the graph classes again (reordered, one repeated): what the graph load charged,
/// the classes of the rows each of the first two loads holds, and whether the third returned
/// the first's cached view.
fn graph_then_text(store: &SqliteStore) -> (u64, Vec<ExtentClass>, Vec<ExtentClass>, bool) {
    const GRAPH: &[ExtentClass] = &[ExtentClass::GraphBase, ExtentClass::GraphLayer];
    let classes = |v: &PinnedView| v.sediment().iter().map(|r| r.class).collect();
    let budget = QueryBudget::new(u64::MAX, None);
    store
        .catalog()
        .with_snapshot(&store.lock(), &budget, |tx, pin| {
            let pinned = budget.live_bytes();
            let graph = pin.load_view(tx, &ViewName::Main, GRAPH)?;
            let charged = budget.live_bytes() - pinned;
            let text = pin.load_view(tx, &ViewName::Main, &[ExtentClass::TextTier])?;
            let again = pin.load_view(
                tx,
                &ViewName::Main,
                &[
                    ExtentClass::GraphLayer,
                    ExtentClass::GraphBase,
                    ExtentClass::GraphLayer,
                ],
            )?;
            Ok((
                charged,
                classes(&graph),
                classes(&text),
                Arc::ptr_eq(&graph, &again),
            ))
        })
        .unwrap()
}

#[test]
fn a_view_loads_and_charges_only_the_sediment_of_the_classes_asked_for() {
    // Arrange: text and graph sediment in one catalog; the same graph sediment alone in another
    let mixed = FileStore::new("classes-mixed");
    catalog_rows(&mixed.store, &["text_tier", "graph_base"], 3, 5, 0);
    let graph_only = FileStore::new("classes-graph");
    catalog_rows(&graph_only.store, &["graph_base"], 3, 5, 0);

    // Act
    let (charged, graph, text, reused) = graph_then_text(&mixed.store);
    let (charged_alone, ..) = graph_then_text(&graph_only.store);

    // Assert: the graph load copies and charges the graph rows alone; the text load in the
    // same pin is its own view with the text rows; the same class set hits the cache
    assert_eq!(graph, vec![ExtentClass::GraphBase; 3]);
    assert_eq!(charged, charged_alone, "the graph load charged other rows");
    assert_eq!(text, vec![ExtentClass::TextTier; 3]);
    assert!(reused, "the same class set loaded main again");
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
    chain_into_main(&store);
    let (_pin, main) = pin_main(&store);
    let pinned: Vec<u64> = test_extents(&main).iter().map(|e| e.id.0).collect();
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

    chain_into_main(&store);

    // Act
    let (_pin, main) = pin_main(&store);

    // Assert
    let paths: Vec<_> = test_extents(&main)
        .into_iter()
        .map(|e| e.location)
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
            txn.publish(inside, &[ViewName::Main])?;
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

    chain_into_main(store);

    // Act
    let (pin, main) = pin_main(store);
    let extents = test_extents(&main);

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
    let conn = store.lock();
    conn.execute_batch("SAVEPOINT outer").unwrap();

    // Act
    let pinned = store.catalog().pin(&conn, &budget, &[]).map(|_| ());
    let snapshot = store.catalog().with_snapshot(&conn, &budget, |_, _| Ok(()));

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

#[test]
fn a_snapshot_on_a_connection_with_a_live_cursor_is_refused_by_name() {
    use super::pin::OuterTransaction;
    use kaban_core::query_budget::QueryBudget;

    // Arrange: a SELECT stepped once and not finished: autocommit, but its read transaction
    // (an older snapshot) is still open
    let store = SqliteStore::open_memory().unwrap();
    sediment_rows(&store, 2);
    let conn = store.lock();
    let mut stmt = conn.prepare("SELECT seq FROM cat_sediment").unwrap();
    let mut rows = stmt.query([]).unwrap();
    assert!(rows.next().unwrap().is_some());
    assert!(conn.is_autocommit());
    let budget = QueryBudget::new(u64::MAX, None);

    // Act
    let out = store.catalog().with_snapshot(&conn, &budget, |_, _| Ok(()));

    // Assert
    let err = out.expect_err("a snapshot adopted a live cursor's transaction");
    let StoreError::Backend { source } = &err else {
        panic!("{err:?}");
    };
    assert!(
        source.driver().downcast_ref::<OuterTransaction>().is_some(),
        "{err:?}"
    );
}

#[test]
fn a_view_does_not_load_through_another_database_at_the_same_generation() {
    use super::pin::OutsidePin;
    use kaban_core::query_budget::QueryBudget;

    // Arrange: two databases, each at generation 1
    let (ours, theirs) = (
        SqliteStore::open_memory().unwrap(),
        SqliteStore::open_memory().unwrap(),
    );
    sediment_rows(&ours, 1);
    sediment_rows(&theirs, 1);
    let budget = QueryBudget::new(u64::MAX, None);
    let other = theirs.lock();

    // Act: inside our snapshot, load main through a read transaction on the other database
    let out = ours
        .catalog()
        .with_snapshot(&ours.lock(), &budget, |_, pin| {
            theirs.catalog().read(&other, |tx| {
                Ok(pin.load_view(tx, &ViewName::Main, EVERY).map(|_| ()))
            })
        })
        .unwrap();

    // Assert
    let err = out.expect_err("a pin loaded another database's view");
    let StoreError::Backend { source } = &err else {
        panic!("{err:?}");
    };
    assert!(
        source.driver().downcast_ref::<OutsidePin>().is_some(),
        "{err:?}"
    );
}

// One owner per published range (END-MAJ-01): an appended range is published at most once, only
// by the catalog of the container it was appended to, and never over a range a catalog row
// already names; retiring one extent never releases bytes another live extent owns.

/// Whether `T` is `Clone`, decided at compile time and observed at run time (autoref probing:
/// the by-value candidate applies only when the bound holds).
struct CloneProbe<T>(std::marker::PhantomData<T>);

trait Cloneable {
    fn is_clone(&self) -> bool {
        true
    }
}

impl<T: Clone> Cloneable for CloneProbe<T> {}

trait NotCloneable {
    fn is_clone(&self) -> bool {
        false
    }
}

impl<T> NotCloneable for &CloneProbe<T> {}

/// The driver's text of a refused store error.
fn driver_text(err: &StoreError) -> String {
    let StoreError::Backend { source } = err else {
        panic!("expected a refusal behind Backend, got {err:?}");
    };
    source.driver().to_string()
}

/// The container ranges `(offset, len)` the catalog rows of `store` name, in id order.
fn kbx_rows(store: &SqliteStore) -> Vec<(i64, i64)> {
    let conn = store.lock();
    let mut stmt = conn
        .prepare(
            "SELECT kbx_offset, kbx_len FROM cat_extent WHERE kbx_offset IS NOT NULL ORDER BY id",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect()
}

/// The raw bytes of `store`'s container at `range`.
fn raw_range(fs: &FileStore, (offset, len): (u64, u64)) -> Vec<u8> {
    let raw = std::fs::read(fs.dir.join("kaban.kbx")).unwrap();
    raw[usize::try_from(offset).unwrap()..usize::try_from(offset + len).unwrap()].to_vec()
}

/// The container range of the pinned test-class extent `id` and its bytes through the pin.
fn pinned_extent(store: &SqliteStore, id: u64) -> Option<((u64, u64), Vec<u8>)> {
    let (pin, main) = pin_main(store);
    let extents = test_extents(&main);
    let e = extents.iter().find(|e| e.id.0 == id)?;
    let super::class::ExtentLocation::Kbx { offset, len } = e.location else {
        panic!("extent {id} is not in the container");
    };
    Some(((offset, len), pin.bytes(e).unwrap().as_slice().to_vec()))
}

#[test]
fn an_unpublished_extent_cannot_be_copied_so_one_append_is_published_at_most_once() {
    // Arrange: the probe tells a Clone type
    #[expect(
        clippy::needless_borrow,
        reason = "the same probe expression as for the probed type, whose borrow is needed"
    )]
    let probe_sees_clone = (&CloneProbe::<String>(std::marker::PhantomData)).is_clone();
    assert!(probe_sees_clone, "the probe does not tell a Clone type");

    // Act: whether a second token for the same range can be made from the first
    let cloneable = (&CloneProbe::<super::kbx::Unpublished>(std::marker::PhantomData)).is_clone();

    // Assert: it cannot; `Txn::publish` takes the token by value, so publishing it consumes it
    assert!(
        !cloneable,
        "Unpublished is Clone: one append can be published as two extents"
    );
}

#[test]
fn a_token_appended_to_another_container_is_refused_by_name() {
    // Arrange: two projects; a range appended to the first's container
    let first = FileStore::new("foreign-a");
    let second = FileStore::new("foreign-b");
    let token = first
        .store
        .catalog()
        .append_extent(ExtentClass::Test, &[7_u8; 64])
        .unwrap();

    // Act: the second project's commit publishes it
    let out = second
        .store
        .catalog()
        .commit(&mut second.store.lock(), Durability::Normal, |txn| {
            txn.publish(token, &[ViewName::Main])
        });

    // Assert: refused by name, and the second catalog names no range
    let err = out.expect_err("a token of another container was published");
    let text = driver_text(&err);
    assert!(
        text.contains("catalog_publish_refused") && text.contains("another container"),
        "{text}"
    );
    assert_eq!(kbx_rows(&second.store), []);
}

#[test]
fn a_range_that_overlaps_a_range_a_catalog_row_names_is_refused() {
    // Arrange: a catalog row that already names the first GiB of the container (what a corrupt
    // or replayed row would name), then a fresh append inside it
    let fs = FileStore::new("overlap");
    let store = &fs.store;
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let generation = txn.generation()?.0;
            txn.conn()
                .execute(
                    "INSERT INTO cat_extent(class, generation, kbx_offset, kbx_len)
                     VALUES ('test', ?1, 0, 1073741824)",
                    [i64::try_from(generation).unwrap()],
                )
                .map_err(|e| StoreError::Backend {
                    source: kaban_core::BackendSource::new(e),
                })
        })
        .unwrap();
    let token = store
        .catalog()
        .append_extent(ExtentClass::Test, &[5_u8; 64])
        .unwrap();

    // Act
    let out = store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(token, &[ViewName::Main])
        });

    // Assert: refused by name; only the first row names a range
    let err = out.expect_err("an overlapping range was published");
    let text = driver_text(&err);
    assert!(
        text.contains("catalog_publish_refused") && text.contains("overlaps"),
        "{text}"
    );
    assert_eq!(kbx_rows(store), [(0, 1_073_741_824)]);
}

#[test]
fn retiring_one_published_extent_never_releases_the_bytes_of_another() {
    use super::testing::{reclaim, retire};

    // Arrange: two extents, each appended and published on its own
    let fs = FileStore::new("one-owner");
    let store = &fs.store;
    let publish = |fill: u8| {
        let token = store
            .catalog()
            .append_extent(ExtentClass::Test, &[fill; 8192])
            .unwrap();
        store
            .catalog()
            .commit(&mut store.lock(), Durability::Normal, |txn| {
                txn.publish(token, &[ViewName::Main])
            })
            .unwrap()
            .0
    };
    let doomed = publish(3);
    let keeper = publish(4);
    let (doomed_at, _) = pinned_extent(store, doomed).unwrap();

    // Act
    retire(store, &[doomed]).unwrap();
    let reclaimed = reclaim(store).unwrap();

    // Assert: the doomed range alone was released; the keeper reads its own bytes
    assert_eq!(reclaimed.punched, [doomed]);
    assert_eq!(raw_range(&fs, doomed_at), vec![0_u8; 8192]);
    let (_, bytes) = pinned_extent(store, keeper).unwrap();
    assert_eq!(bytes, vec![4_u8; 8192]);
}

#[test]
fn a_range_whose_publication_rolled_back_stays_unowned_and_is_never_released() {
    use super::testing::{reclaim, retire};

    // Arrange: a publication whose commit fails after publishing, so it rolls back
    let fs = FileStore::new("rolled-back");
    let store = &fs.store;
    let leaked = store
        .catalog()
        .append_extent(ExtentClass::Test, &[9_u8; 8192])
        .unwrap();
    let out = store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(leaked, &[ViewName::Main])?;
            Err::<(), _>(StoreError::NotFound(
                "a writer that fails after publishing".into(),
            ))
        });
    assert!(out.is_err());
    let rows_after_rollback = kbx_rows(store);
    // the leaked range: the container's first 8 KiB (the token itself is gone)
    let leaked_at = (0, 8192);

    // Act: later extents are published, one retired and reclaimed
    let publish = |fill: u8| {
        let token = store
            .catalog()
            .append_extent(ExtentClass::Test, &[fill; 8192])
            .unwrap();
        store
            .catalog()
            .commit(&mut store.lock(), Durability::Normal, |txn| {
                txn.publish(token, &[ViewName::Main])
            })
            .unwrap()
            .0
    };
    let doomed = publish(3);
    let keeper = publish(4);
    retire(store, &[doomed]).unwrap();
    let reclaimed = reclaim(store).unwrap();

    // Assert: no row named the rolled-back range, the later ranges lie beyond it, and
    // reclamation released the doomed range only: the unowned bytes stay for T6's verify
    assert_eq!(rows_after_rollback, []);
    assert_eq!(reclaimed.punched, [doomed]);
    assert_eq!(raw_range(&fs, leaked_at), vec![9_u8; 8192]);
    let (keeper_at, bytes) = pinned_extent(store, keeper).unwrap();
    assert!(keeper_at.0 >= 8192, "{keeper_at:?}");
    assert_eq!(bytes, vec![4_u8; 8192]);
}

/// The extent ids of `view`'s chain, in chain order.
fn chain(store: &SqliteStore, view: &ViewName) -> Vec<u64> {
    let conn = store.lock();
    let mut stmt = conn
        .prepare("SELECT extent_id FROM cat_view_extent WHERE view = ?1 ORDER BY position")
        .unwrap();
    stmt.query_map([view.to_string()], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|id| u64::try_from(id.unwrap()).unwrap())
        .collect()
}

#[test]
fn views_sharing_a_base_resolve_to_their_own_ordered_chains_and_rows_carry_content_hashes() {
    use sha2::{Digest, Sha256};

    // Arrange: two code views over one base, one layer each
    let fs = FileStore::new("ancestry");
    let store = &fs.store;
    let view = |c: char| -> ViewName {
        format!("code@{}", c.to_string().repeat(40))
            .parse()
            .unwrap()
    };
    let (a, b) = (view('a'), view('b'));
    let publish = |class: ExtentClass, bytes: &[u8], views: &[ViewName]| -> u64 {
        let token = store.catalog().append_extent(class, bytes).unwrap();
        store
            .catalog()
            .commit(&mut store.lock(), Durability::Normal, |txn| {
                txn.publish(token, views)
            })
            .unwrap()
            .0
    };
    let written: [&[u8]; 3] = [b"the shared base", b"layer over a", b"layer over b"];
    let base = publish(ExtentClass::CodeBase, written[0], &[a.clone(), b.clone()]);
    let layer_a = publish(ExtentClass::CodeLayer, written[1], std::slice::from_ref(&a));
    let layer_b = publish(ExtentClass::CodeLayer, written[2], std::slice::from_ref(&b));

    // Act
    let (chain_a, chain_b) = (chain(store, &a), chain(store, &b));
    let hashes: Vec<Option<Vec<u8>>> = [base, layer_a, layer_b]
        .iter()
        .map(|id| {
            store
                .lock()
                .query_row(
                    "SELECT content_sha256 FROM cat_extent WHERE id = ?1",
                    [i64::try_from(*id).unwrap()],
                    |r| r.get(0),
                )
                .unwrap()
        })
        .collect();

    // Assert: each view resolves to the base, then its own layer; each row's hash is the
    // SHA-256 of its bytes
    assert_eq!(chain_a, [base, layer_a], "code@a's chain");
    assert_eq!(chain_b, [base, layer_b], "code@b's chain");
    for (hash, bytes) in hashes.iter().zip(written) {
        assert_eq!(hash.as_deref(), Some(Sha256::digest(bytes).as_slice()));
    }
}

/// A file-backed catalog whose `main` holds a text, a graph base and a graph layer extent and
/// three sediment rows, and whose code view `code` holds `code_layers` code extents.
fn view_catalog(name: &str, code: &ViewName, code_layers: usize) -> FileStore {
    let fs = FileStore::new(name);
    let store = &fs.store;
    let publish = |class: ExtentClass, views: &[ViewName]| {
        let token = store.catalog().append_extent(class, &[3_u8; 64]).unwrap();
        store
            .catalog()
            .commit(&mut store.lock(), Durability::Normal, |txn| {
                txn.publish(token, views)
            })
            .unwrap();
    };
    for class in [
        ExtentClass::TextTier,
        ExtentClass::GraphBase,
        ExtentClass::GraphLayer,
    ] {
        publish(class, &[ViewName::Main]);
    }
    for _ in 0..code_layers {
        publish(ExtentClass::CodeLayer, std::slice::from_ref(code));
    }
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            let generation = i64::try_from(txn.generation()?.0).unwrap();
            for i in 0..3 {
                txn.conn()
                    .execute(
                        "INSERT INTO cat_sediment(class, generation, payload) VALUES ('test', ?1, ?2)",
                        rusqlite::params![generation, format!("row {i}").into_bytes()],
                    )
                    .map_err(|e| StoreError::Backend {
                        source: kaban_core::BackendSource::new(e),
                    })?;
            }
            Ok(())
        })
        .unwrap();
    fs
}

/// What one snapshot of `store` charged: on pinning, after loading `main`, after loading it
/// again, after loading `code`; the two chains' lengths; whether loading `main` again returned
/// the cached view.
fn view_charges(store: &SqliteStore, code: &ViewName) -> ([u64; 4], [usize; 2], bool) {
    let budget = kaban_core::query_budget::QueryBudget::new(u64::MAX, None);
    let out = store
        .catalog()
        .with_snapshot(&store.lock(), &budget, |tx, pin| {
            let pinned = budget.live_bytes();
            let main = pin.load_view(tx, &ViewName::Main, EVERY)?;
            let with_main = budget.live_bytes();
            let again = pin.load_view(tx, &ViewName::Main, EVERY)?;
            let cached = budget.live_bytes();
            let code = pin.load_view(tx, code, EVERY)?;
            let with_code = budget.live_bytes();
            Ok((
                [pinned, with_main, cached, with_code],
                [main.chain().len(), code.chain().len()],
                Arc::ptr_eq(&main, &again),
            ))
        })
        .unwrap();
    assert_eq!(budget.live_bytes(), 0, "a view's charge outlived its pin");
    out
}

#[test]
fn a_pin_charges_a_view_only_once_it_is_loaded_and_only_for_that_view() {
    // Arrange: the same `main` in two catalogs, one of them with 40 code extents besides
    let code: ViewName = format!("code@{}", "c".repeat(40)).parse().unwrap();
    let with_code = view_catalog("views-code", &code, 40);
    let main_only = view_catalog("views-main", &code, 0);

    // Act
    let ([pinned, with_main, cached, loaded_code], chains, reused) =
        view_charges(&with_code.store, &code);
    let ([pinned_alone, with_main_alone, ..], ..) = view_charges(&main_only.store, &code);

    // Assert: the code extents cost nothing until the code view loads; loading `main` costs
    // what `main` alone costs; a loaded view is cached; loading the code view charges it
    assert_eq!(
        pinned, pinned_alone,
        "the pin charged for views it did not load"
    );
    assert!(with_main > pinned, "loading main charged nothing");
    assert_eq!(
        with_main - pinned,
        with_main_alone - pinned_alone,
        "loading main charged for another view's rows"
    );
    assert_eq!(cached, with_main, "a cached view was charged again");
    assert!(
        loaded_code > cached,
        "loading the code view charged nothing"
    );
    assert_eq!(chains, [3, 40]);
    assert!(reused, "a loaded view was loaded again");
}

#[test]
fn a_writer_catalog_registers_the_text_graph_and_code_classes() {
    // Arrange, Act
    let store = SqliteStore::open_memory().unwrap();

    // Assert: the three production classes, in that order, before any test class; graph and
    // code own their bases beside their layers
    let classes: Vec<Vec<ExtentClass>> = store
        .catalog()
        .classes()
        .iter()
        .map(|c| c.classes().to_vec())
        .collect();
    assert_eq!(
        classes,
        vec![
            vec![ExtentClass::TextTier],
            vec![ExtentClass::GraphLayer, ExtentClass::GraphBase],
            vec![ExtentClass::CodeLayer, ExtentClass::CodeBase]
        ]
    );
}

#[test]
fn a_pin_hands_out_a_view_it_loaded_without_a_connection() {
    // Arrange: text sediment in `main`
    let fs = FileStore::new("loaded");
    catalog_rows(&fs.store, &["text_tier"], 1, 4, 0);
    let code: ViewName = format!("code@{}", "d".repeat(40)).parse().unwrap();
    let budget = QueryBudget::new(u64::MAX, None);

    // Act: `main` loaded for the text class, then asked of the cache
    let (hit, other_class, other_view) = fs
        .store
        .catalog()
        .with_snapshot(&fs.store.lock(), &budget, |tx, pin| {
            let text = pin.load_view(tx, &ViewName::Main, &[ExtentClass::TextTier])?;
            Ok((
                pin.loaded(&ViewName::Main, &[ExtentClass::TextTier])
                    .is_some_and(|v| Arc::ptr_eq(&v, &text)),
                pin.loaded(&ViewName::Main, &[ExtentClass::GraphBase]),
                pin.loaded(&code, &[]),
            ))
        })
        .unwrap();

    // Assert: the cached view comes back; one loaded for other classes or another view does not
    assert!(hit, "the loaded view was not handed out");
    assert!(other_class.is_none(), "a text load served the graph class");
    assert!(other_view.is_none(), "an unloaded view was handed out");
}

/// Append `bytes` as an extent of `class` and publish it into `main`; its id.
fn publish_into_main(store: &SqliteStore, class: ExtentClass, bytes: &[u8]) -> u64 {
    let token = store.catalog().append_extent(class, bytes).unwrap();
    store
        .catalog()
        .commit(&mut store.lock(), Durability::Normal, |txn| {
            txn.publish(token, &[ViewName::Main])
        })
        .unwrap()
        .0
}

#[test]
fn a_fold_reaches_its_class_s_extents_past_another_class_s_between_them() {
    // Arrange: `main` = [T0, G0, T1, G1, T2]: the test class's run with graph extents between
    // (layers heavy enough for the test class to fold, the graph's too light to)
    let fs = FileStore::new("fold-interleaved");
    let store = &fs.store;
    let _observed = super::testing::install_edge_sediment_class(store);
    let t0 = publish_into_main(store, ExtentClass::Test, b"t0");
    let g0 = publish_into_main(store, ExtentClass::GraphBase, &[1; 1000]);
    let t1 = publish_into_main(store, ExtentClass::Test, b"t1");
    let g1 = publish_into_main(store, ExtentClass::GraphBase, &[2; 10]);
    let t2 = publish_into_main(store, ExtentClass::Test, b"t2");

    // Act
    let report = super::testing::compact(
        store,
        super::compact::CompactBudget {
            memory_bytes: u64::MAX,
            deadline: std::time::Duration::from_secs(10),
        },
    )
    .unwrap();

    // Assert: one fold of T0..T2 placed where T0 was; the graph extents keep their order
    let folds: Vec<Vec<u64>> = report.published.iter().map(|c| c.extents.clone()).collect();
    assert_eq!(folds, vec![vec![t0, t1, t2]], "{report:?}");
    let (pin, main) = pin_main(store);
    let chain: Vec<(ExtentClass, u64)> = main.chain().iter().map(|e| (e.class, e.id.0)).collect();
    assert_eq!(
        chain[1..],
        [(ExtentClass::GraphBase, g0), (ExtentClass::GraphBase, g1)]
    );
    assert_eq!(chain[0].0, ExtentClass::Test);
    assert_eq!(pin.bytes(&main.chain()[0]).unwrap().as_slice(), b"t0t1t2");
}

#[test]
fn a_replaced_run_with_a_gap_in_its_own_class_is_refused() {
    // Arrange: `main` = [T0, G0, T1, T2], and a claim on T0 and T2 (T1 left out)
    let fs = FileStore::new("switch-gap");
    let store = &fs.store;
    let t0 = publish_into_main(store, ExtentClass::Test, b"t0");
    let _g0 = publish_into_main(store, ExtentClass::GraphBase, b"g0");
    let _t1 = publish_into_main(store, ExtentClass::Test, b"t1");
    let t2 = publish_into_main(store, ExtentClass::Test, b"t2");
    let claim = super::testing::claim(store, &[t0, t2]).unwrap().unwrap();

    // Act
    let out = super::testing::publish(
        store,
        claim,
        &[b"t0t2".to_vec()],
        &[("main", &[t0, t2])],
        &[t0, t2],
        &[],
    );

    // Assert
    let err = out
        .map(drop)
        .expect_err("a run with a gap in its class was replaced")
        .0;
    let StoreError::Backend { source } = &err else {
        panic!("not a refused switch: {err:?}");
    };
    let text = source.driver().to_string();
    assert!(text.contains("is not consecutive"), "{text}");
}
