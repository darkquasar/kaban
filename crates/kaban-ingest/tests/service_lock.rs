//! The service-level ingest lock (docs-ingest-plan.md §16 item 3 (a); DI2.0): every `Ingestor`
//! of one process that is bound to the same `(store, workspace)` shares one run lock, so two
//! ingestions of one workspace serialise whichever adapter built them, while ingestions of
//! different workspaces over the same store do not wait for each other. Shared rig and fake
//! preparer: `common/pool.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use common::pool::{Fake, Mode};
use kaban_api::{ApiConfig, KabanApi};
use kaban_core::WorkspaceId;
use kaban_ingest::{IngestConfig, Ingestor, WalkConfig};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// An `Ingestor` the way an adapter builds one: a fresh instance per call over the process's
/// store, bound to `workspace`.
fn ingestor(store: &Arc<SqliteStore>, fake: &Arc<Fake>, workspace: &str) -> Ingestor {
    let api = Arc::new(KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(workspace),
            ..ApiConfig::default()
        },
    ));
    let cfg = IngestConfig {
        workers: 1,
        queue_depth: 2,
        ..IngestConfig::default()
    };
    Ingestor::new(api, store.clone(), store.signer(), fake.clone(), cfg)
}

/// A root with one document; a name containing `slow` makes the `NonCooperativeIf` fake hold
/// the run until the test releases it.
fn root(name: &str, file: &str) -> (Scratch, WalkConfig) {
    let root = Scratch::new("kaban-ingest-service", name);
    fs::write(root.join(file), b"# one\n").unwrap();
    let cfg = WalkConfig::new(root.to_path_buf());
    (root, cfg)
}

/// Two `Ingestor`s over one store and one workspace (what two adapter calls of one process
/// build) run one at a time: while the first run is held inside its extractor, the second
/// waits instead of running alongside it, and completes once the first has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_ingestors_over_one_store_and_workspace_serialise() {
    // Arrange
    let fake = Fake::new(Mode::NonCooperativeIf);
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let first = ingestor(&store, &fake, "shared");
    let second = ingestor(&store, &fake, "shared");
    let (_a, cfg_a) = root("same-a", "slow.md");
    let (_b, cfg_b) = root("same-b", "quick.md");

    // Act: the first run is polled first and holds the lock inside the blocked extractor; the
    // second must still be waiting after 300 ms
    let held = first.run(&cfg_a, CancellationToken::new());
    let waiting = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let attempt = tokio::time::timeout(
            Duration::from_millis(300),
            second.run(&cfg_b, CancellationToken::new()),
        )
        .await;
        fake.release();
        attempt
    };
    let (first_report, attempt) = tokio::join!(held, waiting);

    // Assert
    assert!(
        attempt.is_err(),
        "the second ingestion of the same workspace must wait for the first: {attempt:?}"
    );
    let first_report = first_report.unwrap();
    assert_eq!(first_report.documents, 1, "{first_report:?}");
    // the lock is free again: the second run goes through and sees only its own document
    let second_report = second.run(&cfg_b, CancellationToken::new()).await.unwrap();
    assert_eq!(second_report.documents, 1, "{second_report:?}");
}

/// Two `Ingestor`s over one store bound to different workspaces do not block each other: the
/// second completes while the first is still held inside its extractor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingestions_of_different_workspaces_do_not_block_each_other() {
    // Arrange
    let fake = Fake::new(Mode::NonCooperativeIf);
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let first = ingestor(&store, &fake, "alpha");
    let second = ingestor(&store, &fake, "beta");
    let (_a, cfg_a) = root("other-a", "slow.md");
    let (_b, cfg_b) = root("other-b", "quick.md");

    // Act
    let held = first.run(&cfg_a, CancellationToken::new());
    let independent = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let attempt = tokio::time::timeout(
            Duration::from_secs(5),
            second.run(&cfg_b, CancellationToken::new()),
        )
        .await;
        fake.release();
        attempt
    };
    let (first_report, attempt) = tokio::join!(held, independent);

    // Assert
    let second_report = attempt
        .expect("a different workspace must not wait for the held run")
        .unwrap();
    assert_eq!(second_report.documents, 1, "{second_report:?}");
    assert_eq!(first_report.unwrap().documents, 1);
}

/// A run that is cancelled while it waits for the held lock returns `Cancelled` at once instead
/// of waiting for the holder (the existing cancellation is honoured during the wait, D-20).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_waiting_run_returns_cancelled_without_waiting_for_the_holder() {
    // Arrange
    let fake = Fake::new(Mode::NonCooperativeIf);
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let first = ingestor(&store, &fake, "shared");
    let second = ingestor(&store, &fake, "shared");
    let (_a, cfg_a) = root("cancel-a", "slow.md");
    let (_b, cfg_b) = root("cancel-b", "quick.md");
    let token = CancellationToken::new();

    // Act
    let held = first.run(&cfg_a, CancellationToken::new());
    let waiting = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let outcome =
            tokio::time::timeout(Duration::from_secs(5), second.run(&cfg_b, token.clone())).await;
        let waited = started.elapsed();
        fake.release();
        (outcome, waited)
    };
    let (first_report, (outcome, waited)) = tokio::join!(held, waiting);

    // Assert
    assert!(
        matches!(outcome, Ok(Err(kaban_ingest::IngestError::Cancelled))),
        "{outcome:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "returned before the holder: {waited:?}"
    );
    assert_eq!(first_report.unwrap().documents, 1);
}
