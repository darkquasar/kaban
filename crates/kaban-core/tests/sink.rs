//! The `Sink` seam over the `ChangeLog` (engine-spec EN-E10b, EN-F15; engine-plan E2.0 item 2;
//! correction RP-6, SD-10), on an in-memory fake `ChangeLogStore` (no SQLite in core tests):
//! two sinks over the same change sequence produce identical acks; an ack advances only the
//! acking sink's cursor; two sinks on one log do not share cursors; a stopped sink resumes from
//! its ack; a lost lease is reported, never retried, and the cursor does not move.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::{
    Ack, Change, ChangeEntity, ChangeLogStore, ChangeOp, Lease, Result, Sink, SinkId, SinkReport,
    StoreError, run_sink,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// An in-memory `ChangeLog`: rows, one cursor per consumer, one fence per lease name. The ack
/// rule is the store's (MD-13): the cursor moves to `max(current, upto)` only when a lease
/// named `consumer` holds `fence`, else `LeaseLost`.
#[derive(Default)]
struct FakeLog {
    rows: Vec<Change>,
    cursors: Mutex<BTreeMap<String, u64>>,
    fences: Mutex<BTreeMap<String, u64>>,
}

impl FakeLog {
    fn with_rows(n: u64) -> Self {
        let rows = (1..=n)
            .map(|seq| Change {
                seq,
                entity: ChangeEntity::Node,
                op: ChangeOp::Insert,
                id: format!("node-{seq}"),
                workspace_id: "default".into(),
                ts: 0,
                paths: None,
            })
            .collect();
        Self {
            rows,
            ..Self::default()
        }
    }
    /// Acquire `name` with the next fence (the C2.5 acquisition rule, without expiry).
    fn acquire(&self, name: &str) -> Lease {
        let mut fences = self.fences.lock().unwrap();
        let fence = fences.entry(name.to_owned()).or_insert(0);
        *fence += 1;
        Lease {
            name: name.to_owned(),
            owner: format!("owner-{fence}"),
            fence: *fence,
            expires_at: i64::MAX,
        }
    }
    fn cursor(&self, name: &str) -> Option<u64> {
        self.cursors.lock().unwrap().get(name).copied()
    }
}

impl ChangeLogStore for FakeLog {
    fn changes_head(&self) -> Result<u64> {
        Ok(self.rows.last().map_or(0, |c| c.seq))
    }
    fn changes_read(&self, after: u64, limit: usize) -> Result<Vec<Change>> {
        Ok(self
            .rows
            .iter()
            .filter(|c| c.seq > after)
            .take(limit)
            .cloned()
            .collect())
    }
    fn changes_ack(&self, consumer: &str, upto: u64, fence: u64) -> Result<()> {
        let held = self.fences.lock().unwrap().get(consumer).copied() == Some(fence);
        if !held {
            return Err(StoreError::LeaseLost {
                name: consumer.to_owned(),
                fence,
            });
        }
        let mut cursors = self.cursors.lock().unwrap();
        let cursor = cursors.entry(consumer.to_owned()).or_insert(0);
        *cursor = (*cursor).max(upto);
        Ok(())
    }
    fn changes_register(&self, consumer: &str, fence: u64) -> Result<u64> {
        let held = self.fences.lock().unwrap().get(consumer).copied() == Some(fence);
        if !held {
            return Err(StoreError::LeaseLost {
                name: consumer.to_owned(),
                fence,
            });
        }
        let newest = self.rows.last().map_or(0, |c| c.seq);
        Ok(*self
            .cursors
            .lock()
            .unwrap()
            .entry(consumer.to_owned())
            .or_insert(newest))
    }
    fn changes_lag_by_consumer(&self) -> Result<BTreeMap<String, u64>> {
        let newest = self.rows.last().map_or(0, |c| c.seq);
        Ok(self
            .cursors
            .lock()
            .unwrap()
            .iter()
            .map(|(name, cursor)| (name.clone(), newest.saturating_sub(*cursor)))
            .collect())
    }
    fn changes_prune(&self, _tail: u64) -> Result<u64> {
        Ok(0)
    }
}

/// A sink that records the seqs it was handed and acks the last one (the default sink's rule).
struct Recording {
    id: SinkId,
    seen: Vec<u64>,
}

impl Recording {
    fn new(id: SinkId) -> Self {
        Self {
            id,
            seen: Vec::new(),
        }
    }
}

impl Sink for Recording {
    fn id(&self) -> SinkId {
        self.id
    }
    fn consume(&mut self, changes: &[Change]) -> Result<Ack> {
        assert!(!changes.is_empty(), "the runner never hands an empty page");
        self.seen.extend(changes.iter().map(|c| c.seq));
        Ok(Ack {
            upto: changes.last().unwrap().seq,
        })
    }
}

/// A no-op sink: looks at nothing, acks the last seq of the page.
struct Noop(SinkId);

impl Sink for Noop {
    fn id(&self) -> SinkId {
        self.0
    }
    fn consume(&mut self, changes: &[Change]) -> Result<Ack> {
        Ok(Ack {
            upto: changes.last().map_or(0, |c| c.seq),
        })
    }
}

/// Takes the lease over inside `consume`, as a crashed holder's successor would.
struct Takeover<'a> {
    log: &'a FakeLog,
    successor: Option<Lease>,
    seen: Vec<u64>,
}

impl Sink for Takeover<'_> {
    fn id(&self) -> SinkId {
        SinkId::EXPORT
    }
    fn consume(&mut self, changes: &[Change]) -> Result<Ack> {
        self.seen.extend(changes.iter().map(|c| c.seq));
        self.successor = Some(self.log.acquire("export"));
        Ok(Ack {
            upto: changes.last().unwrap().seq,
        })
    }
}

/// A sink whose own work fails.
struct Failing;

impl Sink for Failing {
    fn id(&self) -> SinkId {
        SinkId::EXPORT
    }
    fn consume(&mut self, _changes: &[Change]) -> Result<Ack> {
        Err(StoreError::BadLink)
    }
}

/// Run `sink` over the whole log from `from` in pages of `limit`, returning the acks.
fn drain(log: &FakeLog, sink: &mut dyn Sink, lease: &Lease, from: u64, limit: usize) -> Vec<u64> {
    let mut acks = Vec::new();
    let mut after = from;
    loop {
        let report = run_sink(log, sink, lease, after, limit).unwrap();
        assert!(!report.lease_lost, "{report:?}");
        let Some(acked) = report.acked else {
            break;
        };
        acks.push(acked);
        after = acked;
    }
    acks
}

/// EN-E10b `Verify:` the trait-object test: a recording sink receives every row and both it
/// and a no-op sink produce identical acks over the same change sequence (the acks are the
/// runner's positions, not the sink's work).
#[test]
fn a_recording_sink_and_a_no_op_sink_produce_identical_acks_over_one_sequence() {
    // Arrange
    let log = FakeLog::with_rows(7);
    let lease = log.acquire("export");
    let mut recording = Recording::new(SinkId::EXPORT);
    let mut noop = Noop(SinkId::EXPORT);

    // Act
    let acks_recording = drain(&log, &mut recording, &lease, 0, 3);
    let acks_noop = drain(&log, &mut noop, &lease, 0, 3);

    // Assert
    assert_eq!(recording.seen, (1..=7).collect::<Vec<_>>());
    assert_eq!(acks_recording, [3, 6, 7]);
    assert_eq!(acks_recording, acks_noop);
    assert_eq!(log.cursor("export"), Some(7));
}

/// An ack advances only the acking sink's cursor; two sinks on one log do not share cursors
/// (each under its own lease name), and the rows one consumes are invisible to the other's
/// position.
#[test]
fn acks_advance_only_the_acking_sinks_cursor_and_two_sinks_do_not_share_cursors() {
    // Arrange
    let log = FakeLog::with_rows(5);
    let export = log.acquire("export");
    let reconcile = log.acquire("reconcile");
    let mut a = Recording::new(SinkId::EXPORT);
    let mut b = Recording::new(SinkId::RECONCILE);

    // Act
    let first = run_sink(&log, &mut a, &export, 0, 2).unwrap();
    let second = run_sink(&log, &mut b, &reconcile, 0, 10).unwrap();

    // Assert
    assert_eq!(
        first,
        SinkReport {
            consumed: 2,
            acked: Some(2),
            lease_lost: false,
        }
    );
    assert_eq!(second.acked, Some(5));
    assert_eq!(log.cursor("export"), Some(2), "a's ack moved a only");
    assert_eq!(log.cursor("reconcile"), Some(5));
    assert_eq!(a.seen, [1, 2]);
    assert_eq!(b.seen, [1, 2, 3, 4, 5]);
}

/// A stopped sink resumes from its ack: a fresh instance started at the stored cursor sees
/// only the rows after it, and an empty page acknowledges nothing.
#[test]
fn a_stopped_sink_resumes_from_its_ack() {
    // Arrange
    let log = FakeLog::with_rows(6);
    let lease = log.acquire("export");
    let mut before = Recording::new(SinkId::EXPORT);
    run_sink(&log, &mut before, &lease, 0, 4).unwrap();
    drop(before);
    let resume_from = log.cursor("export").unwrap();

    // Act
    let mut after = Recording::new(SinkId::EXPORT);
    let acks = drain(&log, &mut after, &lease, resume_from, 4);
    let idle = run_sink(&log, &mut after, &lease, 6, 4).unwrap();

    // Assert
    assert_eq!(resume_from, 4);
    assert_eq!(after.seen, [5, 6], "only the rows after the ack");
    assert_eq!(acks, [6]);
    assert_eq!(idle, SinkReport::default(), "an empty page acks nothing");
}

/// RP-6, SD-10: a successor acquires the lease between the read and the ack (the fence
/// moves); the ack is `LeaseLost`, the report says so with `acked: None`, the cursor does not
/// move, and the runner does not retry under the stale lease. The successor's run under the new
/// fence re-consumes the same rows and acks them.
#[test]
fn a_lost_lease_is_reported_and_the_cursor_does_not_move() {
    // Arrange
    let log = FakeLog::with_rows(3);
    let old = log.acquire("export");
    run_sink(&log, &mut Noop(SinkId::EXPORT), &old, 0, 1).unwrap();
    assert_eq!(log.cursor("export"), Some(1));
    let mut sink = Takeover {
        log: &log,
        successor: None,
        seen: Vec::new(),
    };

    // Act
    let report = run_sink(&log, &mut sink, &old, 1, 10).unwrap();

    // Assert
    assert_eq!(
        report,
        SinkReport {
            consumed: 2,
            acked: None,
            lease_lost: true,
        }
    );
    assert_eq!(
        log.cursor("export"),
        Some(1),
        "the fenced-out ack moved nothing"
    );
    let new = sink.successor.take().unwrap();
    assert_eq!(new.fence, old.fence + 1);
    let mut replay = Recording::new(SinkId::EXPORT);
    let again = run_sink(&log, &mut replay, &new, 1, 10).unwrap();
    assert_eq!(
        replay.seen, sink.seen,
        "the successor re-consumes the same rows"
    );
    assert_eq!(again.acked, Some(3));
    assert_eq!(log.cursor("export"), Some(3));
}

/// A sink's own failure propagates and acknowledges nothing.
#[test]
fn a_failing_sink_acknowledges_nothing() {
    // Arrange
    let log = FakeLog::with_rows(2);
    let lease = log.acquire("export");

    // Act
    let err = run_sink(&log, &mut Failing, &lease, 0, 10).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::BadLink), "{err:?}");
    assert_eq!(log.cursor("export"), None);
}
