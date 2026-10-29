//! Persisted rows under permutation and replay (engine-spec EN-F12, EN-F8a; engine-plan E2.2
//! item 7; RP-10): the same assertion set submitted in any order, split across any batch
//! boundaries, to fresh stores yields byte-identical `assertion_status` and `edge` rows; a
//! `ChangeLog` replay from zero through `ReplaySink` reconstructs the `assertion_status` table
//! of a store whose statuses were wiped, and a second replay changes nothing. Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use common::replay::{REPLAY_SINK, ReplaySink};
use kaban_core::ActorId;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ChangeLogStore, ClaimClass, Fixed, IngestStore, Layer,
    LeaseStore, MemoryStore, Method, NewNode, NodeId, Op, Rank, WorkspaceId, WriteBatch, WriteCtx,
    run_sink,
};
use kaban_store_sqlite::SqliteStore;
use proptest::prelude::*;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

/// Three nodes under fixed ids, inserted in a fixed order so the row ids agree across stores.
fn nodes() -> Vec<NodeId> {
    [
        "01a17a03-5995-7000-8000-0000000000a1",
        "01a17a03-5995-7000-8000-0000000000b2",
        "01a17a03-5995-7000-8000-0000000000c3",
    ]
    .iter()
    .map(|s| NodeId::parse(s).unwrap())
    .collect()
}

fn seed_nodes(store: &SqliteStore, ids: &[NodeId]) {
    let ops = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: format!("node {i}"),
                ..Default::default()
            },
        })
        .collect();
    store
        .apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
        .unwrap();
}

fn edge_claim(
    src: &NodeId,
    dst: &NodeId,
    layer: Layer,
    method: Method,
    raw: u32,
    lineage: &str,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: "RELATES_TO".into(),
            dst: dst.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer,
        method,
        extractor_id: None,
        extractor_version: None,
        source_id: Some("doc".into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// The claim set: two triples with competing layers and one single-supporter triple.
fn claims(ids: &[NodeId]) -> Vec<BatchAssertion> {
    let (a, b, c) = (&ids[0], &ids[1], &ids[2]);
    vec![
        edge_claim(a, b, Layer::Gliner, Method::Gliner, 9000, "gl"),
        edge_claim(a, b, Layer::Llm, Method::Llm, 8000, "llm"),
        edge_claim(a, b, Layer::Embed, Method::Embed, 7000, "em"),
        edge_claim(b, c, Layer::Llm, Method::Llm, 5000, "llm"),
        edge_claim(b, c, Layer::Gazetteer, Method::Alias, 6000, "gaz"),
        edge_claim(c, a, Layer::Gliner, Method::Gliner, 4000, "gl"),
    ]
}

type StatusRow = (String, String, String, String);
type EdgeRow = (
    i64,
    i64,
    String,
    Option<f64>,
    String,
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// `assertion_status` joined to the stable claim key: every column except `assertion_id`
/// and `updated_at`, sorted.
fn status_rows(s: &Scratch) -> Vec<StatusRow> {
    let conn = raw(s);
    let mut rows: Vec<StatusRow> = conn
        .prepare(
            "SELECT a.key, s.disposition, s.group_key, s.verdict_rule FROM assertion_status s
             JOIN assertion a ON a.id = s.assertion_id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.sort();
    rows
}

/// `edge`: every column except `id` and `created_at`, sorted.
fn edge_rows(s: &Scratch) -> Vec<EdgeRow> {
    let conn = raw(s);
    let mut rows: Vec<EdgeRow> = conn
        .prepare("SELECT src, dst, rel, confidence, props, layer, method, score, source_id, offset, len FROM edge")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.sort_by(|x, y| x.partial_cmp(y).unwrap());
    rows
}

/// Submit `order` (indexes into the claim set) split at `cuts` into successive batches.
fn submit(store: &SqliteStore, ids: &[NodeId], order: &[usize], cuts: &[bool]) {
    let all = claims(ids);
    let mut batch: Vec<BatchAssertion> = Vec::new();
    for (i, idx) in order.iter().enumerate() {
        batch.push(all[*idx].clone());
        if cuts.get(i).copied().unwrap_or(false) || i + 1 == order.len() {
            let mut b = WriteBatch::new(WorkspaceId::default(), vec![]);
            b.assertions = std::mem::take(&mut batch);
            store.apply_batch(b, &ctx()).unwrap();
        }
    }
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]
    /// Slice (EN-F12, RP-10): the same claims in a random order, split across random batch
    /// boundaries, give byte-identical status and edge rows against the in-order, one-batch
    /// submission.
    #[test]
    fn the_same_claims_in_any_order_and_batching_yield_identical_persisted_rows(
        order in Just((0..6usize).collect::<Vec<_>>()).prop_shuffle(),
        cuts in prop::collection::vec(any::<bool>(), 6),
    ) {
        let ids = nodes();
        let reference = Scratch::new("e22-rows", "reference");
        let permuted = Scratch::new("e22-rows", "permuted");
        let r = SqliteStore::open(&reference.db(), &reference.keys()).unwrap();
        let p = SqliteStore::open(&permuted.db(), &permuted.keys()).unwrap();
        seed_nodes(&r, &ids);
        seed_nodes(&p, &ids);
        submit(&r, &ids, &[0, 1, 2, 3, 4, 5], &[false; 6]);
        submit(&p, &ids, &order, &cuts);
        let statuses = status_rows(&reference);
        prop_assert_eq!(statuses.len(), 6);
        prop_assert_eq!(status_rows(&permuted), statuses);
        let edges = edge_rows(&reference);
        prop_assert_eq!(edges.len(), 3);
        prop_assert_eq!(edge_rows(&permuted), edges);
    }
}

/// Slice (E2 interim review RE4-1; EN-F8a): a replay from the retained rows keeps a masked
/// claim and a forgotten node's claims `rejected`: the retirement lives on the claim row
/// (`rank`, `rank_reason`), so the status table and the edges a replay rebuilds equal the
/// live ones.
#[test]
fn a_replay_from_retained_rows_keeps_masked_and_forgotten_claims_rejected() {
    let s = Scratch::new("e22-rows", "replay-retired");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ids = nodes();
    seed_nodes(&store, &ids);
    submit(&store, &ids, &[0, 1, 2, 3, 4, 5], &[false; 6]);
    // a stronger llm supporter of a->b whose value the new policy denies
    let mut denied = edge_claim(&ids[0], &ids[1], Layer::Llm, Method::Llm, 9500, "llm-x");
    denied.value = json!({"note": "token hunter2"});
    let mut b = WriteBatch::new(WorkspaceId::default(), vec![]);
    b.assertions = vec![denied];
    store.apply_batch(b, &ctx()).unwrap();
    let report = store
        .rescan_assertions("v2", &mut |claim, _audit| {
            claim
                .value
                .as_str()
                .contains("hunter2")
                .then(|| json!({"note": "[REDACTED]"}))
        })
        .unwrap();
    assert_eq!(report.rejected, 1);
    // the third node is forgotten: its two groups lose every supporter
    store
        .forget(&ids[2], kaban_core::ForgetMode::Tombstone, &ctx())
        .unwrap();
    let statuses = status_rows(&s);
    let edges = edge_rows(&s);
    assert_eq!(statuses.len(), 7);
    assert_eq!(
        statuses.iter().filter(|(_, d, ..)| d == "rejected").count(),
        4,
        "the masked claim and the forgotten node's three claims: {statuses:?}"
    );
    assert_eq!(edges.len(), 1, "{edges:?}");
    {
        let conn = Connection::open(s.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        conn.execute("DELETE FROM assertion_status", []).unwrap();
        conn.execute("DELETE FROM edge", []).unwrap();
    }
    let lease = store
        .lease_acquire(REPLAY_SINK.0, "replayer", 60)
        .unwrap()
        .expect("lease free");
    let mut sink = ReplaySink::new(&store);
    let mut cursor = 0;
    loop {
        let report = run_sink(&store, &mut sink, &lease, cursor, 4).unwrap();
        match report.acked {
            Some(upto) if report.consumed > 0 => cursor = upto,
            _ => break,
        }
    }
    store.lease_release(&lease).unwrap();
    assert_eq!(
        status_rows(&s),
        statuses,
        "retired claims stay rejected through the replay"
    );
    assert_eq!(edge_rows(&s), edges);
}

/// Slice (EN-F8a): a `ChangeLog` replay from zero through `ReplaySink` reconstructs the
/// `assertion_status` table (all columns except `updated_at`) and the projected edges of a
/// store whose statuses were wiped; a second replay changes nothing.
#[test]
fn a_changelog_replay_from_zero_reconstructs_the_status_table_and_is_idempotent() {
    let s = Scratch::new("e22-rows", "replay");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ids = nodes();
    seed_nodes(&store, &ids);
    submit(
        &store,
        &ids,
        &[0, 1, 2, 3, 4, 5],
        &[false, false, true, false, false, false],
    );
    // a human link joins one of the groups
    store
        .link(&ids[1], &ids[2], "RELATES_TO", Some(0.7), None, &ctx())
        .unwrap();
    let statuses = status_rows(&s);
    let edges = edge_rows(&s);
    assert_eq!(statuses.len(), 7);
    assert!(statuses.iter().all(|(_, d, ..)| d != "pending"));
    // wipe the projection: statuses and the materialised edges
    {
        let conn = Connection::open(s.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        conn.execute("DELETE FROM assertion_status", []).unwrap();
        conn.execute("DELETE FROM edge", []).unwrap();
    }
    assert_eq!(status_rows(&s).len(), 0, "the statuses are wiped");
    let replay = |store: &SqliteStore| -> usize {
        let lease = store
            .lease_acquire(REPLAY_SINK.0, "replayer", 60)
            .unwrap()
            .expect("lease free");
        let mut sink = ReplaySink::new(store);
        let mut cursor = 0;
        loop {
            let report = run_sink(store, &mut sink, &lease, cursor, 4).unwrap();
            assert!(!report.lease_lost);
            match report.acked {
                Some(upto) if report.consumed > 0 => cursor = upto,
                _ => break,
            }
        }
        store.lease_release(&lease).unwrap();
        sink.claims_seen
    };
    let seen = replay(&store);
    assert!(
        seen >= 7,
        "every claim row of the log was handed over: {seen}"
    );
    assert_eq!(
        status_rows(&s),
        statuses,
        "the status table is reconstructed"
    );
    assert_eq!(edge_rows(&s), edges, "the projected edges are back");
    let changes = store.changes_read(0, 100_000).unwrap().len();
    replay(&store);
    assert_eq!(status_rows(&s), statuses, "a second replay changes nothing");
    assert_eq!(edge_rows(&s), edges);
    assert_eq!(
        store.changes_read(0, 100_000).unwrap().len(),
        changes,
        "and writes no ChangeLog row"
    );
}
