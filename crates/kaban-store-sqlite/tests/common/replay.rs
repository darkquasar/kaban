//! `ReplaySink` (engine-spec EN-F8a "a replay of the `ChangeLog` from zero reconstructs the same
//! `assertion_status` table"; engine-plan E2.2 item 7): a test-only [`Sink`] over the
//! `assertion` rows of the `ChangeLog` that re-decides every claim a page names through the
//! store's `redecide_assertions` seam. Idempotent by construction (EN-E10a): a page seen twice
//! decides the same groups to the same verdicts. `SinkId::RECONCILE` is reserved for the
//! deferred asynchronous loop (OWN-68 SC-12); this sink runs under its own test id.

use kaban_core::{Ack, Change, ChangeEntity, Result, Sink, SinkId};
use kaban_store_sqlite::SqliteStore;

pub(crate) const REPLAY_SINK: SinkId = SinkId("replay-test");

pub(crate) struct ReplaySink<'a> {
    store: &'a SqliteStore,
    /// Claim rows handed to the store so far (every `assertion` row of the log, any op).
    pub(crate) claims_seen: usize,
}

impl<'a> ReplaySink<'a> {
    pub(crate) fn new(store: &'a SqliteStore) -> Self {
        Self {
            store,
            claims_seen: 0,
        }
    }
}

impl Sink for ReplaySink<'_> {
    fn id(&self) -> SinkId {
        REPLAY_SINK
    }
    fn consume(&mut self, changes: &[Change]) -> Result<Ack> {
        let ids: Vec<i64> = changes
            .iter()
            .filter(|c| c.entity == ChangeEntity::Assertion)
            .filter_map(|c| c.id.parse::<i64>().ok())
            .collect();
        self.claims_seen += ids.len();
        if !ids.is_empty() {
            self.store.redecide_assertions(&ids)?;
        }
        Ok(Ack {
            upto: changes.last().map_or(0, |c| c.seq),
        })
    }
}
