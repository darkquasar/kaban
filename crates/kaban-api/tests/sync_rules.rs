//! Public-behaviour tests of the `.kaban/*.md` sync rules (core-spec F13, F5, S1, S5, S6;
//! core-plan C5.1 as amended by `reviews/m5-prep.md` M5-2, M5-3, M5-8, M5-10, M5-11, M5-23,
//! M5-31, M5-32; owner decisions OWN-18, OWN-23, OWN-25) over temporary git repositories: two
//! databases ("clones") share one repository through `sync` and `import`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::sync::{self, SyncOpts};
use kaban_api::{AcceptRequest, ApiConfig, Caller, KabanApi, RememberRequest};
use kaban_core::{Actor, ChangeLogStore, Governance, Lifecycle, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";
/// The workspace both clones bind (OWN-25: files carry none; the repository is the workspace).
const WS: &str = "nightjar-service";

/// A throwaway git repository under a scratch directory; every commit is dated `FIXED_DATE`.
struct Repo {
    scratch: Scratch,
    root: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new("kaban-sync", name);
        let root = scratch.0.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let repo = Self { scratch, root };
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

    /// `.kaban/adr/<dir>/<id>.md`: the engine layout of the shipped ontology (E1.6).
    fn record_path(&self, dir: &str, id: &str) -> PathBuf {
        self.root
            .join(".kaban/adr")
            .join(dir)
            .join(format!("{id}.md"))
    }

    /// Every byte under `.kaban/`, concatenated in path order.
    fn kaban_bytes(&self) -> Vec<u8> {
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
        files.into_iter().flat_map(|(_, b)| b).collect()
    }
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

fn decision(api: &KabanApi, text: &str) -> String {
    api.remember(RememberRequest {
        kind: "Decision".into(),
        text: text.into(),
        summary: Some(text.split('.').next().unwrap().to_owned()),
        tags: vec!["storage".into()],
        props: Some(json!({"rationale": "embedded, zero ops"})),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn accept(api: &KabanApi, id: &str, scope: &[&str]) {
    api.accept(
        AcceptRequest {
            id: id.into(),
            reason: Some("reviewed in PR 42".into()),
            path_scope: scope.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        },
        &human(),
    )
    .unwrap();
}

/// Slice 1 (F13, M5-19): `sync` writes one file per stale project-scoped governed record,
/// the digest and the `.gitignore`; a second run finds nothing dirty and leaves every byte
/// under `.kaban/` identical.
#[test]
fn sync_writes_the_record_files_once_and_a_second_run_rewrites_nothing() {
    let repo = Repo::new("export");
    let (api, store) = repo.clone_db("a");
    let id = decision(
        &api,
        "Use SQLite for the project store. It needs no daemon.",
    );
    accept(&api, &id, &["crates/kaban-store-sqlite/**"]);
    let _fact = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "facts are not serialized".into(),
            ..Default::default()
        })
        .unwrap();

    let first = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(first.written, 1, "{first:?}");
    assert_eq!(first.rewritten, 0, "{first:?}");
    assert_eq!(first.withheld, 0, "{first:?}");
    assert!(first.digest_updated, "{first:?}");
    let path = repo.record_path("decisions", &id);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("---\n"), "{text}");
    assert!(text.contains("governance: accepted"), "{text}");
    assert!(text.contains("Use SQLite for the project store."), "{text}");
    assert!(!text.contains("workspace_id"), "OWN-25: {text}");
    let digest = std::fs::read_to_string(repo.root.join(".kaban/adr/INDEX.md")).unwrap();
    assert!(digest.contains(&id), "{digest}");
    assert_eq!(
        std::fs::read_to_string(repo.root.join(".kaban/.gitignore")).unwrap(),
        "kaban.db*\n"
    );
    // only the ontology directory and the .gitignore under .kaban; the digest and the one
    // record directory under the ontology directory (E1.6 layout)
    let names = |dir: &str| {
        let mut names: Vec<String> = std::fs::read_dir(repo.root.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(".kaban"), [".gitignore", "adr"]);
    assert_eq!(names(".kaban/adr"), ["INDEX.md", "decisions"]);
    // the store knows the file is current
    let node = kaban_core::MemoryStore::get(store.as_ref(), &id.parse().unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(node.serialized_rev, Some(node.rev));
    assert_eq!(node.lifecycle, Lifecycle::Active);
    assert_eq!(node.governance, Governance::Accepted);

    let before = repo.kaban_bytes();
    let second = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!((second.written, second.rewritten), (0, 0), "{second:?}");
    assert!(!second.digest_updated, "{second:?}");
    assert_eq!(
        repo.kaban_bytes(),
        before,
        "a second sync is byte-identical"
    );
    // nothing was staged without --stage
    assert!(!first.staged && !second.staged);
}

/// The stored node as the store holds it (hidden rows included).
fn stored(store: &SqliteStore, id: &str) -> Option<kaban_core::Node> {
    kaban_core::MemoryStore::get(store, &id.parse().unwrap(), true).unwrap()
}

fn events(store: &SqliteStore, id: &str) -> Vec<kaban_core::Event> {
    kaban_core::GovernanceStore::events(store, &id.parse().unwrap()).unwrap()
}

/// Slice 2 (F13 fresh import; OD-15/M5-23 provenance): a committed, unmodified record file
/// imports into a fresh clone as `accepted` with a `human-pr` event stamped with HEAD; the
/// same file uncommitted imports as `proposed` with a warning and is rewritten on the next
/// sync as `proposed`.
#[test]
fn a_committed_file_imports_as_accepted_with_human_pr_and_an_uncommitted_one_as_proposed() {
    let repo = Repo::new("import");
    let (a, _) = repo.clone_db("a");
    let id = decision(&a, "Use SQLite for the project store. It needs no daemon.");
    accept(&a, &id, &["crates/kaban-store-sqlite/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let head = repo.commit("serialize the decision");

    let (b, b_store) = repo.clone_db("b");
    let report = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!(report.files, 1, "{report:?}");
    assert_eq!(report.imported, 1, "{report:?}");
    assert_eq!(report.import_warnings, 0, "{report:?}");
    let node = stored(&b_store, &id).expect("imported");
    assert_eq!(node.governance, Governance::Accepted);
    assert_eq!(
        node.workspace_id, WS,
        "OWN-25: bound to the process workspace"
    );
    assert_eq!(
        node.text,
        "Use SQLite for the project store. It needs no daemon."
    );
    assert_eq!(node.tags, vec!["storage".to_owned()]);
    assert_eq!(
        node.path_scope,
        vec!["crates/kaban-store-sqlite/**".to_owned()]
    );
    assert_eq!(node.props["rationale"], "embedded, zero ops");
    assert_eq!(node.serialized_rev, Some(node.rev));
    let ev = events(&b_store, &id);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].kind, "accepted");
    assert_eq!(ev[0].signer_kind, kaban_core::SignerKind::HumanPr);
    assert_eq!(ev[0].commit_sha.as_deref(), Some(head.as_str()));
    assert_eq!(ev[0].reason, "reviewed in PR 42");
    // a second import of the same files is a no-op
    let again = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!((again.imported, again.unchanged), (0, 1), "{again:?}");

    // negative: the same acceptance claimed by an uncommitted file (a second record)
    let other = decision(&a, "Keep the CLI output to one JSON document. Always.");
    accept(&a, &other, &["crates/kaban/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let (c, c_store) = repo.clone_db("c");
    let report = sync::import(&c, &repo.root, "rebuild").unwrap();
    assert_eq!(report.imported, 2, "{report:?}");
    assert_eq!(report.import_warnings, 1, "{report:?}");
    let demoted = stored(&c_store, &other).expect("imported");
    assert_eq!(demoted.governance, Governance::Proposed);
    assert_eq!(events(&c_store, &other).len(), 0, "no human-pr event");
    assert_ne!(
        demoted.serialized_rev,
        Some(demoted.rev),
        "dirty: the file is corrected on the next sync"
    );
    let corrected = sync::sync(&c, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(corrected.rewritten, 1, "{corrected:?}");
    let text = std::fs::read_to_string(repo.record_path("decisions", &other)).unwrap();
    assert!(text.contains("governance: proposed"), "{text}");
    assert!(text.contains(&format!("rev: {}", demoted.rev)), "{text}");
}

/// Slice 3 (F13 `file rev > db rev`): a clone that advanced a record (proposed -> accepted,
/// rev 1 -> 2) wins in the other clone, through the governance transition table, with the
/// acceptance event; the record is then current on both sides.
#[test]
fn a_higher_file_rev_updates_the_stored_record_through_the_transition_table() {
    let repo = Repo::new("file-wins");
    let (a, _) = repo.clone_db("a");
    let id = decision(&a, "Pin every dependency. Exact versions only.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("proposed");
    let (b, b_store) = repo.clone_db("b");
    sync::import(&b, &repo.root, "rebuild").unwrap();
    let before = stored(&b_store, &id).unwrap();
    assert_eq!(before.governance, Governance::Proposed);
    assert_eq!(before.rev, 1);

    accept(&a, &id, &["Cargo.toml"]);
    let advanced = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(advanced.rewritten, 1, "{advanced:?}");
    let head = repo.commit("accepted");

    let report = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!(report.imported, 1, "{report:?}");
    assert_eq!(report.conflicts.len(), 0, "{report:?}");
    let after = stored(&b_store, &id).unwrap();
    assert_eq!(after.governance, Governance::Accepted);
    assert_eq!(after.rev, 2);
    assert_eq!(after.serialized_rev, Some(2));
    assert_eq!(after.path_scope, vec!["Cargo.toml".to_owned()]);
    let ev = events(&b_store, &id);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].signer_kind, kaban_core::SignerKind::HumanPr);
    assert_eq!(ev[0].commit_sha.as_deref(), Some(head.as_str()));
    // both clones are current: neither sync rewrites anything
    let a2 = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let b2 = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        (a2.written + a2.rewritten, b2.written + b2.rewritten),
        (0, 0)
    );
}

fn conflicts(store: &SqliteStore) -> Vec<kaban_core::ConflictRow> {
    kaban_core::SyncStore::conflicts(store, WS).unwrap()
}

/// Slice 4 (F13 equal rev, different hash; M5-5, M5-31): a hand-edited file at the record's
/// own rev is a `conflicts` row with the file untouched and the record unchanged; a file that
/// asks for a governance move the spec forbids (accepted -> proposed at a higher rev) is a
/// conflict too, not an error, with both sides untouched.
#[test]
fn an_equal_rev_edit_and_an_invalid_governance_regression_are_conflicts_not_imports() {
    let repo = Repo::new("conflict");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Use SQLite for the project store. It needs no daemon.");
    accept(&a, &id, &["crates/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("serialized");
    let path = repo.record_path("decisions", &id);
    let original = std::fs::read_to_string(&path).unwrap();
    let db_before = stored(&a_store, &id).unwrap();

    // a human edits the body without touching `rev`
    let edited = original.replace("It needs no daemon.", "It needs no daemon, ever.");
    assert_ne!(edited, original);
    std::fs::write(&path, &edited).unwrap();
    let report = sync::import(&a, &repo.root, "rebuild").unwrap();
    assert_eq!(report.conflicts, vec![id.clone()], "{report:?}");
    assert_eq!(report.imported, 0, "{report:?}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        edited,
        "file untouched"
    );
    let db_after = stored(&a_store, &id).unwrap();
    assert_eq!(db_after.text, db_before.text, "record untouched");
    assert_eq!(db_after.rev, db_before.rev);
    let rows = conflicts(&a_store);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].node_id, id);
    assert_eq!(
        (rows[0].file_rev, rows[0].db_rev),
        (db_before.rev, db_before.rev)
    );
    assert_ne!(rows[0].file_hash, rows[0].db_hash);
    let stats = a.stats(kaban_api::StatsRequest::default()).unwrap();
    assert_eq!(stats.store["conflicts"], 1, "{stats:?}");
    // a sync neither rewrites the conflicted file nor clears the row
    let synced = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!((synced.written, synced.rewritten), (0, 0), "{synced:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    assert_eq!(conflicts(&a_store).len(), 1);

    // an invalid regression: `accepted -> proposed` at a higher rev
    let regressed = original
        .replace("governance: accepted", "governance: proposed")
        .replace(
            &format!("rev: {}", db_before.rev),
            &format!("rev: {}", db_before.rev + 1),
        );
    std::fs::write(&path, &regressed).unwrap();
    let report = sync::import(&a, &repo.root, "rebuild").unwrap();
    assert_eq!(report.conflicts, vec![id.clone()], "{report:?}");
    assert_eq!(report.imported, 0, "{report:?}");
    let db_after = stored(&a_store, &id).unwrap();
    assert_eq!(db_after.governance, Governance::Accepted);
    assert_eq!(db_after.rev, db_before.rev);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        regressed,
        "file untouched"
    );
    let rows = conflicts(&a_store);
    assert_eq!(rows.len(), 1, "one row per id: {rows:?}");
    assert_eq!(
        (rows[0].file_rev, rows[0].db_rev),
        (db_before.rev + 1, db_before.rev)
    );
}

/// Whether `needle` occurs in `hay` (case-insensitive, the smoke's on-disk canary pattern).
fn contains_bytes(hay: &[u8], needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    let lower = hay.to_ascii_lowercase();
    lower.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Every byte of a clone's database files (closed connection: the clone must be dropped).
fn db_bytes(repo: &Repo, clone: &str) -> Vec<u8> {
    let dir = repo.scratch.0.join(clone);
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        if e.file_name().to_string_lossy().starts_with("kaban.db") {
            out.extend(std::fs::read(e.path()).unwrap());
        }
    }
    out
}

/// Spec 05 SS-10 (D3 review d3-7): the ordinary `sync` carries the import pass's receipt at
/// the shared import-completion boundary, like `import` does: a changed record and a purge
/// tombstone applied by `sync()` both report `committed` at the clone's log head; a pass
/// that applied nothing still reports where it stood.
#[test]
fn sync_carries_the_import_receipt_for_applied_records_and_tombstones() {
    let repo = Repo::new("sync-receipt");
    let (a, _a_store) = repo.clone_db("a");
    let id = decision(
        &a,
        "Carry the receipt on sync. It is the import's position.",
    );
    accept(&a, &id, &["crates/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("serialized");

    let (b, b_store) = repo.clone_db("b");
    let imported = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(imported.import.imported, 1, "{imported:?}");
    let receipt = imported.import.receipt.expect("the pass applied a record");
    assert_eq!(receipt.outcome, kaban_api::WriteOutcome::Committed);
    assert_eq!(receipt.seq, Some(b_store.changes_head().unwrap()));
    assert_eq!(receipt.instance_id, b_store.db_id());

    let again = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(again.import.unchanged, 1, "{again:?}");
    let idle = again
        .import
        .receipt
        .expect("a pass that applied nothing still says where");
    assert_eq!(idle.seq, Some(b_store.changes_head().unwrap()));

    a.forget(kaban_api::ForgetRequest {
        id: id.clone(),
        mode: Some(kaban_core::ForgetMode::Purge),
    })
    .unwrap();
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("purged");
    let purged = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        (purged.import.tombstones, purged.import.purged),
        (1, 1),
        "{purged:?}"
    );
    let receipt = purged.import.receipt.expect("the pass applied a tombstone");
    assert_eq!(receipt.outcome, kaban_api::WriteOutcome::Committed);
    assert_eq!(receipt.seq, Some(b_store.changes_head().unwrap()));
    assert_eq!(stored(&b_store, &id).unwrap().lifecycle, Lifecycle::Purged);
}

/// SS-03 (review a-2): a tombstone that purges a live record in a clone runs the purge's
/// post-commit cleanup there, and a reader still on the clone's WAL holds that cleanup back.
/// The import neither fails nor hides it: the report counts the purge and carries the
/// cleanup detail (`busy`, then the free-page residue a VACUUM removes).
#[test]
fn an_imported_purge_under_a_wal_reader_reports_its_incomplete_cleanup() {
    // Arrange: clone b holds the live record; a purges it and publishes the tombstone
    let repo = Repo::new("tombstone-cleanup");
    let (a, _a_store) = repo.clone_db("a");
    let id = decision(&a, "Rotate the key. purge-canary-cleanup-report.");
    accept(&a, &id, &["ops/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("serialized");
    let (b, b_store) = repo.clone_db("b");
    sync::import(&b, &repo.root, "rebuild").unwrap();
    assert!(stored(&b_store, &id).is_some());
    a.forget(kaban_api::ForgetRequest {
        id: id.clone(),
        mode: Some(kaban_core::ForgetMode::Purge),
    })
    .unwrap();
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("purged");
    // a reader on b's WAL from before the purge: the TRUNCATE checkpoints cannot complete
    let reader = rusqlite::Connection::open_with_flags(
        repo.scratch.0.join("b").join("kaban.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let live: i64 = reader
        .query_row(
            "SELECT count(*) FROM node WHERE lifecycle='active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 1);

    // Act: b imports the tombstone under the reader
    let report = sync::import(&b, &repo.root, "rebuild").unwrap();
    reader.execute_batch("COMMIT").unwrap();

    // Assert: purged and revoked, the cleanup reported incomplete, nothing rejected
    assert_eq!((report.tombstones, report.purged), (1, 1), "{report:?}");
    assert_eq!(report.cleanup.len(), 1, "{report:?}");
    assert!(
        report.cleanup[0].starts_with("database/WAL: busy; free-page residue until VACUUM; index:"),
        "{report:?}"
    );
    assert_eq!(report.rejected, 0, "{report:?}");
    assert_eq!(stored(&b_store, &id).unwrap().lifecycle, Lifecycle::Purged);
}

/// Slice 5 (OWN-18, OWN-23/M5-8, S5): a purge writes the four-key tombstone in place of the
/// record file and the purged text leaves every byte under `.kaban/`; importing the tombstone
/// into a clone that had advanced the record purges it there too (the tombstone always wins,
/// whatever the revs), and the clone's database bytes no longer hold the text; a content file
/// found for a purged record is never imported and is replaced by the tombstone.
#[test]
fn a_tombstone_file_purges_every_clone_and_is_never_rewritten_with_content() {
    const CANARY: &str = "purge-canary-7c1e-rotate-the-signing-key";
    let repo = Repo::new("tombstone");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, &format!("Rotate the key. {CANARY}."));
    accept(&a, &id, &["ops/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("serialized");
    let (b, b_store) = repo.clone_db("b");
    sync::import(&b, &repo.root, "rebuild").unwrap();
    assert!(stored(&b_store, &id).unwrap().text.contains(CANARY));
    // the content file as it was (another clone could still hold it at a higher rev)
    let path = repo.record_path("decisions", &id);
    let content_file = std::fs::read_to_string(&path).unwrap();
    assert!(
        repo.kaban_bytes()
            .windows(CANARY.len())
            .any(|w| w == CANARY.as_bytes())
    );

    // A purges; the next sync writes the tombstone and the digest forgets the record
    a.forget(kaban_api::ForgetRequest {
        id: id.clone(),
        mode: Some(kaban_core::ForgetMode::Purge),
    })
    .unwrap();
    let purged = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(purged.tombstones, 1, "{purged:?}");
    assert_eq!((purged.written, purged.rewritten), (0, 0), "{purged:?}");
    let tombstone = std::fs::read_to_string(&path).unwrap();
    assert_eq!(stored(&a_store, &id).unwrap().lifecycle, Lifecycle::Purged);
    let keys: Vec<&str> = tombstone
        .trim_start_matches("---\n")
        .split("\n---")
        .next()
        .unwrap()
        .lines()
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(keys, ["id", "lifecycle", "rev", "version"], "{tombstone}");
    assert!(tombstone.ends_with("\n---\n"), "no body: {tombstone}");
    assert!(
        !contains_bytes(&repo.kaban_bytes(), CANARY),
        "the purged text left every byte under .kaban"
    );
    let again = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        again.tombstones, 0,
        "the tombstone is written once: {again:?}"
    );
    repo.commit("purged");

    // B advanced the record meanwhile (rev above the tombstone's): the tombstone still wins
    b.forget(kaban_api::ForgetRequest {
        id: id.clone(),
        mode: Some(kaban_core::ForgetMode::Archive),
    })
    .unwrap();
    let b_rev = stored(&b_store, &id).unwrap().rev;
    let report = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!((report.tombstones, report.purged), (1, 1), "{report:?}");
    let ghost = stored(&b_store, &id).unwrap();
    assert_eq!(ghost.lifecycle, Lifecycle::Purged);
    assert_eq!(ghost.text, "");
    assert!(ghost.rev > b_rev);
    // B's sync never rewrites the tombstone with content
    let b_sync = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        (b_sync.written, b_sync.rewritten, b_sync.tombstones),
        (0, 0, 0),
        "{b_sync:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), tombstone);
    drop(b);
    drop(b_store);
    assert!(
        !contains_bytes(&db_bytes(&repo, "b"), CANARY),
        "S5: the purged text left B's database bytes"
    );

    // a content file resurfacing at a higher rev (a stale branch) is not imported and the
    // next sync puts the tombstone back; a fresh clone that sees only the tombstone keeps a
    // ghost
    let resurrected = content_file.replace("rev: 2", "rev: 9");
    assert_ne!(resurrected, content_file);
    std::fs::write(&path, &resurrected).unwrap();
    let report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(report.import.imported, 0, "{report:?}");
    assert_eq!(report.import.stale_files, 1, "{report:?}");
    assert_eq!(report.tombstones, 1, "{report:?}");
    assert_eq!(stored(&a_store, &id).unwrap().text, "");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        tombstone,
        "tombstone back in place"
    );
    let (c, c_store) = repo.clone_db("c");
    let report = sync::import(&c, &repo.root, "rebuild").unwrap();
    assert_eq!(report.tombstones, 1, "{report:?}");
    let ghost = stored(&c_store, &id).expect("a purged ghost so the id never reappears");
    assert_eq!(ghost.lifecycle, Lifecycle::Purged);
    assert!(
        kaban_core::MemoryStore::get(c_store.as_ref(), &id.parse().unwrap(), false)
            .unwrap()
            .is_none(),
        "hidden from reads"
    );
}

/// Slice 6 (OWN-23/M5-11 absence): a serialized record whose file is absent is tombstoned
/// only when git history records the deletion; an absent file no commit ever deleted is
/// re-serialized on the next sync; outside a git repository absence never tombstones (and an
/// acceptance claim imports as `proposed`, OD-15).
#[test]
fn absence_tombstones_only_with_a_recorded_deletion_and_reserializes_otherwise() {
    let repo = Repo::new("absence");
    let (a, a_store) = repo.clone_db("a");
    let deleted = decision(&a, "Deleted later. Its file is removed in a commit.");
    accept(&a, &deleted, &["a/**"]);
    let lost = decision(&a, "Lost without history. Its file vanishes outside git.");
    accept(&a, &lost, &["b/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("two records");
    let (b, b_store) = repo.clone_db("b");
    sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!(
        stored(&b_store, &deleted).unwrap().lifecycle,
        Lifecycle::Active
    );

    // the first file is deleted in a commit: every clone tombstones the record
    repo.git(&[
        "rm",
        "-q",
        "--",
        &format!(".kaban/adr/decisions/{deleted}.md"),
    ]);
    repo.commit("remove the first record");
    // the second file vanishes without a commit recording it
    std::fs::remove_file(repo.record_path("decisions", &lost)).unwrap();
    let report = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!(report.tombstoned_by_absence, 1, "{report:?}");
    assert_eq!(report.reserialized, 1, "{report:?}");
    let gone = stored(&b_store, &deleted).unwrap();
    assert_eq!(gone.lifecycle, Lifecycle::Tombstoned);
    assert!(!gone.text.is_empty(), "a tombstone keeps the text (F5)");
    assert_eq!(
        stored(&b_store, &lost).unwrap().lifecycle,
        Lifecycle::Active
    );
    let resync = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(resync.written, 1, "the lost file is back: {resync:?}");
    assert!(repo.record_path("decisions", &lost).exists());
    assert!(
        !repo.record_path("decisions", &deleted).exists(),
        "no file for a tombstoned record (M5-10)"
    );
    // the same pass on A (which still holds both active) agrees
    let report = sync::import(&a, &repo.root, "rebuild").unwrap();
    assert_eq!(report.tombstoned_by_absence, 1, "{report:?}");
    assert_eq!(
        stored(&a_store, &deleted).unwrap().lifecycle,
        Lifecycle::Tombstoned
    );
    // a tombstoned record is not re-tombstoned or re-serialized by later passes
    let again = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        (
            again.import.tombstoned_by_absence,
            again.import.reserialized,
            again.written
        ),
        (0, 0, 0),
        "{again:?}"
    );

    // outside git: a plain directory with a .kaban tree; absence never tombstones, and an
    // accepted file cannot prove its acceptance
    let plain = repo.scratch.0.join("plain");
    std::fs::create_dir_all(plain.join(".kaban/adr/decisions")).unwrap();
    let dir = repo.scratch.0.join("p");
    std::fs::create_dir_all(&dir).unwrap();
    let p_store = Arc::new(SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap());
    let p = KabanApi::with_config(
        p_store.clone(),
        Policy::empty(),
        p_store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            ..ApiConfig::default()
        },
    );
    std::fs::copy(
        repo.record_path("decisions", &lost),
        plain
            .join(".kaban/adr/decisions")
            .join(format!("{lost}.md")),
    )
    .unwrap();
    let report = sync::import(&p, &plain, "rebuild").unwrap();
    assert_eq!(
        (report.imported, report.import_warnings),
        (1, 1),
        "{report:?}"
    );
    assert_eq!(
        stored(&p_store, &lost).unwrap().governance,
        Governance::Proposed
    );
    sync::sync(&p, &plain, &SyncOpts::default()).unwrap();
    std::fs::remove_file(
        plain
            .join(".kaban/adr/decisions")
            .join(format!("{lost}.md")),
    )
    .unwrap();
    let report = sync::sync(&p, &plain, &SyncOpts::default()).unwrap();
    assert_eq!(report.import.tombstoned_by_absence, 0, "{report:?}");
    assert_eq!(report.import.reserialized, 1, "{report:?}");
    assert_eq!(report.written, 1, "{report:?}");
    assert_eq!(
        stored(&p_store, &lost).unwrap().lifecycle,
        Lifecycle::Active
    );
}

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view.
const CREDENTIAL: &str = "AKIAQWERTYUIOPASDFGH";
/// A redact-class policy: `hunter2` is masked, nothing rejected beyond the builtins.
const REDACT_POLICY: &str = "words:\n  - { term: hunter2, action: redact, id: old-password }\n";

/// `(op, tool, field)` of every audit row of a clone's database, oldest first.
fn audit_rows(repo: &Repo, clone: &str) -> Vec<(String, String, String)> {
    let conn = rusqlite::Connection::open(repo.scratch.0.join(clone).join("kaban.db")).unwrap();
    let mut st = conn
        .prepare("SELECT op, tool, field FROM audit ORDER BY seq")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Slice 7 (S1, M5-2): a record file carrying a credential is skipped before any typed parse,
/// audited as a rejection under the running scope, and nothing of it is stored; a file whose
/// string a redact rule masks imports masked, at `file rev + 1`, and the next sync rewrites
/// the file with the mask so the two sides do not diverge at equal rev.
#[test]
fn a_credential_in_a_file_is_skipped_and_audited_and_a_redaction_is_masked_on_import() {
    let repo = Repo::new("gate");
    let (a, _) = repo.clone_db("a");
    let clean = decision(&a, "Keep the gate on every path. No exceptions.");
    let leaky = decision(&a, "Rotate the key. The old value was hunter2 and must go.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    // a human pastes a credential into the committed file's body (and bumps the rev)
    let path = repo.record_path("decisions", &clean);
    let text = std::fs::read_to_string(&path).unwrap();
    let poisoned = text
        .replace("rev: 1", "rev: 2")
        .replace("No exceptions.", &format!("The key is {CREDENTIAL}."));
    std::fs::write(&path, &poisoned).unwrap();
    repo.commit("poisoned");

    let (b, b_store) = repo.clone_db("b");
    let before = audit_rows(&repo, "b").len();
    let report = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!(report.rejected, 1, "{report:?}");
    assert!(
        stored(&b_store, &clean).is_none(),
        "nothing of the poisoned file is stored"
    );
    assert_eq!(report.imported, 1, "the other file imports: {report:?}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        poisoned,
        "file untouched"
    );
    let rows = audit_rows(&repo, "b");
    assert!(rows.len() > before, "the rejection is audited");
    assert!(
        rows.iter()
            .any(|(op, tool, _)| op == "reject" && tool == "rebuild"),
        "{rows:?}"
    );
    drop(b);
    drop(b_store);
    assert!(
        !contains_bytes(&db_bytes(&repo, "b"), CREDENTIAL),
        "the credential never reached B's database bytes"
    );
    // A still holds the clean record at rev 1; the poisoned file (rev 2) is refused before
    // the rev rule runs, so A's record stays as it is and the file is left to a human
    let a_report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(a_report.import.rejected, 1, "{a_report:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), poisoned);

    // a redact-class policy on a third clone: the masked record is imported one rev above
    // the file and the next sync writes the mask into the file
    let dir = repo.scratch.0.join("c");
    std::fs::create_dir_all(&dir).unwrap();
    let c_store = Arc::new(SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap());
    let policy =
        Policy::from_file(Policy::parse(REDACT_POLICY.as_bytes()).unwrap(), "t".into()).unwrap();
    let c = KabanApi::with_config(
        c_store.clone(),
        policy,
        c_store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: Some(repo.root.clone()),
            repo: Some("repo".into()),
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    );
    let report = sync::import(&c, &repo.root, "rebuild").unwrap();
    assert_eq!(report.imported, 1, "{report:?}");
    let masked = stored(&c_store, &leaky).unwrap();
    assert!(!masked.text.contains("hunter2"), "{}", masked.text);
    assert!(
        masked.text.contains("[REDACTED:old-password]"),
        "{}",
        masked.text
    );
    assert_eq!(masked.rev, 2, "file rev + 1");
    assert_ne!(masked.serialized_rev, Some(masked.rev), "dirty");
    let synced = sync::sync(&c, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(synced.rewritten, 1, "{synced:?}");
    let file = std::fs::read_to_string(repo.record_path("decisions", &leaky)).unwrap();
    assert!(
        !file.contains("hunter2") && file.contains("rev: 2"),
        "{file}"
    );
    assert_eq!(
        sync::import(&c, &repo.root, "rebuild").unwrap().unchanged,
        1,
        "the masked file and the masked record agree"
    );
}

/// Slice 8 (F13, M5-32): `resolve --take db` bumps the record above the file, rewrites the
/// file and clears the row; `--take file` imports the file at `max(file, db) + 1`,
/// re-serializes it and clears the row; a `--take file` that asks for a forbidden governance
/// move is refused and the row stays; an id without a conflict is not found.
#[test]
fn resolve_takes_the_database_or_the_file_and_clears_the_conflict() {
    use kaban_api::sync::{SyncError, Take};
    let repo = Repo::new("resolve");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Use SQLite for the project store. It needs no daemon.");
    accept(&a, &id, &["crates/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let path = repo.record_path("decisions", &id);
    let original = std::fs::read_to_string(&path).unwrap();
    let node_id: kaban_core::NodeId = id.parse().unwrap();

    // no conflict yet: not found
    assert!(matches!(
        sync::resolve(&a, &repo.root, &node_id, Take::Db),
        Err(SyncError::NotFound(_))
    ));

    // --take db: the human edit at equal rev loses
    std::fs::write(&path, original.replace("no daemon.", "no daemon, ever.")).unwrap();
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild")
            .unwrap()
            .conflicts
            .len(),
        1
    );
    let db_rev = stored(&a_store, &id).unwrap().rev;
    let report = sync::resolve(&a, &repo.root, &node_id, Take::Db).unwrap();
    assert_eq!(report.take, Take::Db);
    assert_eq!(report.rev, db_rev + 1);
    assert_eq!(report.node.id, id);
    assert_eq!(
        report.node.text.as_deref(),
        Some("Use SQLite for the project store. It needs no daemon.")
    );
    assert_eq!(conflicts(&a_store), []);
    let after = stored(&a_store, &id).unwrap();
    assert_eq!(
        (after.rev, after.serialized_rev),
        (db_rev + 1, Some(db_rev + 1))
    );
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(
        file.contains("It needs no daemon.\n") && !file.contains("ever"),
        "{file}"
    );
    assert!(file.contains(&format!("rev: {}", db_rev + 1)), "{file}");
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild").unwrap().unchanged,
        1
    );

    // --take file: the human edit at equal rev wins
    let edited = file.replace("no daemon.", "no daemon, truly.");
    std::fs::write(&path, &edited).unwrap();
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild")
            .unwrap()
            .conflicts
            .len(),
        1
    );
    let db_rev = stored(&a_store, &id).unwrap().rev;
    let report = sync::resolve(&a, &repo.root, &node_id, Take::File).unwrap();
    assert_eq!(report.take, Take::File);
    assert_eq!(report.rev, db_rev + 1);
    assert_eq!(
        report.node.text.as_deref(),
        Some("Use SQLite for the project store. It needs no daemon, truly.")
    );
    assert_eq!(conflicts(&a_store), []);
    let after = stored(&a_store, &id).unwrap();
    assert_eq!(
        (after.rev, after.serialized_rev),
        (db_rev + 1, Some(db_rev + 1))
    );
    assert_eq!(after.governance, Governance::Accepted);
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(
        file.contains("truly") && file.contains(&format!("rev: {}", db_rev + 1)),
        "{file}"
    );
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild").unwrap().unchanged,
        1
    );

    // --take file cannot force a forbidden governance move (M5-31)
    let regressed = file.replace("governance: accepted", "governance: proposed");
    std::fs::write(&path, &regressed).unwrap();
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild")
            .unwrap()
            .conflicts
            .len(),
        1
    );
    let err = sync::resolve(&a, &repo.root, &node_id, Take::File).unwrap_err();
    assert!(
        matches!(
            err,
            SyncError::Store(kaban_core::StoreError::InvalidTransition(_))
        ),
        "{err:?}"
    );
    assert_eq!(conflicts(&a_store).len(), 1, "the row stays");
    assert_eq!(
        stored(&a_store, &id).unwrap().governance,
        Governance::Accepted
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        regressed,
        "file untouched"
    );
    // --take db still works and puts the record's state back into the file
    let report = sync::resolve(&a, &repo.root, &node_id, Take::Db).unwrap();
    assert_eq!(report.node.governance, Governance::Accepted);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("governance: accepted")
    );
}

/// Slice 9 (F13 "surfaced by `why`", M5-6): a `why` card of a record in conflict says
/// `conflict: true`; a record without a conflict row carries no `conflict` key on the wire;
/// resolving clears the flag.
#[test]
fn why_marks_the_cards_of_records_in_conflict() {
    use kaban_api::{WhyRequest, WhyTarget};
    let repo = Repo::new("why");
    let (a, _) = repo.clone_db("a");
    let edited = decision(&a, "Use SQLite for the project store. It needs no daemon.");
    accept(&a, &edited, &["crates/kaban-store-sqlite/**"]);
    let clean = decision(&a, "Keep the CLI output to one JSON document. Always.");
    accept(&a, &clean, &["crates/kaban-store-sqlite/cli/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let path = repo.record_path("decisions", &edited);
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("no daemon.", "no daemon, ever.")).unwrap();
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild").unwrap().conflicts,
        vec![edited.clone()]
    );

    let why = |api: &KabanApi| {
        api.why(WhyRequest {
            targets: vec![WhyTarget::Path("crates/kaban-store-sqlite/cli/x.rs".into())],
            ..Default::default()
        })
        .unwrap()
    };
    let response = why(&a);
    let card = |id: &str| response.results.iter().find(|c| c.node.id == id).unwrap();
    assert!(card(&edited).conflict, "{:?}", card(&edited));
    assert!(!card(&clean).conflict);
    let wire = serde_json::to_value(&response).unwrap();
    let wire_card = |id: &str| {
        wire["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["node"]["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(wire_card(&edited)["conflict"], json!(true));
    assert!(
        wire_card(&clean).get("conflict").is_none(),
        "omitted when false"
    );

    sync::resolve(
        &a,
        &repo.root,
        &edited.parse().unwrap(),
        kaban_api::sync::Take::Db,
    )
    .unwrap();
    let response = why(&a);
    assert!(response.results.iter().all(|c| !c.conflict));
}

/// Slice 10 (M5-10, M5-19): a tombstoned record has its file deleted and an archived one
/// keeps a file saying so; a held `export` lease never blocks the files (they are written,
/// the `ChangeLog` ack and prune are skipped and the report says `held`); a free lease is taken
/// and the newest changes row acknowledged.
#[test]
fn tombstoned_records_lose_their_file_archived_ones_keep_it_and_a_held_lease_only_skips_the_ack() {
    let repo = Repo::new("lifecycle");
    let (a, a_store) = repo.clone_db("a");
    let gone = decision(&a, "Tombstoned later. Hidden, text kept.");
    let kept = decision(&a, "Archived later. History stays readable.");
    accept(&a, &kept, &["docs/**"]);
    // another process holds the export lease (its sync is in flight)
    let other = kaban_core::LeaseStore::lease_acquire(a_store.as_ref(), "export", "other:1", 60)
        .unwrap()
        .expect("free");
    assert_eq!(other.name, "export");
    let first = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(first.written, 2, "{first:?}");
    assert_eq!(first.lease, kaban_api::sync::LeaseState::Held, "{first:?}");
    assert!(first.outbox_acked.is_none(), "{first:?}");
    assert_eq!(first.outbox_pruned, 0, "{first:?}");

    a.forget(kaban_api::ForgetRequest {
        id: gone.clone(),
        mode: Some(kaban_core::ForgetMode::Tombstone),
    })
    .unwrap();
    a.forget(kaban_api::ForgetRequest {
        id: kept.clone(),
        mode: Some(kaban_core::ForgetMode::Archive),
    })
    .unwrap();
    let second = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(second.deleted, 1, "{second:?}");
    assert_eq!(second.rewritten, 1, "{second:?}");
    assert!(!repo.record_path("decisions", &gone).exists());
    let archived = std::fs::read_to_string(repo.record_path("decisions", &kept)).unwrap();
    assert!(archived.contains("lifecycle: archived"), "{archived}");
    // both records are current afterwards: nothing dirty
    let third = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        (third.written, third.rewritten, third.deleted),
        (0, 0, 0),
        "{third:?}"
    );

    // a free lease: taken, the newest changes row acknowledged, the registered cursor seen
    // by `stats.changes_lag` (0 right after the ack)
    let (b, _) = repo.clone_db("b");
    let _ = decision(&b, "A record of the other clone. Nothing more.");
    let acked = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        acked.lease,
        kaban_api::sync::LeaseState::Acquired,
        "{acked:?}"
    );
    assert!(acked.outbox_acked.is_some_and(|seq| seq > 0), "{acked:?}");
    let stats = b.stats(kaban_api::StatsRequest::default()).unwrap();
    assert_eq!(stats.store["outbox_lag"], 0, "{stats:?}");
}

/// Slice 11 (M5-13): the startup import runs the import in the process repository when it
/// has a `.kaban/` directory and does nothing without one (or without a repository).
#[test]
fn the_startup_import_runs_only_when_the_process_repository_has_a_kaban_directory() {
    let repo = Repo::new("startup");
    let (a, _) = repo.clone_db("a");
    assert!(sync::startup_import(&a).unwrap().is_none(), "no .kaban yet");
    let id = decision(&a, "Startup imports. Once, when the directory exists.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("serialized");
    let (b, b_store) = repo.clone_db("b");
    let report = sync::startup_import(&b)
        .unwrap()
        .expect("the directory exists");
    assert_eq!(report.imported, 1, "{report:?}");
    assert!(stored(&b_store, &id).is_some());
    // no repository bound: nothing runs
    let dir = repo.scratch.0.join("n");
    std::fs::create_dir_all(&dir).unwrap();
    let n_store = Arc::new(SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap());
    let n = KabanApi::new(n_store.clone(), Policy::empty(), n_store.signer(), None);
    assert_eq!(sync::startup_import(&n).unwrap(), None);
}

/// R5-11 (F15, M5-19): a finished `sync` releases the `export` lease, so the next run in the
/// same second acquires it instead of seeing it held for the rest of the 60 s ttl.
#[test]
fn two_immediate_syncs_both_acquire_the_export_lease() {
    use kaban_api::sync::LeaseState;
    let repo = Repo::new("lease-release");
    let (a, _) = repo.clone_db("a");
    let _ = decision(&a, "Release the lease when done. Every path.");
    let first = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(first.lease, LeaseState::Acquired, "{first:?}");
    let second = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(second.lease, LeaseState::Acquired, "{second:?}");
    assert!(second.outbox_acked.is_some(), "{second:?}");
}

/// R5-5 (F13 "both sides untouched"): a `sync` (not `rebuild`) that finds an equal-rev hand
/// edit while the database side of the same record is dirty records the conflict and does not
/// write the record over the file; the edit survives until `resolve` settles it.
#[test]
fn a_dirty_record_in_equal_rev_conflict_is_not_exported_over_the_edited_file() {
    use kaban_api::sync::Take;
    let repo = Repo::new("conflict-dirty");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Use SQLite for the project store. It needs no daemon.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let path = repo.record_path("decisions", &id);
    let original = std::fs::read_to_string(&path).unwrap();
    let edited = original.replace("It needs no daemon.", "It needs no daemon, ever.");
    std::fs::write(&path, &edited).unwrap();
    // the database side is dirty at the same rev (a re-serialization is pending)
    let node_id: kaban_core::NodeId = id.parse().unwrap();
    kaban_core::SyncStore::mark_serialized(a_store.as_ref(), &node_id, 0).unwrap();

    let report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(report.import.conflicts, vec![id.clone()], "{report:?}");
    assert_eq!((report.written, report.rewritten), (0, 0), "{report:?}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        edited,
        "the file side of the conflict survives the sync"
    );
    assert_eq!(conflicts(&a_store).len(), 1);
    // a second sync changes nothing either
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    // resolving in favour of the database is what rewrites the file
    sync::resolve(&a, &repo.root, &node_id, Take::Db).unwrap();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(
        file.contains("It needs no daemon.\n") && !file.contains("ever"),
        "{file}"
    );
    assert_eq!(conflicts(&a_store), []);
}

/// An `KabanApi` over `store` with the redact-class policy (`hunter2` masked), bound to `repo`.
fn redacting_api(repo: &Repo, store: &Arc<SqliteStore>) -> KabanApi {
    let policy =
        Policy::from_file(Policy::parse(REDACT_POLICY.as_bytes()).unwrap(), "t".into()).unwrap();
    KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: Some(repo.root.clone()),
            repo: Some("repo".into()),
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    )
}

/// R5-12 (M5-2, S4): a redact hit on a file at the record's own rev is the policy correction,
/// not a conflict: the masked record is imported at `db rev + 1`, dirty, and the next sync
/// rewrites the file with the mask. A stale file (rev below the database's) with a redact hit
/// is not imported either way and records no conflict.
#[test]
fn an_equal_rev_redaction_imports_the_masked_record_above_both_sides_instead_of_a_conflict() {
    let repo = Repo::new("redact-equal");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Rotate the key. The old value was hunter2 and must go.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let path = repo.record_path("decisions", &id);
    assert!(std::fs::read_to_string(&path).unwrap().contains("hunter2"));
    // the same database, judged by a redact-class policy now (a policy update, S4): the file
    // and the record agree at rev 1 and both still carry the term
    let c_store = a_store;
    let c = redacting_api(&repo, &c_store);
    let report = sync::sync(&c, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(report.import.conflicts, Vec::<String>::new(), "{report:?}");
    assert_eq!(report.import.imported, 1, "{report:?}");
    let masked = stored(&c_store, &id).unwrap();
    assert_eq!(masked.rev, 2, "db rev + 1: {masked:?}");
    assert!(
        masked.text.contains("[REDACTED:old-password]"),
        "{}",
        masked.text
    );
    assert_eq!(
        report.rewritten, 1,
        "the masked record is re-serialized: {report:?}"
    );
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(
        !file.contains("hunter2") && file.contains("rev: 2"),
        "{file}"
    );
    assert_eq!(conflicts(&c_store), []);
    assert_eq!(
        sync::import(&c, &repo.root, "rebuild").unwrap().unchanged,
        1
    );

    // a stale file with a redact hit: the database is ahead, the file is not imported, no
    // conflict; the next export decides the file (the database view goes through the output
    // gate like every file)
    let (second, second_store) = repo.clone_db("second");
    let stale = decision(&second, "Second record. The old value was hunter2 too.");
    sync::sync(&second, &repo.root, &SyncOpts::default()).unwrap();
    accept(&second, &stale, &["ops/**"]); // rev 2 in the database, the file stays at rev 1
    let d = redacting_api(&repo, &second_store);
    let report = sync::import(&d, &repo.root, "rebuild").unwrap();
    assert_eq!(report.conflicts, Vec::<String>::new(), "{report:?}");
    assert_eq!(report.stale_files, 1, "{report:?}");
    assert_eq!(stored(&second_store, &stale).unwrap().rev, 2);
}

/// R5-1 (S1, M5-2 "the whole file is scanned before any typed parse"): a credential that the
/// YAML parser would drop before the structured gate sees it (a comment line, the discarded
/// value of a duplicate key, a file that does not parse at all) still refuses the file: counted
/// `rejected`, audited under the running scope, nothing stored, the file untouched.
#[test]
fn a_credential_the_yaml_parser_would_drop_still_refuses_the_file_before_parse() {
    let repo = Repo::new("raw-gate");
    let (a, _) = repo.clone_db("a");
    let commented = decision(&a, "Comment case. Nothing else.");
    let duplicated = decision(&a, "Duplicate key case. Nothing else.");
    let malformed = decision(&a, "Malformed case. Nothing else.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let poison = |id: &str, edit: &dyn Fn(&str) -> String| {
        let path = repo.record_path("decisions", id);
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("rev: 1", "rev: 2");
        let poisoned = edit(&text);
        assert!(poisoned.contains(CREDENTIAL));
        std::fs::write(&path, &poisoned).unwrap();
        (path, poisoned)
    };
    // a YAML comment line inside the frontmatter
    let (p1, t1) = poison(&commented, &|t| {
        t.replacen("\nkind:", &format!("\n# key: {CREDENTIAL}\nkind:"), 1)
    });
    // a duplicate `summary` key whose first value carries the credential
    let (p2, t2) = poison(&duplicated, &|t| {
        t.replacen(
            "\nsummary:",
            &format!("\nsummary: {CREDENTIAL}\nsummary:"),
            1,
        )
    });
    // YAML that does not parse at all, with the credential in it
    let (p3, t3) = poison(&malformed, &|t| {
        t.replacen("\nkind:", &format!("\nbroken: [{CREDENTIAL}\nkind:"), 1)
    });
    repo.commit("poisoned");

    let (b, b_store) = repo.clone_db("b");
    let before = audit_rows(&repo, "b").len();
    let report = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!(report.rejected, 3, "{report:?}");
    assert_eq!((report.imported, report.skipped), (0, 0), "{report:?}");
    for id in [&commented, &duplicated, &malformed] {
        assert!(stored(&b_store, id).is_none(), "nothing of {id} is stored");
    }
    for (path, text) in [(&p1, &t1), (&p2, &t2), (&p3, &t3)] {
        assert_eq!(
            &std::fs::read_to_string(path).unwrap(),
            text,
            "file untouched"
        );
    }
    let rows = audit_rows(&repo, "b");
    assert!(
        rows.iter()
            .skip(before)
            .filter(|(op, tool, _)| op == "reject" && tool == "rebuild")
            .count()
            >= 3,
        "each refusal is audited: {rows:?}"
    );
    drop(b);
    drop(b_store);
    assert!(!contains_bytes(&db_bytes(&repo, "b"), CREDENTIAL));
}

/// R5-3 (local resource exhaustion): one import counts every directory entry of the three
/// record directories and the bytes of the files it would read; past either budget the
/// import is refused with its fixed text and nothing is imported. The byte case uses sparse
/// files at the per-file cap (no disk is consumed); the entry case uses invalid names, which
/// count like any other entry.
#[test]
fn an_import_past_the_entry_or_byte_budget_is_refused_and_imports_nothing() {
    use kaban_api::sync::SyncError;
    let repo = Repo::new("budget");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Budget case. One real record.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let conventions = repo.root.join(".kaban/adr/conventions");
    std::fs::create_dir_all(&conventions).unwrap();
    // 65 sparse files of exactly 4 MiB: 260 MiB of declared record bytes, above the 256 MiB
    // budget; 65 entries, far below the entry budget
    let mut sparse = Vec::new();
    for _ in 0..65 {
        let path = conventions.join(format!("{}.md", kaban_core::new_id()));
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap();
        sparse.push(path);
    }
    let (b, b_store) = repo.clone_db("b");
    let err = sync::import(&b, &repo.root, "rebuild").unwrap_err();
    assert!(matches!(err, SyncError::ImportBudget), "{err:?}");
    assert_eq!(
        err.to_string(),
        "the .kaban tree exceeds the import budget (entries or bytes); nothing was imported"
    );
    assert!(stored(&b_store, &id).is_none(), "nothing was imported");
    // a sync over the same tree is refused the same way and writes nothing new
    let bytes = repo.kaban_bytes();
    assert!(matches!(
        sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap_err(),
        SyncError::ImportBudget
    ));
    assert_eq!(repo.kaban_bytes(), bytes);
    for p in &sparse {
        std::fs::remove_file(p).unwrap();
    }
    // within budget again: the real record imports
    assert_eq!(sync::import(&b, &repo.root, "rebuild").unwrap().imported, 1);

    // 50,001 entries across the three directories, none a record name: refused as well
    let failures = repo.root.join(".kaban/adr/failures");
    std::fs::create_dir_all(&failures).unwrap();
    for i in 0..50_000usize {
        std::fs::File::create(failures.join(format!("junk-{i}"))).unwrap();
    }
    let err = sync::import(&a, &repo.root, "rebuild").unwrap_err();
    assert!(matches!(err, SyncError::ImportBudget), "{err:?}");
    assert_eq!(stored(&a_store, &id).unwrap().rev, 1, "untouched");
}

/// R5-2 (S1, containment): a symlinked `.kaban/` or a symlinked record directory is refused
/// with a fixed text on import (nothing read, nothing stored) and on export (the write never
/// follows the link: nothing lands outside the repository and the record stays dirty).
#[test]
fn a_symlinked_kaban_or_record_directory_is_refused_on_import_and_on_export() {
    use kaban_api::sync::SyncError;
    use std::os::unix::fs::symlink;
    let repo = Repo::new("containment");
    let (a, _) = repo.clone_db("a");
    let id = decision(&a, "Containment case. Stays inside.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let real = std::fs::read(repo.record_path("decisions", &id)).unwrap();

    // import, `.kaban` itself a symlink to a tree outside the repository
    let outside = repo.scratch.0.join("outside");
    std::fs::create_dir_all(outside.join("adr/decisions")).unwrap();
    std::fs::write(
        outside.join("adr/decisions").join(format!("{id}.md")),
        &real,
    )
    .unwrap();
    let linked = Repo::new("containment-linked");
    symlink(&outside, linked.root.join(".kaban")).unwrap();
    let (b, b_store) = linked.clone_db("b");
    let err = sync::import(&b, &linked.root, "rebuild").unwrap_err();
    assert!(matches!(err, SyncError::Escapes), "{err:?}");
    assert_eq!(
        err.to_string(),
        "the .kaban directory or a record directory is a symlink or leaves the repository"
    );
    assert!(
        stored(&b_store, &id).is_none(),
        "nothing read through the link"
    );
    assert!(
        sync::startup_import(&b).is_err(),
        "the startup import refuses too"
    );

    // import, a record directory a symlink
    std::fs::remove_file(linked.root.join(".kaban")).unwrap();
    std::fs::create_dir_all(linked.root.join(".kaban/adr")).unwrap();
    symlink(
        outside.join("adr/decisions"),
        linked.root.join(".kaban/adr/decisions"),
    )
    .unwrap();
    let err = sync::import(&b, &linked.root, "rebuild").unwrap_err();
    assert!(matches!(err, SyncError::Escapes), "{err:?}");
    assert!(stored(&b_store, &id).is_none());

    // export, a record directory a symlink: the dirty record is not written through it
    let target = repo.scratch.0.join("export-target");
    std::fs::create_dir_all(&target).unwrap();
    let (c, c_store) = linked.clone_db("c");
    let dirty = decision(&c, "Dirty record. Must not leave the repository.");
    std::fs::remove_file(linked.root.join(".kaban/adr/decisions")).unwrap();
    symlink(&target, linked.root.join(".kaban/adr/decisions")).unwrap();
    let err = sync::sync(&c, &linked.root, &SyncOpts::default()).unwrap_err();
    assert!(matches!(err, SyncError::Escapes), "{err:?}");
    assert_eq!(
        std::fs::read_dir(&target).unwrap().count(),
        0,
        "nothing written through the link"
    );
    let node = stored(&c_store, &dirty).unwrap();
    assert_ne!(node.serialized_rev, Some(node.rev), "still dirty");
    // the link replaced by a directory: the record is written inside the repository
    std::fs::remove_file(linked.root.join(".kaban/adr/decisions")).unwrap();
    let report = sync::sync(&c, &linked.root, &SyncOpts::default()).unwrap();
    assert_eq!(report.written, 1, "{report:?}");
    assert!(linked.record_path("decisions", &dirty).is_file());
}

/// An `KabanApi` over `store` with a policy that rejects the word `nightjar`, bound to `repo`.
fn rejecting_api(repo: &Repo, store: &Arc<SqliteStore>) -> KabanApi {
    const REJECT_POLICY: &str = "words:\n  - { term: nightjar, action: reject, id: codename }\n";
    let policy =
        Policy::from_file(Policy::parse(REJECT_POLICY.as_bytes()).unwrap(), "t".into()).unwrap();
    KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: Some(repo.root.clone()),
            repo: Some("repo".into()),
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    )
}

/// R5-6 (M5-32): `resolve` judges the file it is about to write before it touches the
/// database: a reject hit on the prospective rendering refuses the resolution with the record,
/// its rev, the conflict row and the file all as they were. A failed file write after the
/// database moved leaves an explicit recoverable state: the conflict row stays, the record is
/// dirty at its new rev, and a later `resolve` settles it.
#[test]
fn resolve_gates_before_it_mutates_and_keeps_the_conflict_when_the_write_fails() {
    use kaban_api::sync::{SyncError, Take};
    let repo = Repo::new("resolve-order");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Codename nightjar stays. Resolved by hand.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let path = repo.record_path("decisions", &id);
    let original = std::fs::read_to_string(&path).unwrap();
    let edited = original.replace("by hand.", "by hand, later.");
    std::fs::write(&path, &edited).unwrap();
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild")
            .unwrap()
            .conflicts
            .len(),
        1
    );
    let node_id: kaban_core::NodeId = id.parse().unwrap();
    let before = stored(&a_store, &id).unwrap();

    // the gate rejects the rendering: nothing moves
    let strict = rejecting_api(&repo, &a_store);
    let err = sync::resolve(&strict, &repo.root, &node_id, Take::Db).unwrap_err();
    assert!(
        matches!(err, SyncError::Api(kaban_api::ApiError::Rejected { .. })),
        "{err:?}"
    );
    let after = stored(&a_store, &id).unwrap();
    assert_eq!(
        (after.rev, after.serialized_rev),
        (before.rev, before.serialized_rev)
    );
    assert_eq!(conflicts(&a_store).len(), 1, "the row stays");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        edited,
        "file untouched"
    );

    // the write fails (the record directory is a plain file now): the database moved, the
    // record is dirty at its new rev and the conflict row stays for the retry
    let dir = repo.root.join(".kaban/adr/decisions");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"not a directory\n").unwrap();
    let err = sync::resolve(&a, &repo.root, &node_id, Take::Db).unwrap_err();
    assert!(matches!(err, SyncError::Io { .. }), "{err:?}");
    let after = stored(&a_store, &id).unwrap();
    assert_eq!(after.rev, before.rev + 1, "{after:?}");
    assert_ne!(after.serialized_rev, Some(after.rev), "dirty: {after:?}");
    assert_eq!(
        conflicts(&a_store).len(),
        1,
        "the row stays until the file is written"
    );
    // the directory is back: the retry writes the file at a rev above everything and clears
    std::fs::remove_file(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let report = sync::resolve(&a, &repo.root, &node_id, Take::Db).unwrap();
    assert_eq!(report.rev, before.rev + 2);
    assert_eq!(conflicts(&a_store), []);
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(
        file.contains(&format!("rev: {}", before.rev + 2)) && !file.contains("later"),
        "{file}"
    );
    let after = stored(&a_store, &id).unwrap();
    assert_eq!(after.serialized_rev, Some(after.rev));
}

/// R5-4 (the deferral contract): while another process holds the repository lock (an
/// exclusive `flock` on `.kaban/`), `sync` waits out its bound and then reports `deferred`
/// with nothing read, written or stamped (`rebuild` and the startup import share the same
/// acquisition), and `resolve` refuses with its fixed text; once the holder is gone the same
/// calls do their work. Two 10 s waits: the slowest test of this suite on purpose.
#[test]
fn a_held_repository_lock_defers_sync_and_refuses_resolve() {
    use kaban_api::sync::{SyncError, Take};
    let repo = Repo::new("lock-held");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Lock case. Written only by the holder.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    let path = repo.record_path("decisions", &id);
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("holder.", "holder, really.")).unwrap();
    assert_eq!(
        sync::import(&a, &repo.root, "rebuild")
            .unwrap()
            .conflicts
            .len(),
        1
    );
    let pending = decision(&a, "Second record. Dirty while the lock is held.");
    let node_id: kaban_core::NodeId = id.parse().unwrap();

    // a foreign holder (util-linux `flock` on the directory, no fork so the killed process is
    // the one holding the descriptor), alive until killed
    let mut holder = std::process::Command::new("flock")
        .args(["--no-fork", "-x"])
        .arg(repo.root.join(".kaban"))
        .args(["sleep", "120"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("flock(1) is available");
    // wait until the lock is actually held: a non-blocking probe fails
    let held = |root: &Path| {
        !std::process::Command::new("flock")
            .args(["-n", "-x"])
            .arg(root.join(".kaban"))
            .arg("true")
            .status()
            .unwrap()
            .success()
    };
    let t0 = std::time::Instant::now();
    while !held(&repo.root) {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "holder never locked"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let bytes = repo.kaban_bytes();
    let report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert!(report.deferred, "{report:?}");
    assert_eq!((report.written, report.import.files), (0, 0), "{report:?}");
    assert_eq!(repo.kaban_bytes(), bytes, "nothing written");
    let node = stored(&a_store, &pending).unwrap();
    assert_ne!(node.serialized_rev, Some(node.rev), "still dirty");
    let err = sync::resolve(&a, &repo.root, &node_id, Take::Db).unwrap_err();
    assert!(matches!(err, SyncError::Busy), "{err:?}");
    assert_eq!(
        err.to_string(),
        "another kaban holds the repository; nothing was changed, run the command again"
    );
    assert_eq!(conflicts(&a_store).len(), 1);
    holder.kill().unwrap();
    holder.wait().unwrap();

    // the holder is gone: the same calls do their work
    let report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert!(!report.deferred && report.written == 1, "{report:?}");
    sync::resolve(&a, &repo.root, &node_id, Take::Db).unwrap();
    assert_eq!(conflicts(&a_store), []);
}

/// R5-7 (F-W3-1, S1): a generated document is judged in overlapping windows, never refused
/// for its length alone. A record whose fields are each under the per-field cap but render
/// over it together is written, and a digest of 201 accepted records is produced; a reject
/// hit deep in such a record (past the cap) is still caught and the file withheld. The import
/// side reads the long file back unchanged.
#[test]
fn long_generated_documents_are_written_and_a_hit_past_the_cap_is_still_withheld() {
    let repo = Repo::new("long");
    let (a, a_store) = repo.clone_db("a");
    // ~19,200 chars of text and ~8,100 chars of rationale: each under 20,000, together over
    let text = format!(
        "Keep one writer per module. {}",
        "The reasoning repeats itself. ".repeat(640)
    );
    let long = a
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: text.clone(),
            summary: Some("Keep one writer per module".into()),
            tags: vec!["storage".into()],
            props: Some(json!({"rationale": "measured ".repeat(900)})),
            ..Default::default()
        })
        .unwrap()
        .id;
    accept(&a, &long, &["crates/**"]);
    let mut ids = vec![long.clone()];
    for i in 0..200 {
        let id = decision(&a, &format!("Decision {i:03} of the long digest. Keep it."));
        accept(&a, &id, &[&format!("crates/m{i}/**")]);
        ids.push(id);
    }

    let report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!((report.written, report.withheld), (201, 0), "{report:?}");
    assert!(report.digest_updated, "{report:?}");
    let file = std::fs::read_to_string(repo.record_path("decisions", &long)).unwrap();
    assert!(
        file.chars().count() > 20_000,
        "the record file renders over the per-field cap ({} chars)",
        file.chars().count()
    );
    let digest = std::fs::read_to_string(repo.root.join(".kaban/adr/INDEX.md")).unwrap();
    assert!(
        digest.chars().count() > 20_000,
        "{}",
        digest.chars().count()
    );
    assert_eq!(
        digest.matches("](decisions/").count(),
        201,
        "every accepted record once"
    );
    let node = stored(&a_store, &long).unwrap();
    assert_eq!(node.serialized_rev, Some(node.rev), "stamped clean");

    // a fresh clone reads the long file back
    repo.commit("serialized");
    let (b, b_store) = repo.clone_db("b");
    let imported = sync::import(&b, &repo.root, "rebuild").unwrap();
    assert_eq!(imported.imported, 201, "{imported:?}");
    assert_eq!(
        (imported.rejected, imported.skipped),
        (0, 0),
        "{imported:?}"
    );
    assert_eq!(stored(&b_store, &long).unwrap().text, text);

    // a reject-class term past the cap in the rendering: the window scan still catches it
    let strict = rejecting_api(&repo, &a_store);
    let leaky = strict.remember(RememberRequest {
        kind: "Decision".into(),
        text: format!(
            "Keep the codename out. {}The codename is nightjar.",
            "Filler sentence here. ".repeat(900)
        ),
        summary: Some("Keep the codename out".into()),
        props: Some(json!({"rationale": "measured ".repeat(900)})),
        ..Default::default()
    });
    // the strict policy refuses the write itself (the term is in the text): store it under
    // the lenient service instead, then let the strict one export, as a policy that grew
    assert!(
        leaky.is_err(),
        "the strict service refuses the term on the way in"
    );
    let leaky = a
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: format!(
                "Keep the codename out. {}The codename is nightjar.",
                "Filler sentence here. ".repeat(900)
            ),
            summary: Some("Keep the codename out".into()),
            props: Some(json!({"rationale": "measured ".repeat(900)})),
            ..Default::default()
        })
        .unwrap()
        .id;
    let report = sync::sync(&strict, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!((report.written, report.withheld), (0, 1), "{report:?}");
    assert!(
        !repo.record_path("decisions", &leaky).exists(),
        "the leaky record has no file"
    );
    assert!(
        !contains_bytes(&repo.kaban_bytes(), "nightjar"),
        "the term never reached the repository"
    );
    let node = stored(&a_store, &leaky).unwrap();
    assert_ne!(node.serialized_rev, Some(node.rev), "still dirty");
    drop(ids);
}

/// A clone like [`Repo::clone_db`] that records the author identity on `human-pr` events.
fn signing_api(repo: &Repo, store: &Arc<SqliteStore>) -> KabanApi {
    KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: Some(repo.root.clone()),
            repo: Some("repo".into()),
            record_signer_identity: true,
            engine: std::sync::Arc::default(),
        },
    )
}

/// R5-8 (S1, F14): what git prints is host content. A credential-shaped path in the recorded
/// deletions makes the absence evidence unavailable (nothing is tombstoned, the rejection is
/// audited, the fixed argv is unchanged); a credential-shaped path in the status listing makes
/// the commit evidence unavailable (an accepted file imports as `proposed`); a credential-shaped
/// author is never stored as `signer` (the event keeps `human-pr` and the commit). A clean
/// author is recorded.
#[test]
fn credential_shaped_git_paths_and_authors_are_gated_before_they_are_compared_or_stored() {
    let repo = Repo::new("git-gate");
    let (alpha, a_store) = repo.clone_db("a");
    let deleted = decision(&alpha, "Deleted later. Its file is removed in a commit.");
    accept(&alpha, &deleted, &["a/**"]);
    sync::sync(&alpha, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("one record");
    let (bravo, b_store) = repo.clone_db("b");
    sync::import(&bravo, &repo.root, "rebuild").unwrap();

    // a credential-shaped path is deleted in the same history as the record file
    let shaped = repo
        .root
        .join(format!(".kaban/adr/decisions/{CREDENTIAL}.md"));
    std::fs::write(&shaped, "not a record\n").unwrap();
    repo.commit("add a shaped path");
    repo.git(&[
        "rm",
        "-q",
        "--",
        &format!(".kaban/adr/decisions/{CREDENTIAL}.md"),
    ]);
    repo.git(&[
        "rm",
        "-q",
        "--",
        &format!(".kaban/adr/decisions/{deleted}.md"),
    ]);
    repo.commit("remove both");
    let before = audit_rows(&repo, "b").len();
    let report = sync::import(&bravo, &repo.root, "rebuild").unwrap();
    assert_eq!(
        (report.tombstoned_by_absence, report.reserialized),
        (0, 1),
        "the deletion evidence is unavailable, nothing is tombstoned: {report:?}"
    );
    assert_eq!(
        stored(&b_store, &deleted).unwrap().lifecycle,
        Lifecycle::Active
    );
    let rows = audit_rows(&repo, "b");
    assert!(
        rows[before..]
            .iter()
            .any(|(op, tool, field)| op == "reject" && tool == "rebuild" && field == "git"),
        "the hit is audited under the running scope: {:?}",
        &rows[before..]
    );
    drop(bravo);
    drop(b_store);
    assert!(
        !contains_bytes(&db_bytes(&repo, "b"), CREDENTIAL),
        "the shaped path never reached B's database bytes"
    );

    // a credential-shaped untracked path under .kaban/: the status listing is unavailable, so
    // a committed acceptance cannot be proved and imports as proposed
    let (charlie, c_store) = repo.clone_db("c");
    let accepted = decision(
        &alpha,
        "Committed and accepted. Proved by the status listing.",
    );
    accept(&alpha, &accepted, &["c/**"]);
    sync::sync(&alpha, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("an accepted record");
    std::fs::write(
        repo.root
            .join(format!(".kaban/adr/decisions/notes-{CREDENTIAL}.md")),
        "scratch\n",
    )
    .unwrap();
    let report = sync::import(&charlie, &repo.root, "rebuild").unwrap();
    // every acceptance is unproved: this record and the re-serialized `deleted` one (A wrote
    // its file again above, since its deletion evidence was unavailable)
    assert_eq!(report.import_warnings, report.imported, "{report:?}");
    assert_eq!(report.imported, 2, "{report:?}");
    assert_eq!(
        stored(&c_store, &accepted).unwrap().governance,
        Governance::Proposed
    );
    assert!(
        audit_rows(&repo, "c")
            .iter()
            .any(|(op, tool, field)| op == "reject" && tool == "rebuild" && field == "git"),
    );
    std::fs::remove_file(
        repo.root
            .join(format!(".kaban/adr/decisions/notes-{CREDENTIAL}.md")),
    )
    .unwrap();
    // with the listing clean again the same file proves its acceptance in a fresh clone
    let (delta, d_store) = repo.clone_db("d");
    let report = sync::import(&delta, &repo.root, "rebuild").unwrap();
    assert_eq!(
        (report.imported, report.import_warnings),
        (2, 0),
        "both committed files prove their acceptance: {report:?}"
    );
    assert_eq!(
        stored(&d_store, &accepted).unwrap().governance,
        Governance::Accepted
    );

    // the author: a clean one is recorded as the signer, a credential-shaped one is omitted
    // while the event keeps human-pr and the commit
    let echo_dir = repo.scratch.0.join("e");
    std::fs::create_dir_all(&echo_dir).unwrap();
    let e_store =
        Arc::new(SqliteStore::open(&echo_dir.join("kaban.db"), &echo_dir.join("keys")).unwrap());
    let echo = signing_api(&repo, &e_store);
    let report = sync::import(&echo, &repo.root, "rebuild").unwrap();
    assert_eq!(report.import_warnings, 0, "{report:?}");
    let ev = events(&e_store, &accepted);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].signer.as_deref(), Some("fixture@example.invalid"));

    let shaped_author = format!("{CREDENTIAL}@example.invalid");
    repo.git(&["config", "user.email", &shaped_author]);
    let second = decision(&alpha, "Signed by a shaped author. The signer is omitted.");
    accept(&alpha, &second, &["e/**"]);
    sync::sync(&alpha, &repo.root, &SyncOpts::default()).unwrap();
    let head = repo.commit("a shaped author");
    let before = audit_rows(&repo, "e").len();
    let report = sync::import(&echo, &repo.root, "rebuild").unwrap();
    assert_eq!(report.import_warnings, 0, "{report:?}");
    let ev = events(&e_store, &second);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].signer_kind, kaban_core::SignerKind::HumanPr);
    assert_eq!(ev[0].commit_sha.as_deref(), Some(head.as_str()));
    assert_eq!(ev[0].signer, None, "the shaped author is never stored");
    assert!(
        audit_rows(&repo, "e")[before..]
            .iter()
            .any(|(op, tool, field)| op == "reject" && tool == "rebuild" && field == "git"),
    );
    drop(echo);
    drop(e_store);
    assert!(
        !contains_bytes(&db_bytes(&repo, "e"), CREDENTIAL),
        "the shaped author never reached E's database bytes"
    );
    drop(a_store);
}

/// R5-10: the digest and the `.gitignore` are compared against their current bytes only
/// after a metadata check. A current file above `MAX_GENERATED_BYTES` is never read: `sync`
/// refuses with a fixed text (exit 4) and leaves the file exactly as it was (the record files
/// of the same run are written, since they are replaced without a read). A normal digest is
/// replaced and then left unchanged.
#[test]
fn an_oversized_digest_or_gitignore_is_refused_untouched_and_a_normal_one_is_replaced() {
    use kaban_api::sync::{MAX_GENERATED_BYTES, SyncError};
    let repo = Repo::new("oversized");
    let (a, a_store) = repo.clone_db("a");
    let id = decision(&a, "Cap the reads. Metadata first.");
    accept(&a, &id, &["src/**"]);
    let kaban = repo.root.join(".kaban");
    std::fs::create_dir_all(kaban.join("adr")).unwrap();
    let digest = kaban.join("adr/INDEX.md");
    let f = std::fs::File::create(&digest).unwrap();
    f.set_len(MAX_GENERATED_BYTES + 1).unwrap();
    drop(f);
    let before = std::fs::metadata(&digest).unwrap();

    let err = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap_err();
    assert!(matches!(err, SyncError::GeneratedTooLarge), "{err:?}");
    assert_eq!(
        err.to_string(),
        "a generated file under .kaban is larger than the replacement cap; it was left as it is"
    );
    assert!(matches!(
        kaban_api::ApiError::from(err),
        kaban_api::ApiError::Invalid(_)
    ));
    let after = std::fs::metadata(&digest).unwrap();
    assert_eq!(after.len(), MAX_GENERATED_BYTES + 1, "untouched");
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    assert!(
        repo.record_path("decisions", &id).exists(),
        "the record file of the same run is written"
    );
    let node = stored(&a_store, &id).unwrap();
    assert_eq!(node.serialized_rev, Some(node.rev));

    // the same for .gitignore, with a normal digest back in place
    std::fs::remove_file(&digest).unwrap();
    let gitignore = kaban.join(".gitignore");
    let f = std::fs::File::create(&gitignore).unwrap();
    f.set_len(MAX_GENERATED_BYTES + 1).unwrap();
    drop(f);
    let err = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap_err();
    assert!(matches!(err, SyncError::GeneratedTooLarge), "{err:?}");
    assert_eq!(
        std::fs::metadata(&gitignore).unwrap().len(),
        MAX_GENERATED_BYTES + 1
    );
    assert!(digest.exists(), "the digest was written before the refusal");

    // both normal again: replaced, then unchanged
    std::fs::write(&gitignore, "stale\n").unwrap();
    std::fs::write(&digest, "stale\n").unwrap();
    let report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert!(report.digest_updated, "{report:?}");
    assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "kaban.db*\n");
    let text = std::fs::read_to_string(&digest).unwrap();
    assert!(text.contains("Cap the reads"), "{text}");
    let report = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert!(!report.digest_updated, "{report:?}");
    assert_eq!(std::fs::read_to_string(&digest).unwrap(), text);
}

/// SS-06 (review d-5): the default sink is installed before it consumes: its cursor exists from
/// the first `sync` on, even one that finds nothing to acknowledge, so pruning keeps every row
/// written after it however hard another consumer prunes.
#[test]
fn the_export_cursor_exists_before_the_first_row_is_consumed_and_protects_history_from_the_start() {
    use kaban_core::ChangeLogStore;
    let repo = Repo::new("register-first");
    let (a, store) = repo.clone_db("a");
    let first = sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(first.outbox_acked, None, "an empty log: {first:?}");
    for i in 0..5 {
        let _ = decision(
            &a,
            &format!("Decision {i} written after the sink was installed."),
        );
    }
    let rows = store.changes_read(0, 1_000).unwrap().len();
    assert!(rows >= 5, "{rows}");
    // the hardest prune another consumer could ask for: without a registered cursor only the
    // newest row would survive
    store.changes_prune(0).unwrap();
    assert_eq!(
        store.changes_read(0, 1_000).unwrap().len(),
        rows,
        "every row after the export cursor survives"
    );
}
