//! The document index status model (docs-ingest-plan.md DI1.6, §16 item 3 (c); docs-ingest-spec
//! D6, D7): the `index_status` tool, the `kaban://documents` listing and the `index_docs`
//! report, computed from the stored Document nodes through the `kaban-core` role traits only
//! (no SQL here) and without materialising the index (DI2.0 (c)). `documents_by_status.missing`
//! counts the `Document{status: missing}` placeholders of unresolved wikilink targets (D2,
//! `crate::graph`): one per missing target, upgraded in place (so no longer `missing`) when the
//! target file arrives; `list_nodes("Document", ..)` lists them too, so they are inside
//! [`documents_resource`]'s total.
//!
//! ```text
//!   IngestStore::document_index_summary("Document", ws)   (SQL aggregates)  ==> IndexStatus
//!     documents_by_status, chunks, blocked_chunks, table_unknown_chunks, failed[], rule_ids
//!   IngestStore::list_nodes("Document", ws, offset, limit) (one page, id order) ==> documents[]
//!   IngestStore::index_meta()[ingest_last_run:<ws>]                             ==> last_run
//!   MemoryStore::stats(ws)["outbox_lag"]                                        ==> outbox_lag
//! ```

use crate::extract::extractor_for;
use crate::ids::PIPELINE_VERSION;
use crate::model::{Format, IngestError, IngestReport};
use kaban_api::{
    DocumentSummary, DocumentsResource, FailedDocument, IndexDocsResponse, IndexStatus,
};
use kaban_core::{GraphStore, Node, WorkspaceId};
use serde_json::Value;

/// `index_meta` key prefix of a workspace's last run stamp (RFC 3339 value).
const LAST_RUN_KEY: &str = "ingest_last_run:";

/// The service maximum of one listing page (`IngestStore::list_nodes` clamps there too).
const PAGE: usize = 500;

/// The one wire shape of a run report.
impl From<IngestReport> for IndexDocsResponse {
    fn from(r: IngestReport) -> Self {
        Self {
            gazetteer: r.gazetteer,
            workspace: String::new(),
            documents: r.documents,
            skipped: r.skipped,
            skipped_forgotten: r.skipped_forgotten,
            sections_tombstoned: r.sections_tombstoned,
            failed: r.failed,
            blocked: r.blocked,
            partial: r.partial,
            chunks_upserted: r.chunks_upserted,
            chunks_unchanged: r.chunks_unchanged,
            chunks_tombstoned: r.chunks_tombstoned,
            blocked_chunks: r.blocked_chunks,
            rule_ids: r.rule_ids,
            outbox_rows: r.outbox_rows,
            elapsed_ms: r.elapsed_ms,
            cancelled: r.cancelled,
            timed_out: r.timed_out,
            dry_run: false,
            receipt: None,
        }
    }
}

/// Stamp the end of an ingestion run on the index (`last_run` of [`index_status`]).
///
/// # Errors
/// Store errors.
pub fn record_last_run(store: &dyn GraphStore, workspace: &WorkspaceId) -> Result<(), IngestError> {
    store.set_index_meta(
        &format!("{LAST_RUN_KEY}{workspace}"),
        &kaban_core::now_iso(),
    )?;
    Ok(())
}

fn prop_str<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.props.get(key).and_then(Value::as_str)
}

fn chunk_count(node: &Node) -> u32 {
    node.props
        .get("chunk_ids")
        .and_then(Value::as_array)
        .map_or(0, |a| u32::try_from(a.len()).unwrap_or(u32::MAX))
}

/// One row of the listing from a stored Document node.
fn summary(node: &Node) -> DocumentSummary {
    DocumentSummary {
        path: prop_str(node, "path").unwrap_or_default().to_owned(),
        status: prop_str(node, "status").unwrap_or("unknown").to_owned(),
        chunks: chunk_count(node),
        lang: prop_str(node, "lang").map(str::to_owned),
        title: prop_str(node, "title").map(str::to_owned),
    }
}

fn saturated(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The status of the workspace's document index (docs-ingest-spec D7): the store's aggregates
/// over the active Document nodes (no node is loaded), the last-run stamp and the outbox lag.
///
/// # Errors
/// Store errors.
pub fn index_status(
    store: &dyn GraphStore,
    workspace: &WorkspaceId,
) -> Result<IndexStatus, IngestError> {
    let summary = store.document_index_summary("Document", workspace.as_str())?;
    let mut status = IndexStatus {
        pipeline_version: PIPELINE_VERSION,
        extractors: [Format::Markdown, Format::Text, Format::Pdf]
            .into_iter()
            .filter_map(extractor_for)
            .map(|e| e.id().to_owned())
            .collect(),
        documents_by_status: summary
            .by_status
            .into_iter()
            .map(|(status, n)| (status, saturated(n)))
            .collect(),
        chunks: summary.chunks,
        blocked_chunks: summary.blocked_chunks,
        table_unknown_chunks: summary.table_unknown_chunks,
        failed: summary
            .failed
            .into_iter()
            .map(|row| FailedDocument {
                path: row.path,
                error: row.error,
            })
            .collect(),
        rule_ids: summary
            .rule_ids
            .into_iter()
            .map(|(rule, n)| (rule, saturated(n)))
            .collect(),
        ..IndexStatus::default()
    };
    status.extractors.dedup();
    status.last_run = store
        .index_meta()?
        .entries
        .remove(&format!("{LAST_RUN_KEY}{workspace}"));
    status.gazetteer = store
        .index_meta()?
        .entries
        .get(&format!("gazetteer_last_run:{workspace}"))
        .and_then(|raw| serde_json::from_str(raw).ok());
    status.outbox_lag = store
        .stats(Some(workspace.as_str()))?
        .get("outbox_lag")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(status)
}

/// The `kaban://documents` body: the status plus one page of the listing, read as one page
/// through the store role (id order, so pages are stable and consistent with each other;
/// `offset` rows skipped, `limit` clamped to the service maximum). The total behind `more` is
/// the sum of `documents_by_status`; a page past the end is empty and still carries the status.
///
/// # Errors
/// Store errors.
pub fn documents_resource(
    store: &dyn GraphStore,
    workspace: &WorkspaceId,
    offset: usize,
    limit: usize,
) -> Result<DocumentsResource, IngestError> {
    let status = index_status(store, workspace)?;
    let limit = limit.clamp(1, PAGE);
    let total: usize = status
        .documents_by_status
        .values()
        .map(|&n| usize::try_from(n).unwrap_or(usize::MAX))
        .fold(0, usize::saturating_add);
    let documents: Vec<DocumentSummary> = store
        .list_nodes("Document", workspace.as_str(), offset, limit)?
        .iter()
        .map(summary)
        .collect();
    Ok(DocumentsResource {
        more: offset.saturating_add(documents.len()) < total,
        status,
        documents,
        offset,
        limit,
    })
}

/// Map a whole-run failure onto the service error vocabulary (exit codes per core-spec F7):
/// store errors keep their code, cancellation and exhaustion are invalid-request class.
impl From<IngestError> for kaban_api::ApiError {
    fn from(e: IngestError) -> Self {
        match e {
            IngestError::Store(s) => kaban_api::ApiError::Store(s),
            IngestError::Api(a) => a,
            IngestError::Policy(p) => kaban_api::ApiError::Policy(p),
            other => kaban_api::ApiError::Invalid(other.to_string()),
        }
    }
}
