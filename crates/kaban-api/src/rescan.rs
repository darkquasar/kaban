//! The rescan job (core-spec S4 "rescan job after policy change"; core-plan C2.7; MD-18,
//! MD-19): on every start the stored rows the loaded policy has not judged yet are re-judged in
//! batches of [`RESCAN_BATCH`], one write transaction per batch, the cancellation token checked
//! between batches. Rows carry no policy stamp (REC-08); the store's change log is the freshness
//! signal. When the database's `index_meta.policy_hash` differs from the loaded policy's
//! `effective_hash()` the run walks every node and every edge; otherwise it judges only the rows
//! the change log names after the `policy_scan_seq` watermark, so a row written after a complete
//! walk (by a process still under an older policy, say) or behind a running walk's cursor is
//! judged by the next run. The binary runs it on `spawn_blocking` after serving starts
//! (`kaban serve`) or in the foreground (`kaban rescan`).
//!
//! ```text
//!   rescan_if_policy_changed(token)                  store.rescan (one BEGIN IMMEDIATE each)
//!   +---------------------------------------+       +------------------------------------+
//!   | marker == version and                 |       | change log after the watermark      |
//!   |   watermark == head => UpToDate        |       |   (rows behind the walk's cursor)   |
//!   | loop from Start:                      |  =>   | walk: nodes, then edges             |
//!   |   store.rescan(version, cursor, 500) -+------>| complete walk: policy_hash :=       |
//!   |   cursor == Done => Completed         |       |   version (same transaction)        |
//!   |   token cancelled => Cancelled        |       | per row: judge(row) ->              |
//!   |   (earlier batches committed)         |       |   Keep / Redact in place /          |
//!   +---------------------------------------+       |   Reject (tombstone node, drop edge)|
//!                                                   | watermark := head after own writes  |
//!   judge = scan_node_strings (inventory.rs)        +------------------------------------+
//!   for nodes, the props scan of `link` for edges
//! ```

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::types::{RescanOutcome, RescanStatus};
use kaban_core::{
    Edge, EdgeRescanAction, IndexMeta, Node, RescanAction, RescanCursor, RescanJudge, RescanReport,
};
use kaban_policy::Policy;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

/// Rows (and change-log entries) per rescan transaction (MD-18).
pub const RESCAN_BATCH: usize = 500;

/// Tool name recorded on rescan verdicts.
const TOOL: &str = "rescan";

impl KabanApi {
    /// The policy instance this service judges with, shared with ingestion so one process has
    /// one policy (D-3). Hold the read lock briefly and never across a store call.
    pub fn policy_handle(&self) -> Arc<RwLock<Policy>> {
        Arc::clone(&self.policy)
    }

    /// Re-judge the stored rows the loaded policy has not judged yet (see the module docs).
    /// Synchronous and blocking: callers run it on a blocking thread. One transaction per
    /// batch of [`RESCAN_BATCH`] rows; `cancel` is checked between batches, so a cancelled run
    /// leaves the batches that ran committed (a cancelled walk leaves the marker unchanged and
    /// the next start walks again). `UpToDate` means nothing was left to judge.
    ///
    /// # Errors
    /// Store errors; the batch that failed is rolled back and `policy_hash` is left unchanged.
    pub fn rescan_if_policy_changed(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RescanOutcome, ApiError> {
        let version = self.policy_version();
        let mut outcome = RescanOutcome {
            status: RescanStatus::UpToDate,
            policy_version: version.clone(),
            batches: 0,
            scanned: 0,
            kept: 0,
            redacted: 0,
            rejected: 0,
        };
        // a read-only check first: a database the loaded policy has fully judged takes no
        // write transaction at start
        let marker = self.store.index_meta()?;
        let head = self.store.changes_head()?.to_string();
        if marker.entries.get(IndexMeta::POLICY_HASH) == Some(&version)
            && marker.entries.get(IndexMeta::POLICY_SCAN_SEQ) == Some(&head)
        {
            tracing::debug!(policy_version = %version, "policy unchanged and no row changed; nothing to rescan");
            return Ok(outcome);
        }
        let mut total = RescanReport::default();
        let mut judge = Judge(self);
        loop {
            let report = self
                .store
                .rescan(&version, total.cursor, RESCAN_BATCH, &mut judge)?;
            total += report;
            if report.scanned > 0 {
                if outcome.batches == 0 {
                    tracing::info!(policy_version = %version, "rescanning stored rows under the loaded policy");
                }
                outcome.batches += 1;
                tracing::debug!(
                    batch = outcome.batches,
                    scanned = report.scanned,
                    redacted = report.redacted,
                    rejected = report.rejected,
                    "rescan batch committed"
                );
            }
            if report.cursor == RescanCursor::Done {
                // nothing judged (an empty database, a log of deletes only) stays `UpToDate`
                if outcome.batches > 0 {
                    outcome.status = RescanStatus::Completed;
                }
                break;
            }
            if cancel.is_cancelled() {
                outcome.status = RescanStatus::Cancelled;
                break;
            }
        }
        outcome.scanned = total.scanned;
        outcome.kept = total.kept;
        outcome.redacted = total.redacted;
        outcome.rejected = total.rejected;
        tracing::info!(
            status = ?outcome.status,
            batches = outcome.batches,
            scanned = outcome.scanned,
            redacted = outcome.redacted,
            rejected = outcome.rejected,
            "rescan finished"
        );
        Ok(outcome)
    }

    /// Judge one stored edge's `props` with the scan `link` applies to them on write: a reject
    /// hit deletes the edge, redact hits are masked in place.
    fn judge_edge(&self, e: &Edge) -> EdgeRescanAction {
        let mut red = Vec::new();
        match self.scan_json(TOOL, "props", &e.props, &mut red) {
            Err(_) => EdgeRescanAction::Reject,
            Ok(props) if props != e.props => EdgeRescanAction::Redact { props },
            Ok(_) => EdgeRescanAction::Keep,
        }
    }

    /// Judge one stored node against the loaded policy over the one inventory of persisted
    /// strings the read projection uses too (`inventory.rs`, R2-2): a reject hit on any of them
    /// tombstones the node; redact hits are masked in place in every rewritable column; a
    /// redact hit confined to `kind` or `workspace_id` cannot be rewritten in place (it names a
    /// schema entry or a tenant) and stays masked by the read projection, so the node is kept.
    /// Runs inside the store's batch transaction and must not call back into the store.
    fn judge(&self, n: &Node) -> RescanAction {
        let mut red = Vec::new();
        match self.scan_node_strings(TOOL, n, &mut red) {
            Err(_) => RescanAction::Reject,
            Ok(m) if m.strings.differs_from(n) => RescanAction::Redact {
                strings: Box::new(m.strings),
            },
            Ok(_) => RescanAction::Keep,
        }
    }
}

/// The service's policy as the store's per-row judge.
struct Judge<'a>(&'a KabanApi);

impl RescanJudge for Judge<'_> {
    fn node(&mut self, node: &Node) -> RescanAction {
        self.0.judge(node)
    }
    fn edge(&mut self, edge: &Edge) -> EdgeRescanAction {
        self.0.judge_edge(edge)
    }
}
