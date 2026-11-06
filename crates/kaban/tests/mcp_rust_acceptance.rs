//! RI-3 Task 2: the public `code_*` tools over snapshots the installed Rust producer published
//! through the `kaban-index` binary, queried through the real `kaban` MCP server over stdio
//! (modelled on `walkthrough.rs`). Nothing here is fixture-derived: the snapshot comes from a
//! real `kaban-index index --lang rust` run, so the test is `#[ignore]` and prints a visible
//! `NOT RUN` marker without the producer, the real toolchain or a built `kaban-index`.
//!
//! ```text
//!   fixtures/codegraph/rust-qualification/<fixture> --copy--> <scratch>/<fixture> (one commit)
//!   kaban-index trust / index --lang rust --indexer rust=<producer> --isolation off
//!     ==> <home>/codegraph/<repo-id>/<commit40>.cg.db        (KABAN_HOME = <scratch>/home)
//!   kaban serve (cwd = <scratch>/<fixture>, same KABAN_HOME) <== stdio
//!     workspace: code_definition(answer), code_references(generated_value)
//!     minimal:   code_implementers(DatabaseDriver), code_tests_for(production),
//!                code_references(production) with include_tests false and true
//! ```

#![cfg(feature = "code-graph")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible `NOT RUN` marker and the latency line

mod common;
#[path = "common/mcp_client.rs"]
mod mcp_client;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::Scratch;
use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
use mcp_client::{McpClient, Spawn};
use serde_json::{Value, json};

/// The explicit trusted inputs (`kaban-index/tests/runner.rs` env names) and the built
/// `kaban-index` binary (`KABAN_TEST_INDEX_BIN`, else the sibling of this crate's binary).
struct Inputs {
    producer: PathBuf,
    sysroot: PathBuf,
    index_bin: PathBuf,
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn inputs() -> Option<Inputs> {
    let from_env = |var: &str, probe: &str| {
        let path = std::env::var_os(var).map(PathBuf::from)?;
        let target = if probe.is_empty() {
            path.clone()
        } else {
            path.join(probe)
        };
        executable(&target).then_some(path)
    };
    let Some(producer) = from_env("KABAN_TEST_RUST_PRODUCER", "") else {
        eprintln!(
            "NOT RUN: KABAN_TEST_RUST_PRODUCER is unset or not the installed kaban-rust-scip"
        );
        return None;
    };
    let Some(sysroot) = from_env("KABAN_TEST_RUST_SYSROOT", "bin/rustc") else {
        eprintln!(
            "NOT RUN: KABAN_TEST_RUST_SYSROOT is unset or not the real 1.99.0 toolchain root"
        );
        return None;
    };
    let index_bin = std::env::var_os("KABAN_TEST_INDEX_BIN").map_or_else(
        || Path::new(env!("CARGO_BIN_EXE_kaban")).with_file_name("kaban-index"),
        PathBuf::from,
    );
    if !executable(&index_bin) {
        eprintln!(
            "NOT RUN: no kaban-index binary at {} (build it, or set KABAN_TEST_INDEX_BIN)",
            index_bin.display()
        );
        return None;
    }
    Some(Inputs {
        producer,
        sysroot,
        index_bin,
    })
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codegraph/rust-qualification")
}

/// Copy an unchanged trusted fixture tree (no symlinks) into `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(!kind.is_symlink());
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "kaban fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@kaban.invalid")
        .env("GIT_COMMITTER_NAME", "kaban fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@kaban.invalid")
        .env("GIT_AUTHOR_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_COMMITTER_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The numbers of the descriptors this process holds without `FD_CLOEXEC` (above 2), inherited
/// from an ancestor (nextest under `flock(1)` leaks its lock fd): closed in the shell before
/// `kaban-index` starts, so its `Refuse` descriptor policy holds (`kaban-index/tests/cli.rs`).
fn non_cloexec_fds() -> Vec<u32> {
    kaban_core::process::inheritable_fds().expect("descriptor observation is required")
}

/// `kaban-index <args>` with the real toolchain first on `PATH` and `KABAN_HOME = <home>`, in
/// a child holding only the standard descriptors.
fn kaban_index(inputs: &Inputs, home: &Path, args: &[&str]) -> Output {
    let path = format!("{}:/usr/bin:/bin", inputs.sysroot.join("bin").display());
    let closes = non_cloexec_fds().iter().fold(String::new(), |mut acc, n| {
        let _ = write!(acc, " {n}>&-");
        acc
    });
    Command::new("/bin/bash")
        .arg("-c")
        .arg(format!("exec{closes}\nexec \"$0\" \"$@\"\n"))
        .arg(&inputs.index_bin)
        .args(args)
        .env("KABAN_HOME", home)
        .env("PATH", path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn json(out: &Output, what: &str) -> Value {
    assert!(
        out.status.success(),
        "{what}: {}\n--- stderr\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// A published fixture: its one-commit repository (the server's cwd) and its snapshot.
struct Published {
    repo: PathBuf,
    snapshot: PathBuf,
}

/// Copy `fixture` into a fresh repository, trust it and publish its Rust snapshot into
/// `<home>/codegraph/<repo-id>` exactly as `kaban-index/tests/cli.rs` does (isolation off:
/// the tier matrix is Task 1's).
fn publish(inputs: &Inputs, s: &Scratch, fixture: &str) -> Published {
    let home = s.home();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let repo = s.0.join(fixture);
    copy_tree(&fixtures_dir().join(fixture), &repo);
    let repo = std::fs::canonicalize(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "fixture"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let repo_s = repo.to_str().unwrap();
    let trusted = json(
        &kaban_index(inputs, &home, &["trust", "--repo", repo_s]),
        "trust",
    );
    let repo_id = trusted["repo_id"].as_str().unwrap().to_owned();
    let override_arg = format!("rust={}", inputs.producer.display());
    let report = json(
        &kaban_index(
            inputs,
            &home,
            &[
                "index",
                "--repo",
                repo_s,
                "--lang",
                "rust",
                "--indexer",
                &override_arg,
                "--isolation",
                "off",
            ],
        ),
        "index",
    );
    assert_eq!(report["languages"], json!(["rust"]), "{fixture}: {report}");
    let snapshot = home
        .join("codegraph")
        .join(repo_id)
        .join(format!("{commit}.cg.db"));
    assert!(snapshot.is_file(), "{fixture}: {}", snapshot.display());
    Published { repo, snapshot }
}

/// The SCIP label the snapshot gives the one node named `name` in `path`: the `{label, name}`
/// selector of the public tools.
fn label_of(snapshot: &Path, path: &str, name: &str) -> String {
    let snapshot = SqliteSnapshot::open_ro(snapshot).unwrap();
    let nodes = snapshot.nodes_in_file(path).unwrap();
    let mut labels: Vec<String> = nodes
        .iter()
        .filter(|node| node.name == name)
        .map(|node| node.label.clone())
        .collect();
    labels.dedup();
    assert_eq!(labels.len(), 1, "{path} {name}: {labels:?}");
    labels.remove(0)
}

fn start(s: &Scratch, repo: &Path) -> McpClient {
    McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: repo,
        policy: None,
        workspace: "default",
        audit_export: None,
    })
}

/// One timed `tools/call`.
fn call(c: &mut McpClient, latencies: &mut Vec<Duration>, tool: &str, args: &Value) -> Value {
    let started = Instant::now();
    let result = c.call(tool, args);
    latencies.push(started.elapsed());
    result
}

fn names<'a>(cards: impl Iterator<Item = &'a Value>) -> BTreeSet<String> {
    cards
        .map(|card| card["name"].as_str().unwrap().to_owned())
        .collect()
}

/// One reference site of the `include_tests` check: (enclosing name, enclosing `is_test`, site
/// path, site start line).
type Site = (String, bool, String, u64);

#[test]
#[ignore = "requires the installed Rust producer, the real toolchain and a built kaban-index"]
fn rust_snapshot_public_queries_through_the_mcp_server() {
    let Some(inputs) = inputs() else {
        return;
    };
    let s = Scratch::new("kaban-rust-mcp", "queries");
    let mut latencies = Vec::new();

    // workspace: the definition card of `answer`, the references of the macro-generated
    // `generated_value` (enclosed by `answer`)
    let workspace = publish(&inputs, &s, "workspace");
    let answer = label_of(&workspace.snapshot, "app/src/lib.rs", "answer");
    let generated = label_of(&workspace.snapshot, "app/src/lib.rs", "generated_value");
    let mut c = start(&s, &workspace.repo);
    let definition = call(
        &mut c,
        &mut latencies,
        "code_definition",
        &json!({"target": {"label": answer, "name": "answer"}}),
    );
    assert_eq!(definition["definition"]["name"], "answer", "{definition}");
    assert_eq!(
        definition["definition"]["path"], "app/src/lib.rs",
        "{definition}"
    );
    assert_eq!(definition["definition"]["is_test"], false, "{definition}");
    let references = call(
        &mut c,
        &mut latencies,
        "code_references",
        &json!({"target": {"label": generated, "name": "generated_value"}, "include_tests": false}),
    );
    let enclosing = names(
        references["by_enclosing"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| &group["enclosing"]),
    );
    assert!(enclosing.contains("answer"), "{references}");
    let finished = c.finish();
    assert!(finished.status.success(), "{}", finished.stderr);

    // minimal: the two implementers of the trait, the tests of `production`, and the
    // `include_tests` filter on its references
    let minimal = publish(&inputs, &s, "minimal");
    let trait_label = label_of(&minimal.snapshot, "src/lib.rs", "DatabaseDriver");
    let production = label_of(&minimal.snapshot, "src/lib.rs", "production");
    let mut c = start(&s, &minimal.repo);
    let implementers = call(
        &mut c,
        &mut latencies,
        "code_implementers",
        &json!({"target": {"label": trait_label, "name": "DatabaseDriver"}}),
    );
    assert_eq!(
        names(implementers["results"].as_array().unwrap().iter()),
        ["Postgres", "Sqlite"].map(str::to_owned).into(),
        "{implementers}"
    );
    let tests = call(
        &mut c,
        &mut latencies,
        "code_tests_for",
        &json!({"target": {"label": production, "name": "production"}}),
    );
    let test_names = names(
        tests["tests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| &link["test"]),
    );
    for expected in ["production_works", "integration_works"] {
        assert!(test_names.contains(expected), "{tests}");
    }
    // the reference sites of `production` as (enclosing name, enclosing is_test, site path,
    // site start line), from the fixture source (0-based lines): the production site in
    // `Postgres::execute` under both settings; the inline `tests::production_works` site and
    // the `tests/integration.rs` site only with `include_tests` (ri3-4: inline test scopes in
    // a production file are filtered, not only test files)
    let site = |enclosing: &str, is_test: bool, path: &str, line: u64| -> Site {
        (enclosing.to_owned(), is_test, path.to_owned(), line)
    };
    let production_site = site("execute", false, "src/lib.rs", 13);
    let inline_test_site = site("production_works", true, "src/lib.rs", 54);
    let integration_site = site("integration_works", true, "tests/integration.rs", 2);
    for (include_tests, expected) in [
        (false, vec![production_site.clone()]),
        (
            true,
            vec![production_site, inline_test_site, integration_site],
        ),
    ] {
        let references = call(
            &mut c,
            &mut latencies,
            "code_references",
            &json!({"target": {"label": production, "name": "production"}, "include_tests": include_tests}),
        );
        let sites: BTreeSet<Site> = references["by_enclosing"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| {
                let enclosing = &group["enclosing"];
                group["sites"].as_array().unwrap().iter().map(move |s| {
                    site(
                        enclosing["name"].as_str().unwrap(),
                        enclosing["is_test"].as_bool().unwrap(),
                        s["path"].as_str().unwrap(),
                        s["site"]["start_line"].as_u64().unwrap(),
                    )
                })
            })
            .collect();
        let expected: BTreeSet<Site> = expected.into_iter().collect();
        assert_eq!(
            sites, expected,
            "include_tests {include_tests}: {references}"
        );
        assert_eq!(
            references["count"].as_u64().unwrap(),
            expected.len() as u64,
            "include_tests {include_tests}: {references}"
        );
    }
    let finished = c.finish();
    assert!(finished.status.success(), "{}", finished.stderr);

    latencies.sort();
    eprintln!(
        "rust mcp acceptance: {} calls, median {} ms, max {} ms",
        latencies.len(),
        latencies[latencies.len() / 2].as_millis(),
        latencies[latencies.len() - 1].as_millis()
    );
}
