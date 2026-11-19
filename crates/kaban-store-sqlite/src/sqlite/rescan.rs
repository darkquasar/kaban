//! The policy rescan (`rescan_impl`, core-spec S4; core-plan C2.7; MD-18, MD-19). Rows carry
//! no policy stamp (REC-08): freshness is the change log. Completed-policy freshness and walk
//! progress are kept apart: `index_meta.policy_hash` names the policy the last complete walk
//! ran under and `policy_scan_seq` is the watermark below which every node and edge change has
//! been judged by that policy; both move together, only at a walk's completion or in a run of
//! that same policy. Walks in progress keep a watermark per policy under
//! `policy_walk_seq:<policy>`, so an interrupted or superseded walk never moves another
//! policy's certified watermark. Walks of one policy share that watermark but each has its own
//! cursor (two processes can walk at once), so it never depends on a cursor: a restart keeps
//! it where it is, and it passes only change-log entries that have been judged.
//!
//! ```text
//!   rescan_impl(policy, at, batch, judge)            one BEGIN IMMEDIATE per batch
//!   +--------------------------------------------------------------------------------+
//!   | at = Start (or Changes once policy_hash names another policy, or a walk whose  |
//!   |      own watermark is gone):                                                    |
//!   |      policy_hash == policy and log intact => Changes (certified watermark)      |
//!   |      else walk watermark(policy) := min(itself, head) => Nodes(0) (full walk)   |
//!   | 1. change log: <= batch entries with seq > this run's watermark                |
//!   |      node/edge insert|update => judge again, wherever this walk's cursor is   |
//!   |      (a row ahead of it may be behind another walk's; judged twice at most)    |
//!   | 2. walk: Nodes(c) -> <= batch nodes rid > c; none => Edges(0)                  |
//!   |          Edges(c) -> <= batch edges rowid > c; none => walk complete           |
//!   | 3. watermark := log caught up ? head after this batch's own writes             |
//!   |                               : last entry read                                |
//!   |    walking: walk watermark(policy) := watermark                                 |
//!   |    walk complete: policy_hash := policy, policy_scan_seq := watermark, walk     |
//!   |                   watermark dropped (one transaction)                           |
//!   |    Changes: policy_scan_seq := watermark                                        |
//!   | cursor: walk position, Changes while the log lags, Done when nothing is left   |
//!   +--------------------------------------------------------------------------------+
//! ```

use crate::rows::{NODE_COLS, row_edge, row_node_at};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{
    EdgeRescanAction, IndexMeta, Lifecycle, Node, RescanAction, RescanCursor, RescanJudge,
    RescanReport, now_iso,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;

use super::SqliteStore;
use super::nodes::redacted_content_hash;

/// The edge columns `row_edge` reads, then the edge's row id.
const EDGE_COLS: &str = "e.id, s.id, d.id, e.rel, e.confidence, e.created_at, e.props, e.rowid";
/// An edge with its endpoint ids.
const EDGE_FROM: &str = "edge e JOIN node s ON s.rid = e.src JOIN node d ON d.rid = e.dst";
/// The `index_meta` key prefix of a walk's own change-log watermark, one per policy walking.
const WALK_SEQ_PREFIX: &str = "policy_walk_seq:";

impl SqliteStore {
    /// The body of [`kaban_core::IngestStore::rescan`].
    pub(super) fn rescan_impl(
        &self,
        policy_version: &str,
        at: RescanCursor,
        batch: usize,
        judge: &mut dyn RescanJudge,
    ) -> Result<RescanReport> {
        let mut conn = self.lock();
        self.catalog.commit(&mut conn, kaban_core::Durability::Normal, |txn| {
            let tx = txn.conn();
            let limit = i64::try_from(batch.max(1)).unwrap_or(i64::MAX);
            // the batch reads under the write lock, so a row judged here cannot be rewritten by
            // another writer before its action lands, and every change after this batch's own
            // writes is another writer's
            let walk_key = format!("{WALK_SEQ_PREFIX}{policy_version}");
            let certified = meta(tx, IndexMeta::POLICY_HASH)?.as_deref() == Some(policy_version);
            // a run (re)decides at its start, when the certification it follows names another
            // policy now, or when its walk's own watermark is gone (another run of this policy
            // completed and certified it)
            let restart = match at {
                RescanCursor::Start => true,
                RescanCursor::Changes => !certified,
                RescanCursor::Nodes(_) | RescanCursor::Edges(_) => meta(tx, &walk_key)?.is_none(),
                RescanCursor::Done => false,
            };
            let at = if restart {
                let head = crate::changelog::head(tx)?;
                if certified && log_intact(tx, head)? {
                    RescanCursor::Changes
                } else {
                    // the walk watermark is the policy's, shared by every walk of it: a restart
                    // keeps an older walk's position (or the head, if it is past the head), so it
                    // never skips a change that walk has not judged yet
                    let kept = meta(tx, &walk_key)?
                        .and_then(|v| v.parse::<u64>().ok())
                        .map_or(head, |w| w.min(head));
                    set_meta(tx, &walk_key, kept)?;
                    RescanCursor::Nodes(0)
                }
            } else {
                at
            };
            // the watermark this run reads and moves: the certified one in a run of the certified
            // policy, the walk's own while it walks
            let watermark_key = if at == RescanCursor::Changes {
                IndexMeta::POLICY_SCAN_SEQ
            } else {
                walk_key.as_str()
            };
            let mut report = RescanReport {
                cursor: at,
                ..RescanReport::default()
            };
            if at == RescanCursor::Done {
                return Ok(report);
            }
            let now = now_iso();
            let log = catch_up(tx, watermark_key, limit, judge, &now, &mut report)?;
            let walk_done = match at {
                RescanCursor::Nodes(after) => {
                    match walk_nodes(tx, after, limit, judge, &now, &mut report)? {
                        Some(last) => {
                            report.cursor = RescanCursor::Nodes(last);
                            false
                        }
                        None => walk_edges_from(tx, 0, limit, judge, &mut report)?,
                    }
                }
                RescanCursor::Edges(after) => walk_edges_from(tx, after, limit, judge, &mut report)?,
                RescanCursor::Start | RescanCursor::Changes | RescanCursor::Done => true,
            };
            let watermark = match log {
                CatchUp::CaughtUp => crate::changelog::head(tx)?,
                CatchUp::Behind { last } => last,
            };
            if walk_done && at != RescanCursor::Changes {
                // the walk is complete: the database-wide marker (MD-19) and the watermark it
                // certifies are recorded together in this same transaction; an equal value is
                // left untouched
                tx.execute(
                    "INSERT INTO index_meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value WHERE value <> excluded.value",
                    params![IndexMeta::POLICY_HASH, policy_version],
                ).sql()?;
                set_meta(tx, IndexMeta::POLICY_SCAN_SEQ, watermark)?;
                tx.execute("DELETE FROM index_meta WHERE key = ?1", [&walk_key])
                    .sql()?;
            } else {
                set_meta(tx, watermark_key, watermark)?;
            }
            if walk_done {
                report.cursor = match log {
                    CatchUp::CaughtUp => RescanCursor::Done,
                    CatchUp::Behind { .. } => RescanCursor::Changes,
                };
            }
            Ok(report)
        })
    }
}

/// Whether the change log still holds every entry after the watermark: no watermark yet, one
/// past the head (a copied or rolled-back file), or a prune past it means the rows changed
/// since are unknown and the run must walk the tables.
fn log_intact(conn: &Connection, head: u64) -> Result<bool> {
    let Some(watermark) =
        meta(conn, IndexMeta::POLICY_SCAN_SEQ)?.and_then(|v| v.parse::<u64>().ok())
    else {
        return Ok(false);
    };
    let oldest: Option<i64> = conn
        .query_row("SELECT min(seq) FROM changes", [], |r| r.get(0))
        .sql()?;
    let oldest = oldest.map_or(0, |s| u64::try_from(s).unwrap_or(0));
    Ok(watermark <= head && (oldest == 0 || oldest <= watermark.saturating_add(1)))
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .optional()
    .sql()
}

/// Record `value` under `key`; an equal value is left untouched.
fn set_meta(conn: &Connection, key: &str, value: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value WHERE value <> excluded.value",
        params![key, value.to_string()],
    )
    .sql()?;
    Ok(())
}

/// How far a batch read the change log.
#[derive(Clone, Copy)]
enum CatchUp {
    /// Every entry after the watermark was read.
    CaughtUp,
    /// More entries follow `last`.
    Behind { last: u64 },
}

/// Judge the nodes and edges the change log names after the watermark under `watermark_key`
/// (at most `limit` entries), wherever the caller's walk stands: the watermark is shared by
/// every walk of the policy, so it may only pass an entry that has been judged, and a row
/// ahead of this walk's cursor may sit behind another walk's (it is judged again when this
/// walk reaches it).
fn catch_up(
    conn: &Connection,
    watermark_key: &str,
    limit: i64,
    judge: &mut dyn RescanJudge,
    now: &str,
    report: &mut RescanReport,
) -> Result<CatchUp> {
    let watermark = meta(conn, watermark_key)?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let entries: Vec<(i64, String, String)> = {
        let mut st = conn
            .prepare_cached(
                "SELECT seq, entity, id FROM changes WHERE seq > ?1 AND op IN ('insert', 'update') ORDER BY seq LIMIT ?2",
            )
            .sql()?;
        let rows = st
            .query_map(params![watermark, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .sql()?;
        rows.collect::<rusqlite::Result<_>>().sql()?
    };
    let mut judged = BTreeSet::new();
    for (_, entity, id) in &entries {
        if !judged.insert((entity.as_str(), id.as_str())) {
            continue;
        }
        match entity.as_str() {
            "node" => {
                let mut st = conn
                    .prepare_cached(&format!(
                        "SELECT rid, {NODE_COLS} FROM node WHERE id = ?1 AND lifecycle <> 'purged'"
                    ))
                    .sql()?;
                let row: Option<(i64, Node)> = st
                    .query_row([id], |r| Ok((r.get(0)?, row_node_at(r, 1)?)))
                    .optional()
                    .sql()?;
                if let Some((rid, node)) = row {
                    judge_node(conn, rid, &node, judge, now, report)?;
                }
            }
            "edge" => {
                let mut st = conn
                    .prepare_cached(&format!(
                        "SELECT {EDGE_COLS} FROM {EDGE_FROM} WHERE e.id = ?1"
                    ))
                    .sql()?;
                let row = st
                    .query_row([id], |r| Ok((row_edge(r)?, r.get::<_, i64>(7)?)))
                    .optional()
                    .sql()?;
                if let Some((edge, rowid)) = row {
                    judge_edge(conn, rowid, &edge, judge, report)?;
                }
            }
            _ => {}
        }
    }
    let read = i64::try_from(entries.len()).unwrap_or(i64::MAX);
    Ok(match entries.last() {
        Some((seq, ..)) if read >= limit => CatchUp::Behind {
            last: u64::try_from(*seq).unwrap_or(0),
        },
        _ => CatchUp::CaughtUp,
    })
}

/// Judge at most `limit` non-purged nodes after row id `after`; the last row id judged, or
/// `None` when no node is left.
fn walk_nodes(
    conn: &Connection,
    after: u64,
    limit: i64,
    judge: &mut dyn RescanJudge,
    now: &str,
    report: &mut RescanReport,
) -> Result<Option<u64>> {
    let nodes: Vec<(i64, Node)> = {
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT rid, {NODE_COLS} FROM node WHERE rid > ?1 AND lifecycle <> 'purged' ORDER BY rid LIMIT ?2"
            ))
            .sql()?;
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        let rows = st
            .query_map(params![after, limit], |r| {
                Ok((r.get(0)?, row_node_at(r, 1)?))
            })
            .sql()?;
        rows.collect::<rusqlite::Result<_>>().sql()?
    };
    for (rid, node) in &nodes {
        judge_node(conn, *rid, node, judge, now, report)?;
    }
    Ok(nodes
        .last()
        .map(|(rid, _)| u64::try_from(*rid).unwrap_or_default()))
}

/// Walk edges after row id `after` and set the cursor; whether the edge walk (and with it the
/// whole walk) is complete.
fn walk_edges_from(
    conn: &Connection,
    after: u64,
    limit: i64,
    judge: &mut dyn RescanJudge,
    report: &mut RescanReport,
) -> Result<bool> {
    let edges: Vec<(kaban_core::Edge, i64)> = {
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT {EDGE_COLS} FROM {EDGE_FROM} WHERE e.rowid > ?1 ORDER BY e.rowid LIMIT ?2"
            ))
            .sql()?;
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        let rows = st
            .query_map(params![after, limit], |r| Ok((row_edge(r)?, r.get(7)?)))
            .sql()?;
        rows.collect::<rusqlite::Result<_>>().sql()?
    };
    for (edge, rowid) in &edges {
        judge_edge(conn, *rowid, edge, judge, report)?;
    }
    Ok(match edges.last() {
        Some((_, rowid)) => {
            report.cursor = RescanCursor::Edges(u64::try_from(*rowid).unwrap_or_default());
            false
        }
        None => true,
    })
}

/// Judge one node and apply the action in place.
fn judge_node(
    conn: &Connection,
    rid: i64,
    node: &Node,
    judge: &mut dyn RescanJudge,
    now: &str,
    report: &mut RescanReport,
) -> Result<()> {
    report.scanned += 1;
    match judge.node(node) {
        RescanAction::Keep => {
            report.kept += 1;
        }
        RescanAction::Redact { strings } => {
            // every rewritable string column is replaced (R2-2) and the content identity with
            // it: the pre-redaction hash would keep a guess oracle on the masked secret, and a
            // hash of the masked text alone would let two notes masked alike collide on
            // `node_hash_active`, so the new hash is domain-tagged and mixes in the node id
            let hash = redacted_content_hash(&node.id, &strings);
            let bodies = crate::body_codec::Writer::for_connection(conn)?;
            conn.execute(
                "UPDATE node SET text=?1, summary=?2, tags=?3, props=?4, path_scope=?5, session_id=?6, agent_id=?7, repo=?8, commit_sha=?9, dup_key=?10, content_hash=?11, updated_at=?12, rev=rev+1 WHERE rid=?13",
                params![
                    bodies.stored(&strings.text),
                    strings.summary,
                    serde_json::to_string(&strings.tags)?,
                    serde_json::to_string(&strings.props)?,
                    serde_json::to_string(&strings.path_scope)?,
                    strings.session_id,
                    strings.agent_id,
                    strings.repo,
                    strings.commit_sha,
                    strings.dup_key,
                    hash,
                    now,
                    rid
                ],
            ).sql()?;
            // EN-F10 (E2.7b): a `CodeRef`'s masked `props` move its anchor keys in this
            // transaction, as every other node write's do
            crate::anchors::refresh(conn, rid)?;
            report.redacted += 1;
        }
        RescanAction::Reject => {
            crate::jobs::revoke_document(conn, node.id.as_str(), &node.workspace_id)?;
            // the `ForgetMode::Tombstone` path (lifecycle only, edges kept); a node that is
            // already tombstoned is left as it is
            if node.lifecycle != Lifecycle::Tombstoned {
                kaban_core::lifecycle_transition(node.lifecycle, Lifecycle::Tombstoned)?;
                conn.execute(
                    "UPDATE node SET lifecycle='tombstoned', valid_to=?1, updated_at=?1, rev=rev+1 WHERE rid=?2",
                    params![now, rid],
                )
                .sql()?;
            }
            report.rejected += 1;
        }
    }
    super::artifacts::invalidate_stale(conn, rid)?;
    Ok(())
}

/// Judge one edge's `props` and apply the action in place.
fn judge_edge(
    conn: &Connection,
    rowid: i64,
    edge: &kaban_core::Edge,
    judge: &mut dyn RescanJudge,
    report: &mut RescanReport,
) -> Result<()> {
    report.scanned += 1;
    match judge.edge(edge) {
        EdgeRescanAction::Keep => report.kept += 1,
        EdgeRescanAction::Redact { props } => {
            conn.execute(
                "UPDATE edge SET props = ?1 WHERE rowid = ?2",
                params![serde_json::to_string(&props)?, rowid],
            )
            .sql()?;
            report.redacted += 1;
        }
        EdgeRescanAction::Reject => {
            conn.execute("DELETE FROM edge WHERE rowid = ?1", [rowid])
                .sql()?;
            report.rejected += 1;
        }
    }
    Ok(())
}
