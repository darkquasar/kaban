//! [`SessionStore`]: the hook-session scratch and failure-pairing role trait.

use crate::audit::WriteCtx;
use crate::error::Result;
use crate::ids::NodeId;
use crate::model::{FailurePatch, Node, PendingFailuresQuery, SessionEdit, SessionEditKind};

/// Hook session scratch and failure pairing (core-plan C4.3/C4.4 as amended by M4-13 to
/// M4-17, M4-26: new store methods live in a role trait, never the umbrella).
pub trait SessionStore: Send + Sync {
    /// Append one `session_edits` row (migration 0003). The caller gated `path`.
    ///
    /// # Errors
    /// Backend errors.
    fn session_edit_record(&self, edit: &SessionEdit) -> Result<()>;
    /// The `kind` rows of `session_id` with `ts >= after_ts`, oldest first, at most `limit`.
    ///
    /// # Errors
    /// Backend errors.
    fn session_edits(
        &self,
        session_id: &str,
        kind: SessionEditKind,
        after_ts: i64,
        limit: usize,
    ) -> Result<Vec<SessionEdit>>;
    /// Delete at most `limit` rows with `ts < older_than_ts` (bounded so a hook stays within
    /// its budget); returns how many.
    ///
    /// # Errors
    /// Backend errors.
    fn session_edits_prune(&self, older_than_ts: i64, limit: usize) -> Result<u64>;
    /// Pending Failures per [`PendingFailuresQuery`] (M4-14): active Failures of the workspace
    /// with no outgoing `FIXED_BY` edge and `props.pending_fix` not `false`; `Session` scope
    /// keeps those with a `failure` row in the session, `Workspace` scope returns every pending
    /// Failure with the session's first; then newest first, then id.
    ///
    /// # Errors
    /// Backend errors.
    fn pending_failures(&self, q: &PendingFailuresQuery) -> Result<Vec<Node>>;
    /// Apply `patch` to an active Failure's props in one write transaction (M4-15): `fix` and
    /// `pending_fix` are set, `evidence_run` is appended, `rev` is bumped, the changes row
    /// comes from the node trigger and `ctx.audit` is appended. The node must live in
    /// `ctx.workspace_id` (core-spec S6).
    ///
    /// # Errors
    /// `NotFound` (unknown, hidden or other-workspace id), `KindMismatch` (not a Failure) or a
    /// backend error.
    fn update_failure_props(
        &self,
        id: &NodeId,
        patch: &FailurePatch,
        ctx: &WriteCtx,
    ) -> Result<Node>;
}
