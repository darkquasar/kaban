//! Runtime label declarations survive a read from another store.
#![allow(clippy::unwrap_used)]

mod common;

use common::Scratch;
use kaban_core::{SchemaStore, TypeDef};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

#[test]
fn another_store_describes_the_columns_text_fields_and_rollups_of_a_runtime_label() {
    let scratch = Scratch::new("kaban-declared", "describe");
    let writer = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let reader = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let definition = json!({
        "kind": "label", "name": "ToolCall", "description": "A tool invocation",
        "columns": [{"name": "tool", "ty": "text", "indexed": true},
                    {"name": "ms", "ty": "integer", "indexed": false}],
        "fts_fields": ["props.output"],
        "rollups": [{"name": "calls_by_tool", "label": "ToolCall", "group_by": ["tool"], "op": "count"}]
    });
    writer
        .schema_extend(serde_json::from_value::<TypeDef>(definition.clone()).unwrap())
        .unwrap();
    let label = reader
        .schema()
        .unwrap()
        .into_iter()
        .find(|label| label.name == "ToolCall")
        .unwrap();
    let described = serde_json::to_value(label).unwrap();
    for field in ["columns", "fts_fields", "rollups"] {
        assert_eq!(described[field], definition[field], "persisted {field}");
    }
}

#[test]
fn a_project_created_before_declared_columns_keeps_its_rows_on_reopen() {
    let scratch = Scratch::new("kaban-declared", "legacy");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let before = common::created(&store, "Fact", "a pre-existing record");
    drop(store);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute_batch(
        "ALTER TABLE schema_type DROP COLUMN columns; ALTER TABLE schema_type DROP COLUMN rollups;
         DROP TABLE rollup; DROP TABLE rollup_input;",
    )
    .unwrap();
    drop(conn);
    let (diagnostic, _) = SqliteStore::open_diagnostic(
        &scratch.db(),
        &scratch.keys(),
        &kaban_core::ActorId::parse("local").unwrap(),
    )
    .unwrap();
    let labels = diagnostic.schema().unwrap();
    assert!(
        labels
            .iter()
            .any(|def| def.name == "Fact" && def.declaration.columns.is_empty())
    );
    let missing = kaban_core::MemoryStore::rollup(&diagnostic, "missing", &[]);
    assert!(matches!(
        missing,
        Err(kaban_core::StoreError::InvalidName(_, "unknown rollup"))
    ));
    drop(diagnostic);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let installed: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='rollup_input')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!installed, "diagnostics inspect the old format in place");
    drop(conn);
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let read = kaban_core::MemoryStore::get(&store, &common::nid(&before.id), false)
        .unwrap()
        .unwrap();
    assert_eq!(read.text, before.text);
    assert!(store.schema().unwrap().iter().any(|def| def.name == "Fact"));
}
