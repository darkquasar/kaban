//! `ChangeLog` semantics (core-spec F11; core-plan C2.4 with MD-12, MD-13, MD-24): one `changes`
//! row per node/edge/event write, fenced cursor advance, bounded pruning, `stats.changes_lag`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    Ack, Change, ChangeEntity, ChangeLogStore, ChangeOp, Governance, GovernanceStore, Lease,
    LeaseStore, MemoryStore, NewEvent, NewNode, NodeId, RememberOutcome, SignerKind, Sink, SinkId,
    SinkReport, run_sink,
};
use kaban_store_sqlite::SqliteStore;

/// `(entity, op)` of every changes row after `after`, in order.
fn ops_after(store: &SqliteStore, after: u64) -> Vec<(ChangeEntity, ChangeOp)> {
    store
        .changes_read(after, 100)
        .unwrap()
        .into_iter()
        .map(|c| (c.entity, c.op))
        .collect()
}

/// C2.4 slice 1 (F11, MD-24): every node, edge and event write produces exactly one `changes`
/// row, read back in `seq` order with the row's id and workspace; `remember` + two
/// non-mutating links = the node row plus, per link, the human claim's row, its status
/// (`pending -> winner`) and the edge (engine-spec EN-F8/EN-F9, E2.2: a link is a human
/// assertion projected to one edge) = 7 rows; accepting a Decision = its node update + its
/// event + the human `kind_of_record` claim's row and its status (EN-F12c, E2.3a) = 4 rows.
#[test]
fn every_write_produces_exactly_one_changes_row() {
    let store = SqliteStore::open_memory().unwrap();
    assert!(
        store.changes_read(0, 10).unwrap().is_empty(),
        "a fresh database has an empty ChangeLog"
    );
    let a = created(&store, "Fact", "cassowaries have a casque");
    let b = created(&store, "Idea", "cassowary mascot for the release");
    let last = store.changes_read(0, 10).unwrap().last().unwrap().seq;
    let decision = match store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: "cassowary builds ship weekly".into(),
                ..Default::default()
            },
            &[
                kaban_core::LinkReq {
                    rel: "RELATES_TO".into(),
                    to: Some(nid(&a.id)),
                    ..Default::default()
                },
                kaban_core::LinkReq {
                    rel: "ABOUT".into(),
                    to: Some(nid(&b.id)),
                    ..Default::default()
                },
            ],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { .. } => panic!(),
    };
    let rows = store.changes_read(last, 10).unwrap();
    assert_eq!(
        rows.iter().map(|c| (c.entity, c.op)).collect::<Vec<_>>(),
        vec![
            (ChangeEntity::Node, ChangeOp::Insert),
            (ChangeEntity::Assertion, ChangeOp::Insert),
            (ChangeEntity::Assertion, ChangeOp::Status),
            (ChangeEntity::Edge, ChangeOp::Insert),
            (ChangeEntity::Assertion, ChangeOp::Insert),
            (ChangeEntity::Assertion, ChangeOp::Status),
            (ChangeEntity::Edge, ChangeOp::Insert),
        ],
        "{rows:?}"
    );
    assert_eq!(rows[0].id, decision.id);
    assert!(rows.iter().all(|c| c.workspace_id == "default"));
    assert!(
        rows.windows(2).all(|w| w[0].seq < w[1].seq),
        "ordered by seq"
    );
    assert!(rows.iter().all(|c| c.ts > 0));
    let last = rows.last().unwrap().seq;

    // OWN-84 (1) (E2.5g): the EN-F12c record claim is a human acceptance's; an agent's or
    // the system's acceptance writes the node update and the event only
    let mut human = ctx();
    human.actor = kaban_core::Actor::HumanCli;
    store
        .transition(
            &NodeId::parse(&decision.id).unwrap(),
            Governance::Accepted,
            NewEvent {
                reason: "ok".into(),
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &human,
        )
        .unwrap();
    let rows = store.changes_read(last, 10).unwrap();
    assert_eq!(
        rows.iter().map(|c| (c.entity, c.op)).collect::<Vec<_>>(),
        vec![
            (ChangeEntity::Node, ChangeOp::Update),
            (ChangeEntity::Event, ChangeOp::Insert),
            (ChangeEntity::Assertion, ChangeOp::Insert),
            (ChangeEntity::Assertion, ChangeOp::Status),
        ],
        "a human acceptance is one node update, one event, the human record claim and its status: {rows:?}"
    );
    assert_eq!(rows[0].id, decision.id);
    // `limit` and `after` are honoured
    assert_eq!(store.changes_read(0, 2).unwrap().len(), 2);
    assert_eq!(
        store.changes_read(rows[3].seq, 10).unwrap(),
        vec![],
        "nothing after the newest row"
    );
    // a refreshed link (same src, dst, rel, another confidence) is a second human claim of
    // the triple (its row and its status), retires the prior action's winning status, and
    // produces one edge update row (RE4-10; E2.2).
    store
        .link(
            &nid(&decision.id),
            &nid(&a.id),
            "RELATES_TO",
            Some(0.5),
            None,
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        ops_after(&store, rows[3].seq),
        vec![
            (ChangeEntity::Assertion, ChangeOp::Insert),
            (ChangeEntity::Assertion, ChangeOp::Status),
            (ChangeEntity::Assertion, ChangeOp::Status),
            (ChangeEntity::Edge, ChangeOp::Update),
        ]
    );
    // MD-24: a purge is one explicit `purge` row plus one `delete` row per removed edge, and
    // (EN-F8a, E2.2) one `delete` row per claim naming the node (the `ABOUT` link's claim)
    let before = store.changes_read(0, 100).unwrap().last().unwrap().seq;
    let outcome = store
        .forget(
            &NodeId::parse(&b.id).unwrap(),
            kaban_core::ForgetMode::Purge,
            &ctx(),
        )
        .unwrap();
    let rows = store.changes_read(before, 10).unwrap();
    assert_eq!(
        rows.iter().map(|c| (c.entity, c.op)).collect::<Vec<_>>(),
        vec![
            (ChangeEntity::Edge, ChangeOp::Delete),
            (ChangeEntity::Node, ChangeOp::Purge),
            (ChangeEntity::Assertion, ChangeOp::Delete),
        ],
        "{rows:?}"
    );
    assert_eq!(rows[1].id, b.id);
    assert_eq!(
        outcome.outbox_seq, rows[2].seq,
        "ForgetOutcome points at the last row"
    );
}

/// MD-12: until C2.5's `lease_acquire` exists, the lease row is seeded with raw SQL.
fn seed_lease(db: &std::path::Path, name: &str, fence: i64) {
    let raw = rusqlite::Connection::open(db).unwrap();
    raw.execute(
        "INSERT INTO leases(name, owner, fence, expires_at) VALUES (?1, 'test', ?2, unixepoch() + 60)
         ON CONFLICT(name) DO UPDATE SET fence = excluded.fence",
        rusqlite::params![name, fence],
    )
    .unwrap();
}

fn cursor(db: &std::path::Path, name: &str) -> Option<i64> {
    let raw = rusqlite::Connection::open(db).unwrap();
    raw.query_row(
        "SELECT last_seq FROM projections WHERE name = ?",
        [name],
        |r| r.get(0),
    )
    .ok()
}

fn lag(store: &SqliteStore) -> i64 {
    store.stats(None).unwrap()["changes_lag"].as_i64().unwrap()
}

/// C2.4 slice 2 (F11, F15, MD-13): a consumer holding the current fence advances its cursor
/// (first ack registers the projection); `stats.changes_lag` is `max(seq) - min(last_seq)`
/// and 0 without consumers; cursors never move backwards.
#[test]
fn a_fenced_ack_advances_the_cursor_and_lag_follows() {
    let s = Scratch::new("kaban-changelog", "ack");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    for i in 0..5 {
        created(&store, "Fact", &format!("kookaburra fact {i}"));
    }
    let newest = store.changes_read(0, 10).unwrap().last().unwrap().seq;
    assert_eq!(newest, 5);
    assert_eq!(lag(&store), 0, "no consumer registered: no lag");

    seed_lease(&s.db(), "export", 1);
    store.changes_ack("export", 3, 1).unwrap();
    assert_eq!(cursor(&s.db(), "export"), Some(3));
    assert_eq!(lag(&store), 2);
    store.changes_ack("export", 5, 1).unwrap();
    assert_eq!(lag(&store), 0);
    // an older `upto` is a no-op, never a step back (MD-13)
    store.changes_ack("export", 2, 1).unwrap();
    assert_eq!(cursor(&s.db(), "export"), Some(5));
    // the slowest consumer defines the lag
    seed_lease(&s.db(), "vectors", 1);
    store.changes_ack("vectors", 1, 1).unwrap();
    assert_eq!(lag(&store), 4);
}

/// C2.4 slice 2 negative (F15 SR-2, MD-13): a stale fence (a successor took the lease) and a
/// consumer without any lease are both `LeaseLost`; nothing moves.
#[test]
fn an_ack_with_a_stale_or_missing_fence_is_rejected() {
    let s = Scratch::new("kaban-changelog", "stale");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    created(&store, "Fact", "lyrebirds imitate chainsaws");
    created(&store, "Fact", "lyrebirds imitate camera shutters");
    seed_lease(&s.db(), "export", 1);
    store.changes_ack("export", 1, 1).unwrap();
    // a successor acquired the lease: fence 2
    seed_lease(&s.db(), "export", 2);
    let err = store.changes_ack("export", 2, 1).unwrap_err();
    assert!(
        matches!(&err, kaban_core::StoreError::LeaseLost { name, fence: 1 } if name == "export"),
        "{err:?}"
    );
    assert_eq!(
        cursor(&s.db(), "export"),
        Some(1),
        "the stale holder moved nothing"
    );
    store.changes_ack("export", 2, 2).unwrap();
    assert_eq!(cursor(&s.db(), "export"), Some(2));
    // a consumer that never acquired a lease cannot register a cursor either
    assert!(matches!(
        store.changes_ack("nobody", 1, 0),
        Err(kaban_core::StoreError::LeaseLost { .. })
    ));
    assert_eq!(cursor(&s.db(), "nobody"), None);
}

fn seqs(store: &SqliteStore) -> Vec<u64> {
    store
        .changes_read(0, 1000)
        .unwrap()
        .into_iter()
        .map(|c| c.seq)
        .collect()
}

/// C2.4 slice 3 (F11 "pruning below min cursor with bounded tail"): prune deletes
/// `seq < min(last_seq) - tail` (the plan's SQL, applied uniformly) and never a row at or above
/// the slowest cursor minus the tail; without consumers the newest row stands in for the
/// cursor, so at least the newest `tail` rows survive; the newest row is never deleted, so
/// `seq` never restarts below a cursor (`changes.seq` has no AUTOINCREMENT by design, F17).
#[test]
fn prune_never_deletes_above_the_slowest_cursor_minus_tail() {
    let s = Scratch::new("kaban-changelog", "prune");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    for i in 0..10 {
        created(&store, "Fact", &format!("magpie fact {i}"));
    }
    assert_eq!(seqs(&store), (1..=10).collect::<Vec<_>>());

    // no consumers: the newest row (10) is the floor; rows below 10 - 7 go
    assert_eq!(store.changes_prune(7).unwrap(), 2);
    assert_eq!(seqs(&store), (3..=10).collect::<Vec<_>>());

    seed_lease(&s.db(), "export", 1);
    seed_lease(&s.db(), "vectors", 1);
    store.changes_ack("export", 10, 1).unwrap();
    store.changes_ack("vectors", 8, 1).unwrap();
    // slowest cursor 8, tail 2: rows below 6 go, 6.. stay
    assert_eq!(store.changes_prune(2).unwrap(), 3);
    assert_eq!(seqs(&store), (6..=10).collect::<Vec<_>>());
    // nothing more to prune at the same cursors
    assert_eq!(store.changes_prune(2).unwrap(), 0);
    // tail 0 with every cursor at the newest row: the newest row itself survives
    store.changes_ack("vectors", 10, 1).unwrap();
    assert_eq!(store.changes_prune(0).unwrap(), 4);
    assert_eq!(seqs(&store), vec![10]);
    // and the next write continues the sequence instead of restarting below the cursors
    created(&store, "Fact", "magpies swoop in spring");
    assert_eq!(seqs(&store), vec![10, 11]);
    assert_eq!(lag(&store), 1);
}

/// The database clock decides expiry (`leases.rs`): move the row into the past.
fn expire(db: &std::path::Path, name: &str) {
    let raw = rusqlite::Connection::open(db).unwrap();
    raw.execute(
        "UPDATE leases SET expires_at = unixepoch() - 1 WHERE name = ?1",
        [name],
    )
    .unwrap();
}

/// A sink that records the seqs it is handed and acks the last one; when `takeover` is set it
/// also lets the lease expire and acquires it as a second owner inside `consume` (between the
/// runner's read and its ack), as a crashed holder's successor would.
struct ProbeSink<'a> {
    store: &'a SqliteStore,
    db: std::path::PathBuf,
    takeover: bool,
    successor: Option<Lease>,
    seen: Vec<u64>,
}

impl Sink for ProbeSink<'_> {
    fn id(&self) -> SinkId {
        SinkId::EXPORT
    }
    fn consume(&mut self, changes: &[Change]) -> kaban_core::Result<Ack> {
        self.seen.extend(changes.iter().map(|c| c.seq));
        if self.takeover {
            expire(&self.db, "export");
            self.successor = self
                .store
                .lease_acquire("export", "host:2:new", 30)
                .unwrap();
        }
        Ok(Ack {
            upto: changes.last().unwrap().seq,
        })
    }
}

/// E2.0 item 4 (RP-6, SD-10; EN-E10a idempotency): the `export` holder reads a page, a second
/// owner takes the expired lease before the ack (fence 1 -> 2); `changes_ack` under the stale
/// fence is `LeaseLost`, `run_sink` reports `lease_lost: true` with `acked: None`, the
/// `projections` cursor row is unchanged; a rerun under the new fence re-consumes the same rows,
/// acks them, and the store state is identical (the sink writes nothing).
#[test]
fn a_lease_lost_between_the_read_and_the_ack_leaves_the_cursor_and_a_rerun_re_consumes() {
    // Arrange
    let s = Scratch::new("kaban-changelog", "lease-loss");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    for text in ["one", "two", "three"] {
        created(&store, "Fact", text);
    }
    let rows_before = store.changes_read(0, 100).unwrap();
    let last_seq = rows_before.last().unwrap().seq;
    let stats_before = store.stats(None).unwrap();
    let old = store
        .lease_acquire("export", "host:1:old", 30)
        .unwrap()
        .unwrap();
    assert_eq!(old.fence, 1);
    store
        .changes_ack("export", rows_before[0].seq, old.fence)
        .unwrap();
    let cursor_before = cursor(&s.db(), "export").unwrap();
    let resume_from = u64::try_from(cursor_before).unwrap();
    let mut sink = ProbeSink {
        store: &store,
        db: s.db(),
        takeover: true,
        successor: None,
        seen: Vec::new(),
    };

    // Act
    let report = run_sink(&store, &mut sink, &old, resume_from, 100).unwrap();

    // Assert: the stale ack is refused and nothing moved
    assert_eq!(
        report,
        SinkReport {
            consumed: u64::try_from(rows_before.len()).unwrap() - 1,
            acked: None,
            lease_lost: true,
        }
    );
    assert_eq!(
        cursor(&s.db(), "export"),
        Some(cursor_before),
        "cursor row unchanged"
    );
    let err = store
        .changes_ack("export", last_seq, old.fence)
        .unwrap_err();
    assert!(
        matches!(err, kaban_core::StoreError::LeaseLost { fence: 1, .. }),
        "{err:?}"
    );
    let new = sink.successor.take().unwrap();
    assert_eq!((new.fence, new.owner.as_str()), (2, "host:2:new"));

    // the successor's rerun from the same cursor re-consumes the same rows and acks
    let mut rerun = ProbeSink {
        store: &store,
        db: s.db(),
        takeover: false,
        successor: None,
        seen: Vec::new(),
    };
    let again = run_sink(&store, &mut rerun, &new, resume_from, 100).unwrap();
    assert_eq!(
        rerun.seen, sink.seen,
        "identical rows re-consumed (EN-E10a)"
    );
    assert_eq!(again.acked, Some(last_seq));
    assert!(!again.lease_lost);
    assert_eq!(
        cursor(&s.db(), "export"),
        Some(i64::try_from(last_seq).unwrap())
    );
    assert_eq!(
        store.changes_read(0, 100).unwrap(),
        rows_before,
        "no row written"
    );
    let mut stats_after = store.stats(None).unwrap();
    // the cursor moved, so the lag did; every other counter is identical
    for key in ["changes_lag", "outbox_lag"] {
        stats_after[key] = stats_before[key].clone();
    }
    assert_eq!(stats_after, stats_before, "identical store state");
}
