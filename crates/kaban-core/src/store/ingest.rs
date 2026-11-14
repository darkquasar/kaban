//! [`IngestStore`]: the bulk and maintenance write role trait, with the types only it uses.

use crate::audit::{WorkspaceId, WriteCtx};
use crate::error::Result;
use crate::ids::NodeId;
use crate::model::{
    BatchReport, IndexMeta, Node, RescanCursor, RescanJudge, RescanReport, WriteBatch,
};

/// One `failed` document of a [`DocumentIndexSummary`]: the gated path and the recorded error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedDocumentRow {
    /// `props.path` (the gated relative path).
    pub path: String,
    /// `props.error` (one of the pipeline's fixed texts or a gated extractor message).
    pub error: String,
}

/// Aggregates over the active nodes of one kind inside one workspace, computed by the backend
/// without materialising the rows (docs-ingest DI2.0 (c); `IngestStore::document_index_summary`):
/// the `index_status` numbers behind the `kaban://documents` resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentIndexSummary {
    /// Rows by `props.status` (`unknown` for a row without one); the values sum to the total.
    pub by_status: std::collections::BTreeMap<String, u64>,
    /// Sum of the lengths of `props.chunk_ids`.
    pub chunks: u64,
    /// Sum of `props.blocked_chunks`.
    pub blocked_chunks: u64,
    /// Sum of `props.table_unknown_chunks` (a row without the key counts zero).
    pub table_unknown_chunks: u64,
    /// Rule id -> sum of the hits recorded in `props.rule_ids` over every row.
    pub rule_ids: std::collections::BTreeMap<String, u64>,
    /// The rows whose `props.status` is `failed`, in path order.
    pub failed: Vec<FailedDocumentRow>,
}

/// Bulk and maintenance writes: the per-document ingestion batch (core-spec F16), the policy
/// rescan (core-spec S4) and the DB-scoped `index_meta` bookkeeping they stamp. Used by the
/// ingestion committer and the rescan loop.
pub trait IngestStore: Send + Sync {
    /// Bounded path-ordered active Documents without a current layer completion marker.
    /// # Errors
    /// Backend read failure or unsupported extractor layer.
    fn jobs_pending(
        &self,
        workspace: &WorkspaceId,
        dialect: &str,
        path_prefix: Option<&str>,
        limit: u32,
    ) -> Result<Vec<crate::PendingJob>> {
        let _ = (workspace, dialect, path_prefix, limit);
        Ok(Vec::new())
    }
    /// Acquire a fenced lease and bind the prepared job atomically; contention is `None`.
    /// # Errors
    /// Stale source, incompatible original contract, or backend failure.
    fn job_claim(&self, request: crate::JobClaimRequest) -> Result<Option<crate::Lease>> {
        let _ = request;
        Err(crate::StoreError::InvalidName(
            "claim".into(),
            "unavailable",
        ))
    }
    /// Validate the live holder before reopening; optionally renew after successful gating.
    /// # Errors
    /// Revoked handle, lost holder, or backend failure.
    fn job_holder(
        &self,
        job: &crate::JobId,
        workspace: &WorkspaceId,
        fence: Option<u64>,
        renew: bool,
    ) -> Result<()> {
        let _ = (job, workspace, fence, renew);
        Ok(())
    }
    /// Count current unmarked revisions whose monotonic attempt cap is exhausted.
    /// # Errors
    /// Backend read failure.
    fn jobs_exhausted(&self, workspace: &WorkspaceId, attempts: u32) -> Result<u64> {
        let _ = (workspace, attempts);
        Ok(0)
    }
    /// Explicit DB-only reset of one extractor layer, preserving other authorities.
    /// # Errors
    /// Invalid layer/bound, unsafe ownership or backend failure.
    fn jobs_reindex(
        &self,
        layer: crate::Layer,
        limit: u32,
        ctx: &WriteCtx,
    ) -> Result<crate::RetireReport> {
        let _ = (layer, limit, ctx);
        Err(crate::StoreError::InvalidName(
            "layer".into(),
            "unavailable_producer",
        ))
    }
    /// Atomically retire omitted scoped claims/edges and retain the gated canonical report.
    /// Callback cannot lock the store or await; errors roll back the complete transaction.
    /// # Errors
    /// Revoked, stale, expired source preparation or backend/gate failure.
    fn job_finish(
        &self,
        request: crate::JobFinishRequest,
        ctx: &WriteCtx,
        report: &mut dyn FnMut(
            &crate::IngestJob,
            &crate::JobFinishOutcome,
        ) -> Result<crate::JobFinishStateUpdate>,
    ) -> Result<String> {
        let _ = (request, ctx, report);
        Err(crate::StoreError::InvalidName(
            "job".into(),
            "not_available_in_this_release",
        ))
    }
    /// Prune at most `limit` expired finished rows of this workspace.
    /// # Errors
    /// Backend maintenance failure.
    fn jobs_prune(&self, workspace: &WorkspaceId, now: i64, limit: u32) -> Result<u32> {
        let _ = (workspace, now, limit);
        Ok(0)
    }
    /// Open a metadata-only job or return its original row without resetting state/counters.
    /// Legacy registry fixtures/backends may use this without a published source.
    /// Ordinary runtime brief must use `job_open_checked` instead.
    ///
    /// # Errors
    /// Backend failures or unresolved cross-tenant handle collision, never existence disclosure.
    fn job_open(&self, job: &crate::IngestJob) -> Result<crate::IngestJob>;
    /// Validate the tenant, active owned Document, exact revision and complete live
    /// Chunk binding in the same transaction as ordinary metadata insertion.
    /// A repeat returns the original contract and counters, never resetting state.
    /// The runtime must gate strings and check the returned immutable contract.
    /// Unsupported backends refuse rather than falling back to unchecked opening.
    ///
    /// # Errors
    /// Revoked/stale source preparation, unsupported backend or storage failure.
    fn job_open_checked(&self, request: crate::JobOpenRequest) -> Result<crate::IngestJob> {
        let _ = request;
        Err(crate::StoreError::InvalidName(
            "job".into(),
            "not_available_in_this_release",
        ))
    }
    /// Resolve only this tenant's job; absent, foreign and revoked handles all return `None`.
    ///
    /// # Errors
    /// Backend read/codec failures.
    fn job_get(
        &self,
        job: &crate::JobId,
        workspace: &WorkspaceId,
    ) -> Result<Option<crate::IngestJob>>;
    /// Registry-only stale transition; never modifies documents, chunks or graph history.
    ///
    /// # Errors
    /// Backend failures; `false` means unresolved without disclosing another tenant's row.
    fn job_mark_stale(&self, job: &crate::JobId, workspace: &WorkspaceId) -> Result<bool>;
    /// Publish gated L3 graph/assertions/audit and successful accumulation atomically.
    /// The callback sees the transaction-current row and actual outcomes; it must not
    /// acquire the store or await. Any callback error rolls back the entire publication.
    /// Source/contract races and stale batches never count as successful submits.
    ///
    /// # Errors
    /// Unresolved/revoked job, stale/expired preparation, unsafe batch or backend failure.
    fn job_submit(
        &self,
        request: crate::JobSubmitRequest,
        ctx: &WriteCtx,
        accumulate: &mut dyn FnMut(
            &crate::IngestJob,
            &BatchReport,
        ) -> Result<crate::JobSubmitStateUpdate>,
    ) -> Result<crate::JobSubmitReceipt> {
        let _ = (request, ctx, accumulate);
        Err(crate::StoreError::InvalidName(
            "job".into(),
            "not_available_in_this_release",
        ))
    }
    /// Retire obsolete tracked artifact-only publications in bounded tenant transactions.
    /// No obsolete claim-producing Gazetteer version is admitted: ambiguous claims refuse
    /// before writes, and the trusted current producer always refuses.
    ///
    /// # Errors
    /// Invalid batch bound, invalid source ownership, or transactional backend errors.
    fn retire_producer(
        &self,
        authority: crate::CurrentProducerAuthority,
        old_producer: &str,
        batch_rows: u32,
        ctx: &WriteCtx,
    ) -> Result<crate::RetireReport>;
    /// Offline read-only artifact and reserved sink freshness checks; reports counts only.
    ///
    /// # Errors
    /// Backend read failures; no repair, publication or optimization is performed.
    fn artifact_diagnostics(
        &self,
        workspace: &WorkspaceId,
        authority: crate::CurrentProducerAuthority,
    ) -> Result<crate::ArtifactDiagnostics>;
    /// Bounded, tenant-scoped maintenance of an admitted machine layer. Document paths
    /// must have passed the service CONTENT gate before this storage lookup.
    ///
    /// # Errors
    /// Invalid source ownership, invalid batch bound, or transactional backend errors.
    fn maintain_layer(
        &self,
        authority: crate::CurrentProducerAuthority,
        operation: crate::LayerMaintenance,
        document_path: Option<&str>,
        batch_rows: u32,
        ctx: &WriteCtx,
    ) -> Result<crate::RetireReport>;
    /// Apply one ingestion batch in one write transaction (core-plan C2.6; F16 store side):
    /// node ops in order, then edge ops, then the MD-23 edge diff (outgoing edges of each
    /// upserted node that the batch did not name are deleted; incoming edges are untouched),
    /// in one transaction. A node whose WD-7 content
    /// hash is unchanged writes no row and no changes row. The report's `ChangeLog` range covers
    /// exactly the `changes` rows this transaction wrote. Any failing op rolls the whole batch
    /// back.
    ///
    /// Every id the batch names (upserts, tombstones, both edge endpoints) resolves inside
    /// `batch.workspace_id` (R2-1): another tenant's node is `NotFound`, so no cross-workspace
    /// edge, tombstone or `SUPERSEDES`/`INVALIDATES` side effect is possible, and
    /// `ctx.workspace_id` must equal `batch.workspace_id` (`WorkspaceMismatch` before any write).
    ///
    /// # Errors
    /// `WorkspaceMismatch`, `UnknownKind`, `UnknownRel`, `EndpointNotAllowed`, `SelfLink`,
    /// `NotFound` (unknown, purged or other-workspace id), `KindMismatch`, `InvalidTransition`
    /// or a backend error.
    fn apply_batch(&self, batch: WriteBatch, ctx: &WriteCtx) -> Result<BatchReport>;
    /// Re-judge one batch of stored rows against `policy_version` in one write transaction
    /// (core-spec S4; core-plan C2.7), resuming at `at` (the previous batch's
    /// [`RescanReport::cursor`], [`RescanCursor::Start`] to begin a run); the caller loops
    /// until the cursor is [`RescanCursor::Done`]. `judge` decides per row and the store
    /// applies the action: for a node nothing, a rewrite of every rewritable string column
    /// in place (new `rev`, new `content_hash` over the id and the masked strings) or a
    /// tombstone; for an edge nothing, masked `props` in place or a delete. Rows carry no
    /// policy stamp (REC-08); freshness is the change log instead:
    ///
    /// - From `Start`, when `index_meta.policy_hash` differs from `policy_version` (or the
    ///   change log was pruned past the watermark), the run is a full walk: the watermark
    ///   [`IndexMeta::POLICY_SCAN_SEQ`] is reset to the change-log head, every non-purged
    ///   node is walked by row id, then every edge; the empty edge read records
    ///   `policy_hash = policy_version` in the same transaction.
    /// - Every batch first judges the rows the change log names after the watermark (at most
    ///   `batch` entries), so a row another writer changed behind the walk's cursor is judged
    ///   again; rows ahead of the cursor are left to the walk. The watermark then moves past
    ///   the entries read and, when the log is caught up, past the batch's own writes (with
    ///   more than `batch` entries pending, the own writes are judged again later: the
    ///   actions are idempotent, a row may count twice in a run's report).
    /// - From `Start` with a matching marker the run judges only the change log, so a row
    ///   written after a completed walk (by a process under an older policy, say) is judged
    ///   by the next run.
    ///
    /// A cancelled or crashed walk leaves the earlier batches committed and restarts on the
    /// next run (idempotent). `judge` must not call back into the store.
    ///
    /// # Errors
    /// Backend errors; the batch is rolled back.
    fn rescan(
        &self,
        policy_version: &str,
        at: RescanCursor,
        batch: usize,
        judge: &mut dyn RescanJudge,
    ) -> Result<RescanReport>;
    /// Every `index_meta` entry (DB-scoped bookkeeping: schema version, `db_id`, policy hash).
    ///
    /// # Errors
    /// Backend errors.
    fn index_meta(&self) -> Result<IndexMeta>;
    /// Insert or replace one `index_meta` entry.
    ///
    /// # Errors
    /// Backend errors.
    fn set_index_meta(&self, key: &str, value: &str) -> Result<()>;
    /// Rebuild the active text index from the canonical rows (spec 05 SS-16: the generic
    /// rebuild operates on whichever index serves recall); returns the active rows it
    /// re-indexed. Below the FTS5 retirement that is the in-database index and the derived
    /// identifier vocabulary, rebuilt atomically.
    ///
    /// # Errors
    /// Backend errors; a failed pass leaves the index and version unchanged;
    /// `TextIndexUnavailable` when the database retired FTS5 and no index is attached.
    fn reindex_text(&self) -> Result<u64>;
    /// Current Gazetteer complete-set generation for an ingest-owned Document.
    /// An absent Document has generation zero. The returned guard must accompany publication.
    /// A scoped ingest-owned tombstoned Document remains readable for preparation; publishing
    /// its complete set requires an authorized same-document `Reactivate` in that batch.
    ///
    /// # Errors
    /// Backend errors or an existing Document outside the tenant/ingest ownership scope.
    fn gazetteer_generation(&self, document: &NodeId, workspace: &WorkspaceId) -> Result<u64>;
    /// Whether a completed Gazetteer set still binds this active ingest-owned source and
    /// policy. Successful empty sets are fresh; absent, reset or legacy unbound state is not.
    /// Backends without private freshness state conservatively return false.
    ///
    /// # Errors
    /// Backend errors or an existing Document outside the tenant/ingest ownership scope.
    fn gazetteer_set_fresh(&self, _document: &NodeId, _workspace: &WorkspaceId) -> Result<bool> {
        Ok(false)
    }
    /// Active source coordinates within this tenant; foreign, hidden and unknown ids return
    /// `None` identically, without exposing their existence to a producer.
    ///
    /// # Errors
    /// Backend errors.
    fn artifact_source(
        &self,
        id: &NodeId,
        workspace: &WorkspaceId,
    ) -> Result<Option<crate::ArtifactSource>>;
    /// One page of the active nodes of `kind` inside `workspace_id` (docs-ingest DI1.6: the
    /// `index_status` fold and the `kaban://documents` listing), ordered by id (N5), `offset`
    /// rows skipped, at most `limit` rows (clamped to 500 by the store; the caller paginates).
    /// Tombstoned, archived and purged rows are not listed; another workspace's rows never are
    /// (S6).
    ///
    /// # Errors
    /// Backend errors.
    fn list_nodes(
        &self,
        kind: &str,
        workspace_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Node>>;
    /// The [`DocumentIndexSummary`] of the active nodes of `kind` inside `workspace_id`
    /// (docs-ingest DI2.0 (c)): counts by `props.status`, the sums of `props.chunk_ids` lengths
    /// and `props.blocked_chunks`, the `props.rule_ids` hit totals and the `failed` rows (path
    /// and error), computed by the backend's aggregates over the JSON props, so no row is
    /// decoded into a [`Node`]. Tombstoned, archived and purged rows do not count; another
    /// workspace's rows never do (S6).
    ///
    /// # Errors
    /// Backend errors.
    fn document_index_summary(
        &self,
        kind: &str,
        workspace_id: &str,
    ) -> Result<DocumentIndexSummary>;

    // C4.2 `kaban doctor` (core-spec §7; M4-19): the maintenance checks and the one maintenance
    // operation behind the diagnostic report. Busy and corrupt are outcomes, not errors, so
    // the report can name them; every other backend failure is an error.

    /// The engine's whole-file structural check (`PRAGMA integrity_check` on SQLite): `Ok`, or
    /// `Failed` with the engine's findings.
    ///
    /// # Errors
    /// Backend errors other than a corrupt file or a busy database.
    fn integrity_check(&self) -> Result<MaintenanceOutcome>;
    /// The active text index's own consistency check. For the in-database FTS5 index (below
    /// the spec 05 SS-16 retirement) its plain form: the index is checked against itself, not
    /// against the content table it is external to, because it indexes active rows only
    /// (M4-19; archived and tombstoned rows are not corruption); it needs the write lock for
    /// its duration and reports `Busy` when it cannot get it. For an attached index: whether
    /// it serves a payload (its admission at open is the deeper check).
    ///
    /// # Errors
    /// Backend errors other than a corrupt index or a busy database; `TextIndexUnavailable`
    /// when the database retired FTS5 and no index is attached.
    fn fts_integrity_check(&self) -> Result<MaintenanceOutcome>;
    /// Rebuild the database file to reclaim free pages (`VACUUM`; core-spec S5 residual,
    /// `doctor --vacuum`). `Busy` when another connection holds the lock.
    ///
    /// # Errors
    /// Backend errors other than a busy database.
    fn vacuum(&self) -> Result<MaintenanceOutcome>;
    /// Optimize the active text index only on an explicit maintenance request. Canonical
    /// nodes, edges, assertions and the `ChangeLog` are unchanged; never called on a write.
    /// For the in-database FTS5 index it is a purge's post-commit cleanup too, so it retries
    /// one reported incomplete; an attached index merges its segments as it commits, so
    /// there is nothing to run and `Ok` reports that.
    ///
    /// # Errors
    /// Backend errors other than a busy or corrupt index; `TextIndexUnavailable` when the
    /// database retired FTS5 and no index is attached.
    fn optimize_fts(&self) -> Result<MaintenanceOutcome>;
}

/// Outcome of one maintenance check or operation of `doctor` (core-spec §7; M4-19): a check
/// that cannot run because another connection holds the lock, or that finds the file or index
/// damaged, is a reported outcome rather than an error, so the report can carry it next to
/// the other checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceOutcome {
    /// The check passed, or the operation ran.
    Ok,
    /// The check found damage: the engine's own findings, one per line (table, index and row
    /// identifiers, never stored content), or one fixed line when the engine refused the file.
    Failed(Vec<String>),
    /// Another connection held the lock for the whole busy timeout; nothing was checked or
    /// changed.
    Busy,
}
