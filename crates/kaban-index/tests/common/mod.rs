//! Shared test support for the `kaban-index` writer tests (stage1-plan CG3.2..CG3.4): the test
//! policy with the `CANARY_*` credential rule, the committed fixture bytes, synthetic index
//! builders over `scip::types`, an RAII scratch directory (test-fixture-raii) and the one-call
//! pipeline from bytes to a published snapshot.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)] // shared test support, included by `#[path]`

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use kaban_index::gate::Admitted;
use kaban_index::writer::{self, Meta, Provenance, Rows};
use kaban_index::{GatedIndex, admit_index, decode_and_gate};
use kaban_policy::Policy;
use protobuf::Message;

/// The §8.1 canary prefix: `CANARY_DOC`, `CANARY_SNIPPET`, `CANARY_SYM` all match the rule.
pub const CANARY_RULE: &str = "canary-cred";

/// A policy with one credential-class rule (always rejects) on `CANARY_*`, one redact rule on
/// `REDACT_ME`, and the formed-string cases of G1-S1 step 3 (`joined-canary` for `ident_terms`
/// and the `indexer:<lang>` join, `joined-doc` for a documentation join across the blank line,
/// `file-ref` and `file-name` for a file node's `ref` and `name`, RU1-1); builtin rules off so
/// the test controls every hit.
pub fn test_policy() -> Policy {
    let yaml = b"
version: 1
defaults: { action: reject }
extend: { use_builtin: false }
rules:
  - { id: canary-cred, class: credential, regex: 'CANARY_[A-Za-z0-9_]+' }
  - { id: redact-me, class: generic, action: redact, regex: 'REDACT_ME' }
  - { id: joined-canary, class: credential, regex: 'canary joined' }
  - { id: joined-doc, class: credential, regex: 'ALPHA_HALF\\n+BETA_HALF' }
  - { id: file-ref, class: credential, regex: 'file:drivers/formed[.]go' }
  - { id: file-name, class: credential, regex: '^basename-only[.]go$' }
";
    let file = Policy::parse(yaml).expect("test policy parses");
    Policy::from_file(file, "test".into()).expect("test policy compiles")
}

/// `fixtures/codegraph/` of this repository.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codegraph")
}

/// The committed bytes of `fixtures/codegraph/<rel>`.
pub fn fixture_bytes(rel: &str) -> Vec<u8> {
    let path = fixtures_dir().join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Copy unchanged trusted qualification inputs into an explicit execution scratch.
pub fn copy_trusted_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(!kind.is_symlink());
        if kind.is_dir() {
            copy_trusted_tree(&entry.path(), &target.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}

/// A deterministic 40-hex commit for tests: `n` repeated into the width.
pub fn commit(n: u8) -> String {
    format!("{n:02x}").repeat(20)
}

/// The go-drivers C2 commit the committed index was produced from (`fixtures/codegraph/VERSIONS`).
pub const GO_DRIVERS_COMMIT: &str = "730da842221a844e8c2c82891dda9094f6428858";

// ---------------------------------------------------------------------------------------------
// scratch directories (test-fixture-raii, std only: no `tempfile` dependency in this crate)

static SCRATCH_SEQ: AtomicU32 = AtomicU32::new(0);

/// A fresh 0700 directory under the system temp dir, removed with everything in it on drop.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub fn new() -> Self {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kaban-index-{}-{seq}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).expect("scratch directory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------------------------
// synthetic indexes

pub fn sym_info(symbol: &str) -> scip::types::SymbolInformation {
    scip::types::SymbolInformation {
        symbol: symbol.to_owned(),
        ..Default::default()
    }
}

pub fn sym_info_kind(
    symbol: &str,
    kind: scip::types::symbol_information::Kind,
) -> scip::types::SymbolInformation {
    scip::types::SymbolInformation {
        symbol: symbol.to_owned(),
        kind: kind.into(),
        ..Default::default()
    }
}

pub fn occurrence(symbol: &str, range: &[i32]) -> scip::types::Occurrence {
    scip::types::Occurrence {
        symbol: symbol.to_owned(),
        range: range.to_vec(),
        ..Default::default()
    }
}

pub fn definition(symbol: &str, range: &[i32]) -> scip::types::Occurrence {
    scip::types::Occurrence {
        symbol: symbol.to_owned(),
        range: range.to_vec(),
        symbol_roles: 1,
        ..Default::default()
    }
}

pub fn definition_enclosing(
    symbol: &str,
    range: &[i32],
    enclosing: &[i32],
) -> scip::types::Occurrence {
    scip::types::Occurrence {
        symbol: symbol.to_owned(),
        range: range.to_vec(),
        symbol_roles: 1,
        enclosing_range: enclosing.to_vec(),
        ..Default::default()
    }
}

pub fn implementation(target: &str) -> scip::types::Relationship {
    scip::types::Relationship {
        symbol: target.to_owned(),
        is_implementation: true,
        ..Default::default()
    }
}

pub fn document(
    path: &str,
    symbols: Vec<scip::types::SymbolInformation>,
    occurrences: Vec<scip::types::Occurrence>,
) -> scip::types::Document {
    scip::types::Document {
        relative_path: path.to_owned(),
        language: "go".to_owned(),
        symbols,
        occurrences,
        ..Default::default()
    }
}

pub fn index(documents: Vec<scip::types::Document>) -> scip::types::Index {
    let tool_info = scip::types::ToolInfo {
        name: "synthetic".to_owned(),
        version: "0".to_owned(),
        ..Default::default()
    };
    let metadata = scip::types::Metadata {
        project_root: "file:///repo".to_owned(),
        tool_info: protobuf::MessageField::some(tool_info),
        ..Default::default()
    };
    scip::types::Index {
        metadata: protobuf::MessageField::some(metadata),
        documents,
        ..Default::default()
    }
}

pub fn bytes(i: &scip::types::Index) -> Vec<u8> {
    i.write_to_bytes().expect("serialise synthetic index")
}

/// A go-drivers style global symbol under the `example.com/drivers` package at version `v`.
pub fn go_sym(v: &str, descriptors: &str) -> String {
    format!("scip-go gomod example.com/drivers {v} `example.com/drivers/drivers`/{descriptors}")
}

// ---------------------------------------------------------------------------------------------
// the pipeline

/// Decode and gate `bytes` under the test policy.
pub fn gated(bytes: &[u8], policy: &Policy) -> GatedIndex {
    decode_and_gate(bytes, policy).expect("decode and gate")
}

/// A `Meta` with fixed provenance values for tests.
pub fn meta(commit: &str) -> Meta {
    Meta {
        commit: writer::Commit::parse(commit).expect("40-hex commit"),
        repo_id: "0123456789abcdef0123456789abcdef".to_owned(),
        policy_version: "test".to_owned(),
        provenance: Provenance {
            isolation: kaban_codegraph::IsolationTier::None,
            isolation_detail: "test".to_owned(),
            resources: Vec::new(),
            container_image_digest: None,
        },
    }
}

/// Derive the rows of a snapshot from admitted run A (and Go run B) under `policy`.
pub fn rows<'a>(a: &Admitted<'a>, b: Option<&Admitted<'a>>, policy: &Policy) -> Rows {
    Rows::derive(a, b, policy)
}

/// Decode, gate, admit, derive and publish `<dir>/<commit>.cg.db` from the index bytes.
pub fn publish(
    dir: &Path,
    commit: &str,
    a_bytes: &[u8],
    b_bytes: Option<&[u8]>,
    policy: &Policy,
) -> PathBuf {
    let a = gated(a_bytes, policy);
    let b = b_bytes.map(|b| gated(b, policy));
    let admitted_a = admit_index(&a);
    let admitted_b = b.as_ref().map(|b| admit_index(b));
    let rows = Rows::derive(&admitted_a, admitted_b.as_ref(), policy);
    writer::publish(dir, &rows, &meta(commit), policy).expect("publish")
}

/// The published file's bytes.
pub fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read snapshot")
}

// ---------------------------------------------------------------------------------------------
// the committed synthetic indexes (`fixtures/codegraph/synthetic/*.scip`, written from code by
// the `#[ignore] regen_synthetic_fixtures` test of `tests/writer.rs`)

use scip::types::symbol_information::Kind;

/// Every committed synthetic index by file stem, built from code. A test reads the committed
/// bytes through [`synthetic_bytes`], which also asserts they still equal this builder's output.
pub fn synthetic_index(stem: &str) -> scip::types::Index {
    let v = GO_DRIVERS_COMMIT;
    match stem {
        // G1-F1 `writer_duplicates_counted`: one symbol declared twice in one document
        "duplicate" => {
            let dup = go_sym(v, "Dup#");
            index(vec![document(
                "drivers/dup.go",
                vec![
                    sym_info_kind(&dup, Kind::Struct),
                    sym_info_kind(&dup, Kind::Interface),
                ],
                vec![definition(&dup, &[1, 5, 8])],
            )])
        }
        // G1-F1 `writer_grammar_failure_skips_row`: one malformed symbol (unbalanced backtick)
        // with a definition, a reference and a relationship towards a well-formed symbol
        "malformed" => {
            let good = go_sym(v, "Good#");
            let bad = "scip-go gomod example.com/drivers 1.0.0 `unterminated/Bad#";
            let mut info = sym_info_kind(bad, Kind::Struct);
            info.relationships.push(implementation(&good));
            index(vec![document(
                "drivers/bad.go",
                vec![sym_info_kind(&good, Kind::Interface), info],
                vec![
                    definition(&good, &[1, 5, 9]),
                    definition(bad, &[3, 5, 8]),
                    occurrence(bad, &[5, 1, 4]),
                ],
            )])
        }
        // G1-F2 `writer_signature_doc_bounds`: a 5 KiB doc and a 2,000-byte signature
        "big-doc" => {
            let sym = go_sym(v, "Big#");
            let mut info = sym_info_kind(&sym, Kind::Struct);
            info.documentation = vec!["x".repeat(5 * 1024)];
            info.signature_documentation = protobuf::MessageField::some(scip::types::Signature {
                text: "s".repeat(2000),
                ..Default::default()
            });
            index(vec![document(
                "drivers/big.go",
                vec![info],
                vec![definition(&sym, &[1, 5, 8])],
            )])
        }
        // `[EN-amend]` range forms: F encloses a reference to G; the forms differ per file
        "typed-only" => range_form_index(RangeStyle::Typed, RangeStyle::Typed),
        "legacy-only" => range_form_index(RangeStyle::Legacy, RangeStyle::Legacy),
        "mixed" => range_form_index(RangeStyle::Typed, RangeStyle::Legacy),
        // §8.3 step 7 `CANARY_SYM`: a fixture variant with the canary as an identifier (a struct
        // and its method), beside a clean symbol
        "canary-sym" => {
            let plain = go_sym(v, "Plain#");
            let sym = go_sym(v, "CANARY_SYM#");
            let method = go_sym(v, "CANARY_SYM#Open().");
            index(vec![document(
                "drivers/variant.go",
                vec![
                    sym_info_kind(&plain, Kind::Struct),
                    sym_info_kind(&sym, Kind::Struct),
                    sym_info_kind(&method, Kind::Method),
                ],
                vec![
                    definition(&plain, &[1, 5, 10]),
                    definition(&sym, &[3, 5, 15]),
                    definition_enclosing(&method, &[5, 20, 24], &[5, 0, 7, 1]),
                    occurrence(&plain, &[6, 2, 7]),
                ],
            )])
        }
        other => panic!("no synthetic index named {other}"),
    }
}

/// How a synthetic occurrence carries its ranges.
#[derive(Clone, Copy)]
pub enum RangeStyle {
    /// `typed_range` / `typed_enclosing_range` only.
    Typed,
    /// `range` / `enclosing_range` only.
    Legacy,
}

pub fn styled(
    symbol: &str,
    roles: i32,
    r: [i32; 4],
    enc: Option<[i32; 4]>,
    style: RangeStyle,
) -> scip::types::Occurrence {
    let mut o = scip::types::Occurrence {
        symbol: symbol.to_owned(),
        symbol_roles: roles,
        ..Default::default()
    };
    match style {
        RangeStyle::Legacy => {
            o.range = r.to_vec();
            if let Some(e) = enc {
                o.enclosing_range = e.to_vec();
            }
        }
        RangeStyle::Typed => {
            o.set_multi_line_range(scip::types::MultiLineRange {
                start_line: r[0],
                start_character: r[1],
                end_line: r[2],
                end_character: r[3],
                ..Default::default()
            });
            if let Some(e) = enc {
                o.set_multi_line_enclosing_range(scip::types::MultiLineRange {
                    start_line: e[0],
                    start_character: e[1],
                    end_line: e[2],
                    end_character: e[3],
                    ..Default::default()
                });
            }
        }
    }
    o
}

/// Two documents: `F()` whose enclosing range holds a reference to `G()` (definition and
/// enclosing in `def_style`), and the reference site itself in `site_style`.
fn range_form_index(def_style: RangeStyle, site_style: RangeStyle) -> scip::types::Index {
    let v = GO_DRIVERS_COMMIT;
    let (f, g) = (go_sym(v, "F()."), go_sym(v, "G()."));
    index(vec![document(
        "drivers/forms.go",
        vec![
            sym_info_kind(&f, Kind::Function),
            sym_info_kind(&g, Kind::Function),
        ],
        vec![
            styled(&f, 1, [2, 5, 2, 6], Some([2, 0, 5, 1]), def_style),
            styled(&g, 1, [8, 5, 8, 6], Some([8, 0, 9, 1]), def_style),
            styled(&g, 0, [3, 2, 3, 3], None, site_style),
        ],
    )])
}

/// The stems [`synthetic_index`] knows.
pub const SYNTHETIC_STEMS: &[&str] = &[
    "duplicate",
    "malformed",
    "big-doc",
    "typed-only",
    "legacy-only",
    "mixed",
    "canary-sym",
];

/// `examples/policy-test-canaries.yaml` (the §8.1 canaries as credential rules).
pub fn canary_policy() -> Policy {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-test-canaries.yaml");
    Policy::load(&path).expect("the canary policy loads")
}

/// The committed bytes of `fixtures/codegraph/synthetic/<stem>.scip`, asserted equal to the
/// builder's output (run `cargo test -p kaban-index --test writer -- --ignored regen` after
/// changing a builder).
pub fn synthetic_bytes(stem: &str) -> Vec<u8> {
    let built = bytes(&synthetic_index(stem));
    let path = fixtures_dir()
        .join("synthetic")
        .join(format!("{stem}.scip"));
    let committed = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{} missing ({e}); run the ignored regen test",
            path.display()
        )
    });
    assert_eq!(
        committed,
        built,
        "{} drifted from its builder; run the ignored regen test",
        path.display()
    );
    committed
}
