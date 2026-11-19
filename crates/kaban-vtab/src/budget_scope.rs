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
use std::time::{Duration, Instant};

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
    /// The connection always uses a private cache (`SQLITE_OPEN_PRIVATECACHE` is added): a
    /// shared cache could hand it schema and pages another connection allocated, uncharged or
    /// charged to an earlier request.
    ///
    /// # Errors
    /// [`QueryError::SharedCache`] when `flags` hold `SQLITE_OPEN_SHARED_CACHE` or `path` is a
    /// `file:` URI with a `cache` parameter other than `private`; otherwise a [`QueryError`] when
    /// SQLite cannot open or configure the connection, naming the limit when the budget caused it.
    pub fn open_query_connection(
        &self,
        path: &Path,
        flags: OpenFlags,
    ) -> Result<QueryConnection<'_>, QueryError> {
        if flags.contains(OpenFlags::SQLITE_OPEN_SHARED_CACHE) || asks_shared_cache(path) {
            return Err(QueryError::SharedCache);
        }
        let flags = flags | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
        let mut conn = Connection::open_with_flags(path, flags).map_err(|e| self.translate(e))?;
        self.configure(&mut conn).map_err(|e| self.translate(e))?;
        Ok(QueryConnection {
            conn,
            _scope: PhantomData,
        })
    }

    /// Classify a SQLite error raised inside this request: `SQLITE_NOMEM`, `SQLITE_INTERRUPT` or
    /// `SQLITE_BUSY` become [`QueryError::Limit`] only when this budget actually exceeded a limit
    /// (the busy handler gives up a lock wait once it has); every other error, and a genuine
    /// out-of-memory, interrupt or busy, is kept as [`QueryError::Sqlite`].
    #[must_use]
    pub fn translate(&self, error: rusqlite::Error) -> QueryError {
        match error.sqlite_error_code() {
            Some(
                ErrorCode::OutOfMemory | ErrorCode::OperationInterrupted | ErrorCode::DatabaseBusy,
            ) => match self.budget.check() {
                Err(exceeded) => QueryError::Limit(exceeded),
                Ok(()) => QueryError::Sqlite(error),
            },
            _ => QueryError::Sqlite(error),
        }
    }

    /// Lookaside off, worker threads off, a progress handler that interrupts a statement once
    /// the budget's deadline passed or the request was cancelled (or a limit was recorded), and a
    /// busy handler in place of rusqlite's default 5 s busy timeout: a lock wait retries every
    /// [`BUSY_PAUSE`] only while the budget holds, for at most [`BUSY_WAIT`].
    fn configure(&self, conn: &mut Connection) -> rusqlite::Result<()> {
        disable_lookaside(conn)?;
        conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_WORKER_THREADS, 0)?;
        let budget = self.budget.clone();
        conn.progress_handler(PROGRESS_STEPS, Some(move || budget.check().is_err()))?;
        let budget = self.budget.clone();
        let mut waiting_since = Instant::now();
        conn.busy_handler(Some(move |attempt: i32| {
            if attempt == 0 {
                waiting_since = Instant::now();
            }
            if budget.check().is_err() || waiting_since.elapsed() >= BUSY_WAIT {
                return false;
            }
            std::thread::sleep(BUSY_PAUSE);
            true
        }))
    }
}

/// The longest one lock wait lasts under a healthy budget before SQLite reports `SQLITE_BUSY`.
const BUSY_WAIT: Duration = Duration::from_secs(1);

/// The pause between two attempts to take a lock; it bounds how late a deadline or a
/// cancellation ends a lock wait.
const BUSY_PAUSE: Duration = Duration::from_millis(5);

/// Whether `path`, read as SQLite reads a `file:` URI (this build interprets them whatever the
/// open flags say), carries a `cache` query parameter other than `private`. Names and values are
/// percent-decoded first, as SQLite decodes them. Overrides the open flags, so it is refused.
fn asks_shared_cache(path: &Path) -> bool {
    let path = path.to_string_lossy();
    let Some(scheme) = path.get(..5) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("file:") {
        return false;
    }
    let Some((_, query)) = path.split_once('?') else {
        return false;
    };
    let query = query.split_once('#').map_or(query, |(query, _)| query);
    query.split('&').any(|parameter| {
        let (name, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        percent_decoded(name) == b"cache" && percent_decoded(value) != b"private"
    })
}

/// `text` with every `%XX` escape decoded, as SQLite decodes URI components.
fn percent_decoded(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escape = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(byte) = escape {
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    decoded
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
    /// The query connection was asked to share SQLite's cache with other connections
    /// (`SQLITE_OPEN_SHARED_CACHE`, or `cache=` other than `private` in a `file:` URI); a
    /// request's connection keeps a private cache, charged to it and closed with it.
    SharedCache,
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
            Self::SharedCache => f.write_str(
                "query refused: the query connection must use a private cache, not a shared one",
            ),
            Self::Sqlite(error) => write!(f, "query failed: {error}"),
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Limit(exceeded) => Some(exceeded),
            Self::SharedCache => None,
            Self::Sqlite(error) => Some(error),
        }
    }
}
