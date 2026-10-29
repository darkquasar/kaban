//! Worker pool bounds: the prepared queue, the live blocking tasks, the reorder window and two
//! concurrent runs never exceed the configured `queue_depth` / `workers` (docs-ingest-plan.md
//! DI0.3; core-spec F16, N4, N5; PR-8; RD-4, RD-5). Shared rig and fake preparer:
//! `common/pool.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::pool::{Fake, Mode, corpus, rig};
use kaban_ingest::IngestConfig;
use std::fs;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Plan slice 2: the prepared-document queue never holds more than `queue_depth` items
/// (instrumented gauge), even with fast workers and a committer slowed by real transactions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_never_exceeds_queue_depth() {
    let fake = Fake::new(Mode::Fast);
    let r = rig(
        "queue",
        fake,
        IngestConfig {
            workers: 4,
            queue_depth: 2,
            ..IngestConfig::default()
        },
    );
    corpus(&r.root, 60);
    let report = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.documents, 60);
    let m = r.ingestor.metrics();
    assert!(
        m.max_queued >= 1 && m.max_queued <= 2,
        "queue bound violated: {m:?}"
    );
    assert!(
        m.max_in_flight <= 2 + 4,
        "memory bound (queue_depth + workers) violated: {m:?}"
    );
}

/// Plan slice 6: the number of live blocking extraction tasks never exceeds `workers`
/// (the fake's own counter and the pool's agree).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_blocking_tasks_never_exceed_workers() {
    let fake = Fake::new(Mode::Jitter);
    let r = rig(
        "live",
        fake.clone(),
        IngestConfig {
            workers: 2,
            queue_depth: 8,
            ..IngestConfig::default()
        },
    );
    corpus(&r.root, 30);
    let report = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.documents, 30);
    let m = r.ingestor.metrics();
    assert!(
        m.max_live_blocking >= 1 && m.max_live_blocking <= 2,
        "{m:?}"
    );
    assert!(
        fake.max_live.load(Ordering::SeqCst) <= 2,
        "fake saw {} live",
        fake.max_live.load(Ordering::SeqCst)
    );
    assert_eq!(m.blocking_completed, 30);
}

/// Review RD-4 (F16): with the first document stalled, fast later workers must not pile their
/// prepared documents into the committer's reorder buffer without bound. Dispatched but not yet
/// committed documents (processing, queued and pending together) never exceed `queue_depth`,
/// and every document still commits once the stall is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_first_sequence_bounds_the_reorder_window_to_queue_depth() {
    let fake = Fake::new(Mode::NonCooperativeIf);
    let r = rig(
        "window",
        fake.clone(),
        IngestConfig {
            workers: 4,
            queue_depth: 2,
            doc_timeout: Duration::from_secs(30),
            ..IngestConfig::default()
        },
    );
    fs::write(r.root.join("a-slow.md"), b"# stalls first\n").unwrap();
    corpus(&r.root, 59); // doc-XX.md all sort after a-slow.md
    let run = tokio::time::timeout(
        Duration::from_secs(20),
        r.ingestor.run(&r.cfg, CancellationToken::new()),
    );
    let releaser = async {
        // give the fast workers time to run ahead of the stalled first sequence
        tokio::time::sleep(Duration::from_millis(500)).await;
        fake.release();
    };
    let (report, ()) = tokio::join!(run, releaser);
    let report = report.expect("hang guard").unwrap();
    assert_eq!(report.documents, 60, "{report:?}");
    let m = r.ingestor.metrics();
    assert!(
        m.max_uncommitted <= 2,
        "reorder window exceeded queue_depth: {m:?}"
    );
    assert!(m.max_uncommitted >= 2, "the window was used: {m:?}");
}

/// Review RD-5 (F16): the worker limit belongs to the `Ingestor`, not to one run. Two runs
/// started concurrently on the same `Ingestor` are serialized and never have more than
/// `workers` blocking extraction tasks alive between them; both complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_runs_never_exceed_workers_live_blocking_tasks() {
    let fake = Fake::new(Mode::Jitter);
    let r = rig(
        "concurrent",
        fake.clone(),
        IngestConfig {
            workers: 2,
            queue_depth: 8,
            ..IngestConfig::default()
        },
    );
    corpus(&r.root, 30);
    let (a, b) = tokio::join!(
        r.ingestor.run(&r.cfg, CancellationToken::new()),
        r.ingestor.run(&r.cfg, CancellationToken::new()),
    );
    assert_eq!((a.unwrap().documents, b.unwrap().documents), (30, 30));
    assert!(
        fake.max_live.load(Ordering::SeqCst) <= 2,
        "fake saw {} live blocking tasks across two concurrent runs",
        fake.max_live.load(Ordering::SeqCst)
    );
    assert_eq!(fake.completed.lock().unwrap().len(), 60);
}
