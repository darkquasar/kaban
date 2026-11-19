//! Bounded graph traversal (core-spec F6): active nodes of the queried workspace reached from
//! the start ids within `max_depth`, and the edges among them. Extracted from `sqlite.rs` as a
//! pure move (M3-25). EN-F46
//! (engine-plan E2.8): `rels` and `node_kinds` are validated against the loaded ontology before
//! the walk (an undeclared name is `UnknownRel`/`UnknownKind`, never a silent empty result) and
//! the edge filter runs inside every neighbour query, before the hub cut.

use crate::ontology;
use crate::rows::{NODE_COLS, row_edge, row_node};
use crate::sql_error::SqlResultExt;
use crate::sqlite::{SqliteStore, WS_COL, in_workspace};
use kaban_core::{Edge, EdgeFilter, Lifecycle, Result, StoreError, TraverseQuery, TraverseResult};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;

/// Most edges one node contributes per direction and hop (hub protection).
const HUB_LIMIT: usize = 200;

/// Append the relationship filter (`rels` bound as `?3..`, M3 review R3-7), the EN-F46 edge
/// filter (its values bound after the rels, in the order `producers`, `methods`, `min_score`)
/// and the deterministic hub cut to a neighbour query: the filters run before the cut, so a
/// hub whose oldest 200 edges use other relationships or producers still yields its matching
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
        filter.push_str(&in_list("producer", f.producers.len()));
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
        f.producers
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

/// The traversal.
pub(crate) fn traverse(conn: &Connection, q: &TraverseQuery) -> Result<TraverseResult> {
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
                "SELECT (?1 IS NULL OR {WS_COL} = ?1) AND n.lifecycle = 'active' FROM node n WHERE n.rid=?2"
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
    let sql_out = with_rels_and_hub_cut(
        "SELECT e.id, s.id, d.id, e.rel, e.confidence, e.created_at, e.props, e.dst FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst WHERE e.src=?1 AND (?2 IS NULL OR d.workspace_id = ?2) AND d.lifecycle = 'active'",
        q.rels.len(),
        q.edge_filter.as_ref(),
    );
    let sql_in = with_rels_and_hub_cut(
        "SELECT e.id, s.id, d.id, e.rel, e.confidence, e.created_at, e.props, e.src FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst WHERE e.dst=?1 AND (?2 IS NULL OR s.workspace_id = ?2) AND s.lifecycle = 'active'",
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
    let mut rids: Vec<i64> = seen.iter().copied().collect();
    rids.sort_unstable();
    for rid in rids {
        if let Some(n) = st.query_row([rid], row_node).optional().sql()? {
            let keep = (n.lifecycle == Lifecycle::Active
                || q.start_ids.iter().any(|s| s.as_str() == n.id))
                && (q.node_kinds.is_empty() || q.node_kinds.contains(&n.kind))
                && in_workspace(&n, q.workspace_id.as_deref());
            if keep {
                nodes.push(n);
            }
        }
    }
    let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    edges.retain(|e| ids.contains(e.src.as_str()) && ids.contains(e.dst.as_str()));
    edges.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(TraverseResult { nodes, edges })
}
