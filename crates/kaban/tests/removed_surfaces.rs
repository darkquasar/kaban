//! The governance surfaces are gone from the public interface (spec 11 DEL-01, OD-2, OD-6):
//! `kaban --help` lists none of the removed verbs or global flags, each removed verb is an
//! unrecognized subcommand (the fixed parse-failure text, exit 4), and `tools/list` carries
//! none of the five removed tools. The remaining verbs and tools are still there.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::process::{Command, Output};
use std::time::Duration;

/// Every CLI verb removed with governance, the git renderer and packs.
const REMOVED_VERBS: [&str; 16] = [
    "why",
    "explain",
    "accept",
    "review",
    "protect",
    "unmerge",
    "governance-history",
    "promote",
    "ingest-decisions",
    "resolve",
    "verify-audit",
    "sync",
    "serialize",
    "export",
    "import",
    "pack",
];

/// Global flags removed with the audit export and packs.
const REMOVED_FLAGS: [&str; 2] = ["--pack", "--audit-export"];

/// Verbs that stay (a regression guard against deleting too much).
const KEPT_VERBS: [&str; 11] = [
    "stats", "reindex", "serve", "bench", "scan", "rescan", "remember", "recall", "forget",
    "doctor", "ontology",
];

/// The MCP tools removed with governance.
const REMOVED_TOOLS: [&str; 5] = [
    "accept",
    "explain",
    "why",
    "governance_history",
    "pruning_protection",
];

/// Core tools that stay.
const KEPT_TOOLS: [&str; 8] = [
    "remember",
    "recall",
    "forget",
    "link",
    "traverse",
    "stats",
    "schema_describe",
    "schema_extend",
];

/// Run the binary in the scratch directory (outside any repository, so nothing can touch
/// a checkout) with a private home and database.
fn kaban(s: &Scratch, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(&s.0)
        .env("KABAN_HOME", s.home())
        .env_remove("KABAN_DB")
        .env_remove("KABAN_AUDIT_EXPORT")
        .args(["--db", s.db().to_str().unwrap()])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// The subcommand names of the `Commands:` section of `kaban --help`.
fn help_commands(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect()
}

#[test]
fn help_lists_none_of_the_removed_verbs_or_flags() {
    let s = Scratch::new("kaban-removed", "help");
    let out = kaban(&s, &["--help"]);
    assert!(out.status.success(), "--help exits 0: {:?}", out.status);
    let help = String::from_utf8(out.stdout).unwrap();
    let commands = help_commands(&help);
    let listed: Vec<&str> = REMOVED_VERBS
        .iter()
        .copied()
        .filter(|v| commands.iter().any(|c| c == v))
        .collect();
    assert!(
        listed.is_empty(),
        "removed verbs still in --help: {listed:?}"
    );
    let flags: Vec<&str> = REMOVED_FLAGS
        .iter()
        .copied()
        .filter(|f| help.contains(f))
        .collect();
    assert!(flags.is_empty(), "removed flags still in --help: {flags:?}");
    let missing: Vec<&str> = KEPT_VERBS
        .iter()
        .copied()
        .filter(|v| !commands.iter().any(|c| c == v))
        .collect();
    assert!(
        missing.is_empty(),
        "kept verbs missing from --help: {missing:?}"
    );
}

#[test]
fn every_removed_verb_is_an_unrecognized_subcommand() {
    let s = Scratch::new("kaban-removed", "verbs");
    let mut recognized = Vec::new();
    for verb in REMOVED_VERBS {
        let out = kaban(&s, &[verb]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.code() != Some(4) || !stderr.contains("the subcommand is not recognized") {
            recognized.push(verb);
        }
    }
    assert!(
        recognized.is_empty(),
        "removed verbs still recognized: {recognized:?}"
    );
}

#[test]
fn removed_global_flags_are_unknown_arguments() {
    let s = Scratch::new("kaban-removed", "flags");
    let mut accepted = Vec::new();
    for flag in REMOVED_FLAGS {
        let out = kaban(&s, &[flag, "x", "stats"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.code() != Some(4) || !stderr.contains("an argument is not recognized") {
            accepted.push(flag);
        }
    }
    assert!(
        accepted.is_empty(),
        "removed flags still accepted: {accepted:?}"
    );
}

#[test]
fn tools_list_carries_none_of_the_removed_tools() {
    let s = Scratch::new("kaban-removed", "tools");
    let mut srv = Server::start(&s);
    let id = srv.request("tools/list", &json!({}));
    let resp = srv.response(id, Duration::from_secs(10)).unwrap();
    let names: Vec<String> = resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    let present: Vec<&str> = REMOVED_TOOLS
        .iter()
        .copied()
        .filter(|t| names.iter().any(|n| n == t))
        .collect();
    assert!(
        present.is_empty(),
        "removed tools still listed: {present:?}"
    );
    let missing: Vec<&str> = KEPT_TOOLS
        .iter()
        .copied()
        .filter(|t| !names.iter().any(|n| n == t))
        .collect();
    assert!(missing.is_empty(), "kept tools missing: {missing:?}");
    // a removed tool is now an unknown tool: the fixed JSON-RPC error, no echo
    let id = srv.request(
        "tools/call",
        &json!({"name": "why", "arguments": {"targets": [{"path": "a.rs"}]}}),
    );
    let resp: Value = srv.response(id, Duration::from_secs(10)).unwrap();
    assert_eq!(
        resp["error"]["message"],
        json!("tool not found"),
        "a removed tool is unknown: {resp}"
    );
    let _ = srv.finish();
}
