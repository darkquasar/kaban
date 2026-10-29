//! The span cleanup of a projection through `apply_batch` (the wave-1 astra review, RW1-1 and
//! RW1-2; engine-spec EN-F13 "span-winner materialisation", EN-F12c, EN-F12a N5): a human's
//! `MENTIONS` row survives a non-winning group of another claim class on the same coordinates
//! byte-identical; a `type` span that loses its winner still drops the reconciler's row; a
//! changed winner replaces the reconciler's previous row instead of adding a second one, to a
//! new target or to a `proposed` record; an unchanged winner rewrites nothing. Every batch is a
//! separate `apply_batch` under `EdgeDiff::None`, so the structural diff (E2.5a) plays no part.
//! Invented rows beside the EN-F13 fixture (`common::orange`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use common::orange::{claim, fixture, store_with_infra, submit_with};
use kaban_core::{BatchAssertion, ClaimClass, EdgeDiff, NewNode, NodeId, Op};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// A read-only connection that reads body columns as kaban does (`kaban_body`).
fn raw(s: &Scratch) -> Connection {
    let conn = Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    conn
}

/// A node's text as the fixture spells it: the chunk is `c17`, every other node its text.
fn label(fx: &Value, text: &str) -> String {
    if text == fx["chunk"]["text"].as_str().unwrap() {
        fx["chunk"]["id"].as_str().unwrap().to_owned()
    } else {
        text.to_owned()
    }
}

/// Every `edge` row as the fixture's `edges` rows spell them: `[src, rel, dst, layer, method,
/// score in units of 1/10000, offset, len]`, ordered by `(rel, dst)`.
fn edges(fx: &Value, conn: &Connection) -> Value {
    let rows: Vec<Value> = conn
        .prepare(
            "SELECT kaban_body(s.text), e.rel, kaban_body(d.text), e.layer, e.method, CAST(round(e.score * 10000) AS INTEGER), e.offset, e.len
             FROM edge e JOIN node s ON s.rid = e.src JOIN node d ON d.rid = e.dst ORDER BY e.rel, d.text",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(json!([
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

/// Every `edge` row with every column as text, ordered by `(rel, dst, id)`: the comparison a
/// byte-identical claim needs (`id` and `created_at` included, so a delete and re-insert of
/// the same shape shows).
fn full_rows(conn: &Connection) -> Vec<Vec<Option<String>>> {
    conn.prepare(
        "SELECT CAST(id AS TEXT), CAST(src AS TEXT), CAST(dst AS TEXT), rel, CAST(confidence AS TEXT),
                created_at, props, workspace_id, layer, method, CAST(score AS TEXT), source_id,
                CAST(offset AS TEXT), CAST(len AS TEXT)
         FROM edge ORDER BY rel, dst, id",
    )
    .unwrap()
    .query_map([], |r| {
        (0..14)
            .map(|i| r.get::<_, Option<String>>(i))
            .collect::<rusqlite::Result<Vec<_>>>()
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every record the reconciler proposed as `[kind, text, governance]`, ordered by `(kind, text)`.
fn records(conn: &Connection) -> Value {
    let rows: Vec<Value> = conn
        .prepare(
            "SELECT kind, kaban_body(text), governance FROM node
             WHERE json_extract(props, '$.proposed_by') = 'kaban-reconcile' ORDER BY kind, text",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(json!([
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

/// `extractor_id -> disposition` of every claim (the row id travels in `extractor_id`).
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

/// An invented fixture row on span 27..32 of `c17` (the `human_first` shape with the named
/// fields replaced), for [`claim`]: `id` travels in `extractor_id`, the lineage is its own.
fn row(id: &str, value: &str, layer: &str, method: &str, raw_score: u32, rank: &str) -> Value {
    json!({
        "id": id, "offset": 27, "len": 5, "value": value, "layer": layer, "method": method,
        "raw_score": raw_score, "lineage_id": format!("lineage:{id}"), "rank": rank
    })
}

/// The fixture's `human_first` row: a human `link` c17 -> "Apple Inc." at 27..32.
fn human_first(fx: &Value, chunk: &NodeId) -> BatchAssertion {
    claim(fx, &fx["human_first"], chunk)
}

fn none(
    store: &SqliteStore,
    fx: &Value,
    chunk: &NodeId,
    ids: &[&str],
    ops: Vec<Op>,
    claims: Vec<BatchAssertion>,
) -> kaban_core::BatchReport {
    submit_with(store, fx, chunk, ids, ops, claims, EdgeDiff::None)
}

/// RW1-1 (EN-F12c, EN-F13; the kept red `e25d-red-cross-class.log`): a human `type` winner
/// materialises `MENTIONS` c17 -> "Apple Inc."; a zero-score `canonical_id` claim for the same
/// `(chunk, offset, len)` then arrives in its own batch and its own group is rejected
/// (`no_valid_support`), and an `identity` claim after it is a `merge` candidate: the human's
/// row survives both byte-identical. Span cleanup belongs to the class that owns the
/// materialisation (`type`) and never to a human's row.
#[test]
fn a_non_winning_group_of_another_class_on_the_same_span_leaves_the_human_mentions_row_byte_identical()
 {
    // Arrange: the human's row
    let fx = fixture();
    let s = Scratch::new("e25d-span", "cross-class");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    none(
        &store,
        &fx,
        &chunk,
        &[],
        vec![],
        vec![human_first(&fx, &chunk)],
    );
    let conn = raw(&s);
    assert_eq!(
        edges(&fx, &conn),
        fx["human_first_edges"],
        "the human's edge"
    );
    let before = full_rows(&conn);

    // Act: a rejected group of another class on the same coordinates
    let mut rejected = claim(
        &fx,
        &row("x1", "cid:unknown", "gazetteer", "alias", 0, "normal"),
        &chunk,
    );
    rejected.claim_class = ClaimClass::CanonicalId;
    none(&store, &fx, &chunk, &[], vec![], vec![rejected]);

    // Assert
    assert_eq!(
        statuses(&conn)["x1"],
        "rejected",
        "the other class's own verdict"
    );
    assert_eq!(
        full_rows(&conn),
        before,
        "the human's MENTIONS row survives a rejected group of another class on the same span byte-identical"
    );

    // Act: a candidate group of another class on the same coordinates
    let mut candidate = claim(
        &fx,
        &row("x2", "same-as", "gliner", "gliner", 5000, "normal"),
        &chunk,
    );
    candidate.claim_class = ClaimClass::Identity;
    none(&store, &fx, &chunk, &[], vec![], vec![candidate]);

    // Assert
    assert_eq!(
        statuses(&conn)["x2"],
        "candidate",
        "the other class's own verdict"
    );
    assert_eq!(
        full_rows(&conn),
        before,
        "the human's MENTIONS row survives a candidate group of another class on the same span byte-identical"
    );
}

/// RW1-1 postcheck, the positive side (EN-F13, E2.5's rule): the machine `type` winner of span
/// 27..32 (a1, a3 over a2) materialises the reconciler's `MENTIONS` row; an `llm` `x:Fruit`
/// claim from a new lineage pushes the span below the margin, and the `Candidate` decision
/// drops the row the reconciler wrote (no record: the winner had a target).
#[test]
fn a_type_span_that_loses_its_winner_drops_the_reconcilers_mentions_row() {
    // Arrange
    let fx = fixture();
    let s = Scratch::new("e25d-span", "lost-winner");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    none(&store, &fx, &chunk, &["a1", "a2", "a3"], vec![], vec![]);
    let conn = raw(&s);
    assert_eq!(
        edges(&fx, &conn),
        fx["variants"]["without_llm"]["edges"],
        "the machine winner's row"
    );

    // Act
    let fruit = claim(
        &fx,
        &row("f1", "x:Fruit", "llm", "llm", 9000, "normal"),
        &chunk,
    );
    let report = none(&store, &fx, &chunk, &[], vec![], vec![fruit]);

    // Assert
    assert_eq!(statuses(&conn)["a1"], "candidate", "{report:?}");
    assert_eq!(
        edges(&fx, &conn),
        json!([]),
        "a type span decided without a winner loses the reconciler's MENTIONS row"
    );
    assert_eq!(records(&conn), json!([]));
}

/// A second entity the fixture does not know: `infra:Team` "Orange team", the target B.
fn team_b() -> (NodeId, Op) {
    let id = NodeId::new();
    let op = Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "infra:Team".into(),
            text: "Orange team".into(),
            ..Default::default()
        },
    };
    (id, op)
}

/// RW1-2 (EN-F13, EN-F12a N5; the kept red `e25d-red-winner-change.log`): the machine `Vendor`
/// winner of span 27..32 targets A ("Apple Inc."); a `preferred` human `Team` claim for the
/// same span targets B: after that decision exactly one `MENTIONS` row for the span exists,
/// c17 -> B with `layer: human`, and A's row is gone. The same batch resubmitted changes no
/// row (`id` and `created_at` included).
#[test]
fn a_changed_span_winner_replaces_the_reconcilers_mentions_row_and_an_unchanged_one_rewrites_nothing()
 {
    // Arrange: the machine winner's row to A
    let fx = fixture();
    let s = Scratch::new("e25d-span", "winner-a-to-b");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    none(&store, &fx, &chunk, &["a1", "a2", "a3"], vec![], vec![]);
    let conn = raw(&s);
    assert_eq!(
        edges(&fx, &conn),
        fx["variants"]["without_llm"]["edges"],
        "the machine winner's row to A"
    );
    let (b, b_op) = team_b();
    let mut human = claim(
        &fx,
        &row("h2", "infra:Team", "human", "link", 10_000, "preferred"),
        &chunk,
    );
    human.object = Some(b.clone());

    // Act: the preferred human winner to B
    none(
        &store,
        &fx,
        &chunk,
        &[],
        vec![b_op.clone()],
        vec![human.clone()],
    );

    // Assert
    assert_eq!(statuses(&conn)["h2"], "winner");
    assert_eq!(
        edges(&fx, &conn),
        json!([[
            "c17",
            "MENTIONS",
            "Orange team",
            "human",
            "reconciled",
            10000,
            27,
            5
        ]]),
        "exactly one MENTIONS row for the span, the new winner's; A's row is gone"
    );

    // Act: the unchanged winner (N5)
    let before = full_rows(&conn);
    none(
        &store,
        &fx,
        &chunk,
        &["a1", "a2", "a3"],
        vec![b_op],
        vec![human],
    );

    // Assert
    assert_eq!(
        full_rows(&conn),
        before,
        "an unchanged winner changes no edge row"
    );
}

/// RW1-2, target -> no target (EN-F13 default (b)): the machine `Vendor` winner of span 27..32
/// targets A; a `preferred` human `Hardware` claim without an `object` wins the span: the old
/// `MENTIONS` row is gone and the `proposed` record (kind `infra:Hardware`, the span bytes
/// `apple`) with its `EVIDENCE_FOR` edge is the span's one output.
#[test]
fn a_span_winner_that_moves_to_a_value_without_a_target_drops_the_old_mentions_row_and_proposes() {
    // Arrange
    let fx = fixture();
    let s = Scratch::new("e25d-span", "target-to-none");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    none(&store, &fx, &chunk, &["a1", "a2", "a3"], vec![], vec![]);
    let conn = raw(&s);
    assert_eq!(
        edges(&fx, &conn),
        fx["variants"]["without_llm"]["edges"],
        "the machine winner's row to A"
    );
    let human = claim(
        &fx,
        &row("h3", "infra:Hardware", "human", "link", 10_000, "preferred"),
        &chunk,
    );
    assert!(human.object.is_none());

    // Act
    none(&store, &fx, &chunk, &[], vec![], vec![human]);

    // Assert
    assert_eq!(statuses(&conn)["h3"], "winner");
    assert_eq!(
        edges(&fx, &conn),
        json!([[
            "c17",
            "EVIDENCE_FOR",
            "apple",
            "human",
            "reconciled",
            10000,
            27,
            5
        ]]),
        "the old MENTIONS row is gone; the proposed record's EVIDENCE_FOR is the span's output"
    );
    assert_eq!(
        records(&conn),
        json!([["infra:Hardware", "apple", "proposed"]])
    );
}
