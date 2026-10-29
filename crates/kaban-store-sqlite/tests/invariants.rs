//! Cloud-readiness invariants as tests (core-spec F17 as clarified by PR-6; core-plan C2.8):
//! no autoincrement ids and no exposed row ids, append-only `events` and `audit`, the tenant
//! key on every tenant-scoped table and on none of the DB-scoped bookkeeping tables, content-hash
//! dedupe, and `SUPERSEDES` recorded as an event instead of an edit of the governing record.
//!
//! The schema checks read `sqlite_master` and `pragma_table_info` through a second connection on
//! a file-backed store (the store exposes no SQL on purpose); the behavioural checks go through
//! `GraphStore`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::{
    ActorId, AuditStore, GovernanceStore, IngestStore, Lifecycle, MemoryStore, NewNode, Node,
    NodeId, RememberOutcome, SchemaStore, WorkspaceId,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::Connection;
use std::collections::BTreeSet;

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

/// Every table in `sqlite_master` (including FTS shadow tables).
fn tables(conn: &Connection) -> Vec<String> {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// `(column name, notnull)` of `table`; a PRIMARY KEY column counts as NOT NULL (the 0004
/// `discovered_overflow(workspace_id TEXT PRIMARY KEY)` of engine-spec §3.2, taken verbatim).
fn columns(conn: &Connection, table: &str) -> Vec<(String, bool)> {
    let mut st = conn
        .prepare(&format!(
            "SELECT name, \"notnull\", pk FROM pragma_table_info('{table}')"
        ))
        .unwrap();
    st.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get::<_, i64>(1)? != 0 || r.get::<_, i64>(2)? != 0,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn no_table_uses_autoincrement_and_the_public_node_has_no_row_id() {
    // Arrange
    let s = Scratch::new("kaban-inv", "autoincrement");
    let store = s.store();
    let node = created(&store, "Fact", "ids are uuidv7, never counters");

    // Act
    let conn = s.raw();
    let ddl_with_autoincrement: Vec<String> = {
        let mut st = conn
            .prepare("SELECT name FROM sqlite_master WHERE sql LIKE '%AUTOINCREMENT%'")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };

    // Assert: no AUTOINCREMENT anywhere (also no `sqlite_sequence` table), and the public model
    // exposes a UUIDv7 id only. The destructuring below names every field of `Node` without
    // `..`, so adding a `rid` (or any row-id) field would fail to compile here.
    assert!(
        ddl_with_autoincrement.is_empty(),
        "{ddl_with_autoincrement:?}"
    );
    assert!(!tables(&conn).iter().any(|t| t == "sqlite_sequence"));
    let Node {
        id,
        kind: _,
        text: _,
        summary: _,
        tags: _,
        confidence: _,
        importance: _,
        lifecycle: _,
        governance: _,
        store_scope: _,
        path_scope: _,
        rev: _,
        serialized_rev: _,
        dup_key: _,
        created_at: _,
        updated_at: _,
        valid_from: _,
        valid_to: _,
        session_id: _,
        agent_id: _,
        workspace_id: _,
        repo: _,
        commit_sha: _,
        accepted_commit: _,
        props: _,
        content_hash: _,
        policy_version: _,
    } = node;
    assert!(NodeId::parse(&id).is_ok(), "{id}");
}

#[test]
fn events_and_audit_rows_can_neither_be_updated_nor_deleted() {
    // Arrange: one governance event (through SUPERSEDES) and one audit row (through remember)
    let s = Scratch::new("kaban-inv", "append-only");
    let store = s.store();
    let old = created(&store, "Decision", "deploy on fridays");
    store
        .transition(
            &nid(&old.id),
            kaban_core::Governance::Accepted,
            kaban_core::NewEvent::default(),
            &ctx(),
        )
        .unwrap();
    let new = created(&store, "Decision", "deploy on tuesdays");
    store
        .link(
            &nid(&new.id),
            &nid(&old.id),
            "SUPERSEDES",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let mut audited = ctx();
    audited.audit.push(kaban_core::AuditEntry {
        op: kaban_core::AuditOp::Redact,
        tool: "test".into(),
        field: "text".into(),
        workspace_id: WorkspaceId::default(),
        policy_version: "pv".into(),
        input_hmac: "00".into(),
        rules: vec!["r".into()],
        node_id: None,
    });
    store.audit_append(&audited.audit).unwrap();
    let conn = s.raw();
    let events_before: i64 = conn
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let audit_before: i64 = conn
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    assert!(events_before >= 1 && audit_before >= 1);

    // Act
    let results = [
        conn.execute("UPDATE events SET reason = 'edited'", []),
        conn.execute("DELETE FROM events", []),
        conn.execute("UPDATE audit SET rules = '[]'", []),
        conn.execute("DELETE FROM audit", []),
    ];

    // Assert: every statement is refused and nothing changed
    for (i, r) in results.iter().enumerate() {
        let err = r
            .as_ref()
            .expect_err(&format!("statement {i} was accepted"));
        assert!(
            err.to_string().contains("append-only"),
            "statement {i}: {err}"
        );
    }
    let events_after: i64 = conn
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let audit_after: i64 = conn
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!((events_after, audit_after), (events_before, audit_before));
    let reasons: Vec<String> = {
        let mut st = conn.prepare("SELECT reason FROM events").unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert!(!reasons.iter().any(|r| r == "edited"), "{reasons:?}");
}

/// Tenant-scoped tables (F17 as clarified by PR-6; the 0004 overlay tables of engine-spec §3.2,
/// E1.7), plus the ones that appear with a vector tier.
const TENANT_SCOPED: &[&str] = &[
    "node",
    "edge",
    "events",
    "changes",
    "audit",
    "assertion",
    "node_anchor",
    "kind_stats",
    "discovered_overflow",
    "node_candidate",
    "derived_artifact",
    "ingest_jobs",
    "pruning_protection",
];
const TENANT_SCOPED_IF_PRESENT: &[&str] = &["embeddings", "similar_candidates"];
/// DB-scoped bookkeeping tables: no workspace column by design (`session_edits` since
/// migration 0003, M4-13), and the 0004 child tables keyed by their parent row
/// (`assertion_status`, `node_identity`: the parent carries the workspace).
const DB_SCOPED: &[&str] = &[
    "projections",
    "leases",
    "conflicts",
    "index_meta",
    "schema_type",
    "confirmations",
    "read_audit_seen",
    "session_edits",
    "assertion_status",
    "node_identity",
];

#[test]
fn exactly_the_tenant_scoped_tables_carry_a_not_null_workspace_id() {
    // Arrange
    let s = Scratch::new("kaban-inv", "tenant-key");
    let _store = s.store();
    let conn = s.raw();
    let all = tables(&conn);

    // Act: which tables have a NOT NULL `workspace_id`, which have one at all
    let mut not_null = BTreeSet::new();
    let mut any = BTreeSet::new();
    for t in &all {
        for (col, notnull) in columns(&conn, t) {
            if col == "workspace_id" {
                any.insert(t.clone());
                if notnull {
                    not_null.insert(t.clone());
                }
            }
        }
    }

    // Assert: the NOT NULL set is exactly the tenant-scoped tables that exist; no bookkeeping
    // table has the column; every bookkeeping table the plan names and that exists is covered
    let expected: BTreeSet<String> = TENANT_SCOPED
        .iter()
        .chain(TENANT_SCOPED_IF_PRESENT.iter())
        .filter(|t| all.iter().any(|x| x == *t))
        .map(ToString::to_string)
        .collect();
    assert_eq!(not_null, expected, "tables with workspace_id NOT NULL");
    assert_eq!(
        any, not_null,
        "a table with a nullable workspace_id would be neither tenant- nor DB-scoped"
    );
    for t in DB_SCOPED {
        if all.iter().any(|x| x == t) {
            assert!(
                !any.contains(*t),
                "bookkeeping table {t} must not carry workspace_id"
            );
        }
    }
    for t in TENANT_SCOPED {
        assert!(
            all.iter().any(|x| x == t),
            "tenant-scoped table {t} missing"
        );
    }
    for t in DB_SCOPED {
        assert!(
            all.iter().any(|x| x == t),
            "bookkeeping table {t} missing (every DB-scoped table exists since 0003, M4-13)"
        );
    }
}

#[test]
fn a_node_edge_event_or_audit_row_without_a_workspace_is_refused() {
    // Arrange: two real nodes so an edge insert can reference existing rows
    let s = Scratch::new("kaban-inv", "null-workspace");
    let store = s.store();
    let a = created(&store, "Fact", "first endpoint");
    let b = created(&store, "Fact", "second endpoint");
    let conn = s.raw();
    let rid = |id: &str| -> i64 {
        conn.query_row("SELECT rid FROM node WHERE id = ?", [id], |r| r.get(0))
            .unwrap()
    };
    let now = kaban_core::now_iso();

    // Act
    let node = conn.execute(
        "INSERT INTO node(id, kind, text, created_at, updated_at, workspace_id, content_hash, policy_version)
         VALUES (?, 'Fact', 'tenantless', ?, ?, NULL, 'h', 'pv')",
        rusqlite::params![kaban_core::new_id(), now, now],
    );
    let edge = conn.execute(
        "INSERT INTO edge(id, src, dst, rel, created_at, workspace_id) VALUES (?, ?, ?, 'RELATES_TO', ?, NULL)",
        rusqlite::params![kaban_core::new_id(), rid(&a.id), rid(&b.id), now],
    );
    let event = conn.execute(
        "INSERT INTO events(node_id, kind, reason, signer_kind, ts, workspace_id) VALUES (?, 'accepted', 'r', 'agent', 0, NULL)",
        [&a.id],
    );
    let audit = conn.execute(
        "INSERT INTO audit(ts, op, tool, field, workspace_id, policy_version, input_hmac, rules, prev_hmac, entry_hmac)
         VALUES (0, 'redact', 't', 'f', NULL, 'pv', '00', '[]', '', '')",
        [],
    );

    // Assert: each insert fails on the NOT NULL constraint
    for (name, r) in [
        ("node", node),
        ("edge", edge),
        ("events", event),
        ("audit", audit),
    ] {
        let err = r.expect_err(&format!("{name} row without workspace_id was accepted"));
        assert!(
            err.to_string().contains("NOT NULL constraint failed"),
            "{name}: {err}"
        );
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
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

#[test]
fn supersedes_records_an_event_and_leaves_the_old_records_text_untouched() {
    // Arrange: an accepted decision and its replacement
    let s = Scratch::new("kaban-inv", "supersedes");
    let store = s.store();
    let old = created(&store, "Decision", "we deploy on fridays");
    store
        .transition(
            &nid(&old.id),
            kaban_core::Governance::Accepted,
            kaban_core::NewEvent {
                reason: "team vote".into(),
                ..Default::default()
            },
            &ctx(),
        )
        .unwrap();
    let new = created(&store, "Decision", "we deploy on tuesdays");

    // Act
    store
        .link(
            &nid(&new.id),
            &nid(&old.id),
            "SUPERSEDES",
            None,
            None,
            &ctx(),
        )
        .unwrap();

    // Assert: the old record keeps its text, moves to archived/superseded with a new rev, and
    // the transition is an appended event; the new record is untouched
    let old_now = store.get(&nid(&old.id), true).unwrap().unwrap();
    assert_eq!(old_now.text, "we deploy on fridays");
    assert_eq!(old_now.lifecycle, Lifecycle::Archived);
    assert_eq!(old_now.governance, kaban_core::Governance::Superseded);
    assert!(old_now.valid_to.is_some());
    assert!(old_now.rev > old.rev);
    let events = store.events(&nid(&old.id)).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["accepted", "superseded"], "{events:?}");
    assert!(events[1].reason.contains("SUPERSEDES"), "{events:?}");
    let new_now = store.get(&nid(&new.id), false).unwrap().unwrap();
    assert_eq!(new_now.text, "we deploy on tuesdays");
    assert_eq!(new_now.rev, new.rev);
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
            governance: vec![],
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
