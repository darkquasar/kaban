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
use kaban_core::MemoryStore;
use kaban_core::config::{CatalogCfg, SealOps};
use kaban_core::query_budget::QueryBudget;
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
