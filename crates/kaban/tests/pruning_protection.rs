//! CLI `HumanCli` / MCP Agent parity on the durable public workflow.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::{Scratch, Server};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn cli(s: &Scratch, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
#[test]
fn cli_and_mcp_share_get_set_clear_and_refuse_serialized_authority() {
    let s = Scratch::new("kaban-protect", "parity");
    let out = cli(&s, &["remember", "Fact", "invented protected fact"]);
    assert!(out.status.success(), "{out:?}");
    let remembered: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = remembered["id"].as_str().unwrap();
    for (operation, expected) in [("get", false), ("set", true), ("get", true)] {
        let out = cli(&s, &["protect", operation, "--node", id]);
        assert!(out.status.success(), "{out:?}");
        let response: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(response["explicit"], expected);
    }
    let mut server = Server::start(&s);
    for (operation, expected) in [
        ("get", true),
        ("clear", false),
        ("set", true),
        ("clear", false),
    ] {
        let request=server.request("tools/call",&json!({"name":"pruning_protection","arguments":{"operation":operation,"target_kind":"node","id":id}}));
        let response = server.response(request, Duration::from_secs(10)).unwrap();
        assert_ne!(response["result"]["isError"], true, "{response}");
        assert_eq!(
            response["result"]["structuredContent"]["explicit"],
            expected
        );
    }
    for field in ["actor", "workspace_id"] {
        let mut args = json!({"operation":"set","target_kind":"node","id":id});
        args[field] = json!("foreign");
        let request = server.request(
            "tools/call",
            &json!({"name":"pruning_protection","arguments":args}),
        );
        let response = server.response(request, Duration::from_secs(10)).unwrap();
        assert!(
            response["result"]["isError"] == true || response.get("error").is_some(),
            "{response}"
        );
    }
    server.finish();
    for (raw, success) in [
        (
            json!({"operation":"set","target_kind":"node","id":id}),
            true,
        ),
        (
            json!({"operation":"clear","target_kind":"node","id":id,"actor":"agent"}),
            false,
        ),
        (
            json!({"operation":"clear","target_kind":"node","id":id}),
            true,
        ),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .args([
                "--db",
                s.db().to_str().unwrap(),
                "--json",
                "--stdin-json",
                "protect",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(raw.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.success(), success, "{out:?}");
    }
    let out = cli(&s, &["protect", "get", "--node", id]);
    assert!(
        !serde_json::from_slice::<Value>(&out.stdout).unwrap()["explicit"]
            .as_bool()
            .unwrap()
    );
}

#[cfg(feature = "ingest")]
#[derive(Debug, Clone, Copy)]
enum RetainedChildRemoval {
    Forget,
    Reject,
    Clear,
}

#[cfg(feature = "ingest")]
fn retained_children_after_safe_refresh(removal: RetainedChildRemoval) {
    let terminal = match removal {
        RetainedChildRemoval::Forget => "forget",
        RetainedChildRemoval::Reject => "reject",
        RetainedChildRemoval::Clear => "clear",
    };
    let s = Scratch::new("kaban-protect", terminal);
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("a.md"),
        "# Old invented heading\n\nOld harmless body.\n",
    )
    .unwrap();
    let run = |args: &[&str], body: Option<&Value>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "warn")
            .current_dir(&root)
            .args(["--db", s.db().to_str().unwrap(), "--json"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(body) = body {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(body.to_string().as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    run(&["ingest", "a.md"], None);
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM node WHERE kind IN ('Section','Chunk') AND lifecycle='active'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ids.len(), 2);
    for id in &ids {
        run(&["protect", "set", "--node", id], None);
    }
    std::fs::write(
        root.join("a.md"),
        "# Current invented heading\n\nCurrent harmless body.\n",
    )
    .unwrap();
    let brief = run(&["ingest", "job", "brief", "a.md"], None);
    for id in &ids {
        let state: String = conn
            .query_row("SELECT lifecycle FROM node WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(
            state, "active",
            "safe refresh must retain protected old child"
        );
        let current: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM node d,json_each(d.props,'$.section_ids') i WHERE d.kind='Document' AND i.value=?1) OR EXISTS(SELECT 1 FROM node d,json_each(d.props,'$.chunk_ids') i WHERE d.kind='Document' AND i.value=?1)",[id],|r|r.get(0)).unwrap();
        assert!(
            !current,
            "protected historical child is not current source inventory"
        );
    }
    let job = brief["job"].as_str().unwrap();
    let submit = run(
        &["ingest", "job", "submit", job, "--stdin-json"],
        Some(
            &json!({"records":[{"local_id":"r","kind":"Fact","text":"Current source-supported record"}]}),
        ),
    );
    assert_eq!(
        submit["created"], 1,
        "protected historical children must not stale current-source submission"
    );
    match removal {
        RetainedChildRemoval::Forget => {
            run(&["forget", "--document", "a.md"], None);
        }
        RetainedChildRemoval::Reject => {
            std::fs::write(root.join("a.md"), "AKIAQWERTYUIOPASDFGH\n").unwrap();
            run(&["ingest", "a.md"], None);
        }
        RetainedChildRemoval::Clear => {
            for id in &ids {
                run(&["protect", "clear", "--node", id], None);
            }
            std::fs::write(
                root.join("a.md"),
                "# Third invented heading\n\nThird harmless body.\n",
            )
            .unwrap();
            run(&["ingest", "a.md"], None);
        }
    }
    for id in ids {
        let state: String = conn
            .query_row("SELECT lifecycle FROM node WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(
            state, "tombstoned",
            "{terminal} must remove historical children retained outside current source inventory"
        );
    }
}

#[cfg(feature = "ingest")]
#[test]
fn safe_refresh_retained_children_are_removed_by_explicit_document_forget() {
    retained_children_after_safe_refresh(RetainedChildRemoval::Forget);
}

#[cfg(feature = "ingest")]
#[test]
fn safe_refresh_retained_children_are_removed_by_policy_reject() {
    retained_children_after_safe_refresh(RetainedChildRemoval::Reject);
}

#[cfg(feature = "ingest")]
#[test]
fn safe_refresh_retained_children_are_pruned_after_clear_and_next_omission() {
    retained_children_after_safe_refresh(RetainedChildRemoval::Clear);
}

#[cfg(feature = "ingest")]
#[test]
fn explicit_document_forget_overrides_protected_document_and_children() {
    let s = Scratch::new("kaban-protect", "explicit-document");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/a.md"),
        "# Invented source\n\nHarmless invented content.\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "warn")
            .current_dir(&root)
            .args(["--db", s.db().to_str().unwrap(), "--json"])
            .args(args)
            .output()
            .unwrap()
    };
    let prepared = run(&["ingest", "job", "brief", "docs/a.md"]);
    assert!(prepared.status.success(), "{prepared:?}");
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM node WHERE kind IN ('Document','Section','Chunk')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(ids.len() > 1);
    for id in &ids {
        assert!(run(&["protect", "set", "--node", id]).status.success());
    }
    let forgotten = run(&["forget", "--document", "docs/a.md"]);
    assert!(forgotten.status.success(), "{forgotten:?}");
    let active:i64=conn.query_row("SELECT count(*) FROM node WHERE kind IN ('Document','Section','Chunk') AND lifecycle='active'",[],|r|r.get(0)).unwrap();
    assert_eq!(
        active, 0,
        "explicit forget --document must override durable pruning protection on the Document and all children"
    );
    let jobs: i64 = conn
        .query_row("SELECT count(*) FROM ingest_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jobs, 0, "explicit document forget must revoke its job");
}

#[cfg(feature = "ingest")]
#[test]
fn policy_rejected_refresh_overrides_protected_old_children() {
    let s = Scratch::new("kaban-protect", "policy-refresh");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/a.md"),
        "# Invented source\n\nHarmless invented content.\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "warn")
            .current_dir(&root)
            .args(["--db", s.db().to_str().unwrap(), "--json"])
            .args(args)
            .output()
            .unwrap()
    };
    let first = run(&["ingest", "docs/a.md"]);
    assert!(first.status.success(), "{first:?}");
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM node WHERE kind IN ('Section','Chunk') AND lifecycle='active'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_ne!(ids, [] as [String; 0]);
    for id in &ids {
        assert!(run(&["protect", "set", "--node", id]).status.success());
    }
    std::fs::write(root.join("docs/a.md"), "AKIAQWERTYUIOPASDFGH\n").unwrap();
    let refreshed = run(&["ingest", "docs/a.md"]);
    assert!(refreshed.status.success(), "{refreshed:?}");
    for id in ids {
        let lifecycle: String = conn
            .query_row("SELECT lifecycle FROM node WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(
            lifecycle, "tombstoned",
            "policy-rejected refresh must hide each protected old child; new sanitized metadata remains allowed"
        );
    }
}

#[cfg(feature = "ingest")]
#[test]
fn policy_redacted_refresh_overrides_protected_old_chunk_without_blocking_file() {
    let s = Scratch::new("kaban-protect", "redacted-refresh");
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/a.md"),
        "# Invented source\n\nInvented hunter2 content.\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "warn")
            .current_dir(&root)
            .args(["--db", s.db().to_str().unwrap(), "--json"])
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&["ingest", "docs/a.md"]).status.success());
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM node WHERE kind='Chunk' AND lifecycle='active'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_ne!(ids, [] as [String; 0]);
    for id in &ids {
        assert!(run(&["protect", "set", "--node", id]).status.success());
    }
    let policy = s.0.join("redact.yaml");
    std::fs::write(
        &policy,
        "words:\n  - { term: hunter2, action: redact, id: invented-redact }\n",
    )
    .unwrap();
    let refreshed = run(&["--policy", policy.to_str().unwrap(), "ingest", "docs/a.md"]);
    assert!(refreshed.status.success(), "{refreshed:?}");
    let report: Value = serde_json::from_slice(&refreshed.stdout).unwrap();
    assert_eq!(report["blocked"], 0, "{report}");
    let unmasked:i64=conn.query_row("SELECT count(*) FROM node WHERE kind='Chunk' AND lifecycle='active' AND instr(kaban_body(text),'hunter2')>0",[],|r|r.get(0)).unwrap();
    assert_eq!(
        unmasked, 0,
        "redacted refresh must not retain protected old unmasked chunks"
    );
}
