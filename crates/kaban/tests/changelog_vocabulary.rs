//! `ChangeLog` vocabulary (engine-spec EN-E10b `Verify:` "a grep test that `outbox` no longer
//! occurs in `crates/**` outside the alias"; OWN-53; engine-plan E2.0 item 3). Every `.rs` file
//! under `crates/` is read, production sources and tests alike (the E1.0 `sql_boundary.rs`
//! walker, without its test and store exclusions). Every line containing `outbox` (any case)
//! must be one of:
//!
//! 1. one of the alias's declaration lines, matched by its text and present exactly once in
//!    the tree wherever it lives (OWN-81 (5), the decomposition review: declarations, not
//!    files): the `OutboxStore` supertrait, its three `outbox_*` shims, the blanket impl, the
//!    alias's doc line and the crate re-export;
//! 2. an identifier on the explicit allowlist below, each with the file that owns it today and
//!    the follow-up task that decides its rename (the wire and CLI shapes are E2.11's, EN-E10b's
//!    list covers the trait, the three methods, the module files, the `stats` field and the
//!    `doctor` check only); the identifier is matched, never the path;
//! 3. a file under a prefix whose vocabulary another stream owns (`kaban-ingest/`, the
//!    docs-ingest stream; `kaban-core/src/model.rs`, E2.1's file in the parallel pair);
//! 4. a comment or doc line that cites the requirement text the word comes from (core-spec
//!    F11, EN-E10b or OWN-53);
//! 5. one of the listed golden-pinned description lines, matched by text wherever it lives.
//!
//! The deprecated names (`OutboxStore`, `outbox_read`, `outbox_ack`, `outbox_prune`) are never
//! allowed in code outside those declaration lines, whatever the prefix: a caller of the alias
//! would warn under `clippy -D warnings`. A second copy of a declaration line is reported as
//! well: the alias has one definition site. The offending text is echoed in the failure: it is
//! our own source.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Rule 1: the alias's declaration lines (`kaban-core/src/store.rs` and the `lib.rs` re-export
/// today), each expected exactly once in the tree, matched by text so the owning file may move.
const DECLARATIONS: [(&str, &str); 7] = [
    (
        "pub trait OutboxStore: ChangeLogStore {",
        "the deprecated supertrait",
    ),
    (
        "The pre-E2.0 name of [`ChangeLogStore`] with its `outbox_*` method names",
        "the supertrait's doc line",
    ),
    (
        "fn outbox_read(&self, after: u64, limit: usize) -> Result<Vec<Change>> {",
        "the `changes_read` shim",
    ),
    (
        "fn outbox_ack(&self, consumer: &str, upto: u64, fence: u64) -> Result<()> {",
        "the `changes_ack` shim",
    ),
    (
        "fn outbox_prune(&self, tail: u64) -> Result<u64> {",
        "the `changes_prune` shim",
    ),
    (
        "impl<S: ChangeLogStore + ?Sized> OutboxStore for S {}",
        "the blanket impl",
    ),
    ("pub use store::OutboxStore;", "the crate re-export"),
];

/// The deprecated names, lowercase (never allowed outside the [`DECLARATIONS`]).
const DEPRECATED_NAMES: [&str; 4] = ["outboxstore", "outbox_read", "outbox_ack", "outbox_prune"];

/// Rule 2: identifiers that keep the old word, lowercase, with owner and follow-up.
const ALLOWED_TOKENS: [(&str, &str); 7] = [
    (
        "outbox_seq",
        "RememberOutcome.outbox_seq (kaban-core/src/events.rs) and the `remember` outcome field (kaban-api types/forget.rs, api/mutate.rs; tools_list goldens): E2.11 tool shapes",
    ),
    (
        "outbox_first_seq",
        "BatchReport.outbox_first_seq (kaban-core/src/model.rs, E2.1's file): E2.11 or a coordinator text correction",
    ),
    (
        "outbox_last_seq",
        "BatchReport.outbox_last_seq (kaban-core/src/model.rs, E2.1's file): E2.11 or a coordinator text correction",
    ),
    (
        "outbox_acked",
        "SyncReport.outbox_acked (kaban-api/src/sync.rs; the `kaban sync` JSON asserted by sync_rules.rs): E2.11 CLI shapes",
    ),
    (
        "outbox_pruned",
        "SyncReport.outbox_pruned (kaban-api/src/sync.rs): E2.11 CLI shapes",
    ),
    (
        "outbox_lag",
        "the `stats` deprecated alias of `changes_lag` and the `doctor` alias row (removed in 3.2, EN-E10b); ingest's IndexStatus.outbox_lag (kaban-api types/ingest.rs, mcp_docs goldens): E2.11",
    ),
    (
        "outbox_rows",
        "ingest's report field (kaban-ingest model.rs; kaban-api types/ingest.rs; cli_ingest, mcp_docs, bench/chunks): E2.11 / docs-ingest",
    ),
];

/// Rule 3: path prefixes whose vocabulary another stream owns.
const ALLOWED_PREFIXES: [(&str, &str); 2] = [
    (
        "crates/kaban-ingest/",
        "docs-ingest stream: its own `outbox_rows`/`outbox_lag` vocabulary and comments follow in E2.11 / docs-ingest",
    ),
    (
        "crates/kaban-core/src/model.rs",
        "E2.1's file in the parallel pair: BatchReport fields and their doc comments",
    ),
];

/// Rule 4: a comment line is allowed when it cites the requirement the word comes from.
const CITATIONS: [&str; 3] = ["F11", "EN-E10b", "OWN-53"];

/// Rule 5: description lines pinned by goldens that must not move in E2.0, matched by text.
const ALLOWED_LINES: [(&str, &str); 4] = [
    (
        "/// Outbox rows written.",
        "the schema description of ingest's `outbox_rows` (IngestReport, kaban-api types/ingest.rs), pinned by tools_list.snap: E2.11",
    ),
    (
        "/// Outbox rows not yet acknowledged by every consumer.",
        "the schema description of ingest's IndexStatus.outbox_lag (kaban-api types/ingest.rs), pinned by tools_list.snap: E2.11",
    ),
    (
        "last run, outbox lag, pipeline version",
        "the index_status tool description (ingest's IndexStatus.outbox_lag; kaban/src/mcp/mod.rs), pinned by tools_list.snap: E2.11",
    ),
    (
        "schema version, outbox lag, conflicts",
        "the `kaban doctor --help` text (clap doc comment, kaban/src/main.rs, cli_skeleton golden): E2.11",
    ),
];

/// The workspace root: this crate's manifest directory is `<root>/crates/kaban`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every `.rs` file under `<root>/crates/`, sorted, relative to the root (`target` skipped).
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().unwrap() == "target" {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&root.join("crates"), &mut files);
    let mut relative: Vec<PathBuf> = files
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
        .collect();
    relative.sort();
    relative
}

/// RW1-6: the walker's coverage is asserted over an invented tree, never a pinned real path.
/// A `src/` and a `tests/` file of one crate are both reached, a non-Rust file and a `target/`
/// directory are not, and the count by directory kind is exact.
fn assert_walker_covers_src_and_tests_of_an_invented_tree() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!(
        "kaban-changelog-vocabulary-{}-{stamp}",
        std::process::id()
    ));
    let crate_dir = tmp.join("crates").join("invented");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::create_dir_all(crate_dir.join("tests")).unwrap();
    fs::create_dir_all(crate_dir.join("target")).unwrap();
    fs::write(crate_dir.join("src").join("lib.rs"), "// src\n").unwrap();
    fs::write(crate_dir.join("tests").join("walk.rs"), "// tests\n").unwrap();
    fs::write(crate_dir.join("README.md"), "not rust\n").unwrap();
    fs::write(
        crate_dir.join("target").join("skipped.rs"),
        "// build dir\n",
    )
    .unwrap();

    let sources = rust_sources(&tmp);
    fs::remove_dir_all(&tmp).unwrap();

    assert_eq!(
        sources,
        [
            PathBuf::from("crates/invented/src/lib.rs"),
            PathBuf::from("crates/invented/tests/walk.rs"),
        ],
        "the walker reaches a src/ and a tests/ file, skips target/ and non-Rust files"
    );
    let by_kind = sources.iter().fold(BTreeMap::new(), |mut acc, p| {
        let kind = p.components().nth(2).unwrap().as_os_str().to_owned();
        *acc.entry(kind).or_insert(0usize) += 1;
        acc
    });
    assert_eq!(by_kind.len(), 2, "{by_kind:?}");
    assert!(by_kind.values().all(|n| *n == 1), "{by_kind:?}");
}

/// The identifier-like tokens of `line` that contain `outbox`, lowercase.
fn outbox_tokens(line: &str) -> Vec<String> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| t.to_ascii_lowercase().contains("outbox"))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The declaration line `line` is, if any (rule 1): its text, for the once-only count.
fn declaration(line: &str) -> Option<&'static str> {
    DECLARATIONS
        .iter()
        .find(|(needle, _)| line.contains(needle))
        .map(|(needle, _)| *needle)
}

/// Why one line is allowed, or `None` when it is a violation. The path only matters for the
/// rule 3 prefixes: declarations and pinned text are recognised wherever they live.
fn verdict(relative: &str, line: &str) -> Option<&'static str> {
    if declaration(line).is_some() {
        return Some("alias declaration");
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") && CITATIONS.iter().any(|c| line.contains(c)) {
        return Some("comment citing the requirement");
    }
    let tokens = outbox_tokens(line);
    if tokens
        .iter()
        .any(|t| DEPRECATED_NAMES.contains(&t.as_str()))
    {
        return None;
    }
    if ALLOWED_PREFIXES
        .iter()
        .any(|(prefix, _)| relative.starts_with(prefix))
    {
        return Some("another stream's vocabulary (prefix)");
    }
    if !tokens.is_empty()
        && tokens
            .iter()
            .all(|t| ALLOWED_TOKENS.iter().any(|(name, _)| name == t))
    {
        return Some("allowlisted identifier");
    }
    if ALLOWED_LINES
        .iter()
        .any(|(needle, _)| line.contains(needle))
    {
        return Some("golden-pinned description");
    }
    None
}

/// The violations in one file: `path:line: text`.
fn violations(relative: &Path, source: &str) -> Vec<String> {
    let relative_text = relative.to_str().unwrap();
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.to_ascii_lowercase().contains("outbox"))
        .filter(|(_, line)| verdict(relative_text, line).is_none())
        .map(|(index, line)| format!("{relative_text}:{}: {}", index + 1, line.trim()))
        .collect()
}

/// Every problem over a set of `(relative path, source)` pairs: the violations of each file,
/// then each declaration line that is absent or present more than once (rule 1).
fn check<'a>(sources: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut sites: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for (relative, source) in sources {
        problems.extend(violations(Path::new(relative), source));
        for (index, line) in source.lines().enumerate() {
            if let Some(needle) = declaration(line) {
                sites
                    .entry(needle)
                    .or_default()
                    .push(format!("{relative}:{}", index + 1));
            }
        }
    }
    for (needle, what) in DECLARATIONS {
        match sites.get(needle).map(Vec::as_slice) {
            Some([_]) => {}
            Some(many) => problems.push(format!(
                "declaration present {} times ({what}): `{needle}` at {}",
                many.len(),
                many.join(", ")
            )),
            None => problems.push(format!("declaration missing ({what}): `{needle}`")),
        }
    }
    problems
}

/// The declaration block as it is written today, for the invented-tree tests.
const ALIAS_BLOCK: &str =
    "/// The pre-E2.0 name of [`ChangeLogStore`] with its `outbox_*` method names, kept for one
pub trait OutboxStore: ChangeLogStore {
    fn outbox_read(&self, after: u64, limit: usize) -> Result<Vec<Change>> {
        self.changes_read(after, limit)
    }
    fn outbox_ack(&self, consumer: &str, upto: u64, fence: u64) -> Result<()> {
        self.changes_ack(consumer, upto, fence)
    }
    fn outbox_prune(&self, tail: u64) -> Result<u64> {
        self.changes_prune(tail)
    }
}
impl<S: ChangeLogStore + ?Sized> OutboxStore for S {}
";

const REEXPORT: &str = "pub use store::OutboxStore;\n";

#[test]
fn the_rules_on_invented_lines() {
    assert_eq!(
        verdict("crates/kaban-api/src/x.rs", "let n = report.outbox_acked;"),
        Some("allowlisted identifier")
    );
    assert_eq!(
        verdict("crates/kaban-api/src/x.rs", "store.outbox_read(0, 1)"),
        None,
        "a caller of the deprecated method is a violation"
    );
    assert_eq!(
        verdict("crates/kaban-ingest/src/x.rs", "// the outbox rows"),
        Some("another stream's vocabulary (prefix)")
    );
    assert_eq!(
        verdict(
            "crates/kaban-ingest/tests/x.rs",
            "use kaban_core::OutboxStore;"
        ),
        None,
        "the deprecated trait is refused under every prefix"
    );
    assert_eq!(
        verdict(
            "crates/kaban-api/src/x.rs",
            "// one row per write (core-spec F11, the outbox)"
        ),
        Some("comment citing the requirement")
    );
    assert_eq!(
        verdict("crates/kaban-api/src/x.rs", "// the outbox row"),
        None,
        "an uncited comment must use the ChangeLog vocabulary"
    );
    assert_eq!(
        verdict(
            "crates/kaban-core/src/store/changelog.rs",
            "pub trait OutboxStore: ChangeLogStore {"
        ),
        Some("alias declaration"),
        "a declaration is recognised by its text, not by its path"
    );
    assert_eq!(
        verdict(
            "crates/kaban-core/src/store.rs",
            "    pub fn outbox_read_all(&self) -> Result<Vec<Change>> {"
        ),
        None,
        "the alias file no longer exempts a fresh declaration; only the listed lines are"
    );
    assert_eq!(
        verdict(
            "crates/kaban-api/src/types/ingest.rs",
            "    /// Outbox rows written."
        ),
        Some("golden-pinned description"),
        "pinned text is recognised wherever it lives"
    );
    assert_eq!(
        violations(
            Path::new("crates/kaban-api/src/x.rs"),
            "fn a() {}\nlet q = api.outbox_bookkeeping();\n"
        ),
        ["crates/kaban-api/src/x.rs:2: let q = api.outbox_bookkeeping();"]
    );
}

#[test]
fn a_fresh_outbox_in_production_code_is_reported_whatever_the_file() {
    // Arrange: the alias where it lives today plus one fresh word in a production file
    let fresh = "pub struct Outbox { pub rows: u64 }\n";
    let tree = [
        ("crates/kaban-core/src/store.rs", ALIAS_BLOCK),
        ("crates/kaban-core/src/lib.rs", REEXPORT),
        ("crates/kaban-store-sqlite/src/sync.rs", fresh),
    ];

    // Act
    let problems = check(tree);

    // Assert: the one fresh line, nothing else
    assert_eq!(
        problems,
        ["crates/kaban-store-sqlite/src/sync.rs:1: pub struct Outbox { pub rows: u64 }"]
    );
    // Negative pair: the same alias in the alias file itself gains nothing from its path
    let in_alias_file = [
        ("crates/kaban-core/src/store.rs", ALIAS_BLOCK),
        ("crates/kaban-core/src/lib.rs", REEXPORT),
        ("crates/kaban-core/src/store/changelog.rs", fresh),
    ];
    assert_eq!(
        check(in_alias_file).len(),
        1,
        "a fresh word beside the alias is still one"
    );
}

#[test]
fn the_alias_may_move_but_not_multiply() {
    // Moved: the declarations in another file, the re-export unchanged => clean
    let moved = [
        ("crates/kaban-core/src/store/changelog.rs", ALIAS_BLOCK),
        ("crates/kaban-core/src/lib.rs", REEXPORT),
    ];
    assert_eq!(check(moved), Vec::<String>::new());

    // Doubled: the trait declared twice => reported with both sites
    let doubled = [
        ("crates/kaban-core/src/store.rs", ALIAS_BLOCK),
        ("crates/kaban-core/src/store/changelog.rs", ALIAS_BLOCK),
        ("crates/kaban-core/src/lib.rs", REEXPORT),
    ];
    let problems = check(doubled);
    assert_eq!(problems.len(), 6, "{problems:#?}");
    assert!(
        problems[0].starts_with("declaration present 2 times"),
        "{}",
        problems[0]
    );

    // Gone: a tree without the re-export names the missing declaration
    let missing = [("crates/kaban-core/src/store.rs", ALIAS_BLOCK)];
    assert_eq!(
        check(missing),
        ["declaration missing (the crate re-export): `pub use store::OutboxStore;`"]
    );
}

#[test]
fn outbox_occurs_nowhere_under_crates_outside_the_alias_and_the_allowlist() {
    // Arrange: read every Rust source under crates/ (this file excluded: it names the words)
    let root = workspace_root();
    let sources = rust_sources(&root);
    assert!(!sources.is_empty(), "the walker must reach the real tree");
    assert_walker_covers_src_and_tests_of_an_invented_tree();
    let texts: Vec<(String, String)> = sources
        .iter()
        .filter(|relative| !relative.ends_with("kaban/tests/changelog_vocabulary.rs"))
        .map(|relative| {
            (
                relative.to_str().unwrap().to_owned(),
                fs::read_to_string(root.join(relative)).unwrap(),
            )
        })
        .collect();

    // Act
    let found = check(texts.iter().map(|(p, s)| (p.as_str(), s.as_str())));

    // Assert
    assert!(
        found.is_empty(),
        "`outbox` outside the EN-E10b alias declarations and the allowlist (ChangeLog vocabulary, OWN-53):\n{}",
        found.join("\n")
    );
}
