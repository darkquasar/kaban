//! Placement guards at the E2 exit (engine-spec EN-E10 `Verify:`; engine-plan E2.12 item 3;
//! SO-5, SO-10). What they are (RW5-4): textual markers over source lines and edges of the
//! crate graph, not a parse of the code and not a proof of the whole OWN-61 boundary.
//!
//! * Textual source markers: no production line of the two crates above the store seam,
//!   `kaban-api` and `kaban-ingest`, carries an uppercase SQL keyword marker, a SQL statement
//!   shape in any letter case (`select .. from`, `insert into`, `update .. set`, `delete from`,
//!   `create table`, ...), a `rusqlite` path or the concrete store's name `SqliteStore`, and
//!   none names the attached code-graph schema (`cg.symbols`, `cg.files`, `cg.edges`: the
//!   gazetteer reads symbols through `GraphStore::snapshot_symbols`, SO-10). The composition
//!   root (`crates/kaban`, the binary that opens the store and the bench) and the store crate
//!   itself are the named exceptions for `SqliteStore` ([`COMPOSITION_EXCEPTIONS`]); the scan
//!   covers every other workspace crate's production sources for it.
//! * Direct crate edges (`cargo metadata --no-deps`, the declared dependencies): no
//!   `kaban-core -> kaban-api` edge, no `kaban-codegraph -> {kaban-policy, kaban-store-sqlite,
//!   kaban-api}` edge and no direct normal `rusqlite` dependency of `kaban-api`, `kaban-ingest`,
//!   `kaban-core` or `kaban-policy` (a dev-dependency is not a production edge, as
//!   `scripts/ci/ci.sh` `store_boundary_deps` reads it).
//! * Resolved normal edges (`cargo metadata --all-features` without `--no-deps`,
//!   `resolve.nodes[].deps[].dep_kinds`): every normal path from `kaban-api` or `kaban-ingest`
//!   to `rusqlite` runs through `kaban-codegraph` (the SO-5 grant), and neither reaches
//!   `kaban-store-sqlite` through normal edges.
//!
//! The source scan is textual on purpose and copies the E1.0 helper's semantics
//! (`crates/kaban/tests/sql_boundary.rs`: `production_sources`, `non_test_lines`): every `.rs`
//! file under the two crates' `src/`, a `mod` block preceded by `#[cfg(test)]` skipped to its
//! matching close, a line whose first non-whitespace characters are `//` skipped, string
//! literals counted. A finding is reported with its file and line and is never fixed here
//! (the test asserts the true state; a finding is a stop for the coordinator).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The uppercase SQL keyword and driver markers no production line of the two crates may
/// contain (exact text, case-sensitive).
const FORBIDDEN: [&str; 6] = [
    "SELECT ",
    "INSERT ",
    "UPDATE ",
    "DELETE FROM",
    "rusqlite::",
    "use rusqlite",
];

/// SQL statement shapes matched in any letter case (RW5-4): a keyword and the word that makes
/// it a statement on the same line, so prose such as `select by ref` or a patch header
/// `*** Update File:` is not one. Each entry is `(keyword, then)`: the line, lowercased,
/// contains `keyword` and after it `then` (`""` when the keyword alone is the shape).
const SQL_SHAPES: [(&str, &str); 9] = [
    ("select ", " from "),
    ("insert into ", ""),
    ("insert or ", " into "),
    ("update ", " set "),
    ("delete from ", ""),
    ("create table", ""),
    ("create index", ""),
    ("drop table", ""),
    ("pragma ", "="),
];

/// The concrete store (RW5-4): only the store crate and the composition root name it.
const CONCRETE_STORE: [&str; 1] = ["SqliteStore"];

/// The crates allowed to name [`CONCRETE_STORE`] in production code: the store crate that
/// defines it and the composition root that opens it (the binary's `main`, its CLI commands,
/// the MCP server, the lease keeper and the bench; the owner boundary item of the E2-end
/// review, kept explicit here rather than silently outside the scan).
const COMPOSITION_EXCEPTIONS: [&str; 2] = ["kaban-store-sqlite", "kaban"];

/// The attached code-graph schema's qualified names (SO-10): the store's own SQL names them,
/// nothing above the seam may.
const FORBIDDEN_CG: [&str; 3] = ["cg.symbols", "cg.files", "cg.edges"];

/// The crates above the store seam whose sources the rule applies to.
const CHECKED_CRATES: [&str; 2] = ["kaban-api", "kaban-ingest"];

/// Path components that mark test code rather than production sources.
const SKIPPED_COMPONENTS: [&str; 2] = ["tests", "migrations"];

/// The workspace root: this crate's manifest directory is `<root>/crates/kaban-api`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every `.rs` file under `<root>/crates/<crate>/src/` of the checked crates, sorted, relative
/// to the root; `tests/` and `migrations/` directories are skipped.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    sources_of(root, &CHECKED_CRATES)
}

/// [`production_sources`] over the named crates.
fn sources_of(root: &Path, crates: &[&str]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                let name = path.file_name().unwrap().to_str().unwrap();
                if SKIPPED_COMPONENTS.contains(&name) {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    for krate in crates {
        walk(&root.join("crates").join(krate).join("src"), &mut files);
    }
    let mut relative: Vec<PathBuf> = files
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
        .collect();
    relative.sort();
    relative
}

/// Count the brace delta of one line, ignoring braces inside `"..."` string literals (with
/// backslash escapes) and after a `//` comment start (the E1.0 helper's rule).
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

/// The lines of `source` that are not inside a `mod` block preceded by `#[cfg(test)]`, each
/// with its 1-based line number (the E1.0 helper's contract).
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

/// Where the `#[cfg(test)]` skipper is while scanning a file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    Attributed,
    Skipping(i64),
}

/// The findings in one file: `path:line: text` for every kept, non-comment line that contains
/// one of `markers` (exact text).
fn findings(relative: &Path, source: &str, markers: &[&str]) -> Vec<String> {
    findings_by(relative, source, |line| {
        markers.iter().any(|marker| line.contains(marker))
    })
}

/// Whether `line` carries one of the [`SQL_SHAPES`] in any letter case.
fn sql_shape(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    SQL_SHAPES.iter().any(|(keyword, then)| {
        lower
            .find(keyword)
            .is_some_and(|at| lower[at + keyword.len()..].contains(then))
    })
}

/// The findings in one file for the [`SQL_SHAPES`].
fn shape_findings(relative: &Path, source: &str) -> Vec<String> {
    findings_by(relative, source, sql_shape)
}

/// `path:line: text` for every kept, non-comment line `hit` accepts.
fn findings_by(relative: &Path, source: &str, hit: impl Fn(&str) -> bool) -> Vec<String> {
    non_test_lines(source)
        .into_iter()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| hit(line))
        .map(|(number, line)| format!("{}:{number}: {}", relative.display(), line.trim()))
        .collect()
}

/// `cargo metadata --offline --format-version 1` plus `extra`, parsed.
fn metadata(extra: &[&str]) -> Value {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--offline", "--format-version", "1"])
        .args(extra)
        .current_dir(workspace_root())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo metadata {extra:?} failed: exit {:?}, stderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `(name, kind)` of every declared (direct) dependency edge of each workspace member from
/// `cargo metadata --no-deps --offline`; `kind` is `normal`, `dev` or `build`.
fn member_edges() -> BTreeMap<String, Vec<(String, String)>> {
    let meta = metadata(&["--no-deps"]);
    let mut edges = BTreeMap::new();
    for pkg in meta["packages"].as_array().unwrap() {
        let name = pkg["name"].as_str().unwrap().to_owned();
        let deps = pkg["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["name"].as_str().unwrap().to_owned(),
                    d["kind"].as_str().unwrap_or("normal").to_owned(),
                )
            })
            .collect();
        edges.insert(name, deps);
    }
    edges
}

/// The resolved normal dependency graph (`cargo metadata --all-features --offline`, no
/// `--no-deps`): package name to the names it reaches through a `dep_kinds` entry whose `kind`
/// is null (normal; `dev` and `build` edges are not production edges).
fn resolved_normal_edges() -> BTreeMap<String, BTreeSet<String>> {
    let meta = metadata(&["--all-features"]);
    let names: BTreeMap<&str, &str> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for node in meta["resolve"]["nodes"].as_array().unwrap() {
        let from = names[node["id"].as_str().unwrap()].to_owned();
        let normal = node["deps"].as_array().unwrap().iter().filter(|d| {
            d["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["kind"].is_null())
        });
        let to = graph.entry(from).or_default();
        to.extend(normal.map(|d| names[d["pkg"].as_str().unwrap()].to_owned()));
    }
    graph
}

/// Whether `to` is reachable from `from` over `graph` without passing through `avoid`.
fn reaches(
    graph: &BTreeMap<String, BTreeSet<String>>,
    from: &str,
    to: &str,
    avoid: Option<&str>,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from.to_owned()];
    while let Some(at) = stack.pop() {
        if at == to {
            return true;
        }
        if Some(at.as_str()) == avoid || !seen.insert(at.clone()) {
            continue;
        }
        stack.extend(graph.get(&at).into_iter().flatten().cloned());
    }
    false
}

/// The resolved-graph violations (SO-5): a normal path from `kaban-api` or `kaban-ingest` to
/// `rusqlite` that avoids `kaban-codegraph`, or any normal path from them to
/// `kaban-store-sqlite`.
fn transitive_violations(graph: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    let mut out = Vec::new();
    for from in ["kaban-api", "kaban-ingest"] {
        if reaches(graph, from, "rusqlite", Some("kaban-codegraph")) {
            out.push(format!("{from} reaches rusqlite without kaban-codegraph"));
        }
        if reaches(graph, from, "kaban-store-sqlite", None) {
            out.push(format!("{from} reaches kaban-store-sqlite (normal)"));
        }
    }
    out
}

/// The forbidden edges of a dependency map (EN-E10 `Verify:`, SO-5): `kaban-core -> kaban-api`
/// of any kind, `kaban-codegraph -> {kaban-policy, kaban-store-sqlite, kaban-api}` of any kind,
/// and a normal `rusqlite` edge from `kaban-api`, `kaban-ingest`, `kaban-core` or
/// `kaban-policy` (so `rusqlite` reaches `kaban-api` through `kaban-codegraph` alone).
fn forbidden_edges(edges: &BTreeMap<String, Vec<(String, String)>>) -> Vec<String> {
    let mut out = Vec::new();
    let any_kind = |from: &str, to: &str| {
        edges
            .get(from)
            .into_iter()
            .flatten()
            .filter(|(name, _)| name == to)
            .map(|(_, kind)| format!("{from} -> {to} ({kind})"))
            .collect::<Vec<_>>()
    };
    out.extend(any_kind("kaban-core", "kaban-api"));
    for to in ["kaban-policy", "kaban-store-sqlite", "kaban-api"] {
        out.extend(any_kind("kaban-codegraph", to));
    }
    for from in ["kaban-api", "kaban-ingest", "kaban-core", "kaban-policy"] {
        out.extend(
            edges
                .get(from)
                .into_iter()
                .flatten()
                .filter(|(name, kind)| name == "rusqlite" && kind == "normal")
                .map(|_| format!("{from} -> rusqlite (normal)")),
        );
    }
    out
}

/// EN-E10, SO-10: no SQL text, no `rusqlite` type and no `cg.` schema name in the production
/// sources of `kaban-api` and `kaban-ingest`. Negative first: an invented line with a SQL
/// keyword, one with a `cg.` name and one with the driver import are each caught by path and
/// line, and the same text inside a `#[cfg(test)]` module or a comment is not.
#[test]
fn kaban_api_and_kaban_ingest_sources_carry_no_sql_and_no_rusqlite_type() {
    assert_eq!(
        findings(
            Path::new("x.rs"),
            "// SELECT in a comment\nlet q = \"SELECT 1\"; // code wins\n",
            &FORBIDDEN
        ),
        ["x.rs:2: let q = \"SELECT 1\"; // code wins"]
    );
    assert_eq!(
        findings(
            Path::new("y.rs"),
            "fn f() {}\nlet t = \"cg.symbols\";\n",
            &FORBIDDEN_CG
        ),
        ["y.rs:2: let t = \"cg.symbols\";"]
    );
    assert_eq!(
        findings(Path::new("z.rs"), "use rusqlite::Connection;\n", &FORBIDDEN),
        ["z.rs:1: use rusqlite::Connection;"]
    );
    assert_eq!(
        findings(
            Path::new("t.rs"),
            "fn f() {}\n#[cfg(test)]\nmod t {\n use rusqlite::Connection;\n let q = \"SELECT 1\";\n}\n",
            &FORBIDDEN
        ),
        Vec::<String>::new()
    );
    // RW5-4: lowercase and mixed-case statements are caught by shape; the uppercase markers
    // alone miss them, and prose with a keyword but no statement shape is not a finding
    let lower = "let q = \"select id from node where rid=?\";\nconn.execute(\"update node set rev=1\");\nlet d = \"Delete From edge\";\nlet i = \"insert into t values (1)\";\n";
    assert_eq!(
        findings(Path::new("l.rs"), lower, &FORBIDDEN),
        Vec::<String>::new()
    );
    assert_eq!(
        shape_findings(Path::new("l.rs"), lower),
        [
            "l.rs:1: let q = \"select id from node where rid=?\";",
            "l.rs:2: conn.execute(\"update node set rev=1\");",
            "l.rs:3: let d = \"Delete From edge\";",
            "l.rs:4: let i = \"insert into t values (1)\";",
        ]
    );
    assert_eq!(
        shape_findings(
            Path::new("p.rs"),
            "#[error(\"ambiguous: select by ref\")]\nline.strip_prefix(\"*** Update File: \")\n"
        ),
        Vec::<String>::new()
    );
    // the concrete store: an import is caught, the same import in a test module is not
    assert_eq!(
        findings(
            Path::new("s.rs"),
            "use kaban_store_sqlite::SqliteStore;\n#[cfg(test)]\nmod t {\n use kaban_store_sqlite::SqliteStore;\n}\n",
            &CONCRETE_STORE
        ),
        ["s.rs:1: use kaban_store_sqlite::SqliteStore;"]
    );

    let root = workspace_root();
    let sources = production_sources(&root);
    assert!(
        sources
            .iter()
            .any(|p| p.ends_with("kaban-api/src/hooks.rs")),
        "the walker must reach crates/kaban-api/src/hooks.rs; saw {} files",
        sources.len()
    );
    assert!(
        sources
            .iter()
            .any(|p| p.starts_with("crates/kaban-ingest/src")),
        "the walker must reach crates/kaban-ingest/src"
    );
    assert!(
        sources.iter().all(|p| {
            p.starts_with("crates/kaban-api/src") || p.starts_with("crates/kaban-ingest/src")
        }),
        "only the two crates above the seam are read"
    );

    let mut sql = Vec::new();
    let mut cg = Vec::new();
    for relative in &sources {
        let source = fs::read_to_string(root.join(relative)).unwrap();
        sql.extend(findings(relative, &source, &FORBIDDEN));
        sql.extend(shape_findings(relative, &source));
        cg.extend(findings(relative, &source, &FORBIDDEN_CG));
    }
    assert!(
        sql.is_empty(),
        "SQL text or a rusqlite type in kaban-api or kaban-ingest production code (EN-E10):\n{}",
        sql.join("\n")
    );
    assert!(
        cg.is_empty(),
        "a code-graph schema name above the store seam (SO-10):\n{}",
        cg.join("\n")
    );
}

/// RW5-4, EN-E10: no workspace crate outside [`COMPOSITION_EXCEPTIONS`] names the concrete
/// store `SqliteStore` in a production line (a textual marker; a test module or a comment is
/// not production code). The negative is the exception itself: the composition root does name
/// it, so the scan would see an unlisted crate that did.
#[test]
fn only_the_store_and_the_composition_root_name_the_concrete_store() {
    let root = workspace_root();
    let mut members: Vec<String> = fs::read_dir(root.join("crates"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_str().unwrap().to_owned())
        .filter(|name| root.join("crates").join(name).join("src").is_dir())
        .collect();
    members.sort();
    for checked in CHECKED_CRATES {
        assert!(members.iter().any(|m| m == checked), "{checked} is scanned");
    }
    let scan = |crates: &[&str]| -> Vec<String> {
        sources_of(&root, crates)
            .iter()
            .flat_map(|relative| {
                let source = fs::read_to_string(root.join(relative)).unwrap();
                findings(relative, &source, &CONCRETE_STORE)
            })
            .collect()
    };
    assert!(
        !scan(&["kaban"]).is_empty(),
        "the composition root names SqliteStore, so the marker sees a production use"
    );
    let others: Vec<&str> = members
        .iter()
        .map(String::as_str)
        .filter(|m| !COMPOSITION_EXCEPTIONS.contains(m))
        .collect();
    let found = scan(&others);
    assert!(
        found.is_empty(),
        "the concrete store named outside the composition exceptions (EN-E10):\n{}",
        found.join("\n")
    );
}

/// EN-E10 `Verify:`, SO-5: the dependency graph keeps its direction. Negative first: an
/// invented map with a `kaban-core -> kaban-api` dev edge, a `kaban-codegraph -> kaban-api`
/// edge and a normal `rusqlite` under `kaban-ingest` names all three, and a dev `rusqlite` under
/// `kaban-api` is not a finding. Then the real `cargo metadata --no-deps` has none. These are
/// direct (declared) edges; the transitive route is the resolved-graph test below.
#[test]
fn the_crate_graph_has_no_forbidden_edge_and_no_direct_rusqlite_above_the_store() {
    let dep = |name: &str, kind: &str| (name.to_owned(), kind.to_owned());
    let invented: BTreeMap<String, Vec<(String, String)>> = [
        ("kaban-core".to_owned(), vec![dep("kaban-api", "dev")]),
        (
            "kaban-codegraph".to_owned(),
            vec![dep("kaban-core", "normal"), dep("kaban-api", "normal")],
        ),
        ("kaban-ingest".to_owned(), vec![dep("rusqlite", "normal")]),
        ("kaban-api".to_owned(), vec![dep("rusqlite", "dev")]),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        forbidden_edges(&invented),
        [
            "kaban-core -> kaban-api (dev)",
            "kaban-codegraph -> kaban-api (normal)",
            "kaban-ingest -> rusqlite (normal)",
        ]
    );

    let edges = member_edges();
    for member in [
        "kaban-core",
        "kaban-api",
        "kaban-ingest",
        "kaban-codegraph",
        "kaban-store-sqlite",
    ] {
        assert!(edges.contains_key(member), "{member} is a workspace member");
    }
    // the SO-5 grant is real: the store and the code-graph reader are the two rusqlite owners
    assert!(
        edges["kaban-codegraph"]
            .iter()
            .any(|(name, kind)| name == "rusqlite" && kind == "normal"),
        "kaban-codegraph depends on rusqlite (the SO-5 grant)"
    );
    assert!(
        edges["kaban-api"]
            .iter()
            .any(|(name, kind)| name == "kaban-codegraph" && kind == "normal"),
        "kaban-api -> kaban-codegraph is a direct normal edge (the SO-5 grant's first hop)"
    );
    let found = forbidden_edges(&edges);
    assert!(
        found.is_empty(),
        "forbidden crate-graph edges (EN-E10, SO-5):\n{}",
        found.join("\n")
    );
}

/// SO-5 over the resolved graph (RW5-4): `kaban-codegraph` is the only normal route from
/// `kaban-api` and `kaban-ingest` to `rusqlite`, and neither reaches the store crate. Negative
/// first: an invented graph with a second intermediary (`kaban-policy -> rusqlite`) and a normal
/// store edge under `kaban-ingest` names both, and the grant route alone is not a finding.
#[test]
fn kaban_codegraph_is_the_only_resolved_normal_route_from_the_api_to_rusqlite() {
    let graph = |edges: &[(&str, &str)]| {
        let mut g: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (from, to) in edges {
            g.entry((*from).to_owned())
                .or_default()
                .insert((*to).to_owned());
        }
        g
    };
    let grant = [
        ("kaban-api", "kaban-codegraph"),
        ("kaban-codegraph", "rusqlite"),
        ("kaban-ingest", "kaban-api"),
    ];
    assert_eq!(transitive_violations(&graph(&grant)), Vec::<String>::new());
    let mut leaky = grant.to_vec();
    leaky.extend([
        ("kaban-api", "kaban-policy"),
        ("kaban-policy", "rusqlite"),
        ("kaban-ingest", "kaban-store-sqlite"),
    ]);
    assert_eq!(
        transitive_violations(&graph(&leaky)),
        [
            "kaban-api reaches rusqlite without kaban-codegraph",
            "kaban-ingest reaches rusqlite without kaban-codegraph",
            "kaban-ingest reaches kaban-store-sqlite (normal)",
        ]
    );

    let resolved = resolved_normal_edges();
    assert!(
        reaches(&resolved, "kaban-api", "rusqlite", None),
        "kaban-api reaches rusqlite through normal edges (the SO-5 grant is real)"
    );
    let found = transitive_violations(&resolved);
    assert!(
        found.is_empty(),
        "a resolved normal route above the store seam (SO-5):\n{}",
        found.join("\n")
    );
}
