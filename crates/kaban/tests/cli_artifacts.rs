//! Explicit maintenance stays opt-in and uses the diagnostic CLI gates.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::Scratch;
use kaban_core::{Actor, MemoryStore, NewNode, WorkspaceId, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use std::process::Command;

/// The first writer of a `(src, dst, rel)` triple keeps the row (owner decision 2026-11-14):
/// a later producer's upsert of a gazetteer mention or of an authored link writes nothing and
/// is counted, and a settled repeated ingestion changes neither the links nor the source.
#[cfg(feature = "ingest")]
#[test]
fn a_later_producer_keeps_the_first_writers_edges_and_a_settled_ingest_changes_nothing() {
    use kaban_core::{IngestStore, NodeId, RememberOutcome};
    let scratch = Scratch::new("cli-artifacts", "first-writer");
    let root = scratch.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("note.md"),
        "Public Cart Architecture describes the system.",
    )
    .unwrap();
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    let ctx = WriteCtx::new(WorkspaceId::default(), Actor::Agent);
    let remember = |text: &str| match store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: text.into(),
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
    let target = remember("Public Cart Architecture");
    let authored_target = remember("Authored retained reference");
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
    run(&["ingest", "note.md", "--link", "gazetteer"]);
    let chunks = store.list_nodes("Chunk", "default", 0, 100).unwrap();
    let chunk = NodeId::parse(&chunks[0].id).unwrap();
    store
        .link(
            &chunk,
            &authored_target,
            "MENTIONS",
            Some(0.8),
            None,
            &WriteCtx::new(WorkspaceId::default(), Actor::HumanCli),
        )
        .unwrap();
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
    let links = || {
        let edges = store
            .traverse(&kaban_core::TraverseQuery {
                start_ids: vec![chunk.clone()],
                rels: vec!["MENTIONS".into()],
                max_depth: 1,
                ..Default::default()
            })
            .unwrap()
            .edges;
        serde_json::to_value(edges).unwrap()
    };
    let first = links();
    let producers = |v: &serde_json::Value| -> Vec<(String, String)> {
        let mut out: Vec<_> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let producer: String = conn
                    .query_row(
                        "SELECT producer FROM edge WHERE id=?1",
                        [e["id"].as_str().unwrap()],
                        |r| r.get(0),
                    )
                    .unwrap();
                (e["dst"].as_str().unwrap().to_owned(), producer)
            })
            .collect();
        out.sort();
        out
    };
    let mut expected = vec![
        (target.to_string(), "gazetteer".to_owned()),
        (authored_target.to_string(), "authored".to_owned()),
    ];
    expected.sort();
    assert_eq!(producers(&first), expected);
    // two later producers upsert both triples: each is the first writer's, nothing is written
    let mut later = kaban_core::WriteBatch::new(
        WorkspaceId::default(),
        vec![
            kaban_core::Op::UpsertEdge {
                src: chunk.clone(),
                dst: authored_target,
                rel: "MENTIONS".into(),
                props: Some(
                    serde_json::json!({"producer": "scip_import", "method": "measured", "score": 9000}),
                ),
            },
            kaban_core::Op::UpsertEdge {
                src: chunk.clone(),
                dst: target.clone(),
                rel: "MENTIONS".into(),
                props: Some(serde_json::json!({"method": "llm", "score": 9000})),
            },
        ],
    );
    later.edge_diff = kaban_core::EdgeDiff::Owned {
        producer: kaban_core::Producer::Extractor(kaban_core::ExtractorKind::Llm),
    };
    let report = store.apply_batch(later, &ctx).unwrap();
    assert_eq!(report.edges_kept_by_first_writer, 2, "{report:?}");
    assert_eq!(
        links(),
        first,
        "the first writers keep their rows unchanged"
    );

    let source_before = store.list_nodes("Document", "default", 0, 100).unwrap()[0].clone();
    let settled = run(&["ingest", "note.md", "--link", "gazetteer"]);
    let report: serde_json::Value = serde_json::from_slice(&settled.stdout).unwrap();
    assert_eq!(report["documents"], 0);
    assert_eq!(report["skipped"], 1, "settled repeated ingestion skips");
    assert_eq!(links(), first, "a settled ingestion keeps every link");
    let source_after = store.list_nodes("Document", "default", 0, 100).unwrap()[0].clone();
    assert_eq!(
        serde_json::to_value(source_before).unwrap(),
        serde_json::to_value(source_after).unwrap(),
        "an unchanged explicit ingestion must not mutate the canonical source"
    );
}

/// OD-P1: the layer maintenance verbs are gone (no alias): `reindex layer` and `forget
/// --layer` are refused without touching the database.
#[test]
fn removed_layer_commands_are_refused_and_leave_the_database_alone() {
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
                &WriteCtx::new(WorkspaceId::default(), Actor::Agent),
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
            Some(4),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
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
                &WriteCtx::new(WorkspaceId::default(), Actor::Agent),
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
