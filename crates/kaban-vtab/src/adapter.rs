//! The safe table-function surface and its registration as an eponymous SQLite virtual table.

use std::borrow::Cow;
use std::ffi::{CStr, CString, c_int};
use std::fmt;
use std::mem;
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
    let registered = Registered::new(f)?;
    let name = registered.name;
    conn.create_module(name, &MODULE, Some(Arc::new(registered)))
}

/// Run a call into the table function, turning a panic into an error so it never unwinds into C.
/// The panic payload is leaked, not dropped: its destructor is table-function code too and could
/// panic again outside any containment.
fn contain<T>(name: &str, call: &str, f: impl FnOnce() -> T) -> rusqlite::Result<T> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        mem::forget(payload);
        Error::ModuleError(format!("{name}: {call} panicked"))
    })
}

/// Drop table-function-owned `value` (a row iterator, the function itself) without letting a
/// panicking destructor unwind; a panic payload is leaked, as in [`contain`].
fn drop_contained<T>(value: T) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(move || drop(value))) {
        mem::forget(payload);
    }
}

/// The registered function. Its final drop is table-function code, which can run inside SQLite's
/// module destructor or `xDisconnect`, so it runs contained ([`drop_contained`]).
struct ContainedFunction(Option<Arc<dyn TableFunction>>);

impl ContainedFunction {
    /// The function; `None` only while it is being dropped.
    fn get(&self) -> Option<&dyn TableFunction> {
        self.0.as_deref()
    }
}

impl Drop for ContainedFunction {
    fn drop(&mut self) {
        drop_contained(self.0.take());
    }
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

/// The immutable state of one registered function: the function and what registration read and
/// validated from it. It is allocated once by [`register`], apart from every SQLite object, and
/// shared (`Arc`) by each connection's [`FunctionTable`] and each [`FunctionCursor`]; nothing
/// mutates it after construction, so references to it never alias a value SQLite writes.
struct Registered {
    function: ContainedFunction,
    /// The SQL name, read once at registration.
    name: &'static str,
    /// Every declared column, read once at registration.
    columns: &'static [ColumnDef],
    /// The `CREATE TABLE` statement SQLite reads the declared columns from.
    schema: CString,
    /// For each declared column, where its value comes from.
    slots: Vec<Slot>,
    /// The declared column index of each argument, in declaration order.
    arguments: Vec<usize>,
    /// How many result columns a row must hold.
    results: usize,
}

impl Registered {
    /// Read and validate `function`'s declaration once; callbacks never call `name` or `columns`
    /// again, so a declaration cannot change (or panic) after it was validated.
    fn new(function: Arc<dyn TableFunction>) -> rusqlite::Result<Self> {
        let function = ContainedFunction(Some(function));
        let Some(declared) = function.get() else {
            return Err(Error::ModuleError("table function released".to_owned()));
        };
        let name = contain("table function", "name", || declared.name())?;
        let columns = contain(name, "columns", || declared.columns())?;
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
        if arguments.len() > MAX_ARGUMENTS {
            return Err(Error::ModuleError(format!(
                "{name}: {} argument columns declared, at most {MAX_ARGUMENTS} are supported",
                arguments.len()
            )));
        }
        let schema = schema(columns)?;
        Ok(Self {
            function,
            name,
            columns,
            schema,
            slots,
            arguments,
            results,
        })
    }

    /// The function, for a callback to call (contained).
    fn function(&self) -> rusqlite::Result<&dyn TableFunction> {
        self.function
            .get()
            .ok_or_else(|| Error::ModuleError(format!("{}: table function released", self.name)))
    }

    fn column_name(&self, column: usize) -> &'static str {
        self.columns.get(column).map_or("?", |column| column.name)
    }

    fn is_required(&self, column: usize) -> bool {
        self.columns
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

/// One connection's instance of a registered table function: the SQLite base object SQLite
/// writes to (`nRef`, `zErrMsg`) and a handle on the shared [`Registered`] state.
#[repr(C)]
struct FunctionTable {
    /// The SQLite base object; must stay the first field (rusqlite's `VTab` contract).
    base: ffi::sqlite3_vtab,
    registered: Arc<Registered>,
}

// SAFETY: rusqlite's `VTab` contract requires the implementing struct to be `#[repr(C)]` with
// `ffi::sqlite3_vtab` as its first field, because rusqlite casts the `sqlite3_vtab` pointer SQLite
// passes back to `*mut FunctionTable`. `FunctionTable` is `#[repr(C)]` and `base` is its first
// field. No reference into a `FunctionTable` outlives the callback that created it: a cursor holds
// only the separately allocated `Registered` state, so SQLite's writes to `base` (`nRef` after
// `xOpen`, `zErrMsg` on an error) and rusqlite's `&mut` per `xOpen` alias nothing a cursor reads
// (docs/developer/SAFETY.md, cursor ownership).
unsafe impl<'vtab> VTab<'vtab> for FunctionTable {
    type Aux = Arc<Registered>;
    type Cursor = FunctionCursor<'vtab>;

    fn connect(
        _db: &mut VTabConnection,
        aux: Option<&Self::Aux>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> rusqlite::Result<(Cow<'static, CStr>, Self)> {
        let registered = aux.cloned().ok_or_else(|| {
            Error::ModuleError("table function registered without an implementation".to_owned())
        })?;
        Ok((
            Cow::Owned(registered.schema.clone()),
            Self {
                base: ffi::sqlite3_vtab::default(),
                registered,
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> rusqlite::Result<bool> {
        let registered = &*self.registered;
        let name = registered.name;
        // The first usable equality constraint per argument, and whether an unusable one exists.
        let mut usable: Vec<Option<usize>> = vec![None; registered.arguments.len()];
        let mut unusable = vec![false; registered.arguments.len()];
        let mut limit_known = false;
        for (index, constraint) in info.constraints().enumerate() {
            match constraint.operator() {
                IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LIMIT => {
                    limit_known |= constraint.is_usable();
                }
                IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ => {
                    let Some(position) = registered.argument_position(constraint.column()) else {
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
            usable[*position].is_none() && registered.is_required(registered.arguments[*position])
        };
        let mut unbound = (0..registered.arguments.len()).filter(unbound_required);
        if let Some(missing) = unbound.clone().find(|position| !unusable[*position]) {
            let argument = registered.column_name(registered.arguments[missing]);
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
                name: registered.column_name(registered.arguments[position]),
                value: info.rhs_value(constraint).ok().flatten().map(Value::from),
            });
        }

        // An order on a column the function cannot name (the rowid) is never consumed.
        let order_by: Option<Vec<OrderTerm>> = info
            .order_bys()
            .map(|term| {
                let column = usize::try_from(term.column()).ok()?;
                registered.columns.get(column).map(|column| OrderTerm {
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
        let function = registered.function()?;
        let plan = contain(name, "plan", || function.plan(&input))?;

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
        // Only the shared `Registered` allocation is borrowed for `'vtab`, never `self`: the
        // reference points into the `Arc`'s own heap block, and the cursor keeps that block alive
        // with its own handle.
        let table: &'vtab Self = self;
        Ok(FunctionCursor {
            base: ffi::sqlite3_vtab_cursor::default(),
            registered: &table.registered,
            args: Vec::new(),
            rows: None,
            current: None,
            rowid: 0,
            _keep: Arc::clone(&table.registered),
        })
    }
}

/// One scan over a table function's rows.
#[repr(C)]
struct FunctionCursor<'vtab> {
    /// The SQLite base object; must stay the first field (rusqlite's `VTabCursor` contract).
    base: ffi::sqlite3_vtab_cursor,
    /// The registered function's shared state, inside the `Arc` block `_keep` holds.
    registered: &'vtab Registered,
    /// The scan's arguments, one per argument column.
    args: Vec<Value>,
    rows: Option<Rows<'vtab>>,
    /// The current row; `None` at end of scan.
    current: Option<Vec<Value>>,
    rowid: i64,
    /// Keeps the `Registered` block alive for as long as the cursor, whatever order SQLite
    /// disconnects the table and closes cursors in. Declared last: dropped after `rows`.
    _keep: Arc<Registered>,
}

impl FunctionCursor<'_> {
    /// Move to the next row, ending the scan on exhaustion or error.
    fn advance(&mut self) -> rusqlite::Result<()> {
        let registered = self.registered;
        let name = registered.name;
        let next = match self.rows.as_mut() {
            Some(rows) => contain(name, "rows", || rows.next()),
            None => Ok(None),
        };
        match next {
            Ok(Some(Ok(row))) if row.len() == registered.results => {
                self.current = Some(row);
                self.rowid += 1;
                Ok(())
            }
            Ok(Some(Ok(row))) => {
                self.stop();
                Err(Error::ModuleError(format!(
                    "{name}: a row holds {} values but {} result columns are declared",
                    row.len(),
                    registered.results
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

    /// End the scan. The row iterator's `Drop` is table-function code: it runs contained.
    fn stop(&mut self) {
        drop_contained(self.rows.take());
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
// field. It holds no reference into the `sqlite3_vtab` allocation (see the `VTab` impl above).
unsafe impl VTabCursor for FunctionCursor<'_> {
    fn filter(
        &mut self,
        idx_num: c_int,
        _idx_str: Option<&str>,
        args: &Filters<'_>,
    ) -> rusqlite::Result<()> {
        let registered = self.registered;
        let name = registered.name;
        let mut supplied = args.iter();
        self.args = (0..registered.arguments.len())
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
        let function = registered.function()?;
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
            .and_then(|column| self.registered.slots.get(column));
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
