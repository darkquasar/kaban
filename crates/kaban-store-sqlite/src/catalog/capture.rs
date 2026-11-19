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
//!
//! Writes the AFTER triggers cannot see are refused or captured, never lost: SQLite fires no
//! delete trigger for a row a REPLACE conflict removes (recursive triggers stay off: the stamp
//! trigger updates the rows it fires for). So a node insert or update that would replace
//! another row, a change of a node's `rid` (its identity, PC-1), and an edge insert or update
//! that would replace an edge of another identity raise `catalog_capture_refused` (a plain
//! statement of that shape fails on its constraint anyway). An `INSERT OR REPLACE` of an
//! existing edge identity, which the store's upsert shares its BEFORE trigger with, is
//! captured instead: the replaced row's last values are kept before the insert
//! (`cat_replaced`) and queued as its delete.
//!
//! The identity guards judge values, not the statement's spelling (END-MED-02): SQLite matches
//! an `UPDATE OF` column list against the names a statement writes, so `SET rowid`, `_rowid_`
//! or `oid` would pass a list naming `rid`. The rowid guards (`cat_guard_node_rid`,
//! `cat_guard_edge_rowid`) fire for every update and refuse any change of the rowid; the edge
//! insert guard refuses a rowid that another identity holds (an edge's implicit rowid is a
//! uniqueness a REPLACE resolves by deleting).

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
         CREATE TEMP TABLE IF NOT EXISTS cat_replaced(
           src INTEGER, dst INTEGER, rel TEXT, producer TEXT
         );
         CREATE TEMP TRIGGER IF NOT EXISTS cat_guard_node_bi BEFORE INSERT ON main.node
             WHEN EXISTS (SELECT 1 FROM main.node WHERE rid = new.rid)
               OR EXISTS (SELECT 1 FROM main.node WHERE id = new.id)
               OR (new.lifecycle = 'active' AND EXISTS (
                     SELECT 1 FROM main.node
                     WHERE content_hash = new.content_hash AND lifecycle = 'active')) BEGIN
           SELECT RAISE(ABORT, 'catalog_capture_refused: a node insert would replace a row');
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_guard_node_rid BEFORE UPDATE ON main.node
             WHEN new.rid IS NOT old.rid BEGIN
           SELECT RAISE(ABORT, 'catalog_capture_refused: a node update would change its rid');
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_guard_node_bu
             BEFORE UPDATE OF id, content_hash, lifecycle ON main.node
             WHEN (new.id IS NOT old.id AND EXISTS (SELECT 1 FROM main.node WHERE id = new.id))
               OR (new.lifecycle = 'active' AND EXISTS (
                     SELECT 1 FROM main.node
                     WHERE content_hash = new.content_hash AND lifecycle = 'active'
                       AND rid <> old.rid)) BEGIN
           SELECT RAISE(ABORT, 'catalog_capture_refused: a node update would replace a row');
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_guard_edge_bi BEFORE INSERT ON main.edge BEGIN
           SELECT RAISE(ABORT, 'catalog_capture_refused: an edge insert would replace another edge')
             WHERE EXISTS (SELECT 1 FROM main.edge WHERE (id = new.id OR rowid = new.rowid)
                           AND NOT (src = new.src AND dst = new.dst AND rel = new.rel));
           DELETE FROM temp.cat_replaced;
           INSERT INTO temp.cat_replaced(src, dst, rel, producer)
             SELECT src, dst, rel, producer FROM main.edge
             WHERE src = new.src AND dst = new.dst AND rel = new.rel;
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_guard_edge_rowid BEFORE UPDATE ON main.edge
             WHEN new.rowid IS NOT old.rowid BEGIN
           SELECT RAISE(ABORT, 'catalog_capture_refused: an edge update would change its rowid');
         END;
         CREATE TEMP TRIGGER IF NOT EXISTS cat_guard_edge_bu
             BEFORE UPDATE OF id, src, dst, rel ON main.edge
             WHEN EXISTS (SELECT 1 FROM main.edge WHERE rowid <> old.rowid
                          AND (id = new.id OR (src = new.src AND dst = new.dst AND rel = new.rel)))
             BEGIN
           SELECT RAISE(ABORT, 'catalog_capture_refused: an edge update would replace another edge');
         END;
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
           INSERT INTO cat_capture(entity, op, src, dst, rel, b_exists, b_producer, a_exists)
             SELECT 'edge', 'delete', src, dst, rel, 1, producer, 0 FROM temp.cat_replaced;
           DELETE FROM temp.cat_replaced;
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

/// Discard whatever the queue holds: the end of every drain. Every row write of the writer
/// connection runs in [`super::Catalog::commit`], whose rollback also empties the queue, so a
/// commit's set holds that commit's writes only.
///
/// # Errors
/// SQLite errors.
pub(crate) fn reset(conn: &Connection) -> Result<()> {
    conn.execute_batch("DELETE FROM temp.cat_capture; DELETE FROM temp.cat_replaced;")
        .sql()
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
             CREATE TABLE node(rid INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE, kind TEXT, text TEXT,
               summary TEXT, tags TEXT, confidence REAL, importance REAL, status TEXT,
               created_at TEXT, updated_at TEXT, valid_from TEXT, valid_to TEXT,
               source_session TEXT, agent_id TEXT, workspace_id TEXT, props TEXT,
               content_hash TEXT, lifecycle TEXT, store_scope TEXT, path_scope TEXT, rev INTEGER,
               dup_key TEXT, repo TEXT, commit_sha TEXT, session_id TEXT, writer_id INTEGER);
             CREATE TABLE edge(id TEXT PRIMARY KEY,
               src INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
               dst INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
               rel TEXT NOT NULL, confidence REAL, created_at TEXT, props TEXT,
               workspace_id TEXT, producer TEXT, method TEXT, score REAL, source_id TEXT,
               offset INTEGER, len INTEGER, writer_id INTEGER, UNIQUE(src, dst, rel));",
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
            "UPDATE node SET writer_id = 7 WHERE rid = 1;
             UPDATE edge SET writer_id = 7 WHERE id = 'a';
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

    /// Two nodes and the edge `a` (1 -> 2, `authored`), the queue empty.
    fn two_nodes_one_edge() -> Connection {
        let conn = rows_db();
        conn.execute_batch(
            "INSERT INTO node(rid, id, kind, lifecycle)
               VALUES (1, 'n1', 'Fact', 'active'), (2, 'n2', 'Fact', 'active');
             INSERT INTO edge(id, src, dst, rel, producer) VALUES ('a', 1, 2, 'RELATES_TO', 'authored');",
        )
        .unwrap();
        reset(&conn).unwrap();
        conn
    }

    #[test]
    fn writes_the_triggers_cannot_see_are_refused() {
        for sql in [
            // a replaced node row would vanish without a delete image
            "INSERT OR REPLACE INTO node(rid, id, kind, lifecycle) VALUES (1, 'n9', 'Fact', 'active')",
            "INSERT OR REPLACE INTO node(rid, id, kind, lifecycle) VALUES (7, 'n1', 'Fact', 'active')",
            "UPDATE OR REPLACE node SET id = 'n2' WHERE rid = 1",
            // a node's identity is its rid
            "UPDATE node SET rid = 9 WHERE rid = 1",
            // an edge row replaced by id under another identity
            "INSERT OR REPLACE INTO edge(id, src, dst, rel, producer) VALUES ('a', 2, 1, 'RELATES_TO', 'rules')",
            "UPDATE OR REPLACE edge SET id = 'b' WHERE id = 'c'",
        ] {
            // Arrange
            let conn = two_nodes_one_edge();
            conn.execute(
                "INSERT INTO edge(id, src, dst, rel, producer) VALUES ('c', 2, 1, 'RELATES_TO', 'x')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO edge(id, src, dst, rel, producer) VALUES ('b', 1, 1, 'RELATES_TO', 'x')",
                [],
            )
            .unwrap();
            let before: i64 = conn
                .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
                .unwrap();
            reset(&conn).unwrap();

            // Act
            let out = conn.execute(sql, []);

            // Assert: refused, nothing changed, nothing queued
            assert!(out.is_err(), "{sql} was admitted");
            let after: i64 = conn
                .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
                .unwrap();
            assert_eq!(after, before, "{sql}");
            assert!(drain(&conn).unwrap().is_empty(), "{sql}");
        }
    }

    /// `sql` on `conn` is refused by a capture guard: nothing changed, nothing queued.
    fn assert_guard_refuses(conn: &Connection, sql: &str) {
        let snapshot = |conn: &Connection| -> Vec<(i64, String)> {
            let mut stmt = conn
                .prepare(
                    "SELECT rid, id FROM node UNION ALL SELECT rowid, id || ':' || src || '>' || dst
                     FROM edge ORDER BY 1, 2",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(std::result::Result::unwrap)
                .collect()
        };
        let before = snapshot(conn);
        reset(conn).unwrap();

        let out = conn.execute(sql, []);

        let err = out.expect_err(&format!("{sql} was admitted"));
        assert!(
            err.to_string().contains("catalog_capture_refused"),
            "{sql}: refused by something else: {err}"
        );
        assert_eq!(snapshot(conn), before, "{sql}");
        assert!(drain(conn).unwrap().is_empty(), "{sql}");
    }

    #[test]
    fn a_node_identity_change_is_refused_however_the_statement_spells_the_rowid() {
        for column in ["rid", "rowid", "_rowid_", "oid", "ROWID", "\"rid\""] {
            for sql in [
                format!("UPDATE node SET {column} = 9 WHERE rid = 3"),
                format!("UPDATE node SET {column} = {column} + 6, kind = 'Idea' WHERE rid = 3"),
                format!("UPDATE OR REPLACE node SET {column} = 1 WHERE rid = 3"),
            ] {
                // Arrange: node 3 has no edge, so no foreign key refuses the change first
                let conn = two_nodes_one_edge();
                conn.execute(
                    "INSERT INTO node(rid, id, kind, lifecycle) VALUES (3, 'n3', 'Fact', 'active')",
                    [],
                )
                .unwrap();

                // Act, Assert
                assert_guard_refuses(&conn, &sql);
            }
        }
    }

    #[test]
    fn an_edge_write_that_would_replace_another_edge_by_its_rowid_is_refused() {
        for column in ["rowid", "_rowid_", "oid"] {
            for sql in [
                // a new identity at the rowid of edge `a`: REPLACE would delete `a` unseen
                format!(
                    "INSERT OR REPLACE INTO edge({column}, id, src, dst, rel, producer)
                     VALUES ((SELECT rowid FROM edge WHERE id = 'a'), 'q', 2, 1, 'OTHER', 'x')"
                ),
                format!(
                    "REPLACE INTO edge({column}, id, src, dst, rel, producer)
                     VALUES ((SELECT rowid FROM edge WHERE id = 'a'), 'q', 2, 2, 'OTHER', 'x')"
                ),
                // edge `c` moved onto the rowid of edge `a`
                format!(
                    "UPDATE OR REPLACE edge SET {column} = (SELECT rowid FROM edge WHERE id = 'a')
                     WHERE id = 'c'"
                ),
                format!("UPDATE edge SET {column} = 77 WHERE id = 'c'"),
            ] {
                // Arrange
                let conn = two_nodes_one_edge();
                conn.execute(
                    "INSERT INTO edge(id, src, dst, rel, producer) VALUES ('c', 2, 1, 'RELATES_TO', 'x')",
                    [],
                )
                .unwrap();

                // Act, Assert
                assert_guard_refuses(&conn, &sql);
            }
        }
    }

    #[test]
    fn an_edge_first_writer_no_op_and_a_stamp_are_admitted_and_capture_nothing() {
        // Arrange
        let conn = two_nodes_one_edge();

        // Act: the first writer's insert-or-ignore of an existing identity, then a stamp
        conn.execute_batch(
            "INSERT INTO edge(id, src, dst, rel, producer) VALUES ('z', 1, 2, 'RELATES_TO', 'rules')
               ON CONFLICT DO NOTHING;
             INSERT OR IGNORE INTO edge(id, src, dst, rel, producer)
               VALUES ('a', 1, 2, 'RELATES_TO', 'rules');
             UPDATE edge SET writer_id = 7 WHERE id = 'a';
             UPDATE node SET writer_id = 7 WHERE rid = 1;",
        )
        .unwrap();
        let changes = drain(&conn).unwrap();

        // Assert
        assert!(changes.is_empty(), "{changes:?}");
        let producer: String = conn
            .query_row("SELECT producer FROM edge WHERE id = 'a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(producer, "authored");
    }

    #[test]
    fn a_replaced_edge_identity_is_captured_as_a_delete_and_an_insert() {
        // Arrange
        let conn = two_nodes_one_edge();

        // Act: the same identity under another id and producer
        conn.execute(
            "INSERT OR REPLACE INTO edge(id, src, dst, rel, producer) VALUES ('z', 1, 2, 'RELATES_TO', 'rules')",
            [],
        )
        .unwrap();
        let changes = drain(&conn).unwrap();

        // Assert: the replaced row's last values, then the new row
        let sides: Vec<_> = changes
            .edges()
            .iter()
            .map(|e| (e.op, e.before.clone(), e.after.clone()))
            .collect();
        let producer = |p: &str| {
            Some(EdgeImage {
                producer: Some(p.into()),
            })
        };
        assert_eq!(
            sides,
            [
                (CaptureOp::Delete, producer("authored"), None),
                (CaptureOp::Insert, None, producer("rules")),
            ]
        );
    }

    #[test]
    fn an_edge_upsert_is_captured_as_one_update() {
        // Arrange
        let conn = two_nodes_one_edge();

        // Act: the store's one edge writer shape
        conn.execute(
            "INSERT INTO edge(id, src, dst, rel, producer) VALUES ('z', 1, 2, 'RELATES_TO', 'rules')
             ON CONFLICT(src, dst, rel) DO UPDATE SET producer = excluded.producer",
            [],
        )
        .unwrap();
        let changes = drain(&conn).unwrap();

        // Assert
        let ops: Vec<_> = changes.edges().iter().map(|e| e.op).collect();
        assert_eq!(ops, [CaptureOp::Update]);
        assert!(drain(&conn).unwrap().is_empty());
    }
}
