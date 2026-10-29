//! Structural edge queries (stage1-spec §4.3, §6): `implementers` over `IMPLEMENTS`, `members`
//! over `CONTAINS`, `file_members` for the top level of a file (G3: a file node has no
//! `CONTAINS` children of its own; Go's containment parents are packages, Rust's modules),
//! `tests_for` over `TESTED_BY` in both forms, `tests_via` one `REFERENCES` hop before it,
//! `tests_textual` over paths and snippets (TESTS-1).

use std::fmt::Write as _;

use rusqlite::types::ToSql;
use rusqlite::{Connection, params};

use super::{RANGE_ORDER, placeholders, select_nodes, select_nodes_ordered};
use crate::error::ReadError;
use crate::reader::{How, MatchedBy, NodeRow, TestLink, TestsTarget};
use crate::schema::{EdgeType, FileId, NodeId};

/// `SELECT s.* FROM edges e JOIN symbols s ON s.id = e.src WHERE e.dst = ? AND e.type =
/// 'IMPLEMENTS' [AND s.external = 0] ORDER BY (path, ref)`.
pub(super) fn implementers(
    conn: &Connection,
    id: NodeId,
    include_external: bool,
) -> Result<Vec<NodeRow>, ReadError> {
    let predicate = if include_external {
        "e.dst = ?1 AND e.type = ?2"
    } else {
        "e.dst = ?1 AND e.type = ?2 AND s.external = 0"
    };
    select_nodes(
        conn,
        "JOIN edges e ON e.src = s.id",
        predicate,
        params![id.0, EdgeType::Implements.as_str()],
    )
}

/// Direct `CONTAINS` children of `id`, restricted to `labels` when any are given.
pub(super) fn members(
    conn: &Connection,
    id: NodeId,
    labels: &[&str],
) -> Result<Vec<NodeRow>, ReadError> {
    let contains = EdgeType::Contains.as_str();
    let mut params: Vec<&dyn ToSql> = vec![&id.0, &contains];
    let mut predicate = String::from("e.src = ?1 AND e.type = ?2");
    if !labels.is_empty() {
        let _ = write!(
            predicate,
            " AND s.label IN ({})",
            placeholders(3, labels.len())
        );
        params.extend(labels.iter().map(|l| l as &dyn ToSql));
    }
    select_nodes(conn, "JOIN edges e ON e.dst = s.id", &predicate, &params)
}

/// The top-level symbols of `file`: `kind = 'symbol'` rows defined in it that no other symbol
/// of the same file hides, restricted to `labels` when any are given, in definition range
/// order. A symbol hides what lies in its body: inside its enclosing range but outside its
/// own definition range (full positions, half-open), whatever the `CONTAINS` edges say
/// (q1-1: after R-2 an inherent method is a child of its type, whose item range does not
/// enclose the separate `impl`; the module `mod m { struct T; impl T { fn f() {} } }` encloses
/// `f` all the same). The module the Rust producer records for a whole file has both ranges
/// equal to the file and hides nothing. A `CONTAINS` parent in the file without a recorded body hides its children unless
/// it is a package, module or namespace (a Go struct hides its fields and methods; the
/// `package` clause scip-go defines in one file of the package hides nothing). A parent in
/// another file never hides. Locals and the file node are never members.
pub(super) fn file_members(
    conn: &Connection,
    file: FileId,
    labels: &[&str],
) -> Result<Vec<NodeRow>, ReadError> {
    let contains = EdgeType::Contains.as_str();
    let mut params: Vec<&dyn ToSql> = vec![&file.0, &contains];
    let mut predicate = format!(
        "s.file_id = ?1 AND s.kind = 'symbol' AND NOT EXISTS (\
             SELECT 1 FROM symbols p \
             WHERE p.file_id = s.file_id AND p.kind = 'symbol' AND p.enc_sl IS NOT NULL \
             AND {} AND NOT {}) \
         AND NOT EXISTS (\
             SELECT 1 FROM edges e JOIN symbols p ON p.id = e.src \
             WHERE e.dst = s.id AND e.type = ?2 AND p.kind = 'symbol' AND p.file_id = s.file_id \
             AND p.enc_sl IS NULL AND p.label NOT IN ('Package', 'Module', 'Namespace'))",
        encloses("p.enc", "s.def"),
        encloses("p.def", "s.def"),
    );
    if !labels.is_empty() {
        let _ = write!(
            predicate,
            " AND s.label IN ({})",
            placeholders(3, labels.len())
        );
        params.extend(labels.iter().map(|l| l as &dyn ToSql));
    }
    select_nodes_ordered(conn, "", &predicate, RANGE_ORDER, &params)
}

/// SQL: the range in the `outer` columns (`<outer>_sl`, `_sc`, `_el`, `_ec`) encloses the one
/// in the `inner` columns: start at or before the inner start and end at or after the inner
/// end, (line, column) compared in order. SCIP ranges are half-open, so a range that starts
/// where another ends lies outside it (q1-2: `struct S; impl S { fn f() {} }` on one line
/// has the members of its two-line form).
fn encloses(outer: &str, inner: &str) -> String {
    format!(
        "(({outer}_sl < {inner}_sl OR ({outer}_sl = {inner}_sl AND {outer}_sc <= {inner}_sc)) \
          AND ({inner}_el < {outer}_el OR ({inner}_el = {outer}_el AND {inner}_ec <= {outer}_ec)))"
    )
}

/// Reference form for a symbol (`matched_by: reference`), naming form for a file
/// (`matched_by: naming`, heuristic), each the complete `TESTED_BY` target set.
pub(super) fn tests_for(
    conn: &Connection,
    target: TestsTarget,
) -> Result<Vec<TestLink>, ReadError> {
    let tested_by = EdgeType::TestedBy.as_str();
    let (rows, matched_by) = match target {
        TestsTarget::Symbol(id) => (
            select_nodes(
                conn,
                "JOIN edges e ON e.dst = s.id",
                "e.src = ?1 AND e.type = ?2",
                params![id.0, tested_by],
            )?,
            MatchedBy::Reference,
        ),
        TestsTarget::File(file) => (
            select_nodes(
                conn,
                "JOIN edges e ON e.dst = s.id",
                "e.src = (SELECT id FROM symbols WHERE kind = 'file' AND file_id = ?1) AND e.type = ?2",
                params![file.0, tested_by],
            )?,
            MatchedBy::Naming,
        ),
    };
    Ok(rows
        .into_iter()
        .map(|test| TestLink {
            test,
            matched_by,
            heuristic: matched_by == MatchedBy::Naming,
            how: How::Direct,
            via: None,
        })
        .collect())
}

/// SQL: the row is a function or method (a caller, a test function), not a module that
/// re-exports the target, a struct whose field names it or a test-file struct (TESTS-1).
const CALLABLE: &str = "s.label IN ('Function', 'Method')";

/// The `via` tier: the callers of `id` (`REFERENCES` sources that are non-test, non-external
/// functions or methods, in `(path, ref)` order, the first `intermediates` of them), each one's
/// reference form `TESTED_BY` targets tagged with the caller.
pub(super) fn tests_via(
    conn: &Connection,
    id: NodeId,
    intermediates: usize,
) -> Result<Vec<TestLink>, ReadError> {
    let callers = select_nodes(
        conn,
        "JOIN edges e ON e.src = s.id",
        &format!(
            "e.dst = ?1 AND e.type = ?2 AND s.kind = 'symbol' AND s.is_test = 0 \
             AND s.external = 0 AND {CALLABLE}"
        ),
        params![id.0, EdgeType::References.as_str()],
    )?;
    let mut links = Vec::new();
    for caller in callers.into_iter().take(intermediates) {
        for mut link in tests_for(conn, TestsTarget::Symbol(caller.id))? {
            link.how = How::Via;
            link.via = Some(caller.clone());
            links.push(link);
        }
    }
    Ok(links)
}

/// The `textual` tier: test functions and methods (`kind = 'symbol'`, `is_test = 1`) whose
/// path or stored snippet contain every word, ASCII case-insensitively, in `(path, ref)` order.
pub(super) fn tests_textual(conn: &Connection, words: &[&str]) -> Result<Vec<TestLink>, ReadError> {
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let lowered: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let mut predicate = format!("s.kind = 'symbol' AND s.is_test = 1 AND {CALLABLE}");
    for i in 1..=lowered.len() {
        let _ = write!(
            predicate,
            " AND instr(lower(COALESCE(f.path, '') || ' ' || COALESCE(sn.text, '')), ?{i}) > 0"
        );
    }
    let params: Vec<&dyn ToSql> = lowered.iter().map(|w| w as &dyn ToSql).collect();
    Ok(select_nodes(conn, "", &predicate, &params)?
        .into_iter()
        .map(|test| TestLink {
            test,
            matched_by: MatchedBy::Naming,
            heuristic: true,
            how: How::Textual,
            via: None,
        })
        .collect())
}
