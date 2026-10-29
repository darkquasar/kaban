//! Transactional source-owned derived claim publication and retirement.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use common::Scratch;
use kaban_core::ActorId;
use kaban_core::{
    Actor, BatchAssertion, BatchSubject, ClaimClass, EdgeDiff, Fixed, GazetteerClaimSet,
    IngestStore, Layer, MemoryStore, Method, NewNode, NodeId, Op, Owner, Rank, SourceRev,
    TraverseQuery, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use serde_json::json;

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn node(id: &NodeId, kind: &str, text: &str, props: serde_json::Value) -> Op {
    Op::UpsertNode {
        id: id.clone(),
        node: NewNode {
            kind: kind.into(),
            text: text.into(),
            props: Some(props),
            ..Default::default()
        },
    }
}

fn claim(chunk: &NodeId, target: &NodeId) -> BatchAssertion {
    BatchAssertion {
        subject: BatchSubject::Edge {
            src: chunk.clone(),
            rel: "MENTIONS".into(),
            dst: target.clone(),
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object: None,
        value: json!({"method": "exact", "offset": 0, "len": 10, "score": 9000}),
        layer: Layer::Gazetteer,
        method: Method::Exact,
        extractor_id: None,
        extractor_version: Some("1".into()),
        source_id: Some("doc-fixture".into()),
        lineage_id: "gazetteer:doc-fixture".into(),
        raw_score: Fixed(9000),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    }
}

fn hits(store: &SqliteStore, chunk: &NodeId) -> usize {
    store
        .traverse(&TraverseQuery {
            start_ids: vec![chunk.clone()],
            rels: vec!["MENTIONS".into()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap()
        .edges
        .len()
}

#[test]
fn empty_complete_gazetteer_set_retires_prior_claim_projection() {
    let scratch = Scratch::new("artifacts", "complete-set");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut first = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            node(
                &document,
                "Document",
                "Fixture document",
                json!({"source_id":"doc-fixture","owner":"kaban-ingest","chunk_ids":[chunk]}),
            ),
            node(
                &chunk,
                "Chunk",
                "Architecture",
                json!({"owner":"kaban-ingest"}),
            ),
            node(&target, "Fact", "Architecture", json!({})),
        ],
    );
    first.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    first.assertions.push(claim(&chunk, &target));
    store.apply_batch(first, &ctx()).unwrap();
    assert_eq!(hits(&store, &chunk), 1);
    // Base parses and ignores this additive field: the red uses existing public APIs.
    let mut payload =
        serde_json::to_value(WriteBatch::new(WorkspaceId::default(), vec![])).unwrap();
    payload["edge_diff"] = serde_json::to_value(EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    })
    .unwrap();
    payload["gazetteer_claim_sets"] = json!([{"document": document, "source_id":"doc-fixture", "source_rev": SourceRev::from(&store.get(&document,false).unwrap().unwrap()), "expected_generation":0,"config_hash":"off"}]);
    let off: WriteBatch = serde_json::from_value(payload).unwrap();
    store.apply_batch(off, &ctx()).unwrap();
    assert_eq!(
        hits(&store, &chunk),
        0,
        "empty complete Gazetteer producer set must retire prior MENTIONS"
    );
}

fn complete(
    store: &SqliteStore,
    document: &NodeId,
    assertions: Vec<BatchAssertion>,
    enabled: bool,
) -> WriteBatch {
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    batch.assertions = assertions;
    batch.gazetteer_claim_sets.push(GazetteerClaimSet {
        document: document.clone(),
        source_id: "doc-fixture".into(),
        source_rev: store
            .get(document, false)
            .unwrap()
            .as_ref()
            .map(SourceRev::from),
        expected_generation: store
            .gazetteer_generation(document, &WorkspaceId::default())
            .unwrap(),
        config_hash: if enabled { "on" } else { "off" }.into(),
    });
    batch
}

#[test]
fn complete_set_freshness_binds_source_and_distinguishes_empty_unknown_and_reset() {
    let store = SqliteStore::open_memory().unwrap();
    let document = NodeId::new();
    let workspace = WorkspaceId::default();
    assert!(!store.gazetteer_set_fresh(&document, &workspace).unwrap());
    let mut first = complete(&store, &document, vec![], true);
    first.ops.push(node(
        &document,
        "Document",
        "Fixture document",
        json!({"owner":"kaban-ingest","source_id":"doc-fixture","path":"note.md","chunk_ids":[]}),
    ));
    store.apply_batch(first, &ctx()).unwrap();
    assert!(
        store.gazetteer_set_fresh(&document, &workspace).unwrap(),
        "a successful empty complete set is fresh without an artifact"
    );
    let state = store
        .index_meta()
        .unwrap()
        .entries
        .into_iter()
        .find(|(key, _)| key.starts_with("gazetteer-set:"))
        .unwrap();
    let mut unbound: serde_json::Value = serde_json::from_str(&state.1).unwrap();
    unbound.as_object_mut().unwrap().remove("source_rev");
    store
        .set_index_meta(&state.0, &serde_json::to_string(&unbound).unwrap())
        .unwrap();
    assert!(
        !store.gazetteer_set_fresh(&document, &workspace).unwrap(),
        "legacy unbound state is unknown, not fresh"
    );
    store.set_index_meta(&state.0, &state.1).unwrap();
    let changed = WriteBatch::new(
        workspace.clone(),
        vec![node(
            &document,
            "Document",
            "Edited fixture document",
            json!({"owner":"kaban-ingest","source_id":"doc-fixture","path":"note.md","chunk_ids":[]}),
        )],
    );
    store.apply_batch(changed, &ctx()).unwrap();
    assert!(
        !store.gazetteer_set_fresh(&document, &workspace).unwrap(),
        "a source revision change makes an existing nonempty fingerprint stale"
    );
    let rebuilt = complete(&store, &document, vec![], true);
    store.apply_batch(rebuilt, &ctx()).unwrap();
    assert!(store.gazetteer_set_fresh(&document, &workspace).unwrap());
    let source = serde_json::to_value(store.get(&document, false).unwrap()).unwrap();
    let authority = kaban_core::CurrentProducerAuthority::from_config(
        &kaban_core::config::EngineConfig::default(),
        Layer::Gazetteer,
    )
    .unwrap();
    store
        .maintain_layer(
            authority,
            kaban_core::LayerMaintenance::Reindex,
            Some("note.md"),
            10,
            &ctx(),
        )
        .unwrap();
    assert!(
        !store.gazetteer_set_fresh(&document, &workspace).unwrap(),
        "reset is stale even when the prior successful set was empty"
    );
    assert_eq!(
        serde_json::to_value(store.get(&document, false).unwrap()).unwrap(),
        source
    );
    assert!(matches!(
        store.gazetteer_set_fresh(&document, &WorkspaceId::new("foreign")),
        Err(kaban_core::StoreError::NotFound(_))
    ));
}

#[test]
fn complete_set_publishes_canonical_artifact_and_empty_set_cleans_only_its_source() {
    let scratch = Scratch::new("artifacts", "set-publication");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut first = complete(&store, &document, vec![claim(&chunk, &target)], true);
    first.ops = vec![
        node(
            &document,
            "Document",
            "Fixture document",
            json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":[chunk]}),
        ),
        node(
            &chunk,
            "Chunk",
            "Architecture",
            json!({"owner":"kaban-ingest"}),
        ),
        node(&target, "Fact", "Architecture", json!({})),
    ];
    store.apply_batch(first, &ctx()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM derived_artifact WHERE producer_id='gazetteer@1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "real complete-set publication must persist canonical producer identity"
    );
    let row: (String,String,String) = conn.query_row("SELECT d.source_rev,d.config_hash,d.policy_version FROM derived_artifact d JOIN node n ON n.rid=d.source_rid WHERE n.id=?1", [document.as_str()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        serde_json::from_str::<SourceRev>(&row.0).unwrap(),
        SourceRev::from(&store.get(&document, false).unwrap().unwrap())
    );
    assert_eq!(row.1, "on");
    conn.execute("INSERT INTO derived_artifact SELECT artifact_key||':other-layer',workspace_id,'script','script:fixture',source_rid,source_rev,config_hash,policy_version,published_at FROM derived_artifact WHERE producer_id='gazetteer@1'",[]).unwrap();
    conn.execute("INSERT INTO derived_artifact SELECT artifact_key||':foreign','foreign',layer,producer_id,source_rid,source_rev,config_hash,policy_version,published_at FROM derived_artifact WHERE producer_id='gazetteer@1' AND workspace_id='default'",[]).unwrap();
    let replay = complete(&store, &document, vec![claim(&chunk, &target)], true);
    let before: i64 = conn
        .query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))
        .unwrap();
    store.apply_batch(replay, &ctx()).unwrap();
    assert_eq!(
        before,
        conn.query_row("SELECT max(seq) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    let mut unknown = complete(&store, &document, vec![claim(&chunk, &target)], true);
    unknown.assertions[0].extractor_version = Some("unknown".into());
    assert!(store.apply_batch(unknown, &ctx()).is_err());
    assert_eq!(
        before,
        conn.query_row("SELECT max(seq) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    store
        .apply_batch(complete(&store, &document, vec![], false), &ctx())
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM derived_artifact", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn obsolete_artifact_only_producer_retirement_is_bounded_and_current_refuses() {
    use kaban_core::{CurrentProducerAuthority, MaintenanceRefusal, config::EngineConfig};
    let scratch = Scratch::new("artifacts", "producer-retirement");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![node(
                    &source,
                    "Document",
                    "Fixture",
                    json!({"owner":"kaban-ingest"}),
                )],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let rid: i64 = conn
        .query_row("SELECT rid FROM node WHERE id=?1", [source.as_str()], |r| {
            r.get(0)
        })
        .unwrap();
    for (key, workspace, layer, producer) in [
        ("a", "default", "gazetteer", "obsolete-fixture"),
        ("b", "default", "gazetteer", "obsolete-fixture"),
        ("c", "foreign", "gazetteer", "obsolete-fixture"),
        ("d", "default", "script", "obsolete-fixture"),
        ("e", "default", "gazetteer", "gazetteer@1"),
    ] {
        conn.execute(
            "INSERT INTO derived_artifact VALUES(?1,?2,?3,?4,?5,'{}','fixture','v1',0)",
            rusqlite::params![key, workspace, layer, producer, rid],
        )
        .unwrap();
    }
    let authority =
        CurrentProducerAuthority::from_config(&EngineConfig::default(), Layer::Gazetteer).unwrap();
    let current = store
        .retire_producer(authority, "gazetteer@1", 1, &ctx())
        .unwrap();
    assert_eq!(current.refused, Some(MaintenanceRefusal::CurrentProducer));
    assert_eq!(current.rows, 0);
    assert_eq!(current.transactions, 0);
    let report = store
        .retire_producer(authority, "obsolete-fixture", 1, &ctx())
        .unwrap();
    assert_eq!(report.rows, 2);
    assert_eq!(report.transactions, 2);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM derived_artifact", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert!(store.get(&source, false).unwrap().is_some());
    assert_eq!(
        store
            .retire_producer(authority, "unknown-fixture", 1, &ctx())
            .unwrap()
            .refused,
        Some(MaintenanceRefusal::UnavailableProducer)
    );
}

#[test]
fn layer_reset_rejects_prepared_batch_after_empty_prior_set() {
    use kaban_core::{CurrentProducerAuthority, LayerMaintenance, config::EngineConfig};
    let scratch = Scratch::new("artifacts", "empty-reset");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut first = complete(&store, &document, vec![], false);
    first.ops = vec![
        node(
            &document,
            "Document",
            "Fixture",
            json!({"owner":"kaban-ingest","source_id":"doc-fixture","path":"doc.md","chunk_ids":[chunk]}),
        ),
        node(
            &chunk,
            "Chunk",
            "Architecture",
            json!({"owner":"kaban-ingest"}),
        ),
        node(&target, "Fact", "Architecture", json!({})),
    ];
    store.apply_batch(first, &ctx()).unwrap();
    let stale = complete(&store, &document, vec![claim(&chunk, &target)], true);
    let authority =
        CurrentProducerAuthority::from_config(&EngineConfig::default(), Layer::Gazetteer).unwrap();
    let report = store
        .maintain_layer(authority, LayerMaintenance::Reindex, None, 1, &ctx())
        .unwrap();
    assert_eq!(
        store.apply_batch(stale, &ctx()).unwrap().stale_source,
        1,
        "empty prior set must invalidate prepared producer batches"
    );
    assert_eq!(report.transactions, 1);
    assert_eq!(
        report.rows, 0,
        "state-only reset is not an artifact or claim retirement"
    );
    store
        .apply_batch(
            complete(&store, &document, vec![claim(&chunk, &target)], true),
            &ctx(),
        )
        .unwrap();
    assert_eq!(hits(&store, &chunk), 1);
}

#[test]
fn bounded_layer_reindex_and_forget_preserve_human_authority_and_immutable_claims() {
    use kaban_core::{CurrentProducerAuthority, LayerMaintenance, config::EngineConfig};
    let scratch = Scratch::new("artifacts", "layer-maintenance");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut batch = complete(&store, &document, vec![claim(&chunk, &target)], true);
    batch.ops = vec![
        node(
            &document,
            "Document",
            "Fixture",
            json!({"owner":"kaban-ingest","source_id":"doc-fixture","path":"doc.md","chunk_ids":[chunk]}),
        ),
        node(
            &chunk,
            "Chunk",
            "Architecture",
            json!({"owner":"kaban-ingest"}),
        ),
        node(&target, "Fact", "Architecture", json!({})),
    ];
    store.apply_batch(batch, &ctx()).unwrap();
    let authority =
        CurrentProducerAuthority::from_config(&EngineConfig::default(), Layer::Gazetteer).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let before: String = conn
        .query_row(
            "SELECT value FROM assertion WHERE layer='gazetteer'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let stale = complete(&store, &document, vec![claim(&chunk, &target)], true);
    let reindex = store
        .maintain_layer(
            authority,
            LayerMaintenance::Reindex,
            Some("doc.md"),
            1,
            &ctx(),
        )
        .unwrap();
    assert_eq!(reindex.rows, 2);
    assert_eq!(reindex.transactions, 2);
    assert_eq!(hits(&store, &chunk), 0);
    assert_eq!(store.apply_batch(stale, &ctx()).unwrap().stale_source, 1);
    store
        .apply_batch(
            complete(&store, &document, vec![claim(&chunk, &target)], true),
            &ctx(),
        )
        .unwrap();
    assert_eq!(hits(&store, &chunk), 1);
    let human = WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli);
    let linked = store
        .link(&chunk, &target, "MENTIONS", Some(0.75), None, &human)
        .unwrap();
    let forgotten = store
        .maintain_layer(authority, LayerMaintenance::Forget, None, 1, &ctx())
        .unwrap();
    assert_eq!(forgotten.rows, 2);
    let snapshot = serde_json::to_value(&linked).unwrap();
    store
        .apply_batch(
            complete(&store, &document, vec![claim(&chunk, &target)], true),
            &ctx(),
        )
        .unwrap();
    let edges = store
        .traverse(&TraverseQuery {
            start_ids: vec![chunk],
            rels: vec!["MENTIONS".into()],
            max_depth: 1,
            ..Default::default()
        })
        .unwrap()
        .edges;
    assert_eq!(serde_json::to_value(&edges[0]).unwrap(), snapshot);
    let standing: i64 = conn
        .query_row(
            "SELECT count(*) FROM assertion WHERE layer='gazetteer' AND rank<>'deprecated'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        standing, 0,
        "forget-layer must not resurrect identical machine authority on replay"
    );
    assert_eq!(
        before,
        conn.query_row(
            "SELECT value FROM assertion WHERE layer='gazetteer' ORDER BY id LIMIT 1",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap()
    );
    assert!(
        store
            .maintain_layer(
                authority,
                LayerMaintenance::Forget,
                Some("foreign.md"),
                1,
                &ctx()
            )
            .is_err()
    );
}

#[test]
fn offline_artifact_diagnostics_report_stale_retired_and_cursor_counts_without_writes() {
    use kaban_core::{CurrentProducerAuthority, config::EngineConfig};
    let scratch = Scratch::new("artifacts", "diagnostics");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut batch = complete(&store, &document, vec![claim(&chunk, &target)], true);
    batch.ops = vec![
        node(
            &document,
            "Document",
            "Fixture",
            json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":[chunk]}),
        ),
        node(
            &chunk,
            "Chunk",
            "Architecture",
            json!({"owner":"kaban-ingest"}),
        ),
        node(&target, "Fact", "Architecture", json!({})),
    ];
    store.apply_batch(batch, &ctx()).unwrap();
    let authority =
        CurrentProducerAuthority::from_config(&EngineConfig::default(), Layer::Gazetteer).unwrap();
    assert_eq!(
        store
            .artifact_diagnostics(&WorkspaceId::default(), authority)
            .unwrap(),
        kaban_core::ArtifactDiagnostics::default()
    );
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute(
        "UPDATE derived_artifact SET source_rev='malformed',producer_id='retired-fixture'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projections(name,last_seq) VALUES('reconcile:1',0)",
        [],
    )
    .unwrap();
    let before: i64 = conn
        .query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))
        .unwrap();
    let findings = store
        .artifact_diagnostics(&WorkspaceId::default(), authority)
        .unwrap();
    assert_eq!(findings.stale_artifacts, 1);
    assert_eq!(findings.retired_producers, 1);
    assert_eq!(findings.cursors_behind, 1);
    assert_eq!(
        store
            .artifact_diagnostics(&WorkspaceId::new("foreign"), authority)
            .unwrap()
            .stale_artifacts,
        0
    );
    assert_eq!(
        before,
        conn.query_row("SELECT max(seq) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    assert_eq!(
        conn.query_row("SELECT source_rev FROM derived_artifact", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "malformed"
    );
}

#[test]
fn unknown_legacy_layer_mapping_refuses_without_retiring_any_prior_page() {
    use kaban_core::{
        CurrentProducerAuthority, LayerMaintenance, MaintenanceRefusal, config::EngineConfig,
    };
    let scratch = Scratch::new("artifacts", "untracked-layer");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut known = claim(&chunk, &target);
    let mut unknown = known.clone();
    unknown.extractor_version = Some("unknown".into());
    unknown.value = json!({"unknown":true});
    known.value = json!({"known":true});
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            node(
                &document,
                "Document",
                "Fixture",
                json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":[chunk]}),
            ),
            node(
                &chunk,
                "Chunk",
                "Architecture",
                json!({"owner":"kaban-ingest"}),
            ),
            node(&target, "Fact", "Architecture", json!({})),
        ],
    );
    batch.assertions = vec![known, unknown];
    store.apply_batch(batch, &ctx()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let before: i64 = conn
        .query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))
        .unwrap();
    let authority =
        CurrentProducerAuthority::from_config(&EngineConfig::default(), Layer::Gazetteer).unwrap();
    let result = store
        .maintain_layer(authority, LayerMaintenance::Forget, None, 1, &ctx())
        .unwrap();
    assert_eq!(result.refused, Some(MaintenanceRefusal::UntrackedProducer));
    assert_eq!(result.untracked, 1);
    assert_eq!(result.rows, 0);
    assert_eq!(result.transactions, 0);
    conn.execute("INSERT INTO derived_artifact SELECT 'obsolete-fixture',workspace_id,'gazetteer','obsolete-fixture',rid,json_object('content_hash',content_hash,'rev',rev),'fixture',policy_version,0 FROM node WHERE id=?1",[document.as_str()]).unwrap();
    let retired = store
        .retire_producer(authority, "obsolete-fixture", 1, &ctx())
        .unwrap();
    assert_eq!(retired.refused, Some(MaintenanceRefusal::UntrackedProducer));
    assert!(retired.untracked > 0);
    assert_eq!(retired.rows, 0);
    assert_eq!(retired.transactions, 0);
    assert_eq!(
        before,
        conn.query_row("SELECT max(seq) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM assertion WHERE rank='deprecated'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn tombstoned_document_complete_set_requires_exact_same_batch_reactivation() {
    let scratch = Scratch::new("artifacts", "reactivation-guard");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let document = NodeId::new();
    let mut first = complete(&store, &document, vec![], false);
    first.ops.push(node(
        &document,
        "Document",
        "Fixture document",
        json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":[]}),
    ));
    store.apply_batch(first, &ctx()).unwrap();
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::Tombstone {
                    id: document.clone(),
                    reason: "absent source".into(),
                }],
            ),
            &ctx(),
        )
        .unwrap();
    let generation = store.gazetteer_generation(&document, &WorkspaceId::default());
    assert!(
        generation.is_ok(),
        "owned tombstoned Document generation must remain readable for authorized reactivation: {generation:?}"
    );
    let revision = SourceRev::from(&store.get(&document, true).unwrap().unwrap());
    let mut resumed = WriteBatch::new(
        WorkspaceId::default(),
        vec![Op::Reactivate {
            id: document.clone(),
        }],
    );
    resumed.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    resumed.gazetteer_claim_sets.push(GazetteerClaimSet {
        document: document.clone(),
        source_id: "doc-fixture".into(),
        source_rev: Some(revision),
        expected_generation: generation.unwrap(),
        config_hash: "off".into(),
    });
    let mut missing = resumed.clone();
    missing.ops.clear();
    assert!(
        store.apply_batch(missing, &ctx()).is_err(),
        "tombstone without same-batch Reactivate must be refused"
    );
    let mut wrong = resumed.clone();
    wrong.ops = vec![Op::Reactivate { id: NodeId::new() }];
    assert!(store.apply_batch(wrong, &ctx()).is_err());
    let mut stale = resumed.clone();
    stale.gazetteer_claim_sets[0]
        .source_rev
        .as_mut()
        .unwrap()
        .rev -= 1;
    assert_eq!(store.apply_batch(stale, &ctx()).unwrap().stale_source, 1);
    let mut stale_generation = resumed.clone();
    stale_generation.gazetteer_claim_sets[0].expected_generation += 1;
    assert_eq!(
        store
            .apply_batch(stale_generation, &ctx())
            .unwrap()
            .stale_source,
        1
    );
    let mut foreign = resumed.clone();
    foreign.workspace_id = WorkspaceId::new("foreign");
    let foreign_ctx = WriteCtx::new("v1", foreign.workspace_id.clone(), Actor::Agent);
    assert!(store.apply_batch(foreign, &foreign_ctx).is_err());
    let mut hidden_again = resumed.clone();
    hidden_again.ops.push(Op::Tombstone {
        id: document.clone(),
        reason: "still absent".into(),
    });
    assert!(
        store.apply_batch(hidden_again, &ctx()).is_err(),
        "complete set must require an active postcondition and roll back reactivation otherwise"
    );
    let result = store.apply_batch(resumed, &ctx()).unwrap();
    assert_eq!(result.reactivated, 1);
    assert_eq!(result.stale_source, 0);
    assert!(store.get(&document, false).unwrap().is_some());
}

#[test]
fn complete_set_generation_and_reactivation_refuse_archived_purged_and_unowned_documents() {
    for (lifecycle, owner) in [
        ("archived", "kaban-ingest"),
        ("purged", "kaban-ingest"),
        ("tombstoned", "other"),
    ] {
        let scratch = Scratch::new("artifacts", lifecycle);
        let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
        let document = NodeId::new();
        store
            .apply_batch(
                WriteBatch::new(
                    WorkspaceId::default(),
                    vec![node(
                        &document,
                        "Document",
                        "Fixture document",
                        json!({"owner":owner,"source_id":"doc-fixture","chunk_ids":[]}),
                    )],
                ),
                &ctx(),
            )
            .unwrap();
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
        conn.execute(
            "UPDATE node SET lifecycle=?1 WHERE id=?2",
            rusqlite::params![lifecycle, document.as_str()],
        )
        .unwrap();
        assert!(
            store
                .gazetteer_generation(&document, &WorkspaceId::default())
                .is_err()
        );
        let mut batch = WriteBatch::new(
            WorkspaceId::default(),
            vec![Op::Reactivate {
                id: document.clone(),
            }],
        );
        batch.edge_diff = EdgeDiff::Owned {
            owner: Owner::KabanIngest,
        };
        batch.gazetteer_claim_sets.push(GazetteerClaimSet {
            document,
            source_id: "doc-fixture".into(),
            source_rev: None,
            expected_generation: 0,
            config_hash: "off".into(),
        });
        let before: i64 = conn
            .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
            .unwrap();
        assert!(store.apply_batch(batch, &ctx()).is_err());
        let after: i64 = conn
            .query_row("SELECT COALESCE(max(seq),0) FROM changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
    }
}

#[test]
fn unchanged_on_off_on_has_fresh_claims_and_old_batches_cannot_resurrect() {
    let scratch = Scratch::new("artifacts", "generation-cycle");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut on = complete(&store, &document, vec![claim(&chunk, &target)], true);
    on.ops = vec![
        node(
            &document,
            "Document",
            "Fixture document",
            json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":[chunk]}),
        ),
        node(
            &chunk,
            "Chunk",
            "Architecture",
            json!({"owner":"kaban-ingest"}),
        ),
        node(&target, "Fact", "Architecture", json!({})),
    ];
    store.apply_batch(on.clone(), &ctx()).unwrap();
    assert_eq!(hits(&store, &chunk), 1);
    let revision = SourceRev::from(&store.get(&document, false).unwrap().unwrap());
    assert_eq!(
        store
            .gazetteer_generation(&document, &WorkspaceId::default())
            .unwrap(),
        1
    );
    let same = complete(&store, &document, vec![claim(&chunk, &target)], true);
    let report = store.apply_batch(same, &ctx()).unwrap();
    assert_eq!(report.assertions_inserted, 0);
    assert_eq!(report.assertions_noop, 1);
    assert_eq!(
        store
            .gazetteer_generation(&document, &WorkspaceId::default())
            .unwrap(),
        1
    );
    let off = complete(&store, &document, vec![], false);
    store.apply_batch(off.clone(), &ctx()).unwrap();
    assert_eq!(hits(&store, &chunk), 0);
    assert_eq!(
        store.apply_batch(on.clone(), &ctx()).unwrap().stale_source,
        1
    );
    assert_eq!(hits(&store, &chunk), 0);
    store
        .apply_batch(
            complete(&store, &document, vec![claim(&chunk, &target)], true),
            &ctx(),
        )
        .unwrap();
    assert_eq!(hits(&store, &chunk), 1);
    assert_eq!(
        store
            .gazetteer_generation(&document, &WorkspaceId::default())
            .unwrap(),
        3
    );
    assert_eq!(
        SourceRev::from(&store.get(&document, false).unwrap().unwrap()),
        revision
    );
    assert_eq!(store.apply_batch(off, &ctx()).unwrap().stale_source, 1);
    assert_eq!(hits(&store, &chunk), 1);
    assert_eq!(store.apply_batch(on, &ctx()).unwrap().stale_source, 1);
    assert_eq!(hits(&store, &chunk), 1);
}

#[test]
fn stale_source_artifact_batch_writes_no_sentinel() {
    let scratch = Scratch::new("artifacts", "stale-source");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (source, sentinel) = (NodeId::new(), NodeId::new());
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![node(&source, "Fact", "Original source", json!({}))],
            ),
            &ctx(),
        )
        .unwrap();
    let original = store.get(&source, false).unwrap().unwrap();
    let conn = rusqlite::Connection::open_with_flags(
        scratch.db(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let source_rid: i64 = conn
        .query_row(
            "SELECT rid FROM node WHERE id=?1",
            [source.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![node(&source, "Fact", "Edited source", json!({}))],
            ),
            &ctx(),
        )
        .unwrap();
    let mut payload = serde_json::to_value(WriteBatch::new(
        WorkspaceId::default(),
        vec![node(&sentinel, "Fact", "Stale sentinel", json!({}))],
    ))
    .unwrap();
    payload["edge_diff"] = serde_json::to_value(EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    })
    .unwrap();
    payload["artifacts"] = json!([{"key":"0".repeat(64), "workspace_id": WorkspaceId::default(), "layer":"gazetteer", "producer_id":"gazetteer:1", "source_rid":source_rid, "source_rev":{"content_hash":original.content_hash,"rev":original.rev}, "config_hash":"fixture", "policy_version":"v1", "published_at":0}]);
    store
        .apply_batch(serde_json::from_value(payload).unwrap(), &ctx())
        .unwrap();
    assert!(
        store.get(&sentinel, false).unwrap().is_none(),
        "stale source publication must not write sentinel nodes"
    );
}

#[test]
fn complete_set_rejects_foreign_document_producer_source_and_rolls_back() {
    let scratch = Scratch::new("artifacts", "forged-set");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target, sentinel) =
        (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
    let mut first = complete(&store, &document, vec![claim(&chunk, &target)], true);
    first.ops = vec![
        node(
            &document,
            "Document",
            "Fixture document",
            json!({"source_id":"doc-fixture","owner":"kaban-ingest","chunk_ids":[chunk]}),
        ),
        node(
            &chunk,
            "Chunk",
            "Architecture",
            json!({"owner":"kaban-ingest"}),
        ),
        node(&target, "Fact", "Architecture", json!({})),
    ];
    store.apply_batch(first, &ctx()).unwrap();
    let mut forged = complete(&store, &document, vec![], false);
    forged.gazetteer_claim_sets[0].source_id = "foreign-source".into();
    forged
        .ops
        .push(node(&sentinel, "Fact", "Sentinel", json!({})));
    assert!(store.apply_batch(forged, &ctx()).is_err());
    assert!(store.get(&sentinel, false).unwrap().is_none());
    let mut foreign = complete(&store, &document, vec![], false);
    foreign.workspace_id = WorkspaceId::new("foreign");
    let foreign_ctx = WriteCtx::new("v1", WorkspaceId::new("foreign"), Actor::Agent);
    assert!(store.apply_batch(foreign, &foreign_ctx).is_err());
    let mut producer = complete(&store, &document, vec![], false);
    producer.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanParse,
    };
    assert!(store.apply_batch(producer, &ctx()).is_err());
    let mut failed = complete(&store, &document, vec![], false);
    failed
        .ops
        .push(node(&sentinel, "Fact", "Sentinel", json!({})));
    failed.ops.push(Op::UpsertEdge {
        src: sentinel.clone(),
        dst: NodeId::new(),
        rel: "RELATES_TO".into(),
        props: None,
    });
    assert!(store.apply_batch(failed, &ctx()).is_err());
    assert!(store.get(&sentinel, false).unwrap().is_none());
    assert_eq!(
        hits(&store, &chunk),
        1,
        "later invalid operation rolls back producer retirement"
    );
    assert_eq!(
        store
            .gazetteer_generation(&document, &WorkspaceId::default())
            .unwrap(),
        1
    );
}

#[test]
fn empty_set_preserves_human_other_layer_and_foreign_lineage() {
    let scratch = Scratch::new("artifacts", "preservation");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let document = NodeId::new();
    let chunks = [NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new()];
    let targets = [NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new()];
    let mut own = claim(&chunks[0], &targets[0]);
    let mut human = claim(&chunks[1], &targets[1]);
    human.layer = Layer::Human;
    human.lineage_id = "human:fixture".into();
    let mut script = claim(&chunks[2], &targets[2]);
    script.layer = Layer::V4;
    script.method = Method::Measured;
    let mut other = claim(&chunks[3], &targets[3]);
    other.lineage_id = "gazetteer:foreign-source".into();
    other.source_id = Some("foreign-source".into());
    own.raw_score = Fixed(9999);
    let mut first = complete(&store, &document, vec![own, human, script, other], true);
    first.ops.push(node(
        &document,
        "Document",
        "Fixture document",
        json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":chunks}),
    ));
    for (chunk, target) in chunks.iter().zip(&targets) {
        first.ops.push(node(
            chunk,
            "Chunk",
            chunk.as_str(),
            json!({"owner":"kaban-ingest"}),
        ));
        first
            .ops
            .push(node(target, "Fact", target.as_str(), json!({})));
    }
    store.apply_batch(first, &ctx()).unwrap();
    store
        .apply_batch(complete(&store, &document, vec![], false), &ctx())
        .unwrap();
    assert_eq!(hits(&store, &chunks[0]), 0);
    for chunk in &chunks[1..] {
        assert_eq!(
            hits(&store, chunk),
            1,
            "other authority must remain projected"
        );
    }
    // An old raw assertion replay cannot restore the retired own lineage.
    let mut replay = WriteBatch::new(WorkspaceId::default(), vec![]);
    replay.assertions.push(claim(&chunks[0], &targets[0]));
    replay.assertions[0].extractor_version = Some("1:set:1".into());
    store.apply_batch(replay, &ctx()).unwrap();
    assert_eq!(hits(&store, &chunks[0]), 0);
}

#[test]
fn source_edit_invalidates_existing_artifact_in_same_transaction() {
    let scratch = Scratch::new("artifacts", "source-invalidation");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![node(&source, "Fact", "Original source", json!({}))],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let rid: i64 = conn
        .query_row("SELECT rid FROM node WHERE id=?1", [source.as_str()], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute("INSERT INTO derived_artifact(artifact_key,workspace_id,layer,producer_id,source_rid,source_rev,config_hash,policy_version,published_at) VALUES('fixture',?1,'gazetteer','gazetteer:1',?2,'{}','fixture','v1',0)",rusqlite::params![WorkspaceId::default().as_str(),rid]).unwrap();
    let mut failed = WriteBatch::new(
        WorkspaceId::default(),
        vec![node(&source, "Fact", "Uncommitted edit", json!({}))],
    );
    failed.ops.push(Op::UpsertEdge {
        src: source.clone(),
        dst: NodeId::new(),
        rel: "RELATES_TO".into(),
        props: None,
    });
    assert!(store.apply_batch(failed, &ctx()).is_err());
    assert_eq!(
        store.get(&source, false).unwrap().unwrap().text,
        "Original source"
    );
    let remaining: i64 = conn
        .query_row(
            "SELECT count(*) FROM derived_artifact WHERE source_rid=?1",
            [rid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining, 1,
        "failed edit must roll back artifact invalidation"
    );
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![node(&source, "Fact", "Edited source", json!({}))],
            ),
            &ctx(),
        )
        .unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM derived_artifact WHERE source_rid=?1",
            [rid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "source edit must invalidate its prior derived artifacts atomically"
    );
}

#[test]
fn parse_artifact_publication_is_idempotent_and_refuses_owner_or_foreign_key_collision() {
    use kaban_core::{DerivedArtifact, artifact_key};
    let scratch = Scratch::new("artifacts", "parse-publication");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![node(&source, "Fact", "Source", json!({}))],
            ),
            &ctx(),
        )
        .unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let source_rid: i64 = conn
        .query_row("SELECT rid FROM node WHERE id=?1", [source.as_str()], |r| {
            r.get(0)
        })
        .unwrap();
    let source_rev = SourceRev::from(&store.get(&source, false).unwrap().unwrap());
    let artifact = DerivedArtifact {
        key: artifact_key(&source_rev, Layer::Script, "script:1", "config", "v1"),
        workspace_id: WorkspaceId::default(),
        layer: Layer::Script,
        producer_id: "script:1".into(),
        source_rid,
        source_rev,
        config_hash: "config".into(),
        policy_version: "v1".into(),
        published_at: 0,
    };
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanParse,
    };
    batch.artifacts.push(artifact.clone());
    store.apply_batch(batch.clone(), &ctx()).unwrap();
    store.apply_batch(batch.clone(), &ctx()).unwrap();
    for workspace in [WorkspaceId::default(), WorkspaceId::new("other-workspace")] {
        let other = NodeId::new();
        let other_ctx = WriteCtx::new("v1", workspace.clone(), Actor::Agent);
        store
            .apply_batch(
                WriteBatch::new(
                    workspace.clone(),
                    vec![node(&other, "Fact", "Source", json!({}))],
                ),
                &other_ctx,
            )
            .unwrap();
        let other_node = store.get(&other, false).unwrap().unwrap();
        assert_eq!(
            other_node.rev, artifact.source_rev.rev,
            "fixture sources have the same revision"
        );
        assert_ne!(
            other_node.content_hash, artifact.source_rev.content_hash,
            "persisted source hashes bind identity/workspace"
        );
        let other_rid: i64 = conn
            .query_row("SELECT rid FROM node WHERE id=?1", [other.as_str()], |r| {
                r.get(0)
            })
            .unwrap();
        let other_rev = SourceRev::from(&other_node);
        let mut other_batch = WriteBatch::new(workspace.clone(), vec![]);
        other_batch.edge_diff = batch.edge_diff.clone();
        other_batch.artifacts.push(DerivedArtifact {
            key: artifact_key(&other_rev, Layer::Script, "script:1", "config", "v1"),
            workspace_id: workspace,
            source_rid: other_rid,
            source_rev: other_rev,
            ..artifact.clone()
        });
        store.apply_batch(other_batch, &other_ctx).unwrap();
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM derived_artifact", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 3,
        "same content/revision sources and tenants have independent artifact keys"
    );
    let mut wrong = batch.clone();
    wrong.edge_diff = EdgeDiff::Owned {
        owner: Owner::KabanIngest,
    };
    assert!(store.apply_batch(wrong, &ctx()).is_err());
    conn.execute(
        "UPDATE derived_artifact SET workspace_id='foreign' WHERE artifact_key=?1",
        [artifact.key.as_str()],
    )
    .unwrap();
    assert!(
        store.apply_batch(batch, &ctx()).is_err(),
        "foreign artifact key must not be a successful idempotent publication"
    );
}

#[test]
fn artifact_retirement_queries_use_existing_lineage_and_source_indexes() {
    let scratch = Scratch::new("artifacts", "index-plans");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![
            node(
                &document,
                "Document",
                "Fixture",
                json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":[chunk]}),
            ),
            node(
                &chunk,
                "Chunk",
                "Architecture",
                json!({"owner":"kaban-ingest"}),
            ),
            node(&target, "Fact", "Architecture", json!({})),
        ],
    );
    for i in 0..128 {
        let mut assertion = claim(&chunk, &target);
        assertion.value = json!({"fixture":i});
        batch.assertions.push(assertion);
    }
    store.apply_batch(batch, &ctx()).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let rid: i64 = conn
        .query_row(
            "SELECT rid FROM node WHERE id=?1",
            [document.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    for i in 0..128 {
        conn.execute("INSERT INTO derived_artifact VALUES(?1,'default','gazetteer','retired-fixture',?2,'{}','fixture','v1',0)",rusqlite::params![format!("{i:064x}"),rid]).unwrap();
    }
    for (sql, index) in [
        (
            "EXPLAIN QUERY PLAN SELECT id,key FROM assertion WHERE lineage_id='gazetteer:fixture' AND workspace_id='default' AND layer='gazetteer'",
            "assertion_lineage",
        ),
        (
            "EXPLAIN QUERY PLAN DELETE FROM derived_artifact WHERE source_rid=1 AND (source_rev <> '{}' OR policy_version <> 'v1')",
            "derived_artifact_source",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT id FROM assertion WHERE layer='gazetteer' AND workspace_id='default' AND id>0 AND rank<>'deprecated' ORDER BY id LIMIT 3",
            "assertion_layer",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT artifact_key FROM derived_artifact WHERE workspace_id='default' AND layer='gazetteer' AND producer_id='retired-fixture' LIMIT 3",
            "derived_artifact_layer",
        ),
    ] {
        let details: Vec<String> = conn
            .prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            details.iter().any(|detail| detail.contains(index)),
            "indexed retirement query: {details:?}"
        );
        assert!(
            !details.iter().any(|detail| detail.starts_with("SCAN ")),
            "no full scan: {details:?}"
        );
    }
}

#[test]
fn artifact_source_coordinates_hide_foreign_unknown_and_nonactive_sources() {
    let scratch = Scratch::new("artifacts", "source-coordinates");
    let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
    let source = NodeId::new();
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![node(&source, "Fact", "Source", json!({}))],
            ),
            &ctx(),
        )
        .unwrap();
    let admitted = store
        .artifact_source(&source, &WorkspaceId::default())
        .unwrap()
        .unwrap();
    assert!(admitted.source_rid > 0);
    assert_eq!(
        admitted.source_rev,
        SourceRev::from(&store.get(&source, false).unwrap().unwrap())
    );
    assert_eq!(
        store
            .artifact_source(&source, &WorkspaceId::new("foreign"))
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .artifact_source(&NodeId::new(), &WorkspaceId::default())
            .unwrap(),
        None
    );
    store
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::Tombstone {
                    id: source.clone(),
                    reason: "fixture".into(),
                }],
            ),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        store
            .artifact_source(&source, &WorkspaceId::default())
            .unwrap(),
        None
    );
}

#[test]
fn human_deprecated_gazetteer_claim_is_not_resurrected_by_on_off_on() {
    for (reason, after_off) in [
        ("incorrect mention", false),
        ("producer-set:manual", false),
        ("producer-set:retired", false),
        ("producer-set:retired", true),
        ("policy:fixture", true),
    ] {
        let scratch = Scratch::new("artifacts", reason);
        let store = SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap();
        let (document, chunk, target) = (NodeId::new(), NodeId::new(), NodeId::new());
        let mut first = complete(&store, &document, vec![claim(&chunk, &target)], true);
        first.ops = vec![
            node(
                &document,
                "Document",
                "Fixture document",
                json!({"owner":"kaban-ingest","source_id":"doc-fixture","chunk_ids":[chunk]}),
            ),
            node(
                &chunk,
                "Chunk",
                "Architecture",
                json!({"owner":"kaban-ingest"}),
            ),
            node(&target, "Fact", "Architecture", json!({})),
        ];
        store.apply_batch(first, &ctx()).unwrap();
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        let key: String = conn
            .query_row(
                "SELECT key FROM assertion WHERE lineage_id='gazetteer:doc-fixture'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let human_ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli);
        if after_off {
            store
                .apply_batch(complete(&store, &document, vec![], false), &ctx())
                .unwrap();
        }
        if reason == "policy:fixture" {
            store
                .rescan_assertions("v2", &mut |claim, _audit| {
                    Some(claim.value.as_value().clone())
                })
                .unwrap();
        } else {
            store.deprecate_assertion(&key, reason, &human_ctx).unwrap();
        }
        assert_eq!(hits(&store, &chunk), 0);
        if !after_off {
            store
                .apply_batch(complete(&store, &document, vec![], false), &ctx())
                .unwrap();
        }
        store
            .apply_batch(
                complete(&store, &document, vec![claim(&chunk, &target)], true),
                &ctx(),
            )
            .unwrap();
        assert_eq!(
            hits(&store, &chunk),
            0,
            "producer generation must not resurrect a human-deprecated Gazetteer claim (reason={reason})"
        );
    }
}
