//! `node_anchor` maintained on write (engine-spec EN-F10; engine-plan E2.2; CG-JOIN v1 §5 and
//! §11): the rows migration 0004 backfilled once (`migrate_0004`) are kept current by every
//! node write of `remember`, `apply_batch` and the M5 import, inside the writer's transaction.
//!
//! Derivation, the one the backfill uses (byte for byte, so an imported `CodeRef` and a
//! remembered one get identical rows):
//!
//! * a `CodeRef` gets one `path` row keyed `props.relative_path` and one `symbol` row keyed
//!   `props.symbol` (when present and non-empty; `name_path` is never a key), both with
//!   `repo = node.repo` and `snapshot_scope = props.snapshot_commit` when it is 40 hex
//!   (lowercased), `NULL` otherwise;
//! * a record of a kind that governs paths gets one `path` row per `path_scope` glob, keyed
//!   by the glob's literal prefix, or by the universal key `""` for a prefix-less glob
//!   (E1.7a RE2-2); `repo = node.repo`, no scope;
//! * no `id` rows: E1.7 derives none from the stored data either (an external document id
//!   seam arrives with its producer).
//!
//! The desired set is diffed against the stored rows: a changed `symbol` deletes the old row
//! and inserts the new one while an unchanged `path` row is left alone (SO-13
//! `anchor_update_rewrites_key`). A `tombstoned` or `purged` node has no anchor rows
//! (`remove`): a hidden pointer must never join v4's snapshot; an archived record keeps its
//! rows (reachable on request, as the backfill's `ANCHORED` set).

use crate::connection::CgRead;
use crate::rows::{NODE_COLS, row_node};
use crate::sql_error::SqlResultExt;
use kaban_core::{
    AnchorPredicates, GovernedSet, Node, Result, SnapshotSymbol, StoreError, SymbolPage,
    WorkspaceFilter,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::BTreeSet;

/// The `node_anchor.key` of a governed record whose glob has no literal prefix (RE2-2): every
/// path of the repository is a candidate. A consumer selecting anchors by prefix must include
/// the rows with this key.
pub(crate) const UNIVERSAL_PREFIX: &str = "";

/// `props.snapshot_commit` as a scope: 40 hex, lowercased; `None` when absent or not 40 hex
/// (the caller counts the second case).
pub(crate) fn scope_of(props: &serde_json::Value) -> (Option<String>, bool) {
    match props
        .get("snapshot_commit")
        .and_then(serde_json::Value::as_str)
    {
        None => (None, false),
        Some(s) if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
            (Some(s.to_ascii_lowercase()), false)
        }
        Some(_) => (None, true),
    }
}

/// The key of a governed `path` anchor: the literal prefix of a glob (the bytes before the
/// first metacharacter `*`, `?`, `[`, `{`), or [`UNIVERSAL_PREFIX`] when the glob starts with
/// one (the glob matches paths under every prefix).
pub(crate) fn literal_prefix(glob: &str) -> &str {
    let end = glob.find(['*', '?', '[', '{']).unwrap_or(glob.len());
    &glob[..end]
}

/// One desired or stored anchor row of a node: `(anchor_kind, key, repo, snapshot_scope)`.
type Row = (&'static str, String, Option<String>, Option<String>);

/// What a node write hands the maintenance: the columns the derivation reads.
pub(crate) struct NodeFacts<'a> {
    pub(crate) rid: i64,
    pub(crate) workspace_id: &'a str,
    pub(crate) kind: &'a str,
    pub(crate) repo: Option<&'a str>,
    /// The stored `props` JSON (object).
    pub(crate) props: &'a Value,
    /// The stored `path_scope` globs.
    pub(crate) path_scope: &'a [String],
}

/// The rows a node should have (sorted, deduplicated); `governs_paths` is the writer's answer
/// for `facts.kind` (`GovernedSet::governs_paths`).
fn derive(facts: &NodeFacts<'_>, governs_paths: bool) -> BTreeSet<Row> {
    let mut rows = BTreeSet::new();
    let repo = facts.repo.map(str::to_owned);
    if facts.kind == "CodeRef" {
        let (scope, _non40) = scope_of(facts.props);
        if let Some(path) = facts.props.get("relative_path").and_then(Value::as_str)
            && !path.is_empty()
        {
            rows.insert(("path", path.to_owned(), repo.clone(), scope.clone()));
        }
        if let Some(symbol) = facts.props.get("symbol").and_then(Value::as_str)
            && !symbol.is_empty()
        {
            rows.insert(("symbol", symbol.to_owned(), repo.clone(), scope));
        }
    }
    if governs_paths {
        for glob in facts.path_scope {
            let prefix = literal_prefix(glob);
            rows.insert(("path", prefix.to_owned(), repo.clone(), None));
        }
    }
    rows
}

/// The stored rows of `rid`.
fn stored(conn: &Connection, rid: i64) -> Result<BTreeSet<Row>> {
    let mut st = conn
        .prepare_cached(
            "SELECT anchor_kind, key, repo, snapshot_scope FROM node_anchor WHERE rid = ?1",
        )
        .sql()?;
    let rows = st
        .query_map([rid], |r| {
            let kind: String = r.get(0)?;
            Ok((
                match kind.as_str() {
                    "symbol" => "symbol",
                    "id" => "id",
                    _ => "path",
                },
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .sql()?
        .collect::<rusqlite::Result<BTreeSet<Row>>>()
        .sql()?;
    Ok(rows)
}

/// Bring the node's anchor rows to the derived set inside the caller's transaction. Returns
/// how many rows were inserted and deleted.
pub(crate) fn maintain(
    conn: &Connection,
    facts: &NodeFacts<'_>,
    governed: &GovernedSet,
) -> Result<(usize, usize)> {
    maintain_rows(conn, facts, governed.governs_paths(facts.kind))
}

fn maintain_rows(
    conn: &Connection,
    facts: &NodeFacts<'_>,
    governs_paths: bool,
) -> Result<(usize, usize)> {
    let want = derive(facts, governs_paths);
    let have = stored(conn, facts.rid)?;
    let mut deleted = 0;
    for (kind, key, ..) in have.difference(&want) {
        deleted += conn
            .execute(
                "DELETE FROM node_anchor WHERE rid = ?1 AND anchor_kind = ?2 AND key = ?3",
                params![facts.rid, kind, key],
            )
            .sql()?;
    }
    let mut inserted = 0;
    for (kind, key, repo, scope) in want.difference(&have) {
        inserted += conn
            .execute(
                "INSERT OR REPLACE INTO node_anchor(rid, workspace_id, anchor_kind, key, repo, snapshot_scope)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![facts.rid, facts.workspace_id, kind, key, repo, scope],
            )
            .sql()?;
    }
    Ok((inserted, deleted))
}

/// Re-derive the anchors of a stored node from its row (the reactivation and import paths,
/// where the caller holds no [`NodeFacts`]).
pub(crate) fn refresh(conn: &Connection, rid: i64, governed: &GovernedSet) -> Result<()> {
    refresh_with(conn, rid, &|kind| governed.governs_paths(kind))
}

/// Re-derive the anchors of a node whose strings a policy rescan rewrote (E2.7b; EN-F10 "every
/// node write"). The rescan runs without a `WriteCtx` and the store's `reconcile::Setup` keeps
/// its ontology to itself, so the governed answer is read off the row's own anchor rows: a
/// redaction rewrites strings and never a kind, so a record that had a `path` row at its write
/// governs paths still, and one without had none to follow (a redaction adds no glob).
pub(crate) fn rekey(conn: &Connection, rid: i64) -> Result<()> {
    let had_path_rows = stored(conn, rid)?.iter().any(|(kind, ..)| *kind == "path");
    refresh_with(conn, rid, &|kind| kind != "CodeRef" && had_path_rows)
}

fn refresh_with(conn: &Connection, rid: i64, governs_paths: &dyn Fn(&str) -> bool) -> Result<()> {
    let row: Option<(String, String, Option<String>, String, String)> = conn
        .query_row(
            "SELECT workspace_id, kind, repo, props, path_scope FROM node WHERE rid = ?1 AND lifecycle IN ('active', 'archived')",
            [rid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .sql()?;
    let Some((ws, kind, repo, props, path_scope)) = row else {
        return remove(conn, rid);
    };
    let props: Value = serde_json::from_str(&props)?;
    let path_scope: Vec<String> = serde_json::from_str(&path_scope)?;
    maintain_rows(
        conn,
        &NodeFacts {
            rid,
            workspace_id: &ws,
            kind: &kind,
            repo: repo.as_deref(),
            props: &props,
            path_scope: &path_scope,
        },
        governs_paths(&kind),
    )?;
    Ok(())
}

/// Delete every anchor row of a node (tombstone and purge).
pub(crate) fn remove(conn: &Connection, rid: i64) -> Result<()> {
    conn.execute("DELETE FROM node_anchor WHERE rid = ?1", [rid])
        .sql()?;
    Ok(())
}

/// Whether the universal key names every path (kept here so a consumer reads one constant).
#[allow(dead_code)] // E2.7's prefilter reads it; the derivation above writes it
pub(crate) const UNIVERSAL: &str = UNIVERSAL_PREFIX;

/// `MemoryStore::coderefs_by_symbol` (CG-JOIN v1 §3, §7; E2.6): the active `CodeRef` rows
/// whose `symbol` anchor equals `key` under the predicate set, ordered by row id. Runs on the
/// main connection: `node_anchor` is the memory database's, no `cg` here.
pub(crate) fn coderefs_by_symbol(
    conn: &Connection,
    p: &AnchorPredicates<'_>,
    key: &str,
) -> Result<Vec<Node>> {
    let cols = NODE_COLS
        .split(',')
        .map(|c| format!("c.{c}"))
        .collect::<Vec<_>>()
        .join(",");
    // the key is TEXT `=` under the default BINARY collation: byte equality, no normalisation
    // (SD-6); with no resolved snapshot the scope predicate is dropped and every scope matches
    // (cg-join §3; OWN-83 (1): the latest decision rather than nothing, the card signals the
    // missing index with `code: snapshot_absent` and `anchor_commit`); the placeholders are
    // sequential so the parameter list matches the clauses present
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&key, &p.repo];
    let scope = match &p.commit {
        Some(commit) => {
            params.push(commit);
            " AND (a.snapshot_scope IS NULL OR a.snapshot_scope = ?)"
        }
        None => "",
    };
    let workspace = match p.workspace {
        WorkspaceFilter::Only(ws) => {
            params.push(ws);
            " AND a.workspace_id = ?"
        }
        WorkspaceFilter::All => "",
    };
    // `CROSS JOIN` pins the join order (SQLite honours it): the planner drives from
    // `node_anchor_key(anchor_kind, key, workspace_id)` (0004:111) and reaches each `node` row
    // by rowid. A plain `JOIN` on a store without `sqlite_stat1` (the store never runs
    // `ANALYZE`) was planned from `node_kind` and read every active `CodeRef` (G1-F10, the
    // CG5.3 stop; E2.6c). Same rows, same order. `tests/anchors_v4.rs` `RECALL_REF_SHAPE`
    // mirrors this statement after the `SELECT` list (the `commit: None`, `Only(ws)` case: no
    // scope clause) for `recall_ref_uses_index`; a change here changes both.
    let sql = format!(
        "SELECT {cols} FROM node_anchor a CROSS JOIN node c ON c.rid = a.rid \
         WHERE a.anchor_kind = 'symbol' AND a.key = ? AND a.repo = ?{scope}{workspace} \
           AND c.kind = 'CodeRef' AND c.lifecycle = 'active' \
         ORDER BY c.rid"
    );
    let mut st = conn.prepare_cached(&sql).sql()?;
    let rows = st
        .query_map(params.as_slice(), row_node)
        .sql()?
        .collect::<rusqlite::Result<Vec<Node>>>()
        .sql()?;
    Ok(rows)
}

/// `MemoryStore::snapshot_symbols` (CG-JOIN v1 §7, SO-10; E2.6) on the dedicated read
/// connection: one page of `cg.symbols` (`kind = 'symbol' AND external = 0`) joined with
/// `cg.files(path)`, by id after `page.after_id`; the next `after_id` when the page was full.
/// Only the stable columns of §8 are read. The statement is prepared uncached so it is
/// finalised before any later `DETACH`.
pub(crate) fn snapshot_symbols(
    cg: &CgRead,
    commit: &str,
    page: SymbolPage,
) -> Result<(Vec<SnapshotSymbol>, Option<i64>)> {
    if cg.attached_commit() != Some(commit) {
        return Err(StoreError::SnapshotMismatch);
    }
    if page.limit == 0 {
        return Ok((Vec::new(), None));
    }
    let mut st = cg
        .conn()
        .prepare(
            "SELECT s.id, s.key, s.name, s.label, f.path, s.ident_terms \
             FROM cg.symbols s JOIN cg.files f ON f.id = s.file_id \
             WHERE s.kind = 'symbol' AND s.external = 0 AND s.id > ?1 \
             ORDER BY s.id LIMIT ?2",
        )
        .sql()?;
    let rows = st
        .query_map(params![page.after_id, i64::from(page.limit)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                SnapshotSymbol {
                    key: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    name: r.get(2)?,
                    label: r.get(3)?,
                    path: r.get(4)?,
                    ident_terms: r.get(5)?,
                },
            ))
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    let full = rows.len() == page.limit as usize;
    let next = if full {
        rows.last().map(|(id, _)| *id)
    } else {
        None
    };
    Ok((rows.into_iter().map(|(_, s)| s).collect(), next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_coderef_derives_its_path_and_symbol_rows_with_the_same_scope() {
        let props = json!({"relative_path": "src/a.rs", "symbol": "a::f", "name_path": "f",
            "snapshot_commit": "0123456789ABCDEF0123456789abcdef01234567"});
        let facts = NodeFacts {
            rid: 1,
            workspace_id: "default",
            kind: "CodeRef",
            repo: Some("repo"),
            props: &props,
            path_scope: &[],
        };
        let rows: Vec<Row> = derive(&facts, GovernedSet::shipped().governs_paths("CodeRef"))
            .into_iter()
            .collect();
        let scope = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
        assert_eq!(
            rows,
            vec![
                (
                    "path",
                    "src/a.rs".into(),
                    Some("repo".into()),
                    scope.clone()
                ),
                ("symbol", "a::f".into(), Some("repo".into()), scope),
            ]
        );
    }

    #[test]
    fn a_scoped_decision_derives_prefix_rows_and_the_universal_row() {
        let props = json!({});
        let scope = vec!["src/api/**".to_owned(), "**/*.rs".to_owned()];
        let facts = NodeFacts {
            rid: 2,
            workspace_id: "default",
            kind: "Decision",
            repo: None,
            props: &props,
            path_scope: &scope,
        };
        let rows: Vec<Row> = derive(&facts, GovernedSet::shipped().governs_paths("Decision"))
            .into_iter()
            .collect();
        assert_eq!(
            rows,
            vec![
                ("path", String::new(), None, None),
                ("path", "src/api/".into(), None, None),
            ]
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod anchor_key_tests {
    use super::{UNIVERSAL_PREFIX, literal_prefix, scope_of};

    #[test]
    fn the_literal_prefix_stops_at_the_first_metacharacter() {
        assert_eq!(literal_prefix("src/api/**"), "src/api/");
        assert_eq!(literal_prefix("docs/adr-1.md"), "docs/adr-1.md");
        assert_eq!(literal_prefix("**/*.rs"), UNIVERSAL_PREFIX);
        assert_eq!(literal_prefix("*"), UNIVERSAL_PREFIX);
        assert_eq!(literal_prefix("{a,b}/x"), UNIVERSAL_PREFIX);
        assert_eq!(literal_prefix("src/{a,b}/x"), "src/");
        assert_eq!(literal_prefix("src/?.rs"), "src/");
        assert_eq!(literal_prefix("src/[ab].rs"), "src/");
    }

    #[test]
    fn a_scope_is_forty_lowercase_hex_or_dropped_and_counted() {
        let forty = "0123456789ABCDEF0123456789abcdef01234567";
        assert_eq!(
            scope_of(&serde_json::json!({ "snapshot_commit": forty })),
            (Some(forty.to_ascii_lowercase()), false)
        );
        assert_eq!(
            scope_of(&serde_json::json!({ "snapshot_commit": "abc1234" })),
            (None, true)
        );
        assert_eq!(scope_of(&serde_json::json!({})), (None, false));
    }
}
