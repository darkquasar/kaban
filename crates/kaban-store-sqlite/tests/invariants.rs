//! Persisted-data invariants of the store (DEL-08 survivors of the claim-era file): the same
//! content in one workspace and kind is stored once, and malformed JSON in a persisted column
//! is a chained error carrying no stored bytes, never different data (MT-3, RB-5).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{ActorId, IngestStore, MemoryStore, NewNode, RememberOutcome, SchemaStore};
use kaban_store_sqlite::SqliteStore;
use rusqlite::Connection;

/// Invariant-specific views of the scratch directory: the store under test and a raw connection.
trait ScratchExt {
    fn store(&self) -> SqliteStore;
    /// A second connection on the same file (reads the schema; writes only to prove refusals).
    fn raw(&self) -> Connection;
}
impl ScratchExt for Scratch {
    fn store(&self) -> SqliteStore {
        SqliteStore::open(&self.db(), &self.keys()).unwrap()
    }
    fn raw(&self) -> Connection {
        let conn = Connection::open(self.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        conn
    }
}

#[test]
fn the_same_content_in_the_same_workspace_and_kind_is_stored_once() {
    // Arrange
    let s = Scratch::new("kaban-inv", "dedupe");
    let store = s.store();
    let first = created(&store, "Fact", "  the content hash covers trimmed text  ");

    // Act: the same text again (whitespace differs), and the same text as another kind
    let again = store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "the content hash covers trimmed text".into(),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap();
    let other_kind = created(&store, "Idea", "the content hash covers trimmed text");

    // Assert
    match again {
        RememberOutcome::Existing { node, .. } => assert_eq!(node.id, first.id),
        RememberOutcome::Created(n) => panic!("duplicate stored twice: {n:?}"),
    }
    assert_ne!(other_kind.id, first.id, "kind is part of the identity");
    let count: i64 = s
        .raw()
        .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
    assert_eq!(first.content_hash.len(), 64, "sha256 hex");
}

// ---- persisted JSON columns (MT-3, RB-5): malformed JSON is an error, never different data ----

/// Malformed JSON planted in a column by raw SQL. The marker must never appear in any error
/// text (core-spec S1: messages carry no stored content).
const CORRUPT_JSON: &str = "{not json CORRUPT-MARKER-7f3a";

/// The next link of an error chain: `source()`, or the driver error a `BackendSource` holder
/// keeps behind its redacted renderers (RM-1: the holder ends the `source()` chain on purpose,
/// so the descent into the driver is explicit).
fn next_link<'a>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a (dyn std::error::Error + 'static)> {
    match err.downcast_ref::<kaban_core::BackendSource>() {
        Some(holder) => Some(holder.driver()),
        None => err.source(),
    }
}

/// The whole error chain of `e` (through the backend holder), outermost first, rendered with
/// `Display` and `Debug`.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        out.push(format!("{err} / {err:?}"));
        cur = next_link(err);
    }
    out
}

/// The corruption contract: a `serde_json::Error` somewhere in the chain (so the cause is
/// inspectable) and no raw column bytes anywhere in the rendered chain, the driver's own
/// renderings included.
fn assert_chained_json_error(e: &kaban_core::StoreError) {
    let chain = error_chain(e);
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    let mut json_in_chain = false;
    while let Some(err) = cur {
        json_in_chain |= err.is::<serde_json::Error>();
        cur = next_link(err);
    }
    assert!(
        json_in_chain,
        "no serde_json::Error in the chain: {chain:?}"
    );
    for rendered in &chain {
        assert!(
            !rendered.contains("CORRUPT-MARKER"),
            "stored bytes leaked into the error: {rendered}"
        );
    }
}

#[test]
fn malformed_tags_json_on_a_node_is_a_chained_error_not_an_empty_tag_list() {
    // Arrange
    let s = Scratch::new("kaban-inv", "corrupt-tags");
    let store = s.store();
    let n = created(&store, "Failure", "linker could not find libssl");
    s.raw()
        .execute(
            "UPDATE node SET tags = ?1 WHERE id = ?2",
            rusqlite::params![CORRUPT_JSON, n.id],
        )
        .unwrap();

    // Act
    let read = store.get(&nid(&n.id), false);

    // Assert
    let err = read.expect_err("corrupt tags must not read back as a node with empty tags");
    assert_chained_json_error(&err);
}

#[test]
fn malformed_props_json_on_a_node_is_a_chained_error_not_a_null_props_value() {
    let s = Scratch::new("kaban-inv", "corrupt-props");
    let store = s.store();
    let n = created(&store, "Failure", "tests time out under valgrind");
    s.raw()
        .execute(
            "UPDATE node SET props = ?1 WHERE id = ?2",
            rusqlite::params![CORRUPT_JSON, n.id],
        )
        .unwrap();

    let err = store
        .get(&nid(&n.id), false)
        .expect_err("corrupt props must not read back as a node with null props");
    assert_chained_json_error(&err);
}

#[test]
fn malformed_path_scope_json_on_a_node_is_a_chained_error_not_an_unscoped_node() {
    let s = Scratch::new("kaban-inv", "corrupt-path-scope");
    let store = s.store();
    let n = created(&store, "Decision", "crates/kaban-core owns the model");
    s.raw()
        .execute(
            "UPDATE node SET path_scope = ?1 WHERE id = ?2",
            rusqlite::params![CORRUPT_JSON, n.id],
        )
        .unwrap();

    let err = store
        .get(&nid(&n.id), false)
        .expect_err("corrupt path_scope must not read back as a node scoped to everything");
    assert_chained_json_error(&err);
}

#[test]
fn malformed_endpoint_kinds_json_on_a_relationship_is_a_chained_error_not_an_unconstrained_rel() {
    let s = Scratch::new("kaban-inv", "corrupt-endpoint-kinds");
    let store = s.store();
    s.raw()
        .execute(
            "UPDATE schema_type SET from_kinds = ?1 WHERE kind = 'rel' AND name = 'CAUSED_BY'",
            rusqlite::params![CORRUPT_JSON],
        )
        .unwrap();

    let err = store
        .schema()
        .expect_err("corrupt from_kinds must not read back as a relationship allowing any source");
    assert_chained_json_error(&err);
}

#[test]
fn malformed_props_schema_json_on_a_type_is_a_chained_error_not_a_type_without_a_schema() {
    let s = Scratch::new("kaban-inv", "corrupt-props-schema");
    let store = s.store();
    s.raw()
        .execute(
            "UPDATE schema_type SET props_schema = ?1 WHERE kind = 'rel' AND name = 'FIXED_BY'",
            rusqlite::params![CORRUPT_JSON],
        )
        .unwrap();

    let err = store
        .schema()
        .expect_err("a corrupt props_schema must not read back as a type without a schema");
    assert_chained_json_error(&err);
}

#[test]
fn malformed_props_json_on_an_edge_is_a_chained_error_not_an_edge_with_null_props() {
    let s = Scratch::new("kaban-inv", "corrupt-edge-props");
    let store = s.store();
    let failure = created(&store, "Failure", "build broke after the bump");
    let fix = created(&store, "Insight", "pin the linker flags");
    let edge = store
        .link(
            &nid(&failure.id),
            &nid(&fix.id),
            "FIXED_BY",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    s.raw()
        .execute(
            "UPDATE edge SET props = ?1 WHERE id = ?2",
            rusqlite::params![CORRUPT_JSON, edge.id],
        )
        .unwrap();

    let err = store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![nid(&failure.id)],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Both,
            max_depth: 1,
            node_kinds: vec![],
            limit: 10,
            workspace_id: None,
            ..Default::default()
        })
        .expect_err("a corrupt edge props column must not traverse as an edge with null props");
    assert_chained_json_error(&err);
}

#[test]
fn an_unreadable_db_id_row_fails_verification_open_with_the_read_error_not_as_a_missing_id() {
    // Arrange: a database whose db_id row holds a BLOB, so the text read fails (the store used
    // to swallow the query error and claim the database had no db_id at all)
    let s = Scratch::new("kaban-inv", "corrupt-db-id");
    drop(s.store());
    s.raw()
        .execute(
            "UPDATE index_meta SET value = X'00FF' WHERE key = 'db_id'",
            [],
        )
        .unwrap();

    // Act
    let err = SqliteStore::open_existing_key(&s.db(), &s.keys(), &ActorId::default())
        .expect_err("an unreadable db_id must not open for verification");

    // Assert: the read error itself surfaces, not the "no db_id" diagnosis
    assert!(
        !matches!(err, kaban_core::StoreError::Audit(_)),
        "query error misreported as a missing db_id: {err}"
    );
    assert!(
        matches!(err, kaban_core::StoreError::Backend { .. }),
        "{err:?}"
    );
}

#[test]
fn a_backend_failure_keeps_the_driver_error_as_its_source_behind_a_content_free_message() {
    // Arrange: a store whose `index_meta` table disappears underneath it (MT-8: the driver error
    // must stay inspectable through `source()` while the displayed text names no SQL object)
    let s = Scratch::new("kaban-inv", "backend-source");
    let store = s.store();
    s.raw()
        .execute_batch("ALTER TABLE index_meta RENAME TO index_meta_gone")
        .unwrap();

    // Act
    let err = store
        .index_meta()
        .expect_err("a missing table must surface as a backend failure");

    // Assert: the variant is the driver-neutral one and its message is content-free
    assert!(
        matches!(err, kaban_core::StoreError::Backend { .. }),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "storage backend error");
    // ... while the chain keeps the driver's own error object, with its diagnosis intact,
    // behind the `BackendSource` holder (RM-1): the holder is the source, and it renders no
    // driver text itself, so a chain walker never prints the SQL object name
    let source = std::error::Error::source(&err).expect("a backend failure must keep its source");
    let holder = source
        .downcast_ref::<kaban_core::BackendSource>()
        .expect("the source is the backend holder");
    assert!(
        !format!("{err:?}").contains("index_meta") && !format!("{holder}").contains("index_meta"),
        "the driver's diagnosis leaked through Debug or the holder's Display: {err:?}"
    );
    let driver = holder
        .driver()
        .downcast_ref::<rusqlite::Error>()
        .expect("the held driver error must be the rusqlite error itself, not a string copy");
    assert!(
        driver.to_string().contains("index_meta"),
        "driver diagnosis lost: {driver}"
    );
}
