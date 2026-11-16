//! Table-function code never unwinds into SQLite: a panic in its metadata, planning, row
//! production or destruction, including a panic payload whose own `Drop` panics, becomes a named
//! error where an error channel exists and is contained otherwise (spec 11 T12, MAJ-01). Each case
//! runs in a fresh child process, so an abort at an `extern "C"` boundary fails the case instead
//! of the test binary.
#![allow(clippy::unwrap_used, clippy::panic)] // the hostile toys panic on purpose

mod common;
mod fresh;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::integers;
use fresh::{fresh_test, installed_runtime, with_query};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{
    Arg, ColumnDef, ColumnKind, FnError, Plan, PlanInput, Rows, TableFunction, Value,
};

/// A panic payload whose destructor panics again.
struct PanicsWhenDropped;

impl Drop for PanicsWhenDropped {
    fn drop(&mut self) {
        panic!("the panic payload was dropped");
    }
}

/// Where a [`Hostile`] function panics.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    Name,
    Columns,
    Plan,
    Rows,
    Next,
    /// `rows` panics with a [`PanicsWhenDropped`] payload.
    RowsPayload,
    /// The row iterator's `Drop` panics with a [`PanicsWhenDropped`] payload.
    IteratorDropPayload,
    /// The function's own `Drop` panics, when SQLite frees the module's client data.
    FunctionDrop,
}

struct Hostile(Fault);

const HOSTILE_COLUMNS: &[ColumnDef] = &[
    ColumnDef {
        name: "value",
        kind: ColumnKind::Integer,
        argument: None,
    },
    ColumnDef {
        name: "n",
        kind: ColumnKind::Integer,
        argument: Some(Arg { required: false }),
    },
];

struct PayloadOnDrop(Option<i64>);

impl Iterator for PayloadOnDrop {
    type Item = Result<Vec<Value>, FnError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.take().map(|v| Ok(vec![Value::Integer(v)]))
    }
}

impl Drop for PayloadOnDrop {
    fn drop(&mut self) {
        std::panic::panic_any(PanicsWhenDropped);
    }
}

impl TableFunction for Hostile {
    fn name(&self) -> &'static str {
        assert!(self.0 != Fault::Name, "name failed");
        "hostile"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        assert!(self.0 != Fault::Columns, "columns failed");
        HOSTILE_COLUMNS
    }

    fn plan(&self, _input: &PlanInput<'_>) -> Plan {
        assert!(self.0 != Fault::Plan, "plan failed");
        Plan {
            cost: 1.0,
            rows: 1,
            consumes_order: false,
            point_lookup: false,
        }
    }

    fn rows(&self, _args: &[Value], _budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        match self.0 {
            Fault::Rows => panic!("rows failed"),
            Fault::RowsPayload => std::panic::panic_any(PanicsWhenDropped),
            Fault::Next => Ok(Box::new(std::iter::from_fn(|| panic!("next failed")))),
            Fault::IteratorDropPayload => Ok(Box::new(PayloadOnDrop(Some(7)))),
            _ => Ok(Box::new(std::iter::once(Ok(vec![Value::Integer(7)])))),
        }
    }
}

impl Drop for Hostile {
    fn drop(&mut self) {
        if self.0 == Fault::FunctionDrop {
            std::panic::panic_any(PanicsWhenDropped);
        }
    }
}

/// Register `Hostile(fault)` on a request's query connection and return the registration error.
fn registration_error(fault: Fault) -> String {
    let runtime = installed_runtime();
    let budget = QueryBudget::new(u64::MAX, None);
    let scope = kaban_vtab::RequestScope::enter(runtime, &budget).unwrap();
    let conn = scope
        .open_query_connection(std::path::Path::new(":memory:"), fresh::query_flags())
        .unwrap();
    kaban_vtab::register(&conn, Arc::new(Hostile(fault)))
        .unwrap_err()
        .to_string()
}

/// Run `SELECT value FROM hostile` against `Hostile(fault)` and return its outcome.
fn scan(fault: Fault) -> Result<Vec<i64>, String> {
    let mut outcome = None;
    with_query(vec![Arc::new(Hostile(fault))], |conn| {
        outcome = Some(integers(conn, "SELECT value FROM hostile"));
    });
    outcome.unwrap()
}

fresh_test! {
    fn a_panicking_name_is_refused_at_registration() {
        let error = registration_error(Fault::Name);
        assert!(error.contains("name panicked"), "{error}");
    }
}

fresh_test! {
    fn panicking_columns_are_refused_at_registration_naming_the_function() {
        let error = registration_error(Fault::Columns);
        assert!(error.contains("hostile: columns panicked"), "{error}");
    }
}

fresh_test! {
    fn a_panicking_plan_is_an_error_naming_the_function() {
        let error = scan(Fault::Plan).unwrap_err();
        assert!(error.contains("hostile: plan panicked"), "{error}");
    }
}

fresh_test! {
    fn a_panicking_rows_is_an_error_naming_the_function() {
        let error = scan(Fault::Rows).unwrap_err();
        assert!(error.contains("hostile: rows panicked"), "{error}");
    }
}

fresh_test! {
    fn a_panicking_row_step_is_an_error_naming_the_function() {
        let error = scan(Fault::Next).unwrap_err();
        assert!(error.contains("hostile: rows panicked"), "{error}");
    }
}

fresh_test! {
    fn a_panic_payload_whose_drop_panics_is_still_a_named_error() {
        let error = scan(Fault::RowsPayload).unwrap_err();
        assert!(error.contains("hostile: rows panicked"), "{error}");
    }
}

fresh_test! {
    fn an_iterator_drop_panicking_with_a_hostile_payload_is_contained() {
        assert_eq!(scan(Fault::IteratorDropPayload), Ok(vec![7]));
    }
}

fresh_test! {
    fn a_function_whose_drop_panics_is_destroyed_without_aborting() {
        assert_eq!(scan(Fault::FunctionDrop), Ok(vec![7]));
        // The connection closed and SQLite freed the module's client data, which dropped the
        // last handle on the function: the process is still here.
    }
}

/// Counts every metadata call; its argument count grows after the first `columns()` call.
struct Shifty {
    names: Arc<AtomicUsize>,
    columns: Arc<AtomicUsize>,
}

const SHIFTY_NARROW: &[ColumnDef] = &[ColumnDef {
    name: "value",
    kind: ColumnKind::Integer,
    argument: None,
}];

/// 40 optional arguments: more than the 31 an index number can carry.
static SHIFTY_WIDE: std::sync::LazyLock<&'static [ColumnDef]> = std::sync::LazyLock::new(|| {
    let mut columns = vec![SHIFTY_NARROW[0]];
    let names: &'static [String] = Vec::leak((0..40).map(|i| format!("a{i}")).collect());
    columns.extend(names.iter().map(|name| ColumnDef {
        name,
        kind: ColumnKind::Integer,
        argument: Some(Arg { required: false }),
    }));
    Vec::leak(columns)
});

impl TableFunction for Shifty {
    fn name(&self) -> &'static str {
        self.names.fetch_add(1, Ordering::SeqCst);
        "shifty"
    }

    fn columns(&self) -> &'static [ColumnDef] {
        if self.columns.fetch_add(1, Ordering::SeqCst) == 0 {
            SHIFTY_NARROW
        } else {
            *SHIFTY_WIDE
        }
    }

    fn plan(&self, _input: &PlanInput<'_>) -> Plan {
        Plan {
            cost: 1.0,
            rows: 1,
            consumes_order: false,
            point_lookup: false,
        }
    }

    fn rows(&self, _args: &[Value], _budget: &QueryBudget) -> Result<Rows<'_>, FnError> {
        Ok(Box::new(std::iter::once(Ok(vec![Value::Integer(1)]))))
    }
}

fresh_test! {
    fn metadata_is_read_once_at_registration() {
        let names = Arc::new(AtomicUsize::new(0));
        let columns = Arc::new(AtomicUsize::new(0));
        let shifty = Shifty {
            names: Arc::clone(&names),
            columns: Arc::clone(&columns),
        };
        with_query(vec![Arc::new(shifty)], |conn| {
            assert_eq!(integers(conn, "SELECT value FROM shifty"), Ok(vec![1]));
            assert_eq!(integers(conn, "SELECT value FROM shifty"), Ok(vec![1]));
        });
        assert_eq!(names.load(Ordering::SeqCst), 1, "name() was called again after registration");
        assert_eq!(columns.load(Ordering::SeqCst), 1, "columns() was called again");
    }
}
