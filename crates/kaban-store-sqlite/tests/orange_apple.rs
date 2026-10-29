//! The EN-F13 normative fixture "orange or apple" through `apply_batch` (engine-plan E2.3
//! item 4; E2.5 the graph level): the a1..a7 rows of `fixtures/reconcile/orange-apple.json` as
//! batch claims against the `infra` ontology, and the persisted outcome of every span and
//! variant (statuses, verdict counts, review rows, the exact `edge` set and the `proposed`
//! records the span winners materialise). The integers behind each verdict (S, P, margin) are
//! pinned at the core level by E2.1's copy of the fixture (`tests/reconcile_props.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ChangeLogStore, ClaimClass, EdgeDiff, Fixed, IngestStore,
    Layer, MemoryStore, Method, NamespaceSet, NewNode, NodeId, OntologyFile, Op, Owner, Rank,
    SchemaStore, TypeDeclaration, TypeDef, WorkspaceId, WriteBatch, WriteCtx, resolve, stored_name,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::BTreeMap;
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

/// A store reconciling against the shipped files plus the fixture's `infra` ontology, the
/// `infra` kinds declared on the store too (the service's registry load does it; a batch
/// upsert of an entity node of such a kind needs the `schema_type` row).
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

/// The fixed id of the fixture's entity node `name` (`nodes`): the target a producer resolved
/// and names in `object` (EN-F13 target selection, default (a)).
fn entity_id(fx: &Value, name: &str) -> NodeId {
    NodeId::parse(fx["nodes"][name]["id"].as_str().unwrap()).unwrap()
}

/// The upserts of every fixture entity node, in name order: the same rows in every batch of a
/// test, so a later batch is `Unchanged` on them.
fn entity_ops(fx: &Value) -> Vec<Op> {
    fx["nodes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, n)| Op::UpsertNode {
            id: entity_id(fx, name),
            node: NewNode {
                kind: n["kind"].as_str().unwrap().into(),
                text: n["text"].as_str().unwrap().into(),
                ..Default::default()
            },
        })
        .collect()
}

/// One fixture row as the claim a producer batch would carry about chunk `c17`; `object` is
/// the entity row the fixture names, when it names one.
fn claim(fx: &Value, row: &Value, chunk: &NodeId) -> BatchAssertion {
    let text = |k: &str| row[k].as_str().unwrap();
    BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: u32_of(&row["offset"]),
            len: u32_of(&row["len"]),
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: row
            .get("object")
            .and_then(Value::as_str)
            .map(|name| entity_id(fx, name)),
        value: row["value"].clone(),
        layer: Layer::parse(text("layer")).unwrap(),
        method: Method::parse(text("method")).unwrap(),
        extractor_id: Some(text("id").to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("c17".into()),
        lineage_id: text("lineage_id").to_owned(),
        raw_score: Fixed::new(u32_of(&row["raw_score"])).unwrap(),
        rank: row
            .get("rank")
            .and_then(Value::as_str)
            .and_then(Rank::parse)
            .unwrap_or(Rank::Normal),
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// The fixture rows named by `ids`, in that order.
fn rows<'a>(fx: &'a Value, ids: &[&str]) -> Vec<&'a Value> {
    ids.iter()
        .map(|id| {
            fx["assertions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == *id)
                .unwrap()
        })
        .collect()
}

fn ids_of(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect()
}

/// `extractor_id -> disposition` of every claim (the fixture id travels in `extractor_id`).
fn statuses(conn: &Connection) -> BTreeMap<String, String> {
    conn.prepare(
        "SELECT a.extractor_id, s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every `assertion_status` column but `assertion_id` and `updated_at`, keyed by claim key.
fn status_rows(conn: &Connection) -> BTreeMap<String, (String, String, String)> {
    conn.prepare(
        "SELECT a.key, s.disposition, s.group_key, s.verdict_rule FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// A node's text as the fixture spells it: the chunk is `c17`, every other node its text.
fn label(fx: &Value, text: &str) -> String {
    if text == fx["chunk"]["text"].as_str().unwrap() {
        fx["chunk"]["id"].as_str().unwrap().to_owned()
    } else {
        text.to_owned()
    }
}

fn review_rows(fx: &Value, conn: &Connection) -> Vec<(String, String)> {
    conn.prepare(
        "SELECT kaban_body(n.text), c.reason FROM node_candidate c JOIN node n ON n.rid = c.rid ORDER BY c.rid",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))
    .unwrap()
    .map(|r| r.map(|(text, reason)| (label(fx, &text), reason)))
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every `edge` row of the store as the fixture's `edges` rows spell them: `[src, rel, dst,
/// layer, method, score in units of 1/10000, offset, len]`, ordered by `(rel, dst)`.
fn edges(fx: &Value, conn: &Connection) -> Value {
    let rows: Vec<Value> = conn
        .prepare(
            "SELECT kaban_body(s.text), e.rel, kaban_body(d.text), e.layer, e.method, CAST(round(e.score * 10000) AS INTEGER), e.offset, e.len
             FROM edge e JOIN node s ON s.rid = e.src JOIN node d ON d.rid = e.dst ORDER BY e.rel, d.text",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(serde_json::json!([
                label(fx, &r.get::<_, String>(0)?),
                r.get::<_, String>(1)?,
                label(fx, &r.get::<_, String>(2)?),
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ]))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    Value::Array(rows)
}

/// Every record the reconciler proposed (`props.proposed_by = kaban-reconcile`) as the
/// fixture's `records` rows spell them: `[kind, text, governance]`, ordered by `(kind, text)`.
fn records(conn: &Connection) -> Value {
    let rows: Vec<Value> = conn
        .prepare(
            "SELECT kind, kaban_body(text), governance FROM node
             WHERE json_extract(props, '$.proposed_by') = 'kaban-reconcile' ORDER BY kind, text",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(serde_json::json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?
            ]))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    Value::Array(rows)
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn expected_statuses(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(k, d)| (k.clone(), d.as_str().unwrap().to_owned()))
        .collect()
}

fn verdict_counts(report: &kaban_core::BatchReport) -> BTreeMap<String, u32> {
    report
        .verdicts
        .iter()
        .map(|(k, n)| (k.as_str().to_owned(), *n))
        .collect()
}

fn expected_counts(v: &Value) -> BTreeMap<String, u32> {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(k, n)| (k.clone(), u32::try_from(n.as_u64().unwrap()).unwrap()))
        .collect()
}

/// The fixture's entity nodes and the chunk `c17` (its real text, so a span names bytes)
/// written and the named rows submitted as one batch; returns the report.
fn submit(
    store: &SqliteStore,
    fx: &Value,
    chunk: &NodeId,
    ids: &[&str],
    extra: Vec<BatchAssertion>,
) -> kaban_core::BatchReport {
    let mut ops = entity_ops(fx);
    ops.push(Op::UpsertNode {
        id: chunk.clone(),
        node: NewNode {
            kind: "Chunk".into(),
            text: fx["chunk"]["text"].as_str().unwrap().into(),
            ..Default::default()
        },
    });
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    // the producer's diff (`kaban-ingest`'s choice): its own unnamed edges go, the
    // projection's rows (`layer` set, owner `kaban-reconcile` or `human`) are not its to drop
    b.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    b.assertions = extra;
    b.assertions
        .extend(rows(fx, ids).into_iter().map(|r| claim(fx, r, chunk)));
    store.apply_batch(b, &ctx()).unwrap()
}

/// Slice (EN-F13, table over the fixture's variants): `forward` (span 27..32 Vendor wins over
/// a1/a3 with a2 dissent and materialises the one `MENTIONS` edge to the resolved "Apple Inc."
/// scored `6556`; span 27..40 Hardware wins alone with no target and materialises the
/// `proposed` record `apple silicon` with its `EVIDENCE_FOR`; span 56..62 is `Candidate` below
/// the margin, names no object and so holds no `node_candidate` row (OWN116: the source chunk
/// is never the candidate), with nothing in the graph) and `without_llm`
/// (span 27..40 absent, so no record; span 56..62 still a candidate). The edge set is exact.
#[test]
fn the_orange_apple_spans_decide_as_the_fixture_says_in_every_variant() {
    let fx = fixture();
    for (name, variant) in fx["variants"].as_object().unwrap() {
        let s = Scratch::new("e23-orange", name);
        let store = store_with_infra(&s, &fx);
        let chunk = NodeId::new();
        let report = submit(&store, &fx, &chunk, &ids_of(&variant["submit"]), vec![]);
        let conn = raw(&s);
        assert_eq!(
            statuses(&conn),
            expected_statuses(&variant["statuses"]),
            "variant {name}: statuses"
        );
        assert_eq!(
            verdict_counts(&report),
            expected_counts(&variant["verdicts"]),
            "variant {name}: verdicts"
        );
        let expected_reviews: Vec<(String, String)> = variant["review_rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r[0].as_str().unwrap().to_owned(),
                    r[1].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            review_rows(&fx, &conn),
            expected_reviews,
            "variant {name}: review"
        );
        assert_eq!(
            edges(&fx, &conn),
            variant["edges"],
            "variant {name}: the exact edge set the span winners materialise"
        );
        assert_eq!(
            records(&conn),
            variant["records"],
            "variant {name}: the proposed records"
        );
    }
}

/// Variant "a3 twice" (EN-F12b): a3 re-submitted from the same lineage is the same key, zero
/// new rows, zero `ChangeLog` rows, the same statuses.
#[test]
fn a3_resubmitted_from_the_same_lineage_writes_nothing_and_keeps_the_verdict() {
    let fx = fixture();
    let s = Scratch::new("e23-orange", "a3-twice");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    submit(&store, &fx, &chunk, &["a1", "a2", "a3"], vec![]);
    let conn = raw(&s);
    let before = status_rows(&conn);
    let seq = store.changes_read(0, 10_000).unwrap().len() as u64;
    let again = submit(&store, &fx, &chunk, &["a3"], vec![]);
    assert_eq!((again.assertions_inserted, again.assertions_noop), (0, 1));
    assert_eq!(count(&conn, "SELECT count(*) FROM assertion"), 3);
    assert_eq!(store.changes_read(seq, 10_000).unwrap().len(), 0);
    assert_eq!(status_rows(&conn), before);
}

/// Variant "reverse order" (EN-F12a rule (4), RP-10): the full a1..a7 set resubmitted in
/// reverse into a store that holds it writes zero `assertion` rows, zero `ChangeLog` rows,
/// zero new `edge` or `node` rows (the materialised graph is re-decided in place); a fresh
/// store fed the reversed order ends with `assertion_status` rows identical to the forward run
/// in every column but `assertion_id` and `updated_at`, and the same edge set and records.
#[test]
fn the_full_set_resubmitted_in_reverse_order_writes_nothing_and_matches_the_forward_statuses() {
    let fx = fixture();
    let forward: Vec<&str> = ids_of(&fx["variants"]["forward"]["submit"]);
    let reversed: Vec<&str> = forward.iter().rev().copied().collect();
    let s = Scratch::new("e23-orange", "reverse-same");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    submit(&store, &fx, &chunk, &forward, vec![]);
    let conn = raw(&s);
    let before = status_rows(&conn);
    let graph_before = (
        count(&conn, "SELECT count(*) FROM edge"),
        count(&conn, "SELECT count(*) FROM node"),
        edges(&fx, &conn),
        records(&conn),
    );
    let seq = store.changes_read(0, 10_000).unwrap().len() as u64;
    let again = submit(&store, &fx, &chunk, &reversed, vec![]);
    assert_eq!((again.assertions_inserted, again.assertions_noop), (0, 7));
    assert_eq!(store.changes_read(seq, 10_000).unwrap().len(), 0);
    assert_eq!(status_rows(&conn), before);
    assert_eq!(
        (
            count(&conn, "SELECT count(*) FROM edge"),
            count(&conn, "SELECT count(*) FROM node"),
            edges(&fx, &conn),
            records(&conn),
        ),
        graph_before,
        "a re-decide writes no new edge or node row"
    );

    let fresh = Scratch::new("e23-orange", "reverse-fresh");
    let other = store_with_infra(&fresh, &fx);
    submit(&other, &fx, &NodeId::new(), &reversed, vec![]);
    let strip =
        |rows: BTreeMap<String, (String, String, String)>| -> BTreeMap<String, (String, String)> {
            // the group key names the chunk rid, which differs per store; the disposition and
            // the rule are the comparable columns
            rows.into_iter()
                .map(|(k, (d, _, rule))| (k, (d, rule)))
                .collect()
        };
    let fresh_conn = raw(&fresh);
    assert_eq!(strip(status_rows(&fresh_conn)), strip(before));
    assert_eq!(edges(&fx, &fresh_conn), fx["variants"]["forward"]["edges"]);
    assert_eq!(records(&fresh_conn), fx["variants"]["forward"]["records"]);
}

/// Every group key of a store, ascending (the EN-F12a group order).
fn group_keys(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT DISTINCT group_key FROM assertion_status ORDER BY group_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Variant "reverse order", the `explain` half (EN-F12a rule (4) "the same `explain` bytes";
/// engine-plan E2.4 item 4): a fresh store fed a1..a7 forward and a fresh store fed the same
/// rows in reverse hold the same three groups, and the serialised `explain_group` of each is
/// byte-identical between the two (the lineage order inside a value and the candidate order
/// are functions of the stable keys and the sums, never of arrival).
#[test]
fn the_explanations_of_the_three_groups_are_byte_identical_between_the_forward_and_the_reversed_store()
 {
    let fx = fixture();
    let forward: Vec<&str> = ids_of(&fx["variants"]["forward"]["submit"]);
    let reversed: Vec<&str> = forward.iter().rev().copied().collect();
    let f = Scratch::new("e24-explain", "forward");
    let store = store_with_infra(&f, &fx);
    submit(&store, &fx, &NodeId::new(), &forward, vec![]);
    let r = Scratch::new("e24-explain", "reversed");
    let other = store_with_infra(&r, &fx);
    submit(&other, &fx, &NodeId::new(), &reversed, vec![]);
    let (forward_groups, reversed_groups) = (group_keys(&raw(&f)), group_keys(&raw(&r)));
    assert_eq!(forward_groups.len(), 3, "{forward_groups:?}");
    assert_eq!(forward_groups, reversed_groups, "the same three groups");
    for key in &forward_groups {
        let a = serde_json::to_vec(&store.explain_group(key).unwrap()).unwrap();
        let b = serde_json::to_vec(&other.explain_group(key).unwrap()).unwrap();
        assert_eq!(
            a,
            b,
            "group {key}: forward {} / reversed {}",
            String::from_utf8_lossy(&a),
            String::from_utf8_lossy(&b)
        );
    }
}

/// Variant "human first" (EN-F12c): a human `preferred` claim on span 27..32 (a `link` to
/// "Apple Inc.") before a1..a3 wins the span alone; a1 and a3 (the same value) are `dissent`,
/// a2 (another value) is `unresolved`; the one `MENTIONS` edge carries `layer: human` and the
/// layers joining afterwards leave it byte-identical (no record: the human named the target).
#[test]
fn a_human_claim_first_wins_the_span_with_agreeing_dissent_and_an_unresolved_other_value() {
    let fx = fixture();
    let s = Scratch::new("e23-orange", "human-first");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    submit(
        &store,
        &fx,
        &chunk,
        &[],
        vec![claim(&fx, &fx["human_first"], &chunk)],
    );
    let conn = raw(&s);
    assert_eq!(
        edges(&fx, &conn),
        fx["human_first_edges"],
        "the human's edge"
    );
    let report = submit(&store, &fx, &chunk, &["a1", "a2", "a3"], vec![]);
    assert_eq!(
        verdict_counts(&report),
        BTreeMap::from([("winner".to_owned(), 1)])
    );
    let expected: BTreeMap<String, String> = [
        ("h1", "winner"),
        ("a1", "dissent"),
        ("a2", "unresolved"),
        ("a3", "dissent"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(statuses(&conn), expected);
    assert_eq!(count(&conn, "SELECT count(*) FROM node_candidate"), 0);
    assert_eq!(
        edges(&fx, &conn),
        fx["human_first_edges"],
        "the layers joining a protected span leave the human's row"
    );
    assert_eq!(records(&conn), Value::Array(vec![]));
}
