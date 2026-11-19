//! PC-20 items 4 and the leftover list (spec 11 T3 slice B): path scoping and the governance
//! filter are gone from `traverse`. Each removed request key (`follow_anchors`, `kind_set`,
//! `governance`, and the earlier `include_superseded` and `include_candidates`) is refused with
//! an error naming the key, with no alias; a plain traversal carries no `anchored` cards, and an
//! authored `SUPERSEDES` link still archives its target. `GOVERNS` is no longer a rel.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use common::Scratch;
use mcp_client::{McpClient, Spawn};
use serde_json::json;

#[test]
fn traverse_refuses_the_removed_path_and_governance_keys_naming_each() {
    let s = Scratch::new("kaban-traverse-removed", "keys");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let mut mcp = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &root,
        policy: None,
        workspace: "default",
    });
    let decision = |mcp: &mut McpClient, text: &str| -> String {
        mcp.call(
            "remember",
            &json!({"kind": "Decision", "text": text, "props": {"rationale": "numbat sizing"},
                    "path_scope": ["src/numbat/**"]}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let old = decision(&mut mcp, "the numbat cache holds 2 GiB");
    let new = decision(&mut mcp, "the numbat cache holds 4 GiB");
    mcp.call(
        "link",
        &json!({"from": new, "to": old, "rel": "SUPERSEDES"}),
    );

    // Act + Assert: every removed key is refused naming it
    for (key, value) in [
        ("follow_anchors", json!(true)),
        ("kind_set", json!("governing")),
        ("governance", json!(["accepted"])),
        ("include_superseded", json!(true)),
        ("include_candidates", json!(true)),
    ] {
        let mut args = json!({"start_ids": [new], "direction": "both"});
        args[key] = value;
        let refused = mcp.call_err("traverse", &args);
        assert!(refused.contains(&format!("`{key}`")), "{key}: {refused}");
    }

    // the surviving traversal: no `anchored` cards; the superseded target is archived, so it
    // leaves the active walk and recall finds it only with `include_archived`
    let plain = mcp.call(
        "traverse",
        &json!({"start_ids": [new], "direction": "both"}),
    );
    assert!(plain.get("anchored").is_none(), "{plain}");
    assert_eq!(plain["nodes"].as_array().unwrap().len(), 1, "{plain}");
    let hidden = mcp.call("recall", &json!({"query": "numbat cache"}));
    assert!(
        hidden["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["id"] != json!(old)),
        "{hidden}"
    );
    let archived = mcp.call(
        "recall",
        &json!({"query": "numbat cache", "include_archived": true}),
    );
    let target = archived["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!(old))
        .unwrap_or_else(|| panic!("{archived}"));
    assert_eq!(target["lifecycle"], "archived", "{target}");

    // `GOVERNS` is gone with path scoping: linking one is an unknown rel
    let refused = mcp.call_err("link", &json!({"from": new, "to": old, "rel": "GOVERNS"}));
    assert!(refused.contains("unknown_rel"), "{refused}");
    mcp.finish();
}
