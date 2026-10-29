//! The crate-graph and source-shape slices of CG-JOIN v1 §2 (SO-3) and §8 (engine-plan E2.6):
//! `crate_graph_acyclic` over `cargo metadata`, `attach_built_only_from_resolved` over the
//! production sources, `cg_reads_stable_columns_only` over the store's SQL strings.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The `src/` trees of every workspace crate.
fn production_sources() -> Vec<PathBuf> {
    let crates = workspace_root().join("crates");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&crates).unwrap().filter_map(Result::ok) {
        let src = entry.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut out);
        }
    }
    out.sort();
    out
}

/// `(name, kind)` of every dependency edge of each workspace member; `kind` is `normal`,
/// `dev` or `build`.
fn member_edges() -> BTreeMap<String, Vec<(String, String)>> {
    let out = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .current_dir(workspace_root())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let meta: Value = serde_json::from_slice(&out.stdout).unwrap();
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

/// One DFS step of the acyclicity check (white 0, grey 1, black 2).
fn visit<'a>(
    node: &'a str,
    normal: &BTreeMap<&'a str, Vec<&'a str>>,
    state: &mut BTreeMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
) {
    match state.get(node) {
        Some(2) => return,
        Some(1) => panic!("cycle through {node}: {stack:?}"),
        _ => {}
    }
    state.insert(node, 1);
    stack.push(node);
    for dep in &normal[node] {
        visit(dep, normal, state, stack);
    }
    stack.pop();
    state.insert(node, 2);
}

/// Contract §2 (SO-3): `kaban-codegraph` depends on `kaban-core` and utility crates only, never
/// on `kaban-policy`, `kaban-store-sqlite` or `kaban-api`; `kaban-core` has no edge of any kind
/// to `kaban-codegraph`; `kaban-store-sqlite -> kaban-codegraph` is a dev edge only;
/// `kaban-api -> kaban-codegraph` is normal; the normal graph over the members is acyclic.
#[test]
fn crate_graph_acyclic() {
    let edges = member_edges();
    let members: BTreeSet<&str> = edges.keys().map(String::as_str).collect();
    assert!(members.contains("kaban-codegraph"), "{members:?}");

    let codegraph_normal: BTreeSet<&str> = edges["kaban-codegraph"]
        .iter()
        .filter(|(_, kind)| kind == "normal")
        .map(|(name, _)| name.as_str())
        .collect();
    let allowed: BTreeSet<&str> = [
        "kaban-core",
        "rusqlite",
        "hmac",
        "sha2",
        "blake3",
        "serde",
        "serde_json",
        "thiserror",
        "hex",
        "zeroize",
        "tracing",
        "rustix",
    ]
    .into_iter()
    .collect();
    assert!(
        codegraph_normal.is_subset(&allowed),
        "kaban-codegraph normal deps outside the allowlist: {:?}",
        codegraph_normal.difference(&allowed).collect::<Vec<_>>()
    );
    assert!(codegraph_normal.contains("kaban-core"));
    for (name, kind) in &edges["kaban-codegraph"] {
        assert!(
            !matches!(
                name.as_str(),
                "kaban-policy" | "kaban-store-sqlite" | "kaban-api"
            ),
            "forbidden edge kaban-codegraph -> {name} ({kind})"
        );
    }
    assert!(
        !edges["kaban-core"]
            .iter()
            .any(|(name, _)| name == "kaban-codegraph"),
        "kaban-core must not depend on kaban-codegraph in any way"
    );
    let store_to_codegraph: Vec<&str> = edges["kaban-store-sqlite"]
        .iter()
        .filter(|(name, _)| name == "kaban-codegraph")
        .map(|(_, kind)| kind.as_str())
        .collect();
    assert_eq!(
        store_to_codegraph,
        vec!["dev"],
        "store -> codegraph is dev-only"
    );
    assert!(
        edges["kaban-api"]
            .iter()
            .any(|(name, kind)| name == "kaban-codegraph" && kind == "normal"),
        "kaban-api -> kaban-codegraph is a normal edge"
    );

    // the normal graph among members is acyclic (DFS with colours)
    let normal: BTreeMap<&str, Vec<&str>> = edges
        .iter()
        .map(|(name, deps)| {
            (
                name.as_str(),
                deps.iter()
                    .filter(|(dep, kind)| kind == "normal" && members.contains(dep.as_str()))
                    .map(|(dep, _)| dep.as_str())
                    .collect(),
            )
        })
        .collect();
    let mut state = BTreeMap::new();
    for node in normal.keys() {
        visit(node, &normal, &mut state, &mut Vec::new());
    }
}

/// Contract §2 and spec 05 SS-25: a `SnapshotAttach` is built in exactly one production file
/// outside `#[cfg(test)]` modules, `crates/kaban-api/src/snapshot.rs`, through
/// `SnapshotAttach::from_admitted`; no other production file spells the constructor or the
/// literal `SnapshotAttach {` (the definition and the `impl` in `kaban-core` do not count,
/// nor do comments).
#[test]
fn attach_built_only_from_resolved() {
    let root = workspace_root();
    let mut hits = BTreeSet::new();
    for path in production_sources() {
        let text = std::fs::read_to_string(&path).unwrap();
        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
        if production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| {
                l.contains("SnapshotAttach::from_admitted(")
                    || (l.contains("SnapshotAttach {")
                        && !l.contains("struct SnapshotAttach")
                        && !l.contains("impl SnapshotAttach"))
            })
        {
            hits.insert(
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    assert_eq!(
        hits.into_iter().collect::<Vec<_>>(),
        vec!["crates/kaban-api/src/snapshot.rs".to_owned()]
    );
}

/// Spec 05 SS-25: the sealed `AttachHandle` is wrapped in exactly one production file
/// (`kaban-api/src/snapshot.rs`, beside the one `SnapshotAttach` literal) and read in exactly
/// one (the SQLite adapter's `ATTACH DATABASE ?1`, `kaban-store-sqlite/src/connection.rs`),
/// outside `#[cfg(test)]` modules; the definitions in `kaban-core` do not count. Core carries no
/// SQLite URI syntax.
#[test]
fn attach_handle_wrapped_and_read_once() {
    let root = workspace_root();
    let sites = |needle: &str, definition: &str| {
        let mut hits = BTreeSet::new();
        for path in production_sources() {
            let text = std::fs::read_to_string(&path).unwrap();
            let production = text.split("#[cfg(test)]").next().unwrap_or_default();
            if production
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains(needle) && !l.contains(definition))
            {
                hits.insert(
                    path.strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        hits.into_iter().collect::<Vec<_>>()
    };
    assert_eq!(
        sites("from_admitted(", "pub fn from_admitted("),
        vec!["crates/kaban-api/src/snapshot.rs".to_owned()]
    );
    assert_eq!(
        sites("admitted_value()", "pub fn admitted_value("),
        vec!["crates/kaban-store-sqlite/src/connection.rs".to_owned()]
    );
    let mut core = Vec::new();
    rust_files(&root.join("crates/kaban-core/src"), &mut core);
    for path in core {
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("sql_param") && !text.contains("mode=ro"),
            "{} names SQLite URI syntax",
            path.display()
        );
    }
}

/// Contract §8: no engine SQL string reads a `cg` column outside the stable list (`snippets`,
/// `edge_types`, `enc_*`, `signature`, `doc` and `lines` are v4-internal).
#[test]
fn cg_reads_stable_columns_only() {
    let store_src = workspace_root().join("crates/kaban-store-sqlite/src");
    let mut files = Vec::new();
    rust_files(&store_src, &mut files);
    let unstable = [
        "snippets",
        "edge_types",
        "enc_",
        "signature",
        "doc",
        "lines",
    ];
    let mut offenders = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        for (i, line) in text.lines().enumerate() {
            for (pos, _) in line.match_indices("cg.") {
                let rest = &line[pos + 3..];
                if unstable.iter().any(|u| rest.starts_with(u)) {
                    offenders.push(format!("{}:{}: {line}", path.display(), i + 1));
                }
            }
        }
    }
    assert_eq!(offenders, Vec::<String>::new());
}
