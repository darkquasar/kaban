//! Worker pool cancellation and timeouts: a cancelled run leaves a gap-free committed prefix,
//! cooperative and non-cooperative slow extractors are recorded as timeouts, and later runs on
//! the same `Ingestor` see run-local counters and the remaining worker capacity
//! (docs-ingest-plan.md DI0.3; core-spec F16, N4, N5; PR-7, PR-8; dispositions D-13, D-20,
//! D-24, R-5; RD-5). Timing slices use small configured deadlines with generous margins and
//! must run with nothing else building (R-5). Shared rig and fake preparer: `common/pool.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::pool::{Fake, Mode, committed_flags, corpus, rig, scratch, source_of};
use kaban_core::{ChangeEntity, ChangeLogStore, MemoryStore};
use kaban_ingest::{IngestConfig, WalkConfig, document_id};
use std::fs;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Plan slice 3 with D-24: cancelling the token once at least 10 documents are committed stops
/// the run with `cancelled: true`; the committed set is a gap-free prefix of the sorted walk
/// (length >= 10, < 50) and every committed document is whole (its Chunk and `PART_OF` edge
/// landed in the same transaction).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_leaves_exactly_a_gap_free_prefix_committed() {
    let fake = Fake::new(Mode::Alternate);
    let r = rig(
        "cancel",
        fake.clone(),
        IngestConfig {
            workers: 2,
            queue_depth: 2,
            ..IngestConfig::default()
        },
    );
    let sorted = corpus(&r.root, 50);
    let cancel = CancellationToken::new();
    let watcher = {
        let store = r.store.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let docs = store
                    .changes_read(0, 100_000)
                    .unwrap()
                    .iter()
                    .filter(|c| c.entity == ChangeEntity::Node)
                    .count();
                if docs >= 20 {
                    // 20 node rows = 10 Documents + 10 Chunks
                    cancel.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };
    let report = r.ingestor.run(&r.cfg, cancel.clone()).await.unwrap();
    watcher.await.unwrap();
    assert!(report.cancelled, "{report:?}");
    let flags = committed_flags(&r.store, &sorted);
    let committed = flags.iter().take_while(|f| **f).count();
    assert!(
        (10..50).contains(&committed),
        "prefix length {committed}: {flags:?}"
    );
    assert!(
        flags[committed..].iter().all(|f| !f),
        "gap-free prefix expected: {flags:?}"
    );
    assert_eq!(report.documents as usize, committed);
    // dispatch stopped: at most the in-flight workers and the queued items were prepared
    // beyond the committed prefix
    let prepared = fake.completed.lock().unwrap().len();
    assert!(
        prepared <= committed + 2 + 2,
        "dispatch kept going: prepared {prepared}, committed {committed}"
    );
    // whole documents only: every committed Document has its chunk edge
    for rel in &sorted[..committed] {
        let doc = document_id(&source_of(rel));
        let t = r
            .store
            .traverse(&kaban_core::TraverseQuery {
                start_ids: vec![doc.clone()],
                rels: vec!["PART_OF".into()],
                direction: kaban_core::TraverseDirection::In,
                max_depth: 1,
                node_kinds: vec![],
                governance: vec![],
                limit: 10,
                workspace_id: None,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t.edges.len(), 1, "document {rel} is missing its chunk edge");
    }
    // a token cancelled before the run starts is `Err(Cancelled)` (D-20)
    let pre = CancellationToken::new();
    pre.cancel();
    assert!(matches!(
        r.ingestor.run(&r.cfg, pre).await,
        Err(kaban_ingest::IngestError::Cancelled)
    ));
}

/// Plan slice 4 (R-5: 300 ms configured deadline, generous margins): a cooperative slow
/// extractor (100 ms steps calling `check()`, 5 s if never stopped) is recorded
/// `Document{status: failed, error: "timeout"}`, counted in `timed_out`, and its blocking task
/// stops cooperatively: synchronized on the fake's own start and completion stamps, it ran for
/// well under the 5 s it would take uncancelled (a scheduler-tolerant bound instead of the
/// former 200 ms margin).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cooperative_slow_extractor_is_recorded_as_timeout_and_stops_promptly() {
    let fake = Fake::new(Mode::CooperativeSlow);
    let r = rig(
        "coop",
        fake.clone(),
        IngestConfig {
            workers: 2,
            queue_depth: 2,
            doc_timeout: Duration::from_millis(300),
            ..IngestConfig::default()
        },
    );
    fs::write(r.root.join("slow.md"), b"# slow\n").unwrap();
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        r.ingestor.run(&r.cfg, CancellationToken::new()),
    )
    .await
    .expect("hang guard")
    .unwrap();
    assert_eq!(
        (report.documents, report.failed, report.timed_out),
        (0, 1, 1),
        "{report:?}"
    );
    let node = r
        .store
        .get(&document_id(&source_of("slow.md")), true)
        .unwrap()
        .expect("failed Document recorded");
    assert_eq!(node.props["status"], "failed");
    assert_eq!(node.props["error"], "timeout");

    // the blocking task ends on its own, shortly after the deadline: wait for its completion
    // stamp (generous guard), then compare with its start stamp
    let guard = Instant::now();
    let ended = loop {
        if let Some(t) = fake.completed.lock().unwrap().first().copied() {
            break t;
        }
        assert!(
            guard.elapsed() < Duration::from_secs(6),
            "blocking task never ended"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let task_started = fake.started.lock().unwrap()[0];
    let ran_for = ended.saturating_duration_since(task_started);
    assert!(
        ran_for < Duration::from_secs(2),
        "the task did not stop cooperatively: ran for {ran_for:?} (5 s if uncancelled)"
    );
    assert_eq!(r.ingestor.metrics().blocking_completed, 1);
}

/// Plan slice 5 with D-13 (R-5: 300 ms deadline): two non-cooperative fakes (blocked on the
/// release barrier, ignoring `check()`) hold both permits of a `workers = 2` pool; the third
/// document waits (never dispatched); when both time out the run returns `WorkersExhausted {
/// outstanding: 2 }` whose partial report carries the two `failed:"timeout"` documents, the fast
/// document committed before them, and nothing more. No third blocking task was ever started.
/// The barrier is released only after exhaustion was observed; one outer timeout guards hangs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_cooperative_extractors_keep_their_permits_and_exhaust_the_pool() {
    let fake = Fake::new(Mode::NonCooperativeIf);
    let r = rig(
        "exhaust",
        fake.clone(),
        IngestConfig {
            workers: 2,
            queue_depth: 4,
            doc_timeout: Duration::from_millis(300),
            ..IngestConfig::default()
        },
    );
    // sorted order: a-fast.md, b-slow.md, c-slow.md, d-waits.md
    for name in ["a-fast.md", "b-slow.md", "c-slow.md", "d-waits.md"] {
        fs::write(r.root.join(name), format!("# {name}\n")).unwrap();
    }
    let err = tokio::time::timeout(
        Duration::from_secs(10),
        r.ingestor.run(&r.cfg, CancellationToken::new()),
    )
    .await
    .expect("hang guard: the run must return while the non-cooperative tasks still block")
    .expect_err("the pool is exhausted");
    let partial = match err {
        kaban_ingest::IngestError::WorkersExhausted {
            outstanding,
            partial,
        } => {
            assert_eq!(outstanding, 2, "both permits still held");
            partial
        }
        other => panic!("expected WorkersExhausted, got {other}"),
    };
    assert_eq!(
        (partial.documents, partial.failed, partial.timed_out),
        (1, 2, 2),
        "{partial:?}"
    );
    assert!(
        r.store
            .get(&document_id(&source_of("a-fast.md")), true)
            .unwrap()
            .is_some()
    );
    for slow in ["b-slow.md", "c-slow.md"] {
        let node = r
            .store
            .get(&document_id(&source_of(slow)), true)
            .unwrap()
            .expect(slow);
        assert_eq!(node.props["error"], "timeout");
    }
    assert!(
        r.store
            .get(&document_id(&source_of("d-waits.md")), true)
            .unwrap()
            .is_none(),
        "d was never dispatched"
    );
    assert_eq!(
        fake.completed.lock().unwrap().len(),
        1,
        "only the fast fake has returned so far: the other two are still blocked"
    );
    assert!(r.ingestor.metrics().max_live_blocking <= 2);

    // released now that exhaustion was observed: the tasks end and give their permits back
    fake.release();
    let guard = Instant::now();
    while fake.completed.lock().unwrap().len() < 3 {
        assert!(
            guard.elapsed() < Duration::from_secs(5),
            "blocked tasks never ended"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Review RD-5 (D8, PR-8): report counters and `PoolMetrics` are run-local. A run after a run
/// that timed out one document (whose non-cooperative task is still blocked) reports
/// `timed_out: 0` and metrics for its own documents only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_after_a_timed_out_run_reports_only_its_own_timeouts() {
    let fake = Fake::new(Mode::NonCooperativeIf);
    let r = rig(
        "timeout-then-clean",
        fake.clone(),
        IngestConfig {
            workers: 2,
            queue_depth: 4,
            doc_timeout: Duration::from_millis(300),
            ..IngestConfig::default()
        },
    );
    fs::write(r.root.join("slow.md"), b"# slow\n").unwrap();
    let first = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!((first.failed, first.timed_out), (1, 1), "{first:?}");

    let clean_root = scratch("timeout-then-clean-b");
    fs::write(clean_root.join("fast.md"), b"# fast\n").unwrap();
    let second = r
        .ingestor
        .run(
            &WalkConfig::new(clean_root.to_path_buf()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        (second.documents, second.failed, second.timed_out),
        (1, 0, 0),
        "the second run must not inherit the first run's timeout: {second:?}"
    );
    let m = r.ingestor.metrics();
    assert_eq!(
        (m.max_live_blocking, m.blocking_completed),
        (1, 1),
        "metrics are run-local: {m:?}"
    );

    fake.release();
    let guard = Instant::now();
    while fake.completed.lock().unwrap().len() < 2 {
        assert!(
            guard.elapsed() < Duration::from_secs(5),
            "blocked task never ended"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Review RD-5 (F16): a run cancelled while two non-cooperative tasks hold both permits leaves
/// those tasks running; the next run on the same `Ingestor` waits for the remaining capacity
/// (no task of its own starts until the blocked ones return) and then completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_after_cancellation_respects_the_remaining_worker_capacity() {
    let fake = Fake::new(Mode::NonCooperativeIf);
    let r = rig(
        "cancel-then-rerun",
        fake.clone(),
        IngestConfig {
            workers: 2,
            queue_depth: 4,
            doc_timeout: Duration::from_secs(30),
            ..IngestConfig::default()
        },
    );
    for name in ["a-slow.md", "b-slow.md"] {
        fs::write(r.root.join(name), format!("# {name}\n")).unwrap();
    }
    let cancel = CancellationToken::new();
    let canceller = {
        let fake = fake.clone();
        let cancel = cancel.clone();
        async move {
            let guard = Instant::now();
            while fake.started.lock().unwrap().len() < 2 {
                assert!(
                    guard.elapsed() < Duration::from_secs(5),
                    "tasks never started"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            cancel.cancel();
        }
    };
    let (first, ()) = tokio::join!(r.ingestor.run(&r.cfg, cancel), canceller);
    let first = first.unwrap();
    assert!(first.cancelled && first.documents == 0, "{first:?}");

    let next_root = scratch("cancel-then-rerun-b");
    corpus(&next_root, 3);
    let observer = async {
        // both permits are still held by the blocked tasks: nothing of run 2 may start
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = fake.started.lock().unwrap().len();
        fake.release();
        started
    };
    let next_cfg = WalkConfig::new(next_root.to_path_buf());
    let (second, started_before_release) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(20),
            r.ingestor.run(&next_cfg, CancellationToken::new())
        ),
        observer
    );
    assert_eq!(
        started_before_release, 2,
        "run 2 started tasks while run 1's tasks still held every permit"
    );
    let second = second.expect("hang guard").unwrap();
    assert_eq!(second.documents, 3, "{second:?}");
    assert!(
        fake.max_live.load(Ordering::SeqCst) <= 2,
        "fake saw {} live",
        fake.max_live.load(Ordering::SeqCst)
    );
}
