//! The typed queries (stage1-plan CG2.3; stage1-spec G1-F12, G1-F16, §4.1 indexes; cg-join-v1
//! §6, §9): pure SQL over the §4.1 indexes, one feature per module, implementing
//! [`CodeGraphReader`] for [`SqliteSnapshot`]. Every list query returns the complete candidate
//! set in `(path, ref)` order (SQLite sorts a `NULL` path, an external placeholder, first) with
//! no `LIMIT` in the reader (RV-7); the only reader-side bounds are the `neighbours` traversal
//! bounds.
//!
//! ```text
//!   nodes.rs       node, node_by_id, symbol_unique, nodes_in_file, by_label_name, by_name,
//!                  by_path_line, nodes_in_ranges, meta
//!   edges.rs       implementers (IMPLEMENTS), members (CONTAINS), file_members (top level of
//!                  a file), tests_for (TESTED_BY), tests_via (one REFERENCES hop),
//!                  tests_textual (paths and snippets)
//!   references.rs  references: occurrence sites grouped by enclosing node
//!   neighbours.rs  bounded BFS over edges in both directions
//! ```

mod edges;
mod neighbours;
mod nodes;
mod references;

use rusqlite::types::ToSql;
use rusqlite::{Connection, Row};

use crate::error::ReadError;
use crate::limits::Depth;
use crate::reader::{
    CodeGraphReader, Meta, NeighbourGraph, NodeRow, ReferenceGroup, TestLink, TestsTarget,
};
use crate::schema::{EdgeType, FileId, NodeId, NodeKind, Range};
use crate::sqlite::SqliteSnapshot;

/// Whether the snapshot was built under the live policy (G1-F8 `policy: current | stale | off`;
/// RV-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Policy {
    /// `meta.policy_version` equals the live policy version.
    Current,
    /// It differs or is absent: the snapshot's gate verdicts predate the live policy.
    Stale,
    /// The content scan was off when the snapshot was built (`meta.policy_version` is
    /// [`Policy::OFF`], DF-3): nothing was gated, so nothing is current or stale.
    Off,
}

impl Policy {
    /// The `policy_version` a snapshot carries when the content scan was disabled (the live
    /// policy's effective hash under the same setting).
    pub const OFF: &'static str = "off";

    /// The wire value (`ctl Enum`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Off => "off",
        }
    }

    /// The contract §4 projection rule in one place: `Off` when the snapshot's version says
    /// the scan was off, `Current` when it equals `live_policy_version`, else `Stale` (an
    /// absent version included).
    #[must_use]
    pub fn of(snapshot_policy_version: Option<&str>, live_policy_version: &str) -> Self {
        match snapshot_policy_version {
            Some(v) if v == Self::OFF => Self::Off,
            Some(v) if v == live_policy_version => Self::Current,
            _ => Self::Stale,
        }
    }
}

/// [`Policy::of`] over `meta.policy_version` (cg-join-v1 §4: the caller compares the
/// snapshot's version with the live one).
#[must_use]
pub fn reader_policy_flag(meta: &Meta, live_policy_version: &str) -> Policy {
    Policy::of(
        meta.get("policy_version").map(String::as_str),
        live_policy_version,
    )
}

/// The `symbols` columns of a [`NodeRow`], joined with the file path and the snippet.
const NODE_COLUMNS: &str = "s.id, s.kind, s.ref, s.scip, s.key, s.label, s.name, s.file_id, f.path, \
     s.def_sl, s.def_sc, s.def_el, s.def_ec, s.enc_sl, s.enc_sc, s.enc_el, s.enc_ec, \
     s.signature, s.doc, s.external, s.is_test, s.ident_terms, f.language, sn.text";

/// The node source with its file join and its snippet join (CG8.2; `snippets` is keyed by
/// `symbol_id`, so the join adds no row).
const NODE_FROM: &str = "FROM symbols s LEFT JOIN files f ON f.id = s.file_id \
     LEFT JOIN snippets sn ON sn.symbol_id = s.id";

/// The complete-candidate-set order (RV-7).
const NODE_ORDER: &str = "ORDER BY f.path, s.ref";

/// The order of one file's members: by definition range, then ref (`file_members`).
const RANGE_ORDER: &str = "ORDER BY s.def_sl, s.def_sc, s.ref";

/// Select nodes with an extra `join` clause and a `where` predicate, in `(path, ref)` order.
fn select_nodes(
    conn: &Connection,
    join: &str,
    predicate: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<NodeRow>, ReadError> {
    select_nodes_ordered(conn, join, predicate, NODE_ORDER, params)
}

/// [`select_nodes`] with an explicit `ORDER BY` clause.
fn select_nodes_ordered(
    conn: &Connection,
    join: &str,
    predicate: &str,
    order: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<NodeRow>, ReadError> {
    let sql = format!("SELECT {NODE_COLUMNS} {NODE_FROM} {join} WHERE {predicate} {order}");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params, read_node)?;
    rows.map(|r| r.map_err(ReadError::from).and_then(NodeRow::try_from))
        .collect()
}

/// The row before its closed-set columns are checked.
struct RawNode {
    id: i64,
    kind: String,
    r#ref: String,
    scip: Option<String>,
    key: Option<String>,
    label: String,
    name: String,
    file_id: Option<i64>,
    path: Option<String>,
    def: [Option<u32>; 4],
    enc: [Option<u32>; 4],
    signature: Option<String>,
    doc: Option<String>,
    external: bool,
    is_test: bool,
    ident_terms: String,
    language: Option<String>,
    snippet: Option<String>,
}

fn read_node(row: &Row<'_>) -> rusqlite::Result<RawNode> {
    Ok(RawNode {
        id: row.get(0)?,
        kind: row.get(1)?,
        r#ref: row.get(2)?,
        scip: row.get(3)?,
        key: row.get(4)?,
        label: row.get(5)?,
        name: row.get(6)?,
        file_id: row.get(7)?,
        path: row.get(8)?,
        def: [row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?],
        enc: [row.get(13)?, row.get(14)?, row.get(15)?, row.get(16)?],
        signature: row.get(17)?,
        doc: row.get(18)?,
        external: row.get(19)?,
        is_test: row.get(20)?,
        ident_terms: row.get(21)?,
        language: row.get(22)?,
        snippet: row.get(23)?,
    })
}

impl TryFrom<RawNode> for NodeRow {
    type Error = ReadError;

    fn try_from(raw: RawNode) -> Result<Self, ReadError> {
        Ok(NodeRow {
            id: NodeId(raw.id),
            kind: raw.kind.parse::<NodeKind>()?,
            r#ref: raw.r#ref,
            scip: raw.scip,
            key: raw.key,
            label: raw.label,
            name: raw.name,
            file_id: raw.file_id.map(FileId),
            path: raw.path,
            // an empty language is no language (the card shows `null`, never `""`)
            language: raw.language.filter(|l| !l.is_empty()),
            def: join_range(raw.def),
            enc: join_range(raw.enc),
            signature: raw.signature,
            doc: raw.doc,
            snippet: raw.snippet,
            external: raw.external,
            is_test: raw.is_test,
            ident_terms: raw.ident_terms,
        })
    }
}

/// Four nullable columns as one range; any `NULL` means no range.
fn join_range(cols: [Option<u32>; 4]) -> Option<Range> {
    match cols {
        [Some(sl), Some(sc), Some(el), Some(ec)] => Some(Range { sl, sc, el, ec }),
        _ => None,
    }
}

/// `?n, ?n+1, ...` placeholders for `count` extra parameters after `first`.
fn placeholders(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl CodeGraphReader for SqliteSnapshot {
    fn meta(&self) -> Result<Meta, ReadError> {
        self.with_conn(nodes::meta)
    }

    fn node(&self, r#ref: &str) -> Result<Option<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::node(c, r#ref))
    }

    fn node_by_id(&self, id: NodeId) -> Result<Option<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::node_by_id(c, id))
    }

    fn symbol_unique(&self, key: &str) -> Result<Option<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::symbol_unique(c, key))
    }

    fn nodes_in_file(&self, path: &str) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::nodes_in_file(c, path))
    }

    fn by_label_name(&self, label: &str, name: &str) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::by_label_name(c, label, name))
    }

    fn by_name(&self, name: &str) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::by_name(c, name))
    }

    fn by_path_line(&self, path: &str, line: u32) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::by_path_line(c, path, line))
    }

    fn nodes_in_ranges(
        &self,
        file: FileId,
        ranges: &[(u32, u32)],
    ) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| nodes::nodes_in_ranges(c, file, ranges))
    }

    fn implementers(&self, id: NodeId, include_external: bool) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| edges::implementers(c, id, include_external))
    }

    fn members(&self, id: NodeId, labels: &[&str]) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| edges::members(c, id, labels))
    }

    fn file_members(&self, file: FileId, labels: &[&str]) -> Result<Vec<NodeRow>, ReadError> {
        self.with_conn(|c| edges::file_members(c, file, labels))
    }

    fn references(
        &self,
        id: NodeId,
        include_tests: bool,
    ) -> Result<Vec<ReferenceGroup>, ReadError> {
        self.with_conn(|c| references::references(c, id, include_tests))
    }

    fn tests_for(&self, target: TestsTarget) -> Result<Vec<TestLink>, ReadError> {
        self.with_conn(|c| edges::tests_for(c, target))
    }

    fn tests_via(&self, id: NodeId, intermediates: usize) -> Result<Vec<TestLink>, ReadError> {
        self.with_conn(|c| edges::tests_via(c, id, intermediates))
    }

    fn tests_textual(&self, words: &[&str]) -> Result<Vec<TestLink>, ReadError> {
        self.with_conn(|c| edges::tests_textual(c, words))
    }

    fn neighbours_with(
        &self,
        id: NodeId,
        depth: Depth,
        edge_types: &[EdgeType],
        include_locals: bool,
    ) -> Result<NeighbourGraph, ReadError> {
        self.with_conn(|c| {
            neighbours::neighbours(c, id, depth, edge_types, include_locals, self.limits())
        })
    }
}
