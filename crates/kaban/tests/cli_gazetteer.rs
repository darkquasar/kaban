//! Gazetteer CLI control acceptance and rejection through the public binary.
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use common::Scratch;
use std::process::Command;

#[test]
fn admitted_symbol_source_drops_once_over_budget_and_forced_run_keeps_it() {
    use kaban_api::{ApiConfig, KabanApi};
    use kaban_codegraph::write::{self, FileRow, SymbolRow};
    use kaban_codegraph::{DiscoverProvider, FileId, NodeKind};
    use kaban_core::{
        Actor, MemoryStore, NewNode, RememberOutcome, TraverseQuery, WorkspaceId, WriteCtx,
    };
    use kaban_ingest::{
        ChunkConfig, DocumentPreparer, IngestConfig, Ingestor, WalkConfig, run_blocking,
    };
    use kaban_policy::Policy;
    use kaban_store_sqlite::SqliteStore;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let scratch = Scratch::new("symbol-gazetteer", "budget");
    let root = scratch.0.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap()
                .success()
        );
    }
    let head = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let commit = String::from_utf8(head.stdout).unwrap().trim().to_owned();
    let cg = scratch.0.join("codegraph");
    let dir = cg.join("4d2a9f0c7b1e6a3d8c5f2e9b0a7d4c1f");
    std::fs::create_dir_all(&dir).unwrap();
    for path in [&cg, &dir] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = dir.join(format!("{commit}.cg.db"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
    drop(file);
    let conn = rusqlite::Connection::open(&path).unwrap();
    write::begin(&conn).unwrap();
    write::insert_files(
        &conn,
        &[FileRow {
            id: FileId(1),
            path: "budget.go".into(),
            language: "go".into(),
            is_test: false,
            lines: 1,
        }],
    )
    .unwrap();
    let key = "scip-go gomod example.com/budget . Public#BudgetSymbol().";
    write::insert_symbols(
        &conn,
        &[SymbolRow {
            id: kaban_codegraph::NodeId(1),
            kind: NodeKind::Symbol,
            r#ref: format!("scip:{key}"),
            scip: Some(key.into()),
            key: Some(key.into()),
            label: "BudgetSymbol".into(),
            name: "architecture".repeat(20_000),
            file_id: Some(FileId(1)),
            def: None,
            enc: None,
            signature: None,
            doc: None,
            external: false,
            is_test: false,
            ident_terms: "budget symbol".into(),
        }],
    )
    .unwrap();
    let schema = kaban_codegraph::SCHEMA_VERSION.to_string();
    write::insert_meta(
        &conn,
        &[
            ("caches", ""),
            ("commit", &commit),
            ("counts:files", "1"),
            ("counts:symbols", "1"),
            ("isolation", "none"),
            ("isolation_detail", ""),
            ("languages", "go"),
            ("policy_version", "symbol-budget"),
            ("repo_id", "4d2a9f0c7b1e6a3d8c5f2e9b0a7d4c1f"),
            ("schema_version", &schema),
            ("scip_range_form", "end_exclusive"),
            ("symbols_fts_version", "1"),
        ],
    )
    .unwrap();
    write::rebuild_fts(&conn).unwrap();
    write::finish(&conn).unwrap();
    drop(conn);
    let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap());
    let ctx = WriteCtx::new("symbol-budget", WorkspaceId::default(), Actor::Agent);
    let remembered = |node| match store.remember(node, &[], &ctx).unwrap() {
        RememberOutcome::Created(node) | RememberOutcome::Existing { node, .. } => {
            kaban_core::NodeId::parse(&node.id).unwrap()
        }
    };
    let title = remembered(NewNode {
        kind: "Fact".into(),
        text: "Public Cart Architecture".into(),
        ..Default::default()
    });
    let symbol = remembered(NewNode {
        kind: "CodeRef".into(),
        text: "Symbol anchor".into(),
        props: Some(serde_json::json!({"symbol": key})),
        ..Default::default()
    });
    let mut engine = kaban_core::config::EngineConfig::default();
    engine.link.default = "gazetteer".into();
    engine.gazetteer.max_automaton_mb = 1;
    let api = Arc::new(
        KabanApi::with_config(
            store.clone(),
            Policy::from_file(
                Policy::parse(b"defaults: {max_field_chars: 1000000}\n").unwrap(),
                "symbol-budget".into(),
            )
            .unwrap(),
            store.signer(),
            None,
            ApiConfig {
                engine: Arc::new(engine),
                repo: Some("fixture".into()),
                repo_root: Some(root.clone()),
                ..Default::default()
            },
        )
        .with_snapshot_provider(dir.clone(), Arc::new(DiscoverProvider::new(dir))),
    );
    api.with_current_snapshot(|resolved| {
        assert_eq!(resolved, Some(commit.as_str()));
        Ok::<_, kaban_core::StoreError>(())
    })
    .unwrap();
    std::fs::write(
        root.join("note.md"),
        "Public Cart Architecture uses BudgetSymbol.",
    )
    .unwrap();
    for forced in [false, true] {
        let cfg = IngestConfig {
            symbols: forced,
            ..Default::default()
        };
        let preparer = Arc::new(DocumentPreparer::new(
            &api,
            store.clone(),
            store.signer(),
            ChunkConfig::default(),
            cfg.clone(),
        ));
        let ingestor = Ingestor::new(api.clone(), store.clone(), store.signer(), preparer, cfg);
        let report = run_blocking(
            &ingestor,
            &WalkConfig::new(root.clone()),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(report.documents, 1);
        let telemetry = report.gazetteer.unwrap();
        assert_eq!(telemetry.outcome, "ok");
        if forced {
            assert_eq!(telemetry.dropped, Vec::<String>::new());
            assert_eq!(telemetry.budget_lifted_by.as_deref(), Some("--symbols"));
        } else {
            assert_eq!(telemetry.dropped, ["v4_symbols"]);
            assert_eq!(telemetry.budget_lifted_by, None);
        }
        let chunks = kaban_core::IngestStore::list_nodes(
            store.as_ref(),
            "Chunk",
            WorkspaceId::default().as_str(),
            0,
            500,
        )
        .unwrap();
        let edges = store
            .traverse(&TraverseQuery {
                start_ids: chunks
                    .iter()
                    .map(|node| kaban_core::NodeId::parse(&node.id).unwrap())
                    .collect(),
                rels: vec!["MENTIONS".into()],
                max_depth: 1,
                ..Default::default()
            })
            .unwrap()
            .edges;
        assert!(edges.iter().any(|edge| edge.dst == title.as_str()));
        assert_eq!(edges.iter().any(|edge| edge.dst == symbol.as_str()), forced);
    }
}

#[test]
fn link_modes_are_accepted_and_invalid_mode_is_refused() {
    let scratch = Scratch::new("cli-gazetteer", "controls");
    let root = scratch.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("note.md"),
        "# Public Architecture\n\nSafe text.\n",
    )
    .unwrap();
    for mode in ["gazetteer", "none"] {
        let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .current_dir(&root)
            .env("KABAN_HOME", scratch.home())
            .args([
                "--db",
                scratch.db().to_str().unwrap(),
                "--json",
                "ingest",
                "note.md",
                "--link",
                mode,
                "--dry-run",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["dry_run"], true);
        assert_eq!(report["outbox_rows"], 0);
    }
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(&root)
        .env("KABAN_HOME", scratch.home())
        .args([
            "--db",
            scratch.db().to_str().unwrap(),
            "ingest",
            "note.md",
            "--link",
            "unknown",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(out.stdout.len(), 0);
    let marker = format!("{}{}", "AKIA", "QWERTYUIOPASDFGH");
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(&root)
        .env("KABAN_HOME", scratch.home())
        .args([
            "--db",
            scratch.db().to_str().unwrap(),
            "ingest",
            "note.md",
            "--link",
            &marker,
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&marker));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(&marker));
}
