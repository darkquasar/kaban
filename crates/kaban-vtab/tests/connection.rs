//! The dedicated query connection keeps its own cache and never waits for a lock outside the
//! request's budget (spec 11 T12, QRY-04, QRY-06, MED-03, MED-04). Each case runs in a fresh
//! process with the allocator adapter installed. Every lock wait here is bounded by SQLite's own
//! busy policy (at most rusqlite's 5 s default before the fix) and by the child's time limit.
#![allow(clippy::unwrap_used, clippy::panic)] // a wrong outcome fails the case

mod fresh;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fresh::{fresh_test, initialize_sqlite, installed_runtime, query_flags};
use kaban_core::query_budget::{BudgetExceeded, Limit, QueryBudget};
use kaban_vtab::{QueryError, RequestScope};
use rusqlite::{Connection, ErrorCode, OpenFlags};

/// A database file of this case, removed first and when the guard drops.
struct TempDb(PathBuf);

impl TempDb {
    fn new(case: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("kaban-vtab-{}-{case}.db", std::process::id()));
        let db = Self(path);
        db.remove();
        db
    }

    fn remove(&self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("db-journal"));
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

fn single(conn: &Connection, sql: &str) -> rusqlite::Result<i64> {
    conn.query_row(sql, [], |row| row.get(0))
}

fn assert_shared_cache_refused(outcome: Result<kaban_vtab::QueryConnection<'_>, QueryError>) {
    match outcome {
        Err(error @ QueryError::SharedCache) => {
            assert!(error.to_string().contains("private cache"), "{error}");
        }
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("a shared-cache query connection was opened"),
    }
}

fresh_test! {
    fn a_shared_cache_query_connection_is_refused_by_name() {
        let runtime = installed_runtime();
        initialize_sqlite();
        let db = TempDb::new("shared");
        let uri = format!("file:{}?cache=shared", db.0.display());
        // An unscoped shared-cache connection to the same database, held open.
        let holder = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        holder.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);").unwrap();

        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let flag = query_flags() | OpenFlags::SQLITE_OPEN_SHARED_CACHE;
        assert_shared_cache_refused(scope.open_query_connection(&db.0, flag));
        assert_shared_cache_refused(scope.open_query_connection(Path::new(&uri), query_flags()));
        let encoded = format!("file:{}?mode=rwc&%63ache=shar%65d", db.0.display());
        assert_shared_cache_refused(scope.open_query_connection(Path::new(&encoded), query_flags()));

        // A private connection to the same file opens and reads it.
        let private = scope.open_query_connection(&db.0, query_flags()).unwrap();
        assert_eq!(single(&private, "SELECT x FROM t").unwrap(), 1);
        drop(private);
        drop(scope);
        drop(holder);
    }
}

/// A database whose other connection, unscoped, holds an exclusive lock until it drops.
fn locked(case: &str) -> (TempDb, Connection) {
    let db = TempDb::new(case);
    let holder = Connection::open(&db.0).unwrap();
    holder
        .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1); BEGIN EXCLUSIVE;")
        .unwrap();
    (db, holder)
}

/// Read `t` through a query connection under `budget` while the lock is held; return the
/// translated error and how long the statement waited.
fn blocked_read(
    runtime: kaban_vtab::Runtime,
    db: &TempDb,
    budget: &QueryBudget,
) -> (QueryError, Duration) {
    let scope = RequestScope::enter(runtime, budget).unwrap();
    let conn = scope.open_query_connection(&db.0, query_flags()).unwrap();
    let started = Instant::now();
    let error = single(&conn, "SELECT count(*) FROM t").unwrap_err();
    let waited = started.elapsed();
    (scope.translate(error), waited)
}

/// Well under rusqlite's default 5 s busy timeout.
const PROMPT: Duration = Duration::from_secs(2);

fresh_test! {
    fn a_deadline_during_a_lock_wait_ends_promptly_as_the_time_limit() {
        let runtime = installed_runtime();
        initialize_sqlite();
        let (db, _holder) = locked("deadline");
        let budget = QueryBudget::new(u64::MAX, Some(Duration::from_millis(100)));
        let (error, waited) = blocked_read(runtime, &db, &budget);
        assert!(waited < PROMPT, "the lock wait outlived the deadline: {waited:?}");
        assert!(
            matches!(error, QueryError::Limit(BudgetExceeded { limit: Limit::Time, .. })),
            "{error:?}"
        );
    }
}

fresh_test! {
    fn a_cancellation_during_a_lock_wait_ends_promptly_as_cancelled() {
        let runtime = installed_runtime();
        initialize_sqlite();
        let (db, _holder) = locked("cancel");
        let budget = QueryBudget::new(u64::MAX, None);
        let canceller = budget.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        });
        let (error, waited) = blocked_read(runtime, &db, &budget);
        timer.join().unwrap();
        assert!(waited < PROMPT, "the lock wait outlived the cancellation: {waited:?}");
        assert!(
            matches!(error, QueryError::Limit(BudgetExceeded { limit: Limit::Cancelled, .. })),
            "{error:?}"
        );
    }
}

fresh_test! {
    fn a_lock_held_past_a_bounded_wait_with_a_healthy_budget_stays_busy() {
        let runtime = installed_runtime();
        initialize_sqlite();
        let (db, _holder) = locked("busy");
        let budget = QueryBudget::new(u64::MAX, None);
        let (error, waited) = blocked_read(runtime, &db, &budget);
        assert!(waited < Duration::from_secs(3), "the bounded lock wait ran {waited:?}");
        assert!(
            matches!(&error, QueryError::Sqlite(e) if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)),
            "{error:?}"
        );
    }
}
