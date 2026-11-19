//! Usable constraints on a table function's result columns reach its plan, and the ones the plan
//! consumes reach its scan with the chosen strategy, so a bound result id becomes a point lookup
//! (spec 11 T12, QRY-08, T14 Step 5, MAJ-05). Each case runs in a fresh process with the
//! allocator adapter installed.
#![allow(clippy::unwrap_used)]

mod common;
mod fresh;

use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::query_plan;
use fresh::{fresh_test, with_query};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{
    Arg, ColumnDef, ColumnKind, ConstraintOp, FnError, Plan, PlanInput, Rows, Scan, TableFunction,
    Value,
};

/// `squares(n)`: rows `(id, square)` for `id` in `1..=n`. It takes a point lookup when `id = v`
/// is bound, and a sloppy range scan (starting a few ids early) for `id > v`, which it consumes
/// but does not omit, so SQLite must still check it.
#[derive(Default)]
struct Squares {
    produced: Arc<AtomicUsize>,
    planned_id: Arc<AtomicI64>,
    strategies: Arc<Mutex<Vec<i32>>>,
}

const SQUARES_COLUMNS: &[ColumnDef] = &[
    ColumnDef {
        name: "id",
        kind: ColumnKind::Integer,
        argument: None,
    },
    ColumnDef {
        name: "square",
        kind: ColumnKind::Integer,
        argument: None,
    },
    ColumnDef {
        name: "n",
        kind: ColumnKind::Integer,
        argument: Some(Arg { required: true }),
    },
];

const POINT: i32 = 1;
const FROM: i32 = 2;

impl TableFunction for Squares {
    fn name(&self) -> &'static str {
        "squares"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        SQUARES_COLUMNS
    }

    fn plan(&self, input: &PlanInput<'_>) -> Plan {
        let on_id = |op| {
            input
                .constraints
                .iter()
                .position(|c| c.column == "id" && c.op == op)
        };
        if let Some(i) = on_id(ConstraintOp::Eq) {
            if let Some(Value::Integer(id)) = input.constraints[i].value {
                self.planned_id.store(id, Ordering::SeqCst);
            }
            return Plan {
                cost: 1.0,
                rows: 1,
                point_lookup: true,
                consumes: 1 << i,
                omits: 1 << i,
                strategy: POINT,
                ..Plan::default()
            };
        }
        if let Some(i) = on_id(ConstraintOp::Gt) {
            return Plan {
                cost: 10.0,
                rows: 10,
                consumes: 1 << i,
                strategy: FROM,
                ..Plan::default()
            };
        }
        Plan {
            cost: 100.0,
            rows: 100,
            ..Plan::default()
        }
    }

    fn rows(&self, scan: &Scan<'_>, _budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        self.strategies.lock().unwrap().push(scan.strategy);
        let Some(Value::Integer(n)) = scan.args.first().cloned() else {
            return Err(FnError {
                construct: "squares".to_owned(),
                reason: "n must be an integer".to_owned(),
            });
        };
        let bound = |op| {
            scan.bound
                .iter()
                .find_map(|b| match (&b.value, b.column == "id" && b.op == op) {
                    (Value::Integer(v), true) => Some(*v),
                    _ => None,
                })
        };
        let ids = match scan.strategy {
            POINT => bound(ConstraintOp::Eq)
                .map_or(RangeInclusive::new(1, 0), |id| id.max(1)..=id.min(n)),
            FROM => bound(ConstraintOp::Gt).map_or(1..=n, |v| (v - 5).max(1)..=n),
            _ => 1..=n,
        };
        let produced = Arc::clone(&self.produced);
        Ok(Box::new(ids.map(move |id| {
            produced.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Value::Integer(id), Value::Integer(id * id)])
        })))
    }
}

fn pairs(conn: &rusqlite::Connection, sql: &str) -> Vec<(i64, i64)> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fresh_test! {
    fn a_bound_result_id_takes_the_point_lookup_with_the_same_rows_and_bounded_work() {
        let squares = Arc::new(Squares::default());
        let strategies = Arc::clone(&squares.strategies);
        with_query(vec![squares], |conn| {
            conn.execute_batch(
                "CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (3), (7), (50), (200);",
            )
            .unwrap();
            let point = "SELECT t.x, s.square FROM t JOIN squares(100) s ON s.id = t.x ORDER BY t.x";
            // `+s.id` is no column reference, so no constraint reaches the function: a full scan.
            let full = "SELECT t.x, s.square FROM t JOIN squares(100) s ON +s.id = t.x ORDER BY t.x";
            let plan = query_plan(conn, point).unwrap();
            assert!(plan.iter().any(|l| l.contains("SCAN t")), "plan: {plan:?}");

            let by_point = pairs(conn, point);
            assert!(strategies.lock().unwrap().iter().all(|&s| s == POINT), "{strategies:?}");
            let by_scan = pairs(conn, full);

            assert_eq!(by_point, vec![(3, 9), (7, 49), (50, 2500)]);
            assert_eq!(by_point, by_scan, "the point lookup changed the result");
        });
    }
}

fresh_test! {
    fn a_literal_bound_id_is_known_at_plan_time() {
        let squares = Arc::new(Squares::default());
        let (produced, planned) = (Arc::clone(&squares.produced), Arc::clone(&squares.planned_id));
        with_query(vec![squares], |conn| {
            let rows = pairs(conn, "SELECT id, square FROM squares(100) WHERE id = 12");
            assert_eq!(rows, vec![(12, 144)]);
            assert_eq!(planned.load(Ordering::SeqCst), 12, "the plan did not see the literal");
            assert_eq!(produced.load(Ordering::SeqCst), 1);
        });
    }
}

fresh_test! {
    fn a_consumed_constraint_the_function_does_not_omit_is_still_checked_by_sqlite() {
        let squares = Arc::new(Squares::default());
        let (produced, strategies) = (Arc::clone(&squares.produced), Arc::clone(&squares.strategies));
        with_query(vec![squares], |conn| {
            let rows = pairs(conn, "SELECT id, square FROM squares(100) WHERE id > 95");
            assert_eq!(rows, (96..=100).map(|i| (i, i * i)).collect::<Vec<_>>());
            assert_eq!(*strategies.lock().unwrap(), vec![FROM]);
            // The sloppy range started at 90: SQLite's own check removed 90..=95.
            assert_eq!(produced.load(Ordering::SeqCst), 11);
        });
    }
}
