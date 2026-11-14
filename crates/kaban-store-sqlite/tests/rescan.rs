//! The policy rescan's freshness (core-spec S4; REC-08): rows carry no policy stamp, so the
//! change log decides what a run judges. A row another writer changes behind a running walk's
//! cursor is judged again before the run reports done, and a run after a complete walk judges
//! only the rows changed since.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::{
    Actor, IngestStore, Lifecycle, MemoryStore, NewNode, Node, NodeId, Op, RescanAction,
    RescanCursor, RescanReport, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;

fn ctx() -> WriteCtx {
    WriteCtx::new(WorkspaceId::default(), Actor::Agent)
}

fn upsert(store: &SqliteStore, id: &NodeId, text: &str) {
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::UpsertNode {
                    id: id.clone(),
                    node: NewNode {
                        kind: "Fact".into(),
                        text: text.into(),
                        ..Default::default()
                    },
                }],
            ),
            &ctx(),
        )
        .unwrap();
}

/// The policy under test: a node naming the codename is rejected.
fn judge(n: &Node) -> RescanAction {
    if n.text.contains("nightjar") {
        RescanAction::Reject
    } else {
        RescanAction::Keep
    }
}

/// Run batches of `batch` rows from `at` until done, summing the reports.
fn run_from(store: &SqliteStore, mut at: RescanCursor, batch: usize) -> RescanReport {
    let mut total = RescanReport::default();
    while at != RescanCursor::Done {
        let report = store.rescan("v2", at, batch, &mut judge).unwrap();
        at = report.cursor;
        total += report;
    }
    total
}

#[test]
fn an_update_behind_a_running_walks_cursor_is_judged_before_the_run_is_done() {
    // Arrange: three clean nodes
    let store = SqliteStore::open_memory().unwrap();
    let ids: Vec<NodeId> = (0..3).map(|_| NodeId::new()).collect();
    for (i, id) in ids.iter().enumerate() {
        upsert(&store, id, &format!("clean note {i}"));
    }

    // Act: the walk judges the first node, then another writer rewrites it behind the cursor
    let first = store
        .rescan("v2", RescanCursor::Start, 1, &mut judge)
        .unwrap();
    assert_eq!((first.scanned, first.kept), (1, 1), "{first:?}");
    assert!(matches!(first.cursor, RescanCursor::Nodes(_)), "{first:?}");
    upsert(&store, &ids[0], "now naming the nightjar codename");
    let rest = run_from(&store, first.cursor, 1);

    // Assert: the rewritten node was judged again and tombstoned (with one-entry batches the
    // log lags, so the tombstone write itself may be judged again: idempotent)
    assert!(rest.rejected >= 1, "{rest:?}");
    let hidden = store.get(&ids[0], true).unwrap().unwrap();
    assert_eq!(hidden.lifecycle, Lifecycle::Tombstoned);
    // the run's own writes are not judged again: a new run has nothing to do
    let idle = run_from(&store, RescanCursor::Start, 1);
    assert_eq!(idle.scanned, 0, "{idle:?}");
}

#[test]
fn a_run_after_a_complete_walk_judges_only_rows_changed_since() {
    // Arrange: a complete walk under v2
    let store = SqliteStore::open_memory().unwrap();
    for i in 0..4 {
        upsert(&store, &NodeId::new(), &format!("clean note {i}"));
    }
    let walk = run_from(&store, RescanCursor::Start, 100);
    assert_eq!(walk.scanned, 4, "{walk:?}");

    // Act: a writer under an older policy stores a node v2 rejects
    let stale = NodeId::new();
    upsert(&store, &stale, "the nightjar codename from an old process");
    let next = run_from(&store, RescanCursor::Start, 100);

    // Assert: only the changed row is judged, and it is hidden
    assert_eq!((next.scanned, next.rejected), (1, 1), "{next:?}");
    assert!(store.get(&stale, false).unwrap().is_none());
}

#[test]
fn a_changed_policy_walks_every_row_again() {
    let store = SqliteStore::open_memory().unwrap();
    for i in 0..3 {
        upsert(&store, &NodeId::new(), &format!("clean note {i}"));
    }
    assert_eq!(run_from(&store, RescanCursor::Start, 100).scanned, 3);
    let mut seen = 0;
    let mut at = RescanCursor::Start;
    while at != RescanCursor::Done {
        let report = store
            .rescan("v3", at, 2, &mut |_: &Node| {
                seen += 1;
                RescanAction::Keep
            })
            .unwrap();
        at = report.cursor;
    }
    assert_eq!(seen, 3, "a new policy version re-judges every node");
}
