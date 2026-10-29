//! [`IngestStore`]: the bulk and maintenance write role trait, with the types only it uses.

use crate::audit::{Actor, AuditEntry, WorkspaceId, WriteCtx};
use crate::config::PromoteWhen;
use crate::error::Result;
use crate::ids::NodeId;
use crate::model::{
    BatchReport, IndexMeta, KindStat, Node, PromoteReport, PruneReport, RescanAction, RescanReport,
    WriteBatch,
};
use crate::ontology::Qualified;
use serde_json::Value;
use std::time::Duration;

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
    /// with `ctx.audit` appended in the same transaction (D-17). A node whose WD-7 content
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
    /// Re-judge one batch of at most `batch` non-purged nodes whose `policy_version` differs
    /// from `policy_version`, oldest row first, in one write transaction (core-spec S4;
    /// core-plan C2.7): `f` decides per node and the store applies the [`RescanAction`]
    /// (stamp; rewrite every rewritable string column in place with a new `rev` and a new
    /// `content_hash` over the id and the masked strings; or tombstone with a `policy-reject`
    /// event), with the action's audit entries in the same transaction. Every node the batch
    /// judged ends up stamped with `policy_version`, so the caller loops until `scanned == 0`
    /// and a cancelled or crashed run leaves the earlier batches committed and the rest pending
    /// (idempotent re-run). An empty batch records `index_meta.policy_hash = policy_version`
    /// in the same transaction (R2-3: the marker and the "nothing stale" check are atomic).
    /// `f` must not call back into the store.
    ///
    /// # Errors
    /// Backend errors; the batch is rolled back.
    fn rescan(
        &self,
        policy_version: &str,
        batch: usize,
        f: &mut dyn FnMut(&Node) -> RescanAction,
    ) -> Result<RescanReport>;
    /// Re-judge every stored claim value under `policy_version` in one write transaction
    /// (engine-spec EN-S9 "rescan covers `assertion.value`"; engine-plan E2.2 item 11): `f`
    /// sees each claim whose `value` is not null and returns the masked value when the policy
    /// redacts or rejects a string in it (`None` keeps the row), pushing its audit entries. A
    /// masked claim has its `value` column rewritten and is retired for good: `rank` becomes
    /// `deprecated` with `rank_reason` `policy:<rule ids>` (the rules of the entries `f`
    /// pushed for it, else `policy:<policy_version>`), its status is set to `rejected`, and
    /// every touched conflict group is re-decided over its remaining supporters, the way the
    /// replay seam does, so a masked claim neither votes nor projects, now or after any later
    /// decide or replay. The pass is keyed by two DB-scoped `index_meta` rows (claim rows
    /// carry no per-row stamp): `assertion_policy_hash`, the version the last pass ran
    /// under, and `assertion_policy_hw`, the greatest claim id it reached. An equal version
    /// judges the claims above the high-water id only (rows another process inserted since
    /// the last pass; none means nothing is touched and `f` is never called); another
    /// version judges every claim. Both rows are written at the end of a pass, in its
    /// transaction; a purge of claim rows lowers the high-water id to the greatest
    /// surviving one. The report counts claims: `scanned`, `kept`, `rejected` (masked);
    /// `redacted` stays `0`. `f` must not call back into the store.
    ///
    /// # Errors
    /// Backend errors; the transaction is rolled back and both rows are left unchanged.
    fn rescan_assertions(
        &self,
        policy_version: &str,
        f: &mut dyn FnMut(&crate::reconcile::Assertion, &mut Vec<AuditEntry>) -> Option<Value>,
    ) -> Result<RescanReport>;
    /// A human sets one claim's rank to `deprecated` with `reason` (engine-spec EN-F12b, the
    /// `kaban explain <key> --deprecate` action; engine-plan E2.3 item 5) and the claim's
    /// conflict group is re-decided in the same transaction without its vote (the rank is
    /// outside the identity key, EN-F12a (2): an update, never a new row; the gated `reason`
    /// is stored as `rank_reason`). The claim is looked up inside `ctx.workspace_id` (S6: a
    /// key of another workspace is `NotFound`, the same refusal an unknown key gets) and
    /// `ctx.audit` is appended in the same transaction. Returns the claim's status after the
    /// re-decide. The caller judges the actor and the reason; the store judges neither.
    ///
    /// # Errors
    /// `NotFound` for an unknown or foreign key; backend errors (the transaction is rolled
    /// back).
    fn deprecate_assertion(
        &self,
        key: &str,
        reason: &str,
        ctx: &WriteCtx,
    ) -> Result<crate::reconcile::AssertionStatus>;
    /// A human names the winner of one of `node`'s outstanding candidate groups (engine-plan
    /// E2.5 `kaban resolve <id> --winner <value> [--group <key>]`; EN-F5a, EN-F12c; wave-3
    /// review RW3-3): one human `preferred` claim carrying `value` joins the node's
    /// `candidate` group named by `group`, or its only one when `group` is `None`; the group
    /// is re-decided and projected, the node's candidate row is released, and `ctx.audit` is
    /// appended, all in one store transaction (the status write and the `DELETE FROM
    /// node_candidate` commit together or not at all). The group's subject, class and
    /// predicate are read from the group itself; `value` is the caller's gated string. The
    /// node is looked up in `workspace` (S6). The caller judges the actor; the store judges
    /// neither the actor nor the value. Returns the group key.
    ///
    /// # Errors
    /// `NotFound` for an unknown node, one of another workspace, one with no candidate group
    /// or a `group` that is not one of the node's candidate groups; `InvalidName("group",
    /// "ambiguous_group")` when the node has several candidate groups and none is named;
    /// backend errors (the transaction is rolled back).
    fn resolve_winner(
        &self,
        workspace: &str,
        node: &NodeId,
        value: &str,
        group: Option<&str>,
        ctx: &WriteCtx,
    ) -> Result<String>;
    /// A human re-targets a `CodeRef` whose anchored symbol the resolved snapshot no longer
    /// holds (engine-plan E2.5 `kaban resolve <coderef-id> --symbol <key>`, SO-6): one human
    /// `preferred` `prop_value` claim with predicate `symbol` carrying `key` is recorded and
    /// decided, the node's `props.symbol` becomes `key` and its `symbol` anchor row follows
    /// (EN-F10 anchor maintenance on write), and `ctx.audit` is appended, all in one store
    /// transaction. The node is looked up in `workspace` (S6). The caller judges the actor and
    /// the key's form (the normalised key of stage1-spec §4.2); the store judges neither.
    ///
    /// # Errors
    /// `NotFound` for an unknown node, one of another workspace, one that is not an active
    /// `CodeRef`; backend errors (the transaction is rolled back).
    fn resolve_symbol(
        &self,
        workspace: &str,
        node: &NodeId,
        key: &str,
        ctx: &WriteCtx,
    ) -> Result<()>;
    /// Count a human verdict on the reconciled claims of `node` (engine-spec EN-F16; engine-plan
    /// E2.4): in the `priors_obs` table (created on first use, no migration) the
    /// `(layer, method, claim_class)` counter named by `outcome` moves by one for every claim
    /// of the node's conflict groups whose status is `winner` or `candidate` and whose layer is
    /// not `human` (a person's own claim is no evidence about a layer). No edge, claim or
    /// status row changes; `calibration_version` stays 0 (v1 never applies the counters).
    /// Returns how many claims were counted.
    ///
    /// # Errors
    /// `NotFound` for an unknown or purged node; backend errors.
    fn priors_obs_bump(
        &self,
        node: &NodeId,
        outcome: crate::reconcile::PriorsOutcome,
    ) -> Result<u32>;
    /// Every configured prior with its counters (zero before any observation), in the
    /// vocabulary order of `(layer, method, claim_class)`: what `doctor --priors` prints.
    ///
    /// # Errors
    /// Backend errors.
    fn priors_obs(&self) -> Result<Vec<crate::reconcile::PriorsObs>>;
    /// The group keys of `workspace` holding at least one `unresolved` claim (EN-F12b: the
    /// groups only a human action resolves), ascending.
    ///
    /// # Errors
    /// Backend errors.
    fn unresolved_groups(&self, workspace: &str) -> Result<Vec<String>>;
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

    // engine-plan E1.8: discovered labels (EN-F5, EN-F6). The placement itself happens inside
    // `apply_batch` and `remember` under `WriteCtx.discovered`; these are its bookkeeping.

    /// The `kind_stats` rows of `workspace` (EN-F6, EN-S3: another workspace's rows never
    /// appear), ordered by `(ns, kind)`.
    ///
    /// # Errors
    /// Backend errors.
    fn kind_stats(&self, workspace: &WorkspaceId) -> Result<Vec<KindStat>>;
    /// Promote the discovered kind `from` (`x:Vendor`) of `workspace` to the declared kind
    /// `to` (EN-F5, human-only): in one write transaction every node's `kind` is renamed, the
    /// `kind_stats` row moves under `to`, the `node_candidate` rows of reason
    /// `discovered_kind` are deleted (EN-F5a), the `x:` `schema_type` row goes when no
    /// workspace counts it any more and one `events` row (`kind_promoted`) is written.
    ///
    /// # Errors
    /// `HumanOnly` for an agent actor; `Ontology(unknown_kind)` when `to` is not declared or
    /// `from` has no `kind_stats` row in `workspace`; backend errors.
    fn promote_kind(
        &self,
        workspace: &WorkspaceId,
        from: &str,
        to: &Qualified,
        actor: &Actor,
    ) -> Result<PromoteReport>;
    /// List, and unless `dry_run` remove, the `x:` kinds of `workspace` with `n_nodes <
    /// below.min_count` whose `last_seen` is older than `older_than` (EN-F5): their
    /// candidate nodes are tombstoned, their `node_candidate` and `kind_stats` rows deleted
    /// (the cap count drops), the `x:` `schema_type` row when no workspace counts it any more.
    /// A dry run writes nothing.
    ///
    /// # Errors
    /// Backend errors.
    fn prune_kinds(
        &self,
        workspace: &WorkspaceId,
        below: &PromoteWhen,
        older_than: Duration,
        dry_run: bool,
    ) -> Result<PruneReport>;
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
