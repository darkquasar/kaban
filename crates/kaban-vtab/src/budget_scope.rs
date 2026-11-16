//! Request-scoped query budgets over the allocator adapter (spec 11 QRY-04, QRY-06, PC-17).
//!
//! A [`RequestScope`] charges every SQLite allocation its thread makes to one [`QueryBudget`]
//! until it drops. [`RequestScope::open_query_connection`] opens the request's dedicated
//! connection inside the scope; the [`QueryConnection`] borrows the scope, so opening,
//! preparing, stepping, finalizing and closing all happen while the scope is active, and the
//! connection (with every cache it holds) is closed before the request ends. Writers that never
//! enter a scope keep SQLite's original allocation behaviour. Nothing here changes a
//! process-global SQLite limit.

use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;

use kaban_core::query_budget::{BudgetExceeded, Limit, QueryBudget};
use rusqlite::{Connection, OpenFlags};

use crate::Runtime;
use crate::allocator::{self, ScopeGuard};

/// While alive, SQLite allocations on this thread are charged to the request's budget.
///
/// Dropping it, including during an unwind, restores the scope that was active before; blocks
/// charged inside it keep their budget until they are freed, on any thread, after the request.
/// Scopes on one thread nest and must drop in reverse order of creation. It is bound to the
/// thread that entered it.
#[derive(Debug)]
pub struct RequestScope {
    budget: QueryBudget,
    _guard: ScopeGuard,
    _runtime: Runtime,
}

impl RequestScope {
    /// Charge this thread's SQLite allocations, and every table-function scan it runs, to
    /// `budget` until the scope drops. The [`Runtime`] proves the allocator adapter is installed.
    #[must_use]
    pub fn enter(runtime: Runtime, budget: &QueryBudget) -> Self {
        Self {
            budget: budget.clone(),
            _guard: allocator::enter_scope(budget),
            _runtime: runtime,
        }
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
        let conn = Connection::open_with_flags(path, flags).map_err(|e| self.translate(e))?;
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
        QueryError::Sqlite(error)
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
