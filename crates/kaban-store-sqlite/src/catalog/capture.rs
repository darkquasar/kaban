//! Change capture without writer cooperation (spec 11 KER-03, INV-5): TEMP insert, update and
//! delete triggers on `node` and `edge` queue each change's identity, operation and the
//! before/after values the index classes need (existence, label, lifecycle, the edge's
//! producer), deletion values before they disappear, FK cascade deletes included.
//! [`super::Catalog::commit`] freezes and drains the queue once after the writer closure; every
//! consumer sees that one immutable set.
//!
//! The triggers and the queue are TEMP objects of the writer connection: created at open,
//! never stored in the database file, they see exactly the writes of that connection. An update
//! trigger fires on the canonical columns only: the writer stamp (`actor_id`, `instance_id`,
//! `pack_lineage_id`, which `changes_stamp` sets after every change-log row) is not a change of
//! its own.

use kaban_core::Result;
use rusqlite::{Connection, Row};

use crate::rows::bad_enum;
use crate::sql_error::SqlResultExt;

/// What happened to a captured row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureOp {
    /// The row was inserted.
    Insert,
    /// The row was updated.
    Update,
    /// The row was deleted.
    Delete,
}

impl CaptureOp {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "insert" => Some(Self::Insert),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// The captured values of a node row on one side of a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeImage {
    /// The node's label (`kind`).
    pub(crate) kind: String,
    /// The node's lifecycle.
    pub(crate) lifecycle: String,
}

/// One captured node change; a side that does not exist (before an insert, after a delete) is
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeChange {
    /// The node's local id (`node.rid`, PC-1).
    pub(crate) rid: i64,
    /// The operation.
    pub(crate) op: CaptureOp,
    /// The row before the change.
    pub(crate) before: Option<NodeImage>,
    /// The row after the change.
    pub(crate) after: Option<NodeImage>,
}

/// The captured values of an edge row on one side of a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeImage {
    /// The producer that wrote the row (PC-16), `None` for a row without one.
    pub(crate) producer: Option<String>,
}

/// One captured edge change, by the edge's identity `(src, dst, rel)`. An update that moves an
/// edge to another identity is captured as the delete of the old and the insert of the new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeChange {
    /// The source node's `rid`.
    pub(crate) src: i64,
    /// The target node's `rid`.
    pub(crate) dst: i64,
    /// The relationship type.
    pub(crate) rel: String,
    /// The operation.
    pub(crate) op: CaptureOp,
    /// The row before the change.
    pub(crate) before: Option<EdgeImage>,
    /// The row after the change.
    pub(crate) after: Option<EdgeImage>,
}

/// The frozen change set of one commit, in capture order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BatchChanges {
    nodes: Vec<NodeChange>,
    edges: Vec<EdgeChange>,
}

/// The set of a commit that changed nothing.
#[expect(
    dead_code,
    reason = "read through `Txn::changes`, which T13's derived-row hook uses"
)]
pub(crate) static NO_CHANGES: BatchChanges = BatchChanges {
    nodes: Vec::new(),
    edges: Vec::new(),
};

impl BatchChanges {
    /// The captured node changes.
    pub(crate) fn nodes(&self) -> &[NodeChange] {
        &self.nodes
    }
    /// The captured edge changes.
    pub(crate) fn edges(&self) -> &[EdgeChange] {
        &self.edges
    }
    /// Whether the commit changed no captured row.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "T7's classes skip an empty commit")
    )]
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }
}

/// The canonical node columns an update is captured for (every column but the writer stamp).
const NODE_COLUMNS: &str = "id, kind, text, summary, tags, confidence, importance, status, \
    created_at, updated_at, valid_from, valid_to, source_session, agent_id, workspace_id, props, \
    content_hash, lifecycle, store_scope, path_scope, rev, dup_key, repo, commit_sha, session_id";

/// The canonical edge columns an update is captured for: those of `edge_changes_au`.
const EDGE_COLUMNS: &str = "id, src, dst, rel, confidence, created_at, props, workspace_id, \
    producer, method, score, source_id, offset, len";

/// The queue and the triggers. `b_*`/`a_*` hold the before/after image; `b_exists`/`a_exists`
/// say which sides exist.
fn capture_ddl() -> String {
    format!(
        "CREATE TEMP TABLE IF NOT EXISTS cat_capture(
           seq INTEGER PRIMARY KEY,
           entity TEXT NOT NULL,
           op TEXT NOT NULL,
           rid INTEGER, src INTEGER, dst INTEGER, rel TEXT,
           b_exists INTEGER NOT NULL, b_kind TEXT, b_lifecycle TEXT, b_producer TEXT,
           a_exists INTEGER NOT NULL, a_kind TEXT, a_lifecycle TEXT, a_producer TEXT
         );
         CREATE TEMP TRIGGER IF NOT EXISTS cat_capture_node_ai AFTER INSERT ON main.node BEGIN
           INSERT INTO cat_capture(entity, op, rid, b_exists, a_exists, a_kind, a_lifecycle)
             VALUES ('node', 'insert', new.rid, 0, 1, new.kind, new.lifecycle);
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_capture_node_au
             AFTER UPDATE OF {NODE_COLUMNS} ON main.node BEGIN
           INSERT INTO cat_capture(entity, op, rid, b_exists, b_kind, b_lifecycle,
                                   a_exists, a_kind, a_lifecycle)
             VALUES ('node', 'update', new.rid, 1, old.kind, old.lifecycle,
                     1, new.kind, new.lifecycle);
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_capture_node_ad AFTER DELETE ON main.node BEGIN
           INSERT INTO cat_capture(entity, op, rid, b_exists, b_kind, b_lifecycle, a_exists)
             VALUES ('node', 'delete', old.rid, 1, old.kind, old.lifecycle, 0);
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_capture_edge_ai AFTER INSERT ON main.edge BEGIN
           INSERT INTO cat_capture(entity, op, src, dst, rel, b_exists, a_exists, a_producer)
             VALUES ('edge', 'insert', new.src, new.dst, new.rel, 0, 1, new.producer);
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_capture_edge_au
             AFTER UPDATE OF {EDGE_COLUMNS} ON main.edge
             WHEN old.src = new.src AND old.dst = new.dst AND old.rel = new.rel BEGIN
           INSERT INTO cat_capture(entity, op, src, dst, rel, b_exists, b_producer,
                                   a_exists, a_producer)
             VALUES ('edge', 'update', new.src, new.dst, new.rel, 1, old.producer,
                     1, new.producer);
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_capture_edge_moved
             AFTER UPDATE OF {EDGE_COLUMNS} ON main.edge
             WHEN NOT (old.src = new.src AND old.dst = new.dst AND old.rel = new.rel) BEGIN
           INSERT INTO cat_capture(entity, op, src, dst, rel, b_exists, b_producer, a_exists)
             VALUES ('edge', 'delete', old.src, old.dst, old.rel, 1, old.producer, 0);
           INSERT INTO cat_capture(entity, op, src, dst, rel, b_exists, a_exists, a_producer)
             VALUES ('edge', 'insert', new.src, new.dst, new.rel, 0, 1, new.producer);
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_capture_edge_ad AFTER DELETE ON main.edge BEGIN
           INSERT INTO cat_capture(entity, op, src, dst, rel, b_exists, b_producer, a_exists)
             VALUES ('edge', 'delete', old.src, old.dst, old.rel, 1, old.producer, 0);
         END;"
    )
}

/// Install the TEMP capture triggers and their queue on a writer connection at open.
///
/// # Errors
/// SQLite errors.
pub(crate) fn install(conn: &Connection) -> Result<()> {
    conn.execute_batch(&capture_ddl()).sql()
}

/// Discard whatever the queue holds: the start of every commit, so a commit's set holds that
/// commit's writes only (not those of a writer that committed outside the catalog).
///
/// # Errors
/// SQLite errors.
pub(crate) fn reset(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM temp.cat_capture", []).sql()?;
    Ok(())
}

/// Freeze and drain the queue of the open transaction on `conn`: called once per commit.
///
/// # Errors
/// SQLite errors, or a queue row this module did not write.
pub(crate) fn drain(conn: &Connection) -> Result<BatchChanges> {
    let mut changes = BatchChanges::default();
    {
        let mut stmt = conn
            .prepare(
                "SELECT entity, op, rid, src, dst, rel, b_exists, b_kind, b_lifecycle, b_producer,
                        a_exists, a_kind, a_lifecycle, a_producer
                 FROM temp.cat_capture ORDER BY seq",
            )
            .sql()?;
        let mut rows = stmt.query([]).sql()?;
        while let Some(row) = rows.next().sql()? {
            let entity: String = row.get(0).sql()?;
            let op: String = row.get(1).sql()?;
            let op = CaptureOp::parse(&op)
                .ok_or_else(|| bad_enum(1, &op))
                .sql()?;
            match entity.as_str() {
                "node" => changes.nodes.push(node_change(row, op).sql()?),
                "edge" => changes.edges.push(edge_change(row, op).sql()?),
                other => return Err(bad_enum(0, other)).sql(),
            }
        }
    }
    reset(conn)?;
    Ok(changes)
}

/// The column of `b_exists`; `a_exists` follows the four before columns.
const BEFORE: usize = 6;
const AFTER: usize = 10;

fn node_change(row: &Row<'_>, op: CaptureOp) -> rusqlite::Result<NodeChange> {
    let image = |exists: usize| -> rusqlite::Result<Option<NodeImage>> {
        if !row.get::<_, bool>(exists)? {
            return Ok(None);
        }
        Ok(Some(NodeImage {
            kind: row.get(exists + 1)?,
            lifecycle: row.get(exists + 2)?,
        }))
    };
    Ok(NodeChange {
        rid: row.get(2)?,
        op,
        before: image(BEFORE)?,
        after: image(AFTER)?,
    })
}

fn edge_change(row: &Row<'_>, op: CaptureOp) -> rusqlite::Result<EdgeChange> {
    let image = |exists: usize| -> rusqlite::Result<Option<EdgeImage>> {
        if !row.get::<_, bool>(exists)? {
            return Ok(None);
        }
        Ok(Some(EdgeImage {
            producer: row.get(exists + 3)?,
        }))
    };
    Ok(EdgeChange {
        src: row.get(3)?,
        dst: row.get(4)?,
        rel: row.get(5)?,
        op,
        before: image(BEFORE)?,
        after: image(AFTER)?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    /// The two tables the triggers watch, with the cascades of `schema.sql`.
    fn rows_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE node(rid INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT, kind TEXT, text TEXT,
               summary TEXT, tags TEXT, confidence REAL, importance REAL, status TEXT,
               created_at TEXT, updated_at TEXT, valid_from TEXT, valid_to TEXT,
               source_session TEXT, agent_id TEXT, workspace_id TEXT, props TEXT,
               content_hash TEXT, lifecycle TEXT, store_scope TEXT, path_scope TEXT, rev INTEGER,
               dup_key TEXT, repo TEXT, commit_sha TEXT, session_id TEXT, actor_id TEXT,
               instance_id TEXT, pack_lineage_id TEXT);
             CREATE TABLE edge(id TEXT PRIMARY KEY,
               src INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
               dst INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
               rel TEXT NOT NULL, confidence REAL, created_at TEXT, props TEXT,
               workspace_id TEXT, producer TEXT, method TEXT, score REAL, source_id TEXT,
               offset INTEGER, len INTEGER, actor_id TEXT, instance_id TEXT,
               pack_lineage_id TEXT);",
        )
        .unwrap();
        install(&conn).unwrap();
        conn
    }

    fn fact(lifecycle: &str) -> NodeImage {
        NodeImage {
            kind: "Fact".into(),
            lifecycle: lifecycle.into(),
        }
    }

    #[test]
    fn a_node_row_delete_captures_the_node_and_every_cascaded_edge() {
        // Arrange: a hub with an outgoing and an incoming edge, the queue empty
        let conn = rows_db();
        conn.execute_batch(
            "INSERT INTO node(rid, kind, lifecycle)
               VALUES (1, 'Fact', 'active'), (2, 'Fact', 'active'), (3, 'Fact', 'active');
             INSERT INTO edge(id, src, dst, rel, producer)
               VALUES ('a', 1, 2, 'RELATES_TO', 'authored'), ('b', 3, 1, 'RELATES_TO', 'rules');",
        )
        .unwrap();
        reset(&conn).unwrap();

        // Act: delete the node row itself; the foreign keys cascade to both edges
        conn.execute("DELETE FROM node WHERE rid = 1", []).unwrap();
        let changes = drain(&conn).unwrap();

        // Assert: the node with its last values, and both edges with theirs
        assert_eq!(
            changes.nodes(),
            [NodeChange {
                rid: 1,
                op: CaptureOp::Delete,
                before: Some(fact("active")),
                after: None,
            }]
        );
        let edge = |src, dst, producer: &str| EdgeChange {
            src,
            dst,
            rel: "RELATES_TO".into(),
            op: CaptureOp::Delete,
            before: Some(EdgeImage {
                producer: Some(producer.into()),
            }),
            after: None,
        };
        let mut edges = changes.edges().to_vec();
        edges.sort_by_key(|e| (e.src, e.dst));
        assert_eq!(edges, [edge(1, 2, "authored"), edge(3, 1, "rules")]);
        assert!(
            drain(&conn).unwrap().is_empty(),
            "a drain empties the queue"
        );
    }

    #[test]
    fn an_update_keeps_both_sides_and_a_stamp_is_not_a_change() {
        // Arrange
        let conn = rows_db();
        conn.execute_batch(
            "INSERT INTO node(rid, kind, lifecycle) VALUES (1, 'Fact', 'active'), (2, 'Fact', 'active');
             INSERT INTO edge(id, src, dst, rel, producer) VALUES ('a', 1, 2, 'RELATES_TO', 'authored');",
        )
        .unwrap();
        reset(&conn).unwrap();

        // Act: a stamp on both tables, then a lifecycle change and a moved edge
        conn.execute_batch(
            "UPDATE node SET actor_id = 'x' WHERE rid = 1;
             UPDATE edge SET actor_id = 'x' WHERE id = 'a';
             UPDATE node SET lifecycle = 'archived' WHERE rid = 1;
             UPDATE edge SET dst = 1, src = 2 WHERE id = 'a';",
        )
        .unwrap();
        let changes = drain(&conn).unwrap();

        // Assert
        assert_eq!(
            changes.nodes(),
            [NodeChange {
                rid: 1,
                op: CaptureOp::Update,
                before: Some(fact("active")),
                after: Some(fact("archived")),
            }]
        );
        let ops: Vec<_> = changes
            .edges()
            .iter()
            .map(|e| (e.op, e.src, e.dst))
            .collect();
        assert_eq!(
            ops,
            [(CaptureOp::Delete, 1, 2), (CaptureOp::Insert, 2, 1)],
            "a moved edge is a delete and an insert"
        );
    }
}
