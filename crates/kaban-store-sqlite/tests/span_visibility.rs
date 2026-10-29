//! Span-claim candidates and source-chunk visibility (OWN116; engine-spec EN-F49, EN-F5a,
//! EN-F13): an uncertain span claim makes the entity it claims a candidate, never the source
//! Chunk it was read from. The chunk holds no `node_candidate` row and stays in the default
//! read paths; the claimed entity is the review row (hidden by default, present with
//! `include_candidates`); a span group without an object holds no row and is listed for
//! review under its chunk from the claims' `candidate` statuses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use common::orange::{entity_id, fixture, store_with_infra, submit, submit_with};
use kaban_core::{
    BatchAssertion, BatchSubject, ClaimClass, EdgeDiff, Fixed, Layer, MemoryStore, Method, NewNode,
    NodeId, Op, Owner, Rank, ReadFilters, RecallQuery, WorkspaceId,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::Connection;
use serde_json::json;

fn raw(s: &Scratch) -> Connection {
    Connection::open(s.db()).unwrap()
}

/// A `type` claim on span 56..62 of `chunk` (the fixture's "orange team" span) naming `value`
/// and the resolved `object`.
fn span_claim(
    chunk: &NodeId,
    value: &str,
    object: Option<&NodeId>,
    (layer, method): (Layer, Method),
    score: u32,
    lineage: &str,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: 56,
            len: 6,
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: object.cloned(),
        value: json!(value),
        layer,
        method,
        extractor_id: Some(lineage.to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("c17".into()),
        lineage_id: lineage.to_owned(),
        raw_score: Fixed::new(score).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// The `node_candidate` rows as `(node id, reason, group_key)`, in rid order.
fn candidate_rows(conn: &Connection) -> Vec<(String, String, String)> {
    conn.prepare(
        "SELECT n.id, c.reason, c.group_key FROM node_candidate c JOIN node n ON n.rid = c.rid ORDER BY c.rid",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every claim's `(extractor_id, disposition, group_key)`.
fn statuses(conn: &Connection) -> Vec<(String, String, String)> {
    conn.prepare(
        "SELECT a.extractor_id, s.disposition, s.group_key FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id ORDER BY a.extractor_id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The ids `recall` returns for `query` (no hops).
fn recalled(store: &SqliteStore, query: &str, include_candidates: bool) -> Vec<String> {
    store
        .recall(&RecallQuery {
            query: query.into(),
            limit: 10,
            filters: ReadFilters { include_candidates },
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|h| h.node.id)
        .collect()
}

/// The `review_rows` of the default workspace as `(node id, reason, disposition, group_key)`.
fn review(store: &SqliteStore) -> Vec<(String, String, String, String)> {
    store
        .review_rows(WorkspaceId::default().as_str(), None)
        .unwrap()
        .into_iter()
        .map(|r| (r.node.to_string(), r.reason, r.disposition, r.group_key))
        .collect()
}

/// The walkthrough shape (E3.11 step 4): a gliner `x:Org "Cart"` claim at 0.61 over a span of
/// a Chunk, `object` the Org node. The claim is `candidate`; the chunk holds no review row and
/// is recalled by default; the Org is the candidate (hidden by default, recalled with
/// `include_candidates`) and `review` lists the span group under the Org, not the chunk.
#[test]
fn an_uncertain_span_claim_makes_its_entity_the_candidate_and_leaves_the_chunk_visible() {
    let fx = fixture();
    let s = Scratch::new("s29-span-vis", "org");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    let org = NodeId::new();
    let org_op = Op::UpsertNode {
        id: org.clone(),
        node: NewNode {
            kind: "x:Org".into(),
            text: "Cart".into(),
            ..Default::default()
        },
    };
    submit_with(
        &store,
        &fx,
        &chunk,
        &[],
        vec![org_op],
        vec![span_claim(
            &chunk,
            "x:Org",
            Some(&org),
            (Layer::Gliner, Method::Gliner),
            6100,
            "g1",
        )],
        EdgeDiff::Owned {
            owner: Owner::KabanIngest,
        },
    );
    let conn = raw(&s);
    let st = statuses(&conn);
    assert_eq!(st.len(), 1, "{st:?}");
    assert_eq!(st[0].1, "candidate", "{st:?}");
    let group = st[0].2.clone();

    let rows = candidate_rows(&conn);
    assert!(
        rows.iter().all(|(id, ..)| *id != chunk.to_string()),
        "the source chunk is never a candidate: {rows:?}"
    );
    assert!(
        rows.iter().any(|(id, ..)| *id == org.to_string()),
        "the claimed entity is the candidate: {rows:?}"
    );

    // the chunk stays in the default read path; the Org is hidden unless asked for
    assert!(
        recalled(&store, "silicon runners", false).contains(&chunk.to_string()),
        "the chunk is recalled by default"
    );
    assert!(!recalled(&store, "Cart", false).contains(&org.to_string()));
    assert!(recalled(&store, "Cart", true).contains(&org.to_string()));

    // review lists the span group under the claimed entity, nothing under the chunk
    let items = review(&store);
    assert!(
        items
            .iter()
            .any(|(id, _, d, g)| *id == org.to_string() && d == "candidate" && *g == group),
        "the span group is reviewed under the Org: {items:?}"
    );
    assert!(
        items.iter().all(|(id, ..)| *id != chunk.to_string()),
        "{items:?}"
    );
}

/// OWN-118: a weak span mention of a declared, already admitted entity (no review row of its
/// own) never hides it: no row is written, the entity and the chunk stay visible, and review
/// lists the group under the entity from the claims' statuses; a later winner settles it.
#[test]
fn a_weak_span_never_hides_an_admitted_entity_and_is_reviewed_under_it() {
    let fx = fixture();
    let s = Scratch::new("s29-span-vis", "release");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    let apple = entity_id(&fx, "apple");
    let weak = span_claim(
        &chunk,
        "infra:Vendor",
        Some(&apple),
        (Layer::Gazetteer, Method::Alias),
        4000,
        "z1",
    );
    submit(&store, &fx, &chunk, &[], vec![weak.clone()]);
    let conn = raw(&s);
    let st = statuses(&conn);
    assert_eq!(st[0].1, "candidate", "{st:?}");
    assert_eq!(
        candidate_rows(&conn),
        vec![],
        "an admitted entity gets no review row"
    );
    assert!(recalled(&store, "silicon runners", false).contains(&chunk.to_string()));
    assert!(recalled(&store, "Apple", false).contains(&apple.to_string()));
    assert_eq!(
        review(&store),
        vec![(
            apple.to_string(),
            "below_margin".to_owned(),
            "candidate".to_owned(),
            st[0].2.clone()
        )],
        "the group is reviewed under the entity"
    );

    let strong = span_claim(
        &chunk,
        "infra:Vendor",
        Some(&apple),
        (Layer::Llm, Method::Llm),
        9000,
        "z2",
    );
    submit(&store, &fx, &chunk, &[], vec![weak, strong]);
    let st = statuses(&conn);
    assert!(st.iter().any(|(_, d, _)| d == "winner"), "{st:?}");
    assert_eq!(candidate_rows(&conn), vec![]);
    assert_eq!(review(&store), vec![], "the winner settled the group");
    assert!(recalled(&store, "Apple", false).contains(&apple.to_string()));
    assert!(recalled(&store, "silicon runners", false).contains(&chunk.to_string()));
}

/// A span group whose claims name no object (the fixture's 56..62 vote, `a5 a6 a7`) holds no
/// `node_candidate` row: the chunk stays visible, and `review` still lists the group under the
/// chunk (`candidate`, `below_margin`) from the claims' statuses, so it can be resolved there.
#[test]
fn an_objectless_span_candidate_leaves_the_chunk_visible_and_is_reviewed_under_it() {
    let fx = fixture();
    let s = Scratch::new("s29-span-vis", "objectless");
    let store = store_with_infra(&s, &fx);
    let chunk = NodeId::new();
    submit(&store, &fx, &chunk, &["a5", "a6", "a7"], vec![]);
    let conn = raw(&s);
    let st = statuses(&conn);
    assert!(st.iter().all(|(_, d, _)| d == "candidate"), "{st:?}");
    assert_eq!(candidate_rows(&conn), vec![], "no node holds a review row");
    assert!(recalled(&store, "silicon runners", false).contains(&chunk.to_string()));
    assert_eq!(
        review(&store),
        vec![(
            chunk.to_string(),
            "below_margin".to_owned(),
            "candidate".to_owned(),
            st[0].2.clone()
        )],
        "the group is reviewed under its chunk"
    );
}
