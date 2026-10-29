//! [`MemoryStore`]: the memory graph role trait.

use super::anchors::{AnchorPredicates, SnapshotAttach, SnapshotSymbol, SymbolPage};
use super::types::ReviewRow;
use crate::audit::WriteCtx;
use crate::error::Result;
use crate::events::ForgetOutcome;
use crate::ids::NodeId;
use crate::model::{
    Edge, ForgetMode, Hit, LinkReq, NewNode, Node, RecallQuery, RememberOutcome, TraverseQuery,
    TraverseResult,
};
use crate::text_index::Visibility;
use serde_json::Value;

/// The memory graph itself: notes and typed edges, reads by id, hybrid recall, bounded
/// traversal, lifecycle (`forget`), the linked `CodeRef` pointers and the counts behind the
/// `stats` tool. Used by the service layer's remember/query/mutate paths, the MCP adapter and
/// the bench.
pub trait MemoryStore: Send + Sync {
    /// Store a node and its inline links atomically (with `ctx.audit`); exact duplicates return
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
    /// Create or update a typed edge (with `ctx.audit`); `SUPERSEDES`/`INVALIDATES` mark the
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
    /// through archived ones only when `include_superseded` opts in, never through tombstoned
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
    /// How far committed writes are visible to [`Self::recall`]'s seed searches (spec 05
    /// SS-10, SS-11): [`Visibility::Current`] for a store whose index is written in the
    /// record's transaction (the default); an index with its own storage reports the
    /// watermark its serving reader covers.
    ///
    /// # Errors
    /// Backend errors reading the watermark.
    fn text_visible(&self) -> Result<Visibility> {
        Ok(Visibility::Current)
    }
    /// Publish what this process committed and has not yet projected into an index with its
    /// own storage, toward `target` (a receipt's position; `u64::MAX` for everything) until
    /// `deadline` (spec 05 SS-10: the caller's one deadline per public call, passed down),
    /// never rebuilding and never waiting for another process's publisher. Nothing to do for
    /// a store whose index is written in the record's transaction (the default).
    ///
    /// # Errors
    /// A store or index failure; the caller's receipt stays committed whatever happens here.
    fn text_publish(&self, target: u64, deadline: std::time::Instant) -> Result<()> {
        let _ = (target, deadline);
        Ok(())
    }
    /// Bounded traversal over active nodes only (core-spec §4, F6): a start id that is not
    /// active in the queried workspace is `NotFound`, and every frontier expands through
    /// active far endpoints only (R2-4).
    ///
    /// # Errors
    /// `NotFound` for an unknown, non-active or other-workspace start id, or a backend error.
    fn traverse(&self, q: &TraverseQuery) -> Result<TraverseResult>;
    /// The explanation of the conflict group `group_key` (engine-spec EN-F14; engine-plan
    /// E2.4): the group's stored supporters are decided again, read-only, exactly as the write
    /// path decides them (same reconciler, same protection, same thresholds), and the vote is
    /// rendered: the candidates in decision order with their lineages, the margin and the
    /// gates, the tie-break level when two sums were equal, the unresolved reason and the
    /// protecting human claim when they apply. Nothing is written; the document is the same
    /// bytes whatever order the claims arrived in (EN-F12a).
    ///
    /// # Errors
    /// `NotFound` for a group no stored claim belongs to; backend errors.
    fn explain_group(&self, group_key: &str) -> Result<crate::reconcile::Explanation>;
    /// The group key of the claim whose stable key is `key`, in every workspace (the
    /// store-level entry the goldens drive). The service's `explain` entries read through
    /// [`Self::claim_group_in`], the workspace-bound form.
    ///
    /// # Errors
    /// `NotFound` for an unknown key; backend errors.
    fn claim_group(&self, key: &str) -> Result<String>;
    /// [`Self::claim_group`] bound to `workspace` (core-spec S6; the `kaban explain <key>`
    /// entry: a person names a claim, the explanation is its group's): the group key of the
    /// claim `key` when the claim belongs to `workspace`, else `NotFound` exactly as for a
    /// key nothing holds. A group key names its workspace, so the group it returns is the
    /// workspace's own.
    ///
    /// # Errors
    /// `NotFound` for an unknown key or one of another workspace; backend errors.
    fn claim_group_in(&self, workspace: &str, key: &str) -> Result<String>;
    /// The outstanding review items of `workspace` (engine-spec EN-F14a; engine-plan E2.5;
    /// RE4-9): one [`ReviewRow`] per `(node, group)` whose claims are still `candidate` or
    /// `unresolved` with no winner (an `unresolved` dissent parked under a human winner is a
    /// decided group, EN-F12c), plus the `discovered_kind` admission row of a node of an
    /// undeclared kind on its own, in `(rid, group_key)` order; `subject` narrows the list to
    /// one node. A candidate node holds a `node_candidate` row (the per-node truth of EN-F5a);
    /// an unresolved group joins from its statuses.
    ///
    /// # Errors
    /// Backend errors.
    fn review_rows(&self, workspace: &str, subject: Option<&NodeId>) -> Result<Vec<ReviewRow>>;
    /// One page of the active `proposed` records of `workspace`, whatever their kind (the
    /// wave-2 review's RW2-5: `kaban review` lists every record awaiting acceptance, the
    /// governed kinds and the ones the reconciler materialised alike, EN-F13), ordered by id
    /// (N5), `offset` rows skipped, at most `limit` rows (clamped to 500 by the store).
    ///
    /// # Errors
    /// Backend errors.
    fn proposed_nodes_in(&self, workspace: &str, offset: usize, limit: usize) -> Result<Vec<Node>>;
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
    /// Tombstone (default), invalidate, archive or purge a node (with `ctx.audit`, which the
    /// caller fills with the `Forget`/`Purge` entry). Lifecycle follows the spec §4 table;
    /// `Invalidate` also moves governance and writes an `invalidated` event.
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
    /// `kaban unmerge <id>` (engine-spec EN-F51 (5); engine-plan E2.10): every `node_identity`
    /// row naming `id` (as the merged or the canonical node) inside `workspace` is deleted, the
    /// identity claim behind each becomes `deprecated` under the gated `reason` and its group is
    /// re-decided without its vote, with `ctx.audit` in the same transaction. Both nodes stay.
    ///
    /// # Errors
    /// `NotFound` for an unknown or foreign id, or one no identity row names; backend errors.
    fn unmerge(
        &self,
        workspace: &str,
        id: &NodeId,
        reason: &str,
        ctx: &WriteCtx,
    ) -> Result<crate::er::UnmergeOutcome>;
    /// The canonical a merged node reads as (engine-spec EN-F51 (5); E2.10 boundary review
    /// BR-6, the logical-read contract): the `node_identity` redirect of `id` inside
    /// `workspace`, as a node id. `None` for a canonical (it has no row), for a node no
    /// identity row names, for a purged canonical (`NULL` is no redirect) and for a store
    /// without the engine tables. A projection of the winning identity claims, never evidence.
    ///
    /// # Errors
    /// Backend errors.
    fn canonical_of(&self, workspace: &str, id: &NodeId) -> Result<Option<NodeId>>;
}
