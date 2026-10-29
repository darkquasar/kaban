//! Black-box text index maintenance command (`kaban reindex text`) and argument rejection.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test assertions
mod common;
use common::Scratch;
use kaban_core::{Actor, MemoryStore, NewNode, RecallQuery, WorkspaceId, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use std::process::Command;

#[test]
fn reindex_text_backfills_and_repeated_pass_preserves_content() {
    let scratch = Scratch::new("e33-cli", "reindex");
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "LegacyReader".into(),
                ..Default::default()
            },
            &[],
            &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
        )
        .unwrap();
    drop(store);
    // the index beside the database is removed: `reindex text` rebuilds it from the rows
    std::fs::remove_dir_all(kaban_core::text_index::index_dir(&scratch.db())).unwrap();
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", scratch.home())
            .args([
                "--json",
                "--db",
                scratch.db().to_str().unwrap(),
                "reindex",
                "text",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output["rows"], 1);
    }
    let store = SqliteStore::open(&scratch.db(), &scratch.home().join("keys")).unwrap();
    let hits = store
        .recall(&RecallQuery {
            query: "legacy".into(),
            hops: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.text, "LegacyReader");
}

#[test]
fn reindex_rejects_unknown_arm_and_flag() {
    for args in [["reindex", "unknown"], ["reindex", "--unknown"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(output.stdout, [] as [u8; 0]);
    }
}
