//! Assertions in `apply_batch` (engine-spec EN-F8, EN-F9, EN-E9, EN-F12, EN-S3, EN-S9;
//! engine-plan E2.2): claim rows are immutable and workspace-bound, the end-of-batch pass
//! decides every touched group through the injected `Reconciler`, and the projection owns the
//! one edge per triple. Invented data throughout.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, Assertion, AssertionStatus, BatchAssertion, BatchSubject, CandidateReason, ChangeEntity,
    ChangeLogStore, ChangeOp, ClaimClass, Fixed, Governance, GovernanceStore, IngestStore, Layer,
    MemoryStore, Method, NewEvent, NewNode, NodeId, Op, Rank, ReconcileCtx, Reconciler, SignerKind,
    StoreError, TraverseQuery, Verdict, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn upsert(id: &NodeId, kind: &str, text: &str) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: kind.into(),
            text: text.into(),
            ..Default::default()
        },
    }
}

/// An `edge` claim about `src -rel-> dst` from `layer`/`method` with raw score `raw`
/// (1/10000) under lineage `lineage`.
fn edge_claim(
    src: &NodeId,
    rel: &str,
    dst: &NodeId,
    layer: Layer,
    method: Method,
    raw: u32,
    lineage: &str,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: rel.into(),
            dst: dst.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer,
        method,
        extractor_id: Some("fixture".into()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

fn batch(ops: Vec<Op>, assertions: Vec<BatchAssertion>) -> WriteBatch {
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    b.assertions = assertions;
    b
}

/// A read-only second connection on the store's file (WAL: readers never block the store).
fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

/// Every column of every `assertion` row as text, ordered by id (byte-identity checks).
fn claim_rows(conn: &Connection) -> Vec<Vec<Option<String>>> {
    let mut st = conn.prepare("SELECT * FROM assertion ORDER BY id").unwrap();
    let n = st.column_count();
    st.query_map([], |r| {
        (0..n)
            .map(|i| {
                r.get_ref(i).map(|v| match v {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Integer(i) => Some(i.to_string()),
                    rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                    rusqlite::types::ValueRef::Text(t) => {
                        Some(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => Some(format!("{b:?}")),
                })
            })
            .collect()
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn statuses(conn: &Connection) -> Vec<(i64, String)> {
    conn.prepare("SELECT assertion_id, disposition FROM assertion_status ORDER BY assertion_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A `REAL` column in units of 1/10000, as the fixed-point decision rendered it.
#[allow(clippy::cast_possible_truncation)] // 0..=10000 after rounding
fn fixed(real: f64) -> i64 {
    (real * 10_000.0).round() as i64
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Slice: a claim row is byte-identical before and after reconciliation and has exactly one
/// `assertion_status` row (EN-F8).
#[test]
fn a_claim_row_is_byte_identical_before_and_after_reconciliation_and_has_one_status() {
    let s = Scratch::new("e22-assert", "immutable");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let first = store
        .apply_batch(
            batch(
                vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
                vec![edge_claim(
                    &a,
                    "RELATES_TO",
                    &b,
                    Layer::Gliner,
                    Method::Gliner,
                    9000,
                    "l1",
                )],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!((first.assertions_inserted, first.assertions_noop), (1, 0));
    let conn = raw(&s);
    let before = claim_rows(&conn);
    assert_eq!(before.len(), 1);
    // a second layer joins the group: the pass re-decides it over both supporters
    store
        .apply_batch(
            batch(
                vec![],
                vec![edge_claim(
                    &a,
                    "RELATES_TO",
                    &b,
                    Layer::Llm,
                    Method::Llm,
                    8000,
                    "l2",
                )],
            ),
            &ctx(),
        )
        .unwrap();
    let after = claim_rows(&conn);
    assert_eq!(after.len(), 2);
    assert_eq!(after[0], before[0], "the first claim row never changes");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM assertion_status"),
        2,
        "one status row per claim"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM assertion a WHERE NOT EXISTS (SELECT 1 FROM assertion_status s WHERE s.assertion_id = a.id)"
        ),
        0
    );
}

/// Slice: an identical claim re-submitted writes zero rows and zero `ChangeLog` rows (EN-F12a N5).
#[test]
fn an_identical_claim_resubmitted_writes_no_row_and_no_changelog_row() {
    let s = Scratch::new("e22-assert", "noop");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let b1 = batch(
        vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
        vec![edge_claim(
            &a,
            "RELATES_TO",
            &b,
            Layer::Gliner,
            Method::Gliner,
            9000,
            "l1",
        )],
    );
    store.apply_batch(b1.clone(), &ctx()).unwrap();
    let changes = store.changes_read(0, 10_000).unwrap().len();
    let conn = raw(&s);
    let rows = claim_rows(&conn);
    let again = store.apply_batch(b1, &ctx()).unwrap();
    assert_eq!((again.assertions_inserted, again.assertions_noop), (0, 1));
    assert_eq!(claim_rows(&conn), rows);
    let extra: Vec<(ChangeEntity, ChangeOp, String)> = store
        .changes_read(changes as u64, 10_000)
        .unwrap()
        .into_iter()
        .map(|c| (c.entity, c.op, c.id))
        .collect();
    assert!(
        extra.is_empty(),
        "no changes row: no claim, no status change, no edge change; got {extra:?}"
    );
    assert!(again.outbox_first_seq.is_none());
}

/// Slice: two layers asserting one triple yield one edge with `confidence = max`, two statuses
/// (`winner`, `dissent`), and `traverse` returns the edge in today's shape (EN-F9).
#[test]
fn two_layers_asserting_one_triple_yield_one_edge_with_confidence_max_and_two_statuses() {
    let s = Scratch::new("e22-assert", "two-layers");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let report = store
        .apply_batch(
            batch(
                vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
                vec![
                    // gliner prior 0.6 x 0.9 = 0.54; llm prior 0.75 x 0.8 = 0.60: llm wins
                    edge_claim(
                        &a,
                        "RELATES_TO",
                        &b,
                        Layer::Gliner,
                        Method::Gliner,
                        9000,
                        "l1",
                    ),
                    edge_claim(&a, "RELATES_TO", &b, Layer::Llm, Method::Llm, 8000, "l2"),
                ],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.assertions_inserted, 2);
    assert_eq!(report.verdicts.get(&AssertionStatus::Winner), Some(&1));
    let conn = raw(&s);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM edge"),
        1,
        "one edge per triple"
    );
    let (confidence, layer, method, raw, props): (f64, String, String, f64, String) = conn
        .query_row(
            "SELECT confidence, layer, method, score, props FROM edge",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(fixed(confidence), 6000, "max over the layers");
    assert_eq!((layer.as_str(), method.as_str()), ("llm", "llm"));
    assert_eq!(fixed(raw), 8000, "the winner's raw score");
    let props: Value = serde_json::from_str(&props).unwrap();
    assert_eq!(
        props["owner"],
        json!("kaban-reconcile"),
        "a layer claim's edge is the reconciler's own write"
    );
    assert_eq!(props["provenance"].as_array().unwrap().len(), 2);
    let mut st: Vec<String> = statuses(&conn).into_iter().map(|(_, d)| d).collect();
    st.sort();
    assert_eq!(st, vec!["dissent".to_owned(), "winner".to_owned()]);
    let t = store
        .traverse(&TraverseQuery {
            start_ids: vec![a.clone()],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Out,
            max_depth: 1,
            node_kinds: vec![],
            governance: vec![],
            limit: 10,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(t.edges.len(), 1);
    assert_eq!(
        (t.edges[0].src.as_str(), t.edges[0].dst.as_str()),
        (a.as_str(), b.as_str())
    );
    assert_eq!(t.edges[0].rel, "RELATES_TO");
}

/// Slice: a direct `link` produces one human assertion and one edge (EN-F9).
#[test]
fn a_direct_link_produces_one_human_assertion_and_one_edge() {
    let s = Scratch::new("e22-assert", "human-link");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = common::created(&store, "Fact", "a human note");
    let b = common::created(&store, "Fact", "another human note");
    let edge = store
        .link(
            &NodeId::parse(&a.id).unwrap(),
            &NodeId::parse(&b.id).unwrap(),
            "RELATES_TO",
            Some(0.9),
            Some(json!({"note": "by hand"})),
            &ctx(),
        )
        .unwrap();
    assert_eq!(fixed(edge.confidence.unwrap()), 9000);
    assert_eq!(
        edge.props["note"],
        json!("by hand"),
        "the caller's props survive"
    );
    assert_eq!(edge.props["owner"], json!("human"));
    let conn = raw(&s);
    assert_eq!(count(&conn, "SELECT count(*) FROM assertion"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 1);
    let (layer, method, raw_score, class): (String, String, i64, String) = conn
        .query_row(
            "SELECT layer, method, raw_score, claim_class FROM assertion",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (layer.as_str(), method.as_str(), raw_score, class.as_str()),
        ("human", "link", 9000, "edge")
    );
    assert_eq!(statuses(&conn)[0].1, "winner");
    // the same link again: the same key, no second claim
    store
        .link(
            &NodeId::parse(&a.id).unwrap(),
            &NodeId::parse(&b.id).unwrap(),
            "RELATES_TO",
            Some(0.9),
            Some(json!({"note": "by hand"})),
            &ctx(),
        )
        .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM assertion"), 1);
}

/// Negative (EN-F9): a producer edge op naming a layer but no matching `edge` assertion is
/// refused before any write; the upsert is never reached from a bare op.
#[test]
fn a_layer_edge_op_without_its_assertion_is_refused_before_any_write() {
    let store = SqliteStore::open_memory().unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let changes = store.changes_read(0, 10_000).unwrap().len();
    let err = store
        .apply_batch(
            batch(
                vec![
                    upsert(&a, "Fact", "node a"),
                    upsert(&b, "Fact", "node b"),
                    Op::UpsertEdge {
                        src: a.clone(),
                        dst: b.clone(),
                        rel: "RELATES_TO".into(),
                        props: Some(json!({"layer": "gliner"})),
                    },
                ],
                vec![],
            ),
            &ctx(),
        )
        .expect_err("a layer edge needs its assertion");
    assert!(matches!(err, StoreError::EdgeRequiresAssertion), "{err:?}");
    assert_eq!(err.to_string(), "edge_requires_assertion");
    assert!(store.get(&a, true).unwrap().is_none(), "nothing written");
    assert_eq!(store.changes_read(0, 10_000).unwrap().len(), changes);
    // with the assertion the same batch lands
    let report = store
        .apply_batch(
            batch(
                vec![
                    upsert(&a, "Fact", "node a"),
                    upsert(&b, "Fact", "node b"),
                    Op::UpsertEdge {
                        src: a.clone(),
                        dst: b.clone(),
                        rel: "RELATES_TO".into(),
                        props: Some(json!({"layer": "gliner"})),
                    },
                ],
                vec![edge_claim(
                    &a,
                    "RELATES_TO",
                    &b,
                    Layer::Gliner,
                    Method::Gliner,
                    9000,
                    "l1",
                )],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.assertions_inserted, 1);
}

/// Slice (E2 interim review RE4-5; EN-F9): a layer edge op and its `edge` claim in one batch
/// write the triple through the projection alone: one edge row, the winner's, and one `edge`
/// `ChangeLog` row; no unlayered structural row is written first and rewritten.
#[test]
fn a_matching_op_and_claim_yield_one_projected_row_and_no_unlayered_row() {
    let s = Scratch::new("e22-assert", "op-and-claim");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let conn = raw(&s);
    let edge_changes = || count(&conn, "SELECT count(*) FROM changes WHERE entity = 'edge'");
    let before = edge_changes();
    store
        .apply_batch(
            batch(
                vec![
                    upsert(&a, "Fact", "node a"),
                    upsert(&b, "Fact", "node b"),
                    Op::UpsertEdge {
                        src: a.clone(),
                        dst: b.clone(),
                        rel: "RELATES_TO".into(),
                        props: Some(json!({"layer": "gliner"})),
                    },
                ],
                vec![edge_claim(
                    &a,
                    "RELATES_TO",
                    &b,
                    Layer::Gliner,
                    Method::Gliner,
                    9000,
                    "g1",
                )],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 1);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM edge WHERE layer IS NULL"),
        0,
        "no structural row for a claimed triple"
    );
    let (layer, owner): (String, String) = conn
        .query_row(
            "SELECT layer, json_extract(props, '$.owner') FROM edge",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (layer.as_str(), owner.as_str()),
        ("gliner", "kaban-reconcile")
    );
    assert_eq!(
        edge_changes() - before,
        1,
        "one projected write, not a structural insert then a rewrite"
    );
}

/// Slice (E2 interim review RE4-5; EN-F9, EN-S10): a machine `SUPERSEDES` op with its claim
/// is a `Candidate{governed_semantics}`: no edge row, and the lifecycle effect of the rel
/// (`archive_target`) never runs before a winner is materialised, so the accepted target
/// keeps its governance.
#[test]
fn a_machine_supersedes_candidate_leaves_no_active_edge_and_no_lifecycle_effect() {
    let s = Scratch::new("e22-assert", "supersedes-candidate");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let decision = |text: &str| -> NodeId {
        match store
            .remember(
                NewNode {
                    kind: "Decision".into(),
                    text: text.into(),
                    props: Some(json!({"rationale": "fixture"})),
                    ..Default::default()
                },
                &[],
                &ctx(),
            )
            .unwrap()
        {
            kaban_core::RememberOutcome::Created(n) => NodeId::parse(&n.id).unwrap(),
            other @ kaban_core::RememberOutcome::Existing { .. } => panic!("{other:?}"),
        }
    };
    let (newer, older) = (decision("ship weekly"), decision("ship monthly"));
    store
        .transition(
            &older,
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli),
        )
        .unwrap();
    let report = store
        .apply_batch(
            batch(
                vec![Op::UpsertEdge {
                    src: newer.clone(),
                    dst: older.clone(),
                    rel: "SUPERSEDES".into(),
                    props: Some(json!({"layer": "llm"})),
                }],
                vec![edge_claim(
                    &newer,
                    "SUPERSEDES",
                    &older,
                    Layer::Llm,
                    Method::Llm,
                    10_000,
                    "l1",
                )],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        report.verdicts.get(&AssertionStatus::Candidate),
        Some(&1),
        "{report:?}"
    );
    let conn = raw(&s);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM edge"),
        0,
        "a candidate materialises no edge, structural or projected"
    );
    let target = store.get(&older, true).unwrap().unwrap();
    assert_eq!(
        (target.governance, target.lifecycle),
        (Governance::Accepted, kaban_core::Lifecycle::Active),
        "no lifecycle effect before a winner"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM node_candidate"),
        1,
        "the review row is the one output"
    );
}

/// Negative (EN-S3, F17): a claim naming another workspace's node fails the whole batch
/// before any claim row is written; the error names the id, never the other tenant's data.
#[test]
fn a_claim_naming_another_workspaces_node_fails_the_whole_batch() {
    let s = Scratch::new("e22-assert", "foreign");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let other_ws = WorkspaceId::new("other");
    let other_ctx = WriteCtx::new("v1", other_ws.clone(), Actor::Agent);
    let foreign = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(other_ws, vec![upsert(&foreign, "Fact", "theirs")]),
            &other_ctx,
        )
        .unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let err = store
        .apply_batch(
            batch(
                vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
                vec![
                    edge_claim(
                        &a,
                        "RELATES_TO",
                        &b,
                        Layer::Gliner,
                        Method::Gliner,
                        9000,
                        "l1",
                    ),
                    edge_claim(
                        &a,
                        "RELATES_TO",
                        &foreign,
                        Layer::Gliner,
                        Method::Gliner,
                        9000,
                        "l2",
                    ),
                ],
            ),
            &ctx(),
        )
        .expect_err("a foreign object fails the batch");
    assert!(
        matches!(err, StoreError::AssertionSubjectNotFound(ref id) if id == foreign.as_str()),
        "{err:?}"
    );
    assert!(
        store.get(&a, true).unwrap().is_none(),
        "the batch rolled back"
    );
    let conn = raw(&s);
    assert_eq!(count(&conn, "SELECT count(*) FROM assertion"), 0);
}

/// EN-S9 schema test: a chunk's text never reaches an `assertion` or `assertion_status`
/// column nor an edge's props; claims carry offsets and ids only.
#[test]
fn no_assertion_column_receives_chunk_text() {
    let s = Scratch::new("e22-assert", "canary");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let canary = "CANARY-7f3e-surface-text";
    let (chunk, vendor) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(
                vec![
                    upsert(&chunk, "Chunk", &format!("the chunk says {canary} loudly")),
                    upsert(&vendor, "Fact", "a vendor node"),
                ],
                vec![
                    BatchAssertion {
                        subject: BatchSubject::Span {
                            chunk: chunk.clone(),
                            offset: 15,
                            len: 24,
                        },
                        claim_class: ClaimClass::Type,
                        predicate: None,
                        object: None,
                        value: json!("Vendor"),
                        layer: Layer::Gazetteer,
                        method: Method::Exact,
                        extractor_id: None,
                        extractor_version: None,
                        source_id: None,
                        lineage_id: "g1".into(),
                        raw_score: Fixed::new(9000).unwrap(),
                        rank: Rank::Normal,
                        rank_reason: None,
                        calibration_version: 1,
                        supersedes: None,
                    },
                    edge_claim(
                        &chunk,
                        "MENTIONS",
                        &vendor,
                        Layer::Gliner,
                        Method::Gliner,
                        9000,
                        "l1",
                    ),
                ],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    for row in claim_rows(&conn) {
        for col in row.into_iter().flatten() {
            assert!(
                !col.contains(canary),
                "assertion column carries chunk text: {col}"
            );
        }
    }
    let mut st = conn.prepare("SELECT * FROM assertion_status").unwrap();
    let n = st.column_count();
    let mut rows = st.query([]).unwrap();
    while let Some(r) = rows.next().unwrap() {
        for i in 0..n {
            if let Ok(text) = r.get::<_, String>(i) {
                assert!(!text.contains(canary), "status column carries chunk text");
            }
        }
    }
    let props: Vec<String> = conn
        .prepare("SELECT props FROM edge")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_ne!(props.len(), 0);
    assert!(props.iter().all(|p| !p.contains(canary)));
}

/// A reconciler whose every verdict is fixed (RP-10): proves `apply_batch` honours any
/// `Verdict` and never reaches the edge upsert on its own.
struct FakeReconciler(Verdict);

impl Reconciler for FakeReconciler {
    fn decide(&self, group: &[Assertion], _ctx: &ReconcileCtx<'_>) -> Verdict {
        match &self.0 {
            Verdict::Candidate { reason, .. } => Verdict::Candidate {
                ranked: group.iter().map(|a| (a.key.clone(), Fixed::ONE)).collect(),
                reason: *reason,
            },
            other => other.clone(),
        }
    }
}

/// Slice (EN-E9, RP-10): `FakeReconciler(Candidate)` projects a `node_candidate` row (reason
/// `below_margin`) for a group `WeightedVoteV1` would have won and writes no edge;
/// `FakeReconciler(Rejected)` writes statuses only.
#[test]
fn a_fake_reconciler_candidate_verdict_projects_a_review_row_and_no_edge() {
    for (verdict, status) in [
        (
            Verdict::Candidate {
                ranked: vec![],
                reason: CandidateReason::BelowMargin,
            },
            "candidate",
        ),
        (
            Verdict::Rejected {
                reason: "fixture".into(),
            },
            "rejected",
        ),
    ] {
        let s = Scratch::new("e22-assert", &format!("fake-{status}"));
        let store = SqliteStore::open(&s.db(), &s.keys())
            .unwrap()
            .with_reconciler(Box::new(FakeReconciler(verdict)));
        let (a, b) = (NodeId::new(), NodeId::new());
        let report = store
            .apply_batch(
                batch(
                    vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
                    vec![edge_claim(
                        &a,
                        "RELATES_TO",
                        &b,
                        Layer::Llm,
                        Method::Llm,
                        9000,
                        "l1",
                    )],
                ),
                &ctx(),
            )
            .unwrap();
        let conn = raw(&s);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM edge"),
            0,
            "{status}: no edge"
        );
        assert!(
            store
                .changes_read(0, 10_000)
                .unwrap()
                .iter()
                .all(|c| c.entity != ChangeEntity::Edge),
            "{status}: the edge upsert was never reached"
        );
        assert_eq!(statuses(&conn)[0].1, status);
        assert_eq!(
            report
                .verdicts
                .get(&AssertionStatus::parse(status).unwrap()),
            Some(&1)
        );
        let reviews = count(
            &conn,
            "SELECT count(*) FROM node_candidate WHERE reason = 'below_margin'",
        );
        assert_eq!(reviews, i64::from(status == "candidate"), "{status}");
        // one `status` ChangeLog row per changed status (pending -> verdict)
        let status_rows = store
            .changes_read(0, 10_000)
            .unwrap()
            .iter()
            .filter(|c| c.entity == ChangeEntity::Assertion && c.op == ChangeOp::Status)
            .count();
        assert_eq!(status_rows, 1, "{status}");
    }
}

/// Item 10 (E1.8 residual): `kind_stats.mean_score` of a discovered kind follows the winner
/// confidences of the edges projected from its nodes (llm 0.8 x prior 0.75 = 0.6).
#[test]
fn mean_score_follows_the_winner_confidences_of_the_kinds_edges() {
    let s = Scratch::new("e22-assert", "mean-score");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (w, b) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(
                vec![
                    upsert(&w, "Widget", "a discovered widget"),
                    upsert(&b, "Fact", "node b"),
                ],
                vec![edge_claim(
                    &w,
                    "RELATES_TO",
                    &b,
                    Layer::Llm,
                    Method::Llm,
                    8000,
                    "l1",
                )],
            ),
            &ctx(),
        )
        .unwrap();
    let stats = store.kind_stats(&WorkspaceId::default()).unwrap();
    let widget = stats
        .iter()
        .find(|k| k.kind == "Widget")
        .expect("the discovered kind has a stats row");
    assert_eq!(widget.mean_score.map(fixed), Some(6000));
}
