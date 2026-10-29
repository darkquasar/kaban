//! Informational scale test of the `.kaban/*.md` sync (core-spec F13; core-plan C5.5 as amended
//! by `reviews/m5-prep.md` M5-26: no bench for `sync` itself, one focused test that exports
//! 1,000 project-scoped records into a temporary repository under a loose ceiling). The
//! timings are printed for the report; the only assertions on time are the loose ceilings
//! (`CEILING`), chosen far above the observed values so parallel load never fails the suite
//! (owner decision OWN-11: margins scale under load). Correctness assertions are exact: every
//! record gets its file, the digest and the export block list every accepted record (R5-7: a
//! generated document is never withheld for its length), a second run writes nothing and
//! leaves every byte identical, and a fresh clone imports every file.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stdout)] // the timings are the test's informational output

mod common;

use common::Scratch;
use kaban_api::export::{self, ExportFormat, ExportOpts, ExportReport, WriteOutcome};
use kaban_api::sync::{self, SyncOpts};
use kaban_api::{AcceptRequest, ApiConfig, Caller, KabanApi, RememberRequest};
use kaban_core::{Actor, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";
const WS: &str = "nightjar-service";
/// Project-scoped governed records exported (M5-26).
const RECORDS: usize = 1_000;
/// Every record is accepted (`ACCEPTED == RECORDS`), so the digest and the export block list
/// 1,000 entries (about 230,000 chars each, far over the 20,000-character per-field cap).
/// Generated documents are gated in overlapping windows (R5-7, F-W3-1), so neither is withheld
/// for its length; before that correction the digest disappeared above about 85 accepted
/// records and `export` exited 2 above about 108 (observed at 100 accepted records,
/// `m5-W3-report.md`), which is why this test used to accept every twentieth record only.
const ACCEPTED: usize = RECORDS;
/// Loose ceiling per phase (first sync, second sync, fresh import). Observed on the development
/// host: about 0.4 s for the first sync, 0.4 s for the second, 0.6 s for the fresh import; the
/// ceiling is not a performance claim.
const CEILING: Duration = Duration::from_secs(60);

/// A throwaway git repository under a scratch directory, with one dated commit.
struct Repo {
    scratch: Scratch,
    root: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new("kaban-sync-scale", name);
        let root = scratch.0.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let repo = Self { scratch, root };
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

    /// A database ("clone") bound to `WS` with this repository as its root.
    fn clone_db(&self, name: &str) -> (KabanApi, Arc<SqliteStore>) {
        let dir = self.scratch.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap());
        let api = KabanApi::with_config(
            store.clone(),
            Policy::empty(),
            store.signer(),
            None,
            ApiConfig {
                workspace: WorkspaceId::new(WS),
                repo_root: Some(self.root.clone()),
                repo: Some("repo".into()),
                record_signer_identity: false,
                engine: std::sync::Arc::default(),
            },
        );
        (api, store)
    }

    /// `(relative path, bytes)` of every file under `.kaban/`, in path order.
    fn kaban_files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap()));
                }
            }
        }
        let mut files = Vec::new();
        walk(&self.root.join(".kaban"), &mut files);
        files.sort();
        files
    }
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

/// M5-26: 1,000 distinct project-scoped Decisions (50 of them accepted with a scope) are
/// written by one `sync` into the repository, each under its own file, with the digest and the
/// `.gitignore`; the second `sync` writes nothing and leaves every byte identical; a fresh clone
/// imports every file. The three phases are timed and printed; each is under the loose ceiling.
#[test]
fn one_thousand_records_sync_once_then_idempotently_and_rebuild_into_a_fresh_clone() {
    // Arrange: 1,000 records through the service, as agents and people would store them
    let repo = Repo::new("thousand");
    let (a, _a_store) = repo.clone_db("a");
    let seeding = Instant::now();
    let mut accepted = 0usize;
    for i in 0..RECORDS {
        let id = a
            .remember(RememberRequest {
                kind: "Decision".into(),
                text: format!(
                    "Decision {i:04}: module m{i} keeps its own writer. One connection per \
                     process avoids the busy loop seen in run {i}."
                ),
                summary: Some(format!("Decision {i:04}")),
                tags: vec!["storage".into(), format!("m{}", i % 17)],
                props: Some(json!({"rationale": format!("measured in run {i}")})),
                ..Default::default()
            })
            .unwrap()
            .id;
        a.accept(
            AcceptRequest {
                id,
                reason: Some("reviewed".into()),
                path_scope: vec![format!("crates/m{i}/**")],
                ..Default::default()
            },
            &human(),
        )
        .unwrap();
        accepted += 1;
    }
    let seeding = seeding.elapsed();
    assert_eq!(accepted, ACCEPTED);

    // Act: the first sync
    let started = Instant::now();
    let first = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let first_sync = started.elapsed();

    // Assert: one file per record, the digest and the .gitignore, nothing withheld
    assert_eq!(first.written, u32::try_from(RECORDS).unwrap(), "{first:?}");
    assert_eq!((first.rewritten, first.withheld), (0, 0), "{first:?}");
    assert!(first.digest_updated, "{first:?}");
    let files = repo.kaban_files();
    let records = files
        .iter()
        .filter(|(p, _)| p.starts_with(repo.root.join(".kaban/adr/decisions")))
        .count();
    assert_eq!(records, RECORDS);
    assert_eq!(
        files.len(),
        RECORDS + 2,
        "the records, adr/INDEX.md, .gitignore"
    );
    let digest = std::fs::read_to_string(repo.root.join(".kaban/adr/INDEX.md")).unwrap();
    assert_eq!(
        digest.matches("](decisions/").count(),
        accepted,
        "the digest lists the accepted records once each"
    );
    assert!(
        digest.chars().count() > 20_000,
        "{}",
        digest.chars().count()
    );

    // Act: the export block of the same 1,000 accepted records into a user file (R5-7)
    let agents = repo.root.join("AGENTS.md");
    let started = Instant::now();
    let exported = export::export(
        &a,
        &ExportOpts {
            format: ExportFormat::AgentsMd,
            write: Some(agents.clone()),
            ontologies: Vec::new(),
        },
    )
    .unwrap();
    let export_write = started.elapsed();
    match &exported {
        ExportReport::Written {
            outcome, records, ..
        } => {
            assert_eq!(*outcome, WriteOutcome::Created, "{exported:?}");
            assert_eq!(*records, accepted, "{exported:?}");
        }
        other @ ExportReport::Rendered { .. } => panic!("{other:?}"),
    }
    let block = std::fs::read_to_string(&agents).unwrap();
    assert_eq!(block.matches("](.kaban/adr/decisions/").count(), accepted);
    assert!(block.chars().count() > 20_000, "{}", block.chars().count());

    // Act: the second sync, right after the first
    let started = Instant::now();
    let second = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let second_sync = started.elapsed();

    // Assert: nothing written, every byte identical (the export file is outside `.kaban/`)
    assert_eq!(
        (
            second.written,
            second.rewritten,
            second.tombstones,
            second.deleted
        ),
        (0, 0, 0, 0),
        "{second:?}"
    );
    assert!(!second.digest_updated, "{second:?}");
    assert_eq!(
        second.import.unchanged,
        u32::try_from(RECORDS).unwrap(),
        "{second:?}"
    );
    assert_eq!(repo.kaban_files(), files, "a second sync is byte-identical");

    // Act: a fresh clone imports the committed files
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "serialized"]);
    let (b, _b_store) = repo.clone_db("b");
    let started = Instant::now();
    let imported = sync::import(&b, &repo.root, "rebuild").unwrap();
    let fresh_import = started.elapsed();

    // Assert
    assert_eq!(
        imported.files,
        u32::try_from(RECORDS).unwrap(),
        "{imported:?}"
    );
    assert_eq!(
        imported.imported,
        u32::try_from(RECORDS).unwrap(),
        "{imported:?}"
    );
    assert!(imported.conflicts.is_empty(), "{imported:?}");
    assert_eq!(
        (
            imported.rejected,
            imported.skipped,
            imported.import_warnings
        ),
        (0, 0, 0),
        "committed, unmodified files: {imported:?}"
    );

    // the informational timings (M5-26; OWN-11: never a tight gate)
    println!(
        "sync_scale: {RECORDS} records ({accepted} accepted): seeding {seeding:?}, first sync \
         {first_sync:?}, export {export_write:?}, second sync {second_sync:?}, fresh import \
         {fresh_import:?} (ceiling {CEILING:?} per phase)"
    );
    for (phase, took) in [
        ("first sync", first_sync),
        ("export", export_write),
        ("second sync", second_sync),
        ("fresh import", fresh_import),
    ] {
        assert!(
            took < CEILING,
            "{phase} took {took:?}, above the loose ceiling {CEILING:?} (M5-26)"
        );
    }
}
