//! SQLite honours the plans a table function reports (spec 11 T12 Step 2, QRY-08). Each case runs
//! in a fresh process with the allocator adapter installed, on a request's query connection.
#![allow(clippy::unwrap_used)]

mod common;
mod fresh;

use std::sync::Arc;

use common::{Range, integers, query_plan};
use fresh::{fresh_test, with_query};

/// Run `body` on a query connection with `range` and a real table `t(x)` holding 1..=10.
fn with_table(body: impl FnOnce(&rusqlite::Connection)) {
    with_query(vec![Arc::new(Range)], |conn| {
        fill(conn);
        body(conn);
    });
}

fn fill(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TABLE t(x INTEGER);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10)
         INSERT INTO t SELECT i FROM n;",
    )
    .unwrap();
}

/// The table each `SCAN`/`SEARCH` line visits, outermost loop first.
fn loop_order(plan: &[String]) -> Vec<String> {
    plan.iter()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("SCAN ")
                .or_else(|| line.strip_prefix("SEARCH "))?;
            rest.split_whitespace().next().map(str::to_owned)
        })
        .collect()
}

fresh_test! {
    fn a_table_feeding_an_argument_is_the_outer_loop() {
        with_table(|conn| {
            let sql = "SELECT * FROM t JOIN range(1, x)";
            let plan = query_plan(conn, sql);
            assert_eq!(
                plan.as_deref().map(loop_order),
                Ok(vec!["t".to_owned(), "range".to_owned()]),
                "plan: {plan:?}"
            );
            // Row x of t contributes x - 1 rows: 0 + 1 + ... + 9.
            assert_eq!(
                integers(conn, &format!("SELECT count(*) FROM ({sql})")),
                Ok(vec![45])
            );
        });
    }
}

fresh_test! {
    fn a_consumed_order_needs_no_sort() {
        with_table(|conn| {
            let sql = "SELECT value FROM range(1, 100) ORDER BY value LIMIT 2";
            let plan = query_plan(conn, sql);
            assert!(
                plan.as_ref()
                    .is_ok_and(|lines| lines.iter().all(|l| !l.contains("TEMP B-TREE"))),
                "plan: {plan:?}"
            );
            assert_eq!(integers(conn, sql), Ok(vec![1, 2]));
        });
    }
}

fresh_test! {
    fn an_order_the_function_cannot_satisfy_is_sorted_by_sqlite() {
        with_table(|conn| {
            let sql = "SELECT value FROM range(1, 100) ORDER BY value DESC LIMIT 2";
            let plan = query_plan(conn, sql);
            assert!(
                plan.as_ref()
                    .is_ok_and(|lines| lines.iter().any(|l| l.contains("TEMP B-TREE"))),
                "plan: {plan:?}"
            );
            assert_eq!(integers(conn, sql), Ok(vec![99, 98]));
        });
    }
}
