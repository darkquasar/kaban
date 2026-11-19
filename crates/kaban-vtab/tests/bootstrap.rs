//! With `process-bootstrap` on, the startup constructor installs the allocator adapter before the
//! test harness's `main`, without any call (spec 11 T12, QRY-07). Each case runs in a fresh
//! process.
#![cfg(feature = "process-bootstrap")]
#![allow(clippy::unwrap_used)]

mod fresh;

use fresh::fresh_test;
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{InitError, RequestScope, Runtime, initialize_for_embedding};
use rusqlite::OpenFlags;

fresh_test! {
    fn the_runtime_is_installed_before_the_test_body_without_any_call() {
        let runtime = Runtime::get();
        assert!(runtime.is_some(), "the startup constructor installed no runtime");
        let runtime = runtime.unwrap();
        // SAFETY: the call must be refused before it touches SQLite.
        assert_eq!(unsafe { initialize_for_embedding() }, Err(InitError::AlreadyInitialized));
        assert_eq!(Runtime::get(), Some(runtime));
        // The installed wrapper charges a scoped connection.
        let budget = QueryBudget::new(u64::MAX, None);
        let scope = RequestScope::enter(runtime, &budget).unwrap();
        let conn = scope
            .open_query_connection(
                std::path::Path::new(":memory:"),
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            )
            .unwrap();
        assert!(budget.live_bytes() > 0, "the bootstrap installed no charging wrapper");
        drop(conn);
        assert_eq!(budget.live_bytes(), 0);
    }
}
