//! Toy table functions shared by the integration tests.
// Each test binary uses a subset of these helpers.
#![allow(dead_code)]

use std::sync::Arc;

use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{
    Arg, ColumnDef, ColumnKind, FnError, OrderTerm, Plan, PlanInput, Rows, TableFunction, Value,
};

/// `range(start, stop)`: the integers `start..stop`, ascending, in column `value`.
pub(crate) struct Range;

const RANGE_COLUMNS: &[ColumnDef] = &[
    ColumnDef {
        name: "value",
        kind: ColumnKind::Integer,
        argument: None,
    },
    ColumnDef {
        name: "start",
        kind: ColumnKind::Integer,
        argument: Some(Arg { required: true }),
    },
    ColumnDef {
        name: "stop",
        kind: ColumnKind::Integer,
        argument: Some(Arg { required: true }),
    },
];

/// Rows assumed when a bound is only known at run time.
const UNKNOWN_SPAN: i64 = 1000;

fn known_integer(input: &PlanInput<'_>, name: &str) -> Option<i64> {
    input
        .bound
        .iter()
        .find(|arg| arg.name == name)
        .and_then(|arg| match arg.value {
            Some(Value::Integer(v)) => Some(v),
            _ => None,
        })
}

impl TableFunction for Range {
    fn name(&self) -> &'static str {
        "range"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        RANGE_COLUMNS
    }

    fn plan(&self, input: &PlanInput<'_>) -> Plan {
        let span = match (known_integer(input, "start"), known_integer(input, "stop")) {
            (Some(start), Some(stop)) => stop.saturating_sub(start).max(1),
            _ => UNKNOWN_SPAN,
        };
        let ascending_value = [OrderTerm {
            name: "value",
            desc: false,
        }];
        #[allow(clippy::cast_precision_loss)] // an estimate; precision does not matter
        let cost = span as f64;
        Plan {
            cost,
            rows: span,
            consumes_order: input.order_by == ascending_value,
            point_lookup: false,
        }
    }

    fn rows(&self, args: &[Value], _budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        let refuse = |reason: String| FnError {
            construct: "range".to_owned(),
            reason,
        };
        let (start, stop) = match args {
            [Value::Integer(start), Value::Integer(stop)] => (*start, *stop),
            other => {
                return Err(refuse(format!(
                    "start and stop must be integers, got {other:?}"
                )));
            }
        };
        if start > stop {
            return Err(refuse(format!("start {start} is after stop {stop}")));
        }
        Ok(Box::new((start..stop).map(|v| Ok(vec![Value::Integer(v)]))))
    }
}

/// `broken()`: one good row, then a row that fails.
pub(crate) struct Broken;

const BROKEN_COLUMNS: &[ColumnDef] = &[ColumnDef {
    name: "value",
    kind: ColumnKind::Text,
    argument: None,
}];

impl TableFunction for Broken {
    fn name(&self) -> &'static str {
        "broken"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        BROKEN_COLUMNS
    }

    fn plan(&self, _input: &PlanInput<'_>) -> Plan {
        Plan {
            cost: 2.0,
            rows: 2,
            consumes_order: false,
            point_lookup: false,
        }
    }

    fn rows(&self, _args: &[Value], _budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        let rows = [
            Ok(vec![Value::Text("first".to_owned())]),
            Err(FnError {
                construct: "broken".to_owned(),
                reason: "the second row is unreadable".to_owned(),
            }),
        ];
        Ok(Box::new(rows.into_iter()))
    }
}

/// An in-memory connection with `functions` registered.
pub(crate) fn connection(functions: Vec<Arc<dyn TableFunction>>) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for f in functions {
        kaban_vtab::register(&conn, f).unwrap();
    }
    conn
}

/// Run `sql` and collect its first column as integers, or the error text.
pub(crate) fn integers(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<i64>, String> {
    let collect = || -> rusqlite::Result<Vec<i64>> {
        let mut stmt = conn.prepare(sql)?;
        stmt.query_map([], |row| row.get(0))?.collect()
    };
    collect().map_err(|e| e.to_string())
}

/// `EXPLAIN QUERY PLAN` detail lines for `sql`, in plan order, or the error text.
pub(crate) fn query_plan(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<String>, String> {
    let collect = || -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        stmt.query_map([], |row| row.get(3))?.collect()
    };
    collect().map_err(|e| e.to_string())
}

/// Charge this thread's SQLite work and table-function scans to an unbounded budget until the
/// guard drops: a table function scans only inside a request budget scope.
pub(crate) fn unbounded_scope() -> kaban_vtab::test_hooks::ScopeGuard {
    kaban_vtab::test_hooks::scope(&QueryBudget::new(u64::MAX, None))
}

/// `reserving(bytes)`: reserves `bytes` through the scan's budget for the life of the scan and
/// yields one row holding the budget's live bytes after the reservation.
pub(crate) struct Reserving;

const RESERVING_COLUMNS: &[ColumnDef] = &[
    ColumnDef {
        name: "live",
        kind: ColumnKind::Integer,
        argument: None,
    },
    ColumnDef {
        name: "bytes",
        kind: ColumnKind::Integer,
        argument: Some(Arg { required: true }),
    },
];

impl TableFunction for Reserving {
    fn name(&self) -> &'static str {
        "reserving"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        RESERVING_COLUMNS
    }

    fn plan(&self, _input: &PlanInput<'_>) -> Plan {
        Plan {
            cost: 1.0,
            rows: 1,
            consumes_order: false,
            point_lookup: true,
        }
    }

    fn rows(&self, args: &[Value], budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        let refuse = |reason: String| FnError {
            construct: "reserving".to_owned(),
            reason,
        };
        let bytes = match args {
            [Value::Integer(bytes)] => u64::try_from(*bytes).map_err(|e| refuse(e.to_string()))?,
            _ => return Err(refuse("bytes must be an integer".to_owned())),
        };
        let lease = budget.reserve(bytes).map_err(|e| refuse(e.to_string()))?;
        let live = i64::try_from(budget.live_bytes()).unwrap_or(i64::MAX);
        Ok(Box::new(
            std::iter::once(Ok(vec![Value::Integer(live)])).inspect(move |_| {
                let _held = &lease;
            }),
        ))
    }
}
