//! Tenant-bound durable protection against automatic graph pruning.

use crate::{audit_table, sql_error::SqlResultExt};
use kaban_core::{PruningProtection, PruningTarget, Result, StoreError, WriteCtx};
use rusqlite::{Connection, OptionalExtension, params};

// Only explicit roots are expanded. Required endpoint nodes never become new roots.
const COVERAGE: &str = "WITH roots AS (
 SELECT n.rid FROM pruning_protection p JOIN node n ON n.id=p.target_key AND n.workspace_id=p.workspace_id WHERE p.workspace_id=?1 AND p.target_kind='node'
), triples AS (
 SELECT e.src,e.dst,e.rel FROM edge e JOIN node n ON n.rid=e.src WHERE n.workspace_id=?1 AND (e.src IN roots OR e.dst IN roots)
 UNION SELECT a.subject_rid,a.object_rid,a.predicate FROM assertion a WHERE a.workspace_id=?1 AND a.subject_kind='edge' AND (a.subject_rid IN roots OR a.object_rid IN roots)
 UNION SELECT s.rid,d.rid,json_extract(p.target_key,'$[2]') FROM pruning_protection p JOIN node s ON s.id=json_extract(p.target_key,'$[0]') AND s.workspace_id=p.workspace_id JOIN node d ON d.id=json_extract(p.target_key,'$[1]') AND d.workspace_id=p.workspace_id WHERE p.workspace_id=?1 AND p.target_kind='relationship'
), claims AS (
 SELECT a.id,a.subject_rid,a.object_rid,a.chunk_rid FROM assertion a WHERE a.workspace_id=?1 AND (
 a.key IN (SELECT target_key FROM pruning_protection WHERE workspace_id=?1 AND target_kind='claim')
 OR a.subject_rid IN roots OR a.chunk_rid IN roots
 OR (a.subject_kind='edge' AND EXISTS(SELECT 1 FROM triples t WHERE t.src=a.subject_rid AND t.dst=a.object_rid AND t.rel=a.predicate)))
), nodes AS (
 SELECT rid FROM roots UNION SELECT src FROM triples UNION SELECT dst FROM triples
 UNION SELECT subject_rid FROM claims UNION SELECT object_rid FROM claims UNION SELECT chunk_rid FROM claims
) ";

// Historical-version fixtures remain writable without silently claiming that the
// new protection seam is supported: protection() still refuses a missing table.
fn installed(conn: &Connection) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pruning_protection')",[],|r|r.get(0)).sql()
}

fn covered(
    conn: &Connection,
    ws: &str,
    query: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<bool> {
    if !installed(conn)? {
        return Ok(false);
    }
    let marked: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pruning_protection WHERE workspace_id=?1)",
            [ws],
            |r| r.get(0),
        )
        .sql()?;
    if !marked {
        return Ok(false);
    }
    let sql = format!("{COVERAGE}{query}");
    let mut bindings: Vec<&dyn rusqlite::ToSql> = vec![&ws];
    bindings.extend_from_slice(values);
    conn.query_row(&sql, bindings.as_slice(), |r| r.get(0))
        .sql()
}

pub(crate) fn node(conn: &Connection, ws: &str, rid: i64) -> Result<bool> {
    covered(
        conn,
        ws,
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE rid=?2)",
        &[&rid],
    )
}

pub(crate) fn claim(conn: &Connection, ws: &str, id: i64) -> Result<bool> {
    covered(
        conn,
        ws,
        "SELECT EXISTS(SELECT 1 FROM claims WHERE id=?2)",
        &[&id],
    )
}

pub(crate) fn edge(conn: &Connection, ws: &str, src: i64, dst: i64, rel: &str) -> Result<bool> {
    // A protected claim also retains its materialized relationship, but does not protect
    // unrelated claims that happen to name the same required endpoint.
    covered(
        conn,
        ws,
        "SELECT EXISTS(SELECT 1 FROM triples WHERE src=?2 AND dst=?3 AND rel=?4) OR EXISTS(SELECT 1 FROM claims c JOIN assertion a ON a.id=c.id WHERE a.subject_kind='edge' AND a.subject_rid=?2 AND a.object_rid=?3 AND a.predicate=?4)",
        &[&src, &dst, &rel],
    )
}

fn identity(
    conn: &Connection,
    target: &PruningTarget,
    ws: &str,
) -> Result<(&'static str, String, bool)> {
    let missing = || StoreError::NotFound("pruning_target".into());
    let rid = |id: &kaban_core::NodeId| -> Result<i64> {
        conn.query_row(
            "SELECT rid FROM node WHERE id=?1 AND workspace_id=?2 AND lifecycle<>'purged'",
            params![id.as_str(), ws],
            |r| r.get(0),
        )
        .optional()
        .sql()?
        .ok_or_else(missing)
    };
    match target {
        PruningTarget::Node(id) => Ok(("node", id.to_string(), node(conn, ws, rid(id)?)?)),
        PruningTarget::Claim(key) => {
            let id: i64 = conn.query_row("SELECT a.id FROM assertion a WHERE a.key=?1 AND a.workspace_id=?2 AND NOT EXISTS(SELECT 1 FROM node n WHERE n.rid IN (a.subject_rid,a.object_rid,a.chunk_rid) AND (n.workspace_id<>?2 OR n.lifecycle='purged'))",params![key.0,ws],|r|r.get(0)).optional().sql()?.ok_or_else(missing)?;
            Ok(("claim", key.0.clone(), claim(conn, ws, id)?))
        }
        PruningTarget::Relationship { src, dst, rel } => {
            let (s, d) = (rid(src)?, rid(dst)?);
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM edge WHERE src=?1 AND dst=?2 AND rel=?3)",
                    params![s, d, rel],
                    |r| r.get(0),
                )
                .sql()?;
            if !exists {
                return Err(missing());
            }
            Ok((
                "relationship",
                serde_json::to_string(&(src.as_str(), dst.as_str(), rel))?,
                edge(conn, ws, s, d, rel)?,
            ))
        }
    }
}

impl crate::SqliteStore {
    pub(crate) fn protection(
        &self,
        target: &PruningTarget,
        ctx: &WriteCtx,
        set: Option<bool>,
    ) -> Result<PruningProtection> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let ws = ctx.workspace_id.as_str();
        let (kind, key, _) = identity(&tx, target, ws)?;
        if let Some(value) = set {
            if value {
                tx.execute("INSERT INTO pruning_protection(workspace_id,target_kind,target_key) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",params![ws,kind,key]).sql()?;
            } else {
                tx.execute("DELETE FROM pruning_protection WHERE workspace_id=?1 AND target_kind=?2 AND target_key=?3",params![ws,kind,key]).sql()?;
            }
            audit_table::append(&tx, self.signer().as_ref(), &ctx.audit)?;
        }
        let explicit=tx.query_row("SELECT EXISTS(SELECT 1 FROM pruning_protection WHERE workspace_id=?1 AND target_kind=?2 AND target_key=?3)",params![ws,kind,key],|r|r.get(0)).sql()?;
        let (_, _, effective) = identity(&tx, target, ws)?;
        if set.is_some() {
            crate::changelog::record_terminal(&tx, ctx)?;
        }
        tx.commit().sql()?;
        Ok(PruningProtection {
            explicit,
            effective,
        })
    }
}

/// Remove marks whose targets or required references explicit purge destroys.
pub(crate) fn purge(conn: &Connection, ws: &str, rid: i64, id: &str) -> Result<()> {
    if !installed(conn)? {
        return Ok(());
    }
    conn.execute("DELETE FROM pruning_protection WHERE workspace_id=?1 AND ((target_kind='node' AND target_key=?2) OR (target_kind='relationship' AND (json_extract(target_key,'$[0]')=?2 OR json_extract(target_key,'$[1]')=?2)) OR (target_kind='claim' AND target_key IN (SELECT key FROM assertion WHERE workspace_id=?1 AND (subject_rid=?3 OR object_rid=?3 OR chunk_rid=?3))))",params![ws,id,rid]).sql()?;
    Ok(())
}

/// Retained historical children are not current source inventory. Revisit them on
/// a genuine owned source refresh/forget without changing the job's current chunks.
pub(crate) fn reconcile_retained_children(
    conn: &Connection,
    batch: &mut kaban_core::WriteBatch,
    ctx: &WriteCtx,
) -> Result<()> {
    use kaban_core::{EdgeDiff, INGEST_OWNER, Op, Owner, RemovalIntent};
    if batch.edge_diff
        != (EdgeDiff::Owned {
            owner: Owner::KabanIngest,
        })
    {
        return Ok(());
    }
    let named: std::collections::BTreeSet<_> = batch
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::UpsertNode { id, .. } | Op::Tombstone { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let mut extra = std::collections::BTreeSet::new();
    for op in &batch.ops {
        let Op::UpsertNode { id, node } = op else {
            continue;
        };
        let Some(props) = node.props.as_ref() else {
            continue;
        };
        if node.kind != "Document" || props["owner"] != INGEST_OWNER {
            continue;
        }
        let explicit = ctx.removal_intent == RemovalIntent::Explicit;
        if explicit {
            if !batch
                .ops
                .iter()
                .any(|op| matches!(op, Op::Tombstone { id: deleted, .. } if deleted == id))
            {
                continue;
            }
        } else if !matches!(
            props["status"].as_str(),
            Some("ok" | "partial" | "failed" | "blocked")
        ) {
            continue;
        }
        let Some(source) = props["source_id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let document: Option<i64> = conn.query_row(
            "SELECT rid FROM node WHERE id=?1 AND workspace_id=?2 AND kind='Document' AND lifecycle<>'purged' AND json_extract(props,'$.owner')=?3 AND json_extract(props,'$.source_id')=?4",
            params![id.as_str(),ctx.workspace_id.as_str(),INGEST_OWNER,source], |r| r.get(0),
        ).optional().sql()?;
        let Some(document) = document else { continue };
        // Two fixed levels only. A hidden Section can still connect a retained Chunk;
        // ownership and tenant checks apply independently to every node and edge.
        let children = conn
            .prepare_cached(
                "WITH sections AS (
               SELECT s.rid,s.id,s.lifecycle FROM edge e JOIN node s ON s.rid=e.src
               WHERE e.dst=?1 AND e.rel='PART_OF' AND json_extract(e.props,'$.owner')=?3
                 AND s.workspace_id=?2 AND s.kind='Section' AND s.lifecycle<>'purged'
                 AND json_extract(s.props,'$.owner')=?3
             )
             SELECT id FROM sections WHERE lifecycle IN ('active','archived')
             UNION SELECT c.id FROM sections s JOIN edge e ON e.dst=s.rid JOIN node c ON c.rid=e.src
               WHERE e.rel='PART_OF' AND json_extract(e.props,'$.owner')=?3
                 AND c.workspace_id=?2 AND c.kind='Chunk' AND c.lifecycle IN ('active','archived')
                 AND json_extract(c.props,'$.owner')=?3",
            )
            .sql()?
            .query_map(
                params![document, ctx.workspace_id.as_str(), INGEST_OWNER],
                |r| r.get::<_, String>(0),
            )
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        extra.extend(
            children
                .into_iter()
                .filter(|id| !named.contains(id.as_str())),
        );
    }
    for id in extra {
        batch.ops.push(Op::Tombstone {
            id: kaban_core::NodeId::parse(&id)?,
            reason: "absent from owned source refresh".into(),
        });
    }
    Ok(())
}
