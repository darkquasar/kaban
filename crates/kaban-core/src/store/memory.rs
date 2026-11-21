//! [`MemoryStore`]: the memory graph role trait.

use super::anchors::{AnchorPredicates, SnapshotAttach, SnapshotSymbol, SymbolPage};
use crate::audit::WriteCtx;
use crate::error::Result;
use crate::ids::NodeId;
use crate::model::{
    Edge, ForgetMode, ForgetOutcome, Hit, LinkReq, NewNode, Node, RecallQuery, RememberOutcome,
    TraverseQuery, TraverseResult,
};
use serde_json::Value;

/// The memory graph itself: notes and typed edges, reads by id, hybrid recall, bounded
/// traversal, lifecycle (`forget`), the linked `CodeRef` pointers and the counts behind the
/// `stats` tool. Used by the service layer's remember/query/mutate paths, the MCP adapter and
/// the bench.
pub trait MemoryStore: Send + Sync {
    /// Evaluate a single read-only statement at its selected view.
    ///
    /// # Errors
    /// Returns a refusal when the store cannot execute the statement.
    fn query(
        &self,
        statement: &super::Statement,
        limits: &super::QueryLimits,
    ) -> Result<super::RowSet>;
    /// Read a declared aggregate for the ordered, typed group key.
    /// A group with no members counts zero and sums to NULL.
    ///
    /// # Errors
    /// An unknown rollup, an invalid key, or a storage error.
    fn rollup(&self, name: &str, key: &[crate::SqlValue]) -> Result<crate::SqlValue>;
    /// Store a node and its inline links atomically; exact duplicates return
    /// `Existing`.
    ///
    /// # Errors
    /// `UnknownKind`, `UnknownRel`, `EndpointNotAllowed`, `BadLink`, `NotFound` (link endpoint)
    /// or a backend error; nothing is stored on error.
    fn remember(&self, n: NewNode, links: &[LinkReq], ctx: &WriteCtx) -> Result<RememberOutcome> {
        self.remember_with(n, links, &[], ctx)
    }
    /// [`Self::remember`] plus `refs`: each ref is a `CodeRef` node created in the same
    /// transaction unless an active node with the same content hash exists (then reused), and
    /// linked `ABOUT` from the note (new or existing). Nothing is stored on error (F1, M3-2).
    ///
    /// # Errors
    /// As [`Self::remember`].
    fn remember_with(
        &self,
        n: NewNode,
        links: &[LinkReq],
        refs: &[NewNode],
        ctx: &WriteCtx,
    ) -> Result<RememberOutcome>;
    /// Active nodes of `workspace_id` whose `dup_key` equals `dup_key` (F1 near-duplicates),
    /// ordered by `created_at`, then id (N5).
    ///
    /// # Errors
    /// Backend errors.
    fn find_dup_key(&self, workspace_id: &str, dup_key: &str) -> Result<Vec<Node>>;
    /// Fetch a node by id. Tombstoned and purged rows are hidden unless `include_hidden`;
    /// archived rows are always visible by id (core-spec §4 "reads default to active").
    ///
    /// # Errors
    /// Backend errors only; a missing or hidden node is `Ok(None)`.
    fn get(&self, id: &NodeId, include_hidden: bool) -> Result<Option<Node>>;
    /// Create or update a typed edge; `SUPERSEDES`/`INVALIDATES` mark the
    /// destination.
    ///
    /// # Errors
    /// `UnknownRel`, `NotFound`, `EndpointNotAllowed` or a backend error.
    fn link(
        &self,
        src: &NodeId,
        dst: &NodeId,
        rel: &str,
        confidence: Option<f64>,
        props: Option<Value>,
        ctx: &WriteCtx,
    ) -> Result<Edge>;
    /// Hybrid recall (BM25 + graph expansion + RRF), deterministic order. Lifecycle is
    /// enforced while every hop frontier is built (R2-4): expansion runs through active nodes,
    /// through archived ones only when `include_archived` opts in, never through tombstoned
    /// or purged ones, so a hidden node never bridges to an otherwise unreachable one.
    ///
    /// # Errors
    /// Backend errors.
    fn recall(&self, q: &RecallQuery) -> Result<Vec<Hit>>;
    /// [`Self::recall`] whose results all satisfy `eligible` (the caller's read gate: a
    /// record it hides is no deliverable result), with retrieval continued until the limit
    /// is filled with eligible results or the candidates are exhausted (spec 05 SS-11). The
    /// default filters a plain recall's page; a store that pages its seed index refills
    /// through one pinned generation instead, and then runs `eligible` under its own lock:
    /// the predicate must not call back into the store.
    ///
    /// # Errors
    /// Backend errors.
    fn recall_eligible(
        &self,
        q: &RecallQuery,
        eligible: &dyn Fn(&Node) -> bool,
    ) -> Result<Vec<Hit>> {
        Ok(self
            .recall(q)?
            .into_iter()
            .filter(|hit| eligible(&hit.node))
            .collect())
    }
    /// Bounded traversal over active nodes only (core-spec §4, F6): a start id that is not
    /// active in the queried workspace is `NotFound`, and every frontier expands through
    /// active far endpoints only (R2-4).
    ///
    /// # Errors
    /// `NotFound` for an unknown, non-active or other-workspace start id, or a backend error.
    fn traverse(&self, q: &TraverseQuery) -> Result<TraverseResult>;
    /// The active `Chunk` of `workspace` ingested from `path` whose byte range covers
    /// `offset` (`props.path`, `props.byte_start <= offset < props.byte_end`; the
    /// `<path>:<offset>` address of EN-F14a), the lowest id when ranges overlap. The lookup
    /// is bounded (the wave-2 review's RW2-6): the store selects by path and examines at most
    /// its `MAX_SPAN_SCAN` chunks of that path in id order, never the whole chunk population,
    /// so a path nothing holds is `Ok(None)` at a cost independent of the other documents.
    ///
    /// # Errors
    /// Backend errors.
    fn chunk_covering(&self, workspace: &str, path: &str, offset: u64) -> Result<Option<Node>>;
    /// Tombstone (default), invalidate, archive or purge a node (which the
    /// caller fills with the `Forget`/`Purge` entry). Lifecycle follows the spec §4 table;
    /// `Invalidate` has the same effect as `Tombstone`.
    ///
    /// # Errors
    /// `NotFound` (unknown or purged id), `InvalidTransition` or a backend error.
    fn forget(&self, id: &NodeId, mode: ForgetMode, ctx: &WriteCtx) -> Result<ForgetOutcome>;
    /// The `CodeRef` pointers linked to `owners` (core-spec F19; M3 review R3-10): the active
    /// `CodeRef` nodes that are the target of an `ABOUT` edge from, or the source of an
    /// `EVIDENCE_FOR` edge into, one of the owners, in the owner's own workspace. One bounded
    /// query for the whole list: at most `limit` rows, in owner order (the position in
    /// `owners`) then edge id, a pointer linked to two owners returned once (first position).
    ///
    /// # Errors
    /// Backend errors; an unknown owner has no pointers.
    fn linked_refs(&self, owners: &[NodeId], limit: usize) -> Result<Vec<Node>>;
    /// Attach the admitted code-graph snapshot `attach` as the `cg` schema of the store's
    /// dedicated read connection (CG-JOIN v1 §2, SO-4; engine-plan E2.6): opened lazily on the
    /// first call, read-only, re-attached when `commit` or `generation` differs from the
    /// current attachment, left attached across requests until [`Self::detach_snapshot`] or
    /// the store closes. The main write connection never attaches anything. The argument is
    /// the core-owned struct only: no path, no caller-typed string.
    ///
    /// ```compile_fail
    /// # fn attach(store: &dyn kaban_core::MemoryStore) -> kaban_core::Result<()> {
    /// store.attach_snapshot("file:/tmp/x.cg.db")
    /// # }
    /// ```
    ///
    /// # Errors
    /// `Backend` for a store without a file (an in-memory store has no second connection to
    /// open) or a driver failure; nothing is attached on error.
    fn attach_snapshot(&self, attach: &SnapshotAttach) -> Result<()>;
    /// Detach the current snapshot, if any, and close the read connection.
    ///
    /// # Errors
    /// Backend errors.
    fn detach_snapshot(&self) -> Result<()>;
    /// The active `CodeRef` nodes whose `symbol` anchor is byte-equal to `key` under the
    /// CG-JOIN v1 §3 predicates (`p`): the workspace filter, `repo = ApiConfig.repo`, and a
    /// `snapshot_scope` that is `NULL` or equal to the resolved commit (only `NULL` when no
    /// snapshot resolved). Byte equality on the key (SD-6): no collation, no normalisation.
    /// Ordered by row id (N5).
    ///
    /// # Errors
    /// Backend errors.
    fn coderefs_by_symbol(&self, p: &AnchorPredicates<'_>, key: &str) -> Result<Vec<Node>>;
    /// One page of the attached snapshot's `kind = 'symbol' AND external = 0` rows joined with
    /// their file path (CG-JOIN v1 §7, SO-10: the gazetteer's one symbol source), ordered by
    /// id, `page.after_id` exclusive, at most `page.limit` rows; the second value is the next
    /// `after_id` when the page was full. `commit` must equal the attached snapshot's commit.
    ///
    /// # Errors
    /// `SnapshotMismatch` when nothing is attached or the attached commit differs (no rows);
    /// backend errors.
    fn snapshot_symbols(
        &self,
        commit: &str,
        page: SymbolPage,
    ) -> Result<(Vec<SnapshotSymbol>, Option<i64>)>;
    /// Counts by kind/rel inside `workspace_id` (`None` = every workspace; legacy NULL rows
    /// count as `default`) and the global database size, as JSON.
    ///
    /// # Errors
    /// Backend errors.
    fn stats(&self, workspace_id: Option<&str>) -> Result<Value>;
}
