//! A request scope charges its dedicated query connection, from open to close, and every
//! table-function scan inside it to the request's budget, and names the limit a query exceeded
//! (spec 11 T12, QRY-04, QRY-06, PC-17). Each case runs in a fresh process with the allocator
//! adapter installed.
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;
mod fresh;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use common::{Range, Reserving, integers};
use fresh::{Block, fresh_test, initialize_sqlite, installed_runtime};
use kaban_core::query_budget::{BudgetExceeded, Limit, QueryBudget};
use kaban_vtab::{QueryError, RequestScope, ScopeActive, register};
use rusqlite::{OpenFlags, ffi};

/// Install the adapter and initialize SQLite outside any scope, so its process-wide state is
/// never charged to a request.
fn setup() -> kaban_vtab::Runtime {
    let runtime = installed_runtime();
    initialize_sqlite();
    runtime
}

fn memory() -> &'static Path {
    Path::new(":memory:")
}

fn query_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI
}

/// A statement that builds one large string: about 1 MB of live SQLite memory.
const LARGE: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 10000) \
                     SELECT length(group_concat(printf('%0100d', x))) FROM c";

/// A statement that does not end on its own within any test's patience; every run of it goes
/// through [`bounded`], whose watchdog interrupts it.
const ENDLESS: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                       SELECT count(*) FROM c";

fn single(conn: &rusqlite::Connection, sql: &str) -> rusqlite::Result<i64> {
    conn.query_row(sql, [], |row| row.get(0))
}

/// How long a statement waiting for an interrupt may run before the watchdog stops it.
const WATCHDOG: Duration = Duration::from_secs(5);

/// Run [`ENDLESS`] and return its error. A watchdog interrupts it after [`WATCHDOG`], and then
/// this fails the test: whatever was meant to interrupt it never did. The wait never hangs.
fn bounded(conn: &rusqlite::Connection) -> rusqlite::Error {
    let handle = conn.get_interrupt_handle();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let fired = done_rx.recv_timeout(WATCHDOG).is_err();
        if fired {
            handle.interrupt();
        }
        fired
    });
    let outcome = single(conn, ENDLESS);
    let _ = done_tx.send(()); // the watchdog may have fired and gone
    let fired = watchdog.join().unwrap();
    assert!(
        !fired,
        "the statement ran {WATCHDOG:?} uninterrupted: {outcome:?}"
    );
    outcome.unwrap_err()
}

fresh_test! {
    fn a_request_charges_open_to_close_and_releases_everything_after_close() {
        let runtime = setup();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        let opened = budget.live_bytes();
        assert!(opened > 0, "opening the query connection charged nothing");
        assert_eq!(single(&conn, LARGE).unwrap(), 1_009_999);
        conn.close().unwrap();
        assert_eq!(budget.live_bytes(), 0, "closing left charged bytes behind");
        drop(scope);

        // A later request pays for its own connection again: nothing cached was handed over.
        let later = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &later).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        assert_eq!(later.live_bytes(), opened);
        drop(conn);
        assert_eq!(later.live_bytes(), 0);
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn the_query_connection_has_lookaside_and_worker_threads_off() {
        let runtime = setup();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        assert_eq!(single(&conn, LARGE).unwrap(), 1_009_999);
        let (mut current, mut highwater) = (-1, -1);
        // SAFETY: the handle is the live connection's, used on its own thread; the status call
        // writes two ints through valid pointers and changes nothing.
        let rc = unsafe {
            ffi::sqlite3_db_status(
                conn.handle(),
                ffi::SQLITE_DBSTATUS_LOOKASIDE_USED,
                &raw mut current,
                &raw mut highwater,
                0,
            )
        };
        assert_eq!(rc, ffi::SQLITE_OK);
        assert_eq!((current, highwater), (0, 0), "lookaside slots were used");
        assert_eq!(
            conn.limit(rusqlite::limits::Limit::SQLITE_LIMIT_WORKER_THREADS).unwrap(),
            0
        );
    }
}

fresh_test! {
    fn a_query_past_the_memory_limit_fails_with_the_named_limit() {
        let runtime = setup();
        let budget = QueryBudget::new(256 * 1024, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        let error = scope.translate(single(&conn, LARGE).unwrap_err());
        assert!(
            matches!(
                error,
                QueryError::Limit(BudgetExceeded {
                    limit: Limit::Memory,
                    ..
                })
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("memory limit"), "{error}");
        drop(conn);
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn a_query_past_the_deadline_is_interrupted_with_the_named_limit() {
        let runtime = setup();
        let budget = QueryBudget::new(u64::MAX, Some(Duration::from_millis(50)));
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        let error = scope.translate(bounded(&conn));
        assert!(
            matches!(
                error,
                QueryError::Limit(BudgetExceeded {
                    limit: Limit::Time,
                    ..
                })
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("time limit"), "{error}");
    }
}

fresh_test! {
    fn a_cancelled_request_is_interrupted_as_cancelled() {
        let runtime = setup();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        let canceller = budget.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            canceller.cancel();
        });
        let error = scope.translate(bounded(&conn));
        timer.join().unwrap();
        assert!(
            matches!(
                error,
                QueryError::Limit(BudgetExceeded {
                    limit: Limit::Cancelled,
                    ..
                })
            ),
            "{error:?}"
        );
    }
}

fresh_test! {
    fn a_genuine_sqlite_failure_is_not_reported_as_a_limit() {
        let runtime = setup();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        let error = scope.translate(single(&conn, "SELEC 1").unwrap_err());
        assert!(matches!(error, QueryError::Sqlite(_)), "{error:?}");
        // An interrupt nobody's budget caused stays an interrupt.
        let handle = conn.get_interrupt_handle();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            handle.interrupt();
        });
        let error = scope.translate(bounded(&conn));
        timer.join().unwrap();
        assert!(
            matches!(&error, QueryError::Sqlite(e)
                if e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)),
            "{error:?}"
        );
    }
}

fresh_test! {
    fn a_table_function_reserving_past_the_scope_limit_is_refused_naming_the_limit() {
        let runtime = setup();
        let budget = QueryBudget::new(512 * 1024, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
        register(&conn, Arc::new(Reserving)).unwrap();

        // Within the limit, the scan's budget is the scope's: the reservation shows up in it.
        let before = budget.live_bytes();
        let live = integers(&conn, "SELECT live FROM reserving(1000)").unwrap();
        assert!(live[0] >= i64::try_from(before + 1000).unwrap(), "{live:?} vs {before}");

        let error = integers(&conn, "SELECT live FROM reserving(1000000)").unwrap_err();
        assert!(error.contains("reserving") && error.contains("Memory"), "{error}");
    }
}

#[test]
fn a_table_function_outside_a_request_scope_is_refused_by_name() {
    let conn = common::connection(vec![Arc::new(Range)]);
    let error = integers(&conn, "SELECT value FROM range(1, 4)").unwrap_err();
    assert!(
        error.contains("range") && error.contains("outside a request budget scope"),
        "{error}"
    );
}

fresh_test! {
    fn an_unwinding_request_restores_the_thread_and_releases_its_connection() {
        let runtime = setup();
        let budget = QueryBudget::new(u64::MAX, None);
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            let scope = RequestScope::enter(runtime, &budget).unwrap();
            let conn = scope.open_query_connection(memory(), query_flags()).unwrap();
            assert!(budget.live_bytes() > 0);
            let _ = &conn;
            panic!("request cancelled by unwinding");
        }));
        assert!(unwound.is_err());
        assert_eq!(budget.live_bytes(), 0, "the unwound connection kept its charge");
        // The thread is unscoped again: a new block is not charged to the old budget.
        let block = Block::malloc(64).unwrap();
        assert_eq!(budget.live_bytes(), 0);
        block.free();
    }
}

fresh_test! {
    fn a_block_outliving_its_request_keeps_its_charge_until_freed() {
        let runtime = setup();
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let block = Block::malloc(1000).unwrap();
        drop(scope);
        let held = budget.live_bytes();
        assert!(held >= 1000);
        std::thread::spawn(move || block.free()).join().unwrap();
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn a_second_scope_on_the_thread_is_refused_and_the_first_keeps_charging() {
        let runtime = setup();
        let a = QueryBudget::new(u64::MAX, None);
        let scope_a = RequestScope::enter(runtime, &a).unwrap();
        let conn_a = scope_a.open_query_connection(memory(), query_flags()).unwrap();

        let b = QueryBudget::new(u64::MAX, None);
        let refused = RequestScope::enter(runtime, &b);
        assert_eq!(refused.as_ref().err(), Some(&ScopeActive), "a nested scope was entered");
        assert!(refused.unwrap_err().to_string().contains("already active"));

        // Connection A runs, and opens, under A's budget only.
        let before = a.live_bytes();
        let mut stmt = conn_a.prepare(LARGE).unwrap();
        assert!(a.live_bytes() > before, "preparing on A charged nothing to A");
        assert_eq!(stmt.query_row([], |row| row.get::<_, i64>(0)).unwrap(), 1_009_999);
        drop(stmt);
        let second = scope_a.open_query_connection(memory(), query_flags()).unwrap();
        assert_eq!(b.live_bytes(), 0, "B was charged while it was refused");
        drop(second);
        drop(conn_a);
        assert_eq!(a.live_bytes(), 0);
        drop(scope_a);

        // With A gone the thread takes a new scope.
        let scope_b = RequestScope::enter(runtime, &b).unwrap();
        let block = Block::malloc(64).unwrap();
        assert!(b.live_bytes() > 0);
        assert_eq!(a.live_bytes(), 0);
        block.free();
        drop(scope_b);
    }
}

fresh_test! {
    fn the_thread_takes_a_new_scope_after_an_unwound_one() {
        let runtime = setup();
        let a = QueryBudget::new(u64::MAX, None);
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            let _scope = RequestScope::enter(runtime, &a).unwrap();
            panic!("request unwinding");
        }));
        assert!(unwound.is_err());
        let b = QueryBudget::new(u64::MAX, None);
        let scope_b = RequestScope::enter(runtime, &b).unwrap();
        let block = Block::malloc(64).unwrap();
        assert!(b.live_bytes() > 0);
        assert_eq!(a.live_bytes(), 0);
        block.free();
        drop(scope_b);
    }
}
