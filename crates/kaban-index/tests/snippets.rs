//! CG8.2 snippets (stage1-spec G1-F17, G1-F7, G1-S1, RS-3): the committed lines through the
//! pinned `git show <sha>:<path>`, never the working tree; the 8-line and 1,024-byte bounds; a
//! NUL blob, a blob over the 1 MiB cap and a removed file yield no row and count
//! `snippet_skipped`; the canary is masked under a redact rule and dropped (counted
//! `policy_dropped`) under the fixture's credential rule, and never reaches the `.cg.db`.
//!
//! Each test builds a small git repository under a scratch directory and a synthetic index
//! whose definition ranges point at the committed lines; the collection runs in-process with
//! `InheritedFds::Tolerate` (the test binary inherits the slot lock's descriptor).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{
    Scratch, bytes, canary_policy, definition, document, gated, go_sym, index, meta, rows,
    sym_info, test_policy,
};
use kaban_codegraph::{CodeGraphReader, NodeId, SqliteSnapshot};
use kaban_core::process::InheritedFds;
use kaban_index::derive::TRUNCATION_MARKER;
use kaban_index::snippets::{self, Collected, MAX_SNIPPET_BYTES};
use kaban_index::writer::{self, Rows};
use kaban_index::{WarningClass, admit_index};
use kaban_policy::Policy;

// ---------------------------------------------------------------------------------------------
// fixtures

/// `git <args>` in `cwd` with a fixed identity and date (the `tests/cli.rs` helper).
fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_COMMITTER_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn head(repo: &Path) -> String {
    git(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// A repository at `dir` with `files` in one commit; the canonical path and the commit.
fn init_repo(dir: &Path, files: &[(&str, &[u8])]) -> (PathBuf, String) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    for (path, content) in files {
        fs::write(dir.join(path), content).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    let repo = fs::canonicalize(dir).unwrap();
    let sha = head(&repo);
    (repo, sha)
}

/// One synthetic document: its path and its symbols as `(descriptors, def range)`.
type Doc<'a> = (&'a str, &'a [(&'a str, [i32; 4])]);

/// The rows of one synthetic index: one document per [`Doc`], every symbol a go-drivers style
/// global with a definition occurrence at its range.
fn synthetic_rows(docs: &[Doc<'_>], policy: &Policy) -> Rows {
    let documents = docs
        .iter()
        .map(|(path, syms)| {
            document(
                path,
                syms.iter()
                    .map(|(s, _)| sym_info(&go_sym("v1", s)))
                    .collect(),
                syms.iter()
                    .map(|(s, r)| definition(&go_sym("v1", s), r))
                    .collect(),
            )
        })
        .collect();
    let b = bytes(&index(documents));
    let g = gated(&b, policy);
    let a = admit_index(&g);
    rows(&a, None, policy)
}

fn id_of(rows: &Rows, descriptors: &str) -> NodeId {
    let scip = go_sym("v1", descriptors);
    rows.symbols()
        .iter()
        .find(|s| s.scip.as_deref() == Some(scip.as_str()))
        .unwrap_or_else(|| panic!("{descriptors} is a row"))
        .id
}

fn text_of<'a>(c: &'a Collected, rows: &Rows, descriptors: &str) -> Option<&'a str> {
    let id = id_of(rows, descriptors);
    c.rows
        .iter()
        .find(|r| r.symbol_id == id)
        .map(|r| r.text.as_str())
}

/// The collected text of `descriptors` asserted against `expected` without printing either:
/// the failure names the property and the byte counts (`obs-no-sensitive-data`: a snippet is
/// content, and in the canary tests it is the secret; RW2-8).
fn expect_text(
    c: &Collected,
    rows: &Rows,
    descriptors: &str,
    expected: Option<&str>,
    property: &str,
) {
    let got = text_of(c, rows, descriptors);
    if got != expected {
        let describe = |t: Option<&str>| {
            t.map_or_else(
                || "no row".to_owned(),
                |t| format!("a row of {} bytes", t.len()),
            )
        };
        panic!(
            "{property}: expected {}, got {}",
            describe(expected),
            describe(got)
        );
    }
}

fn collect(repo: &Path, sha: &str, rows: &Rows, policy: &Policy) -> Collected {
    snippets::collect_with(repo, sha, rows, policy, InheritedFds::Tolerate)
}

/// The fixture canary under a redact-action rule (the free-field branch that keeps the mask).
fn redact_policy() -> Policy {
    let yaml = b"
version: 1
defaults: { action: reject }
extend: { use_builtin: false }
rules:
  - { id: canary-snippet, class: generic, action: redact, regex: 'CANARY_SNIPPET' }
";
    let file = Policy::parse(yaml).expect("redact policy parses");
    Policy::from_file(file, "test".into()).expect("redact policy compiles")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------------------------
// [red] the source and the bounds (G1-F17, G1-F7)

#[test]
fn snippet_bounds_and_source() {
    let s = Scratch::new();
    let mut lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    lines[25] = "x".repeat(1500);
    let committed = format!("{}\n", lines.join("\n"));
    let (repo, sha) = init_repo(
        &s.path().join("repo"),
        &[
            ("x.go", committed.as_bytes()),
            ("nul.go", b"package x\n\0\n"),
        ],
    );
    // the working tree moves on after the commit: the snapshot must not see it
    fs::write(repo.join("x.go"), "rewritten\n").unwrap();
    let policy = test_policy();
    let rows = synthetic_rows(
        &[
            (
                "x.go",
                &[
                    ("A().", [2, 0, 21, 1]),
                    ("B().", [25, 0, 25, 5]),
                    ("C().", [28, 0, 29, 1]),
                    ("E().", [40, 0, 40, 3]),
                ],
            ),
            ("nul.go", &[("D().", [0, 0, 0, 3])]),
        ],
        &policy,
    );
    let c = collect(&repo, &sha, &rows, &policy);
    expect_text(
        &c,
        &rows,
        "A().",
        Some(lines[2..=9].join("\n").as_str()),
        "the snippet is the committed lines (not the working tree), 8 of the 20-line body",
    );
    let b = text_of(&c, &rows, "B().").expect("the long line has a snippet");
    assert!(
        b.len() <= MAX_SNIPPET_BYTES && b.ends_with(TRUNCATION_MARKER),
        "a long line is cut at 1,024 bytes with the marker: {} bytes",
        b.len()
    );
    assert!(b.starts_with("xxxx"), "the cut keeps the head of the line");
    expect_text(
        &c,
        &rows,
        "C().",
        Some(lines[28..=29].join("\n").as_str()),
        "a body shorter than 8 lines is taken whole",
    );
    expect_text(
        &c,
        &rows,
        "D().",
        None,
        "a blob with a NUL byte yields no row",
    );
    expect_text(
        &c,
        &rows,
        "E().",
        None,
        "a definition past the end of the blob yields no row",
    );
    assert_eq!((c.skipped, c.policy_dropped), (2, 0));
    assert!(
        c.rows.windows(2).all(|w| w[0].symbol_id < w[1].symbol_id),
        "rows sorted by symbol_id"
    );
}

// ---------------------------------------------------------------------------------------------
// [red] the canary (G1-S1 free-field rules; §8.1)

#[test]
fn snippet_canary_redacted_or_dropped() {
    let s = Scratch::new();
    let src = b"package p\n\nfunc Open() string { return \"CANARY_SNIPPET\" }\n";
    let (repo, sha) = init_repo(&s.path().join("repo"), &[("p.go", src)]);
    let docs: &[Doc<'_>] = &[("p.go", &[("Open().", [2, 5, 2, 9])])];

    // reject (the fixture's credential-class rule): no row, `policy_dropped` 1
    let reject = canary_policy();
    let mut rows_r = synthetic_rows(docs, &reject);
    let c = collect(&repo, &sha, &rows_r, &reject);
    assert!(
        c.rows.is_empty(),
        "a reject hit drops the snippet row: {} rows kept",
        c.rows.len()
    );
    assert_eq!((c.policy_dropped, c.skipped), (1, 0));
    rows_r.set_snippets(c);
    assert!(
        rows_r.warnings().contains(&WarningClass::PolicyDropped),
        "the drop surfaces as the existing policy_dropped warning"
    );
    assert_eq!(rows_r.counts().snippet_skipped, 0);
    let path = writer::publish(s.path(), &rows_r, &meta(&sha), &reject).expect("publish");
    let db = fs::read(&path).unwrap();
    assert!(
        !contains(&db, b"CANARY_SNIPPET"),
        "the canary is on no byte of the snapshot"
    );
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    let m: BTreeMap<String, String> = snap.meta().unwrap();
    assert_eq!(
        m["counts:policy_dropped"], "1",
        "the drop is persisted under the existing counter"
    );
    drop(snap);

    // redact: the row stays with the masked text, the canary itself is absent
    let redact = redact_policy();
    let rows_m = synthetic_rows(docs, &redact);
    let c = collect(&repo, &sha, &rows_m, &redact);
    let text = text_of(&c, &rows_m, "Open().").expect("a redacted snippet keeps its row");
    assert!(
        !text.contains("CANARY_SNIPPET"),
        "the canary is masked, not kept: {} bytes stored",
        text.len()
    );
    assert!(
        text.contains("[REDACTED:canary-snippet]"),
        "the mask is the policy's: {} bytes stored",
        text.len()
    );
    assert!(
        text.starts_with("func Open() string"),
        "the clean head of the line is kept"
    );
    assert_eq!((c.policy_dropped, c.skipped), (0, 0));
}

// ---------------------------------------------------------------------------------------------
// [red] the ceiling on the admitted representation (G1-F17; RW2-7): a mask longer than its
// match grows the masked text, the stored row stays within 1,024 bytes and ends on whole masks

/// The mask of [`expanding_policy`]'s rule: 14 bytes for a 5-byte match.
const EXPANDING_MASK: &str = "[REDACTED:tok]";

/// A redact-action rule whose mask is longer than its match (`TOKEN`), so repeated matches
/// push the masked text past the ceiling (`defaults.mask` is the default template).
fn expanding_policy() -> Policy {
    let yaml = b"
version: 1
defaults: { action: reject }
extend: { use_builtin: false }
rules:
  - { id: tok, class: generic, action: redact, regex: 'TOKEN' }
";
    let file = Policy::parse(yaml).expect("expanding policy parses");
    Policy::from_file(file, "test".into()).expect("expanding policy compiles")
}

#[test]
fn snippet_redacted_within_ceiling() {
    let s = Scratch::new();
    // 900 bytes before the masks (under the ceiling, so the pre-scan bound leaves it whole),
    // 2,250 after; two matches stay under the ceiling masked; a clean line over the ceiling
    let many = "TOKEN ".repeat(150);
    let few = "TOKEN ".repeat(2);
    let long = "x".repeat(1500);
    let committed = format!("{many}\n{few}\n{long}\n");
    let (repo, sha) = init_repo(&s.path().join("repo"), &[("t.go", committed.as_bytes())]);
    let policy = expanding_policy();
    let mut rows = synthetic_rows(
        &[(
            "t.go",
            &[
                ("Many().", [0, 0, 0, 5]),
                ("Few().", [1, 0, 1, 5]),
                ("Long().", [2, 0, 2, 5]),
            ],
        )],
        &policy,
    );
    let c = collect(&repo, &sha, &rows, &policy);
    assert_eq!((c.policy_dropped, c.skipped), (0, 0));
    assert_eq!(
        c.rows.len(),
        3,
        "every row keeps a snippet under a redact-only rule"
    );
    rows.set_snippets(c);
    let path = writer::publish(s.path(), &rows, &meta(&sha), &policy).expect("publish");
    let db = fs::read(&path).unwrap();
    assert!(
        !contains(&db, b"TOKEN"),
        "the matched text is on no byte of the snapshot"
    );
    let snap = SqliteSnapshot::open_ro(&path).unwrap();
    let stored = |descriptors: &str| -> String {
        snap.node_by_id(id_of(&rows, descriptors))
            .unwrap()
            .expect("the symbol is a node of the snapshot")
            .snippet
            .expect("the node has a stored snippet")
    };

    // the property: the stored masked text is within the ceiling, cut outside every mask
    let many = stored("Many().");
    assert!(
        many.len() <= MAX_SNIPPET_BYTES,
        "the stored masked snippet is within the 1,024-byte ceiling: {} bytes",
        many.len()
    );
    assert!(
        many.ends_with(TRUNCATION_MARKER),
        "a masked snippet cut at the ceiling carries the marker"
    );
    let body = &many[..many.len() - TRUNCATION_MARKER.len()];
    let whole_masks = body.matches(EXPANDING_MASK).count();
    assert!(
        whole_masks >= 1 && body.split(' ').all(|p| p.is_empty() || p == EXPANDING_MASK),
        "the cut lands outside every mask: {whole_masks} whole masks, no mask fragment"
    );

    // negative: a masked snippet under the ceiling is stored whole, no marker
    let few = stored("Few().");
    assert!(
        few == format!("{EXPANDING_MASK} {EXPANDING_MASK} "),
        "a masked snippet within the ceiling is stored whole: {} bytes",
        few.len()
    );

    // postcheck positive: a clean long snippet still truncates to 1,024 with the marker
    let long = stored("Long().");
    assert!(
        long.len() <= MAX_SNIPPET_BYTES
            && long.ends_with(TRUNCATION_MARKER)
            && long.starts_with("xxxx"),
        "a clean long snippet is cut at 1,024 bytes with the marker: {} bytes",
        long.len()
    );
}

// ---------------------------------------------------------------------------------------------
// [red] the bound on git itself (default 4: a blob over the 1 MiB cap is killed by the runner)

#[test]
fn snippet_show_timeout_skips_and_continues() {
    let s = Scratch::new();
    let big: Vec<u8> = format!("{}\n", "b".repeat(99)).repeat(16_000).into_bytes();
    assert!(
        big.len() > (1 << 20),
        "the blob exceeds the 1 MiB stdout cap"
    );
    let (repo, sha) = init_repo(
        &s.path().join("repo"),
        &[
            ("big.go", big.as_slice()),
            ("small.go", b"package s\nfunc Small() {}\n"),
        ],
    );
    let policy = test_policy();
    let rows = synthetic_rows(
        &[
            ("big.go", &[("Big().", [0, 0, 0, 3])]),
            ("small.go", &[("Small().", [1, 5, 1, 10])]),
        ],
        &policy,
    );
    let c = collect(&repo, &sha, &rows, &policy);
    assert_eq!(
        c.skipped, 1,
        "the bounded `git show` skips its file's symbol and the run continues"
    );
    expect_text(
        &c,
        &rows,
        "Small().",
        Some("func Small() {}"),
        "the other file's rows are present",
    );
    expect_text(
        &c,
        &rows,
        "Big().",
        None,
        "the capped file's symbol has no row",
    );
    assert_eq!(c.policy_dropped, 0);
}

// ---------------------------------------------------------------------------------------------
// feature: a removed file has no snippet in the next snapshot (RS-3)

#[test]
fn snippet_absent_after_file_removed() {
    let s = Scratch::new();
    let (repo, c1) = init_repo(
        &s.path().join("repo"),
        &[
            ("a.go", b"package a\nfunc KeptA() {}\n"),
            ("b.go", b"package b\nfunc GoneB() {}\n"),
        ],
    );
    git(&repo, &["rm", "-q", "b.go"]);
    git(&repo, &["commit", "-q", "-m", "c2"]);
    let c2 = head(&repo);
    assert_ne!(c1, c2);
    let policy = test_policy();
    let rows = synthetic_rows(
        &[
            ("a.go", &[("KeptA().", [1, 5, 1, 10])]),
            ("b.go", &[("GoneB().", [1, 5, 1, 10])]),
        ],
        &policy,
    );
    // positive: at C1 both files are in the commit
    let at_c1 = collect(&repo, &c1, &rows, &policy);
    let kept = Some("func KeptA() {}");
    expect_text(
        &at_c1,
        &rows,
        "KeptA().",
        kept,
        "at C1 the kept file has its row",
    );
    expect_text(
        &at_c1,
        &rows,
        "GoneB().",
        Some("func GoneB() {}"),
        "at C1 the file removed later has its row",
    );
    assert_eq!((at_c1.skipped, at_c1.policy_dropped), (0, 0));
    // negative: at C2 the removed file's symbol has no snippet and counts `snippet_skipped`
    let at_c2 = collect(&repo, &c2, &rows, &policy);
    expect_text(
        &at_c2,
        &rows,
        "KeptA().",
        kept,
        "at C2 the kept file has its row",
    );
    expect_text(
        &at_c2,
        &rows,
        "GoneB().",
        None,
        "at C2 the removed file has no row",
    );
    assert_eq!((at_c2.skipped, at_c2.policy_dropped), (1, 0));
    assert_eq!(at_c2.rows.len(), 1);
}
