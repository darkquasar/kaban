//! Rank partition, lineage, the unresolved verdict and the human and accepted-record
//! protection (engine-spec EN-F12b, EN-F12c, EN-S10; engine-plan E2.3): what reaches the one
//! edge upsert and what never does. Invented data throughout.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ChangeLogStore, ClaimClass, Fixed, Governance,
    GovernanceStore, IngestStore, Layer, MemoryStore, Method, NewEvent, NewNode, NodeId, Op, Rank,
    SignerKind, StoreError, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::collections::BTreeMap;

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn human_ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli)
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

/// An `edge` claim about `src -rel-> dst` with `raw` (1/10000) under `lineage` and `rank`.
#[allow(clippy::too_many_arguments)] // a fixture row
fn edge_claim(
    src: &NodeId,
    rel: &str,
    dst: &NodeId,
    layer: Layer,
    method: Method,
    raw: u32,
    lineage: &str,
    rank: Rank,
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
        extractor_id: Some(lineage.to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

/// A `type` claim `value` about node `node` (the fixture id travels in `extractor_id`).
fn type_claim(
    node: &NodeId,
    value: &str,
    layer: Layer,
    method: Method,
    raw: u32,
    lineage: &str,
    rank: Rank,
) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Node(node.clone()),
        claim_class: ClaimClass::Type,
        predicate: None,
        object: None,
        value: json!(value),
        layer,
        method,
        extractor_id: Some(lineage.to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(raw).unwrap(),
        rank,
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

fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `extractor_id -> disposition` (`None` for a claim without one: the `link` claim).
fn statuses(conn: &Connection) -> BTreeMap<Option<String>, String> {
    conn.prepare(
        "SELECT a.extractor_id, s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Every column of every `edge` row as text, ordered by rowid (byte-identity checks).
fn edge_rows(conn: &Connection) -> Vec<Vec<Option<String>>> {
    let mut st = conn.prepare("SELECT * FROM edge ORDER BY rowid").unwrap();
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

#[allow(clippy::cast_possible_truncation)] // 0..=10000 after rounding
fn fixed(real: f64) -> i64 {
    (real * 10_000.0).round() as i64
}

fn expect(pairs: &[(Option<&str>, &str)]) -> BTreeMap<Option<String>, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.map(str::to_owned), (*v).to_owned()))
        .collect()
}

/// A Decision whose props satisfy its kind's schema (`rationale` is required): accepting it
/// is a human `kind_of_record` claim v1 can validate as a record (E2.3a).
fn valid_decision(store: &SqliteStore) -> NodeId {
    match store
        .remember(
            NewNode {
                kind: "Decision".into(),
                text: "ship weekly".into(),
                props: Some(json!({"rationale": "a weekly cadence"})),
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
}

/// Slice (EN-F12c): a human `link` is a `preferred` claim under the human lineage; a
/// gazetteer assertion on the same triple afterwards leaves the edge row byte-identical and
/// becomes `dissent`.
#[test]
fn a_human_link_followed_by_a_gazetteer_assertion_leaves_the_edge_row_byte_identical() {
    let s = Scratch::new("e23-prot", "human-link");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = common::created(&store, "Fact", "a human note");
    let b = common::created(&store, "Fact", "another human note");
    let (a, b) = (NodeId::parse(&a.id).unwrap(), NodeId::parse(&b.id).unwrap());
    // a low human confidence: a plain vote would let the gazetteer (0.8 x 0.9) outweigh it
    store
        .link(&a, &b, "RELATES_TO", Some(0.5), None, &human_ctx())
        .unwrap();
    let conn = raw(&s);
    let (rank, lineage): (String, String) = conn
        .query_row("SELECT rank, lineage_id FROM assertion", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (rank.as_str(), lineage.as_str()),
        ("preferred", "human:cli")
    );
    let before = edge_rows(&conn);
    assert_eq!(before.len(), 1);

    store
        .apply_batch(
            batch(
                vec![],
                vec![edge_claim(
                    &a,
                    "RELATES_TO",
                    &b,
                    Layer::Gazetteer,
                    Method::Alias,
                    9000,
                    "g1",
                    Rank::Normal,
                )],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(edge_rows(&conn), before, "the human's row is untouched");
    assert_eq!(
        statuses(&conn),
        expect(&[(None, "winner"), (Some("g1"), "dissent")])
    );
}

/// Negative (E2 interim review RE4-5; EN-F12c, EN-S10): a bare producer edge op on a triple
/// a human claim wins is refused with the fixed `edge_requires_assertion` class before any
/// write: the human's row stays byte-identical and no `ChangeLog` row is added.
#[test]
fn a_structural_write_on_a_protected_human_edge_is_refused() {
    let s = Scratch::new("e23-prot", "structural-refused");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = common::created(&store, "Fact", "a human note");
    let b = common::created(&store, "Fact", "another human note");
    let (a, b) = (NodeId::parse(&a.id).unwrap(), NodeId::parse(&b.id).unwrap());
    store
        .link(&a, &b, "RELATES_TO", None, None, &human_ctx())
        .unwrap();
    let conn = raw(&s);
    let before = edge_rows(&conn);
    assert_eq!(before.len(), 1);
    let changes = store.changes_read(0, 10_000).unwrap().len();

    let err = store
        .apply_batch(
            batch(
                vec![Op::UpsertEdge {
                    src: a.clone(),
                    dst: b.clone(),
                    rel: "RELATES_TO".into(),
                    props: None,
                }],
                vec![],
            ),
            &ctx(),
        )
        .expect_err("a producer does not rewrite a human's edge");
    assert!(matches!(err, StoreError::EdgeRequiresAssertion), "{err:?}");
    assert_eq!(
        edge_rows(&conn),
        before,
        "the human's row is byte-identical"
    );
    assert_eq!(store.changes_read(0, 10_000).unwrap().len(), changes);
}

/// Slice (EN-F12c, the protection set over a store that predates the rank): a `normal`-rank
/// human winner protects its group; a stronger machine value against it is
/// `Unresolved{human_protected}` (the human stays `winner`, the other value `unresolved`), a
/// machine supporter agreeing with it is `dissent`.
#[test]
fn a_machine_value_against_a_normal_rank_human_winner_is_unresolved_and_agreement_is_dissent() {
    let s = Scratch::new("e23-prot", "human-winner");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let n = NodeId::new();
    store
        .apply_batch(
            batch(
                vec![upsert(&n, "Fact", "a node")],
                vec![type_claim(
                    &n,
                    "Fact",
                    Layer::Human,
                    Method::Link,
                    5000,
                    "h-old",
                    Rank::Normal,
                )],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    assert_eq!(statuses(&conn), expect(&[(Some("h-old"), "winner")]));
    // gliner 0.6 x 1.0 = 6000 beats the human's 5000 plus the agreeing llm's 750 in a
    // plain vote
    let report = store
        .apply_batch(
            batch(
                vec![],
                vec![
                    type_claim(
                        &n,
                        "Idea",
                        Layer::Gliner,
                        Method::Gliner,
                        10_000,
                        "g1",
                        Rank::Normal,
                    ),
                    type_claim(
                        &n,
                        "Fact",
                        Layer::Llm,
                        Method::Llm,
                        1000,
                        "l1",
                        Rank::Normal,
                    ),
                ],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        report
            .verdicts
            .get(&kaban_core::AssertionStatus::Unresolved),
        Some(&1),
        "{report:?}"
    );
    assert_eq!(
        statuses(&conn),
        expect(&[
            (Some("h-old"), "winner"),
            (Some("g1"), "unresolved"),
            (Some("l1"), "dissent"),
        ])
    );
}

/// E2 interim review RE4-7 (EN-F12b supersession, EN-F12c): a human winner a later claim
/// `supersedes` is out of the vote (`votes()`) and out of the protection set, so the group
/// is decided by what remains: the superseding machine claim wins, the human is `dissent`,
/// and the edge row is re-projected with the machine's layer and confidence (gliner 0.6 x
/// 0.9), not kept byte-identical as the superseded human's.
#[test]
fn a_superseded_human_winner_no_longer_protects() {
    let s = Scratch::new("e23-prot", "superseded-human");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(
                vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
                vec![edge_claim(
                    &a,
                    "RELATES_TO",
                    &b,
                    Layer::Human,
                    Method::Link,
                    5000,
                    "h-old",
                    Rank::Normal,
                )],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    assert_eq!(statuses(&conn), expect(&[(Some("h-old"), "winner")]));
    let human: String = conn
        .query_row("SELECT key FROM assertion", [], |r| r.get(0))
        .unwrap();
    let mut superseding = edge_claim(
        &a,
        "RELATES_TO",
        &b,
        Layer::Gliner,
        Method::Gliner,
        9000,
        "g1",
        Rank::Normal,
    );
    superseding.supersedes = Some(kaban_core::AssertionKey(human));
    store
        .apply_batch(batch(vec![], vec![superseding]), &ctx())
        .unwrap();
    assert_eq!(
        statuses(&conn),
        expect(&[(Some("h-old"), "dissent"), (Some("g1"), "winner")])
    );
    let (confidence, layer): (f64, String) = conn
        .query_row("SELECT confidence, layer FROM edge", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((fixed(confidence), layer.as_str()), (5400, "gliner"));
}

/// Slice (EN-F12c): an L3 edge of another confidence incident to an accepted Decision over
/// `EVIDENCE_FOR` is `unresolved` and the materialised edge is unchanged; the first machine
/// edge to the record was created normally.
#[test]
fn an_l3_edge_of_another_confidence_incident_to_an_accepted_decision_is_unresolved() {
    let s = Scratch::new("e23-prot", "accepted-record");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let decision = valid_decision(&store);
    store
        .transition(
            &decision,
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                path_scope: vec!["src/**".into()],
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &human_ctx(),
        )
        .unwrap();
    let chunk = NodeId::new();
    store
        .apply_batch(
            batch(
                vec![upsert(&chunk, "Chunk", "evidence text")],
                vec![edge_claim(
                    &chunk,
                    "EVIDENCE_FOR",
                    &decision,
                    Layer::Gliner,
                    Method::Gliner,
                    9000,
                    "g1",
                    Rank::Normal,
                )],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    let before = edge_rows(&conn);
    assert_eq!(
        before.len(),
        1,
        "the first machine edge is created normally"
    );
    let report = store
        .apply_batch(
            batch(
                vec![],
                vec![edge_claim(
                    &chunk,
                    "EVIDENCE_FOR",
                    &decision,
                    Layer::Llm,
                    Method::Llm,
                    9500,
                    "l1",
                    Rank::Normal,
                )],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        report
            .verdicts
            .get(&kaban_core::AssertionStatus::Unresolved),
        Some(&1),
        "{report:?}"
    );
    assert_eq!(edge_rows(&conn), before, "the edge row is unchanged");
    assert_eq!(
        statuses(&conn),
        expect(&[
            (None, "winner"),
            (Some("g1"), "unresolved"),
            (Some("l1"), "unresolved"),
        ])
    );
}

/// Slice (EN-F12c, E2.3a): accepting a governed record is a `preferred` human claim about its
/// kind (`method: accept`, lineage `human:cli:<event seq>`, `winner`) written in the
/// acceptance's transaction; a machine `kind_of_record` claim of another kind afterwards is
/// `unresolved`, the human claim stays the winner and the record is untouched.
#[test]
fn accepting_a_record_is_a_preferred_human_claim_a_machine_kind_claim_cannot_unseat() {
    let s = Scratch::new("e23-prot", "accept-claim");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let decision = valid_decision(&store);
    let (accepted, seq) = store
        .transition(
            &decision,
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &human_ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    let claim: (String, String, String, String, String, String) = conn
        .query_row(
            "SELECT claim_class, layer, method, rank, lineage_id, value FROM assertion",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        claim,
        (
            "kind_of_record".into(),
            "human".into(),
            "accept".into(),
            "preferred".into(),
            format!("human:cli:{seq}"),
            r#"{"kind":"Decision","props":{"rationale":"a weekly cadence"}}"#.into(),
        )
    );
    assert_eq!(statuses(&conn), expect(&[(None, "winner")]));

    store
        .apply_batch(
            batch(
                vec![],
                vec![BatchAssertion {
                    subject: BatchSubject::Record(decision.clone()),
                    claim_class: ClaimClass::KindOfRecord,
                    predicate: None,
                    object: None,
                    value: json!({"kind": "Idea", "props": {}}),
                    layer: Layer::Llm,
                    method: Method::Llm,
                    extractor_id: Some("l1".into()),
                    extractor_version: Some("1".into()),
                    source_id: Some("doc-1".into()),
                    lineage_id: "l1".into(),
                    raw_score: Fixed::new(9500).unwrap(),
                    rank: Rank::Normal,
                    rank_reason: None,
                    calibration_version: 1,
                    supersedes: None,
                }],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        statuses(&conn),
        expect(&[(None, "winner"), (Some("l1"), "unresolved")])
    );
    let still = store.get(&decision, false).unwrap().unwrap();
    assert_eq!(
        (still.kind.as_str(), still.governance, still.rev),
        ("Decision", Governance::Accepted, accepted.rev)
    );
}

/// Slice (EN-F12c, E2.3b): a person's acceptance is the authority. Accepting a Decision whose
/// props fail its kind's schema (no `rationale`) is still a `winner` human claim (`layer:
/// human` is exempt from the record validity gate); a machine `kind_of_record` claim for the
/// same record afterwards is `unresolved` against it.
#[test]
fn accepting_a_schema_invalid_record_is_a_winning_human_claim_that_still_protects_it() {
    let s = Scratch::new("e23-prot", "accept-invalid");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    // the store seam does not validate props at `remember`: a Decision without `rationale`
    let decision = common::created(&store, "Decision", "ship weekly, no rationale");
    let decision = NodeId::parse(&decision.id).unwrap();
    store
        .transition(
            &decision,
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &human_ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    let (layer, kind): (String, String) = conn
        .query_row(
            "SELECT layer, json_extract(value, '$.kind') FROM assertion",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((layer.as_str(), kind.as_str()), ("human", "Decision"));
    assert_eq!(
        statuses(&conn),
        expect(&[(None, "winner")]),
        "the human claim wins whatever the kind schema says"
    );

    let report = store
        .apply_batch(
            batch(
                vec![],
                vec![BatchAssertion {
                    subject: BatchSubject::Record(decision.clone()),
                    claim_class: ClaimClass::KindOfRecord,
                    predicate: None,
                    object: None,
                    value: json!({"kind": "Idea", "props": {}}),
                    layer: Layer::Llm,
                    method: Method::Llm,
                    extractor_id: Some("l1".into()),
                    extractor_version: Some("1".into()),
                    source_id: Some("doc-1".into()),
                    lineage_id: "l1".into(),
                    raw_score: Fixed::new(9500).unwrap(),
                    rank: Rank::Normal,
                    rank_reason: None,
                    calibration_version: 1,
                    supersedes: None,
                }],
            ),
            &ctx(),
        )
        .unwrap();
    // the `preferred` human claim is the only voter: the group's verdict is its win, and the
    // machine claim of another value is `unresolved` under it (projection, EN-F12c)
    assert_eq!(
        report.verdicts.get(&kaban_core::AssertionStatus::Winner),
        Some(&1),
        "{report:?}"
    );
    assert_eq!(
        statuses(&conn),
        expect(&[(None, "winner"), (Some("l1"), "unresolved")])
    );
    let still = store.get(&decision, false).unwrap().unwrap();
    assert_eq!(
        (still.kind.as_str(), still.governance),
        ("Decision", Governance::Accepted)
    );
}

/// Slice (EN-F12b): two `preferred` supporters of different values from different lineages
/// are `Unresolved{preferred_conflict}`: both listed, no winner.
#[test]
fn two_preferred_values_from_different_lineages_are_an_unresolved_preferred_conflict() {
    let s = Scratch::new("e23-prot", "preferred-conflict");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let n = NodeId::new();
    let report = store
        .apply_batch(
            batch(
                vec![upsert(&n, "Fact", "a node")],
                vec![
                    type_claim(
                        &n,
                        "Fact",
                        Layer::Human,
                        Method::Link,
                        10_000,
                        "human:cli:1",
                        Rank::Preferred,
                    ),
                    type_claim(
                        &n,
                        "Idea",
                        Layer::Human,
                        Method::Link,
                        10_000,
                        "human:cli:2",
                        Rank::Preferred,
                    ),
                ],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        report
            .verdicts
            .get(&kaban_core::AssertionStatus::Unresolved),
        Some(&1),
        "{report:?}"
    );
    assert_eq!(
        statuses(&raw(&s)),
        expect(&[
            (Some("human:cli:1"), "unresolved"),
            (Some("human:cli:2"), "unresolved"),
        ])
    );
}

/// Every ordering of `items` (a fixture of three claims: six orders).
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(i);
        for mut tail in permutations(&rest) {
            tail.insert(0, head.clone());
            out.push(tail);
        }
    }
    out
}

/// Apply `claims` (`preferred` human `type` claims `(value, lineage, raw)` about one fresh
/// node) in every order, each on a fresh store, and assert the group is unresolved whatever
/// the order: one `Unresolved` verdict in the report, every supporter `unresolved`, no winner.
fn every_order_is_a_preferred_conflict(name: &str, claims: &[(&str, &str, u32)]) {
    for (i, order) in permutations(claims).into_iter().enumerate() {
        let s = Scratch::new("e23-prot", &format!("{name}-{i}"));
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        let n = NodeId::new();
        let report = store
            .apply_batch(
                batch(
                    vec![upsert(&n, "Fact", "a node")],
                    order
                        .iter()
                        .map(|(value, lineage, raw)| {
                            type_claim(
                                &n,
                                value,
                                Layer::Human,
                                Method::Link,
                                *raw,
                                lineage,
                                Rank::Preferred,
                            )
                        })
                        .collect(),
                ),
                &ctx(),
            )
            .unwrap();
        assert_eq!(
            report.verdicts,
            [(kaban_core::AssertionStatus::Unresolved, 1)].into(),
            "order {order:?}: {report:?}"
        );
        let want: Vec<(Option<&str>, &str)> = order
            .iter()
            .map(|(_, lineage, _)| (Some(*lineage), "unresolved"))
            .collect();
        assert_eq!(statuses(&raw(&s)), expect(&want), "order {order:?}");
    }
}

/// E2 interim review RE4-8 (EN-F12b): the preferred conflict is judged over every eligible
/// `preferred` supporter, not the two representatives. `A` from `h1` (10000) against `B` from
/// `h1` (4000) and from `h2` (1000): both representatives are `h1`'s, yet `A/h1` and `B/h2`
/// are two humans of different lineages preferring different values, which no vote resolves.
#[test]
fn a_hidden_preferred_lineage_conflict_is_unresolved_whatever_the_representatives_are() {
    every_order_is_a_preferred_conflict(
        "hidden-conflict",
        &[
            ("Fact", "h1", 10_000),
            ("Idea", "h1", 4000),
            ("Idea", "h2", 1000),
        ],
    );
}

/// E2 interim review RE4-8: a third value carries the other lineage (`A/h1`, `B/h1`, `C/h2`):
/// the two leading representatives share a lineage, the conflict sits with the third value.
#[test]
fn a_third_preferred_value_from_another_lineage_is_a_preferred_conflict() {
    every_order_is_a_preferred_conflict(
        "third-value",
        &[
            ("Fact", "h1", 10_000),
            ("Idea", "h1", 4000),
            ("Failure", "h2", 1000),
        ],
    );
}

/// Slice (EN-F12b): a `deprecated` supporter is listed as dissent and does not vote: the edge
/// carries the remaining supporter's confidence, not the deprecated one's larger weight.
#[test]
fn a_deprecated_supporter_is_listed_as_dissent_and_does_not_vote() {
    let s = Scratch::new("e23-prot", "deprecated");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(
                vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
                vec![
                    // gliner 0.6 x 0.9 = 5400 would win; deprecated, it does not vote
                    edge_claim(
                        &a,
                        "RELATES_TO",
                        &b,
                        Layer::Gliner,
                        Method::Gliner,
                        9000,
                        "g1",
                        Rank::Deprecated,
                    ),
                    // llm 0.75 x 0.6 = 4500
                    edge_claim(
                        &a,
                        "RELATES_TO",
                        &b,
                        Layer::Llm,
                        Method::Llm,
                        6000,
                        "l1",
                        Rank::Normal,
                    ),
                ],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    let (confidence, layer): (f64, String) = conn
        .query_row("SELECT confidence, layer FROM edge", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((fixed(confidence), layer.as_str()), (4500, "llm"));
    assert_eq!(
        statuses(&conn),
        expect(&[(Some("g1"), "dissent"), (Some("l1"), "winner")])
    );
}

/// Slice (EN-F12b): the same claim from one extractor over two chunks of one document counts
/// once (one lineage), so an independent lineage of higher weight wins.
#[test]
fn the_same_claim_from_one_extractor_over_two_chunks_counts_once() {
    let s = Scratch::new("e23-prot", "lineage-once");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let n = NodeId::new();
    let mut twice_a = type_claim(
        &n,
        "Idea",
        Layer::Gliner,
        Method::Gliner,
        6000,
        "g-doc",
        Rank::Normal,
    );
    twice_a.source_id = Some("chunk-1".into());
    let mut twice_b = twice_a.clone();
    twice_b.source_id = Some("chunk-2".into());
    store
        .apply_batch(
            batch(
                vec![upsert(&n, "Fact", "a node")],
                vec![
                    // gliner 0.6 x 0.6 = 3600 per row: 7200 if both counted, 3600 once
                    twice_a,
                    twice_b,
                    // llm 0.75 x 1.0 = 7500: margin 3514 over one lineage (a winner),
                    // 204 over two (below tau_type 3000: a candidate)
                    type_claim(
                        &n,
                        "Fact",
                        Layer::Llm,
                        Method::Llm,
                        10_000,
                        "l1",
                        Rank::Normal,
                    ),
                ],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    assert_eq!(count(&conn, "SELECT count(*) FROM assertion"), 3);
    let st = statuses(&conn);
    assert_eq!(
        st.get(&Some("l1".to_owned())).map(String::as_str),
        Some("winner")
    );
    assert_eq!(
        st.get(&Some("g-doc".to_owned())).map(String::as_str),
        Some("dissent")
    );
}

/// Slice (EN-S10): a reconciler group whose rel carries governed semantics (`CONTRADICTS`,
/// declared without the flag in the shipped file, so the rel list itself routes it) yields
/// `Candidate{governed_semantics}` whatever the vote: a review row, no edge.
#[test]
fn a_group_on_a_governed_semantics_rel_is_a_candidate_whatever_the_vote() {
    let s = Scratch::new("e23-prot", "governed-rel");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, b) = (NodeId::new(), NodeId::new());
    let report = store
        .apply_batch(
            batch(
                vec![upsert(&a, "Fact", "node a"), upsert(&b, "Fact", "node b")],
                vec![edge_claim(
                    &a,
                    "CONTRADICTS",
                    &b,
                    Layer::Llm,
                    Method::Llm,
                    10_000,
                    "l1",
                    Rank::Normal,
                )],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        report.verdicts.get(&kaban_core::AssertionStatus::Candidate),
        Some(&1),
        "{report:?}"
    );
    let conn = raw(&s);
    assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 0);
    let reason: String = conn
        .query_row("SELECT reason FROM node_candidate", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reason, "governed_semantics");
}
