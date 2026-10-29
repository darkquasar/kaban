//! Metadata-only persistent jobs on immutable migration 0004. All predicates bind tenant.
//! Helpers accept an existing connection/transaction so future submit/finish can use the
//! graph transaction. Separate public registry calls do NOT provide graph/job atomicity.

use crate::sql_error::SqlResultExt;
use kaban_core::{BackendSource, IngestJob, JobId, JobStatus, Result, StoreError, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, Row, params};

pub(crate) fn layer(dialect: &str) -> Result<kaban_core::Layer> {
    match dialect {
        "llm" => Ok(kaban_core::Layer::Llm),
        "gliner" => Ok(kaban_core::Layer::Gliner),
        "none" | "script" => Ok(kaban_core::Layer::Script),
        _ => Err(StoreError::InvalidName("claim".into(), "invalid_layer")),
    }
}

fn lease_name(job: &IngestJob, document: &str) -> String {
    format!(
        "ingest:{}:{}:{}@{}",
        job.workspace, job.dialect, document, job.content_hash
    )
}

fn attempt_name(lease: &str) -> String {
    format!("attempt:{lease}")
}

/// Private permanent high-water mark. No retention or reset predicate matches this namespace.
fn allocate_fence(conn: &Connection, workspace: &WorkspaceId, floor: u64) -> Result<u64> {
    let first = sql_count(floor)?
        .checked_add(1)
        .ok_or(StoreError::InvalidName("claim".into(), "too_large"))?;
    let value: Option<i64> = conn
        .query_row(
            "INSERT INTO leases(name,owner,fence,expires_at) VALUES (?1,'kaban-ingest',?2,0)
         ON CONFLICT(name) DO UPDATE SET fence=max(leases.fence+1,excluded.fence)
         WHERE leases.fence<9223372036854775807 RETURNING fence",
            params![format!("ingest-fence:{workspace}"), first],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    value
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(StoreError::InvalidName("claim".into(), "too_large"))
}

pub(crate) fn pending(
    conn: &Connection,
    workspace: &WorkspaceId,
    dialect: &str,
    prefix: Option<&str>,
    limit: u32,
) -> Result<Vec<kaban_core::PendingJob>> {
    let layer = layer(dialect)?;
    let rows = conn.prepare("SELECT n.id,json_extract(n.props,'$.path'),n.content_hash,n.rev FROM node n
        WHERE n.workspace_id=?1 AND n.kind='Document' AND n.lifecycle='active'
        AND json_extract(n.props,'$.owner')='kaban-ingest' AND json_type(n.props,'$.path')='text'
        AND (?3 IS NULL OR substr(json_extract(n.props,'$.path'),1,length(?3))=?3)
        AND NOT EXISTS(SELECT 1 FROM derived_artifact a WHERE a.source_rid=n.rid AND a.workspace_id=?1 AND a.layer=?2
          AND json_extract(a.source_rev,'$.content_hash')=n.content_hash)
        ORDER BY json_extract(n.props,'$.path'),n.id LIMIT ?4").sql()?
        .query_map(params![workspace.as_str(),layer.as_str(),prefix,limit], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))
        .sql()?.collect::<rusqlite::Result<Vec<_>>>().sql()?;
    rows.into_iter()
        .map(|(id, path, content_hash, rev)| {
            Ok(kaban_core::PendingJob {
                document: kaban_core::NodeId::parse(&id)
                    .map_err(|_| StoreError::InvalidName("job".into(), "stale"))?,
                path,
                source_rev: kaban_core::SourceRev { content_hash, rev },
            })
        })
        .collect()
}

pub(crate) fn binding(job: &IngestJob) -> Result<Option<kaban_core::JobLease>> {
    let plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
    plan.get("lease")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()).map_err(StoreError::from))
        .transpose()
}

/// Validation precedes every lifecycle mutation; only retained finished retries skip expiry.
pub(crate) fn holder(
    conn: &Connection,
    job: &IngestJob,
    fence: Option<u64>,
    renew: bool,
) -> Result<()> {
    let Some(bound) = binding(job)? else {
        return if fence.is_none() {
            Ok(())
        } else {
            Err(StoreError::LeaseLost {
                name: "ingest".into(),
                fence: fence.unwrap_or(0),
            })
        };
    };
    let lost = || StoreError::LeaseLost {
        name: bound.name.clone(),
        fence: fence.unwrap_or(0),
    };
    if fence != Some(bound.fence) {
        return Err(lost());
    }
    let row: Option<(String, i64, i64, i64)> = conn
        .query_row(
            "SELECT owner,fence,expires_at,unixepoch() FROM leases WHERE name=?",
            [&bound.name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .sql()?;
    let Some((owner, current, expires, now)) = row else {
        return Err(lost());
    };
    if owner != bound.owner
        || u64::try_from(current).ok() != Some(bound.fence)
        || (job.status != JobStatus::Finished && expires <= now)
    {
        return Err(lost());
    }
    if renew && job.status != JobStatus::Finished {
        crate::leases::renew(
            conn,
            &kaban_core::Lease {
                name: bound.name,
                owner: bound.owner,
                fence: bound.fence,
                expires_at: expires,
            },
            bound.ttl_s,
        )?;
    }
    Ok(())
}

pub(crate) fn claim(
    conn: &Connection,
    request: kaban_core::JobClaimRequest,
) -> Result<Option<kaban_core::Lease>> {
    let mut job = request.job;
    let mut plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
    let document = plan["document_id"]
        .as_str()
        .ok_or(StoreError::InvalidName("job".into(), "stale"))?
        .to_owned();
    let chunks: Vec<kaban_core::NodeId> = plan["chunks"]
        .as_array()
        .ok_or(StoreError::InvalidName("job".into(), "stale"))?
        .iter()
        .map(|v| serde_json::from_value(v["id"].clone()).map_err(StoreError::from))
        .collect::<Result<_>>()?;
    let document_id = kaban_core::NodeId::parse(&document)
        .map_err(|_| StoreError::InvalidName("job".into(), "stale"))?;
    check_source(conn, &job, &document_id, &request.source_rev, &chunks)?;
    let marked: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM derived_artifact a JOIN node n ON n.rid=a.source_rid WHERE n.id=?1 AND a.workspace_id=?2 AND a.layer=?3 AND json_extract(a.source_rev,'$.content_hash')=n.content_hash)",params![document,job.workspace.as_str(),layer(&job.dialect)?.as_str()],|r|r.get(0)).sql()?;
    if marked {
        return Ok(None);
    }
    let name = lease_name(&job, &document);
    let counter = attempt_name(&name);
    let attempts: i64 = conn
        .query_row("SELECT fence FROM leases WHERE name=?", [&counter], |r| {
            r.get(0)
        })
        .optional()
        .sql()?
        .unwrap_or(0);
    if attempts >= i64::from(request.max_attempts) {
        return Ok(None);
    }
    let Some(mut lease) = crate::leases::acquire(conn, &name, &request.worker, request.ttl_s)?
    else {
        return Ok(None);
    };
    // The connection-local acquire already checked contention, under the enclosing write
    // transaction. Its provisional token has never escaped; replace it with a durable one.
    lease.fence = allocate_fence(conn, &job.workspace, lease.fence.saturating_sub(1))?;
    conn.execute(
        "UPDATE leases SET fence=?2 WHERE name=?1",
        params![name, sql_count(lease.fence)?],
    )
    .sql()?;
    conn.execute(
        "INSERT INTO leases(name,owner,fence,expires_at) VALUES (?1,?2,1,0)
        ON CONFLICT(name) DO UPDATE SET fence=fence+1",
        params![counter, name],
    )
    .sql()?;
    let bound = kaban_core::JobLease {
        name: lease.name.clone(),
        owner: lease.owner.clone(),
        fence: lease.fence,
        ttl_s: request.ttl_s,
    };
    if let Some(current) = get(conn, &job.job, &job.workspace)? {
        let mut old: serde_json::Value = serde_json::from_str(&current.plan_json)?;
        for key in ["lease", "submit_state", "finished_report_json"] {
            old.as_object_mut()
                .ok_or(StoreError::InvalidName("job".into(), "stale"))?
                .remove(key);
            plan.as_object_mut()
                .ok_or(StoreError::InvalidName("job".into(), "stale"))?
                .remove(key);
        }
        if old != plan
            || current.status == JobStatus::Stale
            || current.status == JobStatus::Finished
        {
            return Err(StoreError::InvalidName("job".into(), "stale"));
        }
        job = current;
        plan = serde_json::from_str(&job.plan_json)?;
    }
    plan["lease"] = serde_json::to_value(bound)?;
    job.plan_json = plan.to_string();
    job.status = JobStatus::Open;
    // Registry retention is independent of the much shorter holder TTL.
    job.expires_at = job.expires_at.max(lease.expires_at);
    open(conn, &job)?;
    conn.execute("UPDATE ingest_jobs SET plan=?1,status='open',expires_at=?2 WHERE job_id=?3 AND workspace_id=?4",params![job.plan_json,job.expires_at,job.job.as_str(),job.workspace.as_str()]).sql()?;
    Ok(Some(lease))
}

pub(crate) fn done_artifact(
    conn: &Connection,
    job: &IngestJob,
    request: &kaban_core::JobFinishRequest,
    ctx: &kaban_core::WriteCtx,
) -> Result<kaban_core::DerivedArtifact> {
    let layer = layer(&job.dialect)?;
    let source_rid: i64 = conn
        .query_row(
            "SELECT rid FROM node WHERE id=?1 AND workspace_id=?2",
            params![request.document.as_str(), job.workspace.as_str()],
            |r| r.get(0),
        )
        .sql()?;
    // The shipped job protocol, never a worker-supplied producer descriptor, owns this marker.
    let producer = format!("kaban-parse:{}@1", job.dialect);
    let config_hash = job.marker_config_hash();
    let artifact = kaban_core::DerivedArtifact {
        key: kaban_core::artifact_key(
            &request.source_rev,
            layer,
            &producer,
            &config_hash,
            &ctx.policy_version,
        ),
        workspace_id: job.workspace.clone(),
        layer,
        producer_id: producer,
        source_rid,
        source_rev: request.source_rev.clone(),
        config_hash,
        policy_version: ctx.policy_version.clone(),
        published_at: crate::reconcile::persist::now_unix(),
    };
    Ok(artifact)
}

pub(crate) fn release(conn: &Connection, job: &IngestJob) -> Result<()> {
    if let Some(bound) = binding(job)? {
        conn.execute(
            "UPDATE leases SET expires_at=unixepoch()-1 WHERE name=?1 AND fence=?2 AND owner=?3",
            params![bound.name, sql_count(bound.fence)?, bound.owner],
        )
        .sql()?;
    }
    Ok(())
}

/// Revoke only the exact Document/source binding, before purge clears its props.
pub(crate) fn revoke_document(conn: &Connection, document: &str, workspace: &str) -> Result<u32> {
    if !crate::reconcile::engine_tables(conn)? {
        return Ok(0);
    }
    conn.execute("UPDATE leases SET expires_at=unixepoch()-1 WHERE name IN (SELECT json_extract(plan,'$.lease.name') FROM ingest_jobs WHERE workspace_id=?2 AND json_extract(plan,'$.document_id')=?1)",params![document,workspace]).sql()?;
    let deleted = conn.execute(
        "DELETE FROM ingest_jobs WHERE workspace_id=?2 AND json_extract(plan,'$.document_id')=?1
         AND source_id IN (SELECT json_extract(props,'$.source_id') FROM node
         WHERE id=?1 AND workspace_id=?2 AND kind='Document')",
        params![document, workspace],
    ).sql()?;
    u32::try_from(deleted).map_err(|_| StoreError::InvalidName("job".into(), "too_large"))
}

/// Bounded retention maintenance; only finished rows are pruned, never present expired jobs.
pub(crate) fn prune(
    conn: &Connection,
    workspace: &WorkspaceId,
    now: i64,
    limit: u32,
) -> Result<u32> {
    conn.execute("WITH eligible AS (SELECT l.name FROM leases l WHERE substr(l.name,1,length(?1)+8)='ingest:'||?1||':' AND (
       EXISTS(SELECT 1 FROM ingest_jobs j WHERE j.workspace_id=?1 AND j.status='finished' AND j.expires_at<=?2 AND json_extract(j.plan,'$.lease.name')=l.name)
       OR (l.expires_at<?2 AND NOT EXISTS(SELECT 1 FROM node n WHERE n.workspace_id=?1 AND n.kind='Document' AND n.lifecycle='active' AND l.name IN ('ingest:'||?1||':llm:'||n.id||'@'||json_extract(n.props,'$.content_hash'),'ingest:'||?1||':gliner:'||n.id||'@'||json_extract(n.props,'$.content_hash'),'ingest:'||?1||':none:'||n.id||'@'||json_extract(n.props,'$.content_hash'))))) ORDER BY l.name LIMIT ?3)
       DELETE FROM leases WHERE name IN (SELECT name FROM eligible UNION ALL SELECT 'attempt:'||name FROM eligible)",params![workspace.as_str(),now,limit]).sql()?;
    let deleted = conn
        .execute(
            "DELETE FROM ingest_jobs WHERE job_id IN (SELECT job_id FROM ingest_jobs
         WHERE workspace_id=?1 AND status='finished' AND expires_at<=?2
         ORDER BY expires_at,job_id LIMIT ?3)",
            params![workspace.as_str(), now, limit],
        )
        .sql()?;
    u32::try_from(deleted).map_err(|_| StoreError::InvalidName("job".into(), "too_large"))
}

pub(crate) fn exhausted(conn: &Connection, workspace: &WorkspaceId, attempts: u32) -> Result<u64> {
    let value: i64 = conn.query_row("SELECT count(*) FROM node n JOIN leases l ON l.name IN ('attempt:ingest:'||?1||':llm:'||n.id||'@'||json_extract(n.props,'$.content_hash'),'attempt:ingest:'||?1||':gliner:'||n.id||'@'||json_extract(n.props,'$.content_hash'),'attempt:ingest:'||?1||':none:'||n.id||'@'||json_extract(n.props,'$.content_hash')) WHERE n.workspace_id=?1 AND n.kind='Document' AND n.lifecycle='active' AND l.fence>=?2 AND NOT EXISTS(SELECT 1 FROM derived_artifact a WHERE a.workspace_id=?1 AND a.source_rid=n.rid AND json_extract(a.source_rev,'$.content_hash')=n.content_hash AND l.name='attempt:ingest:'||?1||':'||CASE a.layer WHEN 'script' THEN 'none' ELSE a.layer END||':'||n.id||'@'||json_extract(n.props,'$.content_hash'))",params![workspace.as_str(),attempts],|r|r.get(0)).sql()?;
    u64::try_from(value).map_err(|_| StoreError::InvalidName("claim".into(), "invalid_count"))
}

fn json_error(column: usize, error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn count(row: &Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn sql_count(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|error| StoreError::Backend {
        source: BackendSource::new(error),
    })
}

fn read(row: &Row<'_>) -> rusqlite::Result<IngestJob> {
    let handle: String = row.get(0)?;
    let job = JobId::parse(&handle).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let status: String = row.get(18)?;
    let status = match status.as_str() {
        "open" => JobStatus::Open,
        "finished" => JobStatus::Finished,
        "stale" => JobStatus::Stale,
        "expired" => JobStatus::Expired,
        _ => return Err(crate::rows::bad_enum(18, &status)),
    };
    Ok(IngestJob {
        job,
        workspace: WorkspaceId::new(row.get::<_, String>(1)?),
        source_id: row.get(2)?,
        relative_path: row.get(3)?,
        content_hash: row.get(4)?,
        pipeline_version: row.get(5)?,
        chunk_config_hash: row.get(6)?,
        mode: row.get(7)?,
        dialect: row.get(8)?,
        doc_type: row.get(9)?,
        kinds: serde_json::from_str(&row.get::<_, String>(10)?)
            .map_err(|error| json_error(10, error))?,
        plan_json: row.get(11)?,
        named_edges: serde_json::from_str(&row.get::<_, String>(12)?)
            .map_err(|error| json_error(12, error))?,
        submits: count(row, 13)?,
        created: count(row, 14)?,
        updated: count(row, 15)?,
        unchanged: count(row, 16)?,
        redactions: count(row, 17)?,
        status,
        created_at: row.get(19)?,
        expires_at: row.get(20)?,
        finished_at: row.get(21)?,
    })
}

/// Resolve in the supplied transaction; never fall back to an unscoped query.
pub(crate) fn get(
    conn: &Connection,
    job: &JobId,
    workspace: &WorkspaceId,
) -> Result<Option<IngestJob>> {
    conn.query_row(
        "SELECT job_id,workspace_id,source_id,relative_path,content_hash,pipeline_version,
         chunk_config_hash,mode,\"with\",doc_type,kinds,plan,named_edges,submits,created,
         updated,unchanged,redactions,status,created_at,expires_at,finished_at
         FROM ingest_jobs WHERE job_id=?1 AND workspace_id=?2",
        params![job.as_str(), workspace.as_str()],
        read,
    )
    .optional()
    .sql()
}

fn encode_error(error: serde_json::Error) -> StoreError {
    StoreError::Backend {
        source: BackendSource::new(error),
    }
}

/// Open only; a repeat never resets a row or its retained contract. Caller owns compatibility.
pub(crate) fn open(conn: &Connection, job: &IngestJob) -> Result<IngestJob> {
    conn.execute(
        "INSERT INTO ingest_jobs(job_id,workspace_id,source_id,relative_path,content_hash,
         pipeline_version,chunk_config_hash,mode,\"with\",doc_type,kinds,plan,named_edges,
         submits,created,updated,unchanged,redactions,status,created_at,expires_at,finished_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
         ON CONFLICT(job_id) DO NOTHING",
        params![
            job.job.as_str(),
            job.workspace.as_str(),
            job.source_id,
            job.relative_path,
            job.content_hash,
            job.pipeline_version,
            job.chunk_config_hash,
            job.mode,
            job.dialect,
            job.doc_type,
            serde_json::to_string(&job.kinds).map_err(encode_error)?,
            job.plan_json,
            serde_json::to_string(&job.named_edges).map_err(encode_error)?,
            sql_count(job.submits)?,
            sql_count(job.created)?,
            sql_count(job.updated)?,
            sql_count(job.unchanged)?,
            sql_count(job.redactions)?,
            job.status.as_str(),
            job.created_at,
            job.expires_at,
            job.finished_at
        ],
    )
    .sql()?;
    get(conn, &job.job, &job.workspace)?.ok_or_else(|| StoreError::NotFound("job_revoked".into()))
}

/// Ordinary preparation is checked in the caller's insertion transaction, never before it.
pub(crate) fn open_checked(
    conn: &Connection,
    request: &kaban_core::JobOpenRequest,
) -> Result<IngestJob> {
    check_source(
        conn,
        &request.job,
        &request.document,
        &request.source_rev,
        &request.chunks,
    )?;
    open(conn, &request.job)
}

/// Registry-only stale transition, including retained reports that can no longer be served.
pub(crate) fn mark_stale(conn: &Connection, job: &JobId, workspace: &WorkspaceId) -> Result<bool> {
    Ok(conn
        .execute(
            "UPDATE ingest_jobs SET status='stale' WHERE job_id=?1 AND workspace_id=?2",
            params![job.as_str(), workspace.as_str()],
        )
        .sql()?
        != 0)
}

/// Bind the actual live Document and complete Chunk set inside the publication transaction.
pub(crate) fn check_source(
    conn: &Connection,
    job: &IngestJob,
    document: &kaban_core::NodeId,
    expected: &kaban_core::SourceRev,
    chunks: &[kaban_core::NodeId],
) -> Result<()> {
    let plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
    let planned: Option<Vec<&str>> = plan["chunks"]
        .as_array()
        .and_then(|ids| ids.iter().map(|chunk| chunk["id"].as_str()).collect());
    let supplied: Vec<&str> = chunks.iter().map(kaban_core::NodeId::as_str).collect();
    if plan["document_id"].as_str() != Some(document.as_str())
        || planned.as_ref() != Some(&supplied)
    {
        return Err(StoreError::InvalidName("job".into(), "stale"));
    }
    let row: Option<(String, i64, String)> = conn.query_row(
        "SELECT content_hash,rev,props FROM node WHERE id=?1 AND workspace_id=?2 AND kind='Document' AND lifecycle='active' AND json_extract(props,'$.owner')='kaban-ingest'",
        params![document.as_str(), job.workspace.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).optional().sql()?;
    let Some((hash, rev, props)) = row else {
        return Err(StoreError::NotFound("job_revoked".into()));
    };
    let props: serde_json::Value = serde_json::from_str(&props)?;
    if hash != expected.content_hash
        || rev != expected.rev
        || props["source_id"] != job.source_id
        || props["content_hash"] != job.content_hash
    {
        return Err(StoreError::InvalidName("job".into(), "stale"));
    }
    let named: std::collections::BTreeSet<&str> =
        chunks.iter().map(kaban_core::NodeId::as_str).collect();
    let actual: Option<std::collections::BTreeSet<&str>> = props["chunk_ids"]
        .as_array()
        .and_then(|ids| ids.iter().map(serde_json::Value::as_str).collect());
    if named.len() != chunks.len() || actual.as_ref() != Some(&named) {
        return Err(StoreError::InvalidName("job".into(), "stale"));
    }
    for chunk in chunks {
        let live: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM node WHERE id=?1 AND workspace_id=?2 AND kind='Chunk' AND lifecycle='active' AND json_extract(props,'$.owner')='kaban-ingest')",
            params![chunk.as_str(),job.workspace.as_str()], |r| r.get(0),
        ).sql()?;
        if !live {
            return Err(StoreError::InvalidName("job".into(), "stale"));
        }
    }
    Ok(())
}

fn add_count(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .filter(|n| i64::try_from(*n).is_ok())
        .ok_or(StoreError::InvalidName("submit".into(), "too_large"))
}

/// Only an explicitly reindexed exact edge claim may be re-derived by a new checked job.
/// Generic batch replay, finish/policy/forget retirement and human deprecation stay immutable.
pub(crate) fn restore_reindexed(
    conn: &Connection,
    job: &IngestJob,
    claims: &[kaban_core::BatchAssertion],
) -> Result<bool> {
    let mut changed = false;
    for claim in claims {
        let kaban_core::BatchSubject::Edge { src, rel, dst } = &claim.subject else {
            continue;
        };
        if claim.layer != layer(&job.dialect)?
            || claim.method != kaban_core::Method::IngestSubmit
            || claim.source_id.as_deref() != Some(job.source_id.as_str())
            || claim.value["owner"] != "kaban-parse"
        {
            continue;
        }
        let rid = |id: &kaban_core::NodeId| -> Result<Option<i64>> {
            conn.query_row(
                "SELECT rid FROM node WHERE id=?1 AND workspace_id=?2 AND lifecycle='active'",
                params![id.as_str(), job.workspace.as_str()],
                |r| r.get(0),
            )
            .optional()
            .sql()
        };
        let (Some(src), Some(dst)) = (rid(src)?, rid(dst)?) else {
            continue;
        };
        let assertion = kaban_core::Assertion {
            key: kaban_core::AssertionKey::default(),
            workspace_id: job.workspace.clone(),
            subject: kaban_core::Subject::Edge {
                src,
                rel: rel.clone(),
                dst,
            },
            claim_class: claim.claim_class,
            predicate: claim.predicate.clone(),
            object_rid: None,
            value: kaban_core::CanonicalJson::new(claim.value.clone()),
            layer: claim.layer,
            method: claim.method,
            extractor_id: claim.extractor_id.clone(),
            extractor_version: claim.extractor_version.clone(),
            source_id: claim.source_id.clone(),
            lineage_id: claim.lineage_id.clone(),
            raw_score: claim.raw_score,
            rank: claim.rank,
            rank_reason: claim.rank_reason.clone(),
            calibration_version: claim.calibration_version,
            supersedes: claim.supersedes.clone(),
            ts: 0,
        }
        .with_key()?;
        changed |= conn.execute("UPDATE assertion SET rank='normal',rank_reason=NULL WHERE key=?1 AND workspace_id=?2 AND rank='deprecated' AND rank_reason='producer-set:reindex'",params![assertion.key.0,job.workspace.as_str()]).sql()? != 0;
    }
    Ok(changed)
}

/// Persist only successful transaction outcomes; callers cannot replace scalar counters.
pub(crate) fn record_submit(
    conn: &Connection,
    mut job: IngestJob,
    report: &kaban_core::BatchReport,
    state: kaban_core::JobSubmitStateUpdate,
    ctx: &kaban_core::WriteCtx,
) -> Result<IngestJob> {
    let original = kaban_core::JobSubmitContract::from_job(&job)?;
    let old_plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
    let new_plan: serde_json::Value = serde_json::from_str(&state.plan_json)?;
    if old_plan.get("finished_report_json") != new_plan.get("finished_report_json") {
        return Err(StoreError::InvalidName("submit".into(), "invalid_state"));
    }
    job.plan_json = state.plan_json;
    if kaban_core::JobSubmitContract::from_job(&job)? != original {
        return Err(StoreError::InvalidName("submit".into(), "invalid_state"));
    }
    job.named_edges = state.named_edges;
    job.submits = add_count(job.submits, 1)?;
    job.created = add_count(job.created, u64::from(report.inserted))?;
    job.updated = add_count(job.updated, u64::from(report.updated))?;
    job.unchanged = add_count(job.unchanged, u64::from(report.unchanged))?;
    job.redactions = add_count(
        job.redactions,
        ctx.audit
            .iter()
            .filter(|a| a.op == kaban_core::AuditOp::Redact)
            .count() as u64,
    )?;
    conn.execute(
        "UPDATE ingest_jobs SET plan=?1,named_edges=?2,submits=?3,created=?4,updated=?5,unchanged=?6,redactions=?7 WHERE job_id=?8 AND workspace_id=?9 AND status='open'",
        params![job.plan_json,serde_json::to_string(&job.named_edges)?,sql_count(job.submits)?,sql_count(job.created)?,sql_count(job.updated)?,sql_count(job.unchanged)?,sql_count(job.redactions)?,job.job.as_str(),job.workspace.as_str()],
    ).sql()?;
    Ok(job)
}

/// Scope follows owned derivation, not merely an extractor's record list. Chunk paths
/// include earlier retained jobs and historical `PART_OF` links, even after chunk replacement.
const FINISH_SCOPE: &str = "WITH records(rid) AS (
 SELECT e.src FROM edge e JOIN node d ON d.rid=e.dst
 WHERE d.id=?1 AND d.workspace_id=?2 AND e.workspace_id=?2 AND e.rel='DERIVED_FROM'
 AND json_extract(e.props,'$.owner')='kaban-parse'
 UNION SELECT a.subject_rid FROM assertion a JOIN node d ON d.rid=a.object_rid
 WHERE d.id=?1 AND a.workspace_id=?2 AND a.source_id=?3 AND a.subject_kind='edge'
 AND a.predicate='DERIVED_FROM' AND a.method='ingest_submit' AND a.layer IN ('llm','gliner','script')
 AND json_extract(a.value,'$.owner')='kaban-parse'
), chunks(rid) AS (
 SELECT c.rid FROM node c JOIN edge cs ON cs.src=c.rid JOIN node s ON s.rid=cs.dst
 JOIN edge sd ON sd.src=s.rid JOIN node d ON d.rid=sd.dst
 WHERE c.kind='Chunk' AND c.workspace_id=?2 AND s.kind='Section' AND s.workspace_id=?2
 AND d.id=?1 AND d.workspace_id=?2 AND cs.rel='PART_OF' AND sd.rel='PART_OF'
 AND cs.workspace_id=?2 AND sd.workspace_id=?2
 AND json_extract(cs.props,'$.owner')='kaban-ingest' AND json_extract(sd.props,'$.owner')='kaban-ingest'
 UNION SELECT c.rid FROM ingest_jobs j, json_each(j.plan,'$.chunks') p JOIN node c ON c.id=json_extract(p.value,'$.id')
 WHERE j.workspace_id=?2 AND j.source_id=?3 AND json_extract(j.plan,'$.document_id')=?1
 AND c.workspace_id=?2 AND c.kind='Chunk'
) ";

type FinishEdge = (i64, i64, String, String, String);

/// Bounded complete population, collected before any retirement or edge deletion.
pub(crate) struct FinishPopulation {
    pub(crate) edges: Vec<FinishEdge>,
    pub(crate) claims: Vec<i64>,
    pub(crate) orphans: Vec<kaban_core::NodeId>,
    pub(crate) skipped_cap: bool,
}

pub(crate) fn finish_population(
    conn: &Connection,
    job: &IngestJob,
    request: &kaban_core::JobFinishRequest,
) -> Result<FinishPopulation> {
    let plan: serde_json::Value = serde_json::from_str(&job.plan_json)?;
    let records: std::collections::BTreeSet<String> = serde_json::from_value(
        plan["submit_state"]
            .get("named_records")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    )?;
    let limit = i64::from(request.finish_cap) + 1;
    let args = params![
        request.document.as_str(),
        job.workspace.as_str(),
        job.source_id,
        limit
    ];
    let edges = conn.prepare(&format!("{FINISH_SCOPE}
        SELECT e.src,e.dst,e.rel,s.id,d.id FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst
        WHERE e.workspace_id=?2 AND s.workspace_id=?2 AND d.workspace_id=?2
        AND json_extract(e.props,'$.owner')='kaban-parse'
        AND (e.layer IS NULL OR e.layer IN ('llm','gliner','script'))
        AND (e.source_id IS NULL OR e.source_id=?3)
        AND (e.src IN records OR (e.rel='EVIDENCE_FOR' AND e.src IN chunks))
        ORDER BY s.id,d.id,e.rel LIMIT ?4")).sql()?.query_map(args, |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).sql()?.collect::<rusqlite::Result<Vec<FinishEdge>>>().sql()?;
    let claims = conn
        .prepare(&format!(
            "{FINISH_SCOPE}
        SELECT a.id FROM assertion a WHERE a.workspace_id=?2 AND a.source_id=?3
        AND a.subject_kind='edge' AND a.claim_class='edge' AND a.method='ingest_submit'
        AND a.layer IN ('llm','gliner','script') AND json_extract(a.value,'$.owner')='kaban-parse'
        AND (a.subject_rid IN records OR (a.predicate='EVIDENCE_FOR' AND a.subject_rid IN chunks))
        ORDER BY a.id LIMIT ?4"
        ))
        .sql()?
        .query_map(args, |r| r.get(0))
        .sql()?
        .collect::<rusqlite::Result<Vec<i64>>>()
        .sql()?;
    let scoped_records = conn
        .prepare(&format!(
            "{FINISH_SCOPE}
        SELECT n.id,n.governance FROM node n WHERE n.rid IN records AND n.workspace_id=?2
        ORDER BY n.id LIMIT ?4"
        ))
        .sql()?
        .query_map(args, |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    let names: std::collections::BTreeSet<_> = job
        .named_edges
        .iter()
        .cloned()
        .chain(
            edges
                .iter()
                .map(|(_, _, rel, src, dst)| (src.clone(), dst.clone(), rel.clone())),
        )
        .collect();
    let mut all_records = records.clone();
    all_records.extend(scoped_records.iter().map(|(id, _)| id.clone()));
    let skipped_cap = plan["submit_state"]["overflow"].as_bool().unwrap_or(false)
        || names.len().saturating_add(all_records.len()) > request.finish_cap as usize
        || claims.len() > request.finish_cap as usize;
    let orphans = if skipped_cap {
        Vec::new()
    } else {
        scoped_records
            .into_iter()
            .filter(|(id, governance)| governance == "proposed" && !records.contains(id))
            .map(|(id, _)| {
                kaban_core::NodeId::parse(&id)
                    .map_err(|_| StoreError::InvalidName("job".into(), "stale"))
            })
            .collect::<Result<_>>()?
    };
    Ok(FinishPopulation {
        edges,
        claims,
        orphans,
        skipped_cap,
    })
}
