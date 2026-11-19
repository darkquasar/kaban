//! The EN-E2 dependency never-list and the SO-5 grant check (engine-plan E3.12; EN-S11 for
//! `reqwest`).
//!
//! Three sources of truth, none derived from the current lock: the never-list below, the
//! registry packages of `Cargo.lock` at the normative baseline `18abbd4`
//! (`fixtures/never_list/lock-18abbd4.txt`, extracted once) and the grant inventory of exact
//! `(name, version)` tuples per grant row (`fixtures/never_list/grants.txt`). The checks:
//! the default-feature graph of `kaban` has exactly one never-list package, `nom 7.1.3`, reached
//! only through `tantivy-query-grammar 0.26.0` (OWN-J, spec 05 SS-13b); the all-features
//! workspace graph has exactly two, that one and `nom 8.0.0`, reached only through the optional
//! `pdf_oxide 0.3.78` (OWN-116); the graph without default features has none; every registry
//! package in the current `Cargo.lock` is a baseline or granted tuple, so a new crate or a changed
//! pin fails. The checking functions are pure so the negatives run without cargo; the graph checks
//! shell out to `cargo tree --locked --offline` like `kaban-api/tests/crate_graph.rs` does to
//! `cargo metadata`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// EN-E2's never-list plus `reqwest` (EN-S11). A trailing `*` is a name prefix (`candle*`).
const NEVER: [&str; 27] = [
    "tiktoken-rs",
    "tokenizers",
    "comrak",
    "candle*",
    "ort",
    "gliner-rs",
    "gline-rs",
    "daachorse",
    "fst",
    "winnow",
    "nom",
    "vtext",
    "natural",
    "rust-bert",
    "tch",
    "linfa",
    "smartcore",
    "ndarray",
    "mistralrs",
    "kalosm",
    "model2vec-rs",
    "usearch",
    "hnsw_rs",
    "sqlite-vec",
    "jsonschema",
    "boon",
    "reqwest",
];

/// A permitted never-list tuple and the only package that may depend on it.
type Exception = ((&'static str, &'static str), (&'static str, &'static str));

/// `nom 7.1.3` under Tantivy's query grammar (OWN-J): in the default graph since spec 05 SS-13b.
const TANTIVY_NOM: Exception = (("nom", "7.1.3"), ("tantivy-query-grammar", "0.26.0"));

/// `nom 8.0.0` under the optional PDF backend (OWN-116): never in the default graph.
const PDF_NOM: Exception = (("nom", "8.0.0"), ("pdf_oxide", "0.3.78"));

/// Every permitted never-list tuple, sorted as [`never_hits`] sorts.
const NOM_EXCEPTIONS: [Exception; 2] = [TANTIVY_NOM, PDF_NOM];

/// Approved grant rows: the original deliveries plus the integration security fixes.
const GRANT_ROWS: [&str; 11] = [
    "DI1.2/OWN60",
    "CG3.1",
    "DI2.1/OWN69",
    "YAML/OWN86(7)",
    "TANTIVY/OWN-J",
    "ZSTD/OWN-I",
    "SECURITY/20261028",
    "SECURITY/20261029",
    "SECURITY/20261104",
    "MACOS/M1-M21",
    "QUERY/T1",
];

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

const BASELINE: &str = include_str!("fixtures/never_list/lock-18abbd4.txt");
const GRANTS: &str = include_str!("fixtures/never_list/grants.txt");

type Tuple = (String, String);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn is_never(name: &str) -> bool {
    NEVER.iter().any(|n| match n.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == *n,
    })
}

/// The never-list tuples among `packages`, sorted.
fn never_hits<'a>(packages: impl IntoIterator<Item = &'a Tuple>) -> Vec<Tuple> {
    let hits: BTreeSet<Tuple> = packages
        .into_iter()
        .filter(|(name, _)| is_never(name))
        .cloned()
        .collect();
    hits.into_iter().collect()
}

/// One `[[package]]` of a `Cargo.lock`: name, version and source (`None` for a path package).
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
}

/// The `[[package]]` entries of a `Cargo.lock` text (line-based: the lock writes one
/// `key = "value"` per line).
fn lock_packages(text: &str) -> Vec<LockPackage> {
    let mut out = Vec::new();
    for block in text.split("[[package]]").skip(1) {
        let field = |key: &str| {
            block.lines().find_map(|l| {
                l.strip_prefix(key)
                    .and_then(|r| r.strip_prefix(" = \""))
                    .and_then(|r| r.strip_suffix('"'))
                    .map(str::to_owned)
            })
        };
        out.push(LockPackage {
            name: field("name").unwrap(),
            version: field("version").unwrap(),
            source: field("source"),
        });
    }
    out
}

/// The baseline fixture: one `name version` per non-comment line.
fn baseline() -> BTreeSet<Tuple> {
    BASELINE
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (name, version) = l.split_once(' ').unwrap();
            (name.to_owned(), version.to_owned())
        })
        .collect()
}

/// The grant inventory: one `row name version` per non-comment line, `row` one of
/// [`GRANT_ROWS`].
fn grants() -> BTreeSet<Tuple> {
    GRANTS
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            assert_eq!(f.len(), 3, "malformed grant line: {l}");
            assert!(GRANT_ROWS.contains(&f[0]), "unknown grant row: {l}");
            (f[1].to_owned(), f[2].to_owned())
        })
        .collect()
}

/// Lock entries that are neither a baseline nor a granted crates.io tuple, as
/// `name version (source)`; a path package passes only as a `kaban*` workspace member.
fn ungranted(
    lock: &[LockPackage],
    baseline: &BTreeSet<Tuple>,
    grants: &BTreeSet<Tuple>,
) -> Vec<String> {
    lock.iter()
        .filter(|p| {
            let tuple = (p.name.clone(), p.version.clone());
            match p.source.as_deref() {
                None => !p.name.starts_with("kaban"),
                Some(CRATES_IO) => !baseline.contains(&tuple) && !grants.contains(&tuple),
                Some(_) => true,
            }
        })
        .map(|p| format!("{} {} ({:?})", p.name, p.version, p.source))
        .collect()
}

/// `cargo tree --locked --offline --target all` with `args`; panics with the output when cargo
/// fails (no graph evidence is never a pass).
fn cargo_tree(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO"))
        .args(["tree", "--locked", "--offline", "--target", "all"])
        .args(args)
        .current_dir(workspace_root())
        .output()
        .unwrap();
    assert!(out.status.success(), "cargo tree {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// `(depth, (name, version))` of each `cargo tree --prefix depth` line (`0nom v8.0.0`).
fn tree_lines(text: &str) -> Vec<(usize, Tuple)> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let digits = l.find(|c: char| !c.is_ascii_digit()).unwrap();
            let mut words = l[digits..].split_whitespace();
            let name = words.next().unwrap().to_owned();
            let version = words.next().unwrap().strip_prefix('v').unwrap().to_owned();
            (l[..digits].parse().unwrap(), (name, version))
        })
        .collect()
}

fn graph_packages(args: &[&str]) -> BTreeSet<Tuple> {
    let mut all = vec!["--prefix", "depth"];
    all.extend_from_slice(args);
    tree_lines(&cargo_tree(&all))
        .into_iter()
        .map(|(_, t)| t)
        .collect()
}

/// The inverse tree of `package` (`cargo tree -i name@version`) under `edges`, parsed.
fn inverse_tree(package: &Tuple, edges: &[&str]) -> Vec<(usize, Tuple)> {
    let spec = format!("{}@{}", package.0, package.1);
    let mut args = vec!["--prefix", "depth", "-i", spec.as_str()];
    args.extend_from_slice(edges);
    tree_lines(&cargo_tree(&args))
}

/// The depth-1 dependents of `package` in its parsed `inverse` tree other than `parent`, after
/// checking the tree's root is `package` itself: any entry is a second path to a never-list
/// package.
fn stray_parents(package: &Tuple, parent: &Tuple, inverse: &[(usize, Tuple)]) -> Vec<Tuple> {
    let at = |depth| -> BTreeSet<Tuple> {
        inverse
            .iter()
            .filter(|(d, _)| *d == depth)
            .map(|(_, t)| t.clone())
            .collect()
    };
    assert_eq!(at(0), BTreeSet::from([package.clone()]), "{inverse:?}");
    at(1).into_iter().filter(|p| p != parent).collect()
}

fn tuple(name: &str, version: &str) -> Tuple {
    (name.to_owned(), version.to_owned())
}

/// `(package, permitted parent)` of an [`Exception`] as tuples.
fn exception(((name, version), (parent, parent_version)): Exception) -> (Tuple, Tuple) {
    (tuple(name, version), tuple(parent, parent_version))
}

/// The permitted never-list tuples, sorted.
fn exception_packages() -> Vec<Tuple> {
    NOM_EXCEPTIONS.iter().map(|e| exception(*e).0).collect()
}

/// The shipped binary's default graph (normal and build edges, every target) carries the Tantivy
/// adapter and exactly one never-list package, `nom 7.1.3`, whose only dependent is
/// `tantivy-query-grammar 0.26.0` (OWN-J); the PDF backend and its `nom` stay out.
#[test]
fn default_graph_admits_only_nom_under_tantivy_query_grammar() {
    let edges = ["-p", "kaban", "-e", "normal,build"];
    let packages = graph_packages(&edges);
    for member in ["kaban-ingest", "kaban-index-tantivy"] {
        assert!(
            packages.iter().any(|(n, _)| n == member),
            "{member} missing: {packages:?}"
        );
    }
    let (nom, parent) = exception(TANTIVY_NOM);
    assert_eq!(never_hits(&packages), vec![nom.clone()]);
    assert_eq!(
        stray_parents(&nom, &parent, &inverse_tree(&nom, &edges)),
        Vec::<Tuple>::new()
    );
    let (_, pdf_oxide) = exception(PDF_NOM);
    assert!(
        !packages.contains(&pdf_oxide),
        "pdf_oxide in the default graph"
    );
}

/// Without default features the graph still carries the Tantivy adapter (spec 05 OWN-M: the
/// store owns its text index in every build) and, with it, exactly the one never-list package
/// of the default graph under its one permitted dependent; nothing else.
#[test]
fn no_default_graph_carries_the_adapter_and_only_its_nom() {
    let edges = ["-p", "kaban", "--no-default-features", "-e", "normal,build"];
    let packages = graph_packages(&edges);
    assert!(
        packages.iter().any(|(n, _)| n == "kaban-index-tantivy"),
        "{packages:?}"
    );
    let (nom, parent) = exception(TANTIVY_NOM);
    assert_eq!(never_hits(&packages), vec![nom.clone()]);
    assert_eq!(
        stray_parents(&nom, &parent, &inverse_tree(&nom, &edges)),
        Vec::<Tuple>::new()
    );
}

/// With every feature of every workspace member (dev edges included), the only never-list
/// packages are the two `nom` exceptions, each with its one permitted dependent (OWN-116, OWN-J):
/// no direct use, no second path.
#[test]
fn all_features_graph_admits_only_the_nom_exceptions() {
    let edges = ["--workspace", "--all-features", "-e", "normal,build,dev"];
    let packages = graph_packages(&edges);
    assert_eq!(never_hits(&packages), exception_packages());
    for (nom, parent) in NOM_EXCEPTIONS.map(exception) {
        assert_eq!(
            stray_parents(&nom, &parent, &inverse_tree(&nom, &edges)),
            Vec::<Tuple>::new(),
            "{nom:?}"
        );
    }
}

/// Every package in the current `Cargo.lock` is a baseline tuple or a granted tuple (SO-5); the
/// whole lock (every target, every feature) carries no never-list tuple but the nom exceptions.
#[test]
fn lock_names_only_baseline_or_granted_packages() {
    let text = std::fs::read_to_string(workspace_root().join("Cargo.lock")).unwrap();
    let lock = lock_packages(&text);
    let (base, granted) = (baseline(), grants());
    assert_eq!(base.len(), 213, "the 18abbd4 fixture changed");
    assert!(
        base.is_disjoint(&granted),
        "a grant repeats a baseline tuple"
    );
    assert_eq!(ungranted(&lock, &base, &granted), Vec::<String>::new());

    let tuples: Vec<Tuple> = lock
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    assert_eq!(never_hits(&tuples), exception_packages());
}

/// The pure checks reject what they must: a never-list package (exact and prefix), an ungranted
/// crate, a changed pin of a baseline crate, a non-crates.io source, and a permitted `nom` under
/// any parent but its own.
#[test]
fn checks_reject_injected_packages() {
    let injected = [tuple("candle-core", "0.9.0"), tuple("winnow", "0.7.0")];
    let mut packages = vec![tuple("serde", "1.0.0")];
    packages.extend(injected.iter().cloned());
    assert_eq!(never_hits(&packages), injected.to_vec());

    let (base, granted) = (baseline(), grants());
    let (name, version) = base.iter().next().unwrap().clone();
    let pkg = |name: &str, version: &str, source: Option<&str>| LockPackage {
        name: name.to_owned(),
        version: version.to_owned(),
        source: source.map(str::to_owned),
    };
    let lock = [
        pkg(&name, &version, Some(CRATES_IO)),
        pkg("kaban-core", "3.0.0", None),
        pkg("left-pad", "1.0.0", Some(CRATES_IO)),
        pkg(&name, &format!("{version}-changed"), Some(CRATES_IO)),
        pkg(&name, &version, Some("git+https://example.invalid/x")),
        pkg("vendored", "0.1.0", None),
    ];
    let bad = ungranted(&lock, &base, &granted);
    assert_eq!(bad.len(), 4, "{bad:?}");
    assert!(bad[0].starts_with("left-pad 1.0.0"), "{bad:?}");

    // `nom 7.1.3` with a third dependent beside its permitted parent is a stray; and the other
    // exception's parent does not admit it
    let (nom, parent) = exception(TANTIVY_NOM);
    let inverse = tree_lines(
        "0nom v7.1.3\n1tantivy-query-grammar v0.26.0\n2tantivy v0.26.2\n1kaban-core v3.0.0 (/x)\n",
    );
    assert_eq!(
        stray_parents(&nom, &parent, &inverse),
        vec![tuple("kaban-core", "3.0.0")]
    );
    let (_, pdf_oxide) = exception(PDF_NOM);
    assert_eq!(stray_parents(&nom, &pdf_oxide, &inverse[..3]), vec![parent]);
}

/// `ingest_claim` is in the production tool registry (not reserved), so its handler's crates
/// are part of the graphs checked above (engine-plan E3.12 "include `ingest_claim`").
#[test]
fn ingest_claim_is_a_registered_tool() {
    assert!(
        kaban_api::control::tool_schemas()
            .iter()
            .any(|t| t.tool == "ingest_claim")
    );
    assert!(!kaban_api::control::RESERVED.contains(&"ingest_claim"));
}
