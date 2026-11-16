//! A registered table function answers SQL as an eponymous table (spec 11 T12 Step 1).
#![allow(clippy::unwrap_used, clippy::panic)] // the `panicky` toy panics on purpose

mod common;

use std::sync::Arc;

use common::{Broken, Range, connection, integers, unbounded_scope};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{
    Arg, ColumnDef, ColumnKind, FnError, Plan, PlanInput, Scan, Rows, TableFunction, Value,
};

#[test]
fn range_returns_its_integers() {
    let _scope = unbounded_scope();
    let conn = connection(vec![Arc::new(Range)]);
    assert_eq!(
        integers(&conn, "SELECT value FROM range(1, 4)"),
        Ok(vec![1, 2, 3])
    );
}

#[test]
fn argument_columns_are_hidden_from_select_star() {
    let _scope = unbounded_scope();
    let conn = connection(vec![Arc::new(Range)]);
    let columns = conn
        .prepare("SELECT * FROM range(1, 4)")
        .map(|stmt| stmt.column_names().collect::<Vec<_>>().join(","))
        .map_err(|e| e.to_string());
    assert_eq!(columns, Ok("value".to_owned()));
}

#[test]
fn a_missing_required_argument_is_refused_by_name() {
    let _scope = unbounded_scope();
    let conn = connection(vec![Arc::new(Range)]);
    let err = integers(&conn, "SELECT * FROM range").unwrap_err();
    assert!(
        err.contains("range") && err.contains("start"),
        "the error must name the function and the argument: {err}"
    );
}

#[test]
fn a_refusal_from_rows_carries_construct_and_reason() {
    let _scope = unbounded_scope();
    let conn = connection(vec![Arc::new(Range)]);
    let err = integers(&conn, "SELECT value FROM range(5, 1)").unwrap_err();
    assert!(
        err.contains("range: start 5 is after stop 1"),
        "the error must carry construct and reason: {err}"
    );
}

#[test]
fn a_failing_row_carries_construct_and_reason() {
    let _scope = unbounded_scope();
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

/// `panicky(mode)`: mode 0 panics in `rows`, mode 1 yields one row from an iterator whose `Drop`
/// panics.
struct Panicky;

const PANICKY_COLUMNS: &[ColumnDef] = &[
    ColumnDef {
        name: "value",
        kind: ColumnKind::Integer,
        argument: None,
    },
    ColumnDef {
        name: "mode",
        kind: ColumnKind::Integer,
        argument: Some(Arg { required: true }),
    },
];

struct PanicOnDrop(Option<i64>);

impl Iterator for PanicOnDrop {
    type Item = Result<Vec<Value>, FnError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.take().map(|v| Ok(vec![Value::Integer(v)]))
    }
}

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("row iterator dropped");
    }
}

impl TableFunction for Panicky {
    fn name(&self) -> &'static str {
        "panicky"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        PANICKY_COLUMNS
    }

    fn plan(&self, _input: &PlanInput<'_>) -> Plan {
        Plan {
            cost: 1.0,
            rows: 1,
            consumes_order: false,
            point_lookup: false,
            ..Plan::default()
        }
    }

    fn rows(&self, scan: &Scan<'_>, _budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        match scan.args {
            [Value::Integer(1)] => Ok(Box::new(PanicOnDrop(Some(7)))),
            _ => panic!("rows failed"),
        }
    }
}

#[test]
fn a_panic_in_rows_becomes_an_error_naming_the_function() {
    let _scope = unbounded_scope();
    let conn = connection(vec![Arc::new(Panicky)]);
    let err = integers(&conn, "SELECT value FROM panicky(0)").unwrap_err();
    assert!(err.contains("panicky: rows panicked"), "{err}");
}

#[test]
fn a_panic_while_dropping_rows_does_not_unwind_into_sqlite() {
    let _scope = unbounded_scope();
    let conn = connection(vec![Arc::new(Panicky)]);
    assert_eq!(integers(&conn, "SELECT value FROM panicky(1)"), Ok(vec![7]));
}
