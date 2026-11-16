//! The embedding initializer installs the allocator adapter once, before any SQLite use, and
//! refuses every other order by name (spec 11 T12 allocator RED/GREEN, QRY-07). Each case runs
//! in a fresh process.
#![allow(clippy::unwrap_used)]
// The startup bootstrap installs before main, so the embedding initializer's own cases cannot run
// with `process-bootstrap` on; tests/bootstrap.rs covers that build.
#![cfg(not(feature = "process-bootstrap"))]

mod common;
mod fresh;

use std::sync::Arc;

use fresh::{Block, configured_allocator, fresh_test};
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{InitError, RequestScope, Runtime, initialize_for_embedding};
use rusqlite::{Connection, ffi};

fresh_test! {
    fn installation_before_any_sqlite_entry_installs_the_wrapper() {
        let original = configured_allocator().unwrap();
        assert_eq!(Runtime::get(), None);

        // SAFETY: fresh process, single thread, no SQLite entry yet; nothing replaces the
        // allocator or shuts SQLite down afterwards.
        let runtime = unsafe { initialize_for_embedding() }.unwrap();

        let installed = configured_allocator().unwrap();
        assert_ne!(installed.xMalloc.map(|f| f as usize), original.xMalloc.map(|f| f as usize));
        assert_ne!(installed.xFree.map(|f| f as usize), original.xFree.map(|f| f as usize));
        assert_eq!(Runtime::get(), Some(runtime));

        // A scoped SQLite allocation is charged to the scope's budget.
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        assert!(budget.live_bytes() > 0, "opening a connection charged nothing");
        drop(conn);
        drop(scope);
    }
}

fresh_test! {
    fn repeated_token_access_returns_the_same_runtime() {
        // SAFETY: fresh process, single thread, no SQLite entry yet.
        let runtime = unsafe { initialize_for_embedding() }.unwrap();
        assert_eq!(Runtime::get(), Some(runtime));
        assert_eq!(Runtime::get(), Some(runtime));
        let other_thread = std::thread::spawn(Runtime::get).join().unwrap();
        assert_eq!(other_thread, Some(runtime));
    }
}

fresh_test! {
    fn a_host_that_used_sqlite_first_is_refused_by_name_and_left_running() {
        let host = Connection::open_in_memory().unwrap();

        // SAFETY: single thread; the host's SQLite use is exactly what the initializer must
        // detect and refuse without touching SQLite's state.
        let refused = unsafe { initialize_for_embedding() };

        assert_eq!(refused, Err(InitError::HostInitializedSqlite));
        let message = refused.unwrap_err().to_string();
        assert!(message.contains("initialized by the host"), "{message}");
        assert_eq!(Runtime::get(), None);
        // SQLite was not shut down: the host's connection and a new one still work, and the
        // original allocator is still the configured one (no reconfiguration happened).
        let one: i64 = host.query_row("SELECT 1", [], |row| row.get(0)).unwrap();
        assert_eq!(one, 1);
        let again = Connection::open_in_memory().unwrap();
        let two: i64 = again.query_row("SELECT 2", [], |row| row.get(0)).unwrap();
        assert_eq!(two, 2);
        assert_eq!(configured_allocator().err(), Some(ffi::SQLITE_MISUSE));
        // No adapter is installed, so no request scope can be entered (it needs the runtime).
        let block = Block::malloc(64).unwrap();
        block.free();
    }
}

fresh_test! {
    fn a_second_initialization_is_refused_by_name() {
        // SAFETY: fresh process, single thread, no SQLite entry yet.
        let runtime = unsafe { initialize_for_embedding() }.unwrap();

        // SAFETY: as above; the call must be refused before it touches SQLite.
        let again = unsafe { initialize_for_embedding() };
        assert_eq!(again, Err(InitError::AlreadyInitialized));
        assert!(again.unwrap_err().to_string().contains("already initialized"));
        // SAFETY: as above, from another kaban thread.
        let from_thread = std::thread::spawn(|| unsafe { initialize_for_embedding() })
            .join()
            .unwrap();
        assert_eq!(from_thread, Err(InitError::AlreadyInitialized));
        assert_eq!(Runtime::get(), Some(runtime));
    }
}

fresh_test! {
    fn a_failed_initialization_is_not_retried() {
        let _host = Connection::open_in_memory().unwrap();
        // SAFETY: single thread; the first call is refused (host initialized SQLite).
        assert_eq!(unsafe { initialize_for_embedding() }, Err(InitError::HostInitializedSqlite));
        // SAFETY: as above.
        assert_eq!(unsafe { initialize_for_embedding() }, Err(InitError::AlreadyInitialized));
        assert_eq!(Runtime::get(), None);
    }
}

fresh_test! {
    fn registration_without_the_installed_runtime_is_refused_by_name() {
        let conn = Connection::open_in_memory().unwrap();
        let error = kaban_vtab::register(&conn, Arc::new(common::Range)).unwrap_err().to_string();
        assert!(
            error.contains("range") && error.contains("runtime"),
            "registration must name the function and the missing runtime: {error}"
        );
        assert_eq!(Runtime::get(), None);
    }
}
