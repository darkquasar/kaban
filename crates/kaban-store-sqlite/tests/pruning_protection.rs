//! Durable automatic-pruning protection workflows.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::Scratch;
use kaban_core::{
    Actor, EdgeDiff, GovernanceStore, IngestStore, MemoryStore, NewNode, NodeId, Op, Owner,
    PruningTarget, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

#[test]
fn protected_nonhuman_node_survives_omission_then_clear_restores_pruning() {
    let scratch = Scratch::new("protection", "omission");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let id = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(
                ctx.workspace_id.clone(),
                vec![Op::UpsertNode {
                    id: id.clone(),
                    node: NewNode {
                        kind: "Fact".into(),
                        text: "invented".into(),
                        props: Some(json!({"pruning_protection":true})),
                        ..Default::default()
                    },
                }],
            ),
            &ctx,
        )
        .unwrap();
    let target = PruningTarget::Node(id.clone());
    let before = serde_json::to_value(store.get(&id, true).unwrap()).unwrap();
    assert!(
        store
            .pruning_protection_set(&target, &ctx)
            .unwrap()
            .explicit
    );
    assert!(
        store
            .pruning_protection_set(&target, &ctx)
            .unwrap()
            .explicit
    );
    assert_eq!(
        serde_json::to_value(store.get(&id, true).unwrap()).unwrap(),
        before,
        "protection must not fabricate authorship, rank, governance or node revisions"
    );
    store
        .apply_batch(
            WriteBatch::new(
                ctx.workspace_id.clone(),
                vec![Op::Tombstone {
                    id: id.clone(),
                    reason: "producer omission".into(),
                }],
            ),
            &ctx,
        )
        .unwrap();
    assert!(
        store.get(&id, false).unwrap().is_some(),
        "explicitly protected nonhuman node must survive producer omission"
    );
    store.pruning_protection_clear(&target, &ctx).unwrap();
    store
        .apply_batch(
            WriteBatch::new(
                ctx.workspace_id.clone(),
                vec![Op::Tombstone {
                    id: id.clone(),
                    reason: "producer omission".into(),
                }],
            ),
            &ctx,
        )
        .unwrap();
    assert!(store.get(&id, false).unwrap().is_none());
}

#[test]
fn protected_relationship_survives_structural_omission() {
    let store = SqliteStore::open_memory().unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let (a, b) = (NodeId::new(), NodeId::new());
    let make = |id: &NodeId| Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: id.to_string(),
            ..Default::default()
        },
    };
    let mut batch = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![
            make(&a),
            make(&b),
            Op::UpsertEdge {
                src: a.clone(),
                dst: b.clone(),
                rel: "RELATES_TO".into(),
                props: Some(json!({"owner":"kaban-ingest"})),
            },
        ],
    );
    batch.edge_diff = EdgeDiff::None;
    store.apply_batch(batch, &ctx).unwrap();
    let target = PruningTarget::Relationship {
        src: a.clone(),
        dst: b.clone(),
        rel: "RELATES_TO".into(),
    };
    store.pruning_protection_set(&target, &ctx).unwrap();
    let mut omitted = WriteBatch::new(ctx.workspace_id.clone(), vec![make(&a)]);
    omitted.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    store.apply_batch(omitted.clone(), &ctx).unwrap();
    assert!(
        store.pruning_protection_get(&target, &ctx).is_ok(),
        "protected relationship must survive producer omission"
    );
    store.pruning_protection_clear(&target, &ctx).unwrap();
    store.apply_batch(omitted, &ctx).unwrap();
    assert!(store.pruning_protection_get(&target, &ctx).is_err());
}

#[test]
fn protected_gazetteer_claim_survives_empty_complete_set() {
    use kaban_core::{
        BatchAssertion, BatchSubject, ClaimClass, Fixed, GazetteerClaimSet, Layer, Method, Rank,
        SourceRev, TraverseQuery,
    };
    let scratch = Scratch::new("protection", "gazetteer");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let (doc, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let make = |id: &NodeId, kind: &str, props| Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: kind.into(),
            text: id.to_string(),
            props: Some(props),
            ..Default::default()
        },
    };
    let mut first = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![
            make(
                &doc,
                "Document",
                json!({"source_id":"fixture","owner":"kaban-ingest","chunk_ids":[chunk]}),
            ),
            make(&chunk, "Chunk", json!({"owner":"kaban-ingest"})),
            make(&target, "Fact", json!({})),
        ],
    );
    first.edge_diff = EdgeDiff::None;
    first.assertions.push(BatchAssertion {
        subject: BatchSubject::Edge {
            src: chunk.clone(),
            dst: target.clone(),
            rel: "MENTIONS".into(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({"method":"exact","offset":0,"len":1,"score":9000}),
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
    });
    let sibling = NodeId::new();
    first.ops.push(make(&sibling, "Fact", json!({})));
    let mut sibling_claim = first.assertions[0].clone();
    sibling_claim.subject = BatchSubject::Edge {
        src: chunk.clone(),
        dst: sibling,
        rel: "MENTIONS".into(),
    };
    first.assertions.push(sibling_claim);
    store.apply_batch(first, &ctx).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let key: String = conn
        .query_row(
            "SELECT key FROM assertion WHERE object_rid=(SELECT rid FROM node WHERE id=?1)",
            [target.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    let protection = PruningTarget::Claim(kaban_core::AssertionKey(key));
    store.pruning_protection_set(&protection, &ctx).unwrap();
    let mut omitted = WriteBatch::new(ctx.workspace_id.clone(), vec![]);
    omitted.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    omitted.gazetteer_claim_sets.push(GazetteerClaimSet {
        document: doc.clone(),
        source_id: "fixture".into(),
        source_rev: Some(SourceRev::from(&store.get(&doc, false).unwrap().unwrap())),
        expected_generation: 0,
        config_hash: "off".into(),
    });
    store.apply_batch(omitted.clone(), &ctx).unwrap();
    let graph = store
        .traverse(&TraverseQuery {
            start_ids: vec![chunk.clone()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap();
    assert!(
        graph.edges.iter().any(|e| e.rel == "MENTIONS"),
        "protected Gazetteer claim must survive producer omission"
    );
    assert_eq!(graph.edges.len(), 1, "unprotected sibling is pruned");
    store.pruning_protection_clear(&protection, &ctx).unwrap();
    omitted.gazetteer_claim_sets[0].expected_generation =
        store.gazetteer_generation(&doc, &ctx.workspace_id).unwrap();
    store.apply_batch(omitted, &ctx).unwrap();
    assert!(
        store
            .traverse(&TraverseQuery {
                start_ids: vec![chunk],
                max_depth: 1,
                ..Default::default()
            })
            .unwrap()
            .edges
            .is_empty()
    );
}

#[test]
fn protected_kind_candidate_survives_bulk_prune() {
    let scratch = Scratch::new("protection", "kind-candidate");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let node = store
        .remember(
            NewNode {
                kind: "Vendor".into(),
                text: "invented vendor".into(),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap();
    assert!(matches!(node, kaban_core::RememberOutcome::Created(_)));
    let created = match node {
        kaban_core::RememberOutcome::Created(n)
        | kaban_core::RememberOutcome::Existing { node: n, .. } => n,
    };
    let id = NodeId::parse(&created.id).unwrap();
    store
        .pruning_protection_set(&PruningTarget::Node(id.clone()), &ctx)
        .unwrap();
    let dry = store
        .prune_kinds(
            &ctx.workspace_id,
            &kaban_core::config::PromoteWhen {
                min_count: 5,
                ..Default::default()
            },
            std::time::Duration::ZERO,
            true,
        )
        .unwrap();
    let before = store.kind_stats(&ctx.workspace_id).unwrap();
    store
        .prune_kinds(
            &ctx.workspace_id,
            &kaban_core::config::PromoteWhen {
                min_count: 5,
                ..Default::default()
            },
            std::time::Duration::ZERO,
            false,
        )
        .unwrap();
    assert!(
        store.get(&id, false).unwrap().is_some(),
        "protected kind candidate must survive bulk ontology pruning"
    );
    assert_eq!(store.kind_stats(&ctx.workspace_id).unwrap(), before);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let candidates: i64 = conn
        .query_row("SELECT count(*) FROM node_candidate", [], |r| r.get(0))
        .unwrap();
    assert_eq!(candidates, 1);
    store
        .pruning_protection_clear(&PruningTarget::Node(id.clone()), &ctx)
        .unwrap();
    let wet = store
        .prune_kinds(
            &ctx.workspace_id,
            &kaban_core::config::PromoteWhen {
                min_count: 5,
                ..Default::default()
            },
            std::time::Duration::ZERO,
            false,
        )
        .unwrap();
    assert_eq!(dry.kinds, wet.kinds);
    assert_eq!(wet.nodes_tombstoned, 1);
    assert_eq!(
        store.kind_stats(&ctx.workspace_id).unwrap(),
        [] as [kaban_core::KindStat; 0]
    );
}

#[test]
fn protection_is_bounded_tenant_bound_and_persistent_but_explicit_forget_wins() {
    use kaban_core::ForgetMode;
    let scratch = Scratch::new("protection", "bounded");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    let make = |id: &NodeId, props| Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: "Fact".into(),
            text: id.to_string(),
            props: Some(props),
            ..Default::default()
        },
    };
    let edge = |src: &NodeId, dst: &NodeId| Op::UpsertEdge {
        src: src.clone(),
        dst: dst.clone(),
        rel: "RELATES_TO".into(),
        props: Some(json!({"owner":"kaban-ingest"})),
    };
    let mut batch = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![
            make(&a, json!({})),
            make(&b, json!({})),
            make(&c, json!({})),
            edge(&a, &b),
            edge(&b, &c),
        ],
    );
    batch.edge_diff = EdgeDiff::None;
    store.apply_batch(batch, &ctx).unwrap();
    let target = PruningTarget::Node(a.clone());
    store.pruning_protection_set(&target, &ctx).unwrap();
    let state = store
        .pruning_protection_get(&PruningTarget::Node(b.clone()), &ctx)
        .unwrap();
    assert!(!state.explicit && state.effective);
    assert!(
        !store
            .pruning_protection_get(&PruningTarget::Node(c.clone()), &ctx)
            .unwrap()
            .effective,
        "required endpoint must not start a transitive protection flood"
    );
    let foreign = WriteCtx::new("v1", WorkspaceId::new("foreign"), Actor::HumanCli);
    for op in [None, Some(true), Some(false)] {
        let result = match op {
            None => store.pruning_protection_get(&target, &foreign),
            Some(true) => store.pruning_protection_set(&target, &foreign),
            _ => store.pruning_protection_clear(&target, &foreign),
        };
        assert!(matches!(result, Err(kaban_core::StoreError::NotFound(_))));
    }
    let mut replay = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![make(&a, json!({"replacement":true}))],
    );
    replay.edge_diff = EdgeDiff::None;
    store.apply_batch(replay.clone(), &ctx).unwrap();
    store.apply_batch(replay, &ctx).unwrap();
    drop(store);
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    assert!(
        store
            .pruning_protection_get(&target, &ctx)
            .unwrap()
            .explicit
    );
    store.forget(&a, ForgetMode::Tombstone, &ctx).unwrap();
    assert!(store.get(&a, false).unwrap().is_none());
    assert!(
        store
            .pruning_protection_get(&target, &ctx)
            .unwrap()
            .explicit,
        "nonpurge preserves the mark"
    );
    store.forget(&a, ForgetMode::Purge, &ctx).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let marks: i64 = conn
        .query_row("SELECT count(*) FROM pruning_protection", [], |r| r.get(0))
        .unwrap();
    assert_eq!(marks, 0, "explicit purge removes dependent marks");
}

#[test]
fn same_database_import_preserves_marks_and_new_targets_default_false() {
    use kaban_core::{Governance, ImportedRecord, Lifecycle, SyncStore};
    let store = SqliteStore::open_memory().unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let id = NodeId::new();
    let mut record = ImportedRecord {
        id: id.clone(),
        kind: "Fact".into(),
        text: "invented imported fact".into(),
        summary: None,
        tags: vec![],
        confidence: None,
        importance: None,
        lifecycle: Lifecycle::Active,
        governance: Governance::None,
        path_scope: vec![],
        props: json!({}),
        rev: 1,
        created_at: "2026-10-26T10:41:44Z".into(),
        updated_at: "2026-10-26T10:41:44Z".into(),
        valid_from: None,
        valid_to: None,
        accepted_commit: None,
        agent_id: None,
        session_id: None,
        repo: None,
        commit_sha: None,
        dup_key: None,
        event: None,
    };
    store.import_record(&record, &ctx).unwrap();
    let target = PruningTarget::Node(id);
    assert!(
        !store
            .pruning_protection_get(&target, &ctx)
            .unwrap()
            .explicit
    );
    store.pruning_protection_set(&target, &ctx).unwrap();
    record.rev = 2;
    record.props = json!({"replacement":true});
    store.import_record(&record, &ctx).unwrap();
    store.import_record(&record, &ctx).unwrap();
    assert!(
        store
            .pruning_protection_get(&target, &ctx)
            .unwrap()
            .explicit
    );
    record.id = NodeId::new();
    record.text = "new imported fact".into();
    store.import_record(&record, &ctx).unwrap();
    assert!(
        !store
            .pruning_protection_get(&PruningTarget::Node(record.id), &ctx)
            .unwrap()
            .effective
    );
}

#[test]
fn protected_span_claim_retains_its_chunk_and_object_without_copied_marks() {
    use kaban_core::{BatchAssertion, BatchSubject, ClaimClass, Fixed, Layer, Method, Rank};
    let scratch = Scratch::new("protection", "span");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let (chunk, object) = (NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![
            Op::UpsertNode {
                id: chunk.clone(),
                node: NewNode {
                    kind: "Chunk".into(),
                    text: "Mention".into(),
                    ..Default::default()
                },
            },
            Op::UpsertNode {
                id: object.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: "invented span target".into(),
                    ..Default::default()
                },
            },
        ],
    );
    batch.edge_diff = EdgeDiff::None;
    batch.assertions.push(BatchAssertion {
        subject: BatchSubject::Span {
            chunk: chunk.clone(),
            offset: 0,
            len: 7,
        },
        claim_class: ClaimClass::CanonicalId,
        predicate: None,
        object: Some(object.clone()),
        value: json!(object.as_str()),
        layer: Layer::Gazetteer,
        method: Method::Id,
        extractor_id: Some("invented-span".into()),
        extractor_version: Some("1".into()),
        source_id: Some("source".into()),
        lineage_id: "invented-span".into(),
        raw_score: Fixed::ONE,
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    });
    store.apply_batch(batch, &ctx).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let key: String = conn
        .query_row("SELECT key FROM assertion", [], |r| r.get(0))
        .unwrap();
    let target = PruningTarget::Claim(kaban_core::AssertionKey(key));
    store.pruning_protection_set(&target, &ctx).unwrap();
    for id in [&chunk, &object] {
        let state = store
            .pruning_protection_get(&PruningTarget::Node(id.clone()), &ctx)
            .unwrap();
        assert!(!state.explicit && state.effective);
    }
    let omitted = WriteBatch::new(
        ctx.workspace_id.clone(),
        vec![
            Op::Tombstone {
                id: chunk.clone(),
                reason: "omitted".into(),
            },
            Op::Tombstone {
                id: object.clone(),
                reason: "omitted".into(),
            },
        ],
    );
    let report = store.apply_batch(omitted.clone(), &ctx).unwrap();
    assert_eq!(report.tombstoned, 0);
    assert!(
        store.get(&chunk, false).unwrap().is_some() && store.get(&object, false).unwrap().is_some()
    );
    store.pruning_protection_clear(&target, &ctx).unwrap();
    store.apply_batch(omitted, &ctx).unwrap();
    assert!(
        store.get(&chunk, false).unwrap().is_none() && store.get(&object, false).unwrap().is_none()
    );
}

#[test]
fn audit_failure_rolls_back_the_mark_and_purge_removes_claim_and_edge_references() {
    use kaban_core::{AuditEntry, AuditOp, ForgetMode};
    let scratch = Scratch::new("protection", "atomic-audit");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let mut ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let id = NodeId::parse(&common::created(&store, "Fact", "invented atomic fact").id).unwrap();
    let target = PruningTarget::Node(id.clone());
    ctx.audit.push(AuditEntry {
        op: AuditOp::Accept,
        tool: "pruning_protection".into(),
        field: "operation".into(),
        workspace_id: ctx.workspace_id.clone(),
        policy_version: "v1".into(),
        input_hmac: "invented".into(),
        rules: vec![],
        node_id: None,
    });
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute_batch("CREATE TRIGGER refuse_protection_audit BEFORE INSERT ON audit BEGIN SELECT RAISE(ABORT,'invented audit failure'); END;").unwrap();
    assert!(store.pruning_protection_set(&target, &ctx).is_err());
    assert!(
        !store
            .pruning_protection_get(&target, &ctx)
            .unwrap()
            .explicit
    );
    conn.execute_batch("DROP TRIGGER refuse_protection_audit;")
        .unwrap();
    store.pruning_protection_set(&target, &ctx).unwrap();
    let other = NodeId::parse(&common::created(&store, "Fact", "invented endpoint").id).unwrap();
    store
        .link(&id, &other, "RELATES_TO", None, None, &ctx)
        .unwrap();
    let relationship = PruningTarget::Relationship {
        src: id.clone(),
        dst: other.clone(),
        rel: "RELATES_TO".into(),
    };
    store.pruning_protection_set(&relationship, &ctx).unwrap();
    let key: String = conn
        .query_row(
            "SELECT key FROM assertion WHERE subject_kind='edge'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    store
        .pruning_protection_set(&PruningTarget::Claim(kaban_core::AssertionKey(key)), &ctx)
        .unwrap();
    store.forget(&other, ForgetMode::Purge, &ctx).unwrap();
    let marks: i64 = conn
        .query_row("SELECT count(*) FROM pruning_protection", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        marks, 1,
        "only unrelated node mark remains after endpoint purge"
    );
}
