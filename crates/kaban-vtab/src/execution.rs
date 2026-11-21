//! Statement-bound virtual-table planning and scans.

use std::any::Any;
use std::cell::Cell;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;

use crate::{FnError, Plan, PlanInput, QueryConnection, Rows, Scan, Value};
use kaban_core::query_budget::{BudgetLease, QueryBudget};

/// Read operations provided by the owner of the snapshot.
pub trait ExecutionPayload {
    /// Owned cursor state. The adapter reserves its boxed storage before calling [`Self::open`].
    /// The payload reserves any additional heap buffers it creates through the supplied budget.
    type Cursor: 'static;

    /// Open a scan using the statement's fixed read operations.
    ///
    /// # Errors
    /// A refusal from the operation or its budget.
    fn open(
        &self,
        function: &'static str,
        scan: &Scan<'_>,
        budget: &QueryBudget,
    ) -> Result<Self::Cursor, FnError>;

    /// Produce a row using this statement's payload. Returning `None` finishes the scan.
    fn next(
        &self,
        cursor: &mut Self::Cursor,
        budget: &QueryBudget,
    ) -> Option<Result<Vec<Value>, FnError>>;
}

/// Cost a function from its pinned statistics. A planner must not execute SQL.
pub type PlanFunction<'a> =
    dyn Fn(&'static str, &PlanInput<'_>, &QueryBudget) -> Result<Plan, FnError> + 'a;

/// Planning statistics and read operations borrowed for one statement.
pub struct Execution<'a, P> {
    /// The SQL-free planning callback.
    pub plan: &'a PlanFunction<'a>,
    /// The snapshot and its fixed read operations, used only during execution.
    pub payload: &'a P,
}

/// Run synchronously with the dedicated connection's payload and active request budget.
///
/// # Errors
/// Refuses a nested binding.
pub fn with_execution<P: ExecutionPayload, R>(
    conn: &QueryConnection<'_>,
    execution: &Execution<'_, P>,
    run: impl FnOnce() -> R,
) -> Result<R, FnError> {
    if !ACTIVE.get().is_null() {
        return Err(refused("an execution is already active"));
    }
    let binding = Binding {
        connection: &conn.identity,
        statement: Rc::new(()),
        budget: conn.scope.budget(),
        plan: execution.plan,
        scans: execution.payload,
    };
    ACTIVE.set(ptr::from_ref(&binding).cast());
    let _clear = Clear;
    Ok(run())
}

struct Cursor {
    state: Box<dyn Any>,
    _storage: BudgetLease,
}

impl Cursor {
    fn step(&mut self, binding: &Binding<'_>) -> Option<Result<Vec<Value>, FnError>> {
        binding.scans.next(&mut *self.state, binding.budget)
    }
}

trait Scans {
    fn open(
        &self,
        name: &'static str,
        scan: &Scan<'_>,
        budget: &QueryBudget,
    ) -> Result<Cursor, FnError>;
    fn next(
        &self,
        cursor: &mut dyn Any,
        budget: &QueryBudget,
    ) -> Option<Result<Vec<Value>, FnError>>;
}

impl<P: ExecutionPayload> Scans for P {
    fn open(
        &self,
        name: &'static str,
        scan: &Scan<'_>,
        budget: &QueryBudget,
    ) -> Result<Cursor, FnError> {
        let bytes = size_of::<P::Cursor>() as u64;
        let charge = if bytes == 0 {
            0
        } else {
            QueryBudget::allocation_charge(
                bytes,
                align_of::<P::Cursor>().max(align_of::<usize>()) as u64,
            )
            .map_err(|e| refused(&e.to_string()))?
        };
        let storage = budget
            .reserve(charge)
            .map_err(|e| refused(&e.to_string()))?;
        Ok(Cursor {
            state: Box::new(ExecutionPayload::open(self, name, scan, budget)?),
            _storage: storage,
        })
    }

    fn next(
        &self,
        cursor: &mut dyn Any,
        budget: &QueryBudget,
    ) -> Option<Result<Vec<Value>, FnError>> {
        match cursor.downcast_mut() {
            Some(cursor) => ExecutionPayload::next(self, cursor, budget),
            None => Some(Err(refused("invalid cursor for this execution"))),
        }
    }
}

struct Binding<'a> {
    connection: &'a Arc<()>,
    statement: Rc<()>,
    budget: &'a QueryBudget,
    plan: &'a PlanFunction<'a>,
    scans: &'a dyn Scans,
}

thread_local! {
    static ACTIVE: Cell<*const ()> = const { Cell::new(ptr::null()) };
}

struct Clear;
impl Drop for Clear {
    fn drop(&mut self) {
        ACTIVE.set(ptr::null());
    }
}

fn refused(reason: &str) -> FnError {
    FnError {
        construct: "execution".into(),
        reason: reason.into(),
    }
}

fn bound<R>(
    connection: &Arc<()>,
    callback: impl for<'a> FnOnce(&'a Binding<'a>) -> Result<R, FnError>,
) -> Result<R, FnError> {
    let pointer = ACTIVE.get();
    if pointer.is_null() {
        return Err(refused("no active statement execution"));
    }
    // SAFETY: only with_execution installs a pointer, to its immovably borrowed stack binding.
    // The private Clear drops before that binding on return or unwind. The slot is thread-local,
    // nesting is refused, and the higher-ranked callback cannot return a borrow of the binding.
    let binding = unsafe { &*pointer.cast::<Binding<'_>>() };
    if !Arc::ptr_eq(connection, binding.connection) {
        return Err(refused("statement execution belongs to another connection"));
    }
    binding
        .budget
        .check()
        .map_err(|error| refused(&error.to_string()))?;
    let result = callback(binding)?;
    binding
        .budget
        .check()
        .map_err(|error| refused(&error.to_string()))?;
    Ok(result)
}

pub(crate) fn plan(
    connection: &Arc<()>,
    name: &'static str,
    input: &PlanInput<'_>,
) -> Result<Plan, FnError> {
    bound(connection, |binding| {
        (binding.plan)(name, input, binding.budget)
    })
}

pub(crate) fn rows(
    connection: &Arc<()>,
    name: &'static str,
    scan: &Scan<'_>,
) -> Result<Rows<'static>, FnError> {
    let (statement, mut cursor) = bound(connection, |binding| {
        let cursor = binding.scans.open(name, scan, binding.budget)?;
        Ok((Rc::clone(&binding.statement), cursor))
    })?;
    let connection = Arc::clone(connection);
    Ok(Box::new(std::iter::from_fn(move || {
        bound(&connection, |binding| {
            if !Rc::ptr_eq(&statement, &binding.statement) {
                return Err(refused("cursor belongs to a different statement execution"));
            }
            Ok(cursor.step(binding))
        })
        .unwrap_or_else(|error| Some(Err(error)))
    })))
}
