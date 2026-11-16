//! A registered table function answers SQL as an eponymous table (spec 11 T12 Step 1).
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{Broken, Range, connection, integers};

#[test]
fn range_returns_its_integers() {
    let conn = connection(vec![Arc::new(Range)]);
    assert_eq!(
        integers(&conn, "SELECT value FROM range(1, 4)"),
        Ok(vec![1, 2, 3])
    );
}

#[test]
fn argument_columns_are_hidden_from_select_star() {
    let conn = connection(vec![Arc::new(Range)]);
    let columns = conn
        .prepare("SELECT * FROM range(1, 4)")
        .map(|stmt| stmt.column_names().collect::<Vec<_>>().join(","))
        .map_err(|e| e.to_string());
    assert_eq!(columns, Ok("value".to_owned()));
}

#[test]
fn a_missing_required_argument_is_refused_by_name() {
    let conn = connection(vec![Arc::new(Range)]);
    let err = integers(&conn, "SELECT * FROM range").unwrap_err();
    assert!(
        err.contains("range") && err.contains("start"),
        "the error must name the function and the argument: {err}"
    );
}

#[test]
fn a_refusal_from_rows_carries_construct_and_reason() {
    let conn = connection(vec![Arc::new(Range)]);
    let err = integers(&conn, "SELECT value FROM range(5, 1)").unwrap_err();
    assert!(
        err.contains("range: start 5 is after stop 1"),
        "the error must carry construct and reason: {err}"
    );
}

#[test]
fn a_failing_row_carries_construct_and_reason() {
    let conn = connection(vec![Arc::new(Broken)]);
    let err = conn
        .prepare("SELECT value FROM broken")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|e| e.to_string());
    assert!(
        err.as_ref()
            .is_err_and(|e| e.contains("broken: the second row is unreadable")),
        "the error must carry construct and reason: {err:?}"
    );
}
