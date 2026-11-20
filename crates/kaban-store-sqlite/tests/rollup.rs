//! Runtime-declared aggregates agree with the active node properties.
#![allow(clippy::unwrap_used)]

mod common;

use common::{Scratch, ctx};
use kaban_core::{
    ForgetMode, IngestStore, MemoryStore, NewNode, NodeId, Op, SchemaStore, SqlValue, TypeDef,
    WriteBatch,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};

fn definition() -> TypeDef {
    serde_json::from_value(json!({
        "kind":"label", "name":"ToolCall", "description":"Recorded call",
        "columns":[{"name":"tool","ty":"text","indexed":true},{"name":"ms","ty":"integer"},
                   {"name":"cost","ty":"real"},{"name":"started","ty":"timestamp"}],
        "fts_fields":["props.output"],
        "rollups":[{"name":"calls","label":"ToolCall","group_by":["tool"],"op":"count"},
                  {"name":"elapsed","label":"ToolCall","group_by":["tool"],"op":{"sum":"ms"}},
                  {"name":"costs","label":"ToolCall","group_by":["tool"],"op":{"sum":"cost"}}]
    }))
    .unwrap()
}

fn upsert(id: &NodeId, props: Value) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "ToolCall".into(),
            text: format!("Call {id}"),
            props: Some(props),
            ..Default::default()
        },
    }
}

#[test]
fn declared_count_integer_and_fractional_sums_agree_with_active_node_properties() {
    let scratch = Scratch::new("kaban-rollup", "fractional");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    store.schema_extend(definition()).unwrap();
    let rows = [
        json!({"tool":"edit","ms":7,"cost":1.25}),
        json!({"tool":"edit","ms":12,"cost":2.5}),
    ];
    store
        .apply_batch(
            WriteBatch::new(
                kaban_core::WorkspaceId::default(),
                rows.into_iter()
                    .map(|props| upsert(&NodeId::new(), props))
                    .collect(),
            ),
            &ctx(),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rollup')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        present,
        "the committed aggregates are available to another connection"
    );
    let oracle: (i64,i64,f64) = conn.query_row("SELECT count(*), sum(json_extract(props,'$.ms')), sum(json_extract(props,'$.cost')) FROM node WHERE kind='ToolCall' AND lifecycle='active'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(oracle, (2, 19, 3.75));
    let result: (i64,i64,f64) = conn.query_row("SELECT (SELECT value FROM rollup WHERE name='calls'),(SELECT value FROM rollup WHERE name='elapsed'),(SELECT value FROM rollup WHERE name='costs')", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(result, oracle);
}

#[test]
fn typed_rows_and_their_index_are_materialized_from_canonical_properties() {
    let scratch = Scratch::new("kaban-rollup", "canonical");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    store.schema_extend(definition()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let id = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(
                kaban_core::WorkspaceId::default(),
                vec![upsert(
                    &id,
                    json!({
                        "tool":"edit","ms":42,"cost":1.25,"started":"2026-09-29T12:00:00Z"
                    }),
                )],
            ),
            &ctx(),
        )
        .unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='col_ToolCall'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1, "declaring a label creates its column table");
    let typed: (String, i64, f64, String) = conn
        .query_row(
            "SELECT tool,ms,cost,started FROM col_ToolCall JOIN node USING(rid) WHERE node.id=?",
            [id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        typed,
        ("edit".into(), 42, 1.25, "2026-09-29T12:00:00Z".into())
    );
    let indexed: i64 = conn.query_row("SELECT count(*) FROM pragma_index_list('col_ToolCall') i, pragma_index_info(i.name) c WHERE c.name='tool'", [], |r| r.get(0)).unwrap();
    assert_eq!(indexed, 1);
}

#[test]
fn direct_typed_edits_survive_lifecycle_changes_and_rollup_redeclaration() {
    let scratch = Scratch::new("kaban-rollup", "column-only");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let id = NodeId::new();
    let mut def = definition();
    store.schema_extend(def.clone()).unwrap();
    store
        .apply_batch(
            WriteBatch::new(
                kaban_core::WorkspaceId::default(),
                vec![upsert(&id, json!({"tool":"old","ms":7}))],
            ),
            &ctx(),
        )
        .unwrap();
    let sql = |s| kaban_store_sqlite::catalog_testing::corrupt(&store, s).unwrap();
    sql(
        "UPDATE col_ToolCall SET tool='middle', ms=11; UPDATE col_ToolCall SET tool=NULL, ms=NULL;
         UPDATE col_ToolCall SET tool='final', ms=13;",
    );
    let get = |name, key: &str| store.rollup(name, &[SqlValue::Text(key.into())]).unwrap();
    assert_eq!(get("calls", "old"), SqlValue::Integer(0));
    assert_eq!(get("elapsed", "final"), SqlValue::Integer(13));
    store.forget(&id, ForgetMode::Archive, &ctx()).unwrap();
    assert_eq!(get("calls", "final"), SqlValue::Integer(0));
    // The public reconciliation contract restricts restoration to ingest-owned core kinds.
    sql("UPDATE node SET lifecycle='active' WHERE kind='ToolCall'");
    assert_eq!(
        get("elapsed", "final"),
        SqlValue::Integer(13),
        "lifecycle changes preserve the typed values"
    );
    def.declaration.rollups.retain(|r| r.name != "costs");
    store.schema_extend(def.clone()).unwrap();
    assert_eq!(
        get("elapsed", "final"),
        SqlValue::Integer(13),
        "changing aggregates preserves the typed values"
    );
    assert!(store.rollup("costs", &[SqlValue::Null]).is_err());
    def.declaration.columns.retain(|c| c.name != "cost");
    store.schema_extend(def).unwrap();
    assert_eq!(
        get("elapsed", "final"),
        SqlValue::Integer(13),
        "retained columns keep their independent writes"
    );
    sql("UPDATE node SET kind='Fact' WHERE kind='ToolCall'");
    assert_eq!(get("calls", "final"), SqlValue::Integer(0));
}

fn assert_recount(
    writer: &SqliteStore,
    reader: &SqliteStore,
    db: &rusqlite::Connection,
    step: u32,
) {
    let mut groups = 0;
    for tool in [None, Some("edit"), Some("read"), Some("move")] {
        let (count, elapsed, costs): (i64, Option<i64>, Option<f64>) = db
            .prepare_cached(
                "SELECT count(*),sum(ms),sum(cost) FROM
             (SELECT c.ms,c.cost FROM col_ToolCall c JOIN node n USING(rid)
              WHERE n.kind='ToolCall' AND n.lifecycle='active' AND c.tool IS ? ORDER BY c.rid)",
            )
            .unwrap()
            .query_row([tool], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        if count != 0 {
            groups += 1;
        }
        let key = [tool.map_or(SqlValue::Null, |s| SqlValue::Text(s.into()))];
        for store in [writer, reader] {
            for (name, expected) in [
                ("calls", SqlValue::Integer(count)),
                ("elapsed", elapsed.map_or(SqlValue::Null, SqlValue::Integer)),
                ("costs", costs.map_or(SqlValue::Null, SqlValue::Real)),
            ] {
                assert_eq!(
                    store.rollup(name, &key).unwrap(),
                    expected,
                    "commit {step}, {name} {tool:?}"
                );
            }
        }
    }
    let saved: i64 = db
        .prepare_cached("SELECT count(*) FROM rollup")
        .unwrap()
        .query_row([], |r| r.get(0))
        .unwrap();
    assert_eq!(
        saved,
        groups * 3,
        "orphan or missing group at commit {step}"
    );
}

#[test]
fn five_thousand_seeded_commits_agree_with_an_independent_typed_recount() {
    let scratch = Scratch::new("kaban-rollup", "seeded");
    let writer = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let reader = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let db = rusqlite::Connection::open(scratch.db()).unwrap();
    let id = |n: u32| NodeId::parse(&format!("00000000-0000-7000-8000-{n:012x}")).unwrap();
    let mut ids: Vec<_> = (0..32).map(id).collect();
    writer.schema_extend(definition()).unwrap();
    assert_recount(&writer, &reader, &db, 0);
    let apply = |ops| {
        writer
            .apply_batch(
                WriteBatch::new(kaban_core::WorkspaceId::default(), ops),
                &ctx(),
            )
            .unwrap()
    };
    let mut seed = 0x5eed_1234u32;
    let mut random = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed >> 8
    };
    for step in 1u32..=5_000 {
        let slot = usize::try_from(random() % 32).unwrap();
        let tool = [None, Some("edit"), Some("read"), Some("move")]
            [usize::try_from(random() % 4).unwrap()];
        let ms = (!step.is_multiple_of(7)).then(|| i64::from(random() % 1_000) - 500);
        let cost = (!step.is_multiple_of(11)).then(|| (f64::from(random() % 100) - 50.0) / 4.0);
        if step % 199 == 0 && writer.get(&ids[slot], false).unwrap().is_some() {
            let mode = match (step / 199) % 3 {
                0 => ForgetMode::Purge,
                1 => ForgetMode::Tombstone,
                _ => ForgetMode::Archive,
            };
            writer.forget(&ids[slot], mode, &ctx()).unwrap();
            ids[slot] = id(32 + step);
        } else if step > 32 && !step.is_multiple_of(4) {
            // These are column-only canonical writes through the production catalog commit.
            // Two changes of the same row also exercise an intermediate group absent at commit.
            let rid: i64 = db
                .query_row(
                    "SELECT min(rid) FROM col_ToolCall WHERE _active=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let group = match tool {
                None => "NULL",
                Some("edit") => "'edit'",
                Some("read") => "'read'",
                _ => "'move'",
            };
            let ms_sql = ms.map_or_else(|| "NULL".into(), |v| v.to_string());
            let cost_sql = cost.map_or_else(|| "NULL".into(), |v| v.to_string());
            kaban_store_sqlite::catalog_testing::corrupt(
                &writer,
                &format!(
                    "UPDATE col_ToolCall SET tool='intermediate' WHERE rid={rid};
                 UPDATE col_ToolCall SET tool={group},ms={ms_sql},cost={cost_sql} WHERE rid={rid};"
                ),
            )
            .unwrap();
        } else {
            apply(vec![upsert(
                &ids[slot],
                json!({"tool":tool,"ms":ms,"cost":cost,"step":step}),
            )]);
        }
        assert_recount(&writer, &reader, &db, step);
    }
}

#[test]
fn cancellation_nulls_and_integer_overflow_match_sql_or_roll_back_the_batch() {
    let scratch = Scratch::new("kaban-rollup", "cancellation");
    let writer = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let reader = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let db = rusqlite::Connection::open(scratch.db()).unwrap();
    writer.schema_extend(definition()).unwrap();
    let ids = [NodeId::new(), NodeId::new(), NodeId::new()];
    let apply = |ops| {
        writer.apply_batch(
            WriteBatch::new(kaban_core::WorkspaceId::default(), ops),
            &ctx(),
        )
    };
    apply(vec![
        upsert(&ids[0], json!({"cost":1e20,"ms":i64::MAX})),
        upsert(&ids[1], json!({"cost":3.75})),
        upsert(&ids[2], json!({"cost":-1e20})),
    ])
    .unwrap();
    assert_eq!(
        reader.rollup("costs", &[SqlValue::Null]).unwrap(),
        SqlValue::Real(3.75)
    );
    assert_recount(&writer, &reader, &db, 0);
    let old = reader.get(&ids[1], false).unwrap().unwrap();
    let sediment = || {
        db.query_row("SELECT max(seq) FROM cat_sediment", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    let seq = sediment();
    let result = apply(vec![upsert(&ids[1], json!({"ms":1,"cost":8}))]);
    assert!(matches!(
        result,
        Err(kaban_core::StoreError::RollupOverflow)
    ));
    assert_eq!(
        reader.get(&ids[1], false).unwrap().unwrap().props,
        old.props
    );
    assert_eq!(sediment(), seq);
    assert_recount(&writer, &reader, &db, 1);
    apply(vec![
        upsert(&ids[1], json!({"tool":"move"})),
        upsert(&ids[2], json!({"tool":"move"})),
    ])
    .unwrap();
    assert_recount(&writer, &reader, &db, 2);
}

#[test]
fn a_derived_failure_rolls_back_row_and_declaration_backfills_with_their_indexes() {
    let scratch = Scratch::new("kaban-rollup", "rollback");
    let writer = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let reader = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let db = rusqlite::Connection::open(scratch.db()).unwrap();
    let mut def = definition();
    writer.schema_extend(def.clone()).unwrap();
    let id = NodeId::new();
    let apply = |props| {
        writer.apply_batch(
            WriteBatch::new(kaban_core::WorkspaceId::default(), vec![upsert(&id, props)]),
            &ctx(),
        )
    };
    apply(json!({"tool":"edit","ms":9,"bad":"not numeric"})).unwrap();
    let schema = || {
        serde_json::to_value(
            reader
                .schema()
                .unwrap()
                .into_iter()
                .find(|d| d.name == "ToolCall")
                .unwrap(),
        )
        .unwrap()
    };
    let previous_schema = schema();
    let state = || {
        db.query_row(
            "SELECT
        (SELECT group_concat(hex(payload)) FROM cat_sediment ORDER BY seq),
        (SELECT max(generation) FROM cat_generation),
        (SELECT count(*) FROM rollup_input),
        (SELECT group_concat(name||hex(key)||members||nonnull||coalesce(value,'NULL')) FROM rollup),
        (SELECT props FROM node WHERE id=?),
        (SELECT json_array(tool,ms,cost,started) FROM col_ToolCall)",
            [id.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )
        .unwrap()
    };
    let previous_state = state();
    db.execute_batch(
        "CREATE TRIGGER refuse_aggregate BEFORE INSERT ON rollup
        WHEN new.name='elapsed' BEGIN SELECT raise(ABORT,'injected rollup failure'); END;",
    )
    .unwrap();
    assert!(apply(json!({"tool":"read","ms":34})).is_err());
    assert_eq!(state(), previous_state);
    def.declaration.rollups.retain(|r| r.name != "calls");
    assert!(writer.schema_extend(def.clone()).is_err());
    assert_eq!(schema(), previous_schema);
    assert_eq!(state(), previous_state);
    db.execute_batch("DROP TRIGGER refuse_aggregate").unwrap();
    def.declaration
        .columns
        .push(serde_json::from_value(json!({"name":"bad","ty":"integer"})).unwrap());
    assert!(
        writer.schema_extend(def).is_err(),
        "invalid typed backfill must roll back"
    );
    assert_eq!(schema(), previous_schema);
    assert_eq!(state(), previous_state);
    assert_recount(&writer, &reader, &db, 0);
}

#[test]
fn public_reactivation_keeps_the_ingest_kind_and_ownership_restrictions() {
    let store = SqliteStore::open_memory().unwrap();
    store.schema_extend(definition()).unwrap();
    let custom = NodeId::new();
    let document = NodeId::new();
    let apply = |ops| {
        store.apply_batch(
            WriteBatch::new(kaban_core::WorkspaceId::default(), ops),
            &ctx(),
        )
    };
    let owner = json!({"owner":"kaban-ingest"});
    apply(vec![
        upsert(&custom, owner.clone()),
        Op::UpsertNode {
            id: document.clone(),
            node: NewNode {
                kind: "Document".into(),
                text: "source".into(),
                props: Some(owner),
                ..Default::default()
            },
        },
    ])
    .unwrap();
    for id in [&custom, &document] {
        store.forget(id, ForgetMode::Tombstone, &ctx()).unwrap();
    }
    assert!(apply(vec![Op::Reactivate { id: custom.clone() }]).is_err());
    apply(vec![Op::Reactivate {
        id: document.clone(),
    }])
    .unwrap();
    assert!(store.get(&document, false).unwrap().is_some());
    assert!(store.get(&custom, false).unwrap().is_none());
    assert_eq!(
        store.rollup("calls", &[SqlValue::Null]).unwrap(),
        SqlValue::Integer(0)
    );
}

#[test]
fn timestamp_columns_accept_calendar_dates_and_refuse_malformed_and_untyped_values() {
    let store = SqliteStore::open_memory().unwrap();
    store.schema_extend(definition()).unwrap();
    let id = NodeId::new();
    let write = |stamp: &str| {
        store.apply_batch(
            WriteBatch::new(
                kaban_core::WorkspaceId::default(),
                vec![upsert(&id, json!({"started":stamp}))],
            ),
            &ctx(),
        )
    };
    write("2024-02-29T23:59:59.123456789+08:00").unwrap();
    for stamp in [
        "2023-02-29T00:00:00Z",
        "yesterday",
        "2024-01-01T00:00:00+99:90",
    ] {
        assert!(
            write(stamp).is_err(),
            "invalid timestamp {stamp} was accepted"
        );
    }
    assert!(
        kaban_store_sqlite::catalog_testing::corrupt(
            &store,
            "UPDATE col_ToolCall SET started='later'"
        )
        .is_err()
    );
}

#[test]
fn case_distinct_rollup_names_are_independent_of_sqlite_index_names() {
    let store = SqliteStore::open_memory().unwrap();
    let mut def = definition();
    let mut upper = def.declaration.rollups.last().unwrap().clone();
    upper.name = "COSTS".into();
    def.declaration.rollups.push(upper);
    store.schema_extend(def).unwrap();
    let id = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(
                kaban_core::WorkspaceId::default(),
                vec![upsert(&id, json!({"cost":2.5}))],
            ),
            &ctx(),
        )
        .unwrap();
    for name in ["costs", "COSTS"] {
        assert_eq!(
            store.rollup(name, &[SqlValue::Null]).unwrap(),
            SqlValue::Real(2.5)
        );
    }
}
