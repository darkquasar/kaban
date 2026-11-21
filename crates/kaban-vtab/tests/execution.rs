//! A statement's read callbacks share its connection, payload and budget.

#![allow(clippy::unwrap_used, clippy::panic)] // test assertions
mod fresh;

use fresh::fresh_test;
use kaban_core::query_budget::QueryBudget;
use kaban_vtab::{
    ColumnDef, ColumnKind, Execution, ExecutionPayload, FnError, Plan, RequestScope, Scan, Value,
    register_execution, with_execution,
};
use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, Authorization};
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const COLS: &[ColumnDef] = &[ColumnDef {
    name: "value",
    kind: ColumnKind::Integer,
    argument: None,
}];

struct PointLookup<'a> {
    connection: &'a Connection,
    key: i64,
    owner: &'a QueryBudget,
    reading_fact: &'a AtomicBool,
}

struct Reading<'a>(&'a AtomicBool);
impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

fn assert_direct_read_denied(conn: &Connection) {
    let error = conn.prepare("SELECT value FROM facts").unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::AuthorizationForStatementDenied)
    );
}

impl PointLookup<'_> {
    // Only this fixed, uncached statement can use the store's private read permission.
    fn value(&self, key: i64) -> rusqlite::Result<i64> {
        self.reading_fact.store(true, Ordering::Relaxed);
        let _reading = Reading(self.reading_fact);
        self.connection
            .query_row("SELECT value FROM facts WHERE id = ?1", [key], |r| r.get(0))
    }
}

impl ExecutionPayload for PointLookup<'_> {
    type Cursor = std::ops::Range<i64>;

    fn open(
        &self,
        function: &'static str,
        _: &Scan<'_>,
        _: &QueryBudget,
    ) -> Result<Self::Cursor, FnError> {
        assert_eq!(function, "points");
        Ok(self.key..self.key + 2)
    }

    fn next(
        &self,
        cursor: &mut Self::Cursor,
        budget: &QueryBudget,
    ) -> Option<Result<Vec<Value>, FnError>> {
        let key = cursor.next()?;
        let before = self.owner.live_bytes();
        let _lease = budget.reserve(64).unwrap();
        assert_eq!(self.owner.live_bytes(), before + 64);
        Some(
            self.value(key)
                .map(|v| vec![Value::Integer(v)])
                .map_err(|error| FnError {
                    construct: "points".into(),
                    reason: error.to_string(),
                }),
        )
    }
}

fresh_test! {
    fn planning_and_rows_use_the_statement_binding() {
        let budget = QueryBudget::new(1 << 22, None);
        let scope = RequestScope::enter(fresh::installed_runtime(), &budget).unwrap();
        let mut conn = scope
            .open_query_connection(Path::new(":memory:"), fresh::query_flags())
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE facts(id INTEGER PRIMARY KEY, value INTEGER);
                            INSERT INTO facts VALUES (1, 41)",
        )
        .unwrap();
        register_execution(&conn, "points", COLS).unwrap();
        let reading_fact = Arc::new(AtomicBool::new(false));
        let allowed = Arc::clone(&reading_fact);
        conn.set_authorizer(move |ctx| match ctx.action {
            AuthAction::Read {
                table_name: "facts",
                column_name,
            } if !(allowed.load(Ordering::Relaxed)
                && ctx.database_name == Some("main")
                && matches!(column_name, "id" | "value")) =>
            {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        })
        .unwrap();
        let estimates = [2_i64];
        let statistics_read = Cell::new(false);
        let planner = |name: &'static str, _: &kaban_vtab::PlanInput<'_>, _: &QueryBudget| {
            assert_eq!(name, "points");
            statistics_read.set(true);
            Ok(Plan {
                rows: estimates[0],
                cost: 2.0,
                ..Plan::default()
            })
        };
        let payload = PointLookup {
            connection: &conn,
            key: 1,
            owner: &budget,
            reading_fact: &reading_fact,
        };
        let transaction = conn.unchecked_transaction().unwrap();
        transaction
            .execute("INSERT INTO facts VALUES (2, 73)", [])
            .unwrap();
        let result = with_execution(
            &conn,
            &Execution {
                plan: &planner,
                payload: &payload,
            },
            || {
                let _ = conn.prepare("EXPLAIN QUERY PLAN SELECT value FROM points")?;
                assert!(statistics_read.get());
                assert_direct_read_denied(&conn);
                conn.prepare("SELECT value FROM points")?
                    .query_map([], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            },
        );
        assert_eq!(
            result
                .map_err(|e| e.to_string())
                .and_then(|rows| rows.map_err(|e| e.to_string())),
            Ok(vec![41, 73])
        );
        assert!(!reading_fact.load(Ordering::Relaxed));
        assert_direct_read_denied(&conn);
        transaction.commit().unwrap();
        conn.close().unwrap();
        drop(scope);
        assert_eq!(budget.live_bytes(), 0);
    }
}

struct LargeCursor<'a>(&'a Cell<bool>);

impl ExecutionPayload for LargeCursor<'_> {
    type Cursor = [u8; 64 * 1024];

    #[expect(
        clippy::large_stack_arrays,
        reason = "this oversized inline cursor must be refused before construction"
    )]
    fn open(
        &self,
        _: &'static str,
        _: &Scan<'_>,
        _: &QueryBudget,
    ) -> Result<Self::Cursor, FnError> {
        self.0.set(true);
        Ok([0; 64 * 1024])
    }

    fn next(&self, _: &mut Self::Cursor, _: &QueryBudget) -> Option<Result<Vec<Value>, FnError>> {
        None
    }
}

fresh_test! {
    fn the_owned_cursor_is_charged_before_the_payload_constructs_it() {
        let limit = 1 << 20;
        let budget = QueryBudget::new(limit, None);
        let scope = RequestScope::enter(fresh::installed_runtime(), &budget).unwrap();
        let conn = scope.open_query_connection(Path::new(":memory:"), fresh::query_flags()).unwrap();
        register_execution(&conn, "points", COLS).unwrap();
        let opened = Cell::new(false);
        let execution = Execution {
            plan: &|_, _, _| Ok(Plan::default()),
            payload: &LargeCursor(&opened),
        };
        let err = with_execution(&conn, &execution, || {
            let mut statement = conn.prepare("SELECT value FROM points").unwrap();
            let _held = budget.reserve(limit - budget.live_bytes() - 32 * 1024).unwrap();
            statement.query_row([], |r| r.get::<_, i64>(0)).unwrap_err()
        }).unwrap();
        assert!(!opened.get(), "cursor constructed before reserving its storage");
        assert!(matches!(scope.translate(err), kaban_vtab::QueryError::Limit(
            kaban_core::query_budget::BudgetExceeded { limit: kaban_core::query_budget::Limit::Memory, .. }
        )));
        conn.close().unwrap();
        assert_eq!(budget.live_bytes(), 0);
    }
}
struct CountRows<'a>(&'a Cell<usize>);

impl ExecutionPayload for CountRows<'_> {
    type Cursor = std::ops::Range<i64>;

    fn open(
        &self,
        _: &'static str,
        _: &Scan<'_>,
        _: &QueryBudget,
    ) -> Result<Self::Cursor, FnError> {
        Ok(1..3)
    }

    fn next(
        &self,
        cursor: &mut Self::Cursor,
        _: &QueryBudget,
    ) -> Option<Result<Vec<Value>, FnError>> {
        self.0.set(self.0.get() + 1);
        cursor.next().map(|value| Ok(vec![Value::Integer(value)]))
    }
}

fn counting<'a>(payload: &'a CountRows<'_>) -> Execution<'a, CountRows<'a>> {
    Execution {
        plan: &|_, _, _| Ok(Plan::default()),
        payload,
    }
}

fn first(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT value FROM points", [], |row| row.get(0))
}

fresh_test! {
    fn only_the_bound_connection_can_plan_or_scan_and_nested_binding_is_refused() {
        let scope =
            RequestScope::enter(fresh::installed_runtime(), &QueryBudget::new(1 << 23, None)).unwrap();
        let a = scope
            .open_query_connection(Path::new(":memory:"), fresh::query_flags())
            .unwrap();
        let b = scope
            .open_query_connection(Path::new(":memory:"), fresh::query_flags())
            .unwrap();
        register_execution(&a, "points", COLS).unwrap();
        register_execution(&b, "points", COLS).unwrap();
        let calls = Cell::new(0);
        let data = CountRows(&calls);
        let missing = first(&a).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("no active statement execution"),
            "{missing}"
        );
        // A statement already prepared on b must also refuse at scan time.
        let mut prepared = with_execution(&b, &counting(&data), || {
            b.prepare("SELECT value FROM points")
        })
        .unwrap()
        .unwrap();
        with_execution(&a, &counting(&data), || {
            let wrong_plan = first(&b).unwrap_err();
            assert!(
                wrong_plan.to_string().contains("another connection"),
                "{wrong_plan}"
            );
            let wrong_scan = prepared.query_row([], |r| r.get::<_, i64>(0)).unwrap_err();
            assert!(
                wrong_scan.to_string().contains("another connection"),
                "{wrong_scan}"
            );
            let nested =
                with_execution(&b, &counting(&data), || panic!("nested run entered")).unwrap_err();
            assert!(nested.reason.contains("already active"), "{nested}");
            assert_eq!(calls.get(), 0);
            first(&a)
        })
        .unwrap()
        .unwrap();
        assert_eq!(calls.get(), 1);
        drop(prepared);
        a.close().unwrap();
        b.close().unwrap();
        assert_eq!(scope.budget().live_bytes(), 0);
    }
}
fresh_test! {
    fn a_cursor_cannot_resume_in_the_next_execution_on_the_same_connection() {
        let scope =
            RequestScope::enter(fresh::installed_runtime(), &QueryBudget::new(1 << 22, None)).unwrap();
        let conn = scope
            .open_query_connection(Path::new(":memory:"), fresh::query_flags())
            .unwrap();
        register_execution(&conn, "points", COLS).unwrap();
        let old_calls = Cell::new(0);
        let new_calls = Cell::new(0);
        let old = CountRows(&old_calls);
        let new = CountRows(&new_calls);
        let mut stmt = with_execution(&conn, &counting(&old), || {
            conn.prepare("SELECT value FROM points")
        })
        .unwrap()
        .unwrap();
        let mut rows = stmt.query([]).unwrap();
        with_execution(&conn, &counting(&old), || {
            assert_eq!(rows.next().unwrap().unwrap().get::<_, i64>(0).unwrap(), 1);
        })
        .unwrap();
        with_execution(&conn, &counting(&new), || {
            let stale = rows.next().unwrap_err();
            assert!(
                stale.to_string().contains("different statement execution"),
                "{stale}"
            );
            assert_eq!(new_calls.get(), 0);
            assert_eq!(old_calls.get(), 1);
            assert_eq!(first(&conn), Ok(1));
        })
        .unwrap();
        drop(rows);
        drop(stmt);
        conn.close().unwrap();
        assert_eq!(scope.budget().live_bytes(), 0);
    }
}
fresh_test! {
    fn errors_and_unwinding_release_the_payload_and_cancelled_scans_stop() {
        let budget = QueryBudget::new(1 << 22, None);
        let scope = RequestScope::enter(fresh::installed_runtime(), &budget).unwrap();
        let conn = scope
            .open_query_connection(Path::new(":memory:"), fresh::query_flags())
            .unwrap();
        register_execution(&conn, "points", COLS).unwrap();
        let calls = Cell::new(0);
        let data = CountRows(&calls);
        let failed = with_execution(&conn, &counting(&data), || conn.prepare("SELECT FROM")).unwrap();
        assert!(failed.is_err());
        drop(failed);
        assert!(
            first(&conn)
                .unwrap_err()
                .to_string()
                .contains("no active statement execution")
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = with_execution(&conn, &counting(&data), || {
                    assert_eq!(first(&conn), Ok(1));
                    panic!("leave the statement");
                });
            }))
            .is_err()
        );
        assert!(
            first(&conn)
                .unwrap_err()
                .to_string()
                .contains("no active statement execution")
        );
        with_execution(&conn, &counting(&data), || {
            let mut stmt = conn.prepare("SELECT value FROM points").unwrap();
            let mut rows = stmt.query([]).unwrap();
            rows.next().unwrap().unwrap();
            budget.cancel();
            let refused = rows.next().unwrap_err();
            assert!(matches!(
                scope.translate(refused),
                kaban_vtab::QueryError::Limit(kaban_core::query_budget::BudgetExceeded {
                    limit: kaban_core::query_budget::Limit::Cancelled,
                    ..
                })
            ));
        })
        .unwrap();
        assert_eq!(calls.get(), 2, "cancelled cursor reached the payload");
        conn.close().unwrap();
        assert_eq!(budget.live_bytes(), 0);
    }
}
