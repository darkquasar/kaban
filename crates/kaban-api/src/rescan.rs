//! The rescan job (core-spec S4 "rescan job after policy change"; core-plan C2.7; MD-18,
//! MD-19): on every start, when the database's `index_meta.policy_hash` differs from the loaded
//! policy's `effective_hash()`, every stored node is re-judged in batches of [`RESCAN_BATCH`],
//! one write transaction per batch walking the table by row position, the cancellation token
//! checked between batches. Nodes carry no policy stamp (REC-08): the marker, recorded with the
//! final empty batch, is what a later start compares. The binary runs it on `spawn_blocking`
//! after serving starts (`kaban serve`) or in the foreground (`kaban rescan`).
//!
//! ```text
//!   rescan_if_policy_changed(token)                      store (one BEGIN IMMEDIATE per batch)
//!   +------------------------------------------+        +-----------------------------------+
//!   | marker == version => UpToDate             |        | SELECT <=500 nodes WHERE          |
//!   | loop:                                     |  =>    |   rid > cursor                    |
//!   |   store.rescan(version, cursor, 500, f) --+------->|   AND lifecycle <> 'purged'       |
//!   |   cursor := report.cursor                 |        | none: index_meta.policy_hash :=   |
//!   |   scanned == 0 => Completed               |        |   version (same transaction)      |
//!   |   token cancelled => Cancelled            |        | per node: judge(node) ->          |
//!   |   (earlier batches committed)             |        |   Keep   : nothing                |
//!   +------------------------------------------+        |   Redact : rewrite every string   |
//!                                                        |            column, new hash,      |
//!   judge = scan_node_strings (inventory.rs):            |            rev+1                  |
//!   the same persisted strings the read projection       |   Reject : tombstone              |
//!   scans; reject anywhere => Reject                     | COMMIT                            |
//!                                                        +-----------------------------------+
//! ```

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::types::{RescanOutcome, RescanStatus};
use kaban_core::{IndexMeta, Node, RescanAction, RescanReport};
use kaban_policy::Policy;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

/// Nodes per rescan transaction (MD-18).
pub const RESCAN_BATCH: usize = 500;

/// Tool name recorded on rescan verdicts.
const TOOL: &str = "rescan";

impl KabanApi {
    /// The policy instance this service judges with, shared with ingestion so one process has
    /// one policy (D-3). Hold the read lock briefly and never across a store call.
    pub fn policy_handle(&self) -> Arc<RwLock<Policy>> {
        Arc::clone(&self.policy)
    }

    /// Re-judge every stored node when the database's policy marker differs from the loaded
    /// policy's version. Synchronous and blocking: callers run it on a blocking thread. One
    /// transaction per batch of [`RESCAN_BATCH`] nodes; `cancel` is checked between batches, so
    /// a cancelled run leaves the batches that ran committed and the marker unchanged, and the
    /// next start walks the table again. `UpToDate` means the marker already matched or there
    /// was no node to judge.
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
        // the store records the marker in the same transaction as the final empty batch
        let marker = self.store.index_meta()?;
        if marker.entries.get(IndexMeta::POLICY_HASH) == Some(&version) {
            tracing::debug!(policy_version = %version, "policy unchanged; nothing to rescan");
            return Ok(outcome);
        }
        let mut total = RescanReport::default();
        loop {
            let report = self
                .store
                .rescan(&version, total.cursor, RESCAN_BATCH, &mut |node| {
                    self.judge(node)
                })?;
            total += report;
            if report.scanned > 0 {
                if outcome.batches == 0 {
                    tracing::info!(policy_version = %version, "policy changed; rescanning stored nodes");
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
            if report.scanned == 0 {
                // an empty database has nothing to rescan: it stays `UpToDate`
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
