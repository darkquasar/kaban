//! Worker pool: documents commit in sorted path order and walker skips or already-blocked paths
//! never stall the single committer (docs-ingest-plan.md DI0.3; core-spec F16, N4; PR-7; RD-3).
//! Shared rig and fake preparer: `common/pool.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::pool::{Fake, Mode, corpus, rig, source_of};
use kaban_core::{ChangeEntity, ChangeLogStore, NodeId};
use kaban_ingest::{IngestConfig, document_id};
use std::fs;
use tokio_util::sync::CancellationToken;

/// Plan slice 1: 50 documents commit in sorted path order regardless of worker completion
/// order: the `ChangeLog` `seq` of each Document insert increases with the path order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifty_documents_commit_in_sorted_path_order() {
    let fake = Fake::new(Mode::Jitter);
    let r = rig(
        "order",
        fake.clone(),
        IngestConfig {
            workers: 4,
            queue_depth: 4,
            ..IngestConfig::default()
        },
    );
    let sorted = corpus(&r.root, 50);
    let report = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.documents, 50, "{report:?}");
    assert!(!report.cancelled && report.failed == 0 && report.skipped == 0);

    let changes = r.store.changes_read(0, 100_000).unwrap();
    let seq_of = |id: &NodeId| -> u64 {
        changes
            .iter()
            .find(|c| c.entity == ChangeEntity::Node && c.id == id.as_str())
            .map_or_else(|| panic!("no changes row for {id}"), |c| c.seq)
    };
    let seqs: Vec<u64> = sorted
        .iter()
        .map(|rel| seq_of(&document_id(&source_of(rel))))
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "outbox seq must follow path order: {seqs:?}"
    );
    assert_eq!(report.outbox_rows, changes.len() as u64);
    assert_eq!(fake.completed.lock().unwrap().len(), 50);
}

/// Review RD-3: a walker skip takes no commit sequence number, so ordinary skips before and
/// between clean documents (a denied `.env`, an unsupported `b.xyz`, a code file) leave the
/// committer a dense sequence and every clean document commits, in path order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walker_skips_before_and_between_documents_do_not_stall_the_committer() {
    let fake = Fake::new(Mode::Fast);
    let r = rig(
        "skips",
        fake,
        IngestConfig {
            workers: 2,
            queue_depth: 2,
            ..IngestConfig::default()
        },
    );
    // sorted: .env (Denied), a.md, b.xyz (Unsupported), c.md, d/.env (Denied), e.md, f.rs (Code)
    for (rel, body) in [
        (".env", "TOKEN=x\n"),
        ("a.md", "# a\n"),
        ("b.xyz", "???\n"),
        ("c.md", "# c\n"),
        ("d/.env", "TOKEN=y\n"),
        ("e.md", "# e\n"),
        ("f.rs", "fn main() {}\n"),
    ] {
        let p = r.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    let report = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        (report.documents, report.skipped, report.failed),
        (3, 4, 0),
        "{report:?}"
    );
    let changes = r.store.changes_read(0, 100_000).unwrap();
    let seqs: Vec<u64> = ["a.md", "c.md", "e.md"]
        .iter()
        .map(|rel| {
            let id = document_id(&source_of(rel));
            changes
                .iter()
                .find(|c| c.entity == ChangeEntity::Node && c.id == id.as_str())
                .map_or_else(|| panic!("{rel} was not committed"), |c| c.seq)
        })
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "path order: {seqs:?}");
}

/// Review RD-3: a path-rejected document that is already stored sorts before a clean document;
/// on the second run it is skipped through its keyed id and takes no sequence number, so the
/// clean document behind it still commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_already_blocked_path_before_a_clean_document_does_not_stall_the_second_run() {
    const CANARY: &str = "AKIAQWERTYUIOPASDFGH";
    let fake = Fake::new(Mode::Fast);
    let r = rig(
        "blocked-first",
        fake,
        IngestConfig {
            workers: 2,
            queue_depth: 2,
            ..IngestConfig::default()
        },
    );
    fs::write(r.root.join(format!("a-notes-{CANARY}.md")), b"# a\n").unwrap();
    fs::write(r.root.join("b-clean.md"), b"# b\n").unwrap();
    let first = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        (first.documents, first.blocked, first.skipped),
        (1, 1, 0),
        "{first:?}"
    );
    let second = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        (second.documents, second.blocked, second.skipped),
        (1, 0, 1),
        "the clean document behind the skipped blocked one must commit: {second:?}"
    );
    // the fake mints a fresh Chunk per run, so the clean document's transaction wrote rows
    assert!(second.outbox_rows > 0, "{second:?}");
}
