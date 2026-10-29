//! Explicit maintenance stays opt-in and uses the diagnostic CLI gates.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::Scratch;
use kaban_core::{Actor, MemoryStore, NewNode, WorkspaceId, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use std::process::Command;

#[cfg(feature = "ingest")]
#[test]
fn explicit_ingestion_restores_reset_links_and_preserves_other_authority() {
    use kaban_core::{IngestStore, NodeId, RememberOutcome};
    let scratch = Scratch::new("cli-artifacts", "explicit-reingest");
    let root = scratch.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("note.md"),
        "Public Cart Architecture describes the system.",
    )
    .unwrap();
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    let ctx = WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent);
    let target = match store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "Public Cart Architecture".into(),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(node) | RememberOutcome::Existing { node, .. } => {
            NodeId::parse(&node.id).unwrap()
        }
    };
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .current_dir(&root)
            .env("KABAN_HOME", scratch.home())
            .env("KABAN_WORKSPACE", "default")
            .args(["--db", scratch.db().to_str().unwrap(), "--json"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        out
    };
    let human_target = match store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "Human retained reference".into(),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(node) | RememberOutcome::Existing { node, .. } => {
            NodeId::parse(&node.id).unwrap()
        }
    };
    run(&["ingest", "note.md", "--link", "gazetteer"]);
    let chunks = store.list_nodes("Chunk", "default", 0, 100).unwrap();
    let chunk = NodeId::parse(&chunks[0].id).unwrap();
    let human = store
        .link(
            &chunk,
            &human_target,
            "MENTIONS",
            Some(0.8),
            None,
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::HumanCli),
        )
        .unwrap();
    let mut other_layer = kaban_core::WriteBatch::new(WorkspaceId::default(), vec![]);
    other_layer.assertions.push(kaban_core::BatchAssertion {
        subject: kaban_core::BatchSubject::Edge {
            src: chunk.clone(),
            rel: "MENTIONS".into(),
            dst: human_target,
        },
        claim_class: kaban_core::ClaimClass::Edge,
        predicate: None,
        object: None,
        value: serde_json::json!({"method":"measured","score":9000}),
        layer: kaban_core::Layer::V4,
        method: kaban_core::Method::Measured,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id: "v4:fixture".into(),
        raw_score: kaban_core::Fixed(9000),
        rank: kaban_core::Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    });
    store.apply_batch(other_layer, &ctx).unwrap();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM derived_artifact WHERE producer_id='gazetteer@1'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let foreign = match store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "Foreign retained source".into(),
                workspace_id: Some("foreign".into()),
                ..Default::default()
            },
            &[],
            &WriteCtx::new("v1", WorkspaceId::new("foreign"), Actor::Agent),
        )
        .unwrap()
    {
        RememberOutcome::Created(node) | RememberOutcome::Existing { node, .. } => node,
    };
    conn.execute("INSERT INTO derived_artifact SELECT 'foreign-sentinel',n.workspace_id,'gazetteer','foreign-fixture',n.rid,json_object('content_hash',n.content_hash,'rev',n.rev),'fixture',n.policy_version,0 FROM node n WHERE n.id=?1",[foreign.id.as_str()]).unwrap();
    conn.execute("INSERT INTO derived_artifact SELECT 'other-layer-sentinel',workspace_id,'script',producer_id,source_rid,source_rev,config_hash,policy_version,published_at FROM derived_artifact WHERE artifact_key<>'foreign-sentinel' LIMIT 1",[]).unwrap();
    let source_before = store.list_nodes("Document", "default", 0, 100).unwrap()[0].clone();
    let human_before: String = conn
        .query_row("SELECT value FROM assertion WHERE layer='human'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let other_before: String = conn
        .query_row("SELECT value FROM assertion WHERE layer='v4'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let reset = run(&["reindex", "layer", "gazetteer"]);
    assert!(String::from_utf8_lossy(&reset.stderr).contains("rebuilding has not run"));
    assert_eq!(conn.query_row("SELECT count(*) FROM derived_artifact WHERE artifact_key IN ('foreign-sentinel','other-layer-sentinel')",[],|r|r.get::<_,i64>(0)).unwrap(),2,"reset preserves other-layer and valid foreign artifacts");
    let links = || {
        store
            .traverse(&kaban_core::TraverseQuery {
                start_ids: vec![chunk.clone()],
                rels: vec!["MENTIONS".into()],
                max_depth: 1,
                ..Default::default()
            })
            .unwrap()
            .edges
    };
    assert!(!links().iter().any(|edge| edge.dst == target.as_str()));
    assert_eq!(
        serde_json::to_value(&links()[0]).unwrap(),
        serde_json::to_value(&human).unwrap()
    );
    run(&["ingest", "note.md", "--link", "gazetteer"]);
    assert!(
        links().iter().any(|edge| edge.dst == target.as_str()),
        "next explicit enabled ingestion restores generated links"
    );
    assert!(
        links().iter().any(
            |edge| serde_json::to_value(edge).unwrap() == serde_json::to_value(&human).unwrap()
        )
    );
    assert_eq!(
        conn.query_row("SELECT value FROM assertion WHERE layer='human'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        human_before
    );
    assert_eq!(
        conn.query_row(
            "SELECT value FROM assertion WHERE layer='v4' AND rank='normal'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        other_before
    );
    assert_eq!(
        serde_json::to_value(
            store
                .get(&NodeId::parse(&foreign.id).unwrap(), false)
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(foreign).unwrap()
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM derived_artifact WHERE artifact_key='foreign-sentinel'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let source_after = store.list_nodes("Document", "default", 0, 100).unwrap()[0].clone();
    assert_eq!(
        serde_json::to_value(source_before).unwrap(),
        serde_json::to_value(source_after).unwrap(),
        "unchanged explicit rebuild must not mutate the canonical source"
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM derived_artifact WHERE artifact_key='other-layer-sentinel'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let settled = run(&["ingest", "note.md", "--link", "gazetteer"]);
    let report: serde_json::Value = serde_json::from_slice(&settled.stdout).unwrap();
    assert_eq!(report["documents"], 0);
    assert_eq!(report["skipped"], 1, "settled repeated ingestion skips");
}

#[test]
fn layer_commands_are_db_only_and_refuse_invalid_or_human_layers() {
    let scratch = Scratch::new("cli-artifacts", "layer");
    {
        let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
        store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: "Untouched".into(),
                    ..Default::default()
                },
                &[],
                &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
            )
            .unwrap();
    }
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", scratch.home())
            .args(["--db", scratch.db().to_str().unwrap(), "--json"])
            .args(args)
            .output()
            .unwrap()
    };
    for args in [
        vec!["reindex", "layer", "gazetteer"],
        vec!["forget", "--layer", "gazetteer"],
    ] {
        let result = run(&args);
        assert_eq!(
            result.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()["rows"],
            0
        );
    }
    for args in [
        vec!["reindex", "layer", "unknown"],
        vec!["forget", "--layer", "human"],
        vec!["forget", "--layer", "embed"],
    ] {
        assert_eq!(run(&args).status.code(), Some(4));
    }
    assert!(!scratch.home().join(".kaban").exists());
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    assert_eq!(store.stats(None).unwrap()["nodes"], 1);
}

#[test]
fn doctor_optimize_is_explicit_and_missing_database_is_never_created() {
    let scratch = Scratch::new("cli-artifacts", "optimize");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", scratch.home())
            .args(["--db", scratch.db().to_str().unwrap(), "--json", "doctor"])
            .args(args)
            .output()
            .unwrap()
    };
    let absent = run(&["--optimize"]);
    assert_eq!(absent.status.code(), Some(5));
    assert!(!scratch.db().exists());
    {
        let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
        store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: "Fixture".into(),
                    ..Default::default()
                },
                &[],
                &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
            )
            .unwrap();
    }
    let regular = run(&[]);
    assert_eq!(
        regular.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&regular.stderr)
    );
    let regular: serde_json::Value = serde_json::from_slice(&regular.stdout).unwrap();
    assert_eq!(
        regular["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|check| check["name"] == "optimize")
            .count(),
        0
    );
    let explicit = run(&["--optimize"]);
    assert_eq!(
        explicit.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let explicit: serde_json::Value = serde_json::from_slice(&explicit.stdout).unwrap();
    let checks: Vec<_> = explicit["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["name"] == "optimize")
        .collect();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["ok"], true);
}
