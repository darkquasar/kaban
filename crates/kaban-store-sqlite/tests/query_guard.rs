//! The public query connection permits model reads and refuses other statement classes.
#![allow(clippy::unwrap_used, clippy::panic)]
mod common;
use common::{Scratch, created};
use kaban_core::{
    MemoryStore, QueryLimits, SchemaStore, SqlValue, Statement, StoreError, ViewName,
};
use kaban_store_sqlite::SqliteStore;

fn statement(sql: &str) -> Statement {
    Statement {
        view: ViewName::Main,
        sql: sql.into(),
        params: vec![],
    }
}

#[test]
fn the_indexed_public_projection_is_readable_and_the_backing_tables_are_private() {
    let scratch = Scratch::new("query", "guard");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let id = created(&store, "Fact", "seaglass").id;
    let mut select = statement("SELECT id, label FROM nodes WHERE id = ?1");
    select.params.push(SqlValue::Text(id.clone()));
    let limits = QueryLimits::default();
    assert_eq!(
        store.query(&select, &limits).unwrap().rows,
        vec![vec![SqlValue::Text(id), SqlValue::Text("Fact".into())]]
    );
    assert_eq!(
        store
            .query(&statement("SELECT count(*) FROM nodes"), &limits)
            .unwrap()
            .rows,
        vec![vec![SqlValue::Integer(1)]]
    );
    assert_eq!(
        store
            .query(&statement("SELECT count(*) FROM Fact"), &limits)
            .unwrap()
            .rows,
        vec![vec![SqlValue::Integer(1)]]
    );
    select.sql.insert_str(0, "EXPLAIN QUERY PLAN ");
    let plan = store.query(&select, &limits).unwrap();
    assert!(plan.rows.iter().flatten().any(|v| matches!(v,SqlValue::Text(detail) if detail.contains("SEARCH") && detail.contains("id=?"))),"{plan:?}");
    for (sql, name) in [
        ("SELECT id FROM main.node", "node"),
        ("SELECT * FROM main.rollup", "table or column"),
        ("SELECT rid FROM Fact", "statement"),
        ("SELECT * FROM cat_generation", "cat_generation"),
        ("PRAGMA user_version", "PRAGMA"),
        ("SELECT 1; SELECT 2", "statements"),
        ("DELETE FROM main.node", "DELETE"),
    ] {
        let result = store.query(&statement(sql), &limits);
        assert!(
            matches!(&result,Err(StoreError::QueryRefused{construct,..}) if construct==name),
            "{name}: {result:?}"
        );
    }
    assert!(matches!(
        store.query(&statement("DELETE FROM nodes"), &limits),
        Err(StoreError::QueryRefused { .. })
    ));
}

#[test]
fn explain_costs_search_from_the_selected_snapshot_before_any_scan() {
    let scratch = Scratch::new("query", "explain-snapshot");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let explain = statement("EXPLAIN QUERY PLAN SELECT id FROM search('seaglass', 5)");
    let detail = || {
        let plan = store.query(&explain, &QueryLimits::default()).unwrap();
        plan.rows
            .into_iter()
            .flatten()
            .find_map(|v| match v {
                SqlValue::Text(s) if s.contains("VIRTUAL TABLE INDEX") => Some(s),
                _ => None,
            })
            .unwrap()
    };
    let empty = detail();
    let search = statement("SELECT id FROM search('seaglass',5)");
    assert_eq!(
        store.query(&search, &QueryLimits::default()).unwrap().rows,
        Vec::<Vec<SqlValue>>::new()
    );
    let id = created(&store, "Fact", "seaglass").id;
    let populated = detail();
    assert_ne!(
        empty, populated,
        "the empty snapshot must select the known-empty scan"
    );
    assert!(empty.ends_with(":1"), "{empty}");
    assert!(populated.ends_with(':'), "{populated}");
    assert_eq!(
        store.query(&search, &QueryLimits::default()).unwrap().rows,
        vec![vec![SqlValue::Text(id)]]
    );
}

#[test]
fn the_final_result_reports_truncation_and_enforces_query_limits() {
    let scratch = Scratch::new("query", "limits");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let sql = statement("VALUES (1),(2)");
    let mut limits = QueryLimits {
        rows: 1,
        ..Default::default()
    };
    let set = store.query(&sql, &limits).unwrap();
    assert_eq!(set.rows, vec![vec![SqlValue::Integer(1)]]);
    assert!(set.truncated);
    limits.rows = 1501;
    assert!(matches!(
        store.query(&sql, &limits),
        Err(StoreError::QueryLimit { limit: "rows", .. })
    ));
    limits = QueryLimits {
        value_bytes: 16,
        ..Default::default()
    };
    let mut bound = statement("SELECT ?1");
    bound.params.push(SqlValue::Blob(vec![0; 17]));
    assert!(matches!(
        store.query(&bound, &limits),
        Err(StoreError::QueryLimit {
            limit: "value_bytes",
            ..
        })
    ));
    limits = QueryLimits {
        memory_bytes: 16,
        ..Default::default()
    };
    assert!(matches!(
        store.query(&sql, &limits),
        Err(StoreError::QueryLimit {
            limit: "memory_bytes",
            ..
        })
    ));
}

#[test]
fn unused_ontology_descriptions_do_not_consume_the_query_working_set() {
    let scratch = Scratch::new("query", "schema-size");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let mut def = store
        .schema()
        .unwrap()
        .into_iter()
        .find(|d| d.name == "Entity")
        .unwrap();
    def.name = "Documented".into();
    def.ns = None;
    def.builtin = false;
    def.description = "x".repeat(640 * 1024);
    def.declaration = kaban_core::TypeDeclaration::default();
    store.schema_extend(def).unwrap();
    let id = created(&store, "Documented", "seaglass").id;
    let mut sql = statement("SELECT label FROM nodes WHERE id=?1");
    sql.params.push(SqlValue::Text(id));
    assert_eq!(
        store.query(&sql, &QueryLimits::default()).unwrap().rows,
        vec![vec![SqlValue::Text("Documented".into())]]
    );
}

#[test]
fn earlier_format_14_relationships_have_row_views_without_a_per_commit_flag() {
    let scratch = Scratch::new("query", "early-model");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let id = created(&store, "Fact", "legacy row").id;
    drop(store);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute_batch("ALTER TABLE schema_type DROP COLUMN per_commit")
        .unwrap();
    drop(conn);
    let actor = kaban_core::ActorId::parse("local").unwrap();
    let (reader, _) = SqliteStore::open_diagnostic(&scratch.db(), &scratch.keys(), &actor).unwrap();
    {
        let rows=reader.query(&statement("SELECT sql_name,per_commit FROM model() WHERE declared_name='PART_OF' AND column_name='src'"),&QueryLimits::default()).unwrap().rows;
        assert_eq!(
            rows,
            vec![vec![SqlValue::Text("PART_OF".into()), SqlValue::Integer(0)]]
        );
    }
    drop(reader);
    let writer = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let mut select = statement("SELECT id FROM Fact WHERE id=?1");
    select.params.push(SqlValue::Text(id.clone()));
    assert_eq!(
        writer.query(&select, &QueryLimits::default()).unwrap().rows,
        vec![vec![SqlValue::Text(id)]]
    );
}
