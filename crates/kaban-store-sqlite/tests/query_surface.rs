//! One main-view statement reads rows, text and graph at the same generation (QRY-01).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test assertions

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    IngestStore, MemoryStore, NewNode, NodeId, Op, QueryLimits, SqlValue, Statement, ViewName,
    WorkspaceId, WriteBatch,
};
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::catalog_testing::{self, Window};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WRITER_DB: &str = "KABAN_QUERY_TEST_DB";
const WRITER_SOURCE: &str = "KABAN_QUERY_TEST_SOURCE";
const WRITER_TARGET: &str = "KABAN_QUERY_TEST_TARGET";
const WAIT: Duration = Duration::from_secs(10);

struct Writer(Child);

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// Both publications use the same process entry point and atomic node/text/edge batch.
fn publish(db: &Path, source: &NodeId, target: &NodeId) {
    let mut child = Writer(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "publish_query_target",
                "--ignored",
                "--nocapture",
            ])
            .env(WRITER_DB, db)
            .env(WRITER_SOURCE, source.as_str())
            .env(WRITER_TARGET, target.as_str())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "query fixture writer exited {status}");
            return;
        }
        assert!(started.elapsed() < WAIT, "query fixture writer timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "child process for the query snapshot regression"]
fn publish_query_target() {
    let Some(db) = std::env::var_os(WRITER_DB) else {
        return;
    };
    let db = Path::new(&db);
    let store = SqliteStore::open(db, &db.with_file_name("keys")).unwrap();
    let source = nid(&std::env::var(WRITER_SOURCE).unwrap());
    let target = nid(&std::env::var(WRITER_TARGET).unwrap());
    let ops = vec![
        Op::UpsertNode {
            id: target.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: format!("amberfinch {target}"),
                ..Default::default()
            },
        },
        Op::UpsertEdge {
            src: source,
            dst: target,
            rel: "RELATES_TO".into(),
            props: None,
        },
    ];
    store
        .apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
        .unwrap();
    catalog_testing::reclaim(&store).unwrap();
}

#[test]
fn a_main_statement_keeps_rows_search_and_expand_at_its_snapshot() {
    let scratch = Scratch::new("kaban-query", "main-snapshot");
    let reader = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = nid(&created(&reader, "Fact", "a quiet harbor").id);
    let known = NodeId::new();
    let late = NodeId::new();
    publish(&scratch.db(), &source, &known);
    assert!(reader.get(&known, false).unwrap().is_some());
    assert!(reader.get(&late, false).unwrap().is_none());

    let statement = Statement {
        view: ViewName::Main,
        sql: "WITH targets(ord, target) AS (VALUES (1, ?2), (2, ?3))
              SELECT (SELECT n.id FROM nodes n WHERE n.id = targets.target),
                     (SELECT s.id FROM search('amberfinch', 10) s WHERE s.id = targets.target),
                     (SELECT e.id FROM expand(?1, 'RELATES_TO', 1, 10) e WHERE e.id = targets.target)
              FROM targets ORDER BY ord"
            .into(),
        params: [&source, &known, &late]
            .map(|id| SqlValue::Text(id.to_string()))
            .into(),
    };
    let limits = QueryLimits {
        time: WAIT * 2,
        rows: 50,
        memory_bytes: 64 << 20,
        value_bytes: 16 << 20,
    };
    let db = scratch.db();
    let late_id = late.clone();
    catalog_testing::set_failpoint(
        &reader,
        Window::ReaderInTransaction,
        Some(Arc::new(move || publish(&db, &source, &late_id))),
    );
    let pinned = reader.query(&statement, &limits);
    catalog_testing::set_failpoint(&reader, Window::ReaderInTransaction, None);

    let present = |id: &NodeId| vec![SqlValue::Text(id.to_string()); 3];
    assert_eq!(
        pinned
            .map(|result| result.rows)
            .map_err(|err| err.to_string()),
        Ok(vec![present(&known), vec![SqlValue::Null; 3]]),
        "the held main statement must return the known node only across rows, search and expand"
    );
    assert_eq!(
        reader.query(&statement, &limits).unwrap().rows,
        vec![present(&known), present(&late)],
        "the next main statement must see the committed node through all three paths"
    );
}
