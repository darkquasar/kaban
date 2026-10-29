//! Entity resolution inside the store (engine-spec EN-F51 (3)-(5), (7); engine-plan E2.10): the
//! node views the pure scorer sees, one resolution pass over a workspace that writes the
//! `identity` claims and the `node_identity` rows of its `Match` pairs, the `unmerge` that
//! reverses one, the cluster-consistency post-filter and the `explain` waterfall rendering.
//! Every decision is the core's (`kaban_core::er`); every row write is here, on the caller's
//! connection, inside the caller's transaction.
//!
//! ```text
//!   resolve(ws)            view_of(node) -> blocks(.., None) -> pairs -> score_pair -> decide
//!                             |                                                  |
//!   Match    -> identity claim {match: true, W, contributions}  -> v1 Winner
//!   Possible -> identity claim {match: false, ...}  -> v1 Candidate{merge} (review row)
//!   None     -> nothing                              decide_groups over the touched groups
//!                                                      -> project::apply -> reproject_identity
//!                                                         (the flat cluster: member -> least rid)
//!
//!   unmerge(id)            UPDATE assertion SET rank = 'deprecated', rank_reason = <reason>
//!                          (every eligible match claim naming id), decide_groups: the
//!                          deprecated claim no longer votes, the projection drops the row
//! ```

use super::{Group, Setup, persist};
use crate::rows::{NODE_COLS, row_node};
use crate::sql_error::SqlResultExt;
use crate::sqlite::{SqliteStore, WS_COL};
use kaban_core::er::{self, ErDecision, ErReport, NodeView, UnmergeOutcome};
use kaban_core::{
    Assertion, AssertionKey, CanonicalJson, ClaimClass, Contribution, Fixed, Layer, Method, Node,
    Rank, Result, StoreError, Subject, UnresolvedReason, Verdict, WorkspaceId, group_key,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The lineage of every ER identity claim (EN-F12b: one engine, one lineage).
pub(crate) const ER_LINEAGE: &str = "reconcile:er";

/// The scorer's view of a stored node: the title is the `kaban-policy` normalised first line
/// of the text (the dependency the core cannot hold), aliases come from `props.aliases[]`,
/// anchors are the node's `node_anchor` keys as `<kind>:<key>`.
pub(crate) fn view_of(conn: &Connection, rid: i64, node: &Node) -> Result<NodeView> {
    let first_line = node.text.lines().next().unwrap_or("");
    let aliases = node
        .props
        .get("aliases")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut st = conn
        .prepare_cached(
            "SELECT anchor_kind, key FROM node_anchor WHERE rid = ?1 ORDER BY anchor_kind, key",
        )
        .sql()?;
    let anchors = st
        .query_map([rid], |r| {
            Ok(format!(
                "{}:{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?
            ))
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<String>>>()
        .sql()?;
    Ok(NodeView {
        id: node.id.clone(),
        workspace_id: node.workspace_id.clone(),
        kind: node.kind.clone(),
        title: kaban_policy::norm_term(first_line, false),
        aliases,
        tags: node.tags.clone(),
        path_scope: node.path_scope.clone(),
        anchors,
        summary: node.summary.clone(),
        dup_key: node.dup_key.clone(),
    })
}

/// Whether `value.match` is true (an ER `Match` or a human "same entity" claim).
pub(crate) fn says_match(a: &Assertion) -> Option<bool> {
    a.value.as_value().get("match").and_then(Value::as_bool)
}

/// The active nodes of `ws` with their row ids, ascending, less the ones already redirected
/// (a merged node is represented by its canonical and is never paired again).
fn active_nodes(conn: &Connection, ws: &str) -> Result<Vec<(i64, Node)>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {NODE_COLS}, n.rid FROM node n WHERE {WS_COL} = ?1 AND n.lifecycle = 'active'
             AND n.rid NOT IN (SELECT rid FROM node_identity WHERE canonical_rid IS NOT NULL)
             ORDER BY n.rid"
        ))
        .sql()?;
    let rows = st
        .query_map([ws], |r| {
            let node = row_node(r)?;
            let rid: i64 = r.get(r.as_ref().column_count() - 1)?;
            Ok((rid, node))
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// One resolution pass over `ws` (EN-F51 (1)-(5)) on the caller's connection: block, score and
/// decide every pair the blocks name; write an `identity` claim for every `Match` and
/// `Possible` (ids and integers only, EN-S9: `{match, W, contributions: [{field, level, w}]}`,
/// subject the lesser rid, object the greater), then decide the touched groups. The
/// `node_identity` rows are the cluster projection of the final verdicts
/// ([`reproject_identity`]; E2.10b BR-3, BR-4), never a write of this pass: the report counts
/// a `Match` as `matched` when the two nodes share a canonical afterwards and as a conflict
/// when the verdict or a human distinction kept them apart.
pub(crate) fn resolve(conn: &Connection, setup: &Setup, ws: &str, now: i64) -> Result<ErReport> {
    let mut report = ErReport::default();
    if !super::engine_tables(conn)? {
        return Ok(report);
    }
    let nodes = active_nodes(conn, ws)?;
    let mut views = Vec::with_capacity(nodes.len());
    for (rid, node) in &nodes {
        views.push(view_of(conn, *rid, node)?);
    }
    // EN-F24 is E3.4: no fuzzy identity index exists, so the trait has no implementation here; an
    // unavailable index is a backend failure, never an empty neighbourhood (spec 05 SS-15)
    let blocks = er::blocks(&views, None).map_err(|e| StoreError::Backend {
        source: kaban_core::BackendSource::new(e),
    })?;
    let pairs = er::pairs(&blocks);
    let mut groups = BTreeSet::new();
    let mut scored: Vec<(i64, i64, bool)> = Vec::new();
    for (i, j) in pairs {
        report.compared += 1;
        let score = er::score_pair(&views[i], &views[j], &setup.er);
        let decision = er::decide(&score, &setup.er);
        if decision == ErDecision::None {
            continue;
        }
        let is_match = decision == ErDecision::Match;
        let (lo, hi) = (nodes[i].0.min(nodes[j].0), nodes[i].0.max(nodes[j].0));
        let contributions: Vec<Value> = score
            .contributions
            .iter()
            .map(|c| json!({"field": c.field.as_str(), "level": c.level.as_str(), "w": c.w.0}))
            .collect();
        let assertion = Assertion {
            key: AssertionKey::default(),
            workspace_id: WorkspaceId::new(ws),
            subject: Subject::Node(lo),
            claim_class: ClaimClass::Identity,
            predicate: None,
            object_rid: Some(hi),
            value: CanonicalJson::new(
                json!({"match": is_match, "W": score.w.0, "contributions": contributions}),
            ),
            layer: Layer::Reconcile,
            method: Method::Er,
            extractor_id: None,
            extractor_version: None,
            source_id: None,
            lineage_id: ER_LINEAGE.to_owned(),
            raw_score: er::raw_score(&score, &setup.er).map_err(StoreError::Fixed)?,
            rank: Rank::Normal,
            rank_reason: None,
            calibration_version: 0,
            supersedes: None,
            ts: now,
        }
        .with_key()?;
        persist::insert_claim(conn, &assertion)?;
        groups.insert(group_key(&assertion));
        scored.push((lo, hi, is_match));
    }
    super::decide_groups(conn, setup, &groups, now, None)?;
    for (lo, hi, is_match) in scored {
        if !is_match {
            report.possible += 1;
        } else if canonical_of(conn, lo)? == canonical_of(conn, hi)? {
            report.matched += 1;
        } else {
            report.conflicts += 1;
        }
    }
    Ok(report)
}

/// The canonical a node reads as: its redirect, or itself (a `canonical_rid NULL` row, the
/// purged canonical, is no redirect).
fn canonical_of(conn: &Connection, rid: i64) -> Result<i64> {
    conn.prepare_cached(
        "SELECT COALESCE((SELECT canonical_rid FROM node_identity WHERE rid = ?1), ?1)",
    )
    .sql()?
    .query_row([rid], |r| r.get(0))
    .sql()
}

/// The identity claims of one kind naming `rid`, as the cluster projection reads them: the
/// winning `{match: true}` pairs (the status is the final verdict, the shared projection's own
/// write) and the eligible human distinctions (`{match: false}`, `layer = human`, not
/// `deprecated`, not superseded: the claims `votes()` admits).
const WINNING_MATCHES_OF: &str =
    "SELECT CASE WHEN a.subject_rid = ?1 THEN a.object_rid ELSE a.subject_rid END, a.key
     FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
     WHERE a.claim_class = 'identity' AND s.disposition = 'winner'
       AND json_extract(a.value, '$.match') = 1
       AND (a.subject_rid = ?1 OR a.object_rid = ?1)";
const DISTINCTIONS_OF: &str =
    "SELECT CASE WHEN a.subject_rid = ?1 THEN a.object_rid ELSE a.subject_rid END
     FROM assertion a
     WHERE a.claim_class = 'identity' AND a.layer = 'human' AND a.rank <> 'deprecated'
       AND json_extract(a.value, '$.match') = 0
       AND NOT EXISTS (SELECT 1 FROM assertion b WHERE b.supersedes = a.id)
       AND (a.subject_rid = ?1 OR a.object_rid = ?1)";

/// The root of `rid` in a union-find forest kept in a map (path halving).
fn find(parent: &mut BTreeMap<i64, i64>, rid: i64) -> i64 {
    let mut n = rid;
    loop {
        let p = parent.get(&n).copied().unwrap_or(n);
        if p == n {
            return n;
        }
        let gp = parent.get(&p).copied().unwrap_or(p);
        parent.insert(n, gp);
        n = gp;
    }
}

/// The flat cluster projection of the identity verdicts (EN-F51 (5), (7); E2.10b BR-3, BR-4),
/// run by the shared projection whenever an identity group is decided. Over the affected
/// node set (the pair, every node its current clusters hold, every node a winning
/// `{match: true}` pair reaches, to a fixpoint) the winning pairs are joined in ascending
/// `(lo, hi)` order, each join refused when an eligible human distinction spans the two
/// components (so two nodes a human keeps distinct are never joined through a third); every
/// component then has one canonical, its least rid, and every other member one
/// `node_identity` row pointing at it (no chain: `recall` follows one hop). A node that is
/// a canonical or stands alone has no row, so a rejected, deprecated or unresolved identity
/// claim holds no redirect and `unmerge` is durable against the next unchanged pass. A row
/// whose canonical and claim are unchanged is not rewritten (N5).
pub(crate) fn reproject_identity(conn: &Connection, lo: i64, hi: i64, now: i64) -> Result<()> {
    let mut nodes: BTreeSet<i64> = BTreeSet::new();
    let mut matches: BTreeSet<(i64, i64, String)> = BTreeSet::new();
    let mut distinctions: BTreeSet<(i64, i64)> = BTreeSet::new();
    let mut queue = vec![lo, hi];
    let mut cluster = conn
        .prepare_cached(
            "SELECT i.rid FROM node_identity i WHERE i.canonical_rid = ?1
             UNION SELECT i.canonical_rid FROM node_identity i WHERE i.rid = ?1 AND i.canonical_rid IS NOT NULL",
        )
        .sql()?;
    let mut winning = conn.prepare_cached(WINNING_MATCHES_OF).sql()?;
    let mut distinct = conn.prepare_cached(DISTINCTIONS_OF).sql()?;
    while let Some(n) = queue.pop() {
        if !nodes.insert(n) {
            continue;
        }
        let held = cluster
            .query_map([n], |r| r.get::<_, i64>(0))
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        queue.extend(held);
        let pairs = winning
            .query_map([n], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?))
            })
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        for (other, key) in pairs {
            let Some(other) = other else { continue };
            matches.insert((n.min(other), n.max(other), key));
            queue.push(other);
        }
        let apart = distinct
            .query_map([n], |r| r.get::<_, Option<i64>>(0))
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        for other in apart.into_iter().flatten() {
            distinctions.insert((n.min(other), n.max(other)));
        }
    }
    let mut parent: BTreeMap<i64, i64> = BTreeMap::new();
    for (a, b, _) in &matches {
        let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
        if ra == rb {
            continue;
        }
        let bridged = distinctions.iter().any(|(x, y)| {
            let (fx, fy) = (find(&mut parent, *x), find(&mut parent, *y));
            (fx == ra && fy == rb) || (fx == rb && fy == ra)
        });
        if !bridged {
            parent.insert(ra.max(rb), ra.min(rb));
        }
    }
    let mut stored = conn
        .prepare_cached("SELECT canonical_rid, assertion_key FROM node_identity WHERE rid = ?1")
        .sql()?;
    for &n in &nodes {
        let root = find(&mut parent, n);
        let current: Option<(Option<i64>, Option<String>)> = stored
            .query_row([n], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .sql()?;
        if root == n {
            if current.is_some() {
                conn.execute("DELETE FROM node_identity WHERE rid = ?1", [n])
                    .sql()?;
            }
            continue;
        }
        // the evidence behind the member's redirect: its first winning pair inside the component
        let key = matches
            .iter()
            .filter(|(a, b, _)| {
                (*a == n || *b == n) && find(&mut parent, if *a == n { *b } else { *a }) == root
            })
            .map(|(_, _, key)| key.clone())
            .min();
        if current
            .as_ref()
            .is_some_and(|(c, k)| *c == Some(root) && *k == key)
        {
            continue;
        }
        conn.execute(
            "INSERT INTO node_identity(rid, canonical_rid, assertion_key, since) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(rid) DO UPDATE SET canonical_rid = excluded.canonical_rid,
                 assertion_key = excluded.assertion_key, since = excluded.since",
            params![n, root, key, now],
        )
        .sql()?;
    }
    Ok(())
}

/// Cluster consistency (EN-F51 (7)): an `identity` group a human `preferred` claim holds
/// distinct (`{match: false}`) while a live machine claim says `{match: true}` is
/// `Unresolved{identity_conflict}`, not a silent human win: the disagreement is surfaced for
/// an explicit action, and no identity row is written for it (`resolve` checks the same).
pub(crate) fn identity_conflict(verdict: Verdict, group: &Group) -> Verdict {
    if group
        .claims
        .first()
        .is_none_or(|a| a.claim_class != ClaimClass::Identity)
    {
        return verdict;
    }
    let Verdict::Winner { assertion_key, .. } = &verdict else {
        return verdict;
    };
    let Some(winner) = group.claims.iter().find(|a| a.key == *assertion_key) else {
        return verdict;
    };
    let human_distinct = winner.layer == Layer::Human && says_match(winner) == Some(false);
    let machine_match = group.claims.iter().any(|a| {
        a.layer != Layer::Human && a.rank != Rank::Deprecated && says_match(a) == Some(true)
    });
    if human_distinct && machine_match {
        return Verdict::Unresolved {
            ranked: vec![(winner.key.clone(), Fixed::ONE)],
            reason: UnresolvedReason::IdentityConflict,
        };
    }
    verdict
}

/// The signed four-decimal rendering of a `LogWeight` integer (`-33074` is `-3.3074`).
pub(crate) fn signed(w: i64) -> String {
    let sign = if w < 0 { "-" } else { "" };
    let abs = w.unsigned_abs();
    format!("{sign}{}.{:04}", abs / 10_000, abs % 10_000)
}

/// The `er_contributions` waterfall of an `identity` claim (EN-F51 (4), EN-F14): the stored
/// `{field, level, w}` entries with `w` rendered signed; `None` for any other claim.
pub(crate) fn contributions_of(a: &Assertion) -> Option<Vec<Contribution>> {
    if a.claim_class != ClaimClass::Identity {
        return None;
    }
    let entries = a.value.as_value().get("contributions")?.as_array()?;
    Some(
        entries
            .iter()
            .map(|c| Contribution {
                field: c
                    .get("field")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                level: c
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                w: signed(c.get("w").and_then(Value::as_i64).unwrap_or(0)),
            })
            .collect(),
    )
}

/// The `node_identity` rows of the cluster `rid` stands in: its own row, the rows pointing at
/// it and the rows pointing at its canonical, ascending.
fn cluster_rows(conn: &Connection, rid: i64) -> Result<Vec<(i64, Option<i64>)>> {
    let mut st = conn
        .prepare_cached(
            "SELECT i.rid, i.canonical_rid FROM node_identity i
             WHERE i.rid = ?1 OR i.canonical_rid = ?1
                OR (i.canonical_rid IS NOT NULL
                    AND i.canonical_rid = (SELECT canonical_rid FROM node_identity WHERE rid = ?1))
             ORDER BY i.rid",
        )
        .sql()?;
    let rows = st
        .query_map([rid], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    Ok(rows)
}

/// `kaban unmerge <id>` (EN-F51 (5); E2.10b BR-4): every eligible `{match: true}` identity
/// claim naming `id` (either endpoint) becomes `deprecated` under `reason` (the rank is
/// outside the key: no new row, the history kept) and its group is re-decided without its
/// vote; the redirects follow from the shared projection, so the node leaves its cluster,
/// the other members keep whatever their own pairs still say, and the next unchanged ER
/// pass finds the same deprecated claim and writes nothing. Both nodes stay. `restored`
/// lists the nodes whose redirect the reversal removed or moved.
/// # Errors
/// `NotFound` for an unknown or foreign id, or one no identity row names.
pub(crate) fn unmerge(
    conn: &Connection,
    setup: &Setup,
    ws: &str,
    id: &str,
    reason: &str,
    now: i64,
) -> Result<UnmergeOutcome> {
    let (rid, _) = SqliteStore::rid_kind_in_workspace(conn, id, ws)
        .map_err(|_| StoreError::NotFound(id.to_owned()))?;
    let before = cluster_rows(conn, rid)?;
    if before.is_empty() {
        return Err(StoreError::NotFound(id.to_owned()));
    }
    let node_id = |rid: i64| -> Result<Option<String>> {
        conn.query_row("SELECT id FROM node WHERE rid = ?1", [rid], |r| r.get(0))
            .optional()
            .sql()
    };
    let mut outcome = UnmergeOutcome {
        id: id.to_owned(),
        canonical_id: None,
        restored: Vec::new(),
        deprecated: Vec::new(),
    };
    if let Some(canonical) = before.iter().find(|(r, _)| *r == rid).and_then(|(_, c)| *c) {
        outcome.canonical_id = node_id(canonical)?;
    }
    let mut st = conn
        .prepare_cached(
            "SELECT a.key, s.group_key FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
             WHERE a.claim_class = 'identity' AND a.workspace_id = ?2 AND a.rank <> 'deprecated'
               AND json_extract(a.value, '$.match') = 1
               AND (a.subject_rid = ?1 OR a.object_rid = ?1)
             ORDER BY a.key",
        )
        .sql()?;
    let claims = st
        .query_map(params![rid, ws], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    let mut groups = BTreeSet::new();
    for (key, group) in claims {
        conn.execute(
            "UPDATE assertion SET rank = 'deprecated', rank_reason = ?2 WHERE key = ?1",
            params![key, reason],
        )
        .sql()?;
        groups.insert(group);
        outcome.deprecated.push(key);
    }
    super::decide_groups(conn, setup, &groups, now, None)?;
    // a row no eligible claim explains (nothing to re-decide) is removed by the same projection
    reproject_identity(conn, rid, rid, now)?;
    let mut stored = conn
        .prepare_cached("SELECT canonical_rid FROM node_identity WHERE rid = ?1")
        .sql()?;
    for (member, canonical) in before {
        let now_points_at: Option<Option<i64>> =
            stored.query_row([member], |r| r.get(0)).optional().sql()?;
        if now_points_at != Some(canonical)
            && let Some(restored) = node_id(member)?
        {
            outcome.restored.push(restored);
        }
    }
    Ok(outcome)
}
