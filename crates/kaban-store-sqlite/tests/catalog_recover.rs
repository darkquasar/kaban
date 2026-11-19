//! Recovery and verification (spec 11 T6 Step 4, KER-02, KER-07, PC-9, 7.4): verify names
//! every inconsistency it checks, recovery clears what a dead process left (stale claims,
//! unpublished ranges) and nothing else, and after a child process is killed at any of the
//! named crash points, recover then verify report a consistent catalog with every row present
//! or absent.
//!
//! Runs with the `failpoints` feature (the test class and the crash points live behind it).
//! Every wait is bounded: the child is killed and reaped however the test ends, and the test
//! fails if the child does not die within [`STEP`].

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::config::{CatalogCfg, SealOps};
use kaban_core::query_budget::QueryBudget;
use kaban_core::{IngestStore, MemoryStore};
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing::{self, Window};
use std::os::unix::fs::FileExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The bound of any one wait on the child process.
const STEP: Duration = Duration::from_secs(30);
/// The database the crashing child opens (set only for the child process).
const CRASH_DB: &str = "KABAN_T6_CRASH_DB";

fn budget() -> QueryBudget {
    QueryBudget::new(u64::MAX, Some(STEP))
}

/// `len` bytes of the nonzero `fill`.
fn pattern(fill: u8, len: usize) -> Vec<u8> {
    vec![fill; len]
}

/// The test policy: seal at 2 pending rows, fold past 2 layers.
fn policy() -> CatalogCfg {
    CatalogCfg {
        seal_ops: SealOps {
            graph: 2,
            ..SealOps::default()
        },
        fold_depth: 2,
        ..CatalogCfg::default()
    }
}

/// A store of the database at `db` under [`policy`], with the test class installed.
fn open_at(db: &std::path::Path) -> SqliteStore {
    let store = SqliteStore::open(db, &db.with_file_name("keys"))
        .unwrap()
        .with_catalog_config(policy())
        .unwrap();
    let _observed = catalog_testing::install_edge_sediment_class(&store);
    store
}

/// The catalog's contents as a reader sees them: the pending sediment payloads and the bytes of
/// `main`'s chain, concatenated in chain order.
fn contents(store: &SqliteStore) -> (Vec<String>, Vec<u8>) {
    let sediment = catalog_testing::pin_sediment(store).unwrap();
    let pin = catalog_testing::pin(store, &budget()).unwrap();
    let view = pin.view();
    let bytes = view
        .extents()
        .into_iter()
        .flat_map(|id| view.read(id).unwrap().unwrap())
        .collect();
    (sediment, bytes)
}

#[test]
fn a_consistent_catalog_verifies_clean_and_lists_its_classes() {
    // Arrange: two live extents, one of them then folded away and reclaimed
    let s = Scratch::new("kaban-t6-recover", "clean");
    let store = open_at(&s.db());
    catalog_testing::append_and_publish_all(&store, &[pattern(1, 64), pattern(2, 64)]).unwrap();
    catalog_testing::append_and_publish(&store, &pattern(3, 64)).unwrap();
    catalog_testing::compact(&store, store_budget()).unwrap();

    // Act
    let report = store.verify_catalog().unwrap();

    // Assert
    assert_eq!(report.inconsistencies(), 0, "{report:?}");
    assert_eq!(report.unreferenced_ranges, Vec::new(), "{report:?}");
    let classes: Vec<&str> = report.classes.iter().map(|c| c.class.as_str()).collect();
    assert_eq!(classes, vec!["test"], "{report:?}");
    assert_eq!(report.classes[0].live, 1, "{report:?}");
}

/// An unbounded unit budget.
fn store_budget() -> kaban_store_sqlite::CompactBudget {
    kaban_store_sqlite::CompactBudget {
        memory_bytes: u64::MAX,
        deadline: STEP,
    }
}

#[test]
fn verify_names_each_inconsistency_it_checks() {
    // Arrange: a leaked range, a stale claim, a corrupted extent, a missing cold file, a
    // retired extent left in a chain, a gap in a chain and a view to re-import
    let s = Scratch::new("kaban-t6-recover", "inconsistent");
    let store = open_at(&s.db());
    let ids =
        catalog_testing::append_and_publish_all(&store, &[pattern(1, 64), pattern(2, 64)]).unwrap();
    let code = format!("code@{}", "9".repeat(40));
    let other =
        catalog_testing::append_and_publish_into(&store, &pattern(3, 64), &[&code]).unwrap();
    catalog_testing::append_unpublished(&store, &pattern(4, 64)).unwrap();
    drop(catalog_testing::claim(&store, &[ids[1]]).unwrap().unwrap());
    let corrupted = catalog_testing::pin(&store, &budget())
        .unwrap()
        .view()
        .span(ids[0])
        .unwrap();
    let kbx = std::fs::OpenOptions::new()
        .write(true)
        .open(s.0.join("kaban.kbx"))
        .unwrap();
    kbx.write_all_at(&[0xEE], corrupted.0).unwrap();
    catalog_testing::corrupt(
        &store,
        &format!(
            "INSERT INTO cat_extent(class, generation, relpath) VALUES ('test', 1, 'cold/gone.kbx');
             INSERT INTO cat_view_extent(view, position, extent_id)
               SELECT 'main', 7, max(id) FROM cat_extent;
             UPDATE cat_extent SET dead_since = 1 WHERE id = {other};
             INSERT INTO cat_view(name, state) VALUES ('{code}', 'needs_reimport')
               ON CONFLICT(name) DO UPDATE SET state = 'needs_reimport';"
        ),
    )
    .unwrap();

    // Act
    let report = store.verify_catalog().unwrap();

    // Assert: each finding by name
    assert!(!report.maintenance_live, "{report:?}");
    assert_eq!(report.unreferenced_ranges.len(), 1, "{report:?}");
    assert_eq!(report.unreferenced_ranges[0].1, 64, "{report:?}");
    assert_eq!(report.stale_claims, vec![ids[1]], "{report:?}");
    assert_eq!(report.hash_mismatches, vec![ids[0]], "{report:?}");
    assert_eq!(report.missing_files, vec!["cold/gone.kbx".to_owned()]);
    assert_eq!(report.needs_reimport, vec![code.clone()]);
    assert_eq!(
        report.chain_inconsistencies,
        vec![
            "view main: positions not contiguous from 0".to_owned(),
            format!("view {code}: retired extent {other} is still in its chain"),
        ],
        "{report:?}"
    );
    assert_eq!(report.inconsistencies(), 6, "{report:?}");
}

#[test]
fn recovery_clears_stale_claims_and_reclaims_unpublished_ranges_only() {
    // Arrange: a live extent, a claim its process forgot, a leaked range
    let s = Scratch::new("kaban-t6-recover", "recover");
    let store = open_at(&s.db());
    let id = catalog_testing::append_and_publish(&store, &pattern(1, 4096)).unwrap();
    drop(catalog_testing::claim(&store, &[id]).unwrap().unwrap());
    catalog_testing::append_unpublished(&store, &pattern(2, 4096)).unwrap();
    let before = contents(&store);

    // Act
    let recovered = store.recover_catalog().unwrap();
    let report = store.verify_catalog().unwrap();

    // Assert: the claim and the leak are gone, the live extent is untouched
    assert!(!recovered.maintenance_live, "{recovered:?}");
    assert_eq!(recovered.claims_cleared, 1, "{recovered:?}");
    assert_eq!(
        recovered.ranges_reclaimed,
        vec![(4096, 4096)],
        "{recovered:?}"
    );
    assert_eq!(report.inconsistencies(), 0, "{report:?}");
    assert_eq!(contents(&store), before);
    assert!(catalog_testing::claim(&store, &[id]).unwrap().is_some());
}

#[test]
fn recovery_leaves_live_maintenance_alone() {
    // Arrange: a compaction of another store that stops after its claim
    let s = Scratch::new("kaban-t6-recover", "live");
    let store = open_at(&s.db());
    let worker = open_at(&s.db());
    let ids = catalog_testing::append_and_publish_all(
        &store,
        &[pattern(1, 64), pattern(2, 64), pattern(3, 64)],
    )
    .unwrap();
    let (claimed, on_claim) = std::sync::mpsc::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    catalog_testing::set_failpoint(
        &worker,
        catalog_testing::Window::CompactClaimed,
        Some(std::sync::Arc::new(move || {
            let _ = claimed.send(());
            let _ = wait.lock().unwrap().recv_timeout(STEP);
        })),
    );
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(catalog_testing::compact(&worker, store_budget()));
    });
    on_claim
        .recv_timeout(STEP)
        .expect("the compaction never claimed");

    // Act
    let recovered = store.recover_catalog().unwrap();
    let verified = store.verify_catalog().unwrap();
    go.send(()).unwrap();
    let folded = finished.recv_timeout(STEP).expect("no compaction").unwrap();

    // Assert: recovery saw live maintenance and cleared nothing, so the fold published
    assert!(recovered.maintenance_live, "{recovered:?}");
    assert_eq!(recovered.claims_cleared, 0, "{recovered:?}");
    assert!(verified.maintenance_live, "{verified:?}");
    assert_eq!(verified.inconsistencies(), 0, "{verified:?}");
    assert_eq!(folded.published.len(), 1, "{folded:?}");
    assert_eq!(folded.published[0].extents, ids);
}

/// The crashing role: opens the database named by `KABAN_T6_CRASH_DB` and runs the operation
/// that reaches the crash point `KABAN_FAILPOINT` names, where the process kills itself. A
/// no-op without the variables; a role that survives its operation exits with status 3.
#[test]
#[ignore = "the crashing role of the crash-point cases"]
fn crash_role() {
    let (Ok(db), Ok(point)) = (
        std::env::var(CRASH_DB),
        std::env::var(catalog_testing::FAILPOINT_ARM),
    ) else {
        return;
    };
    let store = open_at(std::path::Path::new(&db));
    match point.as_str() {
        "reclaim.after_floor" | "reclaim.mid_punch" => {
            catalog_testing::reclaim_retired(&store).unwrap();
        }
        "recover.mid" => {
            // what a dead compaction leaves: a stale claim and an unpublished range
            let live = catalog_testing::pin(&store, &budget())
                .unwrap()
                .view()
                .extents();
            drop(catalog_testing::claim(&store, &live[..1]).unwrap().unwrap());
            catalog_testing::append_unpublished(&store, &pattern(9, 512)).unwrap();
            store.recover_catalog().unwrap();
        }
        _ => {
            catalog_testing::compact(&store, store_budget()).unwrap();
        }
    }
    std::process::exit(3);
}

/// Arrange the catalog of `store` for a crash at `point`: pending sediment for a seal, a chain
/// past the fold depth for a fold or a publication, a retired extent for a reclamation.
fn arrange(store: &SqliteStore, point: &str) {
    match point {
        "seal.mid" => {
            let a = created(store, "Fact", "the lamp is lit at dusk");
            let b = created(store, "Fact", "the keeper climbs the stair");
            for (from, to) in [(&a, &b), (&b, &a)] {
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
            }
        }
        "reclaim.after_floor" | "reclaim.mid_punch" => {
            catalog_testing::append_and_publish(store, &pattern(1, 4096)).unwrap();
            let doomed =
                catalog_testing::append_and_publish_into(store, &pattern(2, 4096), &[]).unwrap();
            catalog_testing::retire(store, &[doomed]).unwrap();
        }
        _ => {
            catalog_testing::append_and_publish_all(
                store,
                &[pattern(1, 4096), pattern(2, 4096), pattern(3, 4096)],
            )
            .unwrap();
        }
    }
}

/// Kill a child at `point` (arranged by [`arrange`]), then recover and verify.
fn crash_at(point: &str) {
    // Arrange
    let s = Scratch::new("kaban-t6-crash", &point.replace('.', "-"));
    let store = open_at(&s.db());
    arrange(&store, point);
    let before = contents(&store);
    let stderr = std::fs::File::create(s.0.join("child.stderr")).unwrap();
    let mut child = Supervised(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_role", "--ignored", "--nocapture"])
            .env(CRASH_DB, s.db())
            .env(catalog_testing::FAILPOINT_ARM, point)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );

    // Act: the child dies at the point; recover, then verify
    let status = child.wait(STEP);
    let found = store.verify_catalog().unwrap();
    let recovered = store.recover_catalog().unwrap();
    let report = store.verify_catalog().unwrap();

    // Assert: killed there, nothing inconsistent after recovery, every row present or absent
    let stderr = std::fs::read_to_string(s.0.join("child.stderr")).unwrap_or_default();
    assert_eq!(status.signal(), Some(9), "{point}: {status:?}\n{stderr}");
    assert!(!recovered.maintenance_live, "{point}: {recovered:?}");
    assert_eq!(report.inconsistencies(), 0, "{point}: {report:?}");
    assert_eq!(contents(&store), before, "{point}");
    if point == "publish.after_append" || point == "recover.mid" {
        // the dead process left an unpublished range: verify found it, recovery reclaimed it
        assert_eq!(found.unreferenced_ranges.len(), 1, "{point}: {found:?}");
        assert_eq!(recovered.ranges_reclaimed, found.unreferenced_ranges);
    }
    if point == "fold.mid" || point == "publish.after_append" {
        assert_eq!(found.stale_claims.len(), 3, "{point}: {found:?}");
        assert_eq!(recovered.claims_cleared, 3, "{point}: {recovered:?}");
    }
}

/// A child process of this binary, killed and reaped however the test ends.
struct Supervised(std::process::Child);

impl Supervised {
    /// Wait at most `bound` for the child to exit; panics past it.
    fn wait(&mut self, bound: Duration) -> std::process::ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(started.elapsed() < bound, "the child did not die in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "the missing-container child role"]
fn replacement_role() {
    let Ok(db) = std::env::var(CRASH_DB) else {
        return;
    };
    let db = std::path::Path::new(&db);
    SqliteStore::open(db, &db.with_file_name("keys")).unwrap();
}

/// One opener's bounded rendezvous, with each socket released explicitly by the test.
struct ReplacementSteps(std::os::unix::net::UnixListener);

impl ReplacementSteps {
    fn new(path: &std::path::Path) -> Self {
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(listener)
    }

    fn at(&self, expected: &str) -> std::os::unix::net::UnixStream {
        use std::io::BufRead;
        let start = Instant::now();
        let socket = loop {
            match self.0.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(start.elapsed() < STEP, "opener never reached {expected}");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept {expected}: {error}"),
            }
        };
        socket.set_read_timeout(Some(STEP)).unwrap();
        socket.set_write_timeout(Some(STEP)).unwrap();
        let mut name = String::new();
        std::io::BufReader::new(&socket)
            .read_line(&mut name)
            .unwrap();
        assert_eq!(name.trim(), expected);
        socket
    }
}

fn release_step(mut socket: std::os::unix::net::UnixStream) {
    use std::io::Write;
    socket.write_all(&[1]).unwrap();
}

fn replacement_child(s: &Scratch, socket: &std::path::Path) -> Supervised {
    Supervised(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "replacement_role", "--ignored", "--nocapture"])
            .env(CRASH_DB, s.db())
            .env("KABAN_TEST_REPLACEMENT_SOCKET", socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn replacement_owns_maintenance_before_the_writer_and_until_both_classes_publish() {
    use rustix::fs::{FlockOperation, flock};

    let s = Scratch::new("kaban-replacement", "owners");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    created(&store, "Fact", "zebra owner");
    store.reindex_graph().unwrap();
    store.reindex_text().unwrap();
    let graph = catalog_testing::graph_contents(&store).unwrap().extents;
    drop(store);
    std::fs::remove_file(s.0.join("kaban.kbx")).unwrap();
    let first_path = s.0.join("first.sock");
    let second_path = s.0.join("second.sock");
    let first_steps = ReplacementSteps::new(&first_path);
    let second_steps = ReplacementSteps::new(&second_path);
    let mut first = replacement_child(&s, &first_path);
    let before_lock = first_steps.at("replacement.before_lock");
    let maintenance = std::fs::File::open(s.0.join("kaban.kbx.pins/maintenance")).unwrap();
    flock(&maintenance, FlockOperation::NonBlockingLockShared).unwrap();
    let rows = rusqlite::Connection::open(s.db()).unwrap();
    rows.execute_batch("BEGIN IMMEDIATE").unwrap();
    release_step(before_lock);
    flock(&maintenance, FlockOperation::Unlock).unwrap();

    // The opener reaches exclusive ownership while this other process still holds the
    // writer. Taking the writer first would deadlock and fail the bounded rendezvous.
    let locked = first_steps.at("replacement.locked");
    let assert_exclusive = || {
        assert_eq!(
            flock(&maintenance, FlockOperation::NonBlockingLockShared),
            Err(rustix::io::Errno::WOULDBLOCK)
        );
    };
    assert_exclusive();
    rows.execute_batch("COMMIT").unwrap();
    release_step(locked);
    let before_create = first_steps.at("replacement.before_create");
    assert_exclusive();
    let mut second = replacement_child(&s, &second_path);
    release_step(second_steps.at("replacement.before_lock"));
    release_step(before_create);
    for point in ["replacement.after_create", "replacement.before_commit"] {
        let paused = first_steps.at(point);
        assert_exclusive();
        release_step(paused);
    }
    assert!(first.wait(STEP).success());
    let locked = second_steps.at("replacement.locked");
    let generation = || -> i64 {
        rows.query_row("SELECT max(generation) FROM cat_generation", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    let published = generation();
    release_step(locked);
    for point in ["replacement.before_create", "replacement.after_create"] {
        release_step(second_steps.at(point));
    }
    assert!(second.wait(STEP).success());
    assert_eq!(
        generation(),
        published,
        "the competing opener must reuse recovery"
    );
    flock(&maintenance, FlockOperation::NonBlockingLockExclusive).unwrap();
    flock(&maintenance, FlockOperation::Unlock).unwrap();
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(
        catalog_testing::graph_contents(&store).unwrap().extents,
        graph
    );
    assert_eq!(
        store
            .recall(&kaban_core::RecallQuery {
                query: "zebra".into(),
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
    let verified = store.verify_catalog().unwrap();
    assert_eq!(verified.inconsistencies(), 0, "{verified:?}");
}

#[test]
fn empty_and_diagnostic_opens_do_not_rebuild_or_advance_generations() {
    let s = Scratch::new("kaban-replacement", "empty-and-diagnostic");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let generation = || {
        rusqlite::Connection::open(s.db())
            .unwrap()
            .query_row(
                "SELECT COALESCE(max(generation), 0) FROM cat_generation",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(generation(), 0, "empty creation publishes no generation");
    assert_eq!(store.catalog_status().unwrap().classes, []);
    drop(store);
    std::fs::remove_file(s.0.join("kaban.kbx")).unwrap();
    let (diagnostic, _) =
        SqliteStore::open_diagnostic(&s.db(), &s.keys(), &kaban_core::ActorId::default()).unwrap();
    assert!(!s.0.join("kaban.kbx").exists());
    assert_eq!(generation(), 0);
    drop(diagnostic);
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(generation(), 0, "empty replacement publishes no generation");
    assert_eq!(store.catalog_status().unwrap().classes, []);
}

#[test]
fn replacement_preserves_cold_references_and_reports_missing_cold_data() {
    let s = Scratch::new("kaban-replacement", "cold");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    created(&store, "Fact", "zebra");
    store.reindex_graph().unwrap();
    std::fs::write(s.0.join("cold.parquet"), b"retained cold truth").unwrap();
    catalog_testing::corrupt(&store,
        "INSERT INTO cat_extent(class, generation, relpath) VALUES ('cold_file', 1, 'cold.parquet');
         INSERT INTO cat_view_extent(view, position, extent_id)
         SELECT 'main', (SELECT max(position) + 1 FROM cat_view_extent WHERE view = 'main'), max(id) FROM cat_extent;"
    ).unwrap();
    drop(store);
    std::fs::remove_file(s.0.join("kaban.kbx")).unwrap();
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let rows = rusqlite::Connection::open(s.db()).unwrap();
    let retained: i64 = rows
        .query_row(
            "SELECT count(*) FROM cat_extent WHERE relpath = 'cold.parquet'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retained, 1);
    let verified = store.verify_catalog().unwrap();
    assert_eq!(verified.inconsistencies(), 0, "{verified:?}");
    drop(rows);
    drop(store);
    std::fs::remove_file(s.0.join("cold.parquet")).unwrap();
    std::fs::remove_file(s.0.join("kaban.kbx")).unwrap();
    let error = SqliteStore::open(&s.db(), &s.keys())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cold data loss") && error.contains("cold.parquet"),
        "{error}"
    );
    assert!(
        !s.0.join("kaban.kbx").exists(),
        "missing truth refuses replacement"
    );
}

#[test]
fn interrupted_replacement_retries_both_classes_and_preserves_reimport_views() {
    for point in [
        "replacement.before_create",
        "replacement.after_create",
        "replacement.before_commit",
    ] {
        let s = Scratch::new("kaban-replacement", &point.replace('.', "-"));
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        let a = nid(&created(&store, "Fact", "zebra alpha").id);
        let b = nid(&created(&store, "Fact", "zebra beta").id);
        store
            .link(&a, &b, "RELATES_TO", None, None, &ctx())
            .unwrap();
        store.reindex_graph().unwrap();
        store.reindex_text().unwrap();
        let code = format!("code@{}", "9".repeat(40));
        catalog_testing::append_and_publish_into(&store, &pattern(3, 64), &[&code]).unwrap();
        let before = catalog_testing::graph_contents(&store).unwrap().extents;
        drop(store);
        std::fs::remove_file(s.0.join("kaban.kbx")).unwrap();
        let mut child = Supervised(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "replacement_role", "--ignored", "--nocapture"])
                .env(CRASH_DB, s.db())
                .env(catalog_testing::FAILPOINT_ARM, point)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        assert_eq!(child.wait(STEP).signal(), Some(9), "{point}");
        let rows = rusqlite::Connection::open(s.db()).unwrap();
        let pending: i64 = rows
            .query_row(
                "SELECT value FROM cat_meta WHERE key = 'rebuild_container'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
        assert_eq!(
            s.0.join("kaban.kbx").exists(),
            point != "replacement.before_create"
        );
        drop(rows);
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        assert_eq!(
            catalog_testing::graph_contents(&store).unwrap().extents,
            before
        );
        let hits = store
            .recall(&kaban_core::RecallQuery {
                query: "zebra".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 2, "{point}");
        let verified = store.verify_catalog().unwrap();
        assert_eq!(verified.needs_reimport, [code]);
        assert_eq!(verified.inconsistencies(), 0, "{point}: {verified:?}");
        let rows = rusqlite::Connection::open(s.db()).unwrap();
        let pending: i64 = rows
            .query_row(
                "SELECT count(*) FROM cat_meta WHERE key = 'rebuild_container'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
    }
}

#[test]
fn a_crash_after_the_append_of_a_publication_recovers_clean() {
    crash_at("publish.after_append");
}

#[test]
fn a_crash_before_the_commit_of_a_publication_recovers_clean() {
    crash_at("publish.before_commit");
}

#[test]
fn a_crash_in_the_middle_of_a_seal_recovers_clean() {
    crash_at("seal.mid");
}

#[test]
fn a_crash_in_the_middle_of_a_fold_recovers_clean() {
    crash_at("fold.mid");
}

#[test]
fn a_crash_after_the_floor_of_a_reclamation_recovers_clean() {
    crash_at("reclaim.after_floor");
}

#[test]
fn a_crash_between_the_punch_and_the_rows_of_a_reclamation_recovers_clean() {
    crash_at("reclaim.mid_punch");
}

#[test]
fn a_crash_in_the_middle_of_a_recovery_recovers_clean() {
    crash_at("recover.mid");
}

#[test]
fn a_reclamation_while_verify_hashes_leaves_no_mismatch() {
    // Arrange: one live extent; a second store of the database retires and reclaims it between
    // verify's read and its hashing
    let s = Scratch::new("kaban-t6-recover", "verify-race");
    let store = open_at(&s.db());
    let id = catalog_testing::append_and_publish(&store, &pattern(9, 8192)).unwrap();
    let other = open_at(&s.db());
    catalog_testing::set_failpoint(
        &store,
        Window::VerifyHashing,
        Some(std::sync::Arc::new(move || {
            catalog_testing::retire(&other, &[id]).unwrap();
            catalog_testing::reclaim(&other).unwrap();
        })),
    );

    // Act
    let report = store.verify_catalog().unwrap();

    // Assert: what verify read stayed whole while it hashed
    assert_eq!(report.hash_mismatches, Vec::<u64>::new(), "{report:?}");
}

#[test]
fn an_all_zero_unpublished_append_is_reported_and_reclaimed() {
    // Arrange: one published extent, then 64 KiB of zeros appended and never published
    let s = Scratch::new("kaban-t6-recover", "zero-leak");
    let store = open_at(&s.db());
    catalog_testing::append_and_publish(&store, &pattern(1, 64)).unwrap();
    catalog_testing::append_unpublished(&store, &vec![0; 64 * 1024]).unwrap();

    // Act
    let found = store.verify_catalog().unwrap();
    let recovered = store.recover_catalog().unwrap();
    let after = store.verify_catalog().unwrap();

    // Assert: the zeros are a leak like any other: reported, punched, then gone
    assert_eq!(found.unreferenced_ranges.len(), 1, "{found:?}");
    assert_eq!(recovered.ranges_reclaimed, found.unreferenced_ranges);
    assert_eq!(after.unreferenced_ranges, Vec::new(), "{after:?}");
}
