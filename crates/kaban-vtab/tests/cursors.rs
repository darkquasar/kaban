//! Several cursors over one registered table function stay independent: a self-join, interleaved
//! statements and an error with cursors still open (spec 11 T12, CRIT-01). These are regression
//! tests for the cursor ownership model in docs/developer/SAFETY.md; passing them does not prove
//! the absence of aliasing, which the ownership argument there carries. Each case runs in a fresh
//! process with the allocator adapter installed.
#![allow(clippy::unwrap_used)]

mod common;
mod fresh;

use std::sync::Arc;

use common::{Broken, Range, integers};
use fresh::{fresh_test, with_query};

fresh_test! {
    fn a_self_join_runs_two_cursors_over_one_table_at_once() {
        let budget = with_query(vec![Arc::new(Range)], |conn| {
            let pairs: Vec<(i64, i64)> = conn
                .prepare("SELECT a.value, b.value FROM range(1, 4) a JOIN range(1, 3) b ORDER BY a.value, b.value")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(pairs, vec![(1, 1), (1, 2), (2, 1), (2, 2), (3, 1), (3, 2)]);
        });
        assert_eq!(budget.live_bytes(), 0);
    }
}

fresh_test! {
    fn interleaved_statements_keep_their_own_positions() {
        with_query(vec![Arc::new(Range)], |conn| {
            let mut low = conn.prepare("SELECT value FROM range(1, 4)").unwrap();
            let mut high = conn.prepare("SELECT value FROM range(10, 13)").unwrap();
            let mut low_rows = low.query([]).unwrap();
            let mut high_rows = high.query([]).unwrap();
            let mut seen = Vec::new();
            for _ in 0..3 {
                let a: i64 = low_rows.next().unwrap().unwrap().get(0).unwrap();
                let b: i64 = high_rows.next().unwrap().unwrap().get(0).unwrap();
                seen.push((a, b));
            }
            assert!(low_rows.next().unwrap().is_none());
            assert!(high_rows.next().unwrap().is_none());
            assert_eq!(seen, vec![(1, 10), (2, 11), (3, 12)]);
        });
    }
}

fresh_test! {
    fn an_error_with_cursors_open_leaves_the_connection_usable() {
        let budget = with_query(vec![Arc::new(Range), Arc::new(Broken)], |conn| {
            // The outer `range` cursor is open when the inner `broken` cursor fails.
            let error = conn
                .prepare("SELECT r.value, b.value FROM range(1, 3) r JOIN broken b")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| row.get::<_, i64>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap_err();
            assert!(error.to_string().contains("broken: the second row is unreadable"), "{error}");
            // A statement left mid-scan when another fails is still usable afterwards.
            let mut open = conn.prepare("SELECT value FROM range(1, 4)").unwrap();
            let mut rows = open.query([]).unwrap();
            assert_eq!(rows.next().unwrap().unwrap().get::<_, i64>(0).unwrap(), 1);
            assert!(integers(conn, "SELECT value FROM broken").is_err());
            assert_eq!(rows.next().unwrap().unwrap().get::<_, i64>(0).unwrap(), 2);
            drop(rows);
            assert_eq!(integers(conn, "SELECT value FROM range(5, 7)"), Ok(vec![5, 6]));
        });
        assert_eq!(budget.live_bytes(), 0, "cursors left charged memory behind");
    }
}
