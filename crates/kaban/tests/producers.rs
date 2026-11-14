//! PC-16 (spec 11 T3 Step 2c): producers replace the L0 to L3 matrix. After `remember` plus
//! `link`, a document ingest and a gazetteer run over the ingested chunks, `stats.producers`
//! lists the seven producers with their on/off state and budgets (no `layers` field);
//! `traverse` with `edge_filter.producers: ["authored"]` returns the link but no `MENTIONS`
//! edge; `recall` accepts `modes: ["fuzzy"]` and refuses the old `layers` with an error naming
//! the key (so is the removed `include_superseded`); a `config.yaml` using `[l3]` is refused
//! naming the key (PC-5 amended 2026-11-14).

#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use common::Scratch;
use mcp_client::{McpClient, Spawn};
use serde_json::{Value, json};
use std::process::Command;

fn kaban(s: &Scratch, root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(root)
        .env("KABAN_HOME", s.home())
        .env("KABAN_WORKSPACE", "default")
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .output()
        .unwrap()
}

fn json_of(out: &std::process::Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn producers_replace_the_layer_matrix_on_every_surface() {
    let s = Scratch::new("kaban-producers", "surfaces");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("note.md"),
        "Public Cart Architecture describes the system.",
    )
    .unwrap();
    let target = json_of(&kaban(
        &s,
        &root,
        &["remember", "Fact", "Public Cart Architecture"],
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = json_of(&kaban(&s, &root, &["remember", "Fact", "A related fact"]))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    json_of(&kaban(
        &s,
        &root,
        &["ingest", "note.md", "--link", "gazetteer"],
    ));

    let mut mcp = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &root,
        policy: None,
        workspace: "default",
    });
    mcp.call(
        "link",
        &json!({"from": target, "to": other, "rel": "RELATES_TO"}),
    );

    // stats: seven producers, each with its state and budgets; no `layers`
    let stats = mcp.call("stats", &json!({}));
    assert!(stats.get("layers").is_none(), "{stats}");
    let producers = stats["producers"].as_array().unwrap();
    let names: Vec<&str> = producers
        .iter()
        .map(|p| p["producer"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "authored",
            "hook_events",
            "document_ingest",
            "gazetteer",
            "extractor",
            "scip_import",
            "git_history"
        ],
        "{stats}"
    );
    for row in producers {
        assert!(row["on"].is_boolean(), "{row}");
        assert!(row["budgets"].is_array(), "{row}");
    }

    // the gazetteer wrote a MENTIONS edge into the target; the authored filter drops it
    let all = mcp.call(
        "traverse",
        &json!({"start_ids": [target], "direction": "both", "max_depth": 1}),
    );
    let rels = |v: &Value| -> Vec<String> {
        v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["rel"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(rels(&all).contains(&"MENTIONS".to_owned()), "{all}");
    let authored = mcp.call(
        "traverse",
        &json!({"start_ids": [target], "direction": "both", "max_depth": 1,
                "edge_filter": {"producers": ["authored"]}}),
    );
    assert_eq!(rels(&authored), ["RELATES_TO"], "{authored}");

    // recall: `modes` is the name, `layers` is refused naming the key
    let fuzzy = mcp.call(
        "recall",
        &json!({"query": "Public Cart", "modes": ["fuzzy"]}),
    );
    assert!(fuzzy["results"].is_array(), "{fuzzy}");
    let refused = mcp.call_err(
        "recall",
        &json!({"query": "Public Cart", "layers": ["fuzzy"]}),
    );
    assert!(refused.contains("layers"), "{refused}");
    // `include_superseded` is gone too (it equalled `include_archived`; no alias)
    let refused = mcp.call_err(
        "recall",
        &json!({"query": "Public Cart", "include_superseded": true}),
    );
    assert!(refused.contains("include_superseded"), "{refused}");
    mcp.finish();

    // config: `[l3]` is refused naming the key
    std::fs::write(
        s.home().join("config.yaml"),
        "l3:\n  holdout_min_f1: 0.4242\n",
    )
    .unwrap();
    let out = kaban(&s, &root, &["recall", "anything"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("config key invalid: l3"), "{stderr}");
    assert!(!stderr.contains("0.4242"), "{stderr}");
}
