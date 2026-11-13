//! `code_context` through `KabanApi` (stage1-spec G1-F14, G1-N7, §6; stage1-plan CG9.1): the
//! golden over the go-drivers snapshot with its scripted two-commit history (`mkfixture.sh`
//! replayed here with the same fixed names and dates), the bounded `git log` (`null` on a
//! timeout or on malformed output, never an error) and the response gate over the
//! `last_change` summary. Self-contained beside `code_tools.rs`: the fixture publish helpers
//! are copied, not shared.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::Scratch;
use kaban_api::{
    ApiConfig, ApiError, CodeContextRequest, CodeContextResponse, CodeDefinitionRequest, GitRunner,
    KabanApi, LastChange, SymbolSelector,
};
use kaban_codegraph::IsolationTier;
use kaban_codegraph::testing::FixedSnapshot;
use kaban_index::writer::{self, Commit, Meta, Provenance, Rows};
use kaban_index::{admit_index, decode_and_gate};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::Value;

/// `<repo-id>` of the fixture snapshot.
const REPO_ID: &str = "0123456789abcdef0123456789abcdef";

/// The go-drivers fixture's indexed commit (`fixtures/codegraph/go-drivers/COMMITS`, C2).
const GO_COMMIT: &str = "730da842221a844e8c2c82891dda9094f6428858";

/// Ref prefix of the `drivers` package (versioned, as emitted) and its normalised key prefix.
const GO: &str = "scip:scip-go gomod example.com/drivers 730da842221a844e8c2c82891dda9094f6428858 `example.com/drivers/drivers`/";
const GO_KEY: &str = "scip-go gomod example.com/drivers . `example.com/drivers/drivers`/";

/// The fixture's two commit dates (`mkfixture.sh`).
const C1_DATE: &str = "2026-10-26T08:16:02Z";
const C2_DATE: &str = "2026-10-26T09:04:36Z";
const C2_SUMMARY: &str = "C2: rename mem.go, extend PostgresDriver.Open";

/// The snippet canary of the fixtures (`examples/policy-test-canaries.yaml`), assembled at run
/// time so that the source carries no literal the rule matches.
fn canary() -> String {
    format!("{}_{}", "CANARY", "SNIPPET")
}

/// `None` when `got` is `expected`, else the failure message: the property and the byte counts,
/// never either text (RW3-7, the CG8.2a `expect_text` pattern: in the canary tests the summary
/// is the secret, and a failing mask is exactly when it would be printed).
fn text_mismatch(property: &str, expected: &str, got: &str) -> Option<String> {
    (got != expected).then(|| {
        format!(
            "{property}: expected a text of {} bytes, got {} bytes",
            expected.len(),
            got.len()
        )
    })
}

/// Assert `got` is `expected` with a content-free failure ([`text_mismatch`]).
fn expect_text(property: &str, expected: &str, got: &str) {
    if let Some(message) = text_mismatch(property, expected, got) {
        panic!("{message}");
    }
}

/// The helper itself: the clean case is silent (positive); an invented mismatch names the
/// property and the counts and carries no byte of either text (negative).
#[test]
fn text_mismatch_names_the_property_and_counts_and_no_text() {
    let secret = format!("{}-{}", canary(), "subject-7f3a");
    assert!(
        text_mismatch("clean", &secret, &secret).is_none(),
        "equal texts are no mismatch"
    );
    let message = text_mismatch("the property", &secret, "other text")
        .expect("different texts are a mismatch");
    assert!(
        !message.contains(&secret) && !message.contains("other text"),
        "the mismatch message carries a text"
    );
    assert!(
        message.contains("the property") && message.contains(&secret.len().to_string()),
        "the mismatch message names the property and the counts"
    );
}

// ---------------------------------------------------------------------------------------------
// the scripted repository

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codegraph")
}

fn fixture_bytes(rel: &str) -> Vec<u8> {
    let path = fixtures_dir().join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Copy a tree (the fixture's C1 `repo/`, no `.git` inside) into `to`.
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

/// `git <args>` in `root` under `mkfixture.sh`'s fixed identity, dates and config.
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

fn rev_parse(root: &Path, what: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", what])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// `mkfixture.sh:53-68` replayed: C1 the committed tree, C2 the `mem.go` rename plus the
/// `_ = dsn` line in `PostgresDriver.Open`. Returns `(root, HEAD)`.
fn fixture_repo(dir: &Path) -> (PathBuf, String) {
    let root = dir.join("repo");
    copy_tree(&fixtures_dir().join("go-drivers/repo"), &root);
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
    git(&root, C2_DATE, &["commit", "-q", "-m", C2_SUMMARY]);
    let head = rev_parse(&root, "HEAD");
    (root, head)
}

/// A third commit touching `drivers/postgres.go` with `summary` as its subject; returns its id.
fn commit_touching_postgres(root: &Path, summary: &str) -> String {
    let postgres = root.join("drivers/postgres.go");
    let mut src = std::fs::read_to_string(&postgres).unwrap();
    src.push_str("\n// C3\n");
    std::fs::write(&postgres, src).unwrap();
    git(root, "2026-10-26T09:53:10Z", &["add", "-A"]);
    git(
        root,
        "2026-10-26T09:53:10Z",
        &["commit", "-q", "-m", summary],
    );
    rev_parse(root, "HEAD")
}

/// A `#!/bin/sh` git wrapper: `body` runs when the invocation contains ` log `, else the real
/// git runs with the same arguments.
fn git_wrapper(dir: &Path, name: &str, body: &str) -> PathBuf {
    let wrapper = dir.join(name);
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\ncase \" $* \" in *\" log \"*) {body} ;; esac\nexec git \"$@\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    wrapper
}

// ---------------------------------------------------------------------------------------------
// the snapshot (as `code_tools.rs` publishes it)

/// `examples/policy-test-canaries.yaml`: the §8.1 canaries as credential rules (reject).
fn canary_policy_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-test-canaries.yaml")
}

fn canary_policy() -> Policy {
    Policy::load(&canary_policy_path()).expect("the canary policy loads")
}

/// The canary policy as a redacting one: `defaults.action: redact` and the rules in the
/// `generic` class (a `credential` rule always rejects, core-spec S1), so every hit masks.
fn canary_redact_policy(dir: &Path) -> Policy {
    let text = std::fs::read_to_string(canary_policy_path()).unwrap();
    let redact = text
        .replacen("action: reject", "action: redact", 1)
        .replace("class: credential", "class: generic");
    assert_ne!(text, redact, "the canary policy's default action is reject");
    let path = dir.join("policy-redact.yaml");
    std::fs::write(&path, redact).unwrap();
    Policy::load(&path).expect("the redact variant loads")
}

/// The shipped builtin policy (the AWS key rule is live; no canary rule).
fn builtin_policy() -> Policy {
    Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap()
}

fn codegraph_dir(s: &Scratch) -> PathBuf {
    let cg = s.0.join("codegraph");
    let d = cg.join(REPO_ID);
    std::fs::create_dir_all(&d).unwrap();
    for p in [&cg, &d] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    d
}

/// Decode, gate, admit, derive and publish `<dir>/<commit>.cg.db` from the go-drivers index
/// bytes (`code_tools.rs` `publish`), with `policy_version` in `meta`.
fn publish_go(dir: &Path, policy_version: &str) -> PathBuf {
    let policy = canary_policy();
    let a = decode_and_gate(&fixture_bytes("go-drivers/index.scip"), &policy).expect("gate A");
    let b =
        decode_and_gate(&fixture_bytes("go-drivers/index-notest.scip"), &policy).expect("gate B");
    let admitted_a = admit_index(&a);
    let admitted_b = admit_index(&b);
    let rows = Rows::derive(&admitted_a, Some(&admitted_b), &policy);
    let meta = Meta {
        commit: Commit::parse(GO_COMMIT).expect("40-hex commit"),
        repo_id: REPO_ID.to_owned(),
        policy_version: policy_version.to_owned(),
        provenance: Provenance {
            isolation: IsolationTier::None,
            isolation_detail: "test".to_owned(),
            resources: Vec::new(),
            container_image_digest: None,
        },
    };
    writer::publish(dir, &rows, &meta, &policy).expect("publish")
}

/// A git runner with a test-sized deadline (`code_tools.rs` `patient_git`).
fn patient_git() -> GitRunner {
    GitRunner {
        program: PathBuf::from("git"),
        timeout: Duration::from_secs(10),
    }
}

/// A service bound to `root` as repository `fixture` under `policy`, running git through
/// `git`, over the go-drivers snapshot published under the live policy version (`policy:
/// current`) behind a `FixedSnapshot`.
fn service(s: &Scratch, root: &Path, policy: Policy, git: GitRunner) -> KabanApi {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = KabanApi::with_config(
        store.clone(),
        policy,
        store.signer(),
        None,
        ApiConfig {
            repo_root: Some(root.to_path_buf()),
            repo: Some("fixture".to_owned()),
            ..Default::default()
        },
    )
    .with_git(git);
    let dir = codegraph_dir(s);
    let path = publish_go(&dir, &api.policy_version());
    api.with_snapshot_provider(dir, Arc::new(FixedSnapshot::new(path)))
}

// ---------------------------------------------------------------------------------------------
// requests and projections

fn open_key() -> String {
    format!("{GO_KEY}PostgresDriver#Open().")
}

fn by_symbol(key: &str) -> SymbolSelector {
    SymbolSelector {
        symbol: Some(key.to_owned()),
        ..Default::default()
    }
}

fn context(target: SymbolSelector) -> CodeContextRequest {
    CodeContextRequest {
        target,
        current_path: None,
    }
}

fn refs(cards: &[kaban_api::CodeNode]) -> Vec<String> {
    cards.iter().map(|c| c.r#ref.clone()).collect()
}

/// The legs of `PostgresDriver#Open()` (`goldens/edges.txt`): one caller, five callees of
/// which two are the method's own locals (`local 2`, `local 3`, hidden since G4: a local is
/// never a caller or a callee) and the ranking keeps the remaining three by `(path, ref)`.
fn expected_callers() -> Vec<String> {
    vec![format!("{GO}TestPostgresOpen().")]
}

fn expected_callees() -> Vec<String> {
    vec![
        format!("{GO}Conn#"),
        format!("{GO}PostgresDriver#"),
        format!("{GO}pgConn#"),
    ]
}

fn assert_legs_intact(r: &CodeContextResponse) {
    assert_eq!(r.definition.r#ref, format!("{GO}PostgresDriver#Open()."));
    assert_eq!(refs(&r.callers), expected_callers(), "callers");
    assert_eq!(refs(&r.callees), expected_callees(), "callees");
    assert_eq!(r.tests.len(), 1, "tests: {:?}", r.tests);
}

// ---------------------------------------------------------------------------------------------
// the slices

/// [red] G1-F14 `code_context_golden_go_drivers`: over the go-drivers snapshot and its scripted
/// history, `PostgresDriver#Open()` answers with the `code_definition` card, the one caller
/// (`TestPostgresOpen`), its three non-local callees, `approximate: true`, the one
/// `reference` test link and C2 as `last_change`.
#[test]
fn code_context_golden_go_drivers() {
    let s = Scratch::new("kaban-api-code-context", "golden");
    let (root, head) = fixture_repo(&s.0);
    let api = service(&s, &root, builtin_policy(), patient_git());
    let key = open_key();

    let r = api.code_context(context(by_symbol(&key))).unwrap();
    assert_eq!(r.snapshot.commit, GO_COMMIT);
    assert_eq!(r.snapshot.policy, "current");
    let definition = api
        .code_definition(CodeDefinitionRequest {
            target: by_symbol(&key),
            current_path: None,
        })
        .unwrap()
        .definition;
    assert_eq!(r.definition, definition, "the code_definition card");
    assert_eq!(r.definition.path.as_deref(), Some("drivers/postgres.go"));
    assert_eq!(refs(&r.callers), expected_callers(), "callers");
    assert_eq!(refs(&r.callees), expected_callees(), "callees");
    assert!(
        r.callers
            .iter()
            .chain(&r.callees)
            .all(|c| c.snippet.is_none()),
        "legs carry no snippet"
    );
    assert!(r.approximate, "REFERENCES legs are approximate");
    assert_eq!(r.tests.len(), 1, "{:?}", r.tests);
    assert_eq!(r.tests[0].test.r#ref, format!("{GO}TestPostgresOpen()."));
    assert_eq!(r.tests[0].matched_by, "reference");
    assert!(!r.tests[0].heuristic);
    let change = r.last_change.clone().expect("C2 is the last change");
    assert_eq!(change.commit, head);
    assert_eq!(
        change.commit, GO_COMMIT,
        "the scripted history reproduces C2"
    );
    // `%aI` renders UTC as `Z` or `+00:00` depending on the git version: the same instant
    assert!(
        ["2026-10-26T09:04:36Z", "2026-10-26T09:04:36+00:00"]
            .contains(&change.author_date.as_str()),
        "author_date: {}",
        change.author_date
    );
    assert_eq!(change.summary, C2_SUMMARY);
    assert_eq!(
        r.last_change,
        Some(LastChange {
            commit: change.commit.clone(),
            author_date: change.author_date.clone(),
            summary: change.summary.clone(),
        })
    );
    assert_eq!(r.follow_ups, Vec::<kaban_api::FollowUp>::new());
}

/// [red] G1-F14 `code_context_git_timeout_null`: a git that sleeps on `log` under a 200 ms
/// runner deadline leaves `last_change: null`, the rest of the answer intact, inside 300 ms,
/// and the call is `Ok` (a timeout is never an error).
#[test]
fn code_context_git_timeout_null() {
    let s = Scratch::new("kaban-api-code-context", "timeout");
    let (root, _) = fixture_repo(&s.0);
    let wrapper = git_wrapper(&s.0, "git-slow-log.sh", "sleep 5");
    let api = service(
        &s,
        &root,
        builtin_policy(),
        GitRunner {
            program: wrapper,
            timeout: Duration::from_millis(200),
        },
    );
    let started = Instant::now();
    let r = api.code_context(context(by_symbol(&open_key())));
    let elapsed = started.elapsed();
    let r = match r {
        Ok(r) => r,
        Err(e) => panic!("a slow git must not fail the call: {e}"),
    };
    assert_eq!(r.last_change, None, "last_change is null on timeout");
    assert_legs_intact(&r);
    assert!(
        elapsed < Duration::from_millis(300),
        "the runner's deadline bounds the call: {elapsed:?}"
    );
}

/// `code_context_malformed_log_null`: a `log` that prints `abc` (no NUL fields, no 40-hex
/// commit) gives `last_change: null` and the rest intact (negative); the real git gives the C2
/// record (positive).
#[test]
fn code_context_malformed_log_null() {
    let s = Scratch::new("kaban-api-code-context", "malformed");
    let (root, head) = fixture_repo(&s.0);
    let wrapper = git_wrapper(&s.0, "git-abc-log.sh", "echo abc; exit 0");
    let api = service(
        &s,
        &root,
        builtin_policy(),
        GitRunner {
            program: wrapper,
            timeout: Duration::from_secs(10),
        },
    );
    let r = api.code_context(context(by_symbol(&open_key()))).unwrap();
    assert_eq!(r.last_change, None, "malformed log output is null");
    assert_legs_intact(&r);

    let real = api.with_git(patient_git());
    let r = real.code_context(context(by_symbol(&open_key()))).unwrap();
    let change = r.last_change.expect("the real git answers");
    assert_eq!(change.commit, head);
    assert_eq!(change.summary, C2_SUMMARY);
}

/// `code_context_summary_canary_gated`: the handler scans nothing, so a commit subject
/// carrying the snippet canary reaches the response as content; the adapter's
/// `gate_response("code_context", ..)` refuses the result under the reject policy with no
/// canary in the error text (negative), and under the redact variant masks the summary while
/// `last_change.commit` (a control `Hex` leaf) stays (positive).
#[test]
fn code_context_summary_canary_gated() {
    let canary = canary();
    let summary = format!("C3: {canary} in the subject");

    let s = Scratch::new("kaban-api-code-context", "canary-reject");
    let (root, _) = fixture_repo(&s.0);
    let c3 = commit_touching_postgres(&root, &summary);
    let api = service(&s, &root, canary_policy(), patient_git());
    let r = api.code_context(context(by_symbol(&open_key()))).unwrap();
    let change = r.last_change.clone().expect("C3 is the last change");
    assert_eq!(change.commit, c3);
    expect_text("the handler does not scan", &summary, &change.summary);
    let value = serde_json::to_value(&r).unwrap();
    let err = api
        .gate_response("code_context", &value)
        .expect_err("the reject rule refuses the whole result");
    assert!(
        matches!(err, ApiError::Rejected { .. }),
        "the reject rule refuses as Rejected"
    );
    assert!(
        !err.to_string().contains(&canary),
        "the error text carries the canary"
    );
    assert!(
        !format!("{err:?}").contains(&canary),
        "the error debug text carries the canary"
    );

    let s2 = Scratch::new("kaban-api-code-context", "canary-redact");
    let (root2, _) = fixture_repo(&s2.0);
    let c3b = commit_touching_postgres(&root2, &summary);
    let api2 = service(&s2, &root2, canary_redact_policy(&s2.0), patient_git());
    let r2 = api2.code_context(context(by_symbol(&open_key()))).unwrap();
    let value2 = serde_json::to_value(&r2).unwrap();
    let clean = api2
        .gate_response("code_context", &value2)
        .expect("the redact rule masks");
    let masked = clean["last_change"]["summary"].as_str().unwrap();
    assert!(
        !masked.contains(&canary),
        "the masked summary still carries the canary ({} bytes)",
        masked.len()
    );
    assert!(
        masked.contains("[REDACTED:canary-snippet]"),
        "the summary carries the mask ({} bytes)",
        masked.len()
    );
    assert_eq!(clean["last_change"]["commit"], Value::String(c3b));
    assert_eq!(clean["definition"]["ref"], value2["definition"]["ref"]);
    // the gated tree still parses as the declared shape (what the adapter checks, R3-9)
    serde_json::from_value::<CodeContextResponse>(clean).expect("still a CodeContextResponse");
}
