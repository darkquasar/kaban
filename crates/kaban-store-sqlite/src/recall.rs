//! Hybrid recall (core-spec F3): lexical and fuzzy seeds through the [`TextIndex`] seam (spec 05
//! SS-07; the store's own adapter), bounded graph expansion, RRF fusion and a deterministic
//! final order. Extracted from `sqlite.rs` as
//! a pure move (M3-25).
//!
//! Chunk cards (docs-ingest-spec D7; DI1.6): a `Chunk` hit carries `path > heading path` as
//! its summary (the stored summary is the `context_prefix`, `title > h1 > h2`, indexed in the
//! `summary` FTS column; the path comes from the chunk's props). The one-hop expansion that
//! brings the parent `Section` (`PART_OF`) and the Decisions linked `EVIDENCE_FOR` is the
//! ordinary hop 1 over every relationship; `kinds: ["Chunk"]` keeps chunks only.

use crate::candidates;
use crate::rows::{NODE_COLS, row_node};
use crate::sql_error::SqlResultExt;
use crate::sqlite::{WS_COL, in_workspace};
use kaban_core::ident::{QueryClass, classify};
use kaban_core::ontology::meta::RankHints;
use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextHit, TextIndex, TextQuery};
use kaban_core::{Hit, Lifecycle, Node, RecallQuery, Result, StoreError};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

/// How a recall reaches the connection (spec 05 OWN-M, constraint 1). The seed index may read
/// the store back (the store's own adapter reads canonical state through `Rows`, and an
/// in-memory adapter does so through this very connection), so the lock order is the store's
/// connection, then the adapter, never the reverse: a recall takes the lock per statement and
/// holds nothing across `pin` or `search`.
pub(crate) struct Access<'a>(pub(crate) &'a Mutex<Connection>);

impl Access<'_> {
    /// The connection for one statement or one phase; dropped before the next adapter call.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The `CodeRef` link of a seed filter as a SQL predicate over the `CodeRef` row `c`.
const CODEREF_MATCH: &str = "json_extract(c.props, '$.relative_path') = ? AND (? IS NULL OR json_extract(c.props, '$.name_path') = ?) AND (? IS NULL OR json_extract(c.props, '$.repo') = ?)";

/// The revalidation predicates of a [`SeedFilter`] (spec 05 SS-11): what the index may have
/// matched is re-checked against the current row under them, with the candidate exclusion of
/// EN-F49, inside the statement so `LIMIT` never starves a filtered recall.
struct Predicates {
    sql: String,
    binds: Vec<Box<dyn rusqlite::ToSql>>,
}

impl Predicates {
    fn of(conn: &Connection, f: &SeedFilter<'_>) -> Result<Self> {
        // the index holds active rows only, the predicate guards against a stale index
        let mut sql = String::from("n.lifecycle = 'active'");
        // EN-F49: candidates (`node_candidate` rows) never seed or join a recall unless asked
        sql.push_str(&candidates::exclusion_sql(conn, "n", f.read)?);
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !f.kinds.is_empty() {
            sql.push_str(" AND n.kind IN (");
            sql.push_str(&vec!["?"; f.kinds.len()].join(","));
            sql.push(')');
            for k in f.kinds {
                binds.push(Box::new(k.clone()));
            }
        }
        if let Some(ws) = f.workspace_id {
            sql.push_str(" AND ");
            sql.push_str(WS_COL);
            sql.push_str(" = ?");
            binds.push(Box::new(ws.to_owned()));
        }
        // tags inside the join (m2 M5): any of the requested tags among the node's JSON tags
        if !f.tags.is_empty() {
            sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(n.tags) WHERE value IN (");
            sql.push_str(&vec!["?"; f.tags.len()].join(","));
            sql.push_str("))");
            for t in f.tags {
                binds.push(Box::new(t.clone()));
            }
        }
        // "about this CodeRef" (F3): the seed is linked `ABOUT` to, or is the target of an
        // `EVIDENCE_FOR` edge from, a CodeRef whose stored props match the filter exactly
        if let Some(c) = f.code_ref {
            sql.push_str(" AND EXISTS (SELECT 1 FROM edge e JOIN node c ON c.rid = CASE e.rel WHEN 'ABOUT' THEN e.dst ELSE e.src END WHERE ((e.rel = 'ABOUT' AND e.src = n.rid) OR (e.rel = 'EVIDENCE_FOR' AND e.dst = n.rid)) AND c.kind = 'CodeRef' AND ");
            sql.push_str(CODEREF_MATCH);
            sql.push(')');
            binds.push(Box::new(c.relative_path.clone()));
            for opt in [&c.name_path, &c.repo] {
                // bound twice: once for the `IS NULL` guard, once for the comparison
                binds.push(Box::new(opt.clone()));
                binds.push(Box::new(opt.clone()));
            }
        }
        Ok(Self { sql, binds })
    }
}

/// The seeds of one mode (spec 05 SS-11): pages of `page` hits from `view` (one pinned
/// generation for the whole recall), every hit re-read from SQLite in one statement per page
/// and kept only when its row exists at the revision the index saw and still passes the
/// lifecycle, kind, workspace, tag, code-ref and candidate predicates. [`Seeds::fill`]
/// continues retrieval where the last fill stopped, until its target of eligible seeds is
/// kept or the candidates are exhausted (no skip cap: a fixed oversample is insufficient), so
/// the recall asks for more when a later rejection stage (canonical resolution, the read
/// gate) leaves the deliverable page short. An index that cannot answer the mode seeds
/// nothing.
///
/// A revision mismatch is dropped, not re-ranked: the projector re-indexes the row within one
/// page. Excerpts are never taken from here: the caller materializes every returned `rid`
/// from the current row.
struct Seeds {
    /// The revalidation statement: `(rid, rev)` of the page's rows that pass the predicates.
    sql: String,
    predicates: Predicates,
    mode: MatchMode,
    page: usize,
    kept: Vec<TextHit>,
    skip: usize,
    /// No further candidate: the index said so, cannot answer the mode, or the mode is not
    /// in play for this recall.
    exhausted: bool,
}

impl Seeds {
    fn new(
        conn: &Connection,
        q: &RecallQuery,
        mode: MatchMode,
        page: usize,
        in_play: bool,
    ) -> Result<Self> {
        let predicates = Predicates::of(conn, &SeedFilter::of(q))?;
        let sql = format!(
            "SELECT n.rid, n.rev FROM node n WHERE n.rid IN (SELECT value FROM json_each(?1)) AND {}",
            predicates.sql
        );
        Ok(Self {
            sql,
            predicates,
            mode,
            page,
            kept: Vec::new(),
            skip: 0,
            exhausted: !in_play,
        })
    }

    /// Retrieve until `target` eligible seeds are kept or the candidates are exhausted. Each
    /// page is searched with no connection held and revalidated under one statement's lock.
    fn fill(
        &mut self,
        view: &dyn TextIndex,
        q: &RecallQuery,
        target: usize,
        sufficient: usize,
        access: &Access<'_>,
    ) -> Result<()> {
        let filter = SeedFilter::of(q);
        while self.kept.len() < target && !self.exhausted {
            let page = view.search(&TextQuery {
                text: &q.query,
                mode: self.mode,
                filter,
                limit: self.page,
                sufficient,
                skip: self.skip,
            })?;
            let Searched::Hits { hits, exhausted } = page else {
                self.exhausted = true;
                return Ok(());
            };
            if hits.is_empty() {
                self.exhausted = true;
                return Ok(());
            }
            self.skip += hits.len();
            let rids: Vec<i64> = hits.iter().map(|h| h.rid).collect();
            let selected = serde_json::to_string(&rids)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&selected];
            binds.extend(
                self.predicates
                    .binds
                    .iter()
                    .map(|b| b.as_ref() as &dyn rusqlite::ToSql),
            );
            let conn = access.conn();
            let current: HashMap<i64, i64> = conn
                .prepare_cached(&self.sql)
                .sql()?
                .query_map(binds.as_slice(), |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })
                .sql()?
                .collect::<rusqlite::Result<_>>()
                .sql()?;
            drop(conn);
            self.kept.extend(
                hits.into_iter()
                    .filter(|hit| current.get(&hit.rid) == Some(&hit.revision)),
            );
            self.exhausted = exhausted;
        }
        Ok(())
    }

    /// The first `target` eligible seeds kept so far.
    fn first(&self, target: usize) -> &[TextHit] {
        &self.kept[..self.kept.len().min(target)]
    }
}

// Unhinted expansion retains legacy behavior; RankHints has no decay override (OWN-95).
const DEFAULT_HOP_DECAY: f64 = 0.6;
const DEFAULT_SECOND_HOP_RELS: &[&str] = &[
    "CAUSED_BY",
    "FIXED_BY",
    "SUPERSEDES",
    "INVALIDATES",
    "DERIVED_FROM",
];

fn rank_hints<'a>(ontology: &'a kaban_core::Ontology, kind: &str) -> Option<&'a RankHints> {
    ontology
        .kinds
        .iter()
        .find(|(name, _)| kaban_core::stored_name(name) == kind)
        .and_then(|(_, kind)| kind.rank.as_ref())
}

pub(crate) fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// RRF rank contribution `1 / (k + rank + 1)`; ranks are small list positions.
#[allow(clippy::cast_precision_loss)] // ranks are < 300
fn rrf(k: f64, rank: usize) -> f64 {
    1.0 / (k + rank as f64 + 1.0)
}

/// The chunk card header (D7): `path > heading path` for a `Chunk` node, from its props
/// (`path`, written by the ingest batch) and its stored summary (the `context_prefix`); any
/// other node is returned as stored.
fn chunk_card(mut node: Node) -> Node {
    if node.kind != "Chunk" {
        return node;
    }
    let path = node.props.get("path").and_then(Value::as_str).unwrap_or("");
    node.summary = Some(match node.summary.as_deref().filter(|s| !s.is_empty()) {
        Some(prefix) if !path.is_empty() => format!("{path} > {prefix}"),
        Some(prefix) => prefix.to_owned(),
        None => path.to_owned(),
    });
    node
}

/// [`recall`] over the given seed index (spec 05 SS-07): the index answers the lexical and
/// fuzzy seed searches, the connection reached through `access` the revalidation, the graph
/// expansion and the materialization, never held across a call into the index. Retrieval
/// continues (SS-11) until `limit` deliverable results are filled, counted after every
/// rejection stage (revalidation, lifecycle, workspace, kind and tag, canonical resolution
/// and `eligible`: the caller's read gate), or the candidates of every mode in play are
/// exhausted, all through one pinned generation.
pub(crate) fn recall_with(
    access: &Access<'_>,
    ontology: &kaban_core::Ontology,
    config: &kaban_core::config::Recall,
    q: &RecallQuery,
    index: &dyn TextIndex,
    eligible: &dyn Fn(&Node) -> bool,
) -> Result<Vec<Hit>> {
    let permits = |layer| {
        q.layers
            .as_ref()
            .is_none_or(|layers| layers.contains(&layer))
    };
    if q.layers
        .as_ref()
        .is_some_and(|layers| layers.contains(&kaban_core::RecallLayer::Sim))
    {
        return Err(StoreError::InvalidName(
            "sim".into(),
            "not_available_in_this_release",
        ));
    }
    let limit = if q.limit == 0 { 10 } else { q.limit.min(100) };
    let seed_k = (limit * 5).clamp(20, 100);
    let k =
        f64::from(config.effective_rrf_k().map_err(|_| {
            StoreError::InvalidName("recall.fusion".into(), "invalid_configuration")
        })?);
    // one pinned generation for every page of this recall (SS-11), taken with no connection
    // held; the filters travel with every seed search so LIMIT never starves a filtered recall
    let view = index.pin()?;
    let (mut lexical, mut fuzzy) = {
        let conn = access.conn();
        (
            Seeds::new(
                &conn,
                q,
                MatchMode::Lexical,
                seed_k,
                permits(kaban_core::RecallLayer::Fts),
            )?,
            Seeds::new(
                &conn,
                q,
                MatchMode::Fuzzy,
                seed_k,
                permits(kaban_core::RecallLayer::Fuzzy),
            )?,
        )
    };
    let mut target = seed_k;
    loop {
        // 1) lexical candidates, revalidated and refilled (an unavailable index seeds nothing)
        lexical.fill(view.as_ref(), q, target, limit, access)?;
        // the fuzzy seeds join on request, or when the lexical ones are empty
        let fuzzy_in_play = q.fuzzy || lexical.kept.is_empty();
        if fuzzy_in_play {
            fuzzy.fill(view.as_ref(), q, target, limit, access)?;
        }
        let out = fuse(
            &access.conn(),
            ontology,
            k,
            q,
            lexical.first(target),
            fuzzy_in_play.then(|| fuzzy.first(target)),
            limit,
            eligible,
        )?;
        if out.len() >= limit || (lexical.exhausted && (!fuzzy_in_play || fuzzy.exhausted)) {
            return Ok(out);
        }
        target = target.saturating_mul(2);
    }
}

/// Steps 2 and 3 of a recall over the seeds retrieved so far: RRF fusion of the `lexical`
/// list (and the `fuzzy` one when in play) with `k`, bounded graph expansion from the lexical
/// seeds, materialization under the request's filters and `eligible`, and the deterministic
/// final order, cut to `limit`.
#[allow(clippy::too_many_arguments)] // the recall's own inputs, split from the refill loop
fn fuse(
    conn: &Connection,
    ontology: &kaban_core::Ontology,
    k: f64,
    q: &RecallQuery,
    lexical: &[TextHit],
    fuzzy: Option<&[TextHit]>,
    limit: usize,
    eligible: &dyn Fn(&Node) -> bool,
) -> Result<Vec<Hit>> {
    // the revision every seed was revalidated at (SS-11, review t8a-3): a row that changed
    // since (a write between the seed's revalidation and this fusion) is dropped when it is
    // materialized below, under the one lock this fusion holds, never returned as new text
    // under an obsolete match and score
    let revalidated: HashMap<i64, i64> = lexical
        .iter()
        .chain(fuzzy.into_iter().flatten())
        .map(|hit| (hit.rid, hit.revision))
        .collect();
    // 2) RRF over the lexical list + graph expansion (neighbour fetch capped per node)
    let mut score: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
    for (rank, seed) in lexical.iter().enumerate() {
        let e = score.entry(seed.rid).or_insert_with(|| (0.0, vec![]));
        e.0 += rrf(k, rank);
        e.1.push(format!("bm25:{}", rank + 1));
    }
    for (rank, hit) in fuzzy.into_iter().flatten().enumerate() {
        let entry = score.entry(hit.rid).or_insert_with(|| (0.0, vec![]));
        entry.0 += rrf(k, rank);
        entry.1.push(format!("tri:{}", rank + 1));
    }
    let hops = q.hops.min(2);
    // EN-F49 as one predicate for the frontier and for a resolved canonical (E2.10b BR-5)
    let exclude = candidates::exclusion_sql(conn, "n", q.filters)?;
    // archived rows (superseded records, archived notes) join the frontier and the result only
    // on request; invalidated records are tombstoned (F5) and never return (M3-12)
    let archived_ok = q.include_superseded || q.include_archived;
    if hops > 0 {
        let mut frontier: Vec<(i64, f64, usize)> = lexical
            .iter()
            .take(20)
            .enumerate()
            .map(|(r, seed)| (seed.rid, rrf(k, r), r))
            .collect();
        // the workspace (review R1-5) and lifecycle (review R2-4) predicates apply while
        // the frontier is built: a neighbour outside the workspace, tombstoned or purged,
        // or archived without `include_superseded` / `include_archived` (M3-12: the same
        // frontier rule), is never scored and never expanded from
        let graph_sql: Vec<[String; 2]> = lexical.iter().take(20).map(|TextHit { kind, .. }| {
            std::array::from_fn(|index| {
                let rels = match rank_hints(ontology, kind) {
                    Some(hints) => hints.expand_rels.iter().map(String::as_str).collect::<Vec<_>>(),
                    None if index == 1 => DEFAULT_SECOND_HOP_RELS.to_vec(),
                    None => vec![],
                };
                let rel_filter = if rels.is_empty() {
                    if rank_hints(ontology, kind).is_some() { " AND 0".into() } else { String::new() }
                } else {
                    format!(" AND x.rel IN ({})", rels.into_iter().map(sql_literal).collect::<Vec<_>>().join(","))
                };
                format!(
            "SELECT x.nb, x.rel FROM (SELECT e.dst AS nb, e.rel FROM edge e WHERE e.src=?1 UNION ALL SELECT e.src, e.rel FROM edge e WHERE e.dst=?1) x JOIN node n ON n.rid = x.nb WHERE (?2 IS NULL OR {WS_COL} = ?2) AND (n.lifecycle = 'active' OR (?3 AND n.lifecycle = 'archived')){exclude}{rel_filter} ORDER BY x.nb LIMIT 200"
                )
            })
        }).collect();
        for hop in 1..=hops {
            let mut next = Vec::new();
            for (rid, s, origin) in &frontier {
                if rank_hints(ontology, &lexical[*origin].kind)
                    .and_then(|hints| hints.expand_depth)
                    .is_some_and(|depth| hop > depth)
                {
                    continue;
                }
                let mut st = conn
                    .prepare_cached(&graph_sql[*origin][usize::from(hop - 1)])
                    .sql()?;
                let rows = st
                    .query_map(params![rid, q.workspace_id, archived_ok], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })
                    .sql()?;
                for row in rows {
                    let (nb, rel) = row.sql()?;
                    let contrib = s * DEFAULT_HOP_DECAY.powi(i32::from(hop));
                    let e = score.entry(nb).or_insert_with(|| (0.0, vec![]));
                    e.0 += contrib;
                    if e.1.len() < 4 {
                        e.1.push(format!("hop{hop}:{rel}"));
                    }
                    next.push((nb, contrib, *origin));
                }
            }
            next.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)).then(a.2.cmp(&b.2)));
            next.truncate(50);
            frontier = next;
        }
    }
    // 3) materialize (neighbours still need the filters), weight by confidence, rank deterministically
    let mut ranked: Vec<(i64, f64, Vec<String>)> =
        score.into_iter().map(|(rid, (s, w))| (rid, s, w)).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(300);
    let mut st = conn
        .prepare_cached(&format!("SELECT {NODE_COLS} FROM node WHERE rid=?"))
        .sql()?;
    // EN-F51 (5): a merged node reads as its canonical, returned once (the first, highest
    // ranked, occurrence keeps its score) and only when the canonical itself passes the
    // workspace and candidate checks below; a `canonical_rid NULL` row (purged canonical,
    // `persist::purge_claims_of_node`) is no redirect; a store without the engine tables has none
    let redirects = crate::reconcile::engine_tables(conn)?;
    let mut identity = if redirects {
        Some(
            conn.prepare_cached("SELECT canonical_rid FROM node_identity WHERE rid = ?1")
                .sql()?,
        )
    } else {
        None
    };
    // EN-F49 re-applied to the resolved canonical (E2.10b BR-5): the hit passed the exclusion
    // as itself; the node it reads as may still be a candidate, and a visible alias never
    // exposes one (`include_candidates` empties the predicate, so nothing is checked)
    let mut canonical_visible = if redirects && !exclude.is_empty() {
        Some(
            conn.prepare_cached(&format!("SELECT 1 FROM node n WHERE n.rid = ?1{exclude}"))
                .sql()?,
        )
    } else {
        None
    };
    let identifier = classify(&q.query) == QueryClass::Identifier;
    let mut seen: HashSet<i64> = HashSet::new();
    let mut out: Vec<Hit> = Vec::new();
    for (hit, s, mut why) in ranked {
        let rid = match identity.as_mut() {
            Some(st) => st
                .query_row([hit], |r| r.get::<_, Option<i64>>(0))
                .optional()
                .sql()?
                .flatten()
                .unwrap_or(hit),
            None => hit,
        };
        if !seen.insert(rid) {
            continue;
        }
        if rid != hit
            && let Some(st) = canonical_visible.as_mut()
            && st.query_row([rid], |_| Ok(())).optional().sql()?.is_none()
        {
            continue;
        }
        let Some(node) = st.query_row([rid], row_node).optional().sql()? else {
            continue;
        };
        if rid == hit && revalidated.get(&rid).is_some_and(|rev| *rev != node.rev) {
            continue;
        }
        // archived (superseded) records are reachable through expansion only, on request;
        // tombstoned and purged rows never (core-spec F5)
        let lifecycle_ok = node.lifecycle == Lifecycle::Active
            || (archived_ok && node.lifecycle == Lifecycle::Archived);
        if !lifecycle_ok {
            continue;
        }
        if !q.kinds.is_empty() && !q.kinds.contains(&node.kind) {
            continue;
        }
        if !in_workspace(&node, q.workspace_id.as_deref()) {
            continue;
        }
        if !q.tags.is_empty() && !q.tags.iter().any(|t| node.tags.contains(t)) {
            continue;
        }
        let node = chunk_card(node);
        // the caller's read gate (a reject rule hides the card, S4): a hidden record is no
        // deliverable result, and the refill loop asks for more in its place (SS-11)
        if !eligible(&node) {
            continue;
        }
        let confidence = node.confidence.unwrap_or(0.7).clamp(0.0, 1.0);
        let boost = rank_hints(ontology, &node.kind)
            .and_then(|hints| hints.boost)
            .unwrap_or(1.0);
        if identifier {
            why.push("query_class: identifier".into());
        }
        out.push(Hit {
            score: s * (0.5 + 0.5 * confidence) * boost,
            node,
            why,
        });
    }
    // final order (N5, core-plan C3.5): score desc, store, id; one store in M3, so ties fall
    // back to the id (UUIDv7, time-ordered like the row id)
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.node.store_scope.cmp(&b.node.store_scope))
            .then_with(|| a.node.id.cmp(&b.node.id))
    });
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::{Access, recall_with};
    use kaban_core::text_index::TextHit;
    use kaban_core::text_index::fake::FakeIndex;
    use kaban_core::{ForgetMode, MemoryStore, NewNode, Node, NodeId, RecallQuery, WriteCtx};

    fn ctx(ws: &str) -> WriteCtx {
        WriteCtx::new(
            "v1",
            kaban_core::WorkspaceId::new(ws),
            kaban_core::Actor::Agent,
        )
    }

    fn created(store: &crate::SqliteStore, ws: &str, text: &str) -> Node {
        match store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    workspace_id: Some(ws.into()),
                    ..Default::default()
                },
                &[],
                &ctx(ws),
            )
            .unwrap()
        {
            kaban_core::RememberOutcome::Created(n) => n,
            kaban_core::RememberOutcome::Existing { node, .. } => panic!("fresh: {node:?}"),
        }
    }

    /// `(rid, rev)` of `node`.
    fn row(store: &crate::SqliteStore, node: &Node) -> (i64, i64) {
        store
            .lock()
            .query_row("SELECT rid, rev FROM node WHERE id = ?1", [&node.id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
    }

    fn hit(rid: i64, revision: i64) -> TextHit {
        TextHit {
            rid,
            revision,
            score: 1.0,
            kind: "Fact".into(),
        }
    }

    /// SS-11: every hit is re-read from SQLite. A hit whose revision the index saw is not the
    /// current one (stale), a tombstoned record (revoked) and a record of another workspace
    /// are excluded; the current, active, in-workspace one is returned.
    #[test]
    fn stale_revoked_and_cross_workspace_hits_are_excluded() {
        let store = crate::SqliteStore::open_memory().unwrap();
        let valid = created(&store, "ws1", "quokka valid");
        let stale = created(&store, "ws1", "quokka stale");
        let revoked = created(&store, "ws1", "quokka revoked");
        let elsewhere = created(&store, "ws2", "quokka elsewhere");
        let (valid_rid, valid_rev) = row(&store, &valid);
        let (stale_rid, stale_rev) = row(&store, &stale);
        let (revoked_rid, revoked_rev) = row(&store, &revoked);
        let (elsewhere_rid, elsewhere_rev) = row(&store, &elsewhere);
        store
            .forget(
                &NodeId::parse(&revoked.id).unwrap(),
                ForgetMode::Tombstone,
                &ctx("ws1"),
            )
            .unwrap();
        let index = FakeIndex {
            lexical: Some(vec![
                hit(stale_rid, stale_rev - 1),
                hit(revoked_rid, revoked_rev),
                hit(elsewhere_rid, elsewhere_rev),
                hit(valid_rid, valid_rev),
            ]),
            ..Default::default()
        };
        let q = RecallQuery {
            query: "quokka".into(),
            workspace_id: Some("ws1".into()),
            hops: 0,
            ..Default::default()
        };
        let ontology = kaban_core::shipped::resolved().unwrap();
        let config = kaban_core::config::Recall::default();
        let hits = recall_with(
            &Access(store.connection()),
            &ontology,
            &config,
            &q,
            &index,
            &|_| true,
        )
        .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.node.id.as_str()).collect();
        assert_eq!(ids, vec![valid.id.as_str()], "{ids:?}");
    }

    /// SS-11 refill: with 2,500 rejected candidates (rows that do not exist) ranked ahead of
    /// the three valid ones, retrieval continues through the pinned view until the eligible
    /// seeds fill or the candidates are exhausted, and the three valid records are returned;
    /// a fixed oversample of one page would return nothing.
    #[test]
    fn valid_results_behind_many_rejected_candidates_are_returned() {
        let store = crate::SqliteStore::open_memory().unwrap();
        let valid: Vec<Node> = (0..3)
            .map(|i| created(&store, "default", &format!("quokka valid {i}")))
            .collect();
        let mut scripted: Vec<TextHit> = (0..2_500).map(|i| hit(1_000_000 + i, 1)).collect();
        for node in &valid {
            let (rid, rev) = row(&store, node);
            scripted.push(hit(rid, rev));
        }
        let index = FakeIndex {
            lexical: Some(scripted),
            ..Default::default()
        };
        let q = RecallQuery {
            query: "quokka".into(),
            hops: 0,
            ..Default::default()
        };
        let ontology = kaban_core::shipped::resolved().unwrap();
        let config = kaban_core::config::Recall::default();
        let hits = recall_with(
            &Access(store.connection()),
            &ontology,
            &config,
            &q,
            &index,
            &|_| true,
        )
        .unwrap();
        let mut ids: Vec<&str> = hits.iter().map(|h| h.node.id.as_str()).collect();
        ids.sort_unstable();
        let mut expected: Vec<&str> = valid.iter().map(|n| n.id.as_str()).collect();
        expected.sort_unstable();
        assert_eq!(ids, expected);
    }

    /// SS-11 (T2 review t2-5): retrieval continues until the deliverable results fill the
    /// limit, counted after every rejection stage. Eleven matching records with the ten
    /// ranked first hidden by the policy (the read gate's reject rule, here the eligibility
    /// predicate): the recall returns the eleventh, not an empty page.
    #[test]
    fn higher_ranked_policy_rejected_records_do_not_empty_the_page() {
        let store = crate::SqliteStore::open_memory().unwrap();
        let hidden: Vec<Node> = (0..10)
            .map(|i| created(&store, "default", &format!("quokka hidden {i}")))
            .collect();
        let shown = created(&store, "default", "quokka shown");
        let scripted: Vec<TextHit> = hidden
            .iter()
            .chain(std::iter::once(&shown))
            .map(|node| {
                let (rid, rev) = row(&store, node);
                hit(rid, rev)
            })
            .collect();
        let index = FakeIndex {
            lexical: Some(scripted),
            ..Default::default()
        };
        let q = RecallQuery {
            query: "quokka".into(),
            limit: 10,
            hops: 0,
            ..Default::default()
        };
        let ontology = kaban_core::shipped::resolved().unwrap();
        let config = kaban_core::config::Recall::default();
        let eligible = |node: &Node| !node.text.contains("hidden");
        let hits = recall_with(
            &Access(store.connection()),
            &ontology,
            &config,
            &q,
            &index,
            &eligible,
        )
        .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.node.id.as_str()).collect();
        assert_eq!(ids, vec![shown.id.as_str()]);
    }

    /// SS-07: an index that cannot answer either mode seeds nothing, so a recall that would
    /// match a stored note through FTS5 is empty, not an error.
    #[test]
    fn an_unavailable_index_yields_an_empty_recall_not_an_error() {
        let store = crate::SqliteStore::open_memory().unwrap();
        let ctx = WriteCtx::new(
            "v1",
            kaban_core::WorkspaceId::default(),
            kaban_core::Actor::Agent,
        );
        let note = NewNode {
            kind: "Failure".into(),
            text: "linker could not find libssl".into(),
            ..Default::default()
        };
        store.remember(note, &[], &ctx).unwrap();
        let q = RecallQuery {
            query: "libssl".into(),
            ..Default::default()
        };
        assert_eq!(store.recall(&q).unwrap().len(), 1);
        let ontology = kaban_core::shipped::resolved().unwrap();
        let config = kaban_core::config::Recall::default();
        let hits = recall_with(
            &Access(store.connection()),
            &ontology,
            &config,
            &q,
            &FakeIndex::default(),
            &|_| true,
        );
        assert!(hits.unwrap().is_empty());
    }
}
