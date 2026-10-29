//! Public-behaviour tests of the one-time layout move (engine-spec EN-F4 "golden promise,
//! narrowed and restated", OWN-46; engine-plan E1.6; core-spec F13): the pre-engine
//! `.kaban/<dir>/<id>.md` and `.kaban/DECISIONS.md` layout becomes
//! `.kaban/<ontology>/<dir>/<id>.md` and `.kaban/<ontology>/INDEX.md` on the first `kaban
//! sync`, with `git mv` semantics inside a repository, byte-identical files, no `rev` change,
//! and nothing written by the second run. The M5 R5-2 symlink rule and the `conflicts` row
//! for a target with other bytes are the negatives.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::sync::{self, SyncOpts};
use kaban_api::{AcceptRequest, ApiConfig, Caller, KabanApi, RememberRequest};
use kaban_core::{Actor, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";
const WS: &str = "nightjar-service";

/// A throwaway git repository under a scratch directory.
struct Repo {
    scratch: Scratch,
    root: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new("kaban-layout", name);
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

    fn kaban(&self) -> PathBuf {
        self.root.join(".kaban")
    }

    /// Every file under `.kaban/` as `(relative path, bytes)`, sorted.
    fn tree(&self) -> Vec<(String, Vec<u8>)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else {
                    let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                    out.push((rel, std::fs::read(&p).unwrap()));
                }
            }
        }
        let mut files = Vec::new();
        walk(&self.kaban(), &self.kaban(), &mut files);
        files.sort();
        files
    }

    /// Put a serialized tree back into the pre-engine layout by hand, exactly as `docs/developer/SYNC.md`
    /// documents the rollback (`git mv` back, the digest renamed, the `ontology:` key the
    /// pre-engine files never carried stripped), then commit it. The result is the M5 layout:
    /// `.kaban/<dir>/<id>.md`, `.kaban/DECISIONS.md`, `.kaban/.gitignore`.
    fn make_pre_engine(&self) {
        self.commit("engine layout");
        let adr = self.kaban().join("adr");
        if adr.is_dir() {
            for entry in std::fs::read_dir(&adr).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "INDEX.md" {
                    self.git(&["mv", "--", ".kaban/adr/INDEX.md", ".kaban/DECISIONS.md"]);
                } else {
                    self.git(&[
                        "mv",
                        "--",
                        &format!(".kaban/adr/{name}"),
                        &format!(".kaban/{name}"),
                    ]);
                }
            }
            let _ = std::fs::remove_dir(&adr);
        }
        for (rel, bytes) in self.tree() {
            if Path::new(&rel).extension().is_none_or(|e| e != "md") || rel == "DECISIONS.md" {
                continue;
            }
            let text = String::from_utf8(bytes).unwrap();
            let stripped: String = text.lines().filter(|l| !l.starts_with("ontology: ")).fold(
                String::new(),
                |mut out, l| {
                    out.push_str(l);
                    out.push('\n');
                    out
                },
            );
            std::fs::write(self.kaban().join(&rel), stripped).unwrap();
        }
        self.commit("pre-engine layout");
    }
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

fn record(api: &KabanApi, kind: &str, text: &str) -> String {
    api.remember(RememberRequest {
        kind: kind.into(),
        text: text.into(),
        summary: Some(text.split('.').next().unwrap().to_owned()),
        // `rationale` is a Decision prop (the digest and the block render it)
        props: (kind == "Decision").then(|| json!({"rationale": "embedded, zero ops"})),
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

fn revs(store: &SqliteStore, ids: &[&String]) -> Vec<i64> {
    ids.iter()
        .map(|id| {
            kaban_core::MemoryStore::get(store, &id.parse().unwrap(), true)
                .unwrap()
                .unwrap()
                .rev
        })
        .collect()
}

/// A populated repository in the pre-engine layout: two accepted Decisions, one accepted
/// Convention and one Failure, serialized and committed, plus the database that holds them.
fn populated(name: &str) -> (Repo, KabanApi, Arc<SqliteStore>, Vec<String>) {
    let repo = Repo::new(name);
    let (api, store) = repo.clone_db("a");
    let d1 = record(
        &api,
        "Decision",
        "Use SQLite for the project store. No daemon.",
    );
    accept(&api, &d1, &["crates/kaban-store-sqlite/**"]);
    let d2 = record(
        &api,
        "Decision",
        "Keep one JSON document per CLI run. Always.",
    );
    accept(&api, &d2, &["crates/kaban/**"]);
    let c1 = record(
        &api,
        "Convention",
        "Name tests by behaviour. Not by method.",
    );
    accept(&api, &c1, &["**/tests/**"]);
    let f1 = record(
        &api,
        "Failure",
        "The numbat queue stalled under load. Twice.",
    );
    sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    repo.make_pre_engine();
    (repo, api, store, vec![d1, d2, c1, f1])
}

/// Slice 1 (EN-F4 golden promise, OWN-46): one `sync` over a committed pre-engine tree moves
/// every record file under `.kaban/adr/<dir>/` with identical bytes, removes
/// `.kaban/DECISIONS.md`, writes `.kaban/adr/INDEX.md` with the digest's bytes, records the
/// renames in the index (`git mv`), changes no `rev`, and a second `sync` writes nothing.
#[test]
fn the_first_sync_moves_a_pre_engine_tree_once_with_git_mv_and_identical_bytes() {
    // Arrange
    let (repo, api, store, ids) = populated("move");
    let before = repo.tree();
    let names: Vec<&str> = before.iter().map(|(p, _)| p.as_str()).collect();
    assert!(names.contains(&"DECISIONS.md"), "{names:?}");
    assert!(
        names.iter().any(|n| n.starts_with("decisions/")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("conventions/")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("failures/")),
        "{names:?}"
    );
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    let id_refs: Vec<&String> = ids.iter().collect();
    let revs_before = revs(&store, &id_refs);
    let old_digest = std::fs::read(repo.kaban().join("DECISIONS.md")).unwrap();

    // Act
    let first = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();

    // Assert: the new layout, byte for byte
    let after = repo.tree();
    let expected: Vec<(String, Vec<u8>)> = before
        .iter()
        .map(|(p, b)| {
            let moved = match p.as_str() {
                "DECISIONS.md" => "adr/INDEX.md".to_owned(),
                ".gitignore" => p.clone(),
                other => format!("adr/{other}"),
            };
            (moved, b.clone())
        })
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .collect();
    let paths = |t: &[(String, Vec<u8>)]| t.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&after), paths(&expected), "{first:?}");
    for ((path, got), (_, want)) in after.iter().zip(&expected) {
        assert!(got == want, "bytes differ after the move: {path}");
    }
    assert!(!repo.kaban().join("DECISIONS.md").exists());
    assert!(!repo.kaban().join("decisions").exists());
    assert_eq!(
        std::fs::read(repo.kaban().join("adr/INDEX.md")).unwrap(),
        old_digest,
        "the digest's contents are unchanged; only its path moved"
    );
    assert_eq!(
        (
            first.written,
            first.rewritten,
            first.tombstones,
            first.deleted
        ),
        (0, 0, 0, 0),
        "{first:?}"
    );
    assert_eq!(first.import.imported, 0, "{first:?}");
    assert_eq!(first.import.unchanged, 4, "{first:?}");
    assert!(first.import.conflicts.is_empty(), "{first:?}");
    assert_eq!(revs(&store, &id_refs), revs_before, "no rev change");
    // `git mv`: the index records renames, nothing is untracked
    let porcelain = repo.git(&["status", "--porcelain"]);
    let renames = porcelain.lines().filter(|l| l.starts_with("R ")).count();
    assert_eq!(renames, 5, "{porcelain}");
    assert!(!porcelain.contains("??"), "{porcelain}");
    assert!(
        porcelain.contains(".kaban/DECISIONS.md -> .kaban/adr/INDEX.md"),
        "{porcelain}"
    );
    repo.commit("engine layout");

    // Act again: idempotent
    let second = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();

    // Assert
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
    assert_eq!(repo.tree(), after, "a second sync writes nothing");
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
}

/// The fixture ontology `incidents` (`Incident{serialize: {dir: incidents}, export: {digest:
/// false, marker: true, heading: Incidents}}`, `Outage{serialize: none}`) loaded into `api`'s
/// registry from `crates/kaban-core/fixtures/ontology/incidents.yaml`.
fn load_incidents(api: &KabanApi, scratch: &Path) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kaban-core/fixtures/ontology/incidents.yaml");
    let target = scratch.join("incidents.yaml");
    std::fs::copy(fixture, &target).unwrap();
    api.registry().load_file(api, &target).unwrap();
}

/// Slice 2 (EN-F4 `Incident` scenario, OWN-46): a serialized kind of a loaded ontology lands
/// under `.kaban/incidents/incidents/<id>.md` with `ontology: incidents@1`; `export --format
/// markdown` renders an `Incidents` block that `--ontology adr` omits; the record is absent
/// from `.kaban/adr/INDEX.md` and `.kaban/incidents/INDEX.md` does not exist (`digest:
/// false`); an `Outage{serialize: none}` writes no file; an unknown `--ontology` is refused
/// with the fixed class.
#[test]
fn an_incident_serializes_under_its_ontology_and_exports_only_when_its_ontology_is_selected() {
    use kaban_api::export::{self, ExportFormat, ExportOpts, ExportReport};

    // Arrange
    let repo = Repo::new("incident");
    let registry =
        Arc::new(kaban_api::OntologyRegistry::new(Some(repo.scratch.0.clone())).unwrap());
    let (api, store) = repo.clone_db("a");
    let api = api.with_registry(Arc::clone(&registry));
    load_incidents(&api, &repo.scratch.0);
    // the briefing the block renders from takes its kinds from the registry the service
    // holds (E1.6); the store carries no governed set of its own (E1.8)
    drop((api, store));
    let dir = repo.scratch.0.join("a");
    let store = Arc::new(SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap());
    let api = KabanApi::with_config(
        store.clone(),
        Policy::empty(),
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
    .with_registry(registry);
    let decision = record(
        &api,
        "Decision",
        "Use SQLite for the project store. No daemon.",
    );
    accept(&api, &decision, &["crates/**"]);
    let incident = record(
        &api,
        "incidents:Incident",
        "The numbat queue stalled. Twice.",
    );
    accept(&api, &incident, &["ops/queue/**"]);
    let outage = record(
        &api,
        "incidents:Outage",
        "The region went dark. For an hour.",
    );

    // Act
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();

    // Assert: the files
    assert_eq!(report.written, 2, "{report:?}");
    let incident_file = repo
        .kaban()
        .join("incidents/incidents")
        .join(format!("{incident}.md"));
    let text = std::fs::read_to_string(&incident_file).unwrap();
    assert!(text.contains("\nkind: incidents:Incident\n"), "{text}");
    assert!(text.contains("\nontology: incidents@1\n"), "{text}");
    let decision_text = std::fs::read_to_string(
        repo.kaban()
            .join("adr/decisions")
            .join(format!("{decision}.md")),
    )
    .unwrap();
    assert!(
        decision_text.contains("\nontology: adr@1\n"),
        "{decision_text}"
    );
    let names: Vec<String> = repo.tree().into_iter().map(|(p, _)| p).collect();
    assert!(
        !names.iter().any(|n| n.contains(&outage)),
        "serialize: none writes no file: {names:?}"
    );
    assert!(
        !repo.kaban().join("incidents/INDEX.md").exists(),
        "{names:?}"
    );
    let adr_digest = std::fs::read_to_string(repo.kaban().join("adr/INDEX.md")).unwrap();
    assert!(adr_digest.contains(&decision), "{adr_digest}");
    assert!(!adr_digest.contains(&incident), "{adr_digest}");

    // Assert: the export block per selection
    let render = |ontologies: Vec<String>| match export::export(
        &api,
        &ExportOpts {
            format: ExportFormat::Markdown,
            write: None,
            ontologies,
        },
    )
    .unwrap()
    {
        ExportReport::Rendered { block, records, .. } => (block, records),
        other @ ExportReport::Written { .. } => panic!("{other:?}"),
    };
    let (all, all_records) = render(Vec::new());
    assert_eq!(all_records, 2, "{all}");
    assert!(all.contains("\n## Decisions\n"), "{all}");
    assert!(all.contains("\n## Conventions\n"), "{all}");
    assert!(all.contains("\n## Incidents\n"), "{all}");
    assert!(
        all.contains(&format!(
            "([{incident}](.kaban/incidents/incidents/{incident}.md))"
        )),
        "{all}"
    );
    let (adr, adr_records) = render(vec!["adr".into()]);
    assert_eq!(adr_records, 1, "{adr}");
    assert!(!adr.contains("Incidents"), "{adr}");
    assert!(!adr.contains(&incident), "{adr}");
    let (only_incidents, n) = render(vec!["incidents".into()]);
    assert_eq!(n, 1, "{only_incidents}");
    assert!(!only_incidents.contains("## Decisions"), "{only_incidents}");
    assert!(only_incidents.contains(&incident), "{only_incidents}");
    assert_eq!(render(vec!["all".into()]).0, all);

    // Negative: an unloaded ontology name is a fixed-class refusal (exit 2 at the seam)
    let err = export::export(
        &api,
        &ExportOpts {
            format: ExportFormat::Markdown,
            write: None,
            ontologies: vec!["runbooks".into()],
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, export::ExportError::OntologyUnknown),
        "{err:?}"
    );
    assert!(!err.to_string().contains("runbooks"), "{err}");
    let err = sync::sync(
        &api,
        &repo.root,
        &SyncOpts {
            ontologies: kaban_api::export::OntologySelection::Named(vec!["runbooks".into()]),
            ..SyncOpts::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, sync::SyncError::OntologyUnknown), "{err:?}");
    assert!(matches!(
        kaban_api::ApiError::from(err),
        kaban_api::ApiError::Rejected { field, rules } if field == "ontology" && rules == ["ontology_unknown"]
    ));
}

/// Slice 3 (OWN-47): a file whose `ontology: incidents@2` names a version this process has not
/// loaded is refused with the fixed class `ontology_version_unknown` (counted, nothing stored,
/// the file untouched); a key-less file of a non-`adr` kind is refused too (`skipped`), while
/// a key-less `adr` file is read as `adr@1`.
#[test]
fn a_file_naming_an_unloaded_ontology_version_is_refused_and_left_untouched() {
    // Arrange
    let repo = Repo::new("version");
    let registry =
        Arc::new(kaban_api::OntologyRegistry::new(Some(repo.scratch.0.clone())).unwrap());
    let (api, store) = repo.clone_db("a");
    let api = api.with_registry(Arc::clone(&registry));
    load_incidents(&api, &repo.scratch.0);
    let incident = record(
        &api,
        "incidents:Incident",
        "The numbat queue stalled. Twice.",
    );
    let decision = record(
        &api,
        "Decision",
        "Use SQLite for the project store. No daemon.",
    );
    sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    let incident_path = repo
        .kaban()
        .join("incidents/incidents")
        .join(format!("{incident}.md"));
    let written = std::fs::read_to_string(&incident_path).unwrap();
    assert!(written.contains("\nontology: incidents@1\n"), "{written}");
    // another clone advanced the record under a version of the ontology this process has
    // not loaded (rev 1 -> 2, the text edited): the file says `incidents@2`
    let advanced = written
        .replacen("ontology: incidents@1\n", "ontology: incidents@2\n", 1)
        .replacen("rev: 1\n", "rev: 2\n", 1)
        .replacen("stalled. Twice.", "stalled. Three times.", 2);
    std::fs::write(&incident_path, &advanced).unwrap();
    repo.commit("advanced under incidents@2");
    let before = std::fs::metadata(&incident_path)
        .unwrap()
        .modified()
        .unwrap();

    // Act
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();

    // Assert: refused, counted, untouched; the database still holds rev 1
    assert_eq!(report.import.version_unknown, 1, "{report:?}");
    assert_eq!(report.import.imported, 0, "{report:?}");
    assert_eq!(report.import.unchanged, 1, "the Decision: {report:?}");
    assert!(report.import.conflicts.is_empty(), "{report:?}");
    assert_eq!(std::fs::read_to_string(&incident_path).unwrap(), advanced);
    assert_eq!(
        std::fs::metadata(&incident_path)
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    let node = kaban_core::MemoryStore::get(store.as_ref(), &incident.parse().unwrap(), true)
        .unwrap()
        .unwrap();
    assert_eq!(node.rev, 1);
    assert!(node.text.ends_with("Twice."), "{}", node.text);
    assert_eq!(report.written + report.rewritten, 0, "{report:?}");

    // Negative: a key-less file of a non-adr kind is not a pre-engine file: skipped
    let keyless = written.replacen("ontology: incidents@1\n", "", 1);
    std::fs::write(&incident_path, &keyless).unwrap();
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(report.import.skipped, 1, "{report:?}");
    assert_eq!(report.import.version_unknown, 0, "{report:?}");
    assert_eq!(std::fs::read_to_string(&incident_path).unwrap(), keyless);

    // and a key-less adr file reads as adr@1 (unchanged at equal rev)
    let decision_path = repo
        .kaban()
        .join("adr/decisions")
        .join(format!("{decision}.md"));
    let adr_keyless =
        std::fs::read_to_string(&decision_path)
            .unwrap()
            .replacen("ontology: adr@1\n", "", 1);
    std::fs::write(&decision_path, &adr_keyless).unwrap();
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(report.import.unchanged, 1, "{report:?}");
    assert_eq!(report.rewritten, 0, "{report:?}");
    assert_eq!(
        std::fs::read_to_string(&decision_path).unwrap(),
        adr_keyless
    );
}

/// Negative 1 (R5-2 on the new directories): a `.kaban/<ontology>` path component that is a
/// symlink is refused with the fixed containment class and nothing is moved, read or
/// written; the pre-engine tree is left exactly as it was.
#[test]
fn a_symlinked_ontology_directory_refuses_the_move_and_nothing_is_touched() {
    use kaban_api::sync::SyncError;
    use std::os::unix::fs::symlink;

    // Arrange: a committed pre-engine tree and `.kaban/adr` pointing outside the repository
    let (repo, api, _store, _ids) = populated("symlink");
    let outside = repo.scratch.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, repo.kaban().join("adr")).unwrap();
    let before = repo.tree();

    // Act
    let err = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap_err();

    // Assert
    assert!(matches!(err, SyncError::Escapes), "{err:?}");
    assert_eq!(
        err.to_string(),
        "the .kaban directory or a record directory is a symlink or leaves the repository"
    );
    assert_eq!(repo.tree(), before, "nothing moved or written");
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing through the link"
    );
    assert!(repo.kaban().join("DECISIONS.md").is_file());

    // the link replaced by nothing: the move runs
    std::fs::remove_file(repo.kaban().join("adr")).unwrap();
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        report.layout.as_ref().map(|m| m.moved),
        Some(4),
        "{report:?}"
    );
}

/// Negative 2: a move target that already exists with other bytes is a `conflicts` row keyed
/// by the file id and neither file is overwritten; a target with the same bytes just lets
/// the pre-engine copy go. `kaban resolve` on the row of an id the database does not hold is
/// the fixed `NotFound` class (no panic, no path), and the conflicts listing stays usable.
#[test]
fn a_move_target_with_other_bytes_is_a_conflict_row_and_is_never_overwritten() {
    use kaban_api::sync::{SyncError, Take};

    // Arrange: a committed pre-engine tree; one target pre-planted with other bytes (a file
    // that was hand-edited after someone moved it early), one with identical bytes, and a
    // stray pre-engine file of an id the database never held
    let (repo, api, store, ids) = populated("conflict");
    let edited = &ids[0];
    let same = &ids[1];
    let stray = kaban_core::new_id();
    let old = |id: &str| repo.kaban().join("decisions").join(format!("{id}.md"));
    let new = |id: &str| repo.kaban().join("adr/decisions").join(format!("{id}.md"));
    std::fs::create_dir_all(repo.kaban().join("adr/decisions")).unwrap();
    let edited_old = std::fs::read(old(edited)).unwrap();
    let planted = b"---\nnot the same bytes\n---\n".to_vec();
    std::fs::write(new(edited), &planted).unwrap();
    std::fs::copy(old(same), new(same)).unwrap();
    let stray_old = std::fs::read(old(same)).unwrap();
    std::fs::write(old(&stray), &stray_old).unwrap();
    std::fs::write(new(&stray), b"other stray bytes\n").unwrap();
    repo.commit("planted");

    // Act
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();

    // Assert: the rows, the files
    let layout = report.layout.as_ref().expect("a move happened");
    let mut conflicted: Vec<&str> = layout.conflicts.iter().map(|c| c.id.as_str()).collect();
    conflicted.sort_unstable();
    let mut expected = vec![edited.as_str(), stray.as_str()];
    expected.sort_unstable();
    assert_eq!(conflicted, expected, "{layout:?}");
    assert_eq!(
        layout.moved, 3,
        "the duplicate Decision, the Convention, the Failure: {layout:?}"
    );
    assert_eq!(
        std::fs::read(old(edited)).unwrap(),
        edited_old,
        "source untouched"
    );
    assert_eq!(
        std::fs::read(new(edited)).unwrap(),
        planted,
        "target never overwritten"
    );
    assert!(!old(same).exists(), "an identical copy lets the source go");
    assert!(old(&stray).exists() && new(&stray).exists());
    let rows = kaban_core::SyncStore::conflicts_for(
        store.as_ref(),
        &[edited.parse().unwrap(), stray.parse().unwrap()],
    )
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|r| r.file_rev == 0 && r.db_rev == 0));
    assert!(
        rows.iter()
            .all(|r| r.file_hash.len() == 64 && r.db_hash.len() == 64)
    );
    // the record in conflict is excluded from export until resolved (R5-5): the planted
    // bytes stay, the record stays dirty
    assert_eq!(report.rewritten, 0, "{report:?}");
    // the second run: the stray and the edited source stay, nothing else moves
    let again = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    let layout = again
        .layout
        .as_ref()
        .expect("the conflicts are reported again");
    assert_eq!(layout.moved, 0, "{layout:?}");
    assert_eq!(layout.conflicts.len(), 2, "{layout:?}");
    assert_eq!(std::fs::read(new(edited)).unwrap(), planted);

    // the listing works with a row whose id no node holds (the stray)
    let listed = kaban_core::SyncStore::conflicts(store.as_ref(), WS).unwrap();
    assert!(listed.iter().any(|r| r.node_id == *edited), "{listed:?}");
    assert!(
        !listed.iter().any(|r| r.node_id == stray),
        "a row without a node is not listed under a workspace: {listed:?}"
    );

    // `resolve` on the stray's row: the fixed NotFound class, no panic, no path in the text
    let err = sync::resolve(&api, &repo.root, &stray.parse().unwrap(), Take::Db).unwrap_err();
    assert!(
        matches!(err, SyncError::NotFound(ref id) if *id == stray),
        "{err:?}"
    );
    assert!(!err.to_string().contains(".kaban"), "{err}");
    // `resolve --take db` on the edited record: the database wins, the file is rewritten at
    // the new path, the row is gone
    let node_id = edited.parse().unwrap();
    let resolved = sync::resolve(&api, &repo.root, &node_id, Take::Db).unwrap();
    assert!(resolved.rev > 0);
    let text = std::fs::read_to_string(new(edited)).unwrap();
    assert!(text.contains("Use SQLite for the project store"), "{text}");
    assert!(text.contains("\nontology: adr@1\n"), "{text}");
    assert_eq!(
        kaban_core::SyncStore::conflicts_for(store.as_ref(), &[node_id]).unwrap(),
        vec![]
    );
}

// --- E1.7a (astra interim review RE2-4..RE2-7): custom governed kinds through `sync` ---

/// An agent-tier serializable custom kind with `dedup: title+scope` (`ops:Runbook`), the
/// counterpart of the human-tier `incidents:Incident`: written into `scratch` and loaded.
const OPS_YAML: &str = "# E1.7a fixture: an agent-tier serialized kind with the title+scope dedup
ontology: ops
version: 1
kinds:
  Runbook:
    description: \"An operational procedure.\"
    governed: {acceptance: agent, serialize: {dir: runbooks, format: md-frontmatter},
               export: {digest: false, marker: false}, display: {label: Runbook, plural: Runbooks},
               governs_paths: true, dedup: title+scope}
rels: {}
";

/// A clone with its own registry holding the shipped files plus `incidents` and `ops`.
fn custom_clone(repo: &Repo, name: &str) -> (KabanApi, Arc<SqliteStore>) {
    let (api, store) = repo.clone_db(name);
    let registry =
        Arc::new(kaban_api::OntologyRegistry::new(Some(repo.scratch.0.clone())).unwrap());
    let api = api.with_registry(registry);
    load_incidents(&api, &repo.scratch.0);
    let ops = repo.scratch.0.join("ops.yaml");
    std::fs::write(&ops, OPS_YAML).unwrap();
    api.registry().load_file(&api, &ops).unwrap();
    (api, store)
}

fn node_of(store: &SqliteStore, id: &str) -> kaban_core::Node {
    kaban_core::MemoryStore::get(store, &id.parse().unwrap(), true)
        .unwrap()
        .unwrap()
}

/// RE2-5 (Major): the import proof requirement follows the record's governance, not the
/// literal kind name: an `incidents:Incident{acceptance: human}` claimed `accepted` by an
/// uncommitted file imports as `proposed` with a warning (exactly as a Decision does), the
/// committed file imports as `accepted` with the `human_pr` proof; an `ops:Runbook{acceptance:
/// agent}` claimed `accepted` by an uncommitted file imports as `accepted` (the agent tier).
#[test]
fn import_requires_the_acceptance_proof_for_every_human_tier_kind_and_none_for_agent_tier() {
    // Arrange: clone a accepts one of each, serialized but not committed
    let repo = Repo::new("proof-by-governance");
    let (a, _) = custom_clone(&repo, "a");
    let incident = record(&a, "incidents:Incident", "The numbat queue stalled. Twice.");
    accept(&a, &incident, &["ops/**"]);
    let runbook = record(&a, "ops:Runbook", "Rotate the queue. Then restart it.");
    accept(&a, &runbook, &["ops/**"]);
    let decision = record(
        &a,
        "Decision",
        "Use SQLite for the project store. No daemon.",
    );
    accept(&a, &decision, &["crates/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();

    // Act: clone b imports the uncommitted tree
    let (b, b_store) = custom_clone(&repo, "b");
    let report = sync::import(&b, &repo.root, "rebuild").unwrap();

    // Assert: the two human-tier records are demoted alike, the agent-tier one is not
    assert_eq!(report.imported, 3, "{report:?}");
    assert_eq!(report.import_warnings, 2, "{report:?}");
    assert_eq!(
        node_of(&b_store, &incident).governance,
        kaban_core::Governance::Proposed,
        "human tier without proof"
    );
    assert_eq!(
        node_of(&b_store, &decision).governance,
        kaban_core::Governance::Proposed
    );
    assert!(
        kaban_core::GovernanceStore::events(b_store.as_ref(), &incident.parse().unwrap())
            .unwrap()
            .is_empty(),
        "no acceptance event without proof"
    );
    let runbook_b = node_of(&b_store, &runbook);
    assert_eq!(runbook_b.governance, kaban_core::Governance::Accepted);
    let ev =
        kaban_core::GovernanceStore::events(b_store.as_ref(), &runbook.parse().unwrap()).unwrap();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].signer_kind, kaban_core::SignerKind::Agent);

    // and with the proof (the committed file) the Incident imports accepted with `human_pr`
    let head = repo.commit("serialize the records");
    let (c, c_store) = custom_clone(&repo, "c");
    let report = sync::import(&c, &repo.root, "rebuild").unwrap();
    assert_eq!(
        (report.imported, report.import_warnings),
        (3, 0),
        "{report:?}"
    );
    assert_eq!(
        node_of(&c_store, &incident).governance,
        kaban_core::Governance::Accepted
    );
    let ev =
        kaban_core::GovernanceStore::events(c_store.as_ref(), &incident.parse().unwrap()).unwrap();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].signer_kind, kaban_core::SignerKind::HumanPr);
    assert_eq!(ev[0].commit_sha.as_deref(), Some(head.as_str()));
}

/// RE2-6 (Medium): a committed higher-rev `proposed -> accepted` file of an existing custom
/// governed record imports through the governance table of the loaded set (no conflict row,
/// no `NotGoverned`), as the same move does for a Decision.
#[test]
fn a_higher_rev_acceptance_of_an_existing_custom_record_imports_without_a_conflict() {
    // Arrange: clone b holds the proposed Incident; clone a accepts it and commits
    let repo = Repo::new("custom-transition");
    let (a, _) = custom_clone(&repo, "a");
    let incident = record(&a, "incidents:Incident", "The numbat queue stalled. Twice.");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("proposed incident");
    let (b, b_store) = custom_clone(&repo, "b");
    sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        node_of(&b_store, &incident).governance,
        kaban_core::Governance::Proposed
    );
    accept(&a, &incident, &["ops/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("accepted incident");

    // Act
    let report = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();

    // Assert
    assert!(report.import.conflicts.is_empty(), "{report:?}");
    assert_eq!(report.import.imported, 1, "{report:?}");
    let node = node_of(&b_store, &incident);
    assert_eq!(node.governance, kaban_core::Governance::Accepted);
    assert_eq!(node.rev, 2);
    assert_eq!(node.path_scope, vec!["ops/**".to_owned()]);
}

/// RE2-7 (Medium): the near-duplicate key of a serializable custom `dedup: title+scope` kind
/// survives transport: an export -> fresh-store import and a higher-rev import keep the key
/// `remember` computed, and a second `remember` of the same title and scope in the importing
/// clone reports the imported record as a candidate.
#[test]
fn the_dup_key_of_a_custom_title_scope_kind_survives_import_and_reimport() {
    // Arrange
    let repo = Repo::new("custom-dup-key");
    let (a, a_store) = custom_clone(&repo, "a");
    let runbook = a
        .remember(RememberRequest {
            kind: "ops:Runbook".into(),
            text: "Rotate the queue. Then restart it.".into(),
            summary: Some("Rotate the queue".into()),
            path_scope: vec!["ops/**".into()],
            ..Default::default()
        })
        .unwrap()
        .id;
    let key = node_of(&a_store, &runbook).dup_key;
    assert!(key.is_some(), "remember sets the key of a title+scope kind");
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("runbook");

    // Act 1: a fresh store imports it
    let (b, b_store) = custom_clone(&repo, "b");
    sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();

    // Assert 1
    assert_eq!(node_of(&b_store, &runbook).dup_key, key, "fresh import");

    // Act 2: a higher rev (accepted in a) re-imports over the existing row
    accept(&a, &runbook, &["ops/**"]);
    sync::sync(&a, &repo.root, &SyncOpts::default()).unwrap();
    repo.commit("accepted runbook");
    let report = sync::sync(&b, &repo.root, &SyncOpts::default()).unwrap();

    // Assert 2
    assert_eq!(report.import.imported, 1, "{report:?}");
    let imported = node_of(&b_store, &runbook);
    assert_eq!(imported.rev, 2);
    assert_eq!(imported.dup_key, key, "higher-rev import");
    let second = b
        .remember(RememberRequest {
            kind: "ops:Runbook".into(),
            text: "A different body. Same title.".into(),
            summary: Some("rotate  THE queue".into()),
            path_scope: vec!["ops/**".into()],
            ..Default::default()
        })
        .unwrap();
    let ids: Vec<&str> = second
        .candidate_duplicates
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(ids, vec![runbook.as_str()], "{second:?}");
}

/// RE2-4 (Major): a file that names an ontology version this process has not loaded is
/// withheld from the export of the same `sync` as well: a locally dirty row behind it is not
/// serialized over the refused file (bytes and mtime unchanged, `rewritten` 0); the row stays
/// dirty and the refusal count is unchanged. A dirty Decision beside it is still exported.
#[test]
fn a_version_refused_file_is_never_rewritten_by_a_dirty_local_row_in_the_same_sync() {
    // Arrange: the Incident serialized at incidents@1, then advanced on disk under @2 while
    // the local row is dirty (accepted after the first sync)
    let repo = Repo::new("version-dirty");
    let (api, store) = custom_clone(&repo, "a");
    let incident = record(
        &api,
        "incidents:Incident",
        "The numbat queue stalled. Twice.",
    );
    let decision = record(
        &api,
        "Decision",
        "Use SQLite for the project store. No daemon.",
    );
    sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    let incident_path = repo
        .kaban()
        .join("incidents/incidents")
        .join(format!("{incident}.md"));
    let written = std::fs::read_to_string(&incident_path).unwrap();
    let advanced = written
        .replacen("ontology: incidents@1\n", "ontology: incidents@2\n", 1)
        .replacen("rev: 1\n", "rev: 3\n", 1)
        .replacen("stalled. Twice.", "stalled. Three times.", 2);
    std::fs::write(&incident_path, &advanced).unwrap();
    repo.commit("advanced under incidents@2");
    let before = std::fs::metadata(&incident_path)
        .unwrap()
        .modified()
        .unwrap();
    accept(&api, &incident, &["ops/**"]);
    accept(&api, &decision, &["crates/**"]);
    assert_ne!(
        node_of(&store, &incident).serialized_rev,
        Some(node_of(&store, &incident).rev),
        "the local row is dirty"
    );
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Act
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();

    // Assert: refused and withheld; the Decision beside it was rewritten
    assert_eq!(report.import.version_unknown, 1, "{report:?}");
    assert_eq!(report.rewritten, 1, "the Decision only: {report:?}");
    assert_eq!(std::fs::read_to_string(&incident_path).unwrap(), advanced);
    assert_eq!(
        std::fs::metadata(&incident_path)
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    let node = node_of(&store, &incident);
    assert_eq!(node.rev, 2, "the local acceptance");
    assert_ne!(node.serialized_rev, Some(node.rev), "still dirty");
    assert_eq!(
        node_of(&store, &decision).serialized_rev,
        Some(node_of(&store, &decision).rev)
    );
    // a second run makes the same decision again
    let again = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();
    assert_eq!(
        (again.import.version_unknown, again.rewritten),
        (1, 0),
        "{again:?}"
    );
    assert_eq!(std::fs::read_to_string(&incident_path).unwrap(), advanced);
}

/// RE2-3 (Major), the end-to-end negative: an ontology whose `serialize.dir` leaves the tree
/// (`/tmp/<scratch>/outside`, `../outside`) is refused at load with the fixed class, and a
/// `sync` afterwards creates nothing outside `.kaban/` (asserted on the scratch root and on
/// the repository root).
#[test]
fn an_ontology_naming_a_directory_outside_the_tree_is_refused_and_sync_creates_nothing_there() {
    // Arrange
    let repo = Repo::new("dir-outside");
    let (api, _) = custom_clone(&repo, "a");
    // a short absolute path (a long quoted scratch path trips the high-entropy reject rule
    // before the parse, a refusal too but not the one under test)
    let absolute = Path::new("/tmp/kaban-rogue-outside");
    assert!(!absolute.exists(), "{absolute:?} is free");
    let outside = repo.scratch.0.join("outside");
    let escaping = |dir: &str| {
        format!(
            "ontology: rogue\nversion: 1\nkinds:\n  Thing:\n    description: \"a thing\"\n    governed: {{acceptance: agent, serialize: {{dir: {dir}, format: md-frontmatter}}, export: {{digest: false, marker: false}}, display: {{label: Thing, plural: Things}}, governs_paths: false, dedup: none}}\nrels: {{}}\n"
        )
    };
    let _ = record(
        &api,
        "Decision",
        "Use SQLite for the project store. No daemon.",
    );

    // Act
    let mut errors = Vec::new();
    for (i, dir) in [absolute.to_str().unwrap(), "../outside", "a/b"]
        .iter()
        .enumerate()
    {
        let path = repo.scratch.0.join(format!("rogue{i}.yaml"));
        std::fs::write(&path, escaping(dir)).unwrap();
        errors.push(
            api.registry()
                .load_file(&api, &path)
                .unwrap_err()
                .to_string(),
        );
    }
    let report = sync::sync(&api, &repo.root, &SyncOpts::default()).unwrap();

    // Assert
    for e in &errors {
        assert!(e.contains("invalid_identifier"), "{e}");
        assert!(!e.contains("outside"), "no value echoed: {e}");
    }
    assert!(report.written >= 1, "{report:?}");
    assert!(!outside.exists(), "nothing created outside the repository");
    assert!(!absolute.exists());
    assert!(!repo.root.join("outside").exists());
    assert!(!repo.kaban().join("a").exists());
    let names: Vec<String> = std::fs::read_dir(&repo.root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|n| [".git", ".kaban", "README.md"].contains(&n.as_str())),
        "{names:?}"
    );
}
