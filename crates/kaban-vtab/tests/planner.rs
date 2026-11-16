//! SQLite honours the plans a table function reports (spec 11 T12 Step 2, QRY-08).
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{Range, connection, integers, query_plan};

/// A connection with `range` and a real table `t(x)` holding 1..=10.
fn with_table() -> rusqlite::Connection {
    let conn = connection(vec![Arc::new(Range)]);
    conn.execute_batch(
        "CREATE TABLE t(x INTEGER);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10)
         INSERT INTO t SELECT i FROM n;",
    )
    .unwrap();
    conn
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

#[test]
fn a_table_feeding_an_argument_is_the_outer_loop() {
    let conn = with_table();
    let sql = "SELECT * FROM t JOIN range(1, x)";
    let plan = query_plan(&conn, sql);
    assert_eq!(
        plan.as_deref().map(loop_order),
        Ok(vec!["t".to_owned(), "range".to_owned()]),
        "plan: {plan:?}"
    );
    // Row x of t contributes x - 1 rows: 0 + 1 + ... + 9.
    assert_eq!(
        integers(&conn, &format!("SELECT count(*) FROM ({sql})")),
        Ok(vec![45])
    );
}

#[test]
fn a_consumed_order_needs_no_sort() {
    let conn = with_table();
    let sql = "SELECT value FROM range(1, 100) ORDER BY value LIMIT 2";
    let plan = query_plan(&conn, sql);
    assert!(
        plan.as_ref()
            .is_ok_and(|lines| lines.iter().all(|l| !l.contains("TEMP B-TREE"))),
        "plan: {plan:?}"
    );
    assert_eq!(integers(&conn, sql), Ok(vec![1, 2]));
}

#[test]
fn an_order_the_function_cannot_satisfy_is_sorted_by_sqlite() {
    let conn = with_table();
    let sql = "SELECT value FROM range(1, 100) ORDER BY value DESC LIMIT 2";
    let plan = query_plan(&conn, sql);
    assert!(
        plan.as_ref()
            .is_ok_and(|lines| lines.iter().any(|l| l.contains("TEMP B-TREE"))),
        "plan: {plan:?}"
    );
    assert_eq!(integers(&conn, sql), Ok(vec![99, 98]));
}
