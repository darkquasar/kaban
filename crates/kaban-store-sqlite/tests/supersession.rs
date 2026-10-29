//! Supersession validity (OWN-86 (3); engine-spec EN-F8, EN-F8a): a batch claim's
//! `supersedes` names a claim of its own conflict group (workspace, claim class, subject,
//! detail), stored in the batch's workspace or earlier or later in the same batch, and the
//! batch's references form no cycle. A refusal fails the whole batch before any claim row is
//! written; its text is the fixed class, never the key. Invented data throughout.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, Assertion, AssertionKey, BatchAssertion, BatchSubject, CanonicalJson, ClaimClass, Fixed,
    ForgetMode, IngestStore, Layer, MemoryStore, Method, NewNode, NodeId, Op, Rank, Subject,
    WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

const REL: &str = "RELATES_TO";

#[test]
fn stored_claim_replay_after_superseded_object_purge_is_a_noop() {
    let scratch = Scratch::new("supersession", "replay-after-purge");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let ws = WorkspaceId::default();
    let ctx = ctx_in(&ws);
    let (chunk, old_target, new_target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut prior = edge_claim(&chunk, &old_target, Layer::Gazetteer, 9000, "prior");
    prior.subject = BatchSubject::Span {
        chunk: chunk.clone(),
        offset: 0,
        len: 7,
    };
    prior.claim_class = ClaimClass::CanonicalId;
    prior.method = Method::Id;
    prior.object = Some(old_target.clone());
    prior.value = json!(old_target.as_str());
    store
        .apply_batch(
            batch(
                &ws,
                vec![
                    Op::UpsertNode {
                        id: chunk.clone(),
                        node: NewNode {
                            kind: "Chunk".into(),
                            text: "Mention".into(),
                            ..Default::default()
                        },
                    },
                    upsert(&old_target, "old target"),
                    upsert(&new_target, "new target"),
                ],
                vec![prior.clone()],
            ),
            &ctx,
        )
        .unwrap();
    let conn = raw(&scratch);
    let old_key = AssertionKey(
        conn.query_row(
            "SELECT key FROM assertion WHERE lineage_id='prior'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    );
    let mut next = prior;
    next.object = Some(new_target);
    next.value = json!(next.object.as_ref().unwrap().as_str());
    next.lineage_id = "next".into();
    next.supersedes = Some(old_key.clone());
    let replay = batch(&ws, vec![], vec![next]);
    store.apply_batch(replay.clone(), &ctx).unwrap();
    store.forget(&old_target, ForgetMode::Purge, &ctx).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM assertion WHERE lineage_id='prior'"
        ),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM assertion WHERE lineage_id='next'"
        ),
        1
    );
    let before = count(&conn, "SELECT COALESCE(max(seq),0) FROM changes");
    let result = store.apply_batch(replay, &ctx);
    assert!(
        result.is_ok(),
        "stored claim replay must ignore its already-purged supersedes target: {result:?}"
    );
    assert_eq!(result.unwrap().assertions_noop, 1);
    assert_eq!(
        count(&conn, "SELECT COALESCE(max(seq),0) FROM changes"),
        before
    );
}

fn ctx_in(ws: &WorkspaceId) -> WriteCtx {
    WriteCtx::new("v1", ws.clone(), Actor::Agent)
}

fn upsert(id: &NodeId, text: &str) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: text.into(),
            ..Default::default()
        },
    }
}

/// A machine (or human) `edge` claim about `src -RELATES_TO-> dst` under `lineage`.
fn edge_claim(src: &NodeId, dst: &NodeId, layer: Layer, raw: u32, lineage: &str) -> BatchAssertion {
    let method = if layer == Layer::Human {
        Method::Link
    } else {
        Method::Gliner
    };
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: REL.into(),
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

fn superseding(mut claim: BatchAssertion, key: &AssertionKey) -> BatchAssertion {
    claim.supersedes = Some(key.clone());
    claim
}

fn batch(ws: &WorkspaceId, ops: Vec<Op>, assertions: Vec<BatchAssertion>) -> WriteBatch {
    let mut b = WriteBatch::new(ws.clone(), ops);
    b.assertions = assertions;
    b
}

fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn rid(conn: &Connection, id: &NodeId) -> i64 {
    conn.query_row("SELECT rid FROM node WHERE id = ?1", [id.as_str()], |r| {
        r.get(0)
    })
    .unwrap()
}

/// The key the store gives an edge claim of `ws` (EN-F12a identity), computed before the
/// batch that carries it so that a claim of the same batch can name it.
fn key_of(conn: &Connection, ws: &WorkspaceId, c: &BatchAssertion) -> AssertionKey {
    let BatchSubject::Edge { src, rel, dst } = &c.subject else {
        panic!("edge claims only")
    };
    Assertion {
        key: AssertionKey::default(),
        workspace_id: ws.clone(),
        subject: Subject::Edge {
            src: rid(conn, src),
            rel: rel.clone(),
            dst: rid(conn, dst),
        },
        claim_class: c.claim_class,
        predicate: None,
        object_rid: None,
        value: CanonicalJson::new(c.value.clone()),
        layer: c.layer,
        method: c.method,
        extractor_id: c.extractor_id.clone(),
        extractor_version: c.extractor_version.clone(),
        source_id: c.source_id.clone(),
        lineage_id: c.lineage_id.clone(),
        raw_score: c.raw_score,
        rank: c.rank,
        rank_reason: None,
        calibration_version: c.calibration_version,
        supersedes: None,
        ts: 0,
    }
    .with_key()
    .unwrap()
    .key
}

/// The disposition of every claim by lineage, ordered by lineage.
fn dispositions(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare(
        "SELECT a.lineage_id, s.disposition FROM assertion a
         JOIN assertion_status s ON s.assertion_id = a.id ORDER BY a.lineage_id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The refusal's text is its class and nothing else; neither renderer carries the key.
fn assert_class(err: &kaban_core::StoreError, class: &str, key: &AssertionKey, case: &str) {
    assert_eq!(err.to_string(), class, "{case}: the refusal's class");
    assert!(
        !err.to_string().contains(&key.0),
        "{case}: Display carried the key"
    );
    assert!(
        !format!("{err:?}").contains(&key.0),
        "{case}: Debug carried the key"
    );
}

/// Red (OWN-86 (3), EN-S3): a claim may not supersede another tenant's claim. The foreign key
/// is refused as `supersedes_not_found`, exactly as an unknown key is (no existence oracle
/// across tenants), and the whole batch fails before any claim row is written.
#[test]
fn a_claim_superseding_another_workspaces_claim_fails_the_batch_as_not_found() {
    let s = Scratch::new("s19-c2-supersede", "cross-ws");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let other = WorkspaceId::new("other");
    let (their_a, their_b) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(
                &other,
                vec![upsert(&their_a, "their a"), upsert(&their_b, "their b")],
                vec![edge_claim(
                    &their_a,
                    &their_b,
                    Layer::Gliner,
                    9000,
                    "theirs",
                )],
            ),
            &ctx_in(&other),
        )
        .unwrap();
    let conn = raw(&s);
    let foreign = AssertionKey(
        conn.query_row(
            "SELECT key FROM assertion WHERE workspace_id = 'other'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    );
    let ws = WorkspaceId::default();
    let unknown = AssertionKey("0".repeat(64));
    for (case, target) in [("foreign", &foreign), ("unknown", &unknown)] {
        let (a, b) = (NodeId::new(), NodeId::new());
        let err = store
            .apply_batch(
                batch(
                    &ws,
                    vec![upsert(&a, "node a"), upsert(&b, "node b")],
                    vec![superseding(
                        edge_claim(&a, &b, Layer::Gliner, 9000, "ours"),
                        target,
                    )],
                ),
                &ctx_in(&ws),
            )
            .expect_err("another tenant's claim is no supersedes target");
        assert_class(&err, "supersedes_not_found", target, case);
        assert!(
            store.get(&a, true).unwrap().is_none(),
            "{case}: rolled back"
        );
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM assertion WHERE workspace_id <> 'other'"
            ),
            0,
            "{case}: no claim row written"
        );
    }
}

/// Red (OWN-86 (3)): inside one workspace a claim supersedes only a claim of its own conflict
/// group; another subject or another class is `supersedes_cross_group`, for a stored target
/// and for a target in the same batch.
#[test]
fn a_claim_superseding_another_conflict_group_fails_the_batch() {
    let s = Scratch::new("s19-c2-supersede", "cross-group");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = WorkspaceId::default();
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    let stored = edge_claim(&a, &b, Layer::Gliner, 9000, "g-stored");
    store
        .apply_batch(
            batch(
                &ws,
                vec![
                    upsert(&a, "node a"),
                    upsert(&b, "node b"),
                    upsert(&c, "node c"),
                ],
                vec![stored.clone()],
            ),
            &ctx_in(&ws),
        )
        .unwrap();
    let conn = raw(&s);
    let stored_key = key_of(&conn, &ws, &stored);
    let mut other_class = BatchAssertion {
        subject: BatchSubject::Node(a.clone()),
        claim_class: ClaimClass::Type,
        value: Value::String("Fact".into()),
        ..edge_claim(&a, &b, Layer::Gliner, 9000, "g-type")
    };
    other_class.supersedes = Some(stored_key.clone());
    let in_batch = edge_claim(&a, &c, Layer::Gliner, 9000, "g-ac-first");
    let in_batch_key = key_of(&conn, &ws, &in_batch);
    let cases: Vec<(&str, Vec<BatchAssertion>, &AssertionKey)> = vec![
        (
            "stored, other subject",
            vec![superseding(
                edge_claim(&a, &c, Layer::Gliner, 9000, "g-ac"),
                &stored_key,
            )],
            &stored_key,
        ),
        ("stored, other class", vec![other_class], &stored_key),
        (
            "in batch, other subject",
            vec![
                in_batch,
                superseding(
                    edge_claim(&a, &b, Layer::Gliner, 9000, "g-ab"),
                    &in_batch_key,
                ),
            ],
            &in_batch_key,
        ),
    ];
    for (case, claims, target) in cases {
        let err = store
            .apply_batch(batch(&ws, vec![], claims), &ctx_in(&ws))
            .expect_err("a claim of another conflict group is no supersedes target");
        assert_class(&err, "supersedes_cross_group", target, case);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM assertion"),
            1,
            "{case}: no claim row written"
        );
    }
}

/// Red (OWN-86 (3)): a forward reference resolves inside the batch. The superseding claim is
/// listed before the claim it supersedes; the batch is accepted and decided as if the two had
/// arrived in two batches (the `a_superseded_human_winner_no_longer_protects` figures): the
/// superseded human claim no longer votes and the machine claim alone materialises the edge.
#[test]
fn a_forward_supersedes_reference_inside_one_batch_is_accepted_and_decided() {
    let s = Scratch::new("s19-c2-supersede", "forward");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = WorkspaceId::default();
    let (a, b) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(
                &ws,
                vec![upsert(&a, "node a"), upsert(&b, "node b")],
                vec![],
            ),
            &ctx_in(&ws),
        )
        .unwrap();
    let conn = raw(&s);
    let human = edge_claim(&a, &b, Layer::Human, 5000, "h-old");
    let human_key = key_of(&conn, &ws, &human);
    let machine = superseding(edge_claim(&a, &b, Layer::Gliner, 9000, "g1"), &human_key);
    store
        .apply_batch(batch(&ws, vec![], vec![machine, human]), &ctx_in(&ws))
        .expect("a forward reference inside the batch resolves");
    assert_eq!(
        dispositions(&conn),
        vec![
            ("g1".to_owned(), "winner".to_owned()),
            ("h-old".to_owned(), "dissent".to_owned())
        ]
    );
    let linked: String = conn
        .query_row(
            "SELECT t.key FROM assertion a JOIN assertion t ON t.id = a.supersedes
             WHERE a.lineage_id = 'g1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(linked, human_key.0, "the stored link names the human claim");
    let (confidence, layer): (f64, String) = conn
        .query_row("SELECT confidence, layer FROM edge", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        ((confidence * 10_000.0).round(), layer.as_str()),
        (5400.0, "gliner")
    );
}

/// Red (OWN-86 (3)): the batch's references never form a cycle, of two claims or longer;
/// `supersedes_cycle` fails the whole batch. A self-reference is filtered (a claim never
/// supersedes itself, as on the human path) and the claim is stored with no link.
#[test]
fn a_supersedes_cycle_inside_one_batch_fails_the_batch() {
    let s = Scratch::new("s19-c2-supersede", "cycle");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = WorkspaceId::default();
    let (a, b) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            batch(
                &ws,
                vec![upsert(&a, "node a"), upsert(&b, "node b")],
                vec![],
            ),
            &ctx_in(&ws),
        )
        .unwrap();
    let conn = raw(&s);
    let claims: Vec<BatchAssertion> = ["g-a", "g-b", "g-c"]
        .iter()
        .map(|l| edge_claim(&a, &b, Layer::Gliner, 9000, l))
        .collect();
    let keys: Vec<AssertionKey> = claims.iter().map(|c| key_of(&conn, &ws, c)).collect();
    let two = vec![
        superseding(claims[0].clone(), &keys[1]),
        superseding(claims[1].clone(), &keys[0]),
    ];
    let three = vec![
        superseding(claims[0].clone(), &keys[1]),
        superseding(claims[1].clone(), &keys[2]),
        superseding(claims[2].clone(), &keys[0]),
    ];
    for (case, cycle) in [("A->B->A", two), ("A->B->C->A", three)] {
        let err = store
            .apply_batch(batch(&ws, vec![], cycle), &ctx_in(&ws))
            .expect_err("a supersedes cycle fails the batch");
        assert_class(&err, "supersedes_cycle", &keys[0], case);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM assertion"),
            0,
            "{case}: no claim row written"
        );
    }
    store
        .apply_batch(
            batch(&ws, vec![], vec![superseding(claims[0].clone(), &keys[0])]),
            &ctx_in(&ws),
        )
        .expect("a self-reference is filtered, not refused");
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM assertion WHERE supersedes IS NULL"
        ),
        1,
        "the self-reference is stored as no link"
    );
}
