//! Grep lints as tests for the v4 crates (stage1-spec G1-E1, G1-E3, G1-S2, G1-S3; stage1-plan
//! CG2.4, RV-1, SO-8): `no_unsafe_in_new_crates`, `no_scip_symbol_shape`,
//! `no_spawn_outside_runner`, `no_trusted_internal_outside_runners`.
//!
//! The walker and the `#[cfg(test)]` skipper are copies of `sql_boundary.rs`'s with the same
//! semantics (E1.0 left them as plain functions for exactly this): every `.rs` file under
//! `crates/` is read; a `mod` block preceded by `#[cfg(test)]` is skipped by brace depth; a line
//! whose first non-blank characters are `//` is skipped; a trailing comment is judged with its
//! line and string literals count. Each lint names the files it allows; a hit outside them is
//! echoed in the failure (it is our own source).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::fs;
use std::path::{Path, PathBuf};

/// The v4 crates that must never contain `unsafe` (G1-E1; `unsafe_code = "forbid"` is inherited
/// from the workspace lints, this lint catches a crate that stops inheriting them).
const NEW_CRATES: [&str; 2] = ["crates/kaban-codegraph/", "crates/kaban-index/"];

/// The only production files that may construct a `Command` (G1-E3, G1-S3): the one-shot runner
/// in `kaban-core::process` (`exec.rs` builds the `Command`, `run.rs` spawns it), the future
/// indexer runner, and the M3-22 bench fixture builder that shells out to `git` for the N2
/// corpus repository (pre-existing since C3.4; listed so the lint states the truth, reported as a
/// deviation from the G1-E3 sentence in the CG2.4 report).
const SPAWN_SITES: [&str; 4] = [
    "crates/kaban-core/src/process/exec.rs",
    "crates/kaban-core/src/process/run.rs",
    "crates/kaban-index/src/runner.rs",
    "crates/kaban/src/bench/corpus.rs",
];

/// The only production files that may name `TrustedInternal(` or `with_trusted` (RV-1, SO-8):
/// the allowlist module that defines them, the `GitRunner` wrapper and the future indexer runner.
const TRUSTED_INTERNAL_SITES: [&str; 3] = [
    "crates/kaban-core/src/process/env.rs",
    "crates/kaban-api/src/git.rs",
    "crates/kaban-index/src/runner.rs",
];

/// The workspace root: this crate's manifest directory is `<root>/crates/kaban`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every `.rs` file under `<root>/crates/`, sorted, relative to the root; `tests/` directories
/// are included when `with_tests` is set.
fn sources(root: &Path, with_tests: bool) -> Vec<PathBuf> {
    fn walk(dir: &Path, with_tests: bool, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                let name = path.file_name().unwrap().to_str().unwrap();
                if name == "target" || (!with_tests && name == "tests") {
                    continue;
                }
                walk(&path, with_tests, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&root.join("crates"), with_tests, &mut files);
    let mut relative: Vec<PathBuf> = files
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
        .collect();
    relative.sort();
    relative
}

/// Count the brace delta of one line, ignoring braces inside `"..."` string literals (with
/// backslash escapes) and after a `//` comment start.
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

/// Where the `#[cfg(test)]` skipper is while scanning a file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    Attributed,
    Skipping(i64),
}

/// The state after the item a `#[cfg(test)]` decorates and whether its line is kept.
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

/// The lines of `source` outside any `#[cfg(test)] mod` block, with 1-based numbers.
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
                    continue;
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

/// `path:line: text` for every kept, non-comment line of `source` that `hit` matches.
fn violations(relative: &Path, source: &str, hit: impl Fn(&str) -> bool) -> Vec<String> {
    non_test_lines(source)
        .into_iter()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| hit(line))
        .map(|(number, line)| format!("{}:{number}: {}", relative.display(), line.trim()))
        .collect()
}

/// Whether `line` contains `word` as a whole identifier (not part of a longer one).
fn has_word(line: &str, word: &str) -> bool {
    line.match_indices(word).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + word.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Collect the violations of `hit` over `files`, skipping the `allowed` paths.
fn scan(
    root: &Path,
    files: &[PathBuf],
    allowed: &[&str],
    hit: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut found = Vec::new();
    for relative in files {
        if allowed.iter().any(|a| Path::new(a) == relative) {
            continue;
        }
        let source = fs::read_to_string(root.join(relative)).unwrap();
        found.extend(violations(relative, &source, &hit));
    }
    found
}

#[test]
fn no_unsafe_in_new_crates() {
    assert_eq!(
        violations(
            Path::new("x.rs"),
            "// unsafe in a comment\nlet x = unsafe { 1 }; // code\n",
            |l| has_word(l, "unsafe")
        ),
        ["x.rs:2: let x = unsafe { 1 }; // code"]
    );
    assert_eq!(
        violations(Path::new("x.rs"), "fn unsafe_free() {}\n", |l| has_word(
            l, "unsafe"
        )),
        Vec::<String>::new(),
        "a longer identifier is not the keyword"
    );

    let root = workspace_root();
    let files: Vec<PathBuf> = sources(&root, true)
        .into_iter()
        .filter(|p| {
            NEW_CRATES
                .iter()
                .any(|c| p.to_str().unwrap().starts_with(c))
        })
        .collect();
    assert!(
        files
            .iter()
            .any(|p| p.ends_with("kaban-codegraph/src/lib.rs")),
        "the walker must reach crates/kaban-codegraph/src/lib.rs"
    );
    let found = scan(&root, &files, &[], |l| has_word(l, "unsafe"));
    assert!(
        found.is_empty(),
        "`unsafe` in a v4 crate (G1-E1):\n{}",
        found.join("\n")
    );
    for crate_dir in NEW_CRATES {
        let manifest = root.join(crate_dir).join("Cargo.toml");
        if let Ok(text) = fs::read_to_string(&manifest) {
            assert!(
                text.contains("[lints]\nworkspace = true"),
                "{crate_dir}Cargo.toml must adopt the workspace lints (unsafe_code = \"forbid\")"
            );
        }
    }
}

#[test]
fn no_scip_symbol_shape() {
    let root = workspace_root();
    let files = sources(&root, true);
    // this file names the identifier in order to look for it
    let found = scan(&root, &files, &["crates/kaban/tests/lints.rs"], |l| {
        has_word(l, "ScipSymbol")
    });
    assert!(
        found.is_empty(),
        "a `ScipSymbol` shape exists; symbols are content, grammar-checked in the typed handler (G1-S2):\n{}",
        found.join("\n")
    );
}

#[test]
fn no_spawn_outside_runner() {
    let root = workspace_root();
    let files = sources(&root, false);
    let hit = |l: &str| l.contains("Command::new");
    assert!(
        files
            .iter()
            .any(|p| p == Path::new("crates/kaban-core/src/process/exec.rs")),
        "the walker must reach the runner"
    );
    let runner = fs::read_to_string(root.join("crates/kaban-core/src/process/exec.rs")).unwrap();
    assert!(
        !violations(Path::new("exec.rs"), &runner, hit).is_empty(),
        "the runner constructs the Command (the lint sees real sites)"
    );
    let found = scan(&root, &files, &SPAWN_SITES, hit);
    assert!(
        found.is_empty(),
        "`Command::new` outside the runners (G1-E3, G1-S3; allowed: {SPAWN_SITES:?}):\n{}",
        found.join("\n")
    );
}

#[test]
fn no_trusted_internal_outside_runners() {
    let root = workspace_root();
    let files = sources(&root, false);
    let hit = |l: &str| l.contains("TrustedInternal(") || l.contains("with_trusted");
    let env = fs::read_to_string(root.join("crates/kaban-core/src/process/env.rs")).unwrap();
    assert!(
        !violations(Path::new("env.rs"), &env, hit).is_empty(),
        "env.rs defines the names (the lint sees real sites)"
    );
    let found = scan(&root, &files, &TRUSTED_INTERNAL_SITES, hit);
    assert!(
        found.is_empty(),
        "`TrustedInternal(` or `with_trusted` outside the runners (RV-1, SO-8; allowed: {TRUSTED_INTERNAL_SITES:?}):\n{}",
        found.join("\n")
    );
}
