//! The fixture-derived black-box smoke of stage1-spec §8.4 ("one `code_*` call against a
//! committed fixture snapshot"; stage1-plan CG10.3, OD-G1-8): the committed
//! `fixtures/codegraph/go-drivers/index.scip` (and `index-notest.scip`) are decoded, gated and
//! published through the `kaban-index` writer into a temporary `KABAN_HOME`, the fixture
//! history is replayed into a scratch repository so `HEAD` resolves to the indexed commit by
//! sha, and `code_implementers` runs over stdio against the real `kaban` binary started in
//! that repository. No indexer runs: the test prints `fixture-derived`.
//!
//! Distinct from the no-snapshot smoke in `scripts/mcp/mcp_smoke.py` (§21 A7) and from the
//! north-star acceptance tests (CG10.2): this file asserts a property of one answer (count,
//! names, order, no path outside the fixture), never golden bytes.
//!
//! ```text
//!   fixtures/codegraph/go-drivers/repo  --copy-->  <scratch>/repo   (git replay: C1, C2 == COMMITS)
//!   index.scip + index-notest.scip  ==> decode_and_gate / admit_index / Rows::derive ==> writer::publish
//!                                   ==> <home>/codegraph/<repo_id(key, common dir)>/<C2>.cg.db
//!   kaban (cwd = <scratch>/repo, --policy examples/policy.yaml)  <== stdio (mcp_client.rs)
//!     code_implementers {Interface DatabaseDriver}  ==> MemDriver, PostgresDriver, Wrapped, Driver
//!   kaban (cwd = <scratch>/other, same key, no snapshot)         ==> the fixed `no_snapshot` text
//! ```
#![cfg(feature = "code-graph")] // the code tools exist only with the feature; the smoke is theirs
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible `fixture-derived` marker (OD-G1-8)

mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::Scratch;
use kaban_codegraph::IsolationTier;
use kaban_index::writer::{self, Commit, Meta, Provenance, Rows};
use kaban_index::{admit_index, decode_and_gate};
use kaban_policy::Policy;
use mcp_client::{McpClient, Spawn, snapshot_dir, write_codegraph_key};
use serde_json::{Value, json};

/// The go-drivers fixture's indexed commit (`fixtures/codegraph/go-drivers/COMMITS`, C2).
const GO_C2: &str = "730da842221a844e8c2c82891dda9094f6428858";
const C1_DATE: &str = "2026-10-26T08:16:02Z";
const C2_DATE: &str = "2026-10-26T09:04:36Z";

/// The fixed `no_snapshot` hint every code tool answers without a snapshot (G1-F8).
const NO_SNAPSHOT: &str =
    "no_snapshot: no code-graph snapshot for this repository and commit; run `kaban-index index`";

const WORKSPACE: &str = "code-smoke";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture_bytes(rel: &str) -> Vec<u8> {
    let path = repo_root().join("fixtures/codegraph").join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// `git <args>` in `root` under `mkfixture.sh:52-54`'s fixed identity, dates and config.
fn git(root: &Path, date: &str, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "kaban fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@kaban.invalid")
        .env("GIT_COMMITTER_NAME", "kaban fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@kaban.invalid")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse_head(root: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The committed fixture tree copied to `<dir>/<name>` and `mkfixture.sh:55-68` replayed
/// (C1 the committed tree, C2 the `mem.go` rename plus the `_ = dsn` line). Returns the root;
/// `HEAD` is C2 and equals `COMMITS`.
fn go_repo(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    copy_tree(
        &repo_root().join("fixtures/codegraph/go-drivers/repo"),
        &root,
    );
    git(&root, C1_DATE, &["init", "-q", "-b", "main"]);
    git(&root, C1_DATE, &["add", "-A"]);
    git(
        &root,
        C1_DATE,
        &["commit", "-q", "-m", "C1: drivers fixture"],
    );
    git(
        &root,
        C2_DATE,
        &["mv", "drivers/mem.go", "drivers/memory.go"],
    );
    let postgres = root.join("drivers/postgres.go");
    let src = std::fs::read_to_string(&postgres).unwrap();
    let patched = src.replace("\n\t_ = marker\n", "\n\t_ = marker\n\t_ = dsn\n");
    assert_ne!(src, patched, "the marker line is in the fixture");
    std::fs::write(&postgres, patched).unwrap();
    git(&root, C2_DATE, &["add", "-A"]);
    git(
        &root,
        C2_DATE,
        &[
            "commit",
            "-q",
            "-m",
            "C2: rename mem.go, extend PostgresDriver.Open",
        ],
    );
    assert_eq!(
        rev_parse_head(&root),
        GO_C2,
        "the scripted history reproduces C2 (COMMITS); HEAD resolves by sha"
    );
    root
}

fn policy_path() -> PathBuf {
    repo_root().join("examples/policy.yaml")
}

/// Decode, gate, admit, derive and publish `<dir>/<C2>.cg.db` from the committed index bytes
/// under `policy`, as `kaban-api/tests/code_tools.rs` `publish_go` does; `policy_version` is
/// the live policy's hash so the server reports the snapshot's policy as `current`.
fn publish_go(dir: &Path, repo_id: &str, policy: &Policy) -> PathBuf {
    let a = decode_and_gate(&fixture_bytes("go-drivers/index.scip"), policy).expect("gate A");
    let b =
        decode_and_gate(&fixture_bytes("go-drivers/index-notest.scip"), policy).expect("gate B");
    let admitted_a = admit_index(&a);
    let admitted_b = admit_index(&b);
    let rows = Rows::derive(&admitted_a, Some(&admitted_b), policy);
    let meta = Meta {
        commit: Commit::parse(GO_C2).expect("40-hex commit"),
        repo_id: repo_id.to_owned(),
        policy_version: policy.effective_hash().to_owned(),
        provenance: Provenance {
            isolation: IsolationTier::None,
            isolation_detail: "test".to_owned(),
            resources: Vec::new(),
            container_image_digest: None,
        },
    };
    writer::publish(dir, &rows, &meta, policy).expect("publish")
}

/// [`publish_go`] with the Rust producer's coverage sidecar attached (R-1: the rows a
/// `kaban-index index` run writes from `coverage.json`), so the answers carry its figures.
fn publish_go_with_coverage(dir: &Path, repo_id: &str, policy: &Policy) -> PathBuf {
    let a = decode_and_gate(&fixture_bytes("go-drivers/index.scip"), policy).expect("gate A");
    let admitted_a = admit_index(&a);
    let mut rows = Rows::derive(&admitted_a, None, policy);
    rows.set_coverage(writer::Coverage::parse(COVERAGE_JSON.as_bytes()).expect("sidecar"));
    let meta = Meta {
        commit: Commit::parse(GO_C2).expect("40-hex commit"),
        repo_id: repo_id.to_owned(),
        policy_version: policy.effective_hash().to_owned(),
        provenance: Provenance {
            isolation: IsolationTier::None,
            isolation_detail: "test".to_owned(),
            resources: Vec::new(),
            container_image_digest: None,
        },
    };
    writer::publish(dir, &rows, &meta, policy).expect("publish")
}

/// A `coverage.json` as the Rust producer writes it (every counter and the per-crate table).
const COVERAGE_JSON: &str = r#"{
  "targets_skipped": 3, "proc_macros_not_built": 1, "contexts": 277, "declarations": 61569,
  "block_items_skipped": 28242, "block_item_references": 41, "shared_file_contexts_skipped": 352,
  "incomplete_modules": 9, "divergent_bodies": 1, "cfg_divergent_declarations": 4,
  "tokens_without_symbol": 405, "positions_skipped": 2, "scip_bytes": 95184043,
  "block_items_unique": 9000, "block_item_lines": 31000,
  "per_crate": [
    {"crate": "kaban-api", "declarations": 100, "block_items_unique": 10, "block_item_lines": 40},
    {"crate": "kaban-index", "declarations": 200, "block_items_unique": 20, "block_item_lines": 80}
  ]
}"#;

fn implementers_of_database_driver(c: &mut McpClient) -> Value {
    c.call_raw(
        "code_implementers",
        &json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
    )
}

fn names(body: &Value) -> Vec<String> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Positive: the snapshot derived from the committed index answers `code_implementers` of
/// the `DatabaseDriver` interface over stdio with the fixture's four implementers, in a
/// deterministic order, with every path inside the fixture tree.
#[cfg(feature = "code-graph")]
#[test]
fn code_implementers_over_stdio_fixture_derived() {
    eprintln!(
        "code_implementers_over_stdio_fixture_derived: fixture-derived (committed go-drivers/index.scip and index-notest.scip through the kaban-index writer); no indexer run"
    );
    let s = Scratch::new("kaban-code-smoke", "implementers");
    let repo = go_repo(&s.0, "repo");
    let policy = Policy::load(&policy_path()).expect("examples/policy.yaml loads");
    let key = write_codegraph_key(&s.home());
    let dir = snapshot_dir(&s.home(), &key, &repo);
    let repo_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let snapshot = publish_go(&dir, &repo_id, &policy);
    assert_eq!(
        snapshot.file_name().unwrap().to_str().unwrap(),
        format!("{GO_C2}.cg.db")
    );

    let policy_file = policy_path();
    let mut c = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &repo,
        policy: Some(&policy_file),
        workspace: WORKSPACE,
    });
    let first = implementers_of_database_driver(&mut c);
    assert_ne!(first["result"]["isError"], json!(true), "{first}");
    let body: Value =
        serde_json::from_str(first["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let got = names(&body);
    assert!(body["count"].as_u64().unwrap() >= 3, "count >= 3: {body}");
    assert_eq!(
        usize::try_from(body["count"].as_u64().unwrap()).unwrap(),
        got.len(),
        "{body}"
    );
    // the fixture's implementers (kaban-api `tool_code_implementers_golden_go_drivers`):
    // `MemDriver`, `PostgresDriver`, `Wrapped` and the sqlite package's `Driver`; `HttpDriver`
    // is declared in the tree but implements nothing
    for want in ["MemDriver", "PostgresDriver", "Wrapped"] {
        assert!(
            got.contains(&want.to_owned()),
            "{want} missing from {got:?}"
        );
    }
    assert!(
        !got.contains(&"HttpDriver".to_owned()),
        "HttpDriver is not an implementer: {got:?}"
    );
    assert_eq!(body["snapshot"]["commit"], json!(GO_C2), "{body}");

    // deterministic order: the same call answers the same list
    let second = implementers_of_database_driver(&mut c);
    let body2: Value =
        serde_json::from_str(second["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(names(&body2), got, "the order changed between two calls");

    // every path is a relative fixture path; no answer names the scratch directory
    for node in body["results"].as_array().unwrap() {
        let path = node["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with('/') && !path.contains(".."),
            "path outside the fixture: {path}"
        );
        assert!(
            repo.join(path).is_file(),
            "{path} is not a file of the fixture tree"
        );
    }
    let scratch = s.0.to_string_lossy();
    let done = c.finish();
    assert!(done.status.success(), "{:?}", done.status);
    assert!(
        !done.stdout.contains(&*scratch),
        "stdout named the scratch path:\n{}",
        done.stdout
    );
    assert!(
        !done.stderr.contains(&*scratch),
        "the trace named the scratch path:\n{}",
        done.stderr
    );
}

/// Negative: the same call from a server whose working directory is a repository with no
/// snapshot (same key, a different repository id, no `codegraph/<repo-id>/` directory) is the
/// fixed `no_snapshot` text, and neither the reply nor the trace names a path.
#[cfg(feature = "code-graph")]
#[test]
fn code_implementers_without_snapshot_is_no_snapshot() {
    let s = Scratch::new("kaban-code-smoke", "no-snapshot");
    let repo = go_repo(&s.0, "other");
    let _key = write_codegraph_key(&s.home());
    let policy_file = policy_path();
    let mut c = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &repo,
        policy: Some(&policy_file),
        workspace: WORKSPACE,
    });
    let text = c.call_err(
        "code_implementers",
        &json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
    );
    assert_eq!(text, NO_SNAPSHOT);
    let scratch = s.0.to_string_lossy();
    let done = c.finish();
    assert!(
        !done.stdout.contains(&*scratch),
        "stdout named the scratch path:\n{}",
        done.stdout
    );
    assert!(
        !done.stderr.contains(&*scratch),
        "the trace named the scratch path:\n{}",
        done.stderr
    );
}

/// R-1 Task 3: every `code_*` answer carries the compact `coverage` field and the
/// `kaban://code-index` resource the whole table. With the producer's rows in the snapshot the
/// figures and the one-sentence note are there; a snapshot without them (Go) answers all-zero
/// with no note, never an error.
#[cfg(feature = "code-graph")]
#[test]
fn coverage_in_every_answer_and_in_the_index_resource_fixture_derived() {
    eprintln!(
        "coverage_in_every_answer_and_in_the_index_resource_fixture_derived: fixture-derived (go-drivers/index.scip through the kaban-index writer, coverage rows attached); no indexer run"
    );
    let s = Scratch::new("kaban-code-smoke", "coverage");
    let policy = Policy::load(&policy_path()).expect("examples/policy.yaml loads");
    let policy_file = policy_path();
    let key = write_codegraph_key(&s.home());

    // with the producer's rows
    let repo = go_repo(&s.0, "repo");
    let dir = snapshot_dir(&s.home(), &key, &repo);
    let repo_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    publish_go_with_coverage(&dir, &repo_id, &policy);
    let mut c = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &repo,
        policy: Some(&policy_file),
        workspace: WORKSPACE,
    });
    let calls = [
        (
            "code_implementers",
            json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
        ),
        (
            "code_definition",
            json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
        ),
        (
            "code_references",
            json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
        ),
        (
            "code_members",
            json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
        ),
        (
            "code_tests_for",
            json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
        ),
        (
            "code_neighbours",
            json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
        ),
        (
            "code_context",
            json!({"target": {"label": "Interface", "name": "DatabaseDriver"}}),
        ),
    ];
    for (tool, args) in &calls {
        let raw = c.call_raw(tool, args);
        assert_ne!(raw["result"]["isError"], json!(true), "{tool}: {raw}");
        let body: Value =
            serde_json::from_str(raw["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let coverage = &body["coverage"];
        assert_eq!(coverage["declarations"], json!(61569), "{tool}: {body}");
        assert_eq!(
            coverage["block_items_unique"],
            json!(9000),
            "{tool}: {body}"
        );
        assert_eq!(coverage["incomplete_modules"], json!(9), "{tool}: {body}");
        assert_eq!(coverage["skipped_targets"], json!(3), "{tool}: {body}");
        let note = coverage["note"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: {body}"));
        assert!(
            note.contains("9000") && note.contains('3') && note.contains("does not prove absence"),
            "{tool}: {note}"
        );
        assert_eq!(note.matches(". ").count(), 0, "one sentence: {note}");
    }
    let id = c.request("resources/read", &json!({"uri": "kaban://code-index"}));
    let resp = c.response(id, Duration::from_secs(10)).unwrap();
    let index: Value =
        serde_json::from_str(resp["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    let coverage = &index["coverage"];
    assert_eq!(coverage["block_items_skipped"], json!(28242), "{index}");
    assert_eq!(coverage["block_item_lines"], json!(31000), "{index}");
    assert_eq!(coverage["scip_bytes"], json!(95_184_043), "{index}");
    assert_eq!(
        coverage["per_crate"],
        json!([
            {"crate": "kaban-api", "declarations": 100, "block_items_unique": 10, "block_item_lines": 40},
            {"crate": "kaban-index", "declarations": 200, "block_items_unique": 20, "block_item_lines": 80}
        ]),
        "{index}"
    );
    let done = c.finish();
    assert!(done.status.success(), "{:?}", done.status);

    // a Go snapshot without the rows: all-zero, no note, and the resource carries no table
    let other = go_repo(&s.0, "other");
    let dir = snapshot_dir(&s.home(), &key, &other);
    let repo_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    publish_go(&dir, &repo_id, &policy);
    let mut c = McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: &other,
        policy: Some(&policy_file),
        workspace: WORKSPACE,
    });
    let body: Value = serde_json::from_str(
        implementers_of_database_driver(&mut c)["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["coverage"],
        json!({"declarations": 0, "block_items_unique": 0, "incomplete_modules": 0, "skipped_targets": 0}),
        "{body}"
    );
    let id = c.request("resources/read", &json!({"uri": "kaban://code-index"}));
    let resp = c.response(id, Duration::from_secs(10)).unwrap();
    let index: Value =
        serde_json::from_str(resp["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert!(index["coverage"].is_null(), "{index}");
    c.finish();
}
