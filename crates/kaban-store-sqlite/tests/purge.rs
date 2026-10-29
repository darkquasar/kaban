//! The S5 purge and the tombstone over claims (engine-spec EN-F8a; engine-plan E2.2 item 6):
//! a purge removes every claim naming the node (one `delete` `ChangeLog` row each, statuses
//! cascaded) and re-decides every group that lost a member in the same transaction; a
//! tombstone deletes nothing and sets the node's claim statuses to `rejected`;
//! `PRAGMA foreign_key_check` stays empty. Invented data (`fixtures/reconcile/purge-redecide.json`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ChangeEntity, ChangeLogStore, ChangeOp, ClaimClass, Fixed,
    ForgetMode, IngestStore, Layer, MaintenanceOutcome, MemoryStore, Method, NewNode, NodeId, Op,
    Rank, RecallQuery, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write;

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

fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `(object node id, disposition)` of every claim, ordered by claim id.
fn dispositions(conn: &Connection) -> Vec<(Option<String>, String)> {
    conn.prepare(
        "SELECT o.id, s.disposition FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
         LEFT JOIN node o ON o.rid = a.object_rid ORDER BY a.id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The fixture, read without a `serde` derive (the store crate carries none; OWN-4).
struct Fixture {
    span: (u32, u32),
    supporters: Vec<Supporter>,
    before: String,
    after: String,
}
struct Supporter {
    object: String,
    layer: Layer,
    method: Method,
    raw_score: u32,
    lineage_id: String,
}

fn fixture() -> Fixture {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/reconcile/purge-redecide.json"
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let text = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap().to_owned();
    Fixture {
        span: (
            u32::try_from(v["span"]["offset"].as_u64().unwrap()).unwrap(),
            u32::try_from(v["span"]["len"].as_u64().unwrap()).unwrap(),
        ),
        supporters: v["supporters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|sup| Supporter {
                object: text(sup, "object"),
                layer: Layer::parse(&text(sup, "layer")).unwrap(),
                method: Method::parse(&text(sup, "method")).unwrap(),
                raw_score: u32::try_from(sup["raw_score"].as_u64().unwrap()).unwrap(),
                lineage_id: text(sup, "lineage_id"),
            })
            .collect(),
        before: text(&v["before"], "winner"),
        after: text(&v["after"], "winner"),
    }
}

/// Slice: purge of an object node removes the claims naming it (one `delete` `ChangeLog` row
/// each, their statuses cascaded) and the group that lost its winner is re-decided in the
/// same transaction: the fixture's second supporter becomes the winner.
#[test]
fn purge_of_an_object_removes_its_claims_and_redecides_the_group() {
    let fx = fixture();
    let s = Scratch::new("e22-purge", "redecide");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let chunk = NodeId::new();
    let objects: BTreeMap<String, NodeId> = fx
        .supporters
        .iter()
        .map(|sup| (sup.object.clone(), NodeId::new()))
        .collect();
    let mut ops = vec![upsert(&chunk, "Chunk", "hello vendor mention")];
    for (name, id) in &objects {
        ops.push(upsert(id, "Fact", &format!("entity {name}")));
    }
    let assertions = fx
        .supporters
        .iter()
        .map(|sup| BatchAssertion {
            subject: BatchSubject::Span {
                chunk: chunk.clone(),
                offset: fx.span.0,
                len: fx.span.1,
            },
            claim_class: ClaimClass::CanonicalId,
            predicate: None,
            object: Some(objects[&sup.object].clone()),
            value: json!(objects[&sup.object].as_str()),
            layer: sup.layer,
            method: sup.method,
            extractor_id: None,
            extractor_version: None,
            source_id: None,
            lineage_id: sup.lineage_id.clone(),
            raw_score: Fixed::new(sup.raw_score).unwrap(),
            rank: Rank::Normal,
            rank_reason: None,
            calibration_version: 1,
            supersedes: None,
        })
        .collect();
    let mut b = WriteBatch::new(WorkspaceId::default(), ops);
    b.assertions = assertions;
    store.apply_batch(b, &ctx()).unwrap();
    let conn = raw(&s);
    let winner_before = objects[&fx.before].to_string();
    let d = dispositions(&conn);
    assert_eq!(d.len(), 3);
    assert!(
        d.iter()
            .any(|(o, s)| o.as_deref() == Some(winner_before.as_str()) && s == "winner"),
        "{d:?}"
    );
    let changes_before = store.changes_read(0, 10_000).unwrap().len();
    // purge the winner's object
    store
        .forget(&objects[&fx.before], ForgetMode::Purge, &ctx())
        .unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM assertion"),
        2,
        "its claim is gone"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM assertion_status"),
        2,
        "its status cascaded"
    );
    let d = dispositions(&conn);
    let winner_after = objects[&fx.after].to_string();
    assert!(
        d.iter()
            .any(|(o, s)| o.as_deref() == Some(winner_after.as_str()) && s == "winner"),
        "the group was re-decided: {d:?}"
    );
    assert_eq!(d.iter().filter(|(_, s)| s == "dissent").count(), 1);
    let new_rows: Vec<(ChangeEntity, ChangeOp)> = store
        .changes_read(changes_before as u64, 10_000)
        .unwrap()
        .into_iter()
        .map(|c| (c.entity, c.op))
        .collect();
    assert_eq!(
        new_rows
            .iter()
            .filter(|r| **r == (ChangeEntity::Assertion, ChangeOp::Delete))
            .count(),
        1,
        "one delete row per purged claim: {new_rows:?}"
    );
    assert!(
        new_rows.contains(&(ChangeEntity::Assertion, ChangeOp::Status)),
        "the re-decided statuses are logged: {new_rows:?}"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
}

/// Slice: purge of a chunk removes its span claims and their statuses (one `delete` row each);
/// purge of an edge object removes the `edge` claims naming it; `foreign_key_check` empty.
#[test]
fn purge_of_a_chunk_removes_its_span_claims_and_of_an_object_its_edge_claims() {
    let s = Scratch::new("e22-purge", "chunk");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (chunk, vendor, other) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut b = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            upsert(&chunk, "Chunk", "a chunk about a vendor"),
            upsert(&vendor, "Fact", "the vendor"),
            upsert(&other, "Fact", "another node"),
        ],
    );
    let span = |value: &str, lineage: &str| BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: 0,
            len: 5,
        },
        claim_class: ClaimClass::Type,
        predicate: None,
        object: None,
        value: json!(value),
        layer: Layer::Gazetteer,
        method: Method::Exact,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: lineage.into(),
        raw_score: Fixed::new(9000).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    };
    let edge = |src: &NodeId, dst: &NodeId| BatchAssertion {
        subject: BatchSubject::Edge {
            src: src.clone(),
            rel: "RELATES_TO".into(),
            dst: dst.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer: Layer::Llm,
        method: Method::Llm,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: "llm-1".into(),
        raw_score: Fixed::new(8000).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    };
    b.assertions = vec![
        span("Vendor", "g1"),
        span("Team", "g2"),
        edge(&other, &vendor),
        edge(&vendor, &other),
    ];
    store.apply_batch(b, &ctx()).unwrap();
    let conn = raw(&s);
    assert_eq!(count(&conn, "SELECT count(*) FROM assertion"), 4);
    assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 2);
    let mark = store.changes_read(0, 10_000).unwrap().len();
    store.forget(&chunk, ForgetMode::Purge, &ctx()).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM assertion WHERE subject_kind = 'span'"
        ),
        0
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM assertion_status"), 2);
    let deletes = store
        .changes_read(mark as u64, 10_000)
        .unwrap()
        .iter()
        .filter(|c| c.entity == ChangeEntity::Assertion && c.op == ChangeOp::Delete)
        .count();
    assert_eq!(deletes, 2, "one delete row per span claim");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
    // the object of two edge claims (as dst of one, src of the other)
    store.forget(&vendor, ForgetMode::Purge, &ctx()).unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM assertion"),
        0,
        "edge claims naming it went"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 0);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
}

/// Slice (E2 interim review RE4-1): a forgotten node's claims stay out of the vote. After the
/// node is re-added (its id upserted again) and a machine claim joins the group, the human
/// claim the tombstone rejected is still `rejected` and retired (`deprecated`, reason
/// `forgotten`); the machine claim decides the group alone.
#[test]
fn a_forgotten_nodes_claims_do_not_vote_after_it_is_re_added() {
    let s = Scratch::new("e22-purge", "forgotten-votes");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = common::created(&store, "Fact", "a note");
    let b = common::created(&store, "Fact", "another note");
    let (a, b) = (NodeId::parse(&a.id).unwrap(), NodeId::parse(&b.id).unwrap());
    store
        .link(&a, &b, "RELATES_TO", None, None, &ctx())
        .unwrap();
    store.forget(&b, ForgetMode::Tombstone, &ctx()).unwrap();
    let conn = raw(&s);
    assert_eq!(
        dispositions(&conn),
        vec![(Some(b.to_string()), "rejected".to_owned())]
    );

    // Act: the node is re-added and a machine claim joins the human's group
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![upsert(&b, "Fact", "another note")],
    );
    batch.assertions = vec![BatchAssertion {
        subject: BatchSubject::Edge {
            src: a.clone(),
            rel: "RELATES_TO".into(),
            dst: b.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer: Layer::Gliner,
        method: Method::Gliner,
        extractor_id: Some("g1".into()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: "g1".into(),
        raw_score: Fixed::new(9000).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }];
    store.apply_batch(batch, &ctx()).unwrap();

    assert_eq!(
        dispositions(&conn),
        vec![
            (Some(b.to_string()), "rejected".to_owned()),
            (Some(b.to_string()), "winner".to_owned()),
        ],
        "the forgotten human claim does not vote; the machine claim wins alone"
    );
    let (rank, reason): (String, Option<String>) = conn
        .query_row(
            "SELECT rank, rank_reason FROM assertion WHERE layer = 'human'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (rank.as_str(), reason.as_deref()),
        ("deprecated", Some("forgotten"))
    );
    let layer: String = conn
        .query_row("SELECT layer FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(layer, "gliner");
}

/// Slice (E2 interim review RE4-1, the reactivation redirect): `Op::Reactivate` of an
/// ingest-owned Chunk brings its forgotten claims back (`rank` `normal`, reason cleared) and
/// re-decides their groups in the same transaction, so a forgotten claim votes again; a claim
/// the policy rescan retired (`policy:` reason) stays retired and `rejected`.
#[test]
fn a_forgotten_claim_votes_again_after_reactivation_a_policy_retired_one_does_not() {
    let s = Scratch::new("e22-purge", "reactivate-claims");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let (a, chunk) = (NodeId::new(), NodeId::new());
    let owned_chunk = || Op::UpsertNode {
        id: chunk.clone(),
        node: NewNode {
            kind: "Chunk".into(),
            text: "chunk text".into(),
            props: Some(json!({"owner": kaban_core::INGEST_OWNER})),
            ..Default::default()
        },
    };
    let claim = |layer, method, raw: u32, lineage: &str, value| BatchAssertion {
        subject: BatchSubject::Edge {
            src: a.clone(),
            rel: "RELATES_TO".into(),
            dst: chunk.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value,
        layer,
        method,
        extractor_id: Some(lineage.to_owned()),
        extractor_version: Some("1".into()),
        source_id: Some("doc-1".into()),
        lineage_id: lineage.to_owned(),
        raw_score: Fixed::new(raw).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    };
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![upsert(&a, "Fact", "a note"), owned_chunk()],
    );
    batch.assertions = vec![
        // llm 0.75 x 1.0 wins over gliner 0.6 x 0.9; the policy then retires it
        claim(
            Layer::Llm,
            Method::Llm,
            10_000,
            "l1",
            json!({"note": "secret hunter2"}),
        ),
        claim(Layer::Gliner, Method::Gliner, 9000, "g1", json!({})),
    ];
    store.apply_batch(batch, &ctx()).unwrap();
    // a judge that names no rule: the reason falls back to the policy version (D4)
    let report = store
        .rescan_assertions("v2", &mut |claim, _audit| {
            let text = claim.value.as_str();
            text.contains("hunter2")
                .then(|| json!({"note": "[REDACTED]"}))
        })
        .unwrap();
    assert_eq!(report.rejected, 1);
    let conn = raw(&s);
    let rows = |conn: &Connection| -> Vec<(String, String, Option<String>, String)> {
        conn.prepare(
            "SELECT a.lineage_id, a.rank, a.rank_reason, s.disposition FROM assertion a
             JOIN assertion_status s ON s.assertion_id = a.id ORDER BY a.lineage_id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    let row = |l: &str, rank: &str, reason: Option<&str>, d: &str| {
        (
            l.to_owned(),
            rank.to_owned(),
            reason.map(str::to_owned),
            d.to_owned(),
        )
    };
    assert_eq!(
        rows(&conn),
        vec![
            row("g1", "normal", None, "winner"),
            row("l1", "deprecated", Some("policy:v2"), "rejected"),
        ]
    );

    // the pipeline tombstones the chunk: every claim naming it is out
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::Tombstone {
                    id: chunk.clone(),
                    reason: "gone".into(),
                }],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        rows(&conn),
        vec![
            row("g1", "deprecated", Some("forgotten"), "rejected"),
            row("l1", "deprecated", Some("policy:v2"), "rejected"),
        ]
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 0);

    // Act: the chunk comes back
    let report = store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::Reactivate { id: chunk.clone() }],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(report.reactivated, 1, "{report:?}");
    assert_eq!(
        rows(&conn),
        vec![
            row("g1", "normal", None, "winner"),
            row("l1", "deprecated", Some("policy:v2"), "rejected"),
        ],
        "the forgotten claim votes again; the policy-retired one stays retired"
    );
    let layer: String = conn
        .query_row("SELECT layer FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(layer, "gliner");
}

/// Slice: `forget` (tombstone) of a node leaves its claim rows and sets every status to
/// `rejected`, one `status` `ChangeLog` row each; its edges go as today.
#[test]
fn forget_of_a_node_leaves_its_claims_and_sets_rejected() {
    let s = Scratch::new("e22-purge", "forget");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let a = common::created(&store, "Fact", "a note");
    let b = common::created(&store, "Fact", "another note");
    store
        .link(
            &NodeId::parse(&a.id).unwrap(),
            &NodeId::parse(&b.id).unwrap(),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let conn = raw(&s);
    assert_eq!(
        dispositions(&conn),
        vec![(Some(b.id.clone()), "winner".to_owned())]
    );
    let mark = store.changes_read(0, 10_000).unwrap().len();
    store
        .forget(
            &NodeId::parse(&b.id).unwrap(),
            ForgetMode::Tombstone,
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM assertion"),
        1,
        "the claim row stays"
    );
    assert_eq!(
        dispositions(&conn),
        vec![(Some(b.id.clone()), "rejected".to_owned())]
    );
    let statuses = store
        .changes_read(mark as u64, 10_000)
        .unwrap()
        .iter()
        .filter(|c| c.entity == ChangeEntity::Assertion && c.op == ChangeOp::Status)
        .count();
    assert_eq!(statuses, 1);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
}

/// Every new read after the purge commits: no lexical or fuzzy hit, and the row is a ghost.
fn assert_revoked(store: &SqliteStore, s: &Scratch, id: &str, word: &str) {
    for fuzzy in [false, true] {
        let hits = store
            .recall(&RecallQuery {
                query: word.into(),
                hops: 0,
                fuzzy,
                ..Default::default()
            })
            .unwrap();
        assert!(hits.iter().all(|hit| hit.node.id != id), "{fuzzy}");
    }
    let fresh = raw(s);
    let (lifecycle, text): (String, String) = fresh
        .query_row("SELECT lifecycle, text FROM node WHERE id=?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((lifecycle.as_str(), text.as_str()), ("purged", ""));
}

/// SS-03: a reader still on the WAL blocks the TRUNCATE checkpoints. The purge is revoked at
/// once, the cleanup is reported incomplete (`busy`, not an error), and the explicit optimize
/// completes it once the reader is gone. Other modes neither revoke nor clean.
#[test]
fn a_wal_reader_leaves_cleanup_incomplete_while_the_purge_is_revoked() {
    let s = Scratch::new("ss03-purge", "wal-reader");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let victim = common::created(&store, "Fact", "PurgeCanaryQuokka private");
    let other = common::created(&store, "Fact", "kept note");
    let reader = raw(&s);
    reader.execute_batch("BEGIN").unwrap();
    assert_eq!(
        count(
            &reader,
            "SELECT count(*) FROM node WHERE lifecycle='active'"
        ),
        2
    );
    let outcome = store
        .forget(
            &NodeId::parse(&victim.id).unwrap(),
            ForgetMode::Purge,
            &ctx(),
        )
        .unwrap();
    assert!(outcome.revoked);
    assert!(!outcome.physically_cleaned);
    assert!(
        outcome.cleanup.as_deref().is_some_and(
            |c| c.starts_with("database/WAL: busy; free-page residue until VACUUM; index:")
        ),
        "{:?}",
        outcome.cleanup
    );
    assert_revoked(&store, &s, &victim.id, "PurgeCanaryQuokka");
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    assert_eq!(store.optimize_fts().unwrap(), MaintenanceOutcome::Ok);
    let wal = std::fs::metadata(format!("{}-wal", s.db().display())).map_or(0, |m| m.len());
    assert_eq!(wal, 0, "the retried cleanup truncates the WAL");
    let kept = store
        .forget(
            &NodeId::parse(&other.id).unwrap(),
            ForgetMode::Tombstone,
            &ctx(),
        )
        .unwrap();
    assert!(!kept.revoked && !kept.physically_cleaned && kept.cleanup.is_none());
}

/// Occurrences of `needle` in the file at `path` (0 for a missing file).
fn occurrences(path: &std::path::Path, needle: &[u8]) -> usize {
    std::fs::read(path).map_or(0, |bytes| {
        bytes.windows(needle.len()).filter(|w| *w == needle).count()
    })
}

/// SS-03 (review a-1): the post-commit hygiene merges the indexes and truncates the WAL but
/// reclaims no freed page, and under `secure_delete=FAST` a freed overflow page keeps its
/// bytes: a purged body whose stored frame spans overflow pages is still in the checkpointed
/// file, so the purge reports `physically_cleaned: false` with the free-page residue as its
/// detail rather than a success. The VACUUM path (`kaban doctor --optimize --vacuum`: the
/// vacuum, then the optimize's checkpoint) removes the bytes.
#[test]
fn a_purge_reports_the_free_page_residue_until_a_vacuum_removes_the_bytes() {
    // Arrange: a body whose encoded frame is long enough for overflow pages (hex of a
    // pseudo-random sequence: the codec halves it at best); the residue searched for is a
    // slice of the frame as stored, never the plain text
    const WORD: &str = "PurgeCanaryQuokka7731";
    let s = Scratch::new("ss03-purge", "canary-residue");
    let wal = format!("{}-wal", s.db().display());
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let word = WORD;
    let mut body = format!("{word} ");
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    while body.len() < 64 << 10 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        write!(body, "{state:016x}").unwrap();
    }
    let victim = common::created(&store, "Fact", &body);
    let stored: Vec<u8> = raw(&s)
        .query_row("SELECT text FROM node WHERE id = ?1", [&victim.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(stored.len() > 16 << 10, "{} stored bytes", stored.len());
    let canary = &stored[stored.len() / 2..stored.len() / 2 + 64];

    // Act: the purge, hygiene included
    let outcome = store
        .forget(
            &NodeId::parse(&victim.id).unwrap(),
            ForgetMode::Purge,
            &ctx(),
        )
        .unwrap();

    // Assert: revoked, not cleaned, the residue named; the bytes are still in the file
    assert!(outcome.revoked);
    assert!(!outcome.physically_cleaned);
    assert!(
        outcome
            .cleanup
            .as_deref()
            .is_some_and(|c| c.starts_with("database/WAL: free-page residue until VACUUM; index:")),
        "{:?}",
        outcome.cleanup
    );
    assert_revoked(&store, &s, &victim.id, word);
    assert_eq!(occurrences(std::path::Path::new(&wal), canary), 0);
    assert!(
        occurrences(&s.db(), canary) > 0,
        "the freed pages keep the body until a VACUUM"
    );

    // Act: the VACUUM path, then the checkpoint that folds it into the file
    assert_eq!(store.vacuum().unwrap(), MaintenanceOutcome::Ok);
    assert_eq!(store.optimize_fts().unwrap(), MaintenanceOutcome::Ok);

    // Assert: not one byte of the body in the file or its WAL
    assert_eq!(occurrences(&s.db(), canary), 0);
    assert_eq!(occurrences(std::path::Path::new(&wal), canary), 0);
}
