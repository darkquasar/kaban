//! Bounded graph traversal (core-spec F6): active nodes of the queried workspace reached from
//! the start ids within `max_depth`, and the edges among them. Extracted from `sqlite.rs` as a
//! pure move (M3-25). EN-F19 `follow_anchors` (engine-plan E2.7) expands the kept nodes one step
//! into the attached code-graph snapshot after the memory walk ([`follow_anchors`]). EN-F46
//! (engine-plan E2.8): `rels` and `node_kinds` are validated against the loaded ontology before
//! the walk (an undeclared name is `UnknownRel`/`UnknownKind`, never a silent empty result) and
//! the edge filter runs inside every neighbour query, before the hub cut.

use crate::candidates;
use crate::connection::CgRead;
use crate::ontology;
use crate::rows::{NODE_COLS, row_edge, row_node};
use crate::sql_error::SqlResultExt;
use crate::sqlite::{SqliteStore, WS_COL, in_workspace};
use kaban_core::{
    AnchoredRow, Edge, EdgeFilter, Lifecycle, Node, NodeId, Result, StoreError, TraverseQuery,
    TraverseResult,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeSet, HashSet};

/// Most edges one node contributes per direction and hop (hub protection).
const HUB_LIMIT: usize = 200;
/// EN-F19 / CG-JOIN v1 §9 `Limits`: anchored cards per anchored node, after ordering.
const ANCHORED_PER_NODE: usize = 200;
/// EN-F19 / CG-JOIN v1 §9 `Limits`: anchored cards per traversal; the expansion stops here.
const ANCHORED_TOTAL: usize = 500;

/// Append the relationship filter (`rels` bound as `?3..`, M3 review R3-7), the EN-F46 edge
/// filter (its values bound after the rels, in the order `layers`, `methods`, `min_score`)
/// and the deterministic hub cut to a neighbour query: the filters run before the cut, so a
/// hub whose oldest 200 edges use other relationships or layers still yields its matching
/// neighbours. The order key stays `e.id`, a column (N5).
fn with_rels_and_hub_cut(select: &str, rels: usize, edge_filter: Option<&EdgeFilter>) -> String {
    let in_list = |column: &str, n: usize| {
        if n == 0 {
            String::new()
        } else {
            format!(" AND e.{column} IN ({})", vec!["?"; n].join(","))
        }
    };
    let mut filter = in_list("rel", rels);
    if let Some(f) = edge_filter {
        filter.push_str(&in_list("layer", f.layers.len()));
        filter.push_str(&in_list("method", f.methods.len()));
        if f.min_score.is_some() {
            filter.push_str(" AND e.score >= ?");
        }
    }
    format!("{select}{filter} ORDER BY e.id LIMIT {HUB_LIMIT}")
}

/// The bound values of the edge filter, in the order [`with_rels_and_hub_cut`] placed them;
/// a `Fixed` crosses to the `REAL` column as `value / 10000`.
fn edge_filter_params(edge_filter: Option<&EdgeFilter>) -> Vec<Box<dyn rusqlite::ToSql>> {
    let Some(f) = edge_filter else {
        return Vec::new();
    };
    let mut p: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    p.extend(
        f.layers
            .iter()
            .map(|l| Box::new(l.as_str()) as Box<dyn rusqlite::ToSql>),
    );
    p.extend(
        f.methods
            .iter()
            .map(|m| Box::new(m.as_str()) as Box<dyn rusqlite::ToSql>),
    );
    if let Some(min) = f.min_score {
        p.push(Box::new(f64::from(min.0) / 10_000.0));
    }
    p
}

/// EN-F46: every `node_kinds` entry is a declared label and every `rels` entry a declared
/// relationship type of the loaded ontology; the first undeclared name is the error (exit 4
/// at the CLI), rendered as its class alone (`unknown_kind`, `unknown_rel`: the name is caller
/// data, RW3-6). Runs before the walk, so a filter can never match nothing silently.
fn validate_filters(conn: &Connection, q: &TraverseQuery) -> Result<()> {
    for kind in &q.node_kinds {
        if !ontology::kind_exists(conn, kind)? {
            return Err(StoreError::UnknownKind(kind.clone()));
        }
    }
    for rel in &q.rels {
        if ontology::rel_def(conn, rel)?.is_none() {
            return Err(StoreError::UnknownRel(rel.clone()));
        }
    }
    Ok(())
}

/// The traversal; `cg` is the store's read slot (E2.7 default 2: taken after the main lock,
/// read only when `q.follow_anchors` asks for the expansion).
pub(crate) fn traverse(
    conn: &Connection,
    cg: Option<&CgRead>,
    q: &TraverseQuery,
) -> Result<TraverseResult> {
    validate_filters(conn, q)?;
    let depth = q.max_depth.clamp(1, 4);
    let limit = if q.limit == 0 { 100 } else { q.limit.min(500) };
    let mut seen: HashSet<i64> = HashSet::new();
    let mut frontier: Vec<i64> = Vec::new();
    for id in &q.start_ids {
        let (rid, _) = SqliteStore::rid_of(conn, id.as_str())?;
        // a start node outside the bound workspace (S6) or not active (§4, F6: traversal
        // runs over active nodes; F5: tombstoned rows never resurface, archived ones are
        // not traversal starts either, R2-4) is as good as absent
        let visible: bool = conn.query_row(
            &format!(
                "SELECT (?1 IS NULL OR {WS_COL} = ?1) AND n.lifecycle = 'active'{} FROM node n WHERE n.rid=?2",
                candidates::exclusion_sql(conn, "n", q.filters)?
            ),
            params![q.workspace_id, rid],
            |r| r.get(0),
        ).sql()?;
        if !visible {
            return Err(StoreError::NotFound(id.to_string()));
        }
        seen.insert(rid);
        frontier.push(rid);
    }
    let mut edges: Vec<Edge> = Vec::new();
    let mut edge_ids: HashSet<String> = HashSet::new();
    // the far endpoint's workspace (review R1-5) and lifecycle (review R2-4) are checked
    // before it joins the frontier, so a hidden node never bridges two visible ones; a node
    // contributes at most `HUB_LIMIT` edges per direction, chosen by edge id so the cut is
    // deterministic (hub protection, core-plan C3.6; N5)
    // EN-F49: a candidate far endpoint never joins the frontier unless asked
    let sql_out = with_rels_and_hub_cut(
        &format!(
            "SELECT e.id, s.id, d.id, e.rel, e.confidence, e.created_at, e.props, e.dst FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst WHERE e.src=?1 AND (?2 IS NULL OR d.workspace_id = ?2) AND d.lifecycle = 'active'{}",
            candidates::exclusion_sql(conn, "d", q.filters)?
        ),
        q.rels.len(),
        q.edge_filter.as_ref(),
    );
    let sql_in = with_rels_and_hub_cut(
        &format!(
            "SELECT e.id, s.id, d.id, e.rel, e.confidence, e.created_at, e.props, e.src FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst WHERE e.dst=?1 AND (?2 IS NULL OR s.workspace_id = ?2) AND s.lifecycle = 'active'{}",
            candidates::exclusion_sql(conn, "s", q.filters)?
        ),
        q.rels.len(),
        q.edge_filter.as_ref(),
    );
    let mut st_out = conn.prepare_cached(&sql_out).sql()?;
    let mut st_in = conn.prepare_cached(&sql_in).sql()?;
    // `?1` node, `?2` workspace, then one bound value per relationship (R3-7), then the
    // edge filter's values (EN-F46)
    let neighbour_params = |rid: i64| -> Vec<Box<dyn rusqlite::ToSql>> {
        let mut p: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(rid), Box::new(q.workspace_id.clone())];
        p.extend(
            q.rels
                .iter()
                .map(|r| Box::new(r.clone()) as Box<dyn rusqlite::ToSql>),
        );
        p.extend(edge_filter_params(q.edge_filter.as_ref()));
        p
    };
    for _ in 0..depth {
        let mut next = Vec::new();
        for rid in &frontier {
            let mut rows: Vec<(Edge, i64)> = Vec::new();
            let p = neighbour_params(*rid);
            let p: Vec<&dyn rusqlite::ToSql> = p.iter().map(AsRef::as_ref).collect();
            if q.direction.follows_out() {
                rows.extend(
                    st_out
                        .query_map(p.as_slice(), |r| Ok((row_edge(r)?, r.get::<_, i64>(7)?)))
                        .sql()?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .sql()?,
                );
            }
            if q.direction.follows_in() {
                rows.extend(
                    st_in
                        .query_map(p.as_slice(), |r| Ok((row_edge(r)?, r.get::<_, i64>(7)?)))
                        .sql()?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .sql()?,
                );
            }
            for (e, other) in rows {
                if edge_ids.insert(e.id.clone()) {
                    edges.push(e);
                }
                if seen.insert(other) {
                    next.push(other);
                }
                if seen.len() >= limit {
                    break;
                }
            }
        }
        frontier = next;
        if frontier.is_empty() || seen.len() >= limit {
            break;
        }
    }
    let mut st = conn
        .prepare_cached(&format!("SELECT {NODE_COLS} FROM node WHERE rid=?"))
        .sql()?;
    let mut nodes = Vec::new();
    let mut kept_rids = Vec::new();
    let mut rids: Vec<i64> = seen.iter().copied().collect();
    rids.sort_unstable();
    for rid in rids {
        if let Some(n) = st.query_row([rid], row_node).optional().sql()? {
            let keep = (n.lifecycle == Lifecycle::Active
                || q.start_ids.iter().any(|s| s.as_str() == n.id))
                && (q.node_kinds.is_empty() || q.node_kinds.contains(&n.kind))
                && (q.governance.is_empty() || q.governance.contains(&n.governance))
                && in_workspace(&n, q.workspace_id.as_deref());
            if keep {
                nodes.push(n);
                kept_rids.push(rid);
            }
        }
    }
    let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    edges.retain(|e| ids.contains(e.src.as_str()) && ids.contains(e.dst.as_str()));
    edges.sort_by(|a, b| a.id.cmp(&b.id));
    // EN-F19: the expansion runs after the walk, over the kept nodes in their order; off, or
    // without a repository or an attached snapshot, the result is today's byte for byte. The
    // flag is the caller's admission for this request (`api/query.rs` sets it only under a
    // resolution it attached in the same call, S7): the attachment an earlier request left
    // on the read connection never expands on its own.
    let anchored = match (q.follow_anchors, q.anchor_repo.as_deref(), cg) {
        (true, Some(repo), Some(cg)) if cg.attached_commit().is_some() => {
            follow_anchors(conn, cg, q, repo, &nodes, &kept_rids)?
        }
        _ => Vec::new(),
    };
    Ok(TraverseResult {
        nodes,
        edges,
        anchored,
    })
}

/// A card before its `from` is known: `(path, ref, key, name)`; a `file` has no `ref`. The
/// tuple order is the CG-JOIN v1 §9 order `(path, ref)` (a file sorts before the symbols it
/// defines), and the set deduplicates a row reached through two anchors of one node.
type Candidate = (String, Option<String>, Option<String>, Option<String>);

/// The EN-F19 expansion (engine-plan E2.7 item 2): for each kept node, its `path` and `symbol`
/// anchor rows under the EN-F18a predicates (the query's workspace, `repo`, the attached
/// commit or an unscoped row) are read on the main connection, then resolved in `cg` on the
/// read connection. A `path` key names a `cg.files` row (or, ending in `/`, every file under
/// it; the universal key `""` expands nothing) and gives the `file` card plus the file's
/// `kind = 'symbol' AND external = 0` symbols; a `symbol` key gives its unique row plus its
/// one-hop `cg.edges` neighbours (an ambiguous or absent key gives nothing). Each node's
/// candidate set is complete, ordered by `(path, ref)` and truncated to [`ANCHORED_PER_NODE`];
/// the walk stops at [`ANCHORED_TOTAL`]. Only the §8 stable columns are read, every `cg`
/// statement prepared uncached so it is finalised before a later `DETACH` (`anchors.rs`).
fn follow_anchors(
    conn: &Connection,
    cg: &CgRead,
    q: &TraverseQuery,
    repo: &str,
    nodes: &[Node],
    rids: &[i64],
) -> Result<Vec<AnchoredRow>> {
    let commit = cg.attached_commit().unwrap_or_default().to_owned();
    // the anchor rows of one node under the predicates; `?4` is the workspace or NULL
    let mut st_anchors = conn
        .prepare_cached(
            "SELECT a.anchor_kind, a.key FROM node_anchor a \
             WHERE a.rid = ?1 AND a.anchor_kind IN ('path', 'symbol') AND a.repo = ?2 \
               AND (a.snapshot_scope IS NULL OR a.snapshot_scope = ?3) \
               AND (?4 IS NULL OR a.workspace_id = ?4) \
             ORDER BY a.anchor_kind, a.key",
        )
        .sql()?;
    let cgc = cg.conn();
    let mut st_file = cgc
        .prepare(
            "SELECT f.id, f.path FROM cg.files f \
             WHERE CASE WHEN ?2 THEN substr(f.path, 1, length(?1)) = ?1 ELSE f.path = ?1 END \
             ORDER BY f.path",
        )
        .sql()?;
    let mut st_file_symbols = cgc
        .prepare(
            "SELECT s.ref, s.key, s.name FROM cg.symbols s \
             WHERE s.file_id = ?1 AND s.kind = 'symbol' AND s.external = 0",
        )
        .sql()?;
    let mut st_symbol = cgc
        .prepare(
            "SELECT s.ref, s.key, s.name, f.path, s.id FROM cg.symbols s \
             JOIN cg.files f ON f.id = s.file_id \
             WHERE s.key = ?1 AND s.kind = 'symbol' AND s.external = 0 LIMIT 2",
        )
        .sql()?;
    let mut st_neighbours = cgc
        .prepare(
            "SELECT n.ref, n.key, n.name, f.path FROM cg.edges e \
             JOIN cg.symbols n ON n.id = CASE WHEN e.src = ?1 THEN e.dst ELSE e.src END \
             JOIN cg.files f ON f.id = n.file_id \
             WHERE (e.src = ?1 OR e.dst = ?1) AND n.kind = 'symbol' AND n.external = 0",
        )
        .sql()?;
    let symbol_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Candidate> {
        Ok((
            r.get::<_, String>(3)?,
            Some(r.get::<_, String>(0)?),
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    };
    let mut out: Vec<AnchoredRow> = Vec::new();
    for (node, rid) in nodes.iter().zip(rids) {
        if out.len() >= ANCHORED_TOTAL {
            break;
        }
        let anchors = st_anchors
            .query_map(params![rid, repo, commit, q.workspace_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .sql()?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()
            .sql()?;
        if anchors.is_empty() {
            continue;
        }
        let mut cards: BTreeSet<Candidate> = BTreeSet::new();
        for (kind, key) in &anchors {
            if kind == "path" {
                if key.is_empty() {
                    continue; // the universal key names every path and expands nothing
                }
                let prefix = key.ends_with('/');
                let files = st_file
                    .query_map(params![key, prefix], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })
                    .sql()?
                    .collect::<rusqlite::Result<Vec<(i64, String)>>>()
                    .sql()?;
                for (file_id, path) in files {
                    cards.insert((path.clone(), None, None, None));
                    let symbols = st_file_symbols
                        .query_map([file_id], |r| {
                            Ok((
                                path.clone(),
                                Some(r.get::<_, String>(0)?),
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                            ))
                        })
                        .sql()?
                        .collect::<rusqlite::Result<Vec<Candidate>>>()
                        .sql()?;
                    cards.extend(symbols);
                }
            } else {
                let rows = st_symbol
                    .query_map([key], |r| Ok((r.get::<_, i64>(4)?, symbol_row(r)?)))
                    .sql()?
                    .collect::<rusqlite::Result<Vec<(i64, Candidate)>>>()
                    .sql()?;
                // unique in the snapshot or nothing (CG-JOIN v1 §6: ambiguity is never guessed)
                let [(symbol_id, card)] = rows.as_slice() else {
                    continue;
                };
                cards.insert(card.clone());
                let neighbours = st_neighbours
                    .query_map([symbol_id], symbol_row)
                    .sql()?
                    .collect::<rusqlite::Result<Vec<Candidate>>>()
                    .sql()?;
                cards.extend(neighbours);
            }
        }
        let from = NodeId::parse(&node.id)?;
        let room = ANCHORED_TOTAL - out.len();
        out.extend(cards.into_iter().take(ANCHORED_PER_NODE.min(room)).map(
            |(path, r#ref, key, name)| match r#ref {
                Some(r#ref) => AnchoredRow::symbol(from.clone(), path, r#ref, key, name),
                None => AnchoredRow::file(from.clone(), path),
            },
        ));
    }
    Ok(out)
}
