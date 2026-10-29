//! Transaction-local source publication and Gazetteer complete-set state.

pub(super) struct MaintenanceContext<'a> {
    pub(super) setup: &'a crate::reconcile::Setup,
    pub(super) signer: &'a dyn kaban_policy::AuditSigner,
    pub(super) write: &'a WriteCtx,
}

/// Only artifact-only obsolete producers are qualified; historical claim versions are not.
pub(super) fn retire_producer(
    conn: &mut Connection,
    context: &MaintenanceContext<'_>,
    authority: kaban_core::CurrentProducerAuthority,
    old_producer: &str,
    batch_rows: u32,
) -> Result<kaban_core::RetireReport> {
    use kaban_core::{MaintenanceRefusal, RetireReport};
    if batch_rows == 0 {
        return Err(StoreError::EdgeRequiresAssertion);
    }
    if old_producer == authority.producer().producer_id() {
        return Ok(RetireReport {
            refused: Some(MaintenanceRefusal::CurrentProducer),
            ..RetireReport::default()
        });
    }
    let workspace = &context.write.workspace_id;
    let tracked: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM derived_artifact WHERE workspace_id=?1 AND layer='gazetteer' AND producer_id=?2)", params![workspace.as_str(),old_producer], |r| r.get(0)).sql()?;
    if !tracked {
        return Ok(RetireReport {
            refused: Some(MaintenanceRefusal::UnavailableProducer),
            ..RetireReport::default()
        });
    }
    let invalid_source: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM derived_artifact d LEFT JOIN node n ON n.rid=d.source_rid WHERE d.workspace_id=?1 AND d.layer='gazetteer' AND d.producer_id=?2 AND (n.rid IS NULL OR n.workspace_id<>?1 OR n.kind<>'Document' OR COALESCE(json_extract(n.props,'$.owner'),'')<>'kaban-ingest'))",params![workspace.as_str(),old_producer],|r|r.get(0)).sql()?;
    if invalid_source {
        return Err(StoreError::EdgeRequiresAssertion);
    }
    // Every live candidate is inspected before the first write. Current legacy claims are
    // known current authority, not evidence for any hypothetical historical producer.
    let preflight_head: i64 = conn
        .query_row(
            "SELECT COALESCE(max(id),0) FROM assertion WHERE workspace_id=?1 AND layer='gazetteer'",
            [workspace.as_str()],
            |r| r.get(0),
        )
        .sql()?;
    let mut after = 0;
    let mut untracked = 0;
    loop {
        let ids = conn.prepare_cached("SELECT id FROM assertion WHERE layer='gazetteer' AND workspace_id=?1 AND id>?2 ORDER BY id LIMIT ?3").sql()?.query_map(params![workspace.as_str(),after,batch_rows],|r|r.get::<_,i64>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            after = id;
            if let Some(claim) = persist::load_claim(conn, id)? {
                if let Some(source) = legacy_document(conn, &claim)? {
                    let candidate: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM derived_artifact d JOIN node n ON n.rid=d.source_rid WHERE d.workspace_id=?1 AND d.layer='gazetteer' AND d.producer_id=?2 AND n.id=?3)",params![workspace.as_str(),old_producer,source.as_str()],|r|r.get(0)).sql()?;
                    if candidate {
                        untracked += 1;
                    }
                } else {
                    untracked += 1;
                }
            }
        }
    }
    if untracked != 0 {
        return Ok(RetireReport {
            refused: Some(MaintenanceRefusal::UntrackedProducer),
            untracked,
            ..RetireReport::default()
        });
    }
    let mut report = RetireReport::default();
    loop {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        // Recheck candidate claims under the writer lock. A concurrent unknown claim cannot
        // turn artifact-only retirement into unsupported claim retirement.
        let current_head: i64 = tx.query_row("SELECT COALESCE(max(id),0) FROM assertion WHERE workspace_id=?1 AND layer='gazetteer'",[workspace.as_str()],|r|r.get(0)).sql()?;
        if current_head != preflight_head {
            return Err(StoreError::EdgeRequiresAssertion);
        }
        let keys = tx.prepare_cached("SELECT d.artifact_key FROM derived_artifact d JOIN node n ON n.rid=d.source_rid WHERE d.workspace_id=?1 AND d.layer='gazetteer' AND d.producer_id=?2 AND n.workspace_id=?1 AND n.kind='Document' AND json_extract(n.props,'$.owner')='kaban-ingest' ORDER BY d.artifact_key LIMIT ?3").sql()?.query_map(params![workspace.as_str(),old_producer,batch_rows],|r|r.get::<_,String>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
        if keys.is_empty() {
            tx.rollback().sql()?;
            break;
        }
        for key in &keys {
            tx.execute("DELETE FROM derived_artifact WHERE artifact_key=?1 AND workspace_id=?2 AND layer='gazetteer' AND producer_id=?3",params![key,workspace.as_str(),old_producer]).sql()?;
        }
        if report.transactions == 0 {
            crate::audit_table::append(&tx, context.signer, &context.write.audit)?;
        }
        tx.commit().sql()?;
        report.transactions += 1;
        report.rows += keys.len() as u64;
    }
    Ok(report)
}

pub(super) fn maintain_layer(
    conn: &mut Connection,
    context: &MaintenanceContext<'_>,
    authority: kaban_core::CurrentProducerAuthority,
    operation: kaban_core::LayerMaintenance,
    document_path: Option<&str>,
    batch_rows: u32,
) -> Result<kaban_core::RetireReport> {
    let ctx = context.write;
    if batch_rows == 0 {
        return Err(StoreError::EdgeRequiresAssertion);
    }
    let mut report = kaban_core::RetireReport::default();
    let mut reset_documents = BTreeSet::new();
    let mut audited = false;
    let document: Option<String> = if let Some(path) = document_path {
        Some(conn.query_row("SELECT id FROM node WHERE workspace_id=?1 AND kind='Document' AND json_extract(props,'$.owner')='kaban-ingest' AND json_extract(props,'$.path')=?2",params![ctx.workspace_id.as_str(),path],|r|r.get(0)).optional().sql()?.ok_or_else(||StoreError::NotFound("document".into()))?)
    } else {
        None
    };
    // Preflight the exact immutable population read-only: an unknown producer mapping
    // must refuse the request before any earlier page has been retired.
    let mut after = 0;
    loop {
        let ids = layer_claim_page(conn, &ctx.workspace_id, after, batch_rows)?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            after = id;
            if let Some(claim) = persist::load_claim(conn, id)?
                && legacy_document(conn, &claim)?.is_none()
            {
                report.untracked += 1;
            }
        }
    }
    if report.untracked != 0 {
        report.refused = Some(kaban_core::MaintenanceRefusal::UntrackedProducer);
        return Ok(report);
    }
    after = 0;
    loop {
        // Resolve the bounded page outside the write transaction. The historical source-id
        // lookup has no dedicated index; only exact node keys are revalidated under IMMEDIATE.
        let ids = layer_claim_page(conn, &ctx.workspace_id, after, batch_rows)?;
        if ids.is_empty() {
            break;
        }
        let mut resolved = Vec::with_capacity(ids.len());
        for id in ids {
            let claim = persist::load_claim(conn, id)?.ok_or(StoreError::EdgeRequiresAssertion)?;
            let source = legacy_document(conn, &claim)?.ok_or(StoreError::EdgeRequiresAssertion)?;
            resolved.push((id, source));
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let mut touched = BTreeSet::new();
        let mut rows = 0;
        for (id, source) in resolved {
            after = id;
            let claim = persist::load_claim(&tx, id)?.ok_or(StoreError::EdgeRequiresAssertion)?;
            if claim.rank == kaban_core::Rank::Deprecated {
                continue;
            }
            if claim.layer != kaban_core::Layer::Gazetteer
                || authority.producer() != kaban_core::BuiltinProducer::Gazetteer
            {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            if !validate_legacy_document(&tx, &claim, &source)? {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            if document
                .as_ref()
                .is_some_and(|selected| selected != source.as_str())
            {
                continue;
            }
            mark_retired(&tx, &claim, operation)?;
            let key = kaban_core::group_key(&claim);
            persist::ensure_status(&tx, id, &key, persist::now_unix())?;
            touched.insert(key);
            rows += 1;
        }
        reconcile::decide_groups(&tx, context.setup, &touched, persist::now_unix(), None)?;
        if rows != 0 && !audited {
            crate::audit_table::append(&tx, context.signer, &ctx.audit)?;
            audited = true;
        }
        tx.commit().sql()?;
        if rows != 0 {
            report.transactions += 1;
            report.rows += rows;
        }
    }
    loop {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let keys=tx.prepare_cached("SELECT d.artifact_key,n.id FROM derived_artifact d JOIN node n ON n.rid=d.source_rid WHERE d.workspace_id=?1 AND d.layer='gazetteer' AND n.workspace_id=?1 AND (?2 IS NULL OR n.id=?2) ORDER BY d.producer_id,d.artifact_key LIMIT ?3").sql()?.query_map(params![ctx.workspace_id.as_str(),document,batch_rows],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
        if keys.is_empty() {
            tx.rollback().sql()?;
            break;
        }
        for (key, source) in &keys {
            if operation == kaban_core::LayerMaintenance::Reindex
                && !reset_documents.contains(source)
            {
                let source_id = NodeId::parse(source)?;
                if self::document(&tx, &source_id, &ctx.workspace_id, false)?.is_some() {
                    reset_set(&tx, &source_id, &ctx.workspace_id)?;
                    reset_documents.insert(source.clone());
                }
            }
            tx.execute(
                "DELETE FROM derived_artifact WHERE artifact_key=?1 AND workspace_id=?2",
                params![key, ctx.workspace_id.as_str()],
            )
            .sql()?;
        }
        if !audited {
            crate::audit_table::append(&tx, context.signer, &ctx.audit)?;
            audited = true;
        }
        tx.commit().sql()?;
        report.transactions += 1;
        report.rows += keys.len() as u64;
    }
    if operation == kaban_core::LayerMaintenance::Reindex {
        let mut after = String::new();
        loop {
            // Bounded read-only keyset enumeration outside IMMEDIATE, including documents
            // whose last complete set was empty (no claim or artifact can identify them).
            let sources = if let Some(selected) = &document {
                if !after.is_empty() {
                    break;
                }
                vec![selected.clone()]
            } else {
                conn.prepare_cached("SELECT id FROM node WHERE workspace_id=?1 AND kind='Document' AND id>?2 AND lifecycle='active' AND json_extract(props,'$.owner')='kaban-ingest' ORDER BY id LIMIT ?3").sql()?.query_map(params![ctx.workspace_id.as_str(),after,batch_rows],|r|r.get::<_,String>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?
            };
            if sources.is_empty() {
                break;
            }
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .sql()?;
            let mut changed = false;
            for source in sources {
                after.clone_from(&source);
                if reset_documents.contains(&source) {
                    continue;
                }
                let source_id = NodeId::parse(&source)?;
                if self::document(&tx, &source_id, &ctx.workspace_id, false)?.is_some() {
                    reset_set(&tx, &source_id, &ctx.workspace_id)?;
                    reset_documents.insert(source);
                    changed = true;
                }
            }
            if changed && !audited {
                crate::audit_table::append(&tx, context.signer, &ctx.audit)?;
                audited = true;
            }
            tx.commit().sql()?;
            if changed {
                report.transactions += 1;
            }
        }
    }
    Ok(report)
}

use crate::reconcile::{self, persist};
use crate::sql_error::SqlResultExt;
use kaban_core::WriteCtx;
use kaban_core::{
    BatchSubject, BuiltinProducer, DerivedArtifact, GazetteerClaimSet, GovernedSet, Layer,
    Lifecycle, NodeId, Op, Owner, Result, SourceRev, StoreError, WorkspaceId, WriteBatch,
    artifact_key, canonical_json,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Default)]
struct ProducerState {
    generation: u64,
    fingerprint: String,
    source_rev: Option<SourceRev>,
    policy_version: Option<String>,
}

fn retirement_key(key: &str, workspace: &str) -> String {
    format!(
        "gazetteer-set-retired:{}",
        artifact_key(
            &SourceRev {
                content_hash: key.to_owned(),
                rev: 0
            },
            Layer::Gazetteer,
            "kaban-ingest:retirement:1",
            workspace,
            "provenance:1"
        )
        .as_str()
    )
}

/// Machine provenance is private metadata, never inferred from caller-controlled reasons.
pub(crate) fn machine_retired(conn: &Connection, key: &str, workspace: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM index_meta WHERE key=?1 AND value='1')",
        [retirement_key(key, workspace)],
        |r| r.get(0),
    )
    .sql()
}

/// An explicit human deprecation supersedes any prior machine retirement provenance.
pub(crate) fn clear_machine_retirement(
    conn: &Connection,
    key: &str,
    workspace: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM index_meta WHERE key=?1",
        [retirement_key(key, workspace)],
    )
    .sql()?;
    Ok(())
}

/// Source-indexed invalidation after a canonical source write, in that write's transaction.
pub(crate) fn invalidate_stale(conn: &Connection, rid: i64) -> Result<()> {
    let (hash, rev, policy, lifecycle): (String, i64, String, String) = conn
        .query_row(
            "SELECT content_hash,rev,policy_version,lifecycle FROM node WHERE rid=?1",
            [rid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .sql()?;
    let expected = serde_json::to_string(&SourceRev {
        content_hash: hash,
        rev,
    })?;
    conn.execute("DELETE FROM derived_artifact WHERE source_rid=?1 AND (source_rev <> ?2 OR policy_version <> ?3 OR ?4 <> 'active')",params![rid,expected,policy,lifecycle]).sql()?;
    Ok(())
}

fn state_key(document: &NodeId, workspace: &WorkspaceId) -> String {
    let source = SourceRev {
        content_hash: document.to_string(),
        rev: 0,
    };
    format!(
        "gazetteer-set:{}",
        artifact_key(
            &source,
            Layer::Gazetteer,
            "kaban-ingest:complete-set:1",
            workspace.as_str(),
            "state:1"
        )
        .as_str()
    )
}

fn state(conn: &Connection, document: &NodeId, workspace: &WorkspaceId) -> Result<ProducerState> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key=?1",
            [state_key(document, workspace)],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    value.map_or_else(
        || Ok(ProducerState::default()),
        |value| {
            let value: Value = serde_json::from_str(&value)?;
            Ok(ProducerState {
                generation: value["generation"]
                    .as_u64()
                    .ok_or(StoreError::EdgeRequiresAssertion)?,
                fingerprint: value["fingerprint"]
                    .as_str()
                    .ok_or(StoreError::EdgeRequiresAssertion)?
                    .to_owned(),
                source_rev: value
                    .get("source_rev")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?,
                policy_version: value["policy_version"].as_str().map(str::to_owned),
            })
        },
    )
}

fn document(
    conn: &Connection,
    id: &NodeId,
    workspace: &WorkspaceId,
    allow_tombstoned: bool,
) -> Result<Option<(SourceRev, Value)>> {
    let row: Option<(String, i64, String, String, String, String)> = conn
        .query_row(
            "SELECT content_hash, rev, props, kind, lifecycle, workspace_id FROM node WHERE id=?1",
            [id.as_str()],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()
        .sql()?;
    let Some((content_hash, rev, props, kind, lifecycle, ws)) = row else {
        return Ok(None);
    };
    let props: Value = serde_json::from_str(&props)?;
    if ws != workspace.as_str()
        || kind != "Document"
        || !(lifecycle == "active" || (allow_tombstoned && lifecycle == "tombstoned"))
        || props["owner"] != "kaban-ingest"
    {
        return Err(StoreError::NotFound(id.to_string()));
    }
    Ok(Some((SourceRev { content_hash, rev }, props)))
}

pub(super) fn generation(conn: &Connection, id: &NodeId, workspace: &WorkspaceId) -> Result<u64> {
    document(conn, id, workspace, true)?;
    Ok(state(conn, id, workspace)?.generation)
}

/// A completed private set must still bind the current canonical source and policy. Older
/// unbound state is unknown, not fresh; reset clears its fingerprint without changing nodes.
pub(super) fn set_fresh(conn: &Connection, id: &NodeId, workspace: &WorkspaceId) -> Result<bool> {
    let Some((revision, _)) = document(conn, id, workspace, false)? else {
        return Ok(false);
    };
    let state = state(conn, id, workspace)?;
    let policy: String = conn
        .query_row(
            "SELECT policy_version FROM node WHERE id=?1 AND workspace_id=?2",
            params![id.as_str(), workspace.as_str()],
            |row| row.get(0),
        )
        .sql()?;
    Ok(!state.fingerprint.is_empty()
        && state.source_rev.as_ref() == Some(&revision)
        && state.policy_version.as_deref() == Some(policy.as_str()))
}

/// Validate a historical v1 claim's exact source, tenant and ingest-owned chunk scope.
pub(super) fn legacy_document(
    conn: &Connection,
    claim: &kaban_core::Assertion,
) -> Result<Option<NodeId>> {
    if !BuiltinProducer::Gazetteer.matches_legacy_assertion(
        claim.layer,
        claim.extractor_id.as_deref(),
        claim.extractor_version.as_deref(),
    ) {
        return Ok(None);
    }
    let (Some(source), kaban_core::Subject::Edge { src, rel, .. }) =
        (&claim.source_id, &claim.subject)
    else {
        return Ok(None);
    };
    if rel != "MENTIONS" || claim.lineage_id != format!("gazetteer:{source}") {
        return Ok(None);
    }
    let chunk: Option<String> = conn.query_row("SELECT id FROM node WHERE rid=?1 AND workspace_id=?2 AND kind='Chunk' AND json_extract(props,'$.owner')='kaban-ingest'",params![src,claim.workspace_id.as_str()],|r|r.get(0)).optional().sql()?;
    let Some(chunk) = chunk else {
        return Ok(None);
    };
    let mut statement=conn.prepare_cached("SELECT id FROM node WHERE workspace_id=?1 AND kind='Document' AND json_extract(props,'$.owner')='kaban-ingest' AND json_extract(props,'$.source_id')=?2 AND EXISTS(SELECT 1 FROM json_each(json_extract(props,'$.chunk_ids')) WHERE value=?3) LIMIT 2").sql()?;
    let documents = statement
        .query_map(params![claim.workspace_id.as_str(), source, chunk], |r| {
            r.get::<_, String>(0)
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    if documents.len() != 1 {
        return Ok(None);
    }
    Ok(Some(NodeId::parse(&documents[0])?))
}

/// Indexed exact-key post-resolution validation; never scan the Document population in a write.
fn validate_legacy_document(
    conn: &Connection,
    claim: &kaban_core::Assertion,
    document: &NodeId,
) -> Result<bool> {
    let (Some(source), kaban_core::Subject::Edge { src, rel, .. }) =
        (&claim.source_id, &claim.subject)
    else {
        return Ok(false);
    };
    if !BuiltinProducer::Gazetteer.matches_legacy_assertion(
        claim.layer,
        claim.extractor_id.as_deref(),
        claim.extractor_version.as_deref(),
    ) || rel != "MENTIONS"
        || claim.lineage_id != format!("gazetteer:{source}")
    {
        return Ok(false);
    }
    conn.query_row("SELECT EXISTS(SELECT 1 FROM node d JOIN node c ON c.rid=?1 WHERE d.id=?2 AND d.workspace_id=?3 AND c.workspace_id=?3 AND d.kind='Document' AND c.kind='Chunk' AND json_extract(d.props,'$.owner')='kaban-ingest' AND json_extract(c.props,'$.owner')='kaban-ingest' AND json_extract(d.props,'$.source_id')=?4 AND EXISTS(SELECT 1 FROM json_each(json_extract(d.props,'$.chunk_ids')) WHERE value=c.id))",params![src,document.as_str(),claim.workspace_id.as_str(),source],|r|r.get(0)).sql()
}

/// Explicit replay invalidates the optimistic set generation without changing source nodes.
pub(super) fn reset_set(
    conn: &Connection,
    document: &NodeId,
    workspace: &WorkspaceId,
) -> Result<()> {
    let prior = state(conn, document, workspace)?;
    let generation = prior
        .generation
        .checked_add(1)
        .ok_or(StoreError::EdgeRequiresAssertion)?;
    let encoded = serde_json::to_string(&json!({"generation":generation,"fingerprint":""}))?;
    conn.execute("INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![state_key(document,workspace),encoded]).sql()?;
    Ok(())
}

/// Indexed page of historical machine claims, independent of missing statuses.
pub(super) fn layer_claim_page(
    conn: &Connection,
    workspace: &WorkspaceId,
    after: i64,
    limit: u32,
) -> Result<Vec<i64>> {
    conn.prepare_cached("SELECT id FROM assertion WHERE layer='gazetteer' AND workspace_id=?1 AND id>?2 AND rank<>'deprecated' ORDER BY id LIMIT ?3").sql()?.query_map(params![workspace.as_str(),after,limit],|r|r.get(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()
}

pub(super) fn mark_retired(
    conn: &Connection,
    claim: &kaban_core::Assertion,
    operation: kaban_core::LayerMaintenance,
) -> Result<()> {
    let reason = match operation {
        kaban_core::LayerMaintenance::Reindex => "producer-set:retired",
        kaban_core::LayerMaintenance::Forget => "layer:forgotten",
    };
    conn.execute(
        "UPDATE assertion SET rank='deprecated',rank_reason=?2 WHERE key=?1 AND rank<>'deprecated'",
        params![claim.key.0, reason],
    )
    .sql()?;
    if operation == kaban_core::LayerMaintenance::Reindex {
        conn.execute(
            "INSERT INTO index_meta(key,value) VALUES(?1,'1') ON CONFLICT(key) DO NOTHING",
            [retirement_key(&claim.key.0, claim.workspace_id.as_str())],
        )
        .sql()?;
    } else {
        clear_machine_retirement(conn, &claim.key.0, claim.workspace_id.as_str())?;
    }
    Ok(())
}

/// All optimistic guards run before the audit or any batch write.
pub(super) fn stale_sources(
    conn: &Connection,
    batch: &WriteBatch,
    policy: &str,
    governed: &GovernedSet,
) -> Result<u32> {
    if !batch.gazetteer_claim_sets.is_empty() && batch.edge_diff.owner() != Owner::KabanIngest {
        return Err(StoreError::EdgeRequiresAssertion);
    }
    let mut stale = BTreeSet::new();
    for artifact in &batch.artifacts {
        let allowed_owner = match artifact.layer {
            Layer::Gazetteer | Layer::V4 => Owner::KabanIngest,
            Layer::Script | Layer::Llm | Layer::Gliner => Owner::KabanParse,
            Layer::Reconcile => Owner::KabanReconcile,
            _ => return Err(StoreError::EdgeRequiresAssertion),
        };
        if artifact.workspace_id != batch.workspace_id
            || batch.edge_diff.owner() != allowed_owner
            || artifact.producer_id.is_empty()
            || artifact.policy_version != policy
        {
            return Err(StoreError::EdgeRequiresAssertion);
        }
        let source: Option<(String, i64)> = conn.query_row(
            "SELECT content_hash, rev FROM node WHERE rid=?1 AND workspace_id=?2 AND lifecycle='active'",
            params![artifact.source_rid, batch.workspace_id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional().sql()?;
        if source
            != Some((
                artifact.source_rev.content_hash.clone(),
                artifact.source_rev.rev,
            ))
        {
            stale.insert(format!("rid:{}", artifact.source_rid));
        } else if artifact.key
            != artifact_key(
                &artifact.source_rev,
                artifact.layer,
                &artifact.producer_id,
                &artifact.config_hash,
                &artifact.policy_version,
            )
        {
            return Err(StoreError::EdgeRequiresAssertion);
        }
    }
    let mut documents = BTreeSet::new();
    for set in &batch.gazetteer_claim_sets {
        if !documents.insert(set.document.as_str()) || set.source_id.is_empty() {
            return Err(StoreError::EdgeRequiresAssertion);
        }
        let reactivates = batch
            .ops
            .iter()
            .any(|op| matches!(op, Op::Reactivate { id } if id == &set.document));
        let current = document(conn, &set.document, &batch.workspace_id, reactivates)?;
        if reactivates && current.is_some() {
            let lifecycle: String = conn
                .query_row(
                    "SELECT lifecycle FROM node WHERE id=?1 AND workspace_id=?2",
                    params![set.document.as_str(), batch.workspace_id.as_str()],
                    |r| r.get(0),
                )
                .sql()?;
            if lifecycle == "tombstoned" {
                governed.reconciliation_transition(
                    Lifecycle::Tombstoned,
                    "Document",
                    Some("kaban-ingest"),
                )?;
            }
        }
        if let Some((_, props)) = &current
            && props["source_id"] != set.source_id
        {
            return Err(StoreError::EdgeRequiresAssertion);
        }
        if current.map(|(rev, _)| rev) != set.source_rev
            || state(conn, &set.document, &batch.workspace_id)?.generation
                != set.expected_generation
        {
            stale.insert(set.document.to_string());
        }
    }
    Ok(u32::try_from(stale.len()).unwrap_or(u32::MAX))
}

/// Validate post-upsert ownership, derive stable set state, and stamp claim identities.
pub(super) fn prepare_sets(
    conn: &Connection,
    batch: &mut WriteBatch,
) -> Result<Vec<(GazetteerClaimSet, String, String)>> {
    let mut prepared = Vec::new();
    for set in &batch.gazetteer_claim_sets {
        let (revision, props) = document(conn, &set.document, &batch.workspace_id, false)?
            .ok_or_else(|| StoreError::NotFound(set.document.to_string()))?;
        if props["source_id"] != set.source_id {
            return Err(StoreError::EdgeRequiresAssertion);
        }
        let lineage = format!("gazetteer:{}", set.source_id);
        let mut identities = Vec::new();
        for claim in &batch.assertions {
            if claim.layer == Layer::Gazetteer
                && claim.source_id.as_deref() == Some(set.source_id.as_str())
                && claim.lineage_id != lineage
            {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            if claim.layer != Layer::Gazetteer || claim.lineage_id != lineage {
                continue;
            }
            if claim.source_id.as_deref() != Some(set.source_id.as_str()) {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            // Complete-set callers submit the closed v1 extractor. Generation stamping
            // is store-owned and must never disguise an unknown producer as this builtin.
            if claim.extractor_id.is_some() || claim.extractor_version.as_deref() != Some("1") {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            let chunk = match &claim.subject {
                BatchSubject::Edge { src, rel, .. } if rel == "MENTIONS" => src,
                _ => return Err(StoreError::EdgeRequiresAssertion),
            };
            if !props["chunk_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(chunk.as_str())))
            {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            let owned: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM node WHERE id=?1 AND workspace_id=?2 AND kind='Chunk' AND lifecycle='active' AND json_extract(props,'$.owner')='kaban-ingest')", params![chunk.as_str(), batch.workspace_id.as_str()], |r| r.get(0)).sql()?;
            if !owned {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            identities.push(canonical_json(&serde_json::to_value(claim)?));
        }
        identities.sort();
        identities.dedup();
        let nonempty = !identities.is_empty();
        let canonical = canonical_json(
            &json!({"source":set.source_id,"revision":revision,"config":set.config_hash,"claims":identities}),
        );
        let fingerprint = artifact_key(
            &SourceRev {
                content_hash: canonical,
                rev: 0,
            },
            Layer::Gazetteer,
            "kaban-ingest:set-fingerprint:1",
            "",
            "",
        )
        .as_str()
        .to_owned();
        let prior = state(conn, &set.document, &batch.workspace_id)?;
        let generation = if fingerprint == prior.fingerprint {
            prior.generation
        } else {
            prior
                .generation
                .checked_add(1)
                .ok_or(StoreError::EdgeRequiresAssertion)?
        };
        for claim in &mut batch.assertions {
            if claim.layer == Layer::Gazetteer && claim.lineage_id == lineage {
                claim.extractor_version = Some(format!(
                    "{}:set:{generation}",
                    claim.extractor_version.as_deref().unwrap_or("1")
                ));
            }
        }
        let (rid, policy): (i64, String) = conn
            .query_row(
                "SELECT rid,policy_version FROM node WHERE id=?1 AND workspace_id=?2",
                params![set.document.as_str(), batch.workspace_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .sql()?;
        let encoded = serde_json::to_string(
            &json!({"generation":generation,"fingerprint":fingerprint,"source_rev":revision,"policy_version":policy}),
        )?;
        if nonempty {
            let producer = BuiltinProducer::Gazetteer.producer_id();
            let key = artifact_key(
                &revision,
                Layer::Gazetteer,
                producer,
                &set.config_hash,
                &policy,
            );
            conn.execute("DELETE FROM derived_artifact WHERE source_rid=?1 AND layer='gazetteer' AND workspace_id=?2 AND producer_id=?3 AND artifact_key<>?4", params![rid,batch.workspace_id.as_str(),producer,key.as_str()]).sql()?;
            batch.artifacts.push(DerivedArtifact {
                key,
                workspace_id: batch.workspace_id.clone(),
                layer: Layer::Gazetteer,
                producer_id: producer.to_owned(),
                source_rid: rid,
                source_rev: revision,
                config_hash: set.config_hash.clone(),
                policy_version: policy,
                published_at: persist::now_unix(),
            });
        } else {
            conn.execute("DELETE FROM derived_artifact WHERE source_rid=?1 AND layer='gazetteer' AND workspace_id=?2",params![rid,batch.workspace_id.as_str()]).sql()?;
        }
        prepared.push((set.clone(), lineage, encoded));
    }
    Ok(prepared)
}

/// Retire only this tenant's declared Gazetteer lineage, keeping submitted claim keys.
pub(super) fn retire_sets(
    conn: &Connection,
    workspace: &WorkspaceId,
    prepared: &[(GazetteerClaimSet, String, String)],
    kept: &BTreeSet<String>,
    touched: &mut BTreeSet<String>,
    now: i64,
) -> Result<()> {
    for (set, lineage, encoded) in prepared {
        let rows: Vec<(i64, String)> = conn.prepare_cached("SELECT id,key FROM assertion WHERE lineage_id=?1 AND workspace_id=?2 AND layer='gazetteer'").sql()?.query_map(params![lineage, workspace.as_str()], |r| Ok((r.get(0)?, r.get(1)?))).sql()?.collect::<rusqlite::Result<_>>().sql()?;
        for (id, key) in rows {
            if kept.contains(&key)
                || crate::pruning_protection::claim(conn, workspace.as_str(), id)?
            {
                continue;
            }
            if let Some(claim) = persist::load_claim(conn, id)? {
                touched.insert(kaban_core::group_key(&claim));
                let changed=conn.execute("UPDATE assertion SET rank='deprecated',rank_reason='producer-set:retired' WHERE id=?1 AND rank <> 'deprecated'", [id]).sql()?;
                if changed != 0 {
                    conn.execute("INSERT INTO index_meta(key,value) VALUES(?1,'1') ON CONFLICT(key) DO NOTHING",[retirement_key(&key,workspace.as_str())]).sql()?;
                }
                persist::ensure_status(conn, id, &kaban_core::group_key(&claim), now)?;
                persist::set_status(conn, id, kaban_core::AssertionStatus::Rejected, now)?;
            }
        }
        conn.execute("INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value <> excluded.value", params![state_key(&set.document, workspace), encoded]).sql()?;
    }
    Ok(())
}

pub(super) fn publish(conn: &Connection, artifacts: &[DerivedArtifact]) -> Result<()> {
    for a in artifacts {
        let existing: Option<(String,String,String,i64,String,String,String)> = conn.query_row("SELECT workspace_id,layer,producer_id,source_rid,source_rev,config_hash,policy_version FROM derived_artifact WHERE artifact_key=?1",[a.key.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional().sql()?;
        if let Some((workspace, layer, producer, rid, revision, config, policy)) = existing {
            if workspace != a.workspace_id.as_str()
                || layer != a.layer.as_str()
                || producer != a.producer_id
                || rid != a.source_rid
                || serde_json::from_str::<SourceRev>(&revision)? != a.source_rev
                || config != a.config_hash
                || policy != a.policy_version
            {
                return Err(StoreError::EdgeRequiresAssertion);
            }
            continue;
        }
        let current: Option<(String,i64)> = conn.query_row("SELECT content_hash,rev FROM node WHERE rid=?1 AND workspace_id=?2 AND lifecycle='active'",params![a.source_rid,a.workspace_id.as_str()],|r|Ok((r.get(0)?,r.get(1)?))).optional().sql()?;
        if current != Some((a.source_rev.content_hash.clone(), a.source_rev.rev)) {
            return Err(StoreError::EdgeRequiresAssertion);
        }
        conn.execute("INSERT INTO derived_artifact(artifact_key,workspace_id,layer,producer_id,source_rid,source_rev,config_hash,policy_version,published_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(artifact_key) DO NOTHING", params![a.key.as_str(),a.workspace_id.as_str(),a.layer.as_str(),a.producer_id,a.source_rid,serde_json::to_string(&a.source_rev)?,a.config_hash,a.policy_version,a.published_at]).sql()?;
    }
    Ok(())
}
