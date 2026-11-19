//! Spec 11 T3 slice B: entity resolution is gone, so the extractor return no longer carries
//! pair judgements. `ingest_submit` refuses the removed `pair_judgements` key with an error
//! naming it, with no alias and no per-pair skip row.
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: failures are the assertion

mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use common::Scratch;
use mcp_client::{McpClient, Spawn};
use serde_json::json;

#[test]
fn ingest_submit_refuses_the_removed_pair_judgements_key_naming_it() {
    // Arrange
    let s = Scratch::new("kaban-submit-removed", "keys");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let mut mcp = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &root,
        policy: None,
        workspace: "default",
    });
    let id = kaban_core::NodeId::new().to_string();

    // Act
    let refused = mcp.call_err(
        "ingest_submit",
        &json!({"job": "a".repeat(64),
                "pair_judgements": [{"pair": {"a": id, "b": id}, "same": true}]}),
    );

    // Assert
    assert!(refused.contains("`pair_judgements`"), "{refused}");
    mcp.finish();
}
