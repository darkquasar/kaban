//! One main-view statement reads rows, text and graph at the same generation (QRY-01).

#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test assertions

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    IngestStore, MemoryStore, NewNode, NodeId, Op, QueryLimits, SchemaStore, SqlValue, Statement,
    ViewName, WorkspaceId, WriteBatch,
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
const CODE: &str = "code@0123456789abcdef0123456789abcdef01234567";

#[test]
fn the_ontology_used_at_prepare_time_belongs_to_the_pinned_transaction() {
    let scratch = Scratch::new("kaban-query", "ontology-pin");
    let reader = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let writer = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let fact = created(&reader, "Fact", "a quiet harbor").id;
    let mut def = reader
        .schema()
        .unwrap()
        .into_iter()
        .find(|d| d.name == "Entity")
        .unwrap();
    def.name = "Milestone".into();
    def.ns = None;
    def.builtin = false;
    def.declaration = kaban_core::TypeDeclaration::default();
    catalog_testing::set_failpoint(
        &reader,
        Window::ReaderInTransaction,
        Some(Arc::new(move || {
            writer.schema_extend(def.clone()).unwrap();
        })),
    );
    let statement = Statement {
        view: ViewName::Main,
        params: vec![SqlValue::Text(fact.clone())],
        sql: "SELECT f.id,m.declared_name FROM Fact f CROSS JOIN model() m WHERE f.id=?1
         AND m.kind='label' AND m.declared_name IN ('Fact','Milestone') AND m.column_name='id'
         ORDER BY m.declared_name"
            .into(),
    };
    let limits = QueryLimits::default();
    let held = reader.query(&statement, &limits);
    catalog_testing::set_failpoint(&reader, Window::ReaderInTransaction, None);
    let old = vec![SqlValue::Text(fact.clone()), SqlValue::Text("Fact".into())];
    assert_eq!(held.unwrap().rows, vec![old.clone()]);
    assert_eq!(
        reader.query(&statement, &limits).unwrap().rows,
        vec![
            old,
            vec![SqlValue::Text(fact), SqlValue::Text("Milestone".into())]
        ]
    );
}

const GROUPS: usize = 20;
const WRITES: usize = 3;
const READS: usize = 1000 / GROUPS;

fn stress_id(i: usize) -> NodeId {
    nid(&format!("018abcde-f012-7abc-8def-{i:012x}"))
}

fn stress_batch(store: &SqliteStore, db: &Path, view: &ViewName, source: &NodeId, i: usize) {
    let target = stress_id(i);
    let batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            Op::UpsertNode {
                id: target.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: format!("sandpiper{i}"),
                    ..Default::default()
                },
            },
            Op::UpsertEdge {
                src: source.clone(),
                dst: target.clone(),
                rel: "RELATES_TO".into(),
                props: None,
            },
        ],
    );
    catalog_testing::publish_code_batch(store, db, view, batch, source, &target, "RELATES_TO")
        .unwrap();
}

fn await_file(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < WAIT,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "child process for repeated snapshot comparisons"]
fn publish_overlapping_generations() {
    let Some(db) = std::env::var_os(WRITER_DB) else {
        return;
    };
    let db = Path::new(&db);
    let store = SqliteStore::open(db, &db.with_file_name("keys")).unwrap();
    let source = nid(&std::env::var(WRITER_SOURCE).unwrap());
    let code = CODE.parse().unwrap();
    for group in 0..GROUPS {
        await_file(&db.with_file_name(format!("start-{group}")));
        for offset in 1..=WRITES {
            std::thread::sleep(Duration::from_millis(1));
            let i = group * WRITES + offset;
            stress_batch(&store, db, &code, &source, i);
            std::fs::write(db.with_file_name(format!("committed-{i}")), []).unwrap();
        }
    }
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "record the two observed presence distributions"
)]
fn a_thousand_statements_per_view_agree_during_cross_process_publication() {
    let scratch = Scratch::new("kaban-query", "overlap");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = nid(&created(&store, "Fact", "a quiet harbor").id);
    let code: ViewName = CODE.parse().unwrap();
    stress_batch(&store, &scratch.db(), &code, &source, 0);
    let mut child = Writer(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "publish_overlapping_generations", "--ignored"])
            .env(WRITER_DB, scratch.db())
            .env(WRITER_SOURCE, source.as_str())
            .spawn()
            .unwrap(),
    );
    let limits = QueryLimits {
        time: WAIT,
        ..Default::default()
    };
    let mut commits_inside = [0; 2];
    let mut seen = [[0; 2]; 2];
    let mut sql = Statement {
        view: ViewName::Main,
        sql: "SELECT
       (SELECT id FROM nodes WHERE id=?1),
       (SELECT id FROM search(?2,1) WHERE id=?1),
       (SELECT id FROM expand(?3,'RELATES_TO',1,100) WHERE id=?1)"
            .into(),
        params: vec![],
    };
    for group in 0..GROUPS {
        let first = group * WRITES + 1;
        for step in 0..READS {
            // Alternate which view holds h while the child commits the first publication
            // of each group. The remaining publications race the following ordinary reads.
            for view in [group % 2, 1 - group % 2] {
                let held = step == 0 && view == group % 2;
                if held {
                    let start = scratch.db().with_file_name(format!("start-{group}"));
                    let published = scratch.db().with_file_name(format!("committed-{first}"));
                    catalog_testing::set_failpoint(
                        &store,
                        Window::ReaderInTransaction,
                        Some(Arc::new(move || {
                            std::fs::write(&start, []).unwrap();
                            await_file(&published);
                        })),
                    );
                }
                let target = match step % 5 {
                    0 => first,
                    1 => first + 1,
                    2 => first + 2,
                    3 => first - 1,
                    _ => first + WRITES,
                };
                sql.view = if view == 0 {
                    ViewName::Main
                } else {
                    code.clone()
                };
                let id = SqlValue::Text(stress_id(target).to_string());
                sql.params = vec![
                    id.clone(),
                    SqlValue::Text(format!("sandpiper{target}")),
                    SqlValue::Text(source.to_string()),
                ];
                let rows = store.query(&sql, &limits).unwrap().rows;
                if held {
                    catalog_testing::set_failpoint(&store, Window::ReaderInTransaction, None);
                    assert_eq!(
                        rows,
                        vec![vec![SqlValue::Null; 3]],
                        "view held before commit"
                    );
                    commits_inside[view] += 1;
                }
                let present = rows == vec![vec![id; 3]];
                assert!(
                    present || rows == vec![vec![SqlValue::Null; 3]],
                    "mixed generation {group}/{step}/{view}: {rows:?}"
                );
                if step == 0 && !held {
                    assert!(present, "the next snapshot must see the publication");
                }
                seen[view][usize::from(present)] += 1;
            }
        }
        await_file(
            &scratch
                .db()
                .with_file_name(format!("committed-{}", first + WRITES - 1)),
        );
    }
    let start = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(start.elapsed() < WAIT);
        std::thread::sleep(Duration::from_millis(1));
    }
    eprintln!(
        "main and code [absent,present] {seen:?}, publications inside held statements {commits_inside:?}"
    );
    for (counts, commits) in seen.into_iter().zip(commits_inside) {
        assert_eq!(counts.iter().sum::<usize>(), 1000);
        assert!(counts.into_iter().all(|n| n > 0));
        assert_eq!(commits, GROUPS / 2);
    }
}

/// Keep a one-hit Rust search busy inside xFilter. Each pass reads the real pinned Tantivy
/// snapshot and checks the TVF's budget. The fixture signals only after a real hit is found.
struct RepeatedSearch {
    index: kaban_index_tantivy::TantivyIndex,
    entered: std::sync::mpsc::SyncSender<kaban_core::query_budget::QueryBudget>,
}
impl kaban_core::text_index::TextIndex for RepeatedSearch {
    fn search(
        &self,
        at: &kaban_core::text_index::TextSnapshot<'_>,
        q: &kaban_core::text_index::TextQuery<'_>,
        budget: &kaban_core::query_budget::QueryBudget,
    ) -> kaban_core::Result<kaban_core::text_index::Searched> {
        use kaban_core::text_index::{Searched, TextIndex};
        let mut answer = TextIndex::search(&self.index, at, q, budget)?;
        assert!(matches!(&answer,Searched::Hits{hits,..} if hits.len()==1));
        self.entered.send(budget.clone()).unwrap();
        for _ in 0..100_000 {
            budget.check()?;
            answer = TextIndex::search(&self.index, at, q, budget)?;
        }
        Ok(answer)
    }
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "record live per-request charges before and after the final store handle closes"
)]
fn cancelling_inside_a_one_row_rust_scan_releases_a_waiting_checkpoint() {
    let scratch = Scratch::new("kaban-query", "cancel-checkpoint");
    let writer = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let target = created(&writer, "Fact", "orangelark").id;
    let (entered, started) = std::sync::mpsc::sync_channel(1);
    let index = kaban_index_tantivy::TantivyIndex::new(
        &kaban_core::shipped::resolved().unwrap(),
        &kaban_core::config::Recall::default(),
    );
    let reader = SqliteStore::open(&scratch.db(), &scratch.keys())
        .unwrap()
        .with_text_index(Box::new(RepeatedSearch { index, entered }));
    let sql = Statement {
        view: ViewName::Main,
        sql: "SELECT id FROM search('orangelark',1) LIMIT 1".into(),
        params: vec![],
    };
    let limits = QueryLimits {
        time: WAIT * 2,
        ..Default::default()
    };
    let neighbor = Statement {
        sql: "SELECT id FROM nodes WHERE id=?1".into(),
        params: vec![SqlValue::Text(target.clone())],
        view: ViewName::Main,
    };
    let mut closed = Vec::new();
    let mut maximum_pending = None;
    for _ in 0..4 {
        let execution = kaban_core::QueryExecution::new(limits);
        let cancel = execution.budget().clone();
        std::thread::scope(|threads| {
            let read = threads.spawn(|| reader.query_with_execution(&sql, execution));
            let inside = started.recv_timeout(WAIT).unwrap();
            let tick = threads.spawn(|| catalog_testing::checkpoint_tick(&writer).unwrap());
            let marker = scratch.0.join("kaban.kbx.pins").join("admission.pending");
            let start = Instant::now();
            while !marker.exists() {
                assert!(
                    !tick.is_finished() && start.elapsed() < WAIT,
                    "checkpoint never waited behind the query"
                );
                std::thread::yield_now();
            }
            assert!(inside.live_bytes() > 0);
            cancel.cancel();
            let result = read.join().unwrap();
            let checkpoint = tick.join().unwrap();
            assert!(
                matches!(
                    result,
                    Err(kaban_core::StoreError::QueryLimit {
                        limit: "cancelled",
                        ..
                    })
                ),
                "{result:?}"
            );
            assert!(
                checkpoint.drained && checkpoint.backfilled,
                "{checkpoint:?}"
            );
            assert_eq!(checkpoint.checkpointed_frames, checkpoint.wal_frames);
            assert_eq!(inside.live_bytes(), cancel.live_bytes());
        });
        closed.push(cancel);
        let execution = kaban_core::QueryExecution::new(limits);
        let success = execution.budget().clone();
        let watch = success.clone();
        catalog_testing::set_failpoint(
            &reader,
            Window::ReaderInTransaction,
            Some(Arc::new(move || {
                assert!(watch.live_bytes() > 0);
                watch.check().unwrap();
            })),
        );
        assert_eq!(
            reader
                .query_with_execution(&neighbor, execution)
                .unwrap()
                .rows,
            vec![vec![SqlValue::Text(target.clone())]]
        );
        catalog_testing::set_failpoint(&reader, Window::ReaderInTransaction, None);
        success.check().unwrap();
        closed.push(success);
        let live: Vec<_> = closed
            .iter()
            .map(kaban_core::query_budget::QueryBudget::live_bytes)
            .collect();
        let total = live.iter().sum::<u64>();
        eprintln!("closed request budgets with store handles open: {live:?}");
        assert!(
            total <= *maximum_pending.get_or_insert(total),
            "closed requests accumulated live bytes: {live:?}"
        );
    }
    let request = kaban_core::QueryExecution::new(limits);
    request.budget().cancel();
    assert!(matches!(
        reader.query_with_execution(&sql, request),
        Err(kaban_core::StoreError::QueryLimit {
            limit: "cancelled",
            ..
        })
    ));
    // SQLite's Unix VFS may retain a deferred fd while other handles on the inode hold
    // locks. The allocation stays charged to its creator until the final handle releases it.
    drop(reader);
    drop(writer);
    let live: Vec<_> = closed
        .iter()
        .map(kaban_core::query_budget::QueryBudget::live_bytes)
        .collect();
    eprintln!("closed request budgets after last store handle: {live:?}");
    assert_eq!(live, vec![0; closed.len()]);
}

struct Writer(Child);

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// Both publications use the same process entry point and atomic node/text/edge batch.
fn publish(db: &Path, source: &NodeId, target: &NodeId) {
    publish_in(db, source, target, "main", true);
}

fn publish_in(db: &Path, source: &NodeId, target: &NodeId, view: &str, reclaim: bool) {
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
            .env("KABAN_QUERY_TEST_VIEW", view)
            .env("KABAN_QUERY_TEST_RECLAIM", if reclaim { "1" } else { "0" })
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
    let mut ops = vec![Op::UpsertNode {
        id: target.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: format!("amberfinch {target}"),
            ..Default::default()
        },
    }];
    let view = std::env::var("KABAN_QUERY_TEST_VIEW")
        .unwrap()
        .parse()
        .unwrap();
    if view == ViewName::Main {
        ops.push(Op::UpsertEdge {
            src: source,
            dst: target,
            rel: "RELATES_TO".into(),
            props: None,
        });
        store
            .apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
            .unwrap();
    } else {
        let generation = catalog_testing::publish_code_batch(
            &store,
            db,
            &view,
            WriteBatch::new(WorkspaceId::default(), ops),
            &source,
            &target,
            "RELATES_TO",
        )
        .unwrap();
        std::fs::write(
            db.with_file_name(format!("generation-{target}")),
            generation.to_string(),
        )
        .unwrap();
    }
    if std::env::var("KABAN_QUERY_TEST_RECLAIM").unwrap() == "1" {
        catalog_testing::reclaim(&store).unwrap();
    }
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

    let statement = triple(ViewName::Main, &source, &known, &late);
    let limits = QueryLimits {
        time: WAIT * 2,
        ..Default::default()
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
    assert_presence(pinned, &reader, &statement, &limits, &known, &late);
}

fn triple(view: ViewName, source: &NodeId, known: &NodeId, late: &NodeId) -> Statement {
    Statement {
        view,
        sql: "WITH targets(ord, target) AS (VALUES (1, ?2), (2, ?3))
              SELECT (SELECT n.id FROM nodes n WHERE n.id = targets.target),
                     (SELECT s.id FROM search('amberfinch', 10) s WHERE s.id = targets.target),
                     (SELECT e.id FROM expand(?1, 'RELATES_TO', 1, 10) e WHERE e.id = targets.target)
              FROM targets ORDER BY ord"
            .into(),
        params: [source, known, late]
            .map(|id| SqlValue::Text(id.to_string()))
            .into(),
    }
}

fn assert_presence(
    pinned: kaban_core::Result<kaban_core::RowSet>,
    reader: &SqliteStore,
    statement: &Statement,
    limits: &QueryLimits,
    known: &NodeId,
    late: &NodeId,
) {
    let present = |id: &NodeId| vec![SqlValue::Text(id.to_string()); 3];
    assert_eq!(
        pinned
            .map(|result| result.rows)
            .map_err(|err| err.to_string()),
        Ok(vec![present(known), vec![SqlValue::Null; 3]]),
        "the held main statement must return the known node only across rows, search and expand"
    );
    assert_eq!(
        reader.query(statement, limits).unwrap().rows,
        vec![present(known), present(late)],
        "the next main statement must see the committed node through all three paths"
    );
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "record the candidate and both child commit generations"
)]
fn a_named_code_view_uses_h_after_locking_g_and_ignores_h_plus_one() {
    let scratch = Scratch::new("kaban-query", "code-snapshot");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = nid(&created(&store, "Fact", "a quiet harbor").id);
    let g = catalog_testing::with_snapshot(
        &store,
        &kaban_core::query_budget::QueryBudget::new(1 << 20, Some(WAIT)),
        |view| Ok(view.generation()),
    )
    .unwrap();
    let known = NodeId::new();
    let late = NodeId::new();
    for (window, target, reclaim) in [
        (Window::ReaderLocked, known.clone(), false),
        (Window::ReaderInTransaction, late.clone(), true),
    ] {
        let db = scratch.db();
        let src = source.clone();
        catalog_testing::set_failpoint(
            &store,
            window,
            Some(Arc::new(move || {
                publish_in(&db, &src, &target, CODE, reclaim);
            })),
        );
    }
    let limits = QueryLimits {
        time: WAIT * 2,
        ..Default::default()
    };
    let mut sql = triple(CODE.parse().unwrap(), &source, &known, &late);
    let pinned = store.query(&sql, &limits);
    catalog_testing::set_failpoint(&store, Window::ReaderLocked, None);
    catalog_testing::set_failpoint(&store, Window::ReaderInTransaction, None);
    assert_eq!(catalog_testing::pin_retries(&store), 0);
    assert_presence(pinned, &store, &sql, &limits, &known, &late);
    let committed = |target: &NodeId| {
        std::fs::read_to_string(scratch.0.join(format!("generation-{target}")))
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    let h = committed(&known);
    let later = committed(&late);
    assert!(g < h);
    assert_eq!(h + 1, later);
    eprintln!("candidate {g}, snapshot {h}, later commit {later}");
    sql.view = ViewName::Main;
    let rows = store.query(&sql, &limits).unwrap().rows;
    assert_eq!(rows.len(), 2);
    for (row, target) in rows.into_iter().zip([known, late]) {
        let id = SqlValue::Text(target.to_string());
        assert_eq!(row, vec![id.clone(), id, SqlValue::Null]);
    }
    sql.sql = "EXPLAIN QUERY PLAN SELECT id FROM expand(?1,'RELATES_TO',1,10)".into();
    sql.params.truncate(1);
    sql.view = CODE.parse().unwrap();
    let plan = store.query(&sql, &limits).unwrap();
    assert!(
        plan.rows
            .iter()
            .flatten()
            .any(|v| matches!(v,SqlValue::Text(s) if s.contains("VIRTUAL TABLE INDEX"))),
        "{plan:?}"
    );
    sql.view = "code@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        .parse()
        .unwrap();
    assert!(
        matches!(store.query(&sql,&limits),Err(kaban_core::StoreError::QueryRefused{construct,..}) if construct=="view")
    );
}
