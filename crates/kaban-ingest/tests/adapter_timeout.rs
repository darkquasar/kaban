//! Review RD3-3 (docs-ingest-spec D8, PR-8; DI2.2a): the per-document deadline must survive the
//! adapter boundary. `Ingestor::run` returns at the deadline and leaves a non-cooperative parser
//! running on the blocking pool (PR-8); the adapter that built a private runtime around the run
//! must not wait for that thread when the runtime goes away. With the non-cooperative fake and a
//! short `doc_timeout`, the adapter call returns within `doc_timeout + grace` while the fake is
//! still blocked, reports the timeout, and a second call right after returns within the same
//! bound; the fake is released at the end. Shared rig and fake preparer: `common/pool.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::pool::{Fake, Mode, ingestor_over, rig};
use kaban_ingest::{IngestConfig, IngestError, IngestReport, Ingestor, WalkConfig, run_blocking};
use std::fs;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const DOC_TIMEOUT: Duration = Duration::from_millis(300);
/// Scheduler and walk slack over the deadline (R-5 margins).
const GRACE: Duration = Duration::from_secs(2);

/// The adapter boundary under test. The red (`di22a-red-rd3-3.log`) ran a local copy of the
/// base adapter shape (`crates/kaban/src/cli/ingest.rs:141` at `e83cb2a`: a private
/// current-thread runtime, `block_on`, the implicit drop that waits for every blocking task of
/// the runtime) and returned after the fake's 10 s hang guard, not at the deadline.
fn adapter(ingestor: &Ingestor, walk: &WalkConfig) -> Result<IngestReport, IngestError> {
    run_blocking(ingestor, walk, CancellationToken::new())
}

/// Two adapter calls, each over a fresh `Ingestor` on the same store (as the adapter builds
/// them), with one document whose fake parser blocks until released: both return within the
/// bound while the fake still blocks; each records one timeout.
#[test]
fn the_adapter_returns_at_the_document_timeout_while_the_parser_still_blocks() {
    let fake = Fake::new(Mode::NonCooperativeIf);
    let cfg = IngestConfig {
        workers: 1,
        queue_depth: 2,
        doc_timeout: DOC_TIMEOUT,
        ..IngestConfig::default()
    };
    let r = rig("adapter-timeout", fake.clone(), cfg.clone());
    fs::write(r.root.join("slow.md"), b"# slow\n").unwrap();
    // hang guard: a stuck adapter is released after 20 s so the test ends with a measurement
    let _watchdog = {
        let fake = fake.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(20));
            fake.release();
        })
    };
    let bound = DOC_TIMEOUT + GRACE;

    let t0 = Instant::now();
    let first = adapter(&r.ingestor, &r.cfg).unwrap();
    let first_elapsed = t0.elapsed();
    assert!(
        first_elapsed <= bound,
        "the first adapter call returned after {first_elapsed:?} (bound {bound:?}): {first:?}"
    );
    assert_eq!(
        (first.failed, first.timed_out, first.timed_out_outstanding),
        (1, 1, 1),
        "{first:?}"
    );
    assert!(
        fake.completed.lock().unwrap().is_empty(),
        "the fake must still be blocked when the adapter returns"
    );

    let second_ingestor = ingestor_over(&r.store, fake.clone(), cfg);
    let t1 = Instant::now();
    let second = adapter(&second_ingestor, &r.cfg).unwrap();
    let second_elapsed = t1.elapsed();
    assert!(
        second_elapsed <= bound,
        "the second adapter call returned after {second_elapsed:?} (bound {bound:?}): {second:?}"
    );
    assert_eq!(
        (
            second.failed,
            second.timed_out,
            second.timed_out_outstanding
        ),
        (1, 1, 1),
        "{second:?}"
    );
    assert!(
        fake.completed.lock().unwrap().is_empty(),
        "both fakes must still be blocked"
    );

    // released now that both bounds were observed: the abandoned tasks end on their own
    fake.release();
    let guard = Instant::now();
    while fake.completed.lock().unwrap().len() < 2 {
        assert!(
            guard.elapsed() < Duration::from_secs(5),
            "the blocked tasks never ended"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
