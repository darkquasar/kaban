//! The safe table-function surface and its registration as an eponymous SQLite virtual table.

use std::borrow::Cow;
use std::ffi::{CStr, CString, c_int};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use kaban_core::query_budget::QueryBudget;
use rusqlite::vtab::{
    Context, Filters, IndexConstraintOp, IndexFlags, IndexInfo, Module, VTab, VTabConnection,
    VTabCursor, escape_double_quote,
};
use rusqlite::{Error, ffi};

use crate::allocator;
use crate::{BoundArg, ColumnDef, ColumnKind, OrderTerm, Plan, PlanInput, Value};

/// The rows a table function produces: each item is one row holding one value per result
/// (non-argument) column, in declaration order.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Vec<Value>, FnError>> + 'a>;

/// A table-valued function, callable from SQL as `name(arg, ...)`.
pub trait TableFunction: Send + Sync + 'static {
    /// The SQL name; also the eponymous table name.
    fn name(&self) -> &'static str;
    /// Every column, result and argument; argument columns are hidden and bind positionally in
    /// declaration order.
    fn columns(&self) -> &'static [ColumnDef];
    /// Cost one candidate plan whose required arguments are all bound.
    fn plan(&self, input: &PlanInput<'_>) -> Plan;
    /// Produce the rows for one scan. `args` holds one value per argument column in declaration
    /// order ([`Value::Null`] when an optional argument is unbound). `budget` is the budget of
    /// the request scope the scan runs in: reserve through it before allocating working memory.
    ///
    /// # Errors
    /// An [`FnError`] refuses the scan; SQLite reports it as the statement's error.
    fn rows(&self, args: &[Value], budget: &QueryBudget) -> Result<Rows<'_>, FnError>;
}

/// A refusal from a table function, surfaced as a SQLite error whose text is
/// `"{construct}: {reason}"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnError {
    /// What was refused (a function or argument name).
    pub construct: String,
    /// Why.
    pub reason: String,
}

impl fmt::Display for FnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.construct, self.reason)
    }
}

impl std::error::Error for FnError {}

/// `idx_num` carries one bit per argument column (set = bound, in argv order), so a function may
/// declare at most this many arguments.
const MAX_ARGUMENTS: usize = 31;

/// Register `f` on `conn` as an eponymous-only virtual table named [`TableFunction::name`].
///
/// Each scan hands [`TableFunction::rows`] the [`QueryBudget`] of the request scope active on the
/// scanning thread (see [`crate::RequestScope`]); a scan with no active scope is refused with an
/// error naming the function.
///
/// # Errors
/// A rusqlite error when `f` declares more than 31 argument columns, its name is not a valid C
/// string, or SQLite refuses the module.
pub fn register(conn: &rusqlite::Connection, f: Arc<dyn TableFunction>) -> rusqlite::Result<()> {
    const MODULE: Module<'static, FunctionTable> = Module::eponymous_only_module();
    let name = f.name();
    let arguments = f
        .columns()
        .iter()
        .filter(|column| column.argument.is_some())
        .count();
    if arguments > MAX_ARGUMENTS {
        return Err(Error::ModuleError(format!(
            "{name}: {arguments} argument columns declared, at most {MAX_ARGUMENTS} are supported"
        )));
    }
    conn.create_module(name, &MODULE, Some(f))
}

/// Run a call into the table function, turning a panic into an error so it never unwinds into C.
fn contain<T>(name: &str, call: &str, f: impl FnOnce() -> T) -> rusqlite::Result<T> {
    catch_unwind(AssertUnwindSafe(f))
        .map_err(|_| Error::ModuleError(format!("{name}: {call} panicked")))
}

fn refusal(error: &FnError) -> Error {
    Error::ModuleError(error.to_string())
}

/// Where a declared column's value comes from.
#[derive(Debug, Clone, Copy)]
enum Slot {
    /// The k-th value of the current row.
    Result(usize),
    /// The k-th argument of the scan.
    Argument(usize),
}

/// The `CREATE TABLE` statement SQLite reads the declared columns from.
fn schema(columns: &[ColumnDef]) -> rusqlite::Result<CString> {
    let declarations: Vec<String> = columns
        .iter()
        .map(|column| {
            let kind = match column.kind {
                ColumnKind::Integer => " INTEGER",
                ColumnKind::Real => " REAL",
                ColumnKind::Text => " TEXT",
                ColumnKind::Blob => " BLOB",
                ColumnKind::Any => "",
            };
            let hidden = if column.argument.is_some() {
                " HIDDEN"
            } else {
                ""
            };
            format!("\"{}\"{kind}{hidden}", escape_double_quote(column.name))
        })
        .collect();
    CString::new(format!("CREATE TABLE x({})", declarations.join(", ")))
        .map_err(|e| Error::ModuleError(format!("column declaration: {e}")))
}

/// One connection's instance of a registered table function.
#[repr(C)]
struct FunctionTable {
    /// The SQLite base object; must stay the first field (rusqlite's `VTab` contract).
    base: ffi::sqlite3_vtab,
    function: Arc<dyn TableFunction>,
    /// For each declared column, where its value comes from.
    slots: Vec<Slot>,
    /// The declared column index of each argument, in declaration order.
    arguments: Vec<usize>,
    /// How many result columns a row must hold.
    results: usize,
}

impl FunctionTable {
    fn column_name(&self, column: usize) -> &'static str {
        self.function
            .columns()
            .get(column)
            .map_or("?", |column| column.name)
    }

    fn is_required(&self, column: usize) -> bool {
        self.function
            .columns()
            .get(column)
            .and_then(|column| column.argument)
            .is_some_and(|argument| argument.required)
    }

    /// The argument position a constraint on declared column `column` binds, if any.
    fn argument_position(&self, column: c_int) -> Option<usize> {
        let column = usize::try_from(column).ok()?;
        match self.slots.get(column)? {
            Slot::Argument(position) => Some(*position),
            Slot::Result(_) => None,
        }
    }
}

// SAFETY: rusqlite's `VTab` contract requires the implementing struct to be `#[repr(C)]` with
// `ffi::sqlite3_vtab` as its first field, because rusqlite casts the `sqlite3_vtab` pointer SQLite
// passes back to `*mut FunctionTable`. `FunctionTable` is `#[repr(C)]` and `base` is its first
// field. No method unwinds: calls into the table function run under `contain`.
unsafe impl<'vtab> VTab<'vtab> for FunctionTable {
    type Aux = Arc<dyn TableFunction>;
    type Cursor = FunctionCursor<'vtab>;

    fn connect(
        _db: &mut VTabConnection,
        aux: Option<&Self::Aux>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> rusqlite::Result<(Cow<'static, CStr>, Self)> {
        let function = aux.cloned().ok_or_else(|| {
            Error::ModuleError("table function registered without an implementation".to_owned())
        })?;
        let columns = function.columns();
        let mut slots = Vec::with_capacity(columns.len());
        let mut arguments = Vec::new();
        let mut results = 0;
        for (index, column) in columns.iter().enumerate() {
            if column.argument.is_some() {
                slots.push(Slot::Argument(arguments.len()));
                arguments.push(index);
            } else {
                slots.push(Slot::Result(results));
                results += 1;
            }
        }
        let schema = schema(columns)?;
        Ok((
            Cow::Owned(schema),
            Self {
                base: ffi::sqlite3_vtab::default(),
                function,
                slots,
                arguments,
                results,
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> rusqlite::Result<bool> {
        let name = self.function.name();
        // The first usable equality constraint per argument, and whether an unusable one exists.
        let mut usable: Vec<Option<usize>> = vec![None; self.arguments.len()];
        let mut unusable = vec![false; self.arguments.len()];
        let mut limit_known = false;
        for (index, constraint) in info.constraints().enumerate() {
            match constraint.operator() {
                IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LIMIT => {
                    limit_known |= constraint.is_usable();
                }
                IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ => {
                    let Some(position) = self.argument_position(constraint.column()) else {
                        continue;
                    };
                    if !constraint.is_usable() {
                        unusable[position] = true;
                    } else if usable[position].is_none() {
                        usable[position] = Some(index);
                    }
                }
                _ => {}
            }
        }

        // A required argument nothing binds can never be satisfied: refuse by name. One bound
        // only by an unusable constraint may be bound in another loop order: SQLITE_CONSTRAINT.
        let unbound_required = |position: &usize| {
            usable[*position].is_none() && self.is_required(self.arguments[*position])
        };
        let mut unbound = (0..self.arguments.len()).filter(unbound_required);
        if let Some(missing) = unbound.clone().find(|position| !unusable[*position]) {
            let argument = self.column_name(self.arguments[missing]);
            return Err(Error::ModuleError(format!(
                "{name}: missing required argument {argument}"
            )));
        }
        if unbound.next().is_some() {
            return Ok(false);
        }

        let mut bound = Vec::new();
        let mut assignments = Vec::new();
        let mut idx_num: c_int = 0;
        let mut argv_index: c_int = 0;
        for (position, constraint) in usable.iter().enumerate() {
            let Some(constraint) = *constraint else {
                continue;
            };
            argv_index += 1;
            idx_num |= 1 << position;
            assignments.push((constraint, argv_index));
            bound.push(BoundArg {
                name: self.column_name(self.arguments[position]),
                value: info.rhs_value(constraint).ok().flatten().map(Value::from),
            });
        }

        // An order on a column the function cannot name (the rowid) is never consumed.
        let columns = self.function.columns();
        let order_by: Option<Vec<OrderTerm>> = info
            .order_bys()
            .map(|term| {
                let column = usize::try_from(term.column()).ok()?;
                columns.get(column).map(|column| OrderTerm {
                    name: column.name,
                    desc: term.is_order_by_desc(),
                })
            })
            .collect();
        let order_by = order_by.unwrap_or_default();

        let input = PlanInput {
            bound: &bound,
            order_by: &order_by,
            limit_known,
        };
        let plan = contain(name, "plan", || self.function.plan(&input))?;

        for (constraint, argv_index) in assignments {
            let mut usage = info.constraint_usage(constraint);
            usage.set_argv_index(argv_index);
            usage.set_omit(true);
        }
        info.set_idx_num(idx_num);
        let cost = if plan.cost.is_finite() && plan.cost >= 0.0 {
            plan.cost
        } else {
            f64::MAX
        };
        info.set_estimated_cost(cost);
        info.set_estimated_rows(plan.rows.max(0));
        if plan.consumes_order && !order_by.is_empty() {
            info.set_order_by_consumed(true);
        }
        if plan.point_lookup {
            info.set_idx_flags(IndexFlags::SQLITE_INDEX_SCAN_UNIQUE);
        }
        Ok(true)
    }

    fn open(&'vtab mut self) -> rusqlite::Result<FunctionCursor<'vtab>> {
        Ok(FunctionCursor {
            base: ffi::sqlite3_vtab_cursor::default(),
            table: self,
            args: Vec::new(),
            rows: None,
            current: None,
            rowid: 0,
        })
    }
}

/// One scan over a table function's rows.
#[repr(C)]
struct FunctionCursor<'vtab> {
    /// The SQLite base object; must stay the first field (rusqlite's `VTabCursor` contract).
    base: ffi::sqlite3_vtab_cursor,
    table: &'vtab FunctionTable,
    /// The scan's arguments, one per argument column.
    args: Vec<Value>,
    rows: Option<Rows<'vtab>>,
    /// The current row; `None` at end of scan.
    current: Option<Vec<Value>>,
    rowid: i64,
}

impl FunctionCursor<'_> {
    /// Move to the next row, ending the scan on exhaustion or error.
    fn advance(&mut self) -> rusqlite::Result<()> {
        let table = self.table;
        let name = table.function.name();
        let next = match self.rows.as_mut() {
            Some(rows) => contain(name, "rows", || rows.next()),
            None => Ok(None),
        };
        match next {
            Ok(Some(Ok(row))) if row.len() == table.results => {
                self.current = Some(row);
                self.rowid += 1;
                Ok(())
            }
            Ok(Some(Ok(row))) => {
                self.stop();
                Err(Error::ModuleError(format!(
                    "{name}: a row holds {} values but {} result columns are declared",
                    row.len(),
                    table.results
                )))
            }
            Ok(Some(Err(error))) => {
                self.stop();
                Err(refusal(&error))
            }
            Ok(None) => {
                self.stop();
                Ok(())
            }
            Err(error) => {
                self.stop();
                Err(error)
            }
        }
    }

    /// End the scan. The row iterator is dropped under `catch_unwind`: its `Drop` is table-function
    /// code and must not unwind into C either.
    fn stop(&mut self) {
        let rows = self.rows.take();
        drop(catch_unwind(AssertUnwindSafe(move || drop(rows))));
        self.current = None;
    }
}

impl Drop for FunctionCursor<'_> {
    fn drop(&mut self) {
        self.stop();
    }
}

// SAFETY: rusqlite's `VTabCursor` contract requires the implementing struct to be `#[repr(C)]` with
// `ffi::sqlite3_vtab_cursor` as its first field, because rusqlite casts the cursor pointer SQLite
// passes back to `*mut FunctionCursor`. `FunctionCursor` is `#[repr(C)]` and `base` is its first
// field. No method unwinds: calls into the table function and its rows run under `contain`.
unsafe impl VTabCursor for FunctionCursor<'_> {
    fn filter(
        &mut self,
        idx_num: c_int,
        _idx_str: Option<&str>,
        args: &Filters<'_>,
    ) -> rusqlite::Result<()> {
        let table = self.table;
        let name = table.function.name();
        let mut supplied = args.iter();
        self.args = (0..table.arguments.len())
            .map(|position| {
                if (idx_num >> position) & 1 == 1 {
                    supplied.next().map_or(Value::Null, Value::from)
                } else {
                    Value::Null
                }
            })
            .collect();
        self.stop();
        self.rowid = 0;
        // A scan runs only inside a request budget scope: it is charged to that scope's budget.
        let Some(budget) = allocator::current_budget() else {
            return Err(Error::ModuleError(format!(
                "{name}: table function scanned outside a request budget scope"
            )));
        };
        let function: &dyn TableFunction = &*table.function;
        let rows = contain(name, "rows", || function.rows(&self.args, &budget))?
            .map_err(|error| refusal(&error))?;
        self.rows = Some(rows);
        self.advance()
    }

    fn next(&mut self) -> rusqlite::Result<()> {
        self.advance()
    }

    fn eof(&self) -> bool {
        self.current.is_none()
    }

    fn column(&self, ctx: &mut Context, i: c_int) -> rusqlite::Result<()> {
        let slot = usize::try_from(i)
            .ok()
            .and_then(|column| self.table.slots.get(column));
        let value = match slot {
            Some(Slot::Result(k)) => self.current.as_ref().and_then(|row| row.get(*k)),
            Some(Slot::Argument(k)) => self.args.get(*k),
            None => None,
        };
        ctx.set_result(value.unwrap_or(&Value::Null))
    }

    fn rowid(&self) -> rusqlite::Result<i64> {
        Ok(self.rowid)
    }
}
