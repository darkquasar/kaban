//! [`GovernanceStore`]: the governance, confirmation and hook-session role trait.

use crate::audit::WriteCtx;
use crate::error::Result;
use crate::events::{Confirmation, Event, NewConfirmation, NewEvent};
use crate::ids::NodeId;
use crate::lifecycle::Governance;
use crate::model::{
    FailurePatch, GoverningQuery, Node, PendingFailuresQuery, SessionEdit, SessionEditKind,
    WorkspaceFilter,
};

/// Stable identity of an explicitly protected graph entry. Protection affects pruning,
/// not replacement or explicit deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruningTarget {
    /// A node and its directly supported graph entries.
    Node(NodeId),
    /// One stable assertion key.
    Claim(crate::AssertionKey),
    /// One relationship triple.
    Relationship {
        /// Source node identity.
        src: NodeId,
        /// Destination node identity.
        dst: NodeId,
        /// Relationship name.
        rel: String,
    },
}

/// Explicit mark and bounded dependency protection, reported independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruningProtection {
    /// Whether the target itself has a durable mark.
    pub explicit: bool,
    /// Whether this entry is protected directly or by a required dependency.
    pub effective: bool,
}

/// Which way a [`GovernanceStore::supersession_chain`] walks the `SUPERSEDES` edges from its
/// seed (v4 stage1-plan CG5.3; cg-join-v1 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainDirection {
    /// The records the seed supersedes, then theirs: `A SUPERSEDES B` is the edge
    /// `src = A, dst = B`, so the walk follows `src = current` to `dst`.
    Predecessors,
    /// The records that supersede the seed, then theirs: the reverse, `dst = current` to `src`.
    Successors,
}

/// The result of a [`GovernanceStore::supersession_chain`] walk; `Default` is the empty chain
/// (no hop, nothing cut), which an implementation fills in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Chain {
    /// The hops reached, nearest first (breadth-first, edges in `created_at, id` order); the
    /// seed is not among them. Archived hops are returned: they are the chain.
    pub ids: Vec<NodeId>,
    /// The walk did not reach every hop: `limit` was reached, a hop was tombstoned, or a hop
    /// lay outside the workspace filter.
    pub cut: bool,
    /// How many hops lay outside the workspace filter (RS-9): each ended its branch and was
    /// never returned, so a caller can say "history continues in another workspace" without
    /// learning which record.
    pub cut_by_workspace: u32,
}

/// Governance of decisions and conventions (core-spec §4, F2, F14): the transition with its
/// event, the confirmation handshake rows, the `governing` lookup behind `why` and a node's
/// governance events. Used by the accept and why paths.
pub trait GovernanceStore: Send + Sync {
    /// Read protection in the context's workspace. Unsupported backends refuse.
    ///
    /// # Errors
    /// Missing/foreign target or unsupported backend.
    fn pruning_protection_get(
        &self,
        _target: &PruningTarget,
        _ctx: &WriteCtx,
    ) -> Result<PruningProtection> {
        Err(crate::StoreError::InvalidName(
            "pruning_protection".into(),
            "unsupported",
        ))
    }
    /// Set an explicit durable pruning mark atomically with context audit entries.
    ///
    /// # Errors
    /// Missing/foreign target or unsupported backend.
    fn pruning_protection_set(
        &self,
        _target: &PruningTarget,
        _ctx: &WriteCtx,
    ) -> Result<PruningProtection> {
        Err(crate::StoreError::InvalidName(
            "pruning_protection".into(),
            "unsupported",
        ))
    }
    /// Clear an explicit mark; independent dependency protection may remain.
    ///
    /// # Errors
    /// Missing/foreign target or unsupported backend.
    fn pruning_protection_clear(
        &self,
        _target: &PruningTarget,
        _ctx: &WriteCtx,
    ) -> Result<PruningProtection> {
        Err(crate::StoreError::InvalidName(
            "pruning_protection".into(),
            "unsupported",
        ))
    }
    /// Move an active governed record to `to` and append the event in the same transaction
    /// (core-spec §4): `Accepted` records `accepted_commit`, the event's `path_scope` and (when
    /// the column is unset) `ctx.repo`, `Superseded` also archives, `Invalidated` also
    /// tombstones. Returns the moved node and the event's `seq` (M3-18).
    ///
    /// # Errors
    /// `NotFound`, `InvalidTransition` (table, kind or non-active record) or a backend error.
    fn transition(
        &self,
        id: &NodeId,
        to: Governance,
        ev: NewEvent,
        ctx: &WriteCtx,
    ) -> Result<(Node, u64)>;
    /// Whether the non-purged node `id` holds a `discovered_kind` candidate row (engine-spec
    /// EN-F5a; E1.9a RE3-1): the service routes a human acceptance of such a node to the
    /// candidate branch of [`Self::transition`] instead of the governance table, which an
    /// `x:` kind is never in. An unknown or purged id is `false`, never an error, so the
    /// caller's own `NotFound` stays the one existence answer.
    ///
    /// # Errors
    /// Backend errors.
    fn is_candidate(&self, id: &NodeId) -> Result<bool>;
    /// Persist a confirmation handshake row (core-plan C3.2, PR-3): the token's keyed hash and
    /// the request strings, expiring after `ttl_s`.
    ///
    /// # Errors
    /// Backend errors.
    fn confirmation_create(&self, c: NewConfirmation) -> Result<()>;
    /// Redeem the confirmation whose hash is `token_hash` for `node_id`, once: in one
    /// `BEGIN IMMEDIATE`, the row is marked consumed and returned when it is unconsumed, not
    /// expired and issued for that node; the hash is compared in constant time. `None` when
    /// the token is unknown, expired, already consumed or issued for another node (the caller
    /// cannot tell which).
    ///
    /// # Errors
    /// Backend errors.
    fn confirmation_consume(
        &self,
        token_hash: &str,
        node_id: &NodeId,
    ) -> Result<Option<Confirmation>>;
    /// Delete confirmation rows that expired more than a day ago; returns how many.
    ///
    /// # Errors
    /// Backend errors.
    fn confirmation_prune(&self) -> Result<u64>;
    /// Governing records for one `why` target (core-spec F2; core-plan C3.4, M3-7): see
    /// [`GoverningQuery`]. Glob entries are prefiltered, not decided: the caller matches them
    /// exactly.
    ///
    /// # Errors
    /// Backend errors.
    fn governing(&self, q: &GoverningQuery) -> Result<Vec<Node>>;
    /// Governance events of one node, oldest first.
    ///
    /// # Errors
    /// Backend errors; an unknown node has no events.
    fn events(&self, node: &NodeId) -> Result<Vec<Event>>;
    /// The `SUPERSEDES` chain of `id` in one `direction` (v4 stage1-plan CG5.3; cg-join-v1 §7;
    /// the `why` extension of CG6.1 reads superseded history through it): breadth-first over
    /// `SUPERSEDES` edges ordered `created_at, id`, each hop visited once, at most `limit` ids
    /// returned (`cut: true` when more hops exist). Archived hops are returned (they are the
    /// chain); a tombstoned hop ends its branch with `cut: true` and is not returned.
    ///
    /// RS-9, S6: every hop's node is checked against `ws` before it is returned or expanded; a
    /// hop outside the filter ends its branch, is counted in `cut_by_workspace`, sets `cut` and
    /// is never returned, so the walk neither leaks another workspace's record nor bridges two
    /// visible records through a hidden one. The seed is checked the same way: a seed outside
    /// `ws`, tombstoned or unknown is `NotFound` (no existence leak; an archived seed is
    /// allowed, superseded records are the chain's usual seeds).
    ///
    /// # Errors
    /// `NotFound` (unknown, purged, tombstoned or other-workspace seed) or a backend error.
    fn supersession_chain(
        &self,
        id: &NodeId,
        direction: ChainDirection,
        limit: usize,
        ws: &WorkspaceFilter,
    ) -> Result<Chain>;

    // --- Hook session scratch, briefing and failure pairing (core-plan C4.3/C4.4 as amended
    // by M4-13 to M4-17, M4-26: new store methods live in a role trait, never the umbrella) ---

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
    /// The briefing records of `workspace_id` (M4-17): active, accepted records of the kinds
    /// in `kinds` (the caller's stored names from the loaded ontologies, engine-plan E1.6: the
    /// hooks pass the `governs_paths: true` kinds, the export and the digests their block
    /// kinds; the shipped `adr.yaml`: Decisions and Conventions) ordered `COALESCE(importance,
    /// 0) DESC, created_at DESC, id` (N5), at most `limit`; an empty `kinds` is empty.
    ///
    /// # Errors
    /// Backend errors.
    fn briefing(&self, workspace_id: &str, kinds: &[&str], limit: usize) -> Result<Vec<Node>>;
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
