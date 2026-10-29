//! Public-behaviour tests of `kaban export` (core-spec F9a, S1, S4, N5; core-plan C5.2 as
//! amended by `reviews/m5-prep.md` M5-3, M5-12, M5-16, M5-17) over temporary git
//! repositories: the managed marker block is rendered from the accepted records, gated on the
//! way out and spliced into a user file without touching a byte outside the block.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::export::{self, ExportFormat, ExportOpts, ExportReport, WriteOutcome};
use kaban_api::{AcceptRequest, ApiConfig, Caller, KabanApi, RememberRequest};
use kaban_core::{Actor, ImportedRecord, MemoryStore, NodeId, SyncStore, WorkspaceId, WriteCtx};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

const FIXED_DATE: &str = "2026-10-26T08:16:02Z";
const WS: &str = "nightjar-service";

/// A throwaway git repository under a scratch directory; every commit is dated `FIXED_DATE`.
struct Repo {
    scratch: Scratch,
    root: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new("kaban-export", name);
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

    /// A database bound to `WS` with this repository as its root, under `policy`.
    fn db(&self, name: &str, policy: Policy, in_git: bool) -> (KabanApi, Arc<SqliteStore>) {
        let dir = self.scratch.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys")).unwrap());
        let api = KabanApi::with_config(
            store.clone(),
            policy,
            store.signer(),
            None,
            ApiConfig {
                workspace: WorkspaceId::new(WS),
                repo_root: in_git.then(|| self.root.clone()),
                repo: Some("repo".into()),
                record_signer_identity: false,
                engine: std::sync::Arc::default(),
            },
        );
        (api, store)
    }
}

/// Whether `node.text` of `id` is stored as a `BLOB` (an encoded body, spec 05 SS-17).
fn encoded(db: &std::path::Path, id: &str) -> bool {
    let kind: String = rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT typeof(text) FROM node WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    kind == "blob"
}

/// Rewrite the stored record `id` at its next revision with the same content (a sync import
/// of the record as it stands): on a store past the body codec migration the rewrite stores
/// the body encoded.
fn rewrite_in_place(store: &SqliteStore, id: &str) {
    let node = store
        .get(&NodeId::parse(id).unwrap(), true)
        .unwrap()
        .unwrap();
    store
        .import_record(
            &ImportedRecord {
                id: NodeId::parse(&node.id).unwrap(),
                kind: node.kind.clone(),
                text: node.text.clone(),
                summary: node.summary.clone(),
                tags: node.tags.clone(),
                confidence: node.confidence,
                importance: node.importance,
                lifecycle: node.lifecycle,
                governance: node.governance,
                path_scope: node.path_scope.clone(),
                props: node.props.clone(),
                rev: node.rev + 1,
                created_at: node.created_at.clone(),
                updated_at: node.updated_at.clone(),
                valid_from: node.valid_from.clone(),
                valid_to: node.valid_to.clone(),
                accepted_commit: node.accepted_commit.clone(),
                agent_id: node.agent_id.clone(),
                session_id: node.session_id.clone(),
                repo: node.repo.clone(),
                commit_sha: node.commit_sha.clone(),
                dup_key: node.dup_key.clone(),
                event: None,
            },
            &WriteCtx::new("v1", WorkspaceId::new(WS), Actor::HumanCli),
        )
        .unwrap();
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

/// An accepted governed record (`Decision` or `Convention`) with a rationale and a scope.
fn accepted(api: &KabanApi, kind: &str, text: &str, scope: &[&str]) -> String {
    let id = api
        .remember(RememberRequest {
            kind: kind.into(),
            text: text.into(),
            summary: Some(text.split('.').next().unwrap().to_owned()),
            props: Some(json!({"rationale": format!("because {}", text.to_lowercase())})),
            ..Default::default()
        })
        .unwrap()
        .id;
    api.accept(
        AcceptRequest {
            id: id.clone(),
            reason: Some("reviewed in PR 42".into()),
            path_scope: scope.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        },
        &human(),
    )
    .unwrap();
    id
}

fn write_to(path: &std::path::Path, format: ExportFormat) -> ExportOpts {
    ExportOpts {
        format,
        write: Some(path.to_path_buf()),
        ontologies: Vec::new(),
    }
}

fn outcome(report: &ExportReport) -> WriteOutcome {
    match report {
        ExportReport::Written { outcome, .. } => *outcome,
        other @ ExportReport::Rendered { .. } => panic!("expected a written report, got {other:?}"),
    }
}

/// Slice 1 (F9a, N5, M5-17): `export --write` into a fresh `AGENTS.md` creates the file with
/// the block only; the block holds the accepted Decision and Convention with scope and
/// rationale, and a second export at the same HEAD leaves every byte identical and reports
/// `Unchanged` without writing (the mtime does not move).
#[test]
fn export_twice_at_the_same_head_is_byte_identical_and_unchanged() {
    let repo = Repo::new("twice");
    let (api, store) = repo.db("a", Policy::empty(), true);
    // bodies long enough for the body codec to encode them once the store compresses (the
    // SS-17 check below); a shorter body would stay plain and prove nothing
    let decision = accepted(
        &api,
        "Decision",
        "Use SQLite for the project store. It needs no daemon, ships inside the binary and \
         keeps the whole store in one file beside the repository it describes.",
        &["crates/kaban-store-sqlite/**"],
    );
    let convention = accepted(
        &api,
        "Convention",
        "Errors are thiserror enums. No string errors, no panics on a user input, and every \
         library crate exposes its own error type at the boundary.",
        &["crates/**"],
    );
    let path = repo.root.join("AGENTS.md");

    let first = export::export(&api, &write_to(&path, ExportFormat::AgentsMd)).unwrap();
    assert_eq!(outcome(&first), WriteOutcome::Created, "{first:?}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(export::BLOCK_START), "{text}");
    assert!(
        text.ends_with(&format!("{}\n", export::BLOCK_END)),
        "{text:?}"
    );
    assert!(text.contains(&decision), "{text}");
    assert!(text.contains(&convention), "{text}");
    assert!(text.contains("Use SQLite for the project store"), "{text}");
    assert!(text.contains("`crates/kaban-store-sqlite/**`"), "{text}");
    assert!(
        text.contains("because use sqlite for the project store"),
        "{text}"
    );
    let decisions = text.find("## Decisions").unwrap();
    let conventions = text.find("## Conventions").unwrap();
    assert!(decisions < text.find(&decision).unwrap(), "{text}");
    assert!(text.find(&decision).unwrap() < conventions, "{text}");
    assert!(conventions < text.find(&convention).unwrap(), "{text}");
    let before = std::fs::read(&path).unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let second = export::export(&api, &write_to(&path, ExportFormat::AgentsMd)).unwrap();
    assert_eq!(outcome(&second), WriteOutcome::Unchanged, "{second:?}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte-identical");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        mtime,
        "an unchanged block is not rewritten"
    );
    match &second {
        ExportReport::Written {
            records, path: p, ..
        } => {
            assert_eq!(*records, 2, "{second:?}");
            assert_eq!(p, &path.display().to_string());
        }
        other @ ExportReport::Rendered { .. } => panic!("{other:?}"),
    }

    // spec 05 SS-17 (T7 review t7-5): the same records rewritten render the same bytes. Every
    // body is stored as a tagged BLOB (OWN-M); both records are rewritten in place at a new
    // revision and the export renders from the decoded bodies, so the file is byte-identical
    // and left untouched.
    let db = repo.scratch.0.join("a").join("kaban.db");
    for id in [&decision, &convention] {
        assert!(encoded(&db, id), "every body is stored encoded");
        rewrite_in_place(&store, id);
        assert!(encoded(&db, id), "the rewrite stores the body encoded");
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    let third = export::export(&api, &write_to(&path, ExportFormat::AgentsMd)).unwrap();
    assert_eq!(outcome(&third), WriteOutcome::Unchanged, "{third:?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the export of the encoded records is the pre-compression export, byte for byte"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        mtime,
        "an unchanged block is not rewritten"
    );
}

/// Slice 2 (F9a "never touching text outside the block"; M5-17 line endings): a CRLF file
/// with an old block and no trailing newline keeps every byte outside the markers, the new
/// block takes the CRLF of the start-marker line, and the file still ends without a newline;
/// a file without markers gets the block appended (LF), separated from a last line that
/// lacks its newline, and the markers are then found on the next run (`Unchanged`).
#[test]
fn text_outside_the_block_is_byte_preserved_with_crlf_and_without_a_trailing_newline() {
    let repo = Repo::new("outside");
    let (api, _store) = repo.db("a", Policy::empty(), true);
    accepted(
        &api,
        "Decision",
        "Keep the gate on every path. No exceptions.",
        &["src/**"],
    );
    let prefix = b"# My project\r\n\r\nIntro text with trailing spaces   \r\n".to_vec();
    let suffix = b"\r\nTrailing text without newline".to_vec();
    let mut original = prefix.clone();
    original.extend_from_slice(b"<!-- kaban:start -->\r\nold stuff\r\n<!-- kaban:end -->");
    original.extend_from_slice(&suffix);
    let path = repo.root.join("CLAUDE.md");
    std::fs::write(&path, &original).unwrap();

    let report = export::export(&api, &write_to(&path, ExportFormat::ClaudeMd)).unwrap();
    assert_eq!(outcome(&report), WriteOutcome::Updated, "{report:?}");
    let after = std::fs::read(&path).unwrap();
    assert!(
        after.starts_with(&prefix),
        "{:?}",
        String::from_utf8_lossy(&after)
    );
    assert!(
        after.ends_with(&suffix),
        "{:?}",
        String::from_utf8_lossy(&after)
    );
    let inner = &after[prefix.len()..after.len() - suffix.len()];
    let inner = std::str::from_utf8(inner).unwrap();
    assert!(inner.starts_with("<!-- kaban:start -->\r\n"), "{inner:?}");
    assert!(inner.ends_with("\r\n<!-- kaban:end -->"), "{inner:?}");
    assert!(!inner.contains("old stuff"), "{inner:?}");
    assert!(inner.contains("Keep the gate on every path"), "{inner:?}");
    assert!(
        !inner.replace("\r\n", "").contains('\n'),
        "every block line ends with CRLF: {inner:?}"
    );
    // idempotent on the CRLF file too
    let again = export::export(&api, &write_to(&path, ExportFormat::ClaudeMd)).unwrap();
    assert_eq!(outcome(&again), WriteOutcome::Unchanged, "{again:?}");
    assert_eq!(std::fs::read(&path).unwrap(), after);

    // no markers yet: appended after the last line, which gets its missing newline
    let plain = repo.root.join("NOTES.md");
    std::fs::write(&plain, "notes without a final newline").unwrap();
    let report = export::export(&api, &write_to(&plain, ExportFormat::Markdown)).unwrap();
    assert_eq!(outcome(&report), WriteOutcome::Updated, "{report:?}");
    let text = std::fs::read_to_string(&plain).unwrap();
    assert!(
        text.starts_with("notes without a final newline\n<!-- kaban:start -->\n"),
        "{text:?}"
    );
    assert!(text.ends_with("<!-- kaban:end -->\n"), "{text:?}");
    assert!(!text.contains('\r'), "{text:?}");
    let again = export::export(&api, &write_to(&plain, ExportFormat::Markdown)).unwrap();
    assert_eq!(outcome(&again), WriteOutcome::Unchanged, "{again:?}");
    assert_eq!(std::fs::read_to_string(&plain).unwrap(), text);
}

/// Slice 3 (core-spec §7 "refuses if the marker block is malformed"): two start markers, a
/// start without its end, and an end before the start are each refused with
/// `MalformedMarkers { starts, ends }`, the file keeps every byte, no temp file is left
/// behind, and the seam maps it to a request error (exit 4).
#[test]
fn a_malformed_marker_pair_is_refused_and_nothing_is_written() {
    use kaban_api::ApiError;
    use kaban_api::export::ExportError;
    let repo = Repo::new("malformed");
    let (api, _store) = repo.db("a", Policy::empty(), true);
    accepted(&api, "Decision", "One decision. Enough.", &["src/**"]);
    let cases: [(&str, usize, usize); 3] = [
        (
            "a\n<!-- kaban:start -->\nx\n<!-- kaban:start -->\ny\n<!-- kaban:end -->\n",
            2,
            1,
        ),
        ("a\n<!-- kaban:start -->\nx\n", 1, 0),
        ("<!-- kaban:end -->\nx\n<!-- kaban:start -->\n", 1, 1),
    ];
    for (i, (content, starts, ends)) in cases.iter().enumerate() {
        let path = repo.root.join(format!("bad-{i}.md"));
        std::fs::write(&path, content).unwrap();
        let err = export::export(&api, &write_to(&path, ExportFormat::AgentsMd)).unwrap_err();
        match &err {
            ExportError::MalformedMarkers { starts: s, ends: e } => {
                assert_eq!((*s, *e), (*starts, *ends), "case {i}: {err}");
            }
            other => panic!("case {i}: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            *content,
            "case {i}: file untouched"
        );
        assert!(
            matches!(ApiError::from(err), ApiError::Invalid(_)),
            "case {i}: a request error for the seam"
        );
    }
    let names: Vec<String> = std::fs::read_dir(&repo.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("tmp"))
        .collect();
    assert!(names.is_empty(), "no temp file left: {names:?}");
}

/// A policy over the builtins: `codename` rejects the phrase, `old-password` masks the word.
fn policy(yaml: &str) -> Policy {
    Policy::from_file(Policy::parse(yaml.as_bytes()).unwrap(), "t".into()).unwrap()
}

/// Slice 4 (S1 "export output", S4, M5-3): a record stored before the policy grew is
/// rendered, then the whole block is gated once; a reject hit withholds the export (the seam's
/// `Rejected`, exit 2) with the user file untouched and the block absent from the printed
/// report too; a redact hit is withheld as well, with the fixed note naming `kaban rescan`,
/// and masked bytes never reach the file.
#[test]
fn a_denylist_hit_in_the_rendered_block_withholds_the_export_and_leaves_the_file_untouched() {
    use kaban_api::ApiError;
    use kaban_api::export::{ExportError, WithheldCause};
    let repo = Repo::new("withheld");
    let (lenient, store) = repo.db("a", Policy::empty(), true);
    accepted(
        &lenient,
        "Decision",
        "Project Nightjar ships in March. The codename stays internal.",
        &["src/**"],
    );
    accepted(
        &lenient,
        "Convention",
        "The old password hunter2 is retired. Rotate on every release.",
        &["ops/**"],
    );
    let path = repo.root.join("AGENTS.md");
    let original = "# Agents\n<!-- kaban:start -->\nold block\n<!-- kaban:end -->\n";
    std::fs::write(&path, original).unwrap();

    // the policy grew: the phrase is now a reject-class hit
    let strict = KabanApi::with_config(
        store.clone(),
        policy("phrases:\n  - { term: 'Project Nightjar', id: codename }\n"),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: Some(repo.root.clone()),
            repo: Some("repo".into()),
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    );
    let err = export::export(&strict, &write_to(&path, ExportFormat::AgentsMd)).unwrap_err();
    match &err {
        ExportError::Withheld { cause, rules } => {
            assert_eq!(*cause, WithheldCause::Reject, "{err}");
            assert_eq!(rules, &["codename".to_owned()], "{err}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "nothing written"
    );
    let text = err.to_string();
    assert!(
        !text.contains("Nightjar") && !text.contains("hunter2"),
        "the message carries no value: {text}"
    );
    assert!(
        matches!(ApiError::from(err), ApiError::Rejected { .. }),
        "the seam exits 2"
    );
    // without --write the block is withheld the same way (never printed)
    let printed = export::export(
        &strict,
        &ExportOpts {
            format: ExportFormat::Markdown,
            write: None,
            ontologies: Vec::new(),
        },
    );
    assert!(
        matches!(
            printed,
            Err(ExportError::Withheld {
                cause: WithheldCause::Reject,
                ..
            })
        ),
        "{printed:?}"
    );

    // a redact-class policy: withheld too, with the fixed note; the mask never reaches the file
    let masking = KabanApi::with_config(
        store.clone(),
        policy("words:\n  - { term: hunter2, action: redact, id: old-password }\n"),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: Some(repo.root.clone()),
            repo: Some("repo".into()),
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    );
    let err = export::export(&masking, &write_to(&path, ExportFormat::AgentsMd)).unwrap_err();
    assert!(
        matches!(
            err,
            ExportError::Withheld {
                cause: WithheldCause::Redact,
                ..
            }
        ),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("kaban rescan"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    let mapped = ApiError::from(err);
    assert!(matches!(mapped, ApiError::Rejected { .. }), "{mapped:?}");
    let seam_text = mapped.to_string();
    assert!(
        !seam_text.contains("hunter2") && !seam_text.contains("REDACTED"),
        "{seam_text}"
    );

    // the lenient policy still exports (the records are clean under it), replacing the block
    let report = export::export(&lenient, &write_to(&path, ExportFormat::AgentsMd)).unwrap();
    assert_eq!(outcome(&report), WriteOutcome::Updated, "{report:?}");
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.starts_with("# Agents\n<!-- kaban:start -->\n"),
        "{after}"
    );
    assert!(
        after.contains("hunter2") && !after.contains("old block"),
        "{after}"
    );
}

/// Slice 5 (M5-17 "a missing file is created with the block only"): the target is created
/// with exactly the block, 0644, no temp file left; a target whose directory does not exist
/// is an I/O request error (exit 4) and nothing is created, because a user file's parents are
/// never made up.
#[test]
fn a_missing_file_is_created_with_the_block_only_and_a_missing_directory_is_refused() {
    use kaban_api::ApiError;
    use kaban_api::export::ExportError;
    use std::os::unix::fs::PermissionsExt as _;
    let repo = Repo::new("created");
    let (api, _store) = repo.db("a", Policy::empty(), false);
    accepted(
        &api,
        "Convention",
        "Tabs are spaces. Four of them.",
        &["**/*.rs"],
    );
    let dir = repo.root.join("docs");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("CONVENTIONS.md");

    let report = export::export(&api, &write_to(&path, ExportFormat::Markdown)).unwrap();
    assert_eq!(outcome(&report), WriteOutcome::Created, "{report:?}");
    let text = std::fs::read_to_string(&path).unwrap();
    let rendered = match export::export(
        &api,
        &ExportOpts {
            format: ExportFormat::Markdown,
            write: None,
            ontologies: Vec::new(),
        },
    )
    .unwrap()
    {
        ExportReport::Rendered { block, records, .. } => {
            assert_eq!(records, 1);
            block
        }
        other @ ExportReport::Written { .. } => panic!("{other:?}"),
    };
    assert_eq!(text, rendered, "the file is exactly the block");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["CONVENTIONS.md"], "no temp file left");

    let missing = repo.root.join("no-such-dir").join("AGENTS.md");
    let err = export::export(&api, &write_to(&missing, ExportFormat::AgentsMd)).unwrap_err();
    assert!(matches!(err, ExportError::Io { .. }), "{err:?}");
    let text = err.to_string();
    assert!(
        !text.contains("no-such-dir"),
        "no path in the message: {text}"
    );
    assert!(matches!(ApiError::from(err), ApiError::Invalid(_)));
    assert!(!missing.exists() && !missing.parent().unwrap().exists());
}

/// Slice 6 (M5-17): `agents-md` and `claude-md` render the same bytes; `markdown` differs
/// from them by exactly the instruction line; the entries carry scope and rationale and the
/// records come in `(created_at, id)` order inside their block, Decisions before Conventions.
#[test]
fn the_three_formats_differ_only_by_the_instruction_line() {
    let repo = Repo::new("formats");
    let (api, _store) = repo.db("a", Policy::empty(), false);
    let convention = accepted(
        &api,
        "Convention",
        "Name tests by behaviour. Not by method.",
        &["**/tests/**"],
    );
    let later = accepted(&api, "Decision", "Second decision. Later.", &["b/**"]);
    let earlier = accepted(&api, "Decision", "First decision. Earlier.", &["a/**"]);
    // the briefing orders by importance then created_at DESC; the block must not follow it
    let render = |format| match export::export(
        &api,
        &ExportOpts {
            format,
            write: None,
            ontologies: Vec::new(),
        },
    )
    .unwrap()
    {
        ExportReport::Rendered {
            block,
            records,
            format: f,
        } => {
            assert_eq!(f, format);
            assert_eq!(records, 3);
            block
        }
        other @ ExportReport::Written { .. } => panic!("{other:?}"),
    };
    let agents = render(ExportFormat::AgentsMd);
    let claude = render(ExportFormat::ClaudeMd);
    let markdown = render(ExportFormat::Markdown);
    assert_eq!(agents, claude);
    let instruction = format!("{}\n", export::INSTRUCTION_LINE);
    assert_eq!(agents.matches(&instruction).count(), 1, "{agents}");
    assert_eq!(agents.replacen(&instruction, "", 1), markdown);
    assert!(
        !markdown.contains("Read `.kaban/adr/INDEX.md`"),
        "{markdown}"
    );
    // order and entry shape
    let a = markdown.find(&later).unwrap();
    let b = markdown.find(&earlier).unwrap();
    let c = markdown.find(&convention).unwrap();
    assert!(a < b && b < c, "created order then Conventions: {markdown}");
    assert!(
        markdown.contains(&format!(
            "- **Second decision** ([{later}](.kaban/adr/decisions/{later}.md))\n  - scope: `b/**`\n  - rationale: because second decision. later.\n"
        )),
        "{markdown}"
    );
    assert!(
        markdown.contains(&format!("(.kaban/adr/conventions/{convention}.md)")),
        "{markdown}"
    );
    // deterministic and free of timestamps
    assert_eq!(markdown, render(ExportFormat::Markdown));
    assert!(!markdown.contains("2026-"), "{markdown}");
}

/// Slice 7 (M5-12): the first inner line names the HEAD sha inside a git repository and is
/// absent outside one; the rendered bytes otherwise agree, and a new commit changes exactly
/// that line.
#[test]
fn the_sha_line_is_present_inside_git_and_absent_outside() {
    let repo = Repo::new("sha");
    let head = repo.git(&["rev-parse", "HEAD"]);
    let (inside, store) = repo.db("a", Policy::empty(), true);
    accepted(
        &inside,
        "Decision",
        "Pin the toolchain. Stable minus two.",
        &["rust-toolchain.toml"],
    );
    let outside = KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: None,
            repo: None,
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    );
    let block = |api: &KabanApi| match export::export(
        api,
        &ExportOpts {
            format: ExportFormat::AgentsMd,
            write: None,
            ontologies: Vec::new(),
        },
    )
    .unwrap()
    {
        ExportReport::Rendered { block, .. } => block,
        other @ ExportReport::Written { .. } => panic!("{other:?}"),
    };
    let with = block(&inside);
    let without = block(&outside);
    let sha_line = format!("<!-- generated by kaban export at {head} -->\n");
    assert!(
        with.starts_with(&format!("{}\n{sha_line}", export::BLOCK_START)),
        "{with}"
    );
    assert!(!without.contains("generated by kaban export"), "{without}");
    assert_eq!(with.replacen(&sha_line, "", 1), without);
    // a new commit moves the line and nothing else
    std::fs::write(repo.root.join("CHANGES.md"), "- a change\n").unwrap();
    let next = repo.commit("a change");
    assert_ne!(next, head);
    let moved = block(&inside);
    assert_eq!(
        moved,
        with.replacen(&head, &next, 1),
        "only the sha changed:\n{with}\n---\n{moved}"
    );
    // the public renderer agrees with the orchestration
    let blocks: Vec<_> = inside
        .registry()
        .export_blocks()
        .into_iter()
        .filter(|b| b.marker)
        .collect();
    let kinds: Vec<&str> = blocks.iter().map(|b| b.kind.as_str()).collect();
    let records = kaban_core::GovernanceStore::briefing(store.as_ref(), WS, &kinds, 10).unwrap();
    assert_eq!(
        export::render_export(&records, &blocks, ExportFormat::AgentsMd, Some(&next)),
        moved
    );
    assert_eq!(
        export::render_export(&records, &blocks, ExportFormat::AgentsMd, None),
        without
    );
}

/// R5-7 (F-W3-1, S1 "export output"): the rendered block is judged in overlapping windows, so
/// an export of 200 accepted records (well over the 20,000-character per-field cap) is
/// rendered and written, twice identically; a reject-class phrase in the last entry, past the
/// cap, is still caught and the export withheld with the user file untouched.
#[test]
fn a_block_over_the_per_field_cap_is_exported_and_a_hit_past_the_cap_still_withholds_it() {
    use kaban_api::export::{ExportError, WithheldCause};
    let repo = Repo::new("long-block");
    let (lenient, store) = repo.db("a", Policy::empty(), true);
    for i in 0..200 {
        accepted(
            &lenient,
            if i % 2 == 0 { "Decision" } else { "Convention" },
            &format!(
                "Record {i:03} keeps its own writer. One connection per process avoids the busy \
                 loop seen in run {i}, which is the reasoning this entry carries."
            ),
            &[&format!("crates/m{i}/**")],
        );
    }
    let path = repo.root.join("AGENTS.md");
    let first = export::export(&lenient, &write_to(&path, ExportFormat::AgentsMd)).unwrap();
    assert_eq!(outcome(&first), WriteOutcome::Created, "{first:?}");
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.chars().count() > 20_000,
        "the block renders over the per-field cap ({} chars)",
        written.chars().count()
    );
    assert_eq!(
        written.matches("](.kaban/").count(),
        200,
        "every record once"
    );
    let second = export::export(&lenient, &write_to(&path, ExportFormat::AgentsMd)).unwrap();
    assert_eq!(outcome(&second), WriteOutcome::Unchanged, "{second:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), written);

    // the policy grew: the last entry's title is now a reject-class phrase, deep in the block
    accepted(
        &lenient,
        "Convention",
        "Project Nightjar ships in March. The codename stays internal.",
        &["src/**"],
    );
    let strict = KabanApi::with_config(
        store.clone(),
        policy("phrases:\n  - { term: 'Project Nightjar', id: codename }\n"),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WS),
            repo_root: Some(repo.root.clone()),
            repo: Some("repo".into()),
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    );
    let err = export::export(&strict, &write_to(&path, ExportFormat::AgentsMd)).unwrap_err();
    assert!(
        matches!(
            &err,
            ExportError::Withheld { cause: WithheldCause::Reject, rules } if rules == &["codename"]
        ),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        written,
        "the user file is untouched"
    );
    // the lenient service still exports the grown set (201 entries, no hit under its policy)
    let third = export::export(&lenient, &write_to(&path, ExportFormat::AgentsMd)).unwrap();
    assert_eq!(outcome(&third), WriteOutcome::Updated, "{third:?}");
    assert_eq!(
        std::fs::read_to_string(&path)
            .unwrap()
            .matches("](.kaban/")
            .count(),
        201
    );
}

/// R5-10: the target's size is checked before a byte of it is read. A target above
/// `MAX_TARGET_BYTES` is refused with a fixed text (exit 4) and left exactly as it was (bytes,
/// size, mtime), with no temp file beside it; a target just under the cap, and an ordinary
/// one, are spliced idempotently as before.
#[test]
fn an_oversized_export_target_is_refused_untouched_and_a_normal_one_is_replaced_idempotently() {
    use kaban_api::ApiError;
    use kaban_api::export::{ExportError, MAX_TARGET_BYTES};
    let repo = Repo::new("oversized");
    let (api, _store) = repo.db("a", Policy::empty(), true);
    accepted(
        &api,
        "Decision",
        "Cap the reads. Metadata first.",
        &["src/**"],
    );

    // a sparse target one byte over the cap: nothing on disk, everything in the size
    let huge = repo.root.join("AGENTS.md");
    let f = std::fs::File::create(&huge).unwrap();
    f.set_len(MAX_TARGET_BYTES + 1).unwrap();
    drop(f);
    let before = std::fs::metadata(&huge).unwrap();
    let err = export::export(&api, &write_to(&huge, ExportFormat::AgentsMd)).unwrap_err();
    assert!(matches!(err, ExportError::TargetTooLarge), "{err:?}");
    assert_eq!(
        err.to_string(),
        "the target file is larger than the export target cap; nothing was written"
    );
    assert!(matches!(ApiError::from(err), ApiError::Invalid(_)));
    let after = std::fs::metadata(&huge).unwrap();
    assert_eq!(after.len(), MAX_TARGET_BYTES + 1, "untouched");
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    let names: Vec<String> = std::fs::read_dir(&repo.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("kaban-tmp"))
        .collect();
    assert!(names.is_empty(), "no temp file left: {names:?}");

    // exactly at the cap: still read and spliced (the block is appended, bytes preserved)
    let at_cap = repo.root.join("CLAUDE.md");
    std::fs::write(&at_cap, "# Claude\n").unwrap();
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&at_cap)
        .unwrap();
    f.set_len(MAX_TARGET_BYTES).unwrap();
    drop(f);
    let report = export::export(&api, &write_to(&at_cap, ExportFormat::ClaudeMd)).unwrap();
    assert_eq!(outcome(&report), WriteOutcome::Updated, "{report:?}");
    let bytes = std::fs::read(&at_cap).unwrap();
    assert!(
        bytes.starts_with(b"# Claude\n"),
        "bytes outside the block preserved"
    );
    assert!(
        bytes.ends_with(format!("{}\n", export::BLOCK_END).as_bytes()),
        "the block was appended"
    );

    // an ordinary target: replaced, then unchanged
    let normal = repo.root.join("docs.md");
    std::fs::write(
        &normal,
        "# Docs\n<!-- kaban:start -->\nold\n<!-- kaban:end -->\ntail\n",
    )
    .unwrap();
    let first = export::export(&api, &write_to(&normal, ExportFormat::Markdown)).unwrap();
    assert_eq!(outcome(&first), WriteOutcome::Updated, "{first:?}");
    let text = std::fs::read_to_string(&normal).unwrap();
    assert!(text.starts_with("# Docs\n") && text.ends_with("<!-- kaban:end -->\ntail\n"));
    assert!(text.contains("Cap the reads"), "{text}");
    let second = export::export(&api, &write_to(&normal, ExportFormat::Markdown)).unwrap();
    assert_eq!(outcome(&second), WriteOutcome::Unchanged, "{second:?}");
    assert_eq!(std::fs::read_to_string(&normal).unwrap(), text);
}
