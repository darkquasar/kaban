//! `kaban sync [--digest-only] [--ontology ...]` and `kaban export [--ontology ...]` black-box
//! through the binary over a temporary git repository (engine-spec EN-F4, OWN-46; engine-plan
//! E1.6): the per-ontology layout `.kaban/<ontology>/<dir>/<id>.md` with the `ontology:
//! <name>@<version>` key, a loaded `Incident` under `.kaban/incidents/incidents/`, the
//! `Incidents` block selected and omitted by `--ontology`, the shipped-only equality of
//! `export` and `export --ontology adr`, `--digest-only`, and the fixed-class exit 2 of an
//! unloaded name.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

    fn kaban(&self) -> PathBuf {
        self.root.join(".kaban")
    }

    /// Every file under `.kaban/`, relative, sorted.
    fn files(&self) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else {
                    out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.kaban(), &self.kaban(), &mut out);
        out.sort();
        out
    }
}

/// `kaban --db <db> --workspace fixture --json <args>` with the scratch home, run inside the
/// repository (the startup ontology load reads `<repo>/.kaban/ontology/*.yaml`).
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
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn remember(s: &Scratch, db: &Path, repo: &Repo, kind: &str, text: &str) -> String {
    // a Decision's `props_schema` requires `rationale`
    let mut args = vec!["remember", kind, text];
    if kind == "Decision" {
        args.extend(["--props", r#"{"rationale": "embedded, zero ops"}"#]);
    }
    let out = kaban(s, db, &repo.root, &args);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    stdout_json(&out)["id"].as_str().unwrap().to_owned()
}

fn accept(s: &Scratch, db: &Path, repo: &Repo, id: &str) {
    let out = kaban(
        s,
        db,
        &repo.root,
        &[
            "accept",
            id,
            "--reason",
            "reviewed",
            "--path-scope",
            "ops/**",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

fn export(s: &Scratch, db: &Path, repo: &Repo, extra: &[&str]) -> Output {
    let mut args = vec!["export", "--format", "markdown"];
    args.extend_from_slice(extra);
    kaban(s, db, &repo.root, &args)
}

/// The engine layout through the binary: with the fixture `incidents` ontology loaded at
/// startup, an accepted `Incident` serializes under `.kaban/incidents/incidents/<id>.md` with
/// `ontology: incidents@1`, a Decision under `.kaban/adr/decisions/` with `ontology: adr@1`,
/// the `adr` digest is `.kaban/adr/INDEX.md` and lists no Incident, `incidents` has no digest,
/// `export` renders an `Incidents` section that `--ontology adr` omits, `--digest-only` writes
/// no record file, and an unloaded `--ontology` is exit 2 with the fixed class.
#[test]
fn sync_and_export_follow_the_ontology_layout_and_the_ontology_selection() {
    let s = Scratch::new("kaban-cli-ontology", "layout");
    let repo = Repo::new(&s);
    let db = s.0.join("a.db");
    std::fs::create_dir_all(repo.kaban().join("ontology")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kaban-core/fixtures/ontology/incidents.yaml"),
        repo.kaban().join("ontology/incidents.yaml"),
    )
    .unwrap();

    // a Decision (accepted) and an Incident (accepted by a person on the CLI)
    let decision = remember(
        &s,
        &db,
        &repo,
        "Decision",
        "Use SQLite for the project store. No daemon.",
    );
    accept(&s, &db, &repo, &decision);
    let incident = remember(
        &s,
        &db,
        &repo,
        "incidents:Incident",
        "The numbat queue stalled. Twice.",
    );
    accept(&s, &db, &repo, &incident);
    let outage = remember(
        &s,
        &db,
        &repo,
        "incidents:Outage",
        "The region went dark. For an hour.",
    );

    // sync: every ontology is serialized, the adr digest only
    let synced = kaban(&s, &db, &repo.root, &["sync"]);
    assert_eq!(synced.status.code(), Some(0), "{}", stderr(&synced));
    let report = stdout_json(&synced);
    assert_eq!(report["written"], json!(2), "{report}");
    assert_eq!(report["digest_updated"], json!(true), "{report}");
    assert!(
        report.get("layout").is_none(),
        "no pre-engine tree: {report}"
    );
    let files = repo.files();
    assert_eq!(
        files,
        vec![
            ".gitignore".to_owned(),
            "adr/INDEX.md".to_owned(),
            format!("adr/decisions/{decision}.md"),
            format!("incidents/incidents/{incident}.md"),
            "ontology/incidents.yaml".to_owned(),
        ],
        "{files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains(&outage)),
        "serialize: none"
    );
    let incident_text = std::fs::read_to_string(
        repo.kaban()
            .join(format!("incidents/incidents/{incident}.md")),
    )
    .unwrap();
    assert!(
        incident_text.contains("\nontology: incidents@1\n"),
        "{incident_text}"
    );
    assert!(
        incident_text.contains("\nkind: incidents:Incident\n"),
        "{incident_text}"
    );
    let decision_text =
        std::fs::read_to_string(repo.kaban().join(format!("adr/decisions/{decision}.md"))).unwrap();
    assert!(
        decision_text.contains("\nontology: adr@1\n"),
        "{decision_text}"
    );
    let digest = std::fs::read_to_string(repo.kaban().join("adr/INDEX.md")).unwrap();
    assert!(digest.contains(&decision), "{digest}");
    assert!(!digest.contains(&incident), "{digest}");

    // export: the Incidents section is rendered by default and omitted by --ontology adr
    let all = export(&s, &db, &repo, &[]);
    assert_eq!(all.status.code(), Some(0), "{}", stderr(&all));
    let all_block = stdout_json(&all)["block"].as_str().unwrap().to_owned();
    assert!(all_block.contains("\n## Decisions\n"), "{all_block}");
    assert!(all_block.contains("\n## Incidents\n"), "{all_block}");
    assert!(
        all_block.contains(&format!("(.kaban/adr/decisions/{decision}.md)")),
        "{all_block}"
    );
    assert!(
        all_block.contains(&format!("(.kaban/incidents/incidents/{incident}.md)")),
        "{all_block}"
    );
    let adr = export(&s, &db, &repo, &["--ontology", "adr"]);
    assert_eq!(adr.status.code(), Some(0), "{}", stderr(&adr));
    let adr_block = stdout_json(&adr)["block"].as_str().unwrap().to_owned();
    assert!(!adr_block.contains("Incidents"), "{adr_block}");
    assert!(adr_block.contains(&decision), "{adr_block}");
    let both = export(&s, &db, &repo, &["--ontology", "adr,incidents"]);
    assert_eq!(both.status.code(), Some(0), "{}", stderr(&both));
    assert_eq!(stdout_json(&both)["block"], stdout_json(&all)["block"]);

    // --digest-only: no import, no record file, the digests only
    let touched = repo.kaban().join(format!("adr/decisions/{decision}.md"));
    std::fs::remove_file(&touched).unwrap();
    let digest_only = kaban(
        &s,
        &db,
        &repo.root,
        &["sync", "--digest-only", "--ontology", "adr"],
    );
    assert_eq!(
        digest_only.status.code(),
        Some(0),
        "{}",
        stderr(&digest_only)
    );
    let report = stdout_json(&digest_only);
    assert_eq!(report["written"], json!(0), "{report}");
    assert_eq!(report["import"]["files"], json!(0), "{report}");
    assert!(
        !touched.exists(),
        "no record file is written by --digest-only"
    );
    let full = kaban(&s, &db, &repo.root, &["sync"]);
    assert_eq!(full.status.code(), Some(0), "{}", stderr(&full));
    assert!(touched.exists(), "the full sync re-serializes it");

    // an unloaded ontology name: exit 2 with the fixed class, nothing echoed
    for args in [
        vec!["sync", "--ontology", "runbooks"],
        vec!["export", "--ontology", "adr,runbooks"],
    ] {
        let out = kaban(&s, &db, &repo.root, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(out.stdout.is_empty(), "stdout stays empty on error");
        assert!(
            err.contains("REJECTED") && err.contains("ontology_unknown"),
            "{args:?}: {err}"
        );
        assert!(!err.contains("runbooks"), "the name is not echoed: {err}");
    }
}

/// A store holding only the shipped ontologies: `kaban export` and `kaban export --ontology
/// adr` render the same block (the EN-F4 golden promise restated by OWN-46), and
/// `--ontology all` too.
#[test]
fn export_and_export_ontology_adr_render_the_same_block_for_a_shipped_only_store() {
    let s = Scratch::new("kaban-cli-ontology", "shipped");
    let repo = Repo::new(&s);
    let db = s.0.join("a.db");
    let decision = remember(
        &s,
        &db,
        &repo,
        "Decision",
        "Pin the toolchain. Stable minus two.",
    );
    accept(&s, &db, &repo, &decision);
    let convention = remember(&s, &db, &repo, "Convention", "Name tests by behaviour.");
    accept(&s, &db, &repo, &convention);

    let plain = export(&s, &db, &repo, &[]);
    let adr = export(&s, &db, &repo, &["--ontology", "adr"]);
    let all = export(&s, &db, &repo, &["--ontology", "all"]);
    assert_eq!(plain.status.code(), Some(0), "{}", stderr(&plain));
    assert_eq!(adr.status.code(), Some(0), "{}", stderr(&adr));
    let block = stdout_json(&plain)["block"].as_str().unwrap().to_owned();
    assert_eq!(stdout_json(&adr)["block"], json!(block));
    assert_eq!(stdout_json(&all)["block"], json!(block));
    assert_eq!(stdout_json(&plain)["records"], json!(2));
    assert!(block.contains("\n## Decisions\n"), "{block}");
    assert!(block.contains("\n## Conventions\n"), "{block}");
    assert!(
        block.contains(&format!("(.kaban/adr/conventions/{convention}.md)")),
        "{block}"
    );
    assert!(!block.contains("Incidents"), "{block}");
}
