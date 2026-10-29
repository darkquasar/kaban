//! `kaban sync|import|resolve` and the `serve` startup import, black-box through the binary
//! over a temporary git repository (core-spec F13, §6; core-plan C5.1 as amended by
//! `reviews/m5-prep.md` M5-13, M5-16, M5-19, M5-32): one JSON document per run, the exit codes
//! of §6 (unknown id 3, a forbidden move 4, outside a repository 4), `--stage` through git, and
//! a server that imports the files once after it is up and skips the import without `.kaban/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";

/// A throwaway git repository under the scratch directory.
struct Repo {
    root: PathBuf,
}

impl Repo {
    fn new(s: &Scratch) -> Self {
        let root = s.0.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let repo = Self { root };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo.git(&["config", "user.name", "fixture"]);
        std::fs::write(repo.root.join("README.md"), "# fixture\n").unwrap();
        repo.commit("initial");
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_AUTHOR_DATE", FIXED_DATE)
            .env("GIT_COMMITTER_DATE", FIXED_DATE)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.root
            .join(".kaban/adr/decisions")
            .join(format!("{id}.md"))
    }
}

/// `kaban --db <db> --workspace fixture <args>` with the scratch home, run inside `cwd`.
fn kaban(s: &Scratch, db: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args([
            "--db",
            db.to_str().unwrap(),
            "--workspace",
            "fixture",
            "--json",
        ])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout_json(out: &Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "one compact document: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn sync_import_and_resolve_print_one_report_each_with_the_exit_codes_of_the_spec() {
    let s = Scratch::new("kaban-cli-sync", "cli");
    let repo = Repo::new(&s);
    let db_a = s.0.join("a.db");
    let db_b = s.0.join("b.db");
    let root = repo.root.to_str().unwrap();

    // a Decision accepted on the CLI, then serialized with --stage
    let created = kaban(
        &s,
        &db_a,
        &repo.root,
        &[
            "remember",
            "Decision",
            "Use SQLite for the project store. It needs no daemon.",
            "--props",
            r#"{"rationale": "embedded"}"#,
        ],
    );
    assert_eq!(created.status.code(), Some(0), "{}", stderr(&created));
    let id = stdout_json(&created)["id"].as_str().unwrap().to_owned();
    let accepted = kaban(
        &s,
        &db_a,
        &repo.root,
        &[
            "accept",
            &id,
            "--reason",
            "reviewed",
            "--path-scope",
            "crates/**",
        ],
    );
    assert_eq!(accepted.status.code(), Some(0), "{}", stderr(&accepted));
    let synced = kaban(&s, &db_a, &repo.root, &["sync", "--stage"]);
    assert_eq!(synced.status.code(), Some(0), "{}", stderr(&synced));
    let report = stdout_json(&synced);
    assert_eq!(report["written"], json!(1), "{report}");
    assert_eq!(report["staged"], json!(true), "{report}");
    assert_eq!(report["lease"], json!("acquired"), "{report}");
    assert!(repo.record_path(&id).exists());
    let status = repo.git(&["status", "--porcelain", "--", ".kaban"]);
    assert!(
        status.lines().all(|l| l.starts_with("A ")),
        "every .kaban file is staged: {status}"
    );
    let head = repo.commit("serialized");

    // a second database rebuilds from the committed files
    let rebuilt = kaban(&s, &db_b, &repo.root, &["import", "--repo", root]);
    assert_eq!(rebuilt.status.code(), Some(0), "{}", stderr(&rebuilt));
    let report = stdout_json(&rebuilt);
    assert_eq!(report["imported"], json!(1), "{report}");
    assert_eq!(
        report["import_warnings"],
        json!(0),
        "committed bytes: {report}"
    );
    let why = kaban(&s, &db_b, &repo.root, &["why", "--path", "crates/x.rs"]);
    assert_eq!(why.status.code(), Some(0), "{}", stderr(&why));
    let cards = stdout_json(&why);
    assert_eq!(cards["results"][0]["node"]["id"], json!(id), "{cards}");
    assert_eq!(cards["results"][0]["governance"], json!("accepted"));
    assert!(cards["results"][0].get("conflict").is_none(), "{cards}");
    let _ = head;

    // resolve: an id without a conflict is not found (3); a conflict resolves (0) and the
    // card says so until then; a forbidden governance move through --take file is refused (4)
    let not_found = kaban(
        &s,
        &db_a,
        &repo.root,
        &["resolve", &id, "--take", "db", "--repo", root],
    );
    assert_eq!(not_found.status.code(), Some(3), "{}", stderr(&not_found));
    assert_eq!(not_found.stdout.len(), 0, "no document on an error");
    let unknown = kaban(
        &s,
        &db_a,
        &repo.root,
        &[
            "resolve",
            "01a17a03-5cbc-7000-8000-000000000000",
            "--take",
            "db",
            "--repo",
            root,
        ],
    );
    assert_eq!(unknown.status.code(), Some(3), "{}", stderr(&unknown));
    let path = repo.record_path(&id);
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("no daemon.", "no daemon, ever.")).unwrap();
    let rebuilt = kaban(&s, &db_a, &repo.root, &["import"]);
    assert_eq!(rebuilt.status.code(), Some(0), "{}", stderr(&rebuilt));
    assert_eq!(stdout_json(&rebuilt)["conflicts"], json!([id]));
    let why = kaban(&s, &db_a, &repo.root, &["why", "--path", "crates/x.rs"]);
    assert_eq!(stdout_json(&why)["results"][0]["conflict"], json!(true));
    let resolved = kaban(&s, &db_a, &repo.root, &["resolve", &id, "--take", "file"]);
    assert_eq!(resolved.status.code(), Some(0), "{}", stderr(&resolved));
    let report = stdout_json(&resolved);
    assert_eq!(report["take"], json!("file"), "{report}");
    assert_eq!(report["node"]["id"], json!(id), "{report}");
    assert!(
        report["node"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("ever")),
        "{report}"
    );
    assert_eq!(report["rev"], json!(3), "max(file 2, db 2) + 1: {report}");
    let why = kaban(&s, &db_a, &repo.root, &["why", "--path", "crates/x.rs"]);
    assert!(stdout_json(&why)["results"][0].get("conflict").is_none());
    let current = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        current.replace("governance: accepted", "governance: proposed"),
    )
    .unwrap();
    kaban(&s, &db_a, &repo.root, &["import"]);
    let refused = kaban(&s, &db_a, &repo.root, &["resolve", &id, "--take", "file"]);
    assert_eq!(refused.status.code(), Some(4), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("governance transition"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(refused.stdout.len(), 0, "no document on an error");
    let taken = kaban(&s, &db_a, &repo.root, &["resolve", &id, "--take", "db"]);
    assert_eq!(taken.status.code(), Some(0), "{}", stderr(&taken));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("governance: accepted")
    );

    // outside a repository: the fixed text, no database created
    let plain = s.0.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let db_p = s.0.join("p.db");
    for args in [
        &["sync"][..],
        &["import"][..],
        &["resolve", &id, "--take", "db"][..],
    ] {
        let out = kaban(&s, &db_p, &plain, args);
        assert_eq!(out.status.code(), Some(4), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("needs a git repository"), "{args:?}");
    }
    assert!(!db_p.exists());
}

/// A minimal stdio client over a server started in `cwd` with `RUST_LOG=debug` and stderr
/// captured (the shared harness has no working-directory knob).
struct Stdio2 {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<Value>,
    stderr: std::thread::JoinHandle<String>,
    next_id: u64,
}

impl Stdio2 {
    fn start(s: &Scratch, db: &Path, cwd: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "debug")
            .env("TOKIO_WORKER_THREADS", "1")
            .args([
                "--db",
                db.to_str().unwrap(),
                "--workspace",
                "fixture",
                "serve",
            ])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut err = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut err, &mut text);
            text
        });
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in out.lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut srv = Self {
            child,
            lines,
            stderr,
            next_id: 0,
        };
        let init = srv.call(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], json!("kaban"));
        srv.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        srv
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(msg.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn call(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("response within 10 s");
            let v = self.lines.recv_timeout(left).expect("response within 10 s");
            if v.get("method").is_none() && v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn finish(mut self) -> String {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
        self.stderr.join().unwrap()
    }
}

#[test]
fn serve_imports_the_kaban_files_once_at_startup_and_skips_the_import_without_the_directory() {
    let s = Scratch::new("kaban-cli-sync", "serve");
    let repo = Repo::new(&s);
    let db_a = s.0.join("a.db");
    let created = kaban(
        &s,
        &db_a,
        &repo.root,
        &[
            "remember",
            "Decision",
            "Startup imports this one. It is committed.",
            "--props",
            r#"{"rationale": "because"}"#,
        ],
    );
    assert_eq!(created.status.code(), Some(0), "{}", stderr(&created));
    let id = stdout_json(&created)["id"].as_str().unwrap().to_owned();
    let synced = kaban(&s, &db_a, &repo.root, &["sync"]);
    assert_eq!(synced.status.code(), Some(0), "{}", stderr(&synced));
    repo.commit("serialized");

    // a fresh database served from inside the repository: the record arrives after startup
    let db_b = s.0.join("b.db");
    let mut srv = Stdio2::start(&s, &db_b, &repo.root);
    let deadline = Instant::now() + Duration::from_secs(10);
    let found = loop {
        let reply = srv.call(
            "tools/call",
            &json!({"name": "recall", "arguments": {"query": "startup imports"}}),
        );
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("{}");
        let result: Value = serde_json::from_str(text).unwrap_or(json!({}));
        if result["results"]
            .as_array()
            .is_some_and(|r| r.iter().any(|c| c["id"] == json!(id)))
        {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(found, "the committed record was imported at startup");
    let log = srv.finish();
    assert_eq!(
        log.matches("startup import done").count(),
        1,
        "exactly one import per start: {log}"
    );
    assert!(
        !log.contains(&id),
        "counts only, never an id or a path: {log}"
    );

    // served from a repository without `.kaban/`: skipped, nothing imported, no directory made
    let bare = s.0.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let bare_repo = Repo { root: bare.clone() };
    bare_repo.git(&["init", "-q", "-b", "main"]);
    let db_c = s.0.join("c.db");
    let srv = Stdio2::start(&s, &db_c, &bare);
    std::thread::sleep(Duration::from_millis(300));
    let log = srv.finish();
    assert!(log.contains("startup import skipped"), "{log}");
    assert!(!log.contains("startup import done"), "{log}");
    assert!(
        !bare.join(".kaban").exists(),
        "the import never creates the directory"
    );
}

/// R5-2 (§6, containment): a repository whose `.kaban` is a symlink makes `sync` and `import`
/// exit 4 with the fixed text; nothing is read or written through the link (`resolve` reports
/// the missing conflict row first, exit 3, and touches the tree only after it).
#[test]
fn a_symlinked_kaban_directory_exits_4_with_the_fixed_text() {
    let s = Scratch::new("kaban-cli-sync", "symlink");
    let repo = Repo::new(&s);
    let outside = s.0.join("outside");
    std::fs::create_dir_all(outside.join("decisions")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.root.join(".kaban")).unwrap();
    let db = s.0.join("a.db");
    for args in [&["sync"][..], &["import"][..]] {
        let out = kaban(&s, &db, &repo.root, args);
        assert_eq!(out.status.code(), Some(4), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("is a symlink or leaves the repository"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    assert_eq!(
        std::fs::read_dir(outside.join("decisions"))
            .unwrap()
            .count(),
        0
    );
}

/// R5-9: `kaban sync --deadline <s>` is a hard wall-clock bound on the whole run, honoured by
/// `sync` itself (so every host recipe gets it, with or without GNU `timeout`). A sync that
/// stalls behind the repository lock exits 4 at the deadline with a fixed stderr line, well
/// before the 10 s lock wait, with nothing written and no temp file left; the same sync
/// finishes normally once the holder is gone, and a deadline the run meets changes nothing.
#[test]
fn sync_deadline_stops_a_stalled_run_with_the_fixed_text_and_exit_4() {
    let s = Scratch::new("kaban-cli-sync", "deadline");
    let repo = Repo::new(&s);
    let db = s.0.join("a.db");
    let created = kaban(
        &s,
        &db,
        &repo.root,
        &[
            "remember",
            "Decision",
            "Bound the flush. Every host.",
            "--props",
            r#"{"rationale": "bounded"}"#,
        ],
    );
    assert_eq!(created.status.code(), Some(0), "{}", stderr(&created));
    let id = stdout_json(&created)["id"].as_str().unwrap().to_owned();
    std::fs::create_dir_all(repo.root.join(".kaban")).unwrap();
    // another kaban "holds" the repository for the whole run: this sync stalls on the lock
    let mut holder = Command::new("flock")
        .args(["--no-fork", "-x"])
        .arg(repo.root.join(".kaban"))
        .args(["sleep", "120"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("flock(1) is available");
    let held = || {
        !Command::new("flock")
            .args(["-n", "-x"])
            .arg(repo.root.join(".kaban"))
            .arg("true")
            .status()
            .unwrap()
            .success()
    };
    let t0 = Instant::now();
    while !held() {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "holder never locked"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let started = Instant::now();
    let out = kaban(&s, &db, &repo.root, &["sync", "--deadline", "1"]);
    let took = started.elapsed();
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        took < Duration::from_secs(6),
        "the deadline fired before the 10 s lock wait: {took:?}"
    );
    assert!(
        stderr(&out)
            .contains("kaban sync: the deadline passed; the run stopped, the next sync continues"),
        "{}",
        stderr(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "no report: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!repo.record_path(&id).exists(), "nothing written");
    let leftovers: Vec<String> = std::fs::read_dir(repo.root.join(".kaban"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "no temp file left: {leftovers:?}");

    holder.kill().unwrap();
    holder.wait().unwrap();

    // the holder is gone: a deadline the run meets changes nothing (the environment spelling
    // of the same flag)
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .env("KABAN_SYNC_DEADLINE", "30")
        .args([
            "--db",
            db.to_str().unwrap(),
            "--workspace",
            "fixture",
            "--json",
            "sync",
        ])
        .current_dir(&repo.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["written"], json!(1));
    assert!(repo.record_path(&id).exists());
}
