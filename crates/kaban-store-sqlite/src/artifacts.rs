//! Transaction-local source publication and Gazetteer complete-set state.

pub(super) struct MaintenanceContext<'a> {
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
        return Err(StoreError::EdgeSetRefused);
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
        return Err(StoreError::EdgeSetRefused);
    }
    let mut report = RetireReport::default();
    loop {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let keys = tx.prepare_cached("SELECT d.artifact_key FROM derived_artifact d JOIN node n ON n.rid=d.source_rid WHERE d.workspace_id=?1 AND d.layer='gazetteer' AND d.producer_id=?2 AND n.workspace_id=?1 AND n.kind='Document' AND json_extract(n.props,'$.owner')='kaban-ingest' ORDER BY d.artifact_key LIMIT ?3").sql()?.query_map(params![workspace.as_str(),old_producer,batch_rows],|r|r.get::<_,String>(0)).sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
        if keys.is_empty() {
            tx.rollback().sql()?;
            break;
        }
        for key in &keys {
            tx.execute("DELETE FROM derived_artifact WHERE artifact_key=?1 AND workspace_id=?2 AND layer='gazetteer' AND producer_id=?3",params![key,workspace.as_str(),old_producer]).sql()?;
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
    // the current-producer check was a claim check; the bridge until `forget_layer` goes
    // (OD-P1, T3 slice B) keeps the parameter
    _authority: kaban_core::CurrentProducerAuthority,
    operation: kaban_core::LayerMaintenance,
    document_path: Option<&str>,
    batch_rows: u32,
) -> Result<kaban_core::RetireReport> {
    let ctx = context.write;
    if batch_rows == 0 {
        return Err(StoreError::EdgeSetRefused);
    }
    let mut report = kaban_core::RetireReport::default();
    let mut reset_documents = BTreeSet::new();
    let document: Option<String> = if let Some(path) = document_path {
        Some(conn.query_row("SELECT id FROM node WHERE workspace_id=?1 AND kind='Document' AND json_extract(props,'$.owner')='kaban-ingest' AND json_extract(props,'$.path')=?2",params![ctx.workspace_id.as_str(),path],|r|r.get(0)).optional().sql()?.ok_or_else(||StoreError::NotFound("document".into()))?)
    } else {
        None
    };
    // the layer's mention edges go first, in bounded transactions
    loop {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let rows = tx.execute("DELETE FROM edge WHERE rowid IN (SELECT e.rowid FROM edge e WHERE e.workspace_id=?1 AND e.layer='gazetteer' AND (?2 IS NULL OR e.source_id=(SELECT json_extract(props,'$.source_id') FROM node WHERE id=?2 AND workspace_id=?1)) LIMIT ?3)", params![ctx.workspace_id.as_str(), document, batch_rows]).sql()?;
        tx.commit().sql()?;
        if rows == 0 {
            break;
        }
        report.transactions += 1;
        report.rows += rows as u64;
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
            tx.commit().sql()?;
            if changed {
                report.transactions += 1;
            }
        }
    }
    Ok(report)
}

use crate::sql_error::SqlResultExt;
use kaban_core::WriteCtx;
use kaban_core::{
    BuiltinProducer, DerivedArtifact, GazetteerClaimSet, GovernedSet, Layer, Lifecycle, NodeId, Op,
    Owner, Result, SourceRev, StoreError, WorkspaceId, WriteBatch, artifact_key, canonical_json,
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

/// Source-indexed invalidation after a canonical source write, in that write's transaction.
pub(crate) fn invalidate_stale(conn: &Connection, rid: i64) -> Result<()> {
    let (hash, rev, lifecycle): (String, i64, String) = conn
        .query_row(
            "SELECT content_hash,rev,lifecycle FROM node WHERE rid=?1",
            [rid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .sql()?;
    let expected = serde_json::to_string(&SourceRev {
        content_hash: hash,
        rev,
    })?;
    conn.execute(
        "DELETE FROM derived_artifact WHERE source_rid=?1 AND (source_rev <> ?2 OR ?3 <> 'active')",
        params![rid, expected, lifecycle],
    )
    .sql()?;
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
                    .ok_or(StoreError::EdgeSetRefused)?,
                fingerprint: value["fingerprint"]
                    .as_str()
                    .ok_or(StoreError::EdgeSetRefused)?
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
        .ok_or(StoreError::EdgeSetRefused)?;
    let encoded = serde_json::to_string(&json!({"generation":generation,"fingerprint":""}))?;
    conn.execute("INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![state_key(document,workspace),encoded]).sql()?;
    Ok(())
}

/// All optimistic guards run before any batch write.
pub(super) fn stale_sources(
    conn: &Connection,
    batch: &WriteBatch,
    governed: &GovernedSet,
) -> Result<u32> {
    if !batch.gazetteer_claim_sets.is_empty() && batch.edge_diff.owner() != Owner::KabanIngest {
        return Err(StoreError::EdgeSetRefused);
    }
    let mut stale = BTreeSet::new();
    for artifact in &batch.artifacts {
        let allowed_owner = match artifact.layer {
            Layer::Gazetteer | Layer::V4 => Owner::KabanIngest,
            Layer::Script | Layer::Llm | Layer::Gliner => Owner::KabanParse,
            Layer::Reconcile => Owner::KabanReconcile,
            _ => return Err(StoreError::EdgeSetRefused),
        };
        if artifact.workspace_id != batch.workspace_id
            || batch.edge_diff.owner() != allowed_owner
            || artifact.producer_id.is_empty()
        {
            return Err(StoreError::EdgeSetRefused);
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
            return Err(StoreError::EdgeSetRefused);
        }
    }
    let mut documents = BTreeSet::new();
    for set in &batch.gazetteer_claim_sets {
        if !documents.insert(set.document.as_str()) || set.source_id.is_empty() {
            return Err(StoreError::EdgeSetRefused);
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
            return Err(StoreError::EdgeSetRefused);
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

/// Validate post-upsert ownership and derive stable set state from the batch's mention edges.
pub(super) fn prepare_sets(
    conn: &Connection,
    batch: &mut WriteBatch,
) -> Result<Vec<(GazetteerClaimSet, String)>> {
    let mut prepared = Vec::new();
    for set in &batch.gazetteer_claim_sets {
        let (revision, props) = document(conn, &set.document, &batch.workspace_id, false)?
            .ok_or_else(|| StoreError::NotFound(set.document.to_string()))?;
        if props["source_id"] != set.source_id {
            return Err(StoreError::EdgeSetRefused);
        }
        let mut identities = Vec::new();
        for op in &batch.ops {
            let Op::UpsertEdge {
                src,
                rel,
                props: Some(props),
                ..
            } = op
            else {
                continue;
            };
            if props["layer"] != Layer::Gazetteer.as_str() || props["source_id"] != set.source_id {
                continue;
            }
            if rel != "MENTIONS"
                || !props_chunk(&props_of(conn, &set.document, &batch.workspace_id)?, src)
            {
                return Err(StoreError::EdgeSetRefused);
            }
            let owned: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM node WHERE id=?1 AND workspace_id=?2 AND kind='Chunk' AND lifecycle='active' AND json_extract(props,'$.owner')='kaban-ingest')", params![src.as_str(), batch.workspace_id.as_str()], |r| r.get(0)).sql()?;
            if !owned {
                return Err(StoreError::EdgeSetRefused);
            }
            identities.push(canonical_json(&serde_json::to_value(op)?));
        }
        identities.sort();
        identities.dedup();
        let nonempty = !identities.is_empty();
        let canonical = canonical_json(
            &json!({"source":set.source_id,"revision":revision,"config":set.config_hash,"edges":identities}),
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
                .ok_or(StoreError::EdgeSetRefused)?
        };
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
                published_at: crate::edge_write::now_unix(),
            });
        } else {
            conn.execute("DELETE FROM derived_artifact WHERE source_rid=?1 AND layer='gazetteer' AND workspace_id=?2",params![rid,batch.workspace_id.as_str()]).sql()?;
        }
        prepared.push((set.clone(), encoded));
    }
    Ok(prepared)
}

/// Record each set's state and delete this tenant's Gazetteer mention edges of the set's source
/// that the batch did not name (`kept`: the batch's edge triples by row id).
pub(super) fn record_sets(
    conn: &Connection,
    workspace: &WorkspaceId,
    prepared: &[(GazetteerClaimSet, String)],
    kept: &[(i64, i64, String)],
) -> Result<()> {
    for (set, encoded) in prepared {
        let rows: Vec<(i64, i64, String)> = conn.prepare_cached("SELECT src,dst,rel FROM edge WHERE workspace_id=?1 AND rel='MENTIONS' AND layer='gazetteer' AND source_id=?2").sql()?.query_map(params![workspace.as_str(), set.source_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).sql()?.collect::<rusqlite::Result<_>>().sql()?;
        for row in rows {
            if kept.contains(&row) {
                continue;
            }
            conn.execute(
                "DELETE FROM edge WHERE src=?1 AND dst=?2 AND rel=?3",
                params![row.0, row.1, row.2],
            )
            .sql()?;
        }
        conn.execute("INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value <> excluded.value", params![state_key(&set.document, workspace), encoded]).sql()?;
    }
    Ok(())
}

/// The stored props of an ingest Document (empty when it is not one).
fn props_of(conn: &Connection, document: &NodeId, workspace: &WorkspaceId) -> Result<Value> {
    Ok(self::document(conn, document, workspace, false)?.map_or(Value::Null, |(_, props)| props))
}

/// Whether `chunk` is one of the Document `props`' `chunk_ids`.
fn props_chunk(props: &Value, chunk: &NodeId) -> bool {
    props["chunk_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(chunk.as_str())))
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
                return Err(StoreError::EdgeSetRefused);
            }
            continue;
        }
        let current: Option<(String,i64)> = conn.query_row("SELECT content_hash,rev FROM node WHERE rid=?1 AND workspace_id=?2 AND lifecycle='active'",params![a.source_rid,a.workspace_id.as_str()],|r|Ok((r.get(0)?,r.get(1)?))).optional().sql()?;
        if current != Some((a.source_rev.content_hash.clone(), a.source_rev.rev)) {
            return Err(StoreError::EdgeSetRefused);
        }
        conn.execute("INSERT INTO derived_artifact(artifact_key,workspace_id,layer,producer_id,source_rid,source_rev,config_hash,policy_version,published_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(artifact_key) DO NOTHING", params![a.key.as_str(),a.workspace_id.as_str(),a.layer.as_str(),a.producer_id,a.source_rid,serde_json::to_string(&a.source_rev)?,a.config_hash,a.policy_version,a.published_at]).sql()?;
    }
    Ok(())
}
