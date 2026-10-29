//! `neighbours` (stage1-spec G1-F12 `code_neighbours`): iterative BFS over edges of the requested
//! types in both directions, `Limits::neighbours_per_hub` edges expanded per hub and
//! `Limits::neighbours_total` distinct edges in total, at most `depth` hops; deterministic by
//! `(depth, ref)`. An edge reached from both of its ends is one relationship: it is emitted and
//! counted once by `(src, type, dst)` (RG2-3). These are traversal bounds of G1-F12, not list
//! limits (RV-7). Edges to document-local symbols are left out of the walk and counted
//! (`locals_hidden`) unless `include_locals` (G4: a Go function's neighbourhood is otherwise
//! its own parameters and variables, `local N` named by the id); a hidden edge costs no bound.

use std::collections::HashSet;

use rusqlite::Connection;
use rusqlite::types::ToSql;

use super::{nodes, placeholders};
use crate::error::ReadError;
use crate::limits::{Depth, Limits};
use crate::reader::{NeighbourEdge, NeighbourGraph};
use crate::schema::{EdgeType, NodeId};

/// One edge touching a hub, with the far node's id and ref (`src` tells the direction).
struct Touch {
    src: NodeId,
    r#type: EdgeType,
    far: NodeId,
    far_ref: String,
}

pub(super) fn neighbours(
    conn: &Connection,
    start: NodeId,
    depth: Depth,
    edge_types: &[EdgeType],
    include_locals: bool,
    limits: &Limits,
) -> Result<NeighbourGraph, ReadError> {
    // a `Depth` is valid relative to the `Limits` it was built under; this snapshot's own
    // ceiling is checked here, at the boundary (RG2-4)
    if depth.get() > limits.depth_max {
        return Err(ReadError::OutOfRange { argument: "depth" });
    }
    let Some(start_row) = nodes::node_by_id(conn, start)? else {
        return Ok(NeighbourGraph::default());
    };
    let start_ref = start_row.r#ref.clone();
    let mut graph = NeighbourGraph {
        nodes: vec![start_row],
        edges: Vec::new(),
        truncated: false,
        locals_hidden: 0,
    };
    let mut visited: HashSet<NodeId> = HashSet::from([start]);
    // an edge touches both of its ends: it is emitted and counted once, by its identity (RG2-3)
    let mut emitted: HashSet<(NodeId, EdgeType, NodeId)> = HashSet::new();
    // the frontier carries each hub's ref: hubs are graph nodes already
    let mut frontier = vec![(start, start_ref)];
    for _ in 0..depth.get() {
        let mut next: Vec<(NodeId, String)> = Vec::new();
        for (hub, hub_ref) in &frontier {
            let touches = touching(
                conn,
                *hub,
                edge_types,
                include_locals,
                limits.neighbours_per_hub,
            )?;
            if !include_locals {
                graph.locals_hidden += local_touches(conn, *hub, edge_types)?;
            }
            if touches.len() > limits.neighbours_per_hub {
                graph.truncated = true;
            }
            for t in touches.into_iter().take(limits.neighbours_per_hub) {
                let identity = if t.src == *hub {
                    (*hub, t.r#type, t.far)
                } else {
                    (t.far, t.r#type, *hub)
                };
                if !emitted.insert(identity) {
                    continue;
                }
                if graph.edges.len() >= limits.neighbours_total {
                    graph.truncated = true;
                    break;
                }
                let (from, to) = if t.src == *hub {
                    (hub_ref.clone(), t.far_ref.clone())
                } else {
                    (t.far_ref.clone(), hub_ref.clone())
                };
                graph.edges.push(NeighbourEdge {
                    from,
                    r#type: t.r#type,
                    to,
                });
                if visited.insert(t.far) {
                    next.push((t.far, t.far_ref));
                }
            }
        }
        next.sort_by(|a, b| a.1.cmp(&b.1));
        for (id, _) in &next {
            if let Some(row) = nodes::node_by_id(conn, *id)? {
                graph.nodes.push(row);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(graph)
}

/// The far node of an edge touching `?1`, in SQL.
const FAR_NODE: &str = "symbols s ON s.id = CASE WHEN e.src = ?1 THEN e.dst ELSE e.src END";

/// `AND e.type IN (?first, ...)` for the edge-type `names`, pushed onto `params` (which holds
/// `first - 1` values already); empty for every type.
fn type_filter<'a>(names: &'a [&'a str], first: usize, params: &mut Vec<&'a dyn ToSql>) -> String {
    if names.is_empty() {
        return String::new();
    }
    params.extend(names.iter().map(|n| n as &dyn ToSql));
    format!(" AND e.type IN ({})", placeholders(first, names.len()))
}

/// Edges touching `hub` in either direction, in `(far ref, type)` order, `per_hub + 1` at most so
/// the caller can tell a cut from an exact fit; edges whose far node is a local are left out
/// unless `include_locals`.
fn touching(
    conn: &Connection,
    hub: NodeId,
    edge_types: &[EdgeType],
    include_locals: bool,
    per_hub: usize,
) -> Result<Vec<Touch>, ReadError> {
    let fetch = i64::try_from(per_hub + 1).unwrap_or(i64::MAX);
    let mut params: Vec<&dyn ToSql> = vec![&hub.0, &fetch];
    let names: Vec<&str> = edge_types.iter().map(|t| t.as_str()).collect();
    let type_filter = type_filter(&names, 3, &mut params);
    let kind_filter = if include_locals {
        ""
    } else {
        " AND s.kind != 'local'"
    };
    let sql = format!(
        "SELECT e.src, e.dst, e.type, s.id, s.ref \
         FROM edges e JOIN {FAR_NODE} \
         WHERE (e.src = ?1 OR e.dst = ?1){type_filter}{kind_filter} \
         ORDER BY s.ref, e.type, e.src LIMIT ?2"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|r| {
        let (src, _dst, r#type, far, far_ref) = r?;
        Ok(Touch {
            src: NodeId(src),
            r#type: r#type.parse()?,
            far: NodeId(far),
            far_ref,
        })
    })
    .collect()
}

/// How many edges of the requested types touch `hub` at a local far node (the edges
/// [`touching`] leaves out without `include_locals`).
fn local_touches(
    conn: &Connection,
    hub: NodeId,
    edge_types: &[EdgeType],
) -> Result<usize, ReadError> {
    let mut params: Vec<&dyn ToSql> = vec![&hub.0];
    let names: Vec<&str> = edge_types.iter().map(|t| t.as_str()).collect();
    let type_filter = type_filter(&names, 2, &mut params);
    let sql = format!(
        "SELECT count(*) FROM edges e JOIN {FAR_NODE} \
         WHERE (e.src = ?1 OR e.dst = ?1){type_filter} AND s.kind = 'local'"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let n: i64 = stmt.query_row(params.as_slice(), |r| r.get(0))?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}
