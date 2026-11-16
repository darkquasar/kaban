//! Request-scoped query budgets over the allocator adapter (spec 11 QRY-04, QRY-06, PC-17).
//!
//! A [`RequestScope`] charges every SQLite allocation its thread makes to one [`QueryBudget`]
//! until it drops. [`RequestScope::open_query_connection`] opens the request's dedicated
//! connection inside the scope; the [`QueryConnection`] borrows the scope, so opening,
//! preparing, stepping, finalizing and closing all happen while the scope is active, and the
//! connection (with every cache it holds) is closed before the request ends. Writers that never
//! enter a scope keep SQLite's original allocation behaviour. Nothing here changes a
//! process-global SQLite limit.

use std::ffi::{c_int, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;
use std::ptr;

use kaban_core::query_budget::{BudgetExceeded, Limit, QueryBudget};
use rusqlite::{Connection, ErrorCode, OpenFlags, ffi};

use crate::Runtime;
use crate::allocator::{self, ScopeGuard};

/// While alive, SQLite allocations on this thread are charged to the request's budget.
///
/// Dropping it, including during an unwind, restores the scope that was active before; blocks
/// charged inside it keep their budget until they are freed, on any thread, after the request.
/// At most one scope is active on a thread at a time ([`RequestScope::enter`] refuses a second),
/// so every SQLite allocation, statement and scan on the thread belongs to that one request. It
/// is bound to the thread that entered it.
#[derive(Debug)]
pub struct RequestScope {
    budget: QueryBudget,
    _guard: ScopeGuard,
    _runtime: Runtime,
}

impl RequestScope {
    /// Charge this thread's SQLite allocations, and every table-function scan it runs, to
    /// `budget` until the scope drops. The [`Runtime`] proves the allocator adapter is installed.
    ///
    /// # Errors
    /// [`ScopeActive`] when a request scope is already active on this thread: scopes do not nest,
    /// so every allocation and scan on the thread belongs to exactly one request.
    pub fn enter(runtime: Runtime, budget: &QueryBudget) -> Result<Self, ScopeActive> {
        let guard = allocator::try_enter_scope(budget).ok_or(ScopeActive)?;
        Ok(Self {
            budget: budget.clone(),
            _guard: guard,
            _runtime: runtime,
        })
    }

    /// The request's budget.
    #[must_use]
    pub fn budget(&self) -> &QueryBudget {
        &self.budget
    }

    /// Open the request's dedicated query connection inside this scope.
    ///
    /// The connection has lookaside disabled and SQLite worker threads off, so every allocation
    /// it makes is charged here on this thread, and its progress handler interrupts a statement
    /// once the budget's deadline passes or the request is cancelled. It is closed when the
    /// returned value drops or [`QueryConnection::close`] runs, both before the scope ends.
    ///
    /// # Errors
    /// A [`QueryError`] when SQLite cannot open or configure the connection; a refusal caused by
    /// the budget names the limit.
    pub fn open_query_connection(
        &self,
        path: &Path,
        flags: OpenFlags,
    ) -> Result<QueryConnection<'_>, QueryError> {
        let mut conn = Connection::open_with_flags(path, flags).map_err(|e| self.translate(e))?;
        self.configure(&mut conn).map_err(|e| self.translate(e))?;
        Ok(QueryConnection {
            conn,
            _scope: PhantomData,
        })
    }

    /// Classify a SQLite error raised inside this request: `SQLITE_NOMEM` or `SQLITE_INTERRUPT`
    /// become [`QueryError::Limit`] only when this budget actually exceeded a limit; every other
    /// error, and a genuine out-of-memory or interrupt, is kept as [`QueryError::Sqlite`].
    #[must_use]
    pub fn translate(&self, error: rusqlite::Error) -> QueryError {
        match error.sqlite_error_code() {
            Some(ErrorCode::OutOfMemory | ErrorCode::OperationInterrupted) => {
                match self.budget.check() {
                    Err(exceeded) => QueryError::Limit(exceeded),
                    Ok(()) => QueryError::Sqlite(error),
                }
            }
            _ => QueryError::Sqlite(error),
        }
    }

    /// Lookaside off, worker threads off, and a progress handler that interrupts a statement
    /// once the budget's deadline passed or the request was cancelled (or a limit was recorded).
    fn configure(&self, conn: &mut Connection) -> rusqlite::Result<()> {
        disable_lookaside(conn)?;
        conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_WORKER_THREADS, 0)?;
        let budget = self.budget.clone();
        conn.progress_handler(PROGRESS_STEPS, Some(move || budget.check().is_err()))
    }
}

/// Virtual-machine steps between two deadline checks of a running statement.
const PROGRESS_STEPS: c_int = 1000;

/// Free the connection's lookaside buffer and stop using one, so every allocation the connection
/// makes goes through SQLite's allocator, where the scope charges it.
fn disable_lookaside(conn: &Connection) -> rusqlite::Result<()> {
    let (slot_size, slots): (c_int, c_int) = (0, 0);
    // SAFETY: the handle is the live connection's and is used on this thread only for the call.
    // `SQLITE_DBCONFIG_LOOKASIDE` takes exactly (void *buffer, int slot size, int slot
    // count); a null buffer with zero size and count disables lookaside, and SQLite refuses
    // (SQLITE_BUSY) rather than free slots still in use.
    let rc = unsafe {
        ffi::sqlite3_db_config(
            conn.handle(),
            ffi::SQLITE_DBCONFIG_LOOKASIDE,
            ptr::null_mut::<c_void>(),
            slot_size,
            slots,
        )
    };
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(rc),
            Some("disabling lookaside on the query connection".to_owned()),
        ))
    }
}

/// A request's dedicated query connection; it cannot outlive its [`RequestScope`].
#[derive(Debug)]
pub struct QueryConnection<'scope> {
    conn: Connection,
    _scope: PhantomData<&'scope RequestScope>,
}

impl QueryConnection<'_> {
    /// Close the connection inside the scope, reporting a failure to close.
    ///
    /// # Errors
    /// The SQLite error from closing; the connection is closed (or leaked by SQLite) either way.
    pub fn close(self) -> rusqlite::Result<()> {
        self.conn.close().map_err(|(_, error)| error)
    }
}

impl Deref for QueryConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

/// A request scope was entered on a thread that already has one active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeActive;

impl fmt::Display for ScopeActive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a request budget scope is already active on this thread; scopes do not nest")
    }
}

impl std::error::Error for ScopeActive {}

/// Why a request's query failed.
#[derive(Debug)]
pub enum QueryError {
    /// The request's budget exceeded a configured limit.
    Limit(BudgetExceeded),
    /// SQLite failed for a reason other than the budget.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(BudgetExceeded {
                limit: Limit::Memory,
                value,
            }) => write!(
                f,
                "query refused: memory limit exceeded ({value} bytes would be live)"
            ),
            Self::Limit(BudgetExceeded {
                limit: Limit::Time,
                value,
            }) => write!(f, "query refused: time limit exceeded after {value} ms"),
            Self::Limit(BudgetExceeded {
                limit: Limit::Cancelled,
                value,
            }) => write!(f, "query refused: cancelled after {value} ms"),
            Self::Sqlite(error) => write!(f, "query failed: {error}"),
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Limit(exceeded) => Some(exceeded),
            Self::Sqlite(error) => Some(error),
        }
    }
}
