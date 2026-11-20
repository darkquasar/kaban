//! Runtime-declared aggregates agree with the active node properties.
#![allow(clippy::unwrap_used)]

mod common;

use common::{Scratch, ctx};
use kaban_core::{IngestStore, NewNode, NodeId, Op, SchemaStore, TypeDef, WriteBatch};
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
    })).unwrap()
}

fn upsert(id: &NodeId, props: Value) -> Op {
    Op::UpsertNode { id: id.clone(), node: NewNode {
        kind:"ToolCall".into(), text: format!("Call {id}"), props:Some(props), ..Default::default()
    }}
}

#[test]
fn declared_count_integer_and_fractional_sums_agree_with_active_node_properties() {
    let scratch = Scratch::new("kaban-rollup", "fractional");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    store.schema_extend(definition()).unwrap();
    let rows = [json!({"tool":"edit","ms":7,"cost":1.25}), json!({"tool":"edit","ms":12,"cost":2.5})];
    store.apply_batch(WriteBatch::new(Default::default(), rows.into_iter().map(|props| upsert(&NodeId::new(), props)).collect()), &ctx()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let present: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rollup')", [], |r| r.get(0)).unwrap();
    assert!(present, "the committed aggregates are available to another connection");
    let oracle: (i64,i64,f64) = conn.query_row("SELECT count(*), sum(json_extract(props,'$.ms')), sum(json_extract(props,'$.cost')) FROM node WHERE kind='ToolCall' AND lifecycle='active'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(oracle,(2,19,3.75));
    let result: (i64,i64,f64) = conn.query_row("SELECT (SELECT value FROM rollup WHERE name='calls'),(SELECT value FROM rollup WHERE name='elapsed'),(SELECT value FROM rollup WHERE name='costs')", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(result,oracle);
}

#[test]
fn typed_rows_and_their_index_are_materialized_from_canonical_properties() {
    let scratch = Scratch::new("kaban-rollup", "canonical");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    store.schema_extend(definition()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let id = NodeId::new();
    store.apply_batch(WriteBatch::new(Default::default(), vec![upsert(&id, json!({
        "tool":"edit","ms":42,"cost":1.25,"started":"2026-09-29T12:00:00Z"
    }))]), &ctx()).unwrap();
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='col_ToolCall'", [], |r| r.get(0)).unwrap();
    assert_eq!(tables, 1, "declaring a label creates its column table");
    let typed: (String,i64,f64,String) = conn.query_row(
        "SELECT tool,ms,cost,started FROM col_ToolCall JOIN node USING(rid) WHERE node.id=?",
        [id.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    ).unwrap();
    assert_eq!(typed, ("edit".into(),42,1.25,"2026-09-29T12:00:00Z".into()));
    let indexed: i64 = conn.query_row("SELECT count(*) FROM pragma_index_list('col_ToolCall') i, pragma_index_info(i.name) c WHERE c.name='tool'", [], |r| r.get(0)).unwrap();
    assert_eq!(indexed,1);
}
