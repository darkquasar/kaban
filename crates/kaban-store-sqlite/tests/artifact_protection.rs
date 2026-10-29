//! Durable human authority and explicit confidence updates on the public store surface.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::Scratch;
use kaban_core::ActorId;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, Fixed, ForgetMode, Governance,
    GovernanceStore, IngestStore, Layer, MemoryStore, Method, NewEvent, NewNode, NodeId, Op, Rank,
    SignerKind, TraverseQuery, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

fn human_ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli)
}

#[test]
fn explicit_human_link_consolidates_only_its_legacy_heads() {
    let scratch = Scratch::new("artifact-protection", "legacy-heads");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Fact", "Source").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Fact", "Target").id).unwrap();
    let other = NodeId::parse(&common::created(&store, "Fact", "Other").id).unwrap();
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    for (dst, actor, value, confidence, rank, reason) in [
        (&target, "human:cli", "A", 2500, Rank::Preferred, None),
        (&target, "human:cli", "B", 7500, Rank::Preferred, None),
        (&target, "human:other", "other", 5000, Rank::Normal, None),
        (&other, "human:cli", "group", 5000, Rank::Preferred, None),
        (&target, "human:cli", "foreign", 5000, Rank::Normal, None),
        (
            &target,
            "human:cli",
            "retired",
            5000,
            Rank::Deprecated,
            Some("keep reason"),
        ),
    ] {
        let mut claim = machine_claim(&source, dst);
        claim.layer = Layer::Human;
        claim.method = Method::Link;
        claim.extractor_version = None;
        claim.source_id = None;
        claim.lineage_id = actor.into();
        claim.value = json!({"label": value});
        claim.raw_score = Fixed(confidence);
        claim.rank = rank;
        claim.rank_reason = reason.map(str::to_owned);
        batch.assertions.push(claim);
    }
    store.apply_batch(batch, &human_ctx()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    // Adversarial at-rest tenant fixture: its matching triple must not widen the repair.
    conn.execute(
        "UPDATE assertion SET workspace_id='foreign' WHERE value LIKE '%foreign%'",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM assertion_status WHERE assertion_id IN (SELECT id FROM assertion WHERE workspace_id='foreign')", []).unwrap();
    let untouched: Vec<(i64, String, Option<String>)> = conn.prepare("SELECT id,rank,rank_reason FROM assertion WHERE workspace_id='foreign' OR lineage_id='human:other' OR value LIKE '%group%' OR rank='deprecated' ORDER BY id").unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    let linked = store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.25),
            Some(json!({"label":"A"})),
            &human_ctx(),
        )
        .unwrap();
    assert_eq!(linked.confidence, Some(0.25));
    let active: i64 = conn.query_row("SELECT count(*) FROM assertion a WHERE a.workspace_id='default' AND a.lineage_id LIKE 'human:cli%' AND a.value NOT LIKE '%group%' AND a.rank<>'deprecated' AND NOT EXISTS(SELECT 1 FROM assertion b WHERE b.supersedes=a.id)", [], |r|r.get(0)).unwrap();
    assert_eq!(
        active, 1,
        "explicit link must consolidate legacy same-actor heads into one standing vote"
    );
    let after: Vec<(i64, String, Option<String>)> = conn.prepare("SELECT id,rank,rank_reason FROM assertion WHERE workspace_id='foreign' OR lineage_id='human:other' OR value LIKE '%group%' OR rank_reason='keep reason' ORDER BY id").unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(untouched, after);
    store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.75),
            Some(json!({"label":"B"})),
            &human_ctx(),
        )
        .unwrap();
    let before: i64 = conn
        .query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))
        .unwrap();
    store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.75),
            Some(json!({"label":"B"})),
            &human_ctx(),
        )
        .unwrap();
    assert_eq!(
        before,
        conn.query_row("SELECT max(seq) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
}

#[test]
fn human_relink_updates_confidence_and_identical_relink_is_idempotent() {
    let scratch = Scratch::new("artifact-protection", "human-confidence");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Fact", "Source").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Fact", "Target").id).unwrap();
    assert_eq!(
        store
            .link(
                &source,
                &target,
                "RELATES_TO",
                Some(0.25),
                None,
                &human_ctx()
            )
            .unwrap()
            .confidence,
        Some(0.25)
    );
    let second = store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.75),
            None,
            &human_ctx(),
        )
        .unwrap();
    assert_eq!(
        second.confidence,
        Some(0.75),
        "explicit human relink must apply its new confidence"
    );
    let conn = rusqlite::Connection::open_with_flags(
        scratch.db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let before: i64 = conn
        .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
        .unwrap();
    let repeated = store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.75),
            None,
            &human_ctx(),
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(repeated).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    let after: i64 = conn
        .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    let mut machine = WriteBatch::new(WorkspaceId::default(), vec![]);
    machine.assertions.push(machine_claim(&source, &target));
    store.apply_batch(machine, &common::ctx()).unwrap();
    let machine_joined = store
        .traverse(&TraverseQuery {
            start_ids: vec![source.clone()],
            rels: vec!["RELATES_TO".into()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(&machine_joined.edges[0]).unwrap(),
        serde_json::to_value(&second).unwrap(),
        "machine relink must not overwrite human confidence or edge bytes"
    );
    let returned = store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.25),
            None,
            &human_ctx(),
        )
        .unwrap();
    assert_eq!(
        returned.confidence,
        Some(0.25),
        "A to B to A is a fresh action, not a replay of superseded A"
    );
    let active: i64 = conn.query_row("SELECT count(*) FROM assertion a WHERE a.layer='human' AND a.rank<>'deprecated' AND NOT EXISTS(SELECT 1 FROM assertion b WHERE b.supersedes=a.id)", [], |r| r.get(0)).unwrap();
    assert_eq!(
        active, 1,
        "one actor's historical actions must not stack votes"
    );
    let unbound: i64 = conn.query_row("SELECT count(*) FROM assertion a WHERE a.lineage_id LIKE 'human:cli:%' AND NOT EXISTS(SELECT 1 FROM changes c WHERE c.entity='assertion' AND c.op='insert' AND c.id=CAST(a.id AS TEXT) AND c.workspace_id=a.workspace_id AND CAST(c.seq AS TEXT)=substr(a.lineage_id,11))", [], |r| r.get(0)).unwrap();
    assert_eq!(
        unbound, 0,
        "event lineage identifies the actual immutable assertion insert ChangeLog row"
    );
}

fn machine_claim(source: &NodeId, target: &NodeId) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: source.clone(),
            dst: target.clone(),
            rel: "RELATES_TO".into(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({}),
        layer: Layer::Gazetteer,
        method: Method::Exact,
        extractor_id: None,
        extractor_version: Some("1".into()),
        source_id: Some("fixture".into()),
        lineage_id: "gazetteer:fixture".into(),
        raw_score: Fixed(9000),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    }
}

#[test]
fn human_authority_survives_missing_status_and_edge_projection() {
    let scratch = Scratch::new("artifact-protection", "missing-projection");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Fact", "Source").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Fact", "Target").id).unwrap();
    store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.25),
            None,
            &human_ctx(),
        )
        .unwrap();
    // Reset disposable projections, not immutable authority rows.
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute("DELETE FROM assertion_status", []).unwrap();
    conn.execute("DELETE FROM edge", []).unwrap();
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions.push(machine_claim(&source, &target));
    store.apply_batch(batch, &common::ctx()).unwrap();
    let result = store
        .traverse(&TraverseQuery {
            start_ids: vec![source],
            rels: vec!["RELATES_TO".into()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        result.edges.first().and_then(|edge| edge.confidence),
        Some(0.25),
        "immutable human authority must be restored before a machine decision after projection loss"
    );
}

#[test]
fn missing_status_recovery_does_not_resurrect_human_deprecated_authority() {
    let scratch = Scratch::new("artifact-protection", "deprecated-projection");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Fact", "Source").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Fact", "Target").id).unwrap();
    store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.25),
            None,
            &human_ctx(),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    let key: String = conn
        .query_row("SELECT key FROM assertion WHERE layer='human'", [], |r| {
            r.get(0)
        })
        .unwrap();
    store
        .deprecate_assertion(&key, "incorrect link", &human_ctx())
        .unwrap();
    conn.execute("DELETE FROM assertion_status", []).unwrap();
    conn.execute("DELETE FROM edge", []).unwrap();
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions.push(machine_claim(&source, &target));
    store.apply_batch(batch, &common::ctx()).unwrap();
    let confidence: f64 = conn
        .query_row("SELECT confidence FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(confidence, 0.81);
    let winning_humans: i64 = conn.query_row("SELECT count(*) FROM assertion a JOIN assertion_status s ON s.assertion_id=a.id WHERE a.layer='human' AND s.disposition='winner'",[],|r|r.get(0)).unwrap();
    assert_eq!(winning_humans, 0);
}

#[test]
fn missing_status_recovery_never_restores_forgotten_human_authority() {
    let scratch = Scratch::new("artifact-protection", "forgotten-projection");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Fact", "Source").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Fact", "Target").id).unwrap();
    store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.25),
            None,
            &human_ctx(),
        )
        .unwrap();
    store
        .forget(&target, ForgetMode::Tombstone, &human_ctx())
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute("DELETE FROM assertion_status", []).unwrap();
    conn.execute("DELETE FROM edge", []).unwrap();
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions.push(machine_claim(&source, &target));
    store.apply_batch(batch, &common::ctx()).unwrap();
    let resurrected:i64=conn.query_row("SELECT count(*) FROM assertion a JOIN assertion_status s ON s.assertion_id=a.id WHERE a.layer='human' AND s.disposition='winner'",[],|r|r.get(0)).unwrap();
    assert_eq!(resurrected, 0);
    assert!(store.get(&target, false).unwrap().is_none());
}

#[test]
fn identical_human_retry_recovers_projection_without_recording_a_new_action() {
    let scratch = Scratch::new("artifact-protection", "human-retry-recovery");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Fact", "Source").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Fact", "Target").id).unwrap();
    store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.25),
            None,
            &human_ctx(),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute("DELETE FROM assertion_status", []).unwrap();
    conn.execute("DELETE FROM edge", []).unwrap();
    let result = store
        .link(
            &source,
            &target,
            "RELATES_TO",
            Some(0.25),
            None,
            &human_ctx(),
        )
        .unwrap();
    assert_eq!(result.confidence, Some(0.25));
    let actions: i64 = conn
        .query_row(
            "SELECT count(*) FROM assertion WHERE layer='human'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actions, 1);
}

#[test]
fn uniquely_historical_accepted_record_edge_is_restored_before_new_machine_claim() {
    let scratch = Scratch::new("artifact-protection", "accepted-projection");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Chunk", "Evidence").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Decision", "Decision").id).unwrap();
    store
        .transition(
            &target,
            Governance::Accepted,
            NewEvent {
                reason: "reviewed".into(),
                signer_kind: SignerKind::HumanCli,
                ..Default::default()
            },
            &human_ctx(),
        )
        .unwrap();
    let mut first = machine_claim(&source, &target);
    first.subject = BatchSubject::Edge {
        src: source.clone(),
        dst: target.clone(),
        rel: "EVIDENCE_FOR".into(),
    };
    first.layer = Layer::Gliner;
    first.method = Method::Gliner;
    first.lineage_id = "historical".into();
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions.push(first);
    store.apply_batch(batch, &common::ctx()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    let initial: f64 = conn
        .query_row("SELECT confidence FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        initial, 0.54,
        "initial machine edge remains admissible even after acceptance"
    );
    conn.execute("DELETE FROM assertion_status", []).unwrap();
    conn.execute("DELETE FROM edge", []).unwrap();
    let mut next = machine_claim(&source, &target);
    next.subject = BatchSubject::Edge {
        src: source,
        dst: target,
        rel: "EVIDENCE_FOR".into(),
    };
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions.push(next.clone());
    store.apply_batch(batch, &common::ctx()).unwrap();
    let recovered: f64 = conn
        .query_row("SELECT confidence FROM edge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        recovered, initial,
        "historical accepted-record authority must be reconstructed before the incoming machine participates"
    );
    conn.execute("DELETE FROM assertion_status", []).unwrap();
    conn.execute("DELETE FROM edge", []).unwrap();
    let before: i64 = conn
        .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
        .unwrap();
    next.extractor_version = Some("2".into());
    let mut ambiguous = WriteBatch::new(WorkspaceId::default(), vec![]);
    ambiguous.assertions.push(next);
    assert!(
        store.apply_batch(ambiguous, &common::ctx()).is_err(),
        "lost ambiguous accepted projection must refuse to invent an old winner"
    );
    let after: i64 = conn
        .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after, "ambiguous recovery refusal is zero-write");
    let statuses: i64 = conn
        .query_row("SELECT count(*) FROM assertion_status", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        statuses, 0,
        "preflight status recovery rolls back with the refused batch"
    );
}

#[test]
fn projection_recovery_never_recreates_another_workspaces_authority_statuses() {
    let scratch = Scratch::new("artifact-protection", "foreign-recovery");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::parse(&common::created(&store, "Fact", "Source").id).unwrap();
    let target = NodeId::parse(&common::created(&store, "Fact", "Target").id).unwrap();
    let foreign = WorkspaceId::new("foreign");
    let foreign_ctx = WriteCtx::new("v1", foreign.clone(), Actor::HumanCli);
    let (other_source, other_target) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            WriteBatch::new(
                foreign,
                vec![
                    Op::UpsertNode {
                        id: other_source.clone(),
                        node: NewNode {
                            kind: "Fact".into(),
                            text: "Other source".into(),
                            ..Default::default()
                        },
                    },
                    Op::UpsertNode {
                        id: other_target.clone(),
                        node: NewNode {
                            kind: "Fact".into(),
                            text: "Other target".into(),
                            ..Default::default()
                        },
                    },
                ],
            ),
            &foreign_ctx,
        )
        .unwrap();
    store
        .link(
            &other_source,
            &other_target,
            "RELATES_TO",
            Some(0.25),
            None,
            &foreign_ctx,
        )
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute("DELETE FROM assertion_status", []).unwrap();
    conn.execute("DELETE FROM edge", []).unwrap();
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions.push(machine_claim(&source, &target));
    store.apply_batch(batch, &common::ctx()).unwrap();
    let restored:i64=conn.query_row("SELECT count(*) FROM assertion_status s JOIN assertion a ON a.id=s.assertion_id WHERE a.workspace_id='foreign'",[],|r|r.get(0)).unwrap();
    assert_eq!(restored, 0);
}
