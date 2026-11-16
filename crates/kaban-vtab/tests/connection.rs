//! The dedicated query connection keeps its own cache and never waits for a lock outside the
//! request's budget (spec 11 T12, QRY-04, QRY-06, MED-03, MED-04). Each case runs in a fresh
//! process with the allocator adapter installed. Every lock wait here is bounded by SQLite's own
//! busy policy (at most rusqlite's 5 s default before the fix) and by the child's time limit.
#![allow(clippy::unwrap_used)]

mod fresh;

use std::path::{Path, PathBuf};

use fresh::{fresh_test, initialize_sqlite, installed_runtime, query_flags};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{QueryError, RequestScope};
use rusqlite::{Connection, OpenFlags};

/// A database file of this case, removed first and when the guard drops.
struct TempDb(PathBuf);

impl TempDb {
    fn new(case: &str) -> Self {
        let path = std::env::temp_dir().join(format!("kaban-vtab-{}-{case}.db", std::process::id()));
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
