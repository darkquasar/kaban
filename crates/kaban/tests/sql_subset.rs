//! The portable SQL subset (spec 05 SS-24, portability rule 3): no production source or
//! migration adds `WITHOUT ROWID`, `WITH RECURSIVE` or a virtual table outside the named FTS5
//! allowances, and node text stays valid UTF-8 end to end (a lossy read is refused).
//!
//! The walker and the `#[cfg(test)]` skipper are copies of `lints.rs`'s with the same semantics:
//! every `.rs` file under `crates/` outside a `tests/` directory is read and a `mod` block
//! preceded by `#[cfg(test)]` is skipped by brace depth; every `.sql` file under `crates/` is
//! read whole. What is judged is SQL text: each string literal of a `.rs` file (Rust `//` and
//! `/* */` comments, char literals and lifetimes are lexed past; a `\`-newline continuation
//! joins its lines) and the whole of a `.sql` file. In that text SQL `--` and `/* */` comments
//! are removed and every whitespace run, newlines included, becomes one space, so a construct
//! split across lines or around a comment is still the token sequence it is; the match is
//! case-insensitive and on whole words. Every `CREATE VIRTUAL TABLE` declaration is judged on
//! its own: its name must be a frozen allowance of that file and its module must be `fts5`.
//! Test code is exempt. A finding names the file and the line of the construct's first token
//! and echoes that source line: it is our own source.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::ActorId;
use kaban_core::{
    Actor, MemoryStore, NewNode, NodeId, RememberOutcome, StoreError, WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use std::fs;
use std::path::{Path, PathBuf};

/// One named virtual-table allowance: `CREATE VIRTUAL TABLE <table> USING fts5` in `path`.
struct VirtualTable {
    path: &'static str,
    table: &'static str,
}

/// The virtual tables that exist today, frozen (spec 05 SS-24): the SQLite adapter's
/// `node_fts` in the three historical migrations that (re)create it (retired by 0011, spec 05
/// SS-16, together with the runtime `node_fts_tri` no source creates any more), and
/// `kaban-codegraph`'s `symbols_fts`. All are FTS5. A new virtual table is a new named line
/// here, and only an FTS5 table of the SQLite adapter (its sources or its migrations) may be
/// added. No `WITHOUT ROWID` and no production `WITH RECURSIVE` exist today, so those two
/// rules have no allowance at all.
const VIRTUAL_TABLES: [VirtualTable; 4] = [
    VirtualTable {
        path: "crates/kaban-store-sqlite/migrations/0001_spike_baseline.sql",
        table: "node_fts",
    },
    VirtualTable {
        path: "crates/kaban-store-sqlite/migrations/0004_engine.sql",
        table: "node_fts",
    },
    VirtualTable {
        path: "crates/kaban-store-sqlite/migrations/0005_fts_kind.sql",
        table: "node_fts",
    },
    VirtualTable {
        path: "crates/kaban-codegraph/src/schema.rs",
        table: "symbols_fts",
    },
];

/// The SQLite adapter: the only crate that may gain an FTS5 allowance.
const ADAPTER: &str = "crates/kaban-store-sqlite/";

/// The workspace root: this crate's manifest directory is `<root>/crates/kaban`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every `.rs` and `.sql` file under `<root>/crates/` outside a `tests/` directory, sorted,
/// relative to the root.
fn sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                let name = path.file_name().unwrap().to_str().unwrap();
                if name == "target" || name == "tests" {
                    continue;
                }
                walk(&path, out);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "rs" || ext == "sql")
            {
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

/// One SQL text to judge: its characters, each with the 1-based source line it came from.
struct SqlText {
    chars: Vec<(usize, char)>,
}

/// Whether `c` continues an identifier (so `r"` after one is no raw-string start).
fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Where the Rust lexer is while scanning kept lines.
enum Lex {
    Code,
    /// Inside `/* */`, at this nesting depth.
    Block(u32),
    /// Inside `"..."`.
    Str,
    /// Inside `r#"..."#` with this many hashes.
    Raw(usize),
}

/// The string literals of Rust source `lines`, in order: `//` and `/* */` comments, char
/// literals (`'"'`, `'\''`) and lifetimes are lexed past; inside a literal escapes stay as
/// written, a newline is kept, and a `\`-newline continuation is removed together with the
/// indentation that follows it.
fn rust_strings(lines: &[(usize, &str)]) -> Vec<SqlText> {
    let mut out = Vec::new();
    let mut state = Lex::Code;
    let mut current: Vec<(usize, char)> = Vec::new();
    let mut continued = false;
    for &(number, line) in lines {
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        if continued {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            continued = false;
        }
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            match state {
                Lex::Code => {
                    if c == '/' && next == Some('/') {
                        break;
                    }
                    if c == '/' && next == Some('*') {
                        state = Lex::Block(1);
                        i += 2;
                    } else if c == '"' {
                        state = Lex::Str;
                        i += 1;
                    } else if c == 'r'
                        && (i == 0 || !ident_char(chars[i - 1]) || chars[i - 1] == 'b')
                    {
                        let hashes = chars[i + 1..].iter().take_while(|&&h| h == '#').count();
                        if chars.get(i + 1 + hashes) == Some(&'"') {
                            state = Lex::Raw(hashes);
                            i += 2 + hashes;
                        } else {
                            i += 1;
                        }
                    } else if c == '\'' {
                        if next == Some('\\') {
                            // an escaped char literal: through its closing quote
                            i += 2;
                            while i < chars.len() && chars[i] != '\'' {
                                i += 1;
                            }
                            i += 1;
                        } else if chars.get(i + 2) == Some(&'\'') {
                            i += 3;
                        } else {
                            // a lifetime
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                Lex::Block(depth) => {
                    if c == '/' && next == Some('*') {
                        state = Lex::Block(depth + 1);
                        i += 2;
                    } else if c == '*' && next == Some('/') {
                        state = if depth == 1 {
                            Lex::Code
                        } else {
                            Lex::Block(depth - 1)
                        };
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Lex::Str => {
                    if c == '\\' {
                        match next {
                            None => {
                                continued = true;
                                i += 1;
                            }
                            Some(escaped) => {
                                current.push((number, c));
                                current.push((number, escaped));
                                i += 2;
                            }
                        }
                    } else if c == '"' {
                        out.push(SqlText {
                            chars: std::mem::take(&mut current),
                        });
                        state = Lex::Code;
                        i += 1;
                    } else {
                        current.push((number, c));
                        i += 1;
                    }
                }
                Lex::Raw(hashes) => {
                    if c == '"' && (1..=hashes).all(|k| chars.get(i + k) == Some(&'#')) {
                        out.push(SqlText {
                            chars: std::mem::take(&mut current),
                        });
                        state = Lex::Code;
                        i += 1 + hashes;
                    } else {
                        current.push((number, c));
                        i += 1;
                    }
                }
            }
        }
        match state {
            Lex::Str if !continued => current.push((number, '\n')),
            Lex::Raw(_) => current.push((number, '\n')),
            _ => {}
        }
    }
    out
}

/// The judged form of one SQL text: `--` comments to the end of their line and `/* */`
/// comments removed (each counts as whitespace), every whitespace run one space, ASCII
/// upper-cased. `lines[b]` is the source line of the byte at `b` of the text.
fn normalized(text: &SqlText) -> (String, Vec<usize>) {
    let mut upper = String::new();
    let mut lines = Vec::new();
    let mut pending_space = false;
    let mut i = 0;
    let chars = &text.chars;
    while i < chars.len() {
        let (line, c) = chars[i];
        let next = chars.get(i + 1).map(|&(_, n)| n);
        if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i].1 != '\n' {
                i += 1;
            }
            pending_space = true;
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len()
                && !(chars[i].1 == '*' && chars.get(i + 1).map(|&(_, n)| n) == Some('/'))
            {
                i += 1;
            }
            i += 2;
            pending_space = true;
        } else if c.is_whitespace() {
            pending_space = true;
            i += 1;
        } else {
            if pending_space && !upper.is_empty() {
                upper.push(' ');
                lines.push(line);
            }
            pending_space = false;
            let c = c.to_ascii_uppercase();
            upper.push(c);
            lines.extend(std::iter::repeat_n(line, c.len_utf8()));
            i += 1;
        }
    }
    (upper, lines)
}

/// The byte offsets of every whole-word occurrence of `tokens` in `upper`.
fn occurrences(upper: &str, tokens: &str) -> Vec<usize> {
    let bounded = |at: usize| {
        let before = upper[..at].chars().next_back();
        let after = upper[at + tokens.len()..].chars().next();
        !before.is_some_and(ident_char) && !after.is_some_and(ident_char)
    };
    upper
        .match_indices(tokens)
        .map(|(at, _)| at)
        .filter(|&at| bounded(at))
        .collect()
}

/// The declaration every virtual table starts with, judged one occurrence at a time.
const CREATE_VIRTUAL_TABLE: &str = "CREATE VIRTUAL TABLE";

/// Whether the `CREATE VIRTUAL TABLE` declaration whose text follows the keywords (`rest`,
/// normalized) is a named FTS5 allowance of `relative`: its name is on the frozen list for
/// that file and its module is `fts5`. A declaration naming no table or no module is never
/// allowed.
fn virtual_table_allowed(relative: &Path, rest: &str) -> bool {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(rest.len());
    let (table, after) = rest.split_at(name_end);
    let Some(after) = after.trim_start().strip_prefix("USING") else {
        return false;
    };
    let after = after.trim_start();
    let module_end = after.find(|c: char| !ident_char(c)).unwrap_or(after.len());
    after[..module_end] == *"FTS5"
        && VIRTUAL_TABLES
            .iter()
            .any(|a| Path::new(a.path) == relative && a.table.eq_ignore_ascii_case(table))
}

/// `path:line: rule: text` for every construct of `source` outside the subset: the SQL texts
/// of the module doc (every string literal of a `.rs` file outside `#[cfg(test)]`, the whole
/// of a `.sql` file), each judged normalized; `line` is where the construct's first token
/// stands and `text` that source line.
fn violations(relative: &Path, source: &str) -> Vec<String> {
    let is_sql = relative.extension().is_some_and(|ext| ext == "sql");
    let lines: Vec<(usize, &str)> = if is_sql {
        source
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l))
            .collect()
    } else {
        non_test_lines(source)
    };
    let texts = if is_sql {
        vec![SqlText {
            chars: lines
                .iter()
                .flat_map(|&(n, l)| l.chars().chain(std::iter::once('\n')).map(move |c| (n, c)))
                .collect(),
        }]
    } else {
        rust_strings(&lines)
    };
    let mut found = Vec::new();
    for text in &texts {
        let (upper, line_of) = normalized(text);
        let mut hits: Vec<(usize, &str)> = Vec::new();
        for rule in ["WITHOUT ROWID", "WITH RECURSIVE"] {
            hits.extend(occurrences(&upper, rule).into_iter().map(|at| (at, rule)));
        }
        for at in occurrences(&upper, CREATE_VIRTUAL_TABLE) {
            if !virtual_table_allowed(relative, &upper[at + CREATE_VIRTUAL_TABLE.len()..]) {
                hits.push((at, "VIRTUAL TABLE"));
            }
        }
        hits.sort_unstable();
        for (at, rule) in hits {
            let number = line_of[at];
            let echo = lines
                .iter()
                .find(|(n, _)| *n == number)
                .map_or("", |(_, l)| l.trim());
            found.push(format!("{}:{number}: {rule}: {echo}", relative.display()));
        }
    }
    found
}

#[test]
fn no_new_non_portable_sql_in_production_sources_or_migrations() {
    // planted violations, on invented sources, before the tree is read
    let migration = Path::new("crates/kaban-store-sqlite/migrations/0009_new.sql");
    assert_eq!(
        violations(
            migration,
            "-- WITHOUT ROWID in a comment\nCREATE TABLE t(a INTEGER PRIMARY KEY) without  rowid;\nCREATE VIRTUAL TABLE pack_fts USING fts5(text);\n"
        ),
        [
            "crates/kaban-store-sqlite/migrations/0009_new.sql:2: WITHOUT ROWID: CREATE TABLE t(a INTEGER PRIMARY KEY) without  rowid;",
            "crates/kaban-store-sqlite/migrations/0009_new.sql:3: VIRTUAL TABLE: CREATE VIRTUAL TABLE pack_fts USING fts5(text);",
        ],
        "a new migration's WITHOUT ROWID and its unnamed FTS5 table are rejected"
    );
    // the same constructs split across lines and around comments (c-3): the token sequence
    // is judged, and the finding names the first token's line
    assert_eq!(
        violations(
            migration,
            "CREATE TABLE t(a INTEGER PRIMARY KEY)\n  WITHOUT /* kept\n  rowid-less */\n  ROWID;\nWITH -- recursion\n  RECURSIVE r(i) AS (VALUES(1)) SELECT i FROM r;\nCREATE VIRTUAL\n  TABLE pack_fts\n  USING fts5(text);\n"
        ),
        [
            "crates/kaban-store-sqlite/migrations/0009_new.sql:2: WITHOUT ROWID: WITHOUT /* kept",
            "crates/kaban-store-sqlite/migrations/0009_new.sql:5: WITH RECURSIVE: WITH -- recursion",
            "crates/kaban-store-sqlite/migrations/0009_new.sql:7: VIRTUAL TABLE: CREATE VIRTUAL",
        ],
        "a multiline migration construct is found at its first token"
    );
    let api = Path::new("crates/kaban-api/src/x.rs");
    assert_eq!(
        violations(
            api,
            "fn f() { \"With Recursive r(i) AS (VALUES(1))\" }\nfn g() { \"CREATE VIRTUAL TABLE node_fts USING fts5(text)\" }\n#[cfg(test)]\nmod t {\n  fn h() { \"WITH RECURSIVE x(i) AS (VALUES(1))\" }\n}\n"
        ),
        [
            "crates/kaban-api/src/x.rs:1: WITH RECURSIVE: fn f() { \"With Recursive r(i) AS (VALUES(1))\" }",
            "crates/kaban-api/src/x.rs:2: VIRTUAL TABLE: fn g() { \"CREATE VIRTUAL TABLE node_fts USING fts5(text)\" }",
        ],
        "production WITH RECURSIVE and a virtual table outside the adapter are rejected; test code is exempt"
    );
    // Rust string literals spanning lines (c-3): a literal newline, a `\` continuation and a
    // raw string; the finding names the line of the first token
    let adapter = Path::new("crates/kaban-store-sqlite/src/x.rs");
    assert_eq!(
        violations(
            adapter,
            "const A: &str = \"CREATE TABLE t(a)\n    WITHOUT\n    ROWID\";\nconst B: &str = \"WITH \\\n    RECURSIVE r(i) AS (VALUES(1))\";\nlet c = r#\"CREATE VIRTUAL\n    TABLE other USING fts5(x)\"#;\n"
        ),
        [
            "crates/kaban-store-sqlite/src/x.rs:2: WITHOUT ROWID: WITHOUT",
            "crates/kaban-store-sqlite/src/x.rs:4: WITH RECURSIVE: const B: &str = \"WITH \\",
            "crates/kaban-store-sqlite/src/x.rs:6: VIRTUAL TABLE: let c = r#\"CREATE VIRTUAL",
        ],
        "a construct split across the lines of one Rust string is found at its first token"
    );
    // Rust comments and char literals are not SQL text
    assert_eq!(
        violations(
            adapter,
            "fn k() { let q = '\"'; let s = \"ok\"; /* \"WITH RECURSIVE\" */ } // WITHOUT ROWID\n/// CREATE VIRTUAL TABLE in a doc comment\n"
        ),
        Vec::<String>::new(),
        "comments and char literals are lexed past"
    );
    let baseline = Path::new("crates/kaban-store-sqlite/migrations/0001_spike_baseline.sql");
    assert_eq!(
        violations(
            baseline,
            "CREATE VIRTUAL TABLE node_fts USING fts5(text,summary,tokenize='porter unicode61');\nCREATE VIRTUAL TABLE node_fts USING rtree(id, a, b);\nCREATE VIRTUAL TABLE node_fts USING rtree(id, a, b); -- USING fts5\n"
        ),
        [
            "crates/kaban-store-sqlite/migrations/0001_spike_baseline.sql:2: VIRTUAL TABLE: CREATE VIRTUAL TABLE node_fts USING rtree(id, a, b);",
            "crates/kaban-store-sqlite/migrations/0001_spike_baseline.sql:3: VIRTUAL TABLE: CREATE VIRTUAL TABLE node_fts USING rtree(id, a, b); -- USING fts5",
        ],
        "a named allowance admits its FTS5 table only; a trailing comment names no module"
    );
    // the retired runtime table has no allowance left: a source that recreated it would fail
    assert_eq!(
        violations(
            Path::new("crates/kaban-store-sqlite/src/trigram.rs"),
            "\"CREATE VIRTUAL TABLE node_fts_tri USING fts5(text,summary,tokenize='trigram');\"\n"
        ),
        [
            "crates/kaban-store-sqlite/src/trigram.rs:1: VIRTUAL TABLE: \"CREATE VIRTUAL TABLE node_fts_tri USING fts5(text,summary,tokenize='trigram');\""
        ],
        "node_fts_tri was retired with FTS5 (spec 05 SS-16)"
    );
    // every declaration is judged on its own (c-4): the allowed one does not cover the next
    let engine = Path::new("crates/kaban-store-sqlite/migrations/0005_fts_kind.sql");
    assert_eq!(
        violations(
            engine,
            "CREATE VIRTUAL TABLE node_fts USING fts5(text); CREATE VIRTUAL TABLE extra USING rtree(id,min_x,max_x);\nCREATE VIRTUAL TABLE node_fts USING rtree(id,min_x,max_x); -- USING fts5\n"
        ),
        [
            "crates/kaban-store-sqlite/migrations/0005_fts_kind.sql:1: VIRTUAL TABLE: CREATE VIRTUAL TABLE node_fts USING fts5(text); CREATE VIRTUAL TABLE extra USING rtree(id,min_x,max_x);",
            "crates/kaban-store-sqlite/migrations/0005_fts_kind.sql:2: VIRTUAL TABLE: CREATE VIRTUAL TABLE node_fts USING rtree(id,min_x,max_x); -- USING fts5",
        ],
        "two declarations on one line are two judgements; the module is the declaration's own"
    );
    let codegraph = Path::new("crates/kaban-codegraph/src/schema.rs");
    assert_eq!(
        violations(
            codegraph,
            "\"CREATE VIRTUAL TABLE symbols_fts USING fts5(name, key);\"\n\"CREATE VIRTUAL TABLE symbols_tri USING fts5(name);\"\n"
        ),
        [
            "crates/kaban-codegraph/src/schema.rs:2: VIRTUAL TABLE: \"CREATE VIRTUAL TABLE symbols_tri USING fts5(name);\""
        ],
        "codegraph keeps symbols_fts and gains nothing else"
    );
    assert_eq!(
        violations(
            Path::new("crates/kaban-store-sqlite/migrations/0001_spike_baseline.sql"),
            "CREATE VIRTUAL TABLE IF NOT EXISTS node_fts USING fts5(\n"
        ),
        Vec::<String>::new(),
        "a frozen historical migration keeps its node_fts"
    );
    // the allowlist itself stays narrow: FTS5 in the adapter, plus codegraph's symbols_fts
    for a in &VIRTUAL_TABLES {
        assert!(
            a.path.starts_with(ADAPTER)
                || (a.path == "crates/kaban-codegraph/src/schema.rs" && a.table == "symbols_fts"),
            "{} names a virtual table outside the SQLite adapter",
            a.path
        );
    }

    let root = workspace_root();
    let files = sources(&root);
    for reached in [
        "crates/kaban-store-sqlite/schema.sql",
        "crates/kaban-codegraph/src/schema.rs",
        "crates/kaban-store-sqlite/src/sqlite.rs",
    ] {
        assert!(
            files.iter().any(|p| p == Path::new(reached)),
            "the walker must reach {reached}; saw {} files",
            files.len()
        );
    }
    let mut found = Vec::new();
    for relative in &files {
        let source = fs::read_to_string(root.join(relative)).unwrap();
        found.extend(violations(relative, &source));
    }
    assert!(
        found.is_empty(),
        "SQL outside the portable subset (spec 05 SS-24; virtual tables allowed: {:?}):\n{}",
        VIRTUAL_TABLES
            .iter()
            .map(|a| format!("{}:{}", a.path, a.table))
            .collect::<Vec<_>>(),
        found.join("\n")
    );
}

/// Private scratch directory (db + keys), removed on drop (also when the test panics).
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d =
            std::env::temp_dir().join(format!("kaban-sql-subset-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn node_text_round_trips_exactly_and_invalid_utf8_is_refused_not_replaced() {
    let s = Scratch::new("utf8");
    let db = s.0.join("kaban.db");
    let store = SqliteStore::open(&db, &s.0.join("keys")).unwrap();
    let text = "na\u{ef}ve caf\u{e9}: \u{6587}\u{5b57}, \u{1f980}, e\u{301} (NFD kept)";
    let ctx = WriteCtx::new("pv", WorkspaceId::default(), Actor::Agent);
    let new = NewNode {
        kind: "Note".into(),
        text: text.into(),
        ..Default::default()
    };
    let RememberOutcome::Created(node) = store.remember(new, &[], &ctx).unwrap() else {
        panic!("fresh store");
    };
    let id = NodeId::parse(&node.id).unwrap();
    let read = store.get(&id, false).unwrap().unwrap();
    assert_eq!(
        read.text.as_bytes(),
        text.as_bytes(),
        "byte-exact round trip"
    );

    // plant bytes that are not UTF-8 in the TEXT column behind the store's back
    let conn = rusqlite::Connection::open(&db).unwrap();
    SqliteStore::bind_writer(&conn, &ActorId::default()).unwrap();
    conn.execute(
        "UPDATE node SET text = CAST(X'66FF6F' AS TEXT) WHERE id = ?1",
        [&node.id],
    )
    .unwrap();
    let stored: String = conn
        .query_row(
            "SELECT typeof(text) FROM node WHERE id = ?1",
            [&node.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "text");
    drop(conn);
    // refused by the driver's strict decoding, never decoded with U+FFFD
    match store.get(&id, false) {
        Err(StoreError::Backend { source }) => assert!(
            matches!(
                source.driver().downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::Utf8Error(..))
            ),
            "the refusal is the UTF-8 conversion failure: {:?}",
            source.driver()
        ),
        other => panic!(
            "invalid UTF-8 must be refused: {:?}",
            other.map(|n| n.map(|n| n.text))
        ),
    }
}
