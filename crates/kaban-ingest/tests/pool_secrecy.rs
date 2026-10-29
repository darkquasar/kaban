//! Worker pool secrecy: through a full run, a path canary reaches neither the serialized report
//! nor a `trace`-level log capture, and the second run skips the blocked document through its
//! keyed identity (docs-ingest-plan.md DI0.3; core-spec N5; disposition D-6). Shared rig and
//! fake preparer: `common/pool.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::pool::{Fake, LogBuf, Mode, rig};
use kaban_core::{AuditStore, ChangeEntity, ChangeLogStore, MemoryStore, NodeId};
use kaban_ingest::IngestConfig;
use std::fs;
use tokio_util::sync::CancellationToken;

/// D-6 (moved from DI0.2): through the full run, a file named `notes-<CANARY>.md` is counted
/// `blocked` with its rule id in `rule_ids`, the serialized `IngestReport` and a `trace`-level
/// log capture carry no trace of the canary, and the second run skips the blocked document
/// through its keyed identity (no new write) while re-ingesting nothing else either (N5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_report_and_the_trace_log_never_carry_a_path_canary() {
    const CANARY: &str = "AKIAQWERTYUIOPASDFGH";
    let fake = Fake::new(Mode::Fast);
    let r = rig(
        "report-canary",
        fake,
        IngestConfig {
            workers: 2,
            queue_depth: 2,
            ..IngestConfig::default()
        },
    );
    fs::write(
        r.root.join(format!("notes-{CANARY}.md")),
        b"# nothing secret inside\n",
    )
    .unwrap();
    fs::write(r.root.join("clean.md"), b"# clean\n").unwrap();

    let logs = LogBuf::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

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
    assert_eq!(first.rule_ids.get("aws-access-key-id"), Some(&1));
    let json = serde_json::to_string(&first).unwrap();
    assert!(!json.contains(CANARY) && !json.contains("notes-"), "{json}");
    let blocked_id = {
        let changes = r.store.changes_read(0, 100_000).unwrap();
        let ids: Vec<NodeId> = changes
            .iter()
            .filter(|c| c.entity == ChangeEntity::Node)
            .filter_map(|c| NodeId::parse(&c.id).ok())
            .filter(|id| {
                r.store
                    .get(id, true)
                    .unwrap()
                    .is_some_and(|n| n.props["status"] == "blocked")
            })
            .collect();
        assert_eq!(ids.len(), 1, "one blocked Document in the ChangeLog");
        ids[0].clone()
    };
    let blocked_rev = r.store.get(&blocked_id, true).unwrap().unwrap().rev;
    let audit_after_first = r.store.audit_verify().unwrap().entries;

    // the fake mints a fresh Chunk id per run, so `clean.md` writes again (deterministic chunk
    // ids and the content skip arrive with DI1); the blocked document must not
    let second = r
        .ingestor
        .run(&r.cfg, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        second.blocked, 0,
        "the blocked document is skipped via its keyed id: {second:?}"
    );
    assert_eq!(second.skipped, 1);
    assert_eq!(
        r.store.get(&blocked_id, true).unwrap().unwrap().rev,
        blocked_rev,
        "no rewrite"
    );
    assert_eq!(
        r.store.audit_verify().unwrap().entries,
        audit_after_first,
        "no new audit row"
    );

    let log = logs.0.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&log);
    assert!(
        text.contains("ingestion run finished"),
        "trace capture saw the run: {text}"
    );
    assert!(
        !text.contains(CANARY) && !text.contains("notes-"),
        "canary in logs: {text}"
    );
}
