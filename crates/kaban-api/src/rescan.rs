//! The rescan job (core-spec S4 "rescan job after policy change"; core-plan C2.7; MD-18,
//! MD-19; M2 review R2-2, R2-3): on every start, every node stamped with another policy version
//! than the loaded policy's `effective_hash()` is re-judged in batches of [`RESCAN_BATCH`], one
//! write transaction per batch, the cancellation token checked between batches. The binary
//! runs it on `spawn_blocking` after serving starts (`kaban serve`) or in the foreground
//! (`kaban rescan`).
//!
//! ```text
//!   rescan_if_policy_changed(token)                      store (one BEGIN IMMEDIATE per batch)
//!   +------------------------------------------+        +-----------------------------------+
//!   | loop:                                     |  =>    | SELECT <=500 nodes WHERE          |
//!   |   store.rescan(version, 500, judge) ------+------->|   policy_version <> ?             |
//!   |   scanned == 0 => UpToDate (no batch ran) |        |   AND lifecycle <> 'purged'       |
//!   |                   or Completed            |        | none: index_meta.policy_hash :=   |
//!   |   token cancelled => Cancelled            |        |   version (same transaction)      |
//!   |   (earlier batches committed)             |        | per node: judge(node) ->          |
//!   +------------------------------------------+        |   Keep   : stamp                  |
//!                                                        |   Redact : rewrite every string   |
//!   judge = scan_node_strings (inventory.rs):            |            column, new hash,      |
//!   the same persisted strings the read projection       |            rev+1, audit           |
//!   scans; reject anywhere => Reject                     |   Reject : tombstone, event, audit|
//!                                                        | COMMIT                            |
//!                                                        +-----------------------------------+
//! ```

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::types::{RescanOutcome, RescanStatus};
use kaban_core::{Assertion, AuditEntry, Node, RescanAction, RescanReport, WorkspaceId};
use kaban_policy::Policy;
use serde_json::Value;
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

    /// Re-judge every node stamped with another policy version than the loaded policy's. The
    /// stale-row query runs on every start (R2-3): `index_meta.policy_hash` is informational
    /// (MD-19) and is recorded by the store in the same transaction as the final empty batch.
    /// Synchronous and blocking: callers run it on a blocking thread. One transaction per
    /// batch of [`RESCAN_BATCH`] nodes; `cancel` is checked between batches, so a cancelled run
    /// leaves the batches that ran committed and the rest pending for the next start.
    /// `UpToDate` means the first query found nothing stale.
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
        // the same policy-version step covers the claim values (EN-S9): one pass, keyed by the
        // store's claim-side marker, before the node batches; its counts are logged, not
        // folded into the node figures of the outcome
        let claims = self
            .store
            .rescan_assertions(&version, &mut |claim, audit| self.judge_claim(claim, audit))?;
        if claims.scanned > 0 {
            tracing::info!(
                policy_version = %version,
                scanned = claims.scanned,
                masked = claims.rejected,
                "claim values re-judged under the loaded policy"
            );
        }
        // the database-wide marker is informational (MD-19, R2-3): the decision rests on the
        // stale-row query the store runs under its write lock on every start, so a node another
        // process stamped with an older policy after the marker was recorded is still found;
        // the store records the marker in the same transaction as the final empty batch
        let mut total = RescanReport::default();
        loop {
            let report = self
                .store
                .rescan(&version, RESCAN_BATCH, &mut |node| self.judge(node))?;
            total += report;
            if report.scanned > 0 {
                if outcome.batches == 0 {
                    tracing::info!(policy_version = %version, "stale policy stamps found; rescanning stored nodes");
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
                if outcome.batches == 0 {
                    tracing::debug!(policy_version = %version, "no stale policy stamps; nothing to rescan");
                    return Ok(outcome);
                }
                outcome.status = RescanStatus::Completed;
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
    /// Runs inside the store's batch transaction and must not call back into the store (the
    /// audit entries are returned with the action).
    fn judge(&self, n: &Node) -> RescanAction {
        let mut ctx = self.write_ctx(Some(&n.workspace_id));
        let mut red = Vec::new();
        let masked = self.scan_node_strings(&mut ctx, TOOL, n, &mut red);
        for e in &mut ctx.audit {
            e.node_id = Some(n.id.clone());
            e.workspace_id = WorkspaceId::new(n.workspace_id.as_str());
        }
        match masked {
            Err(_) => RescanAction::Reject { audit: ctx.audit },
            Ok(m) if m.strings.differs_from(n) => RescanAction::Redact {
                strings: Box::new(m.strings),
                audit: ctx.audit,
            },
            Ok(_) => RescanAction::Keep,
        }
    }

    /// Judge one stored claim value against the loaded policy (EN-S9): the same JSON gate the
    /// props of a node go through (keys, `key=value` views, string leaves). A redact hit
    /// returns the masked value; a reject hit has no masked form, so the whole value becomes
    /// one `[REDACTED:<rules>]` string, the shape of the redact mask; a clean value returns
    /// `None`. The verdict's audit entries are pushed to `audit` (no node id: a claim is not a
    /// node). Runs inside the store's transaction and must not call back into the store.
    fn judge_claim(&self, claim: &Assertion, audit: &mut Vec<AuditEntry>) -> Option<Value> {
        let value = claim.value.as_value();
        let mut ctx = self.write_ctx(Some(claim.workspace_id.as_str()));
        let mut red = Vec::new();
        let masked = match self.scan_json(&mut ctx, TOOL, "assertion.value", value, &mut red) {
            Ok(masked) if masked == *value => None,
            Ok(masked) => Some(masked),
            Err(ApiError::Rejected { rules, .. }) => {
                Some(Value::String(format!("[REDACTED:{}]", rules.join(","))))
            }
            // a masked key colliding with another: the value cannot be rebuilt, so it goes whole
            Err(_) => Some(Value::String("[REDACTED:assertion.value]".into())),
        };
        audit.append(&mut ctx.audit);
        masked
    }
}
