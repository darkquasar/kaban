//! The adapter's Rust copies of argument values are request memory: planning and scanning
//! reserve them against the active budget before copying, hold the reservation for the copy's
//! life and release it afterwards (spec 11 T12, QRY-06, PC-17, MAJ-03). Each case runs in a fresh
//! process with the allocator adapter installed.
#![allow(clippy::unwrap_used)]

mod common;
mod fresh;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fresh::{fresh_test, installed_runtime, query_flags};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{
    Arg, ColumnDef, ColumnKind, FnError, Plan, PlanInput, RequestScope, Rows, TableFunction,
    Value, register,
};

/// `echo(data)`: one row holding the byte length of `data`. It records the budget's live bytes
/// when it plans and when it scans, which is after the adapter made its copies.
struct Echo {
    budget: QueryBudget,
    planned: Arc<AtomicU64>,
    scanned: Arc<AtomicU64>,
}

const ECHO_COLUMNS: &[ColumnDef] = &[
    ColumnDef {
        name: "len",
        kind: ColumnKind::Integer,
        argument: None,
    },
    ColumnDef {
        name: "data",
        kind: ColumnKind::Any,
        argument: Some(Arg { required: true }),
    },
];

impl TableFunction for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        ECHO_COLUMNS
    }

    fn plan(&self, _input: &PlanInput<'_>) -> Plan {
        self.planned.fetch_max(self.budget.live_bytes(), Ordering::SeqCst);
        Plan {
            cost: 1.0,
            rows: 1,
            consumes_order: false,
            point_lookup: true,
        }
    }

    fn rows(&self, args: &[Value], budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        self.scanned.store(budget.live_bytes(), Ordering::SeqCst);
        let len = match args {
            [Value::Blob(bytes)] => bytes.len(),
            [Value::Text(text)] => text.len(),
            _ => 0,
        };
        let len = i64::try_from(len).unwrap();
        Ok(Box::new(std::iter::once(Ok(vec![Value::Integer(len)]))))
    }
}

/// The size of the argument values: large against everything else the statement holds.
const VALUE: usize = 256 * 1024;

/// What one run observed.
struct Run {
    outcome: Result<i64, String>,
    planned: u64,
    scanned: u64,
    after_close: u64,
}

/// Run `sql` (with `blob` bound to `?1` when given) on a fresh query connection under a budget
/// of `max` bytes.
fn run(max: u64, sql: &str, blob: Option<&[u8]>) -> Run {
    let runtime = installed_runtime();
    let budget = QueryBudget::new(max, None);
    let planned = Arc::new(AtomicU64::new(0));
    let scanned = Arc::new(AtomicU64::new(0));
    let scope = RequestScope::enter(runtime, &budget).unwrap();
    let conn = scope
        .open_query_connection(std::path::Path::new(":memory:"), query_flags())
        .unwrap();
    let echo = Echo {
        budget: budget.clone(),
        planned: Arc::clone(&planned),
        scanned: Arc::clone(&scanned),
    };
    register(&conn, Arc::new(echo)).unwrap();
    let outcome = conn
        .prepare(sql)
        .and_then(|mut stmt| match blob {
            Some(blob) => stmt.query_row([blob], |row| row.get(0)),
            None => stmt.query_row([], |row| row.get(0)),
        })
        .map_err(|e| e.to_string());
    conn.close().unwrap();
    let after_close = budget.live_bytes();
    drop(scope);
    Run {
        outcome,
        planned: planned.load(Ordering::SeqCst),
        scanned: scanned.load(Ordering::SeqCst),
        after_close,
    }
}

fn assert_copy_refused(run: &Run) {
    let error = run.outcome.as_ref().unwrap_err();
    assert!(
        error.contains("echo") && error.contains("data") && error.contains("Memory"),
        "the refusal must name the function, the argument and the limit: {error}"
    );
    assert_eq!(run.after_close, 0, "the refused request kept charged bytes");
}

fresh_test! {
    fn a_scan_argument_too_large_to_copy_is_refused_before_the_copy() {
        let blob = vec![0x5A_u8; VALUE];
        let sql = "SELECT len FROM echo(?1)";
        // With room to spare, the scan sees SQLite's value and the adapter's copy both charged.
        let roomy = run(u64::MAX, sql, Some(&blob));
        assert_eq!(roomy.outcome, Ok(i64::try_from(VALUE).unwrap()));
        assert_eq!(roomy.after_close, 0);
        // Room for SQLite's bound value, but not for the Rust copy on top of it.
        let tight = run(roomy.scanned - VALUE as u64 / 2, sql, Some(&blob));
        assert_copy_refused(&tight);
    }
}

fresh_test! {
    fn a_planned_literal_too_large_to_copy_is_refused_before_the_copy() {
        let hex: String = std::iter::repeat_n("5a", VALUE / 4).collect();
        let sql = format!("SELECT len FROM echo(x'{hex}')");
        let roomy = run(u64::MAX, &sql, None);
        assert_eq!(roomy.outcome, Ok(i64::try_from(VALUE / 4).unwrap()));
        assert_eq!(roomy.after_close, 0);
        let tight = run(roomy.planned - VALUE as u64 / 8, &sql, None);
        assert_copy_refused(&tight);
    }
}
