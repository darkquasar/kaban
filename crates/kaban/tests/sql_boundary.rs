//! Store-boundary hygiene (engine-spec EN-E10 placement, EN-E10a hygiene; engine-plan E1.0):
//! no SQL text and no `rusqlite::` path in any production source outside the store crate.
//!
//! The rule is textual on purpose: every `.rs` file under `crates/` is read, except
//! `crates/kaban-store-sqlite/**` (the one SQL owner), `crates/kaban-codegraph/**` (v4's reader
//! crate with its own SQL, CG-JOIN v1 §2 / SO-5; allowlisted before it exists), `**/tests/**` and
//! `**/migrations/**`. Inside every remaining file, a `mod` block preceded by `#[cfg(test)]` is
//! skipped (brace-depth scan from the attribute to the matching close), and so is any line whose
//! first non-whitespace characters are `//` (a `//`, `///` or `//!` comment; coordinator decision
//! at E1.0: `kaban-api/src/rescan.rs` draws `SELECT` in a crate doc-comment diagram). A trailing
//! comment after code is judged with its line, `/* */` blocks are not special-cased, and string
//! literals count: a `"SELECT "` in production code is a violation even if it is never executed.
//! Every other line must not contain `SELECT `, `INSERT `, `UPDATE `, `DELETE FROM` or
//! `rusqlite::`.
//!
//! `production_sources` (the walker) and `non_test_lines` (the `#[cfg(test)]` skipper) are plain
//! functions so that E2.12's `placement.rs` can copy them with the same semantics; no shared test
//! module is created at E1.0. The offending text is echoed in the failure: it is our own source.
//! A planted tree proves the rule reaches a crate that does not exist yet (spec 05 SS-22).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::fs;
use std::path::{Path, PathBuf};

/// The SQL and driver markers no production line outside the store crate may contain.
const FORBIDDEN: [&str; 5] = ["SELECT ", "INSERT ", "UPDATE ", "DELETE FROM", "rusqlite::"];

/// Directories under `crates/` whose sources are allowed to carry SQL (relative to the workspace
/// root, with a trailing slash so `kaban-store-sqlite-foo` would not match).
const ALLOWED_CRATES: [&str; 3] = [
    "crates/kaban-store-sqlite/",
    "crates/kaban-codegraph/",
    "crates/kaban-vtab/",
];

/// Path components that mark test code or SQL files rather than production sources.
const SKIPPED_COMPONENTS: [&str; 2] = ["tests", "migrations"];

/// The workspace root: this crate's manifest directory is `<root>/crates/kaban`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every `.rs` file under `<root>/crates/` that the boundary rule applies to, sorted, as paths
/// relative to the root: the allowlisted crates and any `tests/` or `migrations/` directory are
/// skipped.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                let name = path.file_name().unwrap().to_str().unwrap();
                if SKIPPED_COMPONENTS.contains(&name) || name == "target" {
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
        .filter(|p| {
            let text = p.to_str().unwrap();
            !ALLOWED_CRATES
                .iter()
                .any(|allowed| text.starts_with(allowed))
        })
        .collect();
    relative.sort();
    relative
}

/// Count the brace delta of one line, ignoring braces inside `"..."` string literals (with
/// backslash escapes) and after a `//` comment start. Raw strings and block comments are not
/// modelled: the sources this test reads do not need them yet.
fn brace_delta(line: &str) -> i64 {
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut previous = '\0';
    for ch in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                '/' if previous == '/' => break,
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        previous = ch;
    }
    depth
}

/// The lines of `source` that are not inside a `mod` block preceded by `#[cfg(test)]`, each with
/// its 1-based line number. The attribute may be followed by other attributes, by `mod name {` on
/// the same or a later line (the block is skipped to its matching close) or by `mod name;` (no
/// block; nothing is skipped). A `#[cfg(test)]` on anything other than a `mod` skips nothing.
fn non_test_lines(source: &str) -> Vec<(usize, &str)> {
    let mut state = State::Code;
    let mut kept = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim_start();
        match state {
            State::Code => {
                if let Some(rest) = trimmed.strip_prefix("#[cfg(test)]") {
                    state = State::Attributed;
                    let rest = rest.trim_start();
                    if rest.is_empty() {
                        continue;
                    }
                    // `#[cfg(test)] mod name {` or `#[cfg(test)] mod name;` on one line
                    let (next, keep) = after_attribute(rest, line);
                    state = next;
                    if keep {
                        kept.push((number, line));
                    }
                    continue;
                }
                kept.push((number, line));
            }
            State::Attributed => {
                if trimmed.starts_with("#[") {
                    continue; // further attributes, e.g. `#[allow(...)]`
                }
                let (next, keep) = after_attribute(trimmed, line);
                state = next;
                if keep {
                    kept.push((number, line));
                }
            }
            State::Skipping(depth) => {
                let depth = depth + brace_delta(line);
                state = if depth <= 0 {
                    State::Code
                } else {
                    State::Skipping(depth)
                };
            }
        }
    }
    kept
}

/// The state after the item a `#[cfg(test)]` decorates and whether its line is kept: a `mod`
/// with a body is skipped from this line to the matching close (a body that closes on the same
/// line skips just that line); a `mod name;` or any other item resumes normal scanning with the
/// line kept.
fn after_attribute(item: &str, full_line: &str) -> (State, bool) {
    let is_mod = item.starts_with("mod ")
        || item.starts_with("pub mod ")
        || item.starts_with("pub(crate) mod ");
    if !is_mod || !item.contains('{') {
        return (State::Code, true);
    }
    let depth = brace_delta(full_line);
    if depth > 0 {
        (State::Skipping(depth), false)
    } else {
        (State::Code, false)
    }
}

/// Where the `#[cfg(test)]` skipper is while scanning a file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Production code: lines are kept.
    Code,
    /// `#[cfg(test)]` seen; waiting for the item it decorates.
    Attributed,
    /// Inside the skipped module at this brace depth.
    Skipping(i64),
}

/// The violations in one file: `path:line: text` for every kept, non-comment line that contains
/// a forbidden marker.
fn violations(relative: &Path, source: &str) -> Vec<String> {
    non_test_lines(source)
        .into_iter()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| FORBIDDEN.iter().any(|marker| line.contains(marker)))
        .map(|(number, line)| format!("{}:{number}: {}", relative.display(), line.trim()))
        .collect()
}

#[test]
fn no_sql_or_rusqlite_outside_the_store_crate_in_production_sources() {
    // the skipper's contract, on invented sources, before the tree is read
    let same_line = "fn a() {}\n#[cfg(test)] mod t { fn x() { \"SELECT \" } }\nfn b() {}\n";
    assert_eq!(
        non_test_lines(same_line)
            .iter()
            .map(|l| l.0)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    let next_line = "fn a() {}\n#[cfg(test)]\n#[allow(dead_code)]\nmod t {\n  fn x() {\n    let s = \"}\";\n  }\n}\nfn b() {}\n";
    assert_eq!(
        non_test_lines(next_line)
            .iter()
            .map(|l| l.0)
            .collect::<Vec<_>>(),
        [1, 9]
    );
    let declaration = "fn a() {}\n#[cfg(test)]\nmod t;\nfn b() { \"SELECT \" }\n";
    assert_eq!(
        non_test_lines(declaration)
            .iter()
            .map(|l| l.0)
            .collect::<Vec<_>>(),
        [1, 3, 4]
    );
    let not_a_mod = "#[cfg(test)]\nfn only_in_tests() { \"INSERT \" }\n";
    assert_eq!(
        non_test_lines(not_a_mod)
            .iter()
            .map(|l| l.0)
            .collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(
        violations(
            Path::new("x.rs"),
            "// SELECT in a comment\nlet q = \"SELECT 1\"; // code wins\n"
        ),
        ["x.rs:2: let q = \"SELECT 1\"; // code wins"]
    );
    assert_eq!(
        violations(
            Path::new("x.rs"),
            "fn f() {}\n#[cfg(test)]\nmod t {\n use rusqlite::Connection;\n}\n"
        ),
        Vec::<String>::new()
    );

    // spec 05 SS-22: a planted crate outside the two SQL owners (a future
    // `kaban-index-tantivy`, say) is walked and its SQL or driver reference rejected; the
    // owners' own SQL is not, and a name that merely extends an owner's is no owner
    let planted = std::env::temp_dir().join(format!("kaban-sql-boundary-{}", std::process::id()));
    let _ = fs::remove_dir_all(&planted);
    for (file, text) in [
        (
            "crates/kaban-index-tantivy/src/lib.rs",
            "fn f() { let q = \"SELECT id FROM node\"; }\nuse rusqlite::Connection;\n",
        ),
        (
            "crates/kaban-store-sqlite-extra/src/lib.rs",
            "fn g() { \"DELETE FROM node\" }\n",
        ),
        (
            "crates/kaban-store-sqlite/src/lib.rs",
            "fn h() { \"SELECT 1\" }\n",
        ),
        (
            "crates/kaban-codegraph/src/lib.rs",
            "use rusqlite::Connection;\n",
        ),
    ] {
        let path = planted.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let mut rejected = Vec::new();
    for relative in production_sources(&planted) {
        let source = fs::read_to_string(planted.join(&relative)).unwrap();
        rejected.extend(violations(&relative, &source));
    }
    fs::remove_dir_all(&planted).unwrap();
    assert_eq!(
        rejected,
        [
            "crates/kaban-index-tantivy/src/lib.rs:1: fn f() { let q = \"SELECT id FROM node\"; }",
            "crates/kaban-index-tantivy/src/lib.rs:2: use rusqlite::Connection;",
            "crates/kaban-store-sqlite-extra/src/lib.rs:1: fn g() { \"DELETE FROM node\" }",
        ]
    );

    let root = workspace_root();
    let sources = production_sources(&root);
    assert!(
        sources
            .iter()
            .any(|p| p.ends_with("kaban/src/lease_keeper.rs")),
        "the walker must reach crates/kaban/src/lease_keeper.rs; saw {} files",
        sources.len()
    );
    assert!(
        !sources
            .iter()
            .any(|p| p.starts_with("crates/kaban-store-sqlite")),
        "the store crate is allowlisted"
    );
    assert!(
        !sources
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "tests")),
        "tests/ directories are skipped"
    );

    let mut found = Vec::new();
    for relative in &sources {
        let source = fs::read_to_string(root.join(relative)).unwrap();
        found.extend(violations(relative, &source));
    }
    assert!(
        found.is_empty(),
        "SQL text or `rusqlite::` outside crates/kaban-store-sqlite in production code (EN-E10, EN-E10a):\n{}",
        found.join("\n")
    );
}
