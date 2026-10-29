//! EN-F13 retention and removal of a human's `MENTIONS` row (OWN-83 (2); engine-plan E2.5f):
//! a human span claim on the orange-apple chunk whose claim a rescan retires (EN-S9
//! `lost_member`) keeps its `MENTIONS` row until a human forgets the record it points to; the
//! forget of another record leaves the row in place. The fixture is the `human_first` shape
//! of `tests/orange_apple.rs` with a second human span claim naming a second vendor, so the
//! chunk has another edge the forget must not touch.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, EdgeDiff, Fixed, ForgetMode, IngestStore,
    Layer, MemoryStore, Method, NamespaceSet, NewNode, NodeId, OntologyFile, Op, Owner, Rank,
    ReadFilters, SchemaStore, TraverseDirection, TraverseQuery, TypeDeclaration, TypeDef,
    WorkspaceId, WriteBatch, WriteCtx, resolve, stored_name,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::sync::Arc;

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/reconcile/orange-apple.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn human() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli)
}

/// The store of `tests/orange_apple.rs`: the shipped files plus the fixture's `infra`
/// ontology, the `infra` kinds declared on the store too.
fn store_with_infra(s: &Scratch, fx: &Value) -> SqliteStore {
    let mut files = kaban_core::shipped::files().unwrap();
    files.push(OntologyFile::from_yaml(fx["ontology"].as_str().unwrap()).unwrap());
    let ontology = resolve(&files, &NamespaceSet::new()).unwrap();
    let infra: Vec<String> = ontology
        .kinds
        .keys()
        .filter(|q| q.ns.as_str() == "infra")
        .map(stored_name)
        .collect();
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap();
    for name in infra {
        store
            .schema_extend(TypeDef {
                kind: "label".into(),
                name,
                ns: None,
                description: "declared".into(),
                from_kinds: vec![],
                to_kinds: vec![],
                directed: true,
                props_schema: None,
                builtin: false,
                declaration: TypeDeclaration::default(),
            })
            .unwrap();
    }
    store
}

/// A read-only connection that reads body columns as kaban does (`kaban_body`).
fn raw(s: &Scratch) -> Connection {
    let conn = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    conn
}

fn u32_of(v: &Value) -> u32 {
    u32::try_from(v.as_u64().unwrap()).unwrap()
}

/// The fixture's `human_first` row (a human `link` on span 27..32, `preferred`) re-aimed at
/// `object`, with `id` as the fixture id and `offset`/`len` as the span.
fn human_claim(
    fx: &Value,
    chunk: &NodeId,
    id: &str,
    offset: u32,
    len: u32,
    object: &NodeId,
) -> BatchAssertion {
    let row = &fx["human_first"];
    let text = |k: &str| row[k].as_str().unwrap();
    BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset,
            len,
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: Some(object.clone()),
        value: row["value"].clone(),
        layer: Layer::parse(text("layer")).unwrap(),
        method: Method::parse(text("method")).unwrap(),
        extractor_id: Some(id.to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("c17".into()),
        lineage_id: format!("human:cli:{id}"),
        raw_score: Fixed::new(u32_of(&row["raw_score"])).unwrap(),
        rank: Rank::parse(text("rank")).unwrap(),
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// Every `edge` row as `[src text, rel, dst text, layer, method, score/10000, offset, len,
/// id, created_at, props]`, ordered by `(rel, dst text)`: the whole row, so "untouched" is
/// byte-identical.
fn edge_rows(conn: &Connection) -> Vec<Value> {
    conn.prepare(
        "SELECT kaban_body(s.text), e.rel, kaban_body(d.text), e.layer, e.method, CAST(round(e.score * 10000) AS INTEGER),
                e.offset, e.len, e.id, e.created_at, e.props
         FROM edge e JOIN node s ON s.rid = e.src JOIN node d ON d.rid = e.dst ORDER BY e.rel, d.text",
    )
    .unwrap()
    .query_map([], |r| {
        Ok(serde_json::json!([
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, Option<i64>>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
        ]))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The `MENTIONS` destinations a depth-1 traverse out of `chunk` reaches, by node text.
fn mentions_from(store: &SqliteStore, chunk: &NodeId) -> Vec<String> {
    let res = store
        .traverse(&TraverseQuery {
            start_ids: vec![chunk.clone()],
            rels: vec!["MENTIONS".into()],
            direction: TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec![],
            kind_set: None,
            edge_filter: None,
            governance: vec![],
            limit: 10,
            workspace_id: None,
            filters: ReadFilters::default(),
            follow_anchors: false,
            anchor_repo: None,
        })
        .unwrap();
    let mut out: Vec<String> = res
        .edges
        .iter()
        .map(|e| {
            res.nodes
                .iter()
                .find(|n| n.id.as_str() == e.dst)
                .unwrap()
                .text
                .clone()
        })
        .collect();
    out.sort();
    out
}

struct Fixture {
    scratch: Scratch,
    store: SqliteStore,
    chunk: NodeId,
    apple: NodeId,
    orange: NodeId,
}

/// The chunk `c17`, the fixture's "Apple Inc." vendor and a second vendor "Orange SA", one
/// human span claim to each (`h1` on 27..32 "apple", `h2` on 56..62 "orange"), submitted as
/// one batch: two human `MENTIONS` rows, nothing else in the graph. Then the rescan under
/// policy `v2` masks `h1` (its value is re-judged, EN-S9): the apple row is `lost_member`
/// and, being a human's, stays (EN-F13); the orange row is untouched.
fn retired_human_row(name: &str) -> Fixture {
    let fx = fixture();
    let scratch = Scratch::new("e25f-forget", name);
    let store = store_with_infra(&scratch, &fx);
    let chunk = NodeId::new();
    let apple = NodeId::parse(fx["nodes"]["apple"]["id"].as_str().unwrap()).unwrap();
    let orange = NodeId::new();
    let vendor = fx["nodes"]["apple"]["kind"].as_str().unwrap();
    let ops = vec![
        Op::UpsertNode {
            id: apple.clone(),
            node: NewNode {
                kind: vendor.into(),
                text: fx["nodes"]["apple"]["text"].as_str().unwrap().into(),
                ..Default::default()
            },
        },
        Op::UpsertNode {
            id: orange.clone(),
            node: NewNode {
                kind: vendor.into(),
                text: "Orange SA".into(),
                ..Default::default()
            },
        },
        Op::UpsertNode {
            id: chunk.clone(),
            node: NewNode {
                kind: "Chunk".into(),
                text: fx["chunk"]["text"].as_str().unwrap().into(),
                ..Default::default()
            },
        },
    ];
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    b.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    b.assertions = vec![
        human_claim(&fx, &chunk, "h1", 27, 5, &apple),
        human_claim(&fx, &chunk, "h2", 56, 6, &orange),
    ];
    store.apply_batch(b, &ctx()).unwrap();
    assert_eq!(
        mentions_from(&store, &chunk),
        vec!["Apple Inc.".to_owned(), "Orange SA".to_owned()],
        "the two human rows before the rescan"
    );
    let before = edge_rows(&raw(&scratch));
    assert_eq!(before.len(), 2, "{before:?}");

    // the rescan retires h1: the policy re-judges `assertion.value` and masks the row
    let report = store
        .rescan_assertions("v2", &mut |claim, _audit| {
            (claim.extractor_id.as_deref() == Some("h1"))
                .then(|| Value::String("[REDACTED]".into()))
        })
        .unwrap();
    assert_eq!((report.scanned, report.rejected), (2, 1), "{report:?}");
    let conn = raw(&scratch);
    let h1: String = conn
        .query_row(
            "SELECT s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id WHERE a.extractor_id = 'h1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(h1, "rejected", "the retired claim is out of the vote");
    assert_eq!(
        edge_rows(&conn),
        before,
        "EN-F13: a human's MENTIONS row whose claim a rescan retired stays, byte-identical"
    );
    Fixture {
        scratch,
        store,
        chunk,
        apple,
        orange,
    }
}

/// Slice (EN-F13, OWN-83 (2)): `kaban forget <record>` in the mode a human uses (the default
/// `Tombstone`) removes the retired human `MENTIONS` row to that record from every read: a
/// traverse out of the chunk no longer reaches it; the chunk and its other human row are
/// byte-identical. The row's storage follows the mode, as every other edge of the forgotten
/// node does: `Tombstone` and `Invalidate` keep the bytes behind the tombstoned endpoint,
/// `Purge` deletes the row from the `edge` table.
#[test]
fn forgetting_the_record_removes_the_retired_human_mentions_row_and_leaves_the_chunks_other_edge() {
    for (name, mode) in [
        ("tombstone", ForgetMode::Tombstone),
        ("invalidate", ForgetMode::Invalidate),
        ("purge", ForgetMode::Purge),
    ] {
        let f = retired_human_row(name);
        let conn = raw(&f.scratch);
        let before = edge_rows(&conn);
        let orange_row = before
            .iter()
            .find(|r| r[2] == "Orange SA")
            .cloned()
            .unwrap();
        let chunk_before = serde_json::to_value(f.store.get(&f.chunk, false).unwrap()).unwrap();

        f.store.forget(&f.apple, mode, &human()).unwrap();

        assert_eq!(
            mentions_from(&f.store, &f.chunk),
            vec!["Orange SA".to_owned()],
            "{name}: the MENTIONS row to the forgotten record is gone from traverse"
        );
        let after = edge_rows(&conn);
        assert_eq!(
            after.iter().find(|r| r[2] == "Orange SA"),
            Some(&orange_row),
            "{name}: the chunk's other human row is byte-identical"
        );
        assert_eq!(
            serde_json::to_value(f.store.get(&f.chunk, false).unwrap()).unwrap(),
            chunk_before,
            "{name}: the chunk itself is untouched"
        );
        let apple_rows = after.iter().filter(|r| r[2] == "Apple Inc.").count();
        let other_edges_of_apple = usize::try_from(
            conn.query_row(
                "SELECT count(*) FROM edge e JOIN node d ON d.rid = e.dst WHERE d.id = ?1",
                [f.apple.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
        )
        .unwrap();
        match mode {
            ForgetMode::Purge => assert_eq!(
                apple_rows, 0,
                "{name}: a purge deletes the human MENTIONS row from the edge table"
            ),
            _ => assert_eq!(
                apple_rows, other_edges_of_apple,
                "{name}: the row is stored as every other edge of the tombstoned node is"
            ),
        }
    }
}

/// Negative case: forgetting another record (the orange vendor) leaves the retired human row
/// to "Apple Inc." in traverse and byte-identical in the `edge` table.
#[test]
fn forgetting_another_record_leaves_the_retired_human_mentions_row_in_place() {
    let f = retired_human_row("other-record");
    let conn = raw(&f.scratch);
    let apple_row = edge_rows(&conn)
        .into_iter()
        .find(|r| r[2] == "Apple Inc.")
        .unwrap();

    f.store
        .forget(&f.orange, ForgetMode::Tombstone, &human())
        .unwrap();

    assert_eq!(
        mentions_from(&f.store, &f.chunk),
        vec!["Apple Inc.".to_owned()],
        "the retired human row to the other record is still read"
    );
    assert_eq!(
        edge_rows(&conn).into_iter().find(|r| r[2] == "Apple Inc."),
        Some(apple_row),
        "and byte-identical at rest"
    );
}
