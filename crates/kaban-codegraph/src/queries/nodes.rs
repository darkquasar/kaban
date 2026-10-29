//! Node lookups (stage1-spec §6 `node`, `by_label_name`, `by_name`, `by_path_line`, `nodes_in_ranges`;
//! cg-join-v1 §6 `symbol_unique`, §9 `nodes_in_file`) and the `meta` table.

use std::fmt::Write as _;

use rusqlite::types::ToSql;
use rusqlite::{Connection, params};

use super::select_nodes;
use crate::error::ReadError;
use crate::reader::{Meta, NodeRow};
use crate::schema::{FileId, NodeId};

pub(super) fn meta(conn: &Connection) -> Result<Meta, ReadError> {
    let mut stmt = conn.prepare_cached("SELECT key, value FROM meta")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect::<Result<Meta, _>>().map_err(ReadError::from)
}

pub(super) fn node(conn: &Connection, r#ref: &str) -> Result<Option<NodeRow>, ReadError> {
    let mut rows = select_nodes(conn, "", "s.ref = ?1", params![r#ref])?;
    Ok(rows.pop())
}

pub(super) fn node_by_id(conn: &Connection, id: NodeId) -> Result<Option<NodeRow>, ReadError> {
    let mut rows = select_nodes(conn, "", "s.id = ?1", params![id.0])?;
    Ok(rows.pop())
}

/// `kind = 'symbol' AND external = 0` rows sharing `key`: none, one, or `Ambiguous`.
pub(super) fn symbol_unique(conn: &Connection, key: &str) -> Result<Option<NodeRow>, ReadError> {
    let mut rows = select_nodes(
        conn,
        "",
        "s.key = ?1 AND s.kind = 'symbol' AND s.external = 0",
        params![key],
    )?;
    match rows.len() {
        0 => Ok(None),
        1 => Ok(rows.pop()),
        _ => Err(ReadError::Ambiguous),
    }
}

pub(super) fn nodes_in_file(conn: &Connection, path: &str) -> Result<Vec<NodeRow>, ReadError> {
    select_nodes(conn, "", "f.path = ?1", params![path])
}

pub(super) fn by_label_name(
    conn: &Connection,
    label: &str,
    name: &str,
) -> Result<Vec<NodeRow>, ReadError> {
    select_nodes(
        conn,
        "",
        "s.label = ?1 AND s.name = ?2",
        params![label, name],
    )
}

/// Every row named `name` whatever its label, kind or `external` flag (the `symbols_name`
/// index), by `(path, ref)`: the name selector's one read (kaban-api `code/selector.rs`).
pub(super) fn by_name(conn: &Connection, name: &str) -> Result<Vec<NodeRow>, ReadError> {
    select_nodes(conn, "", "s.name = ?1", params![name])
}

/// The lines a definition range touches, as SQL over the `def_*` columns against the inclusive
/// line interval `[?from, ?to]`: ranges are end-exclusive (`Range`), so `[sl:sc, el:ec)` touches
/// `sl..=el` when `ec > 0` and `sl..el` when `ec == 0` (an end at column 0 is the end of the
/// previous line); an empty range at `(l, c > 0)` touches line `l`, one at `(l, 0)` touches
/// nothing (RG2-5).
fn touches_lines(from: usize, to: usize) -> String {
    format!("(s.def_sl <= ?{to} AND (s.def_el > ?{from} OR (s.def_el = ?{from} AND s.def_ec > 0)))")
}

/// Definition ranges touching `line` in the file at `path` (file nodes have no range), by the
/// half-open rule of [`touches_lines`].
pub(super) fn by_path_line(
    conn: &Connection,
    path: &str,
    line: u32,
) -> Result<Vec<NodeRow>, ReadError> {
    let predicate = format!("f.path = ?1 AND {}", touches_lines(2, 2));
    select_nodes(conn, "", &predicate, params![path, line])
}

/// Definition ranges touching any inclusive `(from, to)` line interval of `file`, by the
/// half-open rule of [`touches_lines`]; an inverted interval (`from > to`) is empty and matches
/// nothing.
pub(super) fn nodes_in_ranges(
    conn: &Connection,
    file: FileId,
    ranges: &[(u32, u32)],
) -> Result<Vec<NodeRow>, ReadError> {
    let ranges: Vec<&(u32, u32)> = ranges.iter().filter(|(from, to)| from <= to).collect();
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let mut predicate = String::from("s.file_id = ?1 AND (");
    let mut params: Vec<&dyn ToSql> = vec![&file.0];
    for (i, (from, to)) in ranges.iter().enumerate() {
        if i > 0 {
            predicate.push_str(" OR ");
        }
        let (a, b) = (2 + 2 * i, 3 + 2 * i);
        let _ = write!(predicate, "{}", touches_lines(a, b));
        params.push(from);
        params.push(to);
    }
    predicate.push(')');
    select_nodes(conn, "", &predicate, &params)
}
