//! Concurrent `kaban sync` processes over one repository (core-spec F13, S5; OWN-23/M5-8
//! "a tombstone always wins"; review R5-4): eight `kaban sync` processes race over the same
//! `.kaban/` tree, one bound to a database that holds a purged record (a tombstone pending)
//! and seven to databases ("clones") that hold the same record live and dirty at a higher
//! rev. Whatever the interleaving, the record file must end as the tombstone and no purged
//! text may return to the file or to the purged database.
//!
//! Expensive: `#[ignore]`, run alone with
//! `cargo nextest run --workspace -j2 --run-ignored only -E 'test(multi_process)'`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the per-round outcome is the test's report

mod common;

use common::Scratch;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";
/// Clones that hold the record live and dirty (plus the purging database: 8 processes).
const LIVE_CLONES: usize = 7;
/// Rounds, each with a fresh record (the race window is a few milliseconds wide).
const ROUNDS: usize = 6;

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
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "initial"]);
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

    fn record_path(&self, id: &str) -> PathBuf {
        self.root
            .join(".kaban/adr/decisions")
            .join(format!("{id}.md"))
    }
}

/// `kaban --db <db> --workspace fixture --json <args>` inside the repository.
fn command(s: &Scratch, db: &Path, repo: &Repo, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args([
            "--db",
            db.to_str().unwrap(),
            "--workspace",
            "fixture",
            "--json",
        ])
        .args(args)
        .current_dir(&repo.root)
        .stdin(Stdio::null());
    cmd
}

fn run(s: &Scratch, db: &Path, repo: &Repo, args: &[&str]) -> Output {
    let out = command(s, db, repo, args).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `(lifecycle, text)` of a record straight from a database file.
fn row(db: &Path, id: &str) -> (String, String) {
    let conn = rusqlite::Connection::open(db).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    conn.query_row(
        "SELECT lifecycle, kaban_body(text) FROM node WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn contains_bytes(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// R5-4 (S5, M5-8 under concurrency): per round, database A purges a record whose file and
/// seven clones' live copies still carry its text, each clone's copy dirty at a higher rev,
/// then A's sync and the seven clones' syncs race. Afterwards the file is the tombstone, A is
/// still purged with no trace of the text in its bytes, every sync exited 0, and each clone's
/// next sync purges it.
#[test]
#[ignore = "expensive 8-process run; cargo nextest run --run-ignored only -E 'test(multi_process)'"]
fn multi_process_eight_syncs_race_a_tombstone_against_live_content() {
    let s = Scratch::new("kaban-multi", "sync-race");
    let repo = Repo::new(&s);
    let db_a = s.0.join("a.db");
    let clones: Vec<PathBuf> = (0..LIVE_CLONES)
        .map(|i| s.0.join(format!("b{i}.db")))
        .collect();
    for round in 0..ROUNDS {
        let canary = format!("purge-canary-{round}-rotate-the-signing-key");
        // A: a record, serialized; every clone imports it, then advances it (dirty, live,
        // rev above the file's)
        let created = run(
            &s,
            &db_a,
            &repo,
            &[
                "remember",
                "Decision",
                &format!("Rotate the key. {canary}."),
                "--props",
                r#"{"rationale": "the race"}"#,
            ],
        );
        let id = stdout_json(&created)["id"].as_str().unwrap().to_owned();
        run(&s, &db_a, &repo, &["sync"]);
        for db in &clones {
            run(&s, db, &repo, &["import"]);
            run(
                &s,
                db,
                &repo,
                &[
                    "accept",
                    &id,
                    "--reason",
                    "reviewed",
                    "--path-scope",
                    "ops/**",
                ],
            );
        }
        // A: the purge (the tombstone is pending until A's next sync)
        run(&s, &db_a, &repo, &["forget", &id, "--mode", "purge"]);
        assert!(contains_bytes(
            &std::fs::read(repo.record_path(&id)).unwrap(),
            &canary
        ));

        // the race: 8 syncs started back to back, the purging database in the middle
        let mut children = Vec::new();
        let (first, last) = clones.split_at(LIVE_CLONES / 2);
        for db in first.iter().chain(std::iter::once(&db_a)).chain(last) {
            let child = command(&s, db, &repo, &["sync"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            children.push(child);
        }
        let mut deferred = 0;
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert_eq!(
                out.status.code(),
                Some(0),
                "round {round}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if stdout_json(&out)["deferred"] == Value::Bool(true) {
                deferred += 1;
            }
        }

        let file = std::fs::read(repo.record_path(&id)).unwrap();
        let text = String::from_utf8_lossy(&file);
        assert!(
            text.contains("lifecycle: purged") && !contains_bytes(&file, &canary),
            "round {round}: the file must end as the tombstone:\n{text}"
        );
        let (lifecycle, stored) = row(&db_a, &id);
        assert_eq!(lifecycle, "purged", "round {round}: A stays purged");
        assert!(!stored.contains(&canary), "round {round}: A holds no text");
        assert!(
            !contains_bytes(&std::fs::read(&db_a).unwrap(), &canary),
            "round {round}: the purged text never returned to A's database bytes"
        );
        // each clone's next sync imports the tombstone and purges locally (the tombstone
        // always wins)
        for db in &clones {
            run(&s, db, &repo, &["sync"]);
            assert_eq!(row(db, &id).0, "purged", "round {round}: clone purged");
        }
        eprintln!("round {round}: tombstone held, {deferred} of 8 syncs deferred");
    }
}
