//! An older-format project is refused by every entry point and left byte-for-byte as it was
//! (spec 11 CMP-01, OD-1; unified-db-plan Task 4 Step 1). The fixtures are real v3.0.0
//! databases (schema version 13) written by the c15cd88 binary
//! (`crates/kaban-store-sqlite/fixtures/README.md`): `v3.0.0/` closed cleanly (`kaban.db`
//! only) and `v3.0.0-wal/` from a killed writer (`kaban.db` plus a non-empty `kaban.db-wal`).
//!
//! Each case copies one fixture directory into a scratch project, runs one entry point on it
//! (`stats`, `remember`, the `session-start` hook, `serve` with one `tools/list` round trip,
//! `SqliteStore::open`, `doctor`) and then compares the project directory with the copy taken
//! before: the same names and the same sha256 for every file, so no `-wal`, `-shm`, `.owner`
//! or text index appears and no byte changes. The audit-key home lives outside the project.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{ActorId, StoreError};
use kaban_store_sqlite::SqliteStore;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The version the fixtures carry and the one this kaban serves.
const FOUND: u32 = 13;
const SUPPORTED: u32 = 14;
/// The way out the refusal names (OD-1: no upgrade path).
const NO_UPGRADE: &str = "no upgrade path exists; create a fresh project and re-ingest";

/// The fixture files and their recorded sha256 (`fixtures/README.md`).
const CLEAN: &[(&str, &str)] = &[(
    "kaban.db",
    "74a204dd654b923059c32843e7ace21b656cdd50375ba13a2bb299d22bedc23c",
)];
const WAL: &[(&str, &str)] = &[
    (
        "kaban.db",
        "74a204dd654b923059c32843e7ace21b656cdd50375ba13a2bb299d22bedc23c",
    ),
    (
        "kaban.db-wal",
        "9c1c7343b524ca4ad09a8885f831a59539b165dae9110771eb7d0f042dc97b7a",
    ),
];

fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-store-sqlite/fixtures")
        .join(name)
}

fn sha256(path: &Path) -> String {
    let digest = Sha256::digest(std::fs::read(path).unwrap());
    digest.iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

/// Every entry of `dir` by name: a file's sha256, or `dir` for a directory.
fn listing(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let kind = if entry.file_type().unwrap().is_dir() {
                "dir".to_owned()
            } else {
                sha256(&entry.path())
            };
            (name, kind)
        })
        .collect()
}

/// A scratch project holding a copy of one fixture, and an audit-key home beside it.
struct Project {
    scratch: Scratch,
    before: BTreeMap<String, String>,
}

impl Project {
    fn copy(fixture: &str, files: &[(&str, &str)], case: &str) -> Self {
        let source = fixture_dir(fixture);
        let expected: BTreeMap<String, String> = files
            .iter()
            .map(|(name, sha)| ((*name).to_owned(), (*sha).to_owned()))
            .collect();
        assert_eq!(
            listing(&source),
            expected,
            "the fixture {fixture} is not the one fixtures/README.md records"
        );
        let scratch = Scratch::new("kaban-older-format", &format!("{fixture}-{case}"));
        let project = scratch.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(scratch.home()).unwrap();
        for (name, _) in files {
            std::fs::copy(source.join(name), project.join(name)).unwrap();
        }
        let before = listing(&project);
        assert_eq!(before, expected, "the copy matches the fixture");
        Self { scratch, before }
    }

    fn dir(&self) -> PathBuf {
        self.scratch.0.join("project")
    }

    fn db(&self) -> PathBuf {
        self.dir().join("kaban.db")
    }

    fn home(&self) -> PathBuf {
        self.scratch.home()
    }

    /// `kaban --json --db <project db> <args..>` with the private home, outside any repository.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
        cmd.current_dir(self.home())
            .env("KABAN_HOME", self.home())
            .env("KABAN_WORKSPACE", "fixture")
            .env_remove("RUST_LOG")
            .args(["--json", "--db", self.db().to_str().unwrap()])
            .args(args);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// The project directory is exactly as it was copied: same names, same bytes.
    fn assert_unchanged(&self) {
        assert_eq!(
            listing(&self.dir()),
            self.before,
            "the older-format project was changed (a file was written or created)"
        );
    }
}

fn names_both_versions(text: &str) -> bool {
    text.contains(&FOUND.to_string()) && text.contains(&SUPPORTED.to_string())
}

/// A CLI verb on an older-format project: a non-zero exit whose stderr names both versions
/// and the missing upgrade path.
fn assert_refused(out: &Output) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected error: the older-format project opened (exit {:?}); stdout {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        names_both_versions(&stderr) && stderr.contains(NO_UPGRADE),
        "the refusal names {FOUND}, {SUPPORTED} and the missing upgrade path: {stderr}"
    );
}

fn stats(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "stats");
    assert_refused(&project.run(&["stats"]));
    project.assert_unchanged();
}

fn remember(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "remember");
    assert_refused(&project.run(&["remember", "Fact", "x"]));
    project.assert_unchanged();
}

/// The hook keeps its host contract (exit 0 and `{}` on stdout, never a "block"); the refusal
/// is its stderr note.
fn hook(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "hook");
    let mut child = project
        .command(&["hook", "session-start", "--stdin-json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"session_id":"older-format","hook_event_name":"SessionStart","source":"startup"}"#,
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        names_both_versions(&stderr) && stderr.contains(NO_UPGRADE),
        "expected error: the hook did not report the older format: {stderr}"
    );
    assert_eq!(out.status.code(), Some(0), "a hook always exits 0: {out:?}");
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc, serde_json::json!({}), "nothing for the host");
    project.assert_unchanged();
}

/// `serve` refuses before the MCP handshake: one `initialize` + `tools/list` is offered and
/// the process exits non-zero without answering it.
fn serve(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "serve");
    let mut child = project
        .command(&["serve"])
        .env("TOKIO_WORKER_THREADS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"older-format","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
    );
    // a refusing server may already be gone: a broken pipe is part of the outcome; dropping
    // stdin afterwards is the EOF that ends a server which did answer
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(requests.as_bytes());
    }
    // drain both pipes while waiting: a `tools/list` answer can exceed the pipe buffer
    let drain = |mut pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("kaban serve neither refused nor finished within 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    };
    assert_refused(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("\"tools\""),
        "no tools/list answer from a refused project: {stdout}"
    );
    project.assert_unchanged();
}

fn library_open(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "library");
    let keys = project.home().join("keys");
    match SqliteStore::open(&project.db(), &keys) {
        Ok(store) => {
            let found = store.migration().to;
            drop(store);
            panic!("expected error SchemaUnsupported, the fixture opened as version {found}");
        }
        Err(err) => {
            let text = err.to_string();
            assert!(
                matches!(
                    err,
                    StoreError::SchemaUnsupported {
                        found: FOUND,
                        supported: SUPPORTED
                    }
                ),
                "expected SchemaUnsupported {{ found: {FOUND}, supported: {SUPPORTED} }}, got {err:?}"
            );
            assert!(
                names_both_versions(&text) && text.contains(NO_UPGRADE),
                "the display names both versions and the missing upgrade path: {text}"
            );
        }
    }
    project.assert_unchanged();
}

/// The library's refusal: `SchemaUnsupported { found: 13, supported: 14 }`, named in its
/// display with the missing upgrade path.
fn assert_schema_unsupported(case: &str, err: &StoreError) {
    let text = err.to_string();
    assert!(
        matches!(
            err,
            StoreError::SchemaUnsupported {
                found: FOUND,
                supported: SUPPORTED
            }
        ),
        "{case}: expected SchemaUnsupported {{ found: {FOUND}, supported: {SUPPORTED} }}, got {err:?}"
    );
    assert!(
        names_both_versions(&text) && text.contains(NO_UPGRADE),
        "{case}: the display names both versions and the missing upgrade path: {text}"
    );
}

/// The exclusive maintenance hold (`exclusive_hold`) is an entry point of its own: it is
/// refused on an older-format file before its `.owner` exists, and the opens tried while it
/// would be held (the ordinary, diagnostic and hook opens) are refused the same way, never
/// let through as a file this process already admitted.
fn hold_then_open(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "hold");
    let keys = project.home().join("keys");
    let actor = ActorId::default();
    let hold = kaban_store_sqlite::exclusive_hold(&project.db());
    let opens = [
        ("open", SqliteStore::open(&project.db(), &keys).map(drop)),
        (
            "open_diagnostic",
            SqliteStore::open_diagnostic(&project.db(), &keys, &actor).map(drop),
        ),
        (
            "open_for_hook",
            SqliteStore::open_for_hook(&project.db(), &keys, &actor).map(drop),
        ),
    ];
    // the hold, if taken, lives until here: every open above ran under it
    let taken = hold.map(|hold| hold.is_some());
    match taken {
        Ok(taken) => {
            panic!("expected error: exclusive_hold took the older-format file (taken {taken})")
        }
        Err(ref err) => assert_schema_unsupported("exclusive_hold", err),
    }
    for (case, opened) in opens {
        match opened {
            Ok(()) => panic!("expected error: {case} opened the older-format project"),
            Err(err) => assert_schema_unsupported(case, &err),
        }
    }
    project.assert_unchanged();
}

/// The public index reader (`IndexRows::open`) without a store: refused like an open, before
/// SQLite reads the file (a read-only connection to a WAL-mode file would still create
/// `-shm` and could replay the log).
fn index_reader(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "index-rows");
    match kaban_store_sqlite::index_rows::IndexRows::open(&project.db()) {
        Ok(rows) => {
            drop(rows);
            panic!("expected error: IndexRows::open read the older-format project");
        }
        Err(err) => assert_schema_unsupported("IndexRows::open", &err),
    }
    project.assert_unchanged();
}

/// `kaban --db <name> stats` run from inside the project directory, so `name` is relative.
fn run_relative(project: &Project, name: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(project.dir())
        .env("KABAN_HOME", project.home())
        .env("KABAN_WORKSPACE", "fixture")
        .env_remove("RUST_LOG")
        .args(["--json", "--db", name, "stats"])
        .output()
        .unwrap()
}

/// A relative `--db` name is one file to every check and every connection: `file:kaban.db` is
/// the file of that literal name (absent here, so a fresh project is created under it), never
/// read by SQLite as a URI naming the older `kaban.db` beside it; the fixture's files are
/// untouched and nothing is created for them. The plain relative name is the fixture itself
/// and is refused untouched.
fn relative_names(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "relative");
    let literal = run_relative(&project, "file:kaban.db");
    let after = listing(&project.dir());
    for (name, sha) in &project.before {
        assert_eq!(
            after.get(name),
            Some(sha),
            "the older-format {name} was changed through the name file:kaban.db"
        );
    }
    for sidecar in ["kaban.db-wal", "kaban.db-shm", "kaban.db.owner"] {
        if !project.before.contains_key(sidecar) {
            assert!(
                !after.contains_key(sidecar),
                "{sidecar} was created for the older-format kaban.db: {after:?}"
            );
        }
    }
    assert!(
        literal.status.success(),
        "the literal file:kaban.db is a fresh project of its own: {literal:?}"
    );
    assert!(
        after.contains_key("file:kaban.db"),
        "the project was created under its literal name: {after:?}"
    );
    let fresh = Project::copy(fixture, files, "relative-plain");
    assert_refused(&run_relative(&fresh, "kaban.db"));
    fresh.assert_unchanged();
}

/// `doctor` reports the refusal as a failed `schema_version` check and exits 0.
fn doctor(fixture: &str, files: &[(&str, &str)]) {
    let project = Project::copy(fixture, files, "doctor");
    let out = project.run(&["doctor"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "expected the refusal as a report, exit 0: {out:?}"
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], false, "{report}");
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "schema_version")
        .unwrap_or_else(|| panic!("no schema_version check in {report}"));
    assert_eq!(check["ok"], false, "{check}");
    let detail = check["detail"].as_str().unwrap();
    assert!(
        names_both_versions(detail),
        "the check names {FOUND} and {SUPPORTED}: {detail}"
    );
    project.assert_unchanged();
}

#[test]
fn stats_refuses_a_clean_v3_project() {
    stats("v3.0.0", CLEAN);
}

#[test]
fn stats_refuses_a_killed_writer_v3_project() {
    stats("v3.0.0-wal", WAL);
}

#[test]
fn remember_refuses_a_clean_v3_project() {
    remember("v3.0.0", CLEAN);
}

#[test]
fn remember_refuses_a_killed_writer_v3_project() {
    remember("v3.0.0-wal", WAL);
}

#[test]
fn hook_reports_a_clean_v3_project() {
    hook("v3.0.0", CLEAN);
}

#[test]
fn hook_reports_a_killed_writer_v3_project() {
    hook("v3.0.0-wal", WAL);
}

#[test]
fn serve_refuses_a_clean_v3_project() {
    serve("v3.0.0", CLEAN);
}

#[test]
fn serve_refuses_a_killed_writer_v3_project() {
    serve("v3.0.0-wal", WAL);
}

#[test]
fn library_open_refuses_a_clean_v3_project() {
    library_open("v3.0.0", CLEAN);
}

#[test]
fn library_open_refuses_a_killed_writer_v3_project() {
    library_open("v3.0.0-wal", WAL);
}

#[test]
fn doctor_reports_a_clean_v3_project() {
    doctor("v3.0.0", CLEAN);
}

#[test]
fn doctor_reports_a_killed_writer_v3_project() {
    doctor("v3.0.0-wal", WAL);
}

#[test]
fn exclusive_hold_and_the_opens_under_it_refuse_a_clean_v3_project() {
    hold_then_open("v3.0.0", CLEAN);
}

#[test]
fn exclusive_hold_and_the_opens_under_it_refuse_a_killed_writer_v3_project() {
    hold_then_open("v3.0.0-wal", WAL);
}

#[test]
fn the_index_reader_refuses_a_clean_v3_project() {
    index_reader("v3.0.0", CLEAN);
}

#[test]
fn the_index_reader_refuses_a_killed_writer_v3_project() {
    index_reader("v3.0.0-wal", WAL);
}

#[test]
fn a_relative_file_name_never_reaches_a_clean_v3_project() {
    relative_names("v3.0.0", CLEAN);
}

#[test]
fn a_relative_file_name_never_reaches_a_killed_writer_v3_project() {
    relative_names("v3.0.0-wal", WAL);
}
