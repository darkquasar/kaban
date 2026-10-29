//! Decode-time gate and admission policy (stage1-plan CG3.1 slices; stage1-spec G1-S1, RS2-1,
//! RS3-1). Synthetic indexes are built in-test from `scip::types`; the committed Go fixture is
//! read where a real producer's output matters.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_index::INVENTORY;
use protobuf::MessageFull;
use protobuf::reflect::{MessageDescriptor, RuntimeFieldType, RuntimeType};
use std::collections::BTreeSet;

/// Every `string` / `repeated string` field reachable from `scip.Index`, as `Message.field`,
/// found by walking the protobuf descriptors (the schema itself, not our list).
fn schema_string_fields() -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut out = BTreeSet::new();
    let mut stack = vec![scip::types::Index::descriptor()];
    while let Some(md) = stack.pop() {
        if !seen.insert(md.full_name().to_owned()) {
            continue;
        }
        for f in md.fields() {
            let rt = match f.runtime_field_type() {
                RuntimeFieldType::Singular(rt) | RuntimeFieldType::Repeated(rt) => rt,
                RuntimeFieldType::Map(..) => panic!("map fields are not in the pinned schema"),
            };
            match rt {
                RuntimeType::String => {
                    out.insert(format!("{}.{}", md.name(), f.name()));
                }
                RuntimeType::Message(inner) => stack.push(inner),
                _ => {}
            }
        }
    }
    out
}

fn descriptor_of(name: &str) -> MessageDescriptor {
    scip::types::Index::descriptor()
        .file_descriptor()
        .message_by_package_relative_name(name)
        .unwrap_or_else(|| panic!("message {name} is not in the pinned schema"))
}

#[test]
fn inventory_matches_pinned_schema() {
    let listed: BTreeSet<String> = INVENTORY
        .iter()
        .map(|f| format!("{}.{}", f.message, f.field))
        .collect();
    assert_eq!(
        listed.len(),
        INVENTORY.len(),
        "INVENTORY lists a field twice"
    );
    let schema = schema_string_fields();
    let missing: Vec<_> = schema.difference(&listed).collect();
    let extra: Vec<_> = listed.difference(&schema).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "INVENTORY drifted from scip 0.10.0: missing {missing:?}, not in schema {extra:?}"
    );
    // every listed message name resolves in the schema file (guards a typo in `message`)
    for f in INVENTORY {
        let md = descriptor_of(f.message);
        assert!(
            md.fields().any(|d| d.name() == f.field),
            "{}.{} is not a field of the schema",
            f.message,
            f.field
        );
    }
}

// ---------------------------------------------------------------------------------------------
// synthetic indexes and the test policy

const CANARY: &str = "CANARY_SECRET_7f3a";

/// A policy with one credential-class rule (always rejects) on `CANARY_*` and one redact rule
/// on `REDACT_ME`; builtin rules off so the test controls every hit.
fn test_policy() -> kaban_policy::Policy {
    let yaml = b"
version: 1
defaults: { action: reject }
extend: { use_builtin: false }
rules:
  - { id: canary-cred, class: credential, regex: 'CANARY_[A-Za-z0-9_]+' }
  - { id: redact-me, class: generic, action: redact, regex: 'REDACT_ME' }
";
    let file = kaban_policy::Policy::parse(yaml).expect("test policy parses");
    kaban_policy::Policy::from_file(file, "test".into()).expect("test policy compiles")
}

fn sym_info(symbol: &str) -> scip::types::SymbolInformation {
    scip::types::SymbolInformation {
        symbol: symbol.to_owned(),
        ..Default::default()
    }
}

fn occurrence(symbol: &str, range: &[i32]) -> scip::types::Occurrence {
    scip::types::Occurrence {
        symbol: symbol.to_owned(),
        range: range.to_vec(),
        ..Default::default()
    }
}

fn relationship(target: &str) -> scip::types::Relationship {
    scip::types::Relationship {
        symbol: target.to_owned(),
        is_implementation: true,
        ..Default::default()
    }
}

fn document(
    path: &str,
    symbols: Vec<scip::types::SymbolInformation>,
    occurrences: Vec<scip::types::Occurrence>,
) -> scip::types::Document {
    scip::types::Document {
        language: "go".to_owned(),
        relative_path: path.to_owned(),
        symbols,
        occurrences,
        ..Default::default()
    }
}

fn index(documents: Vec<scip::types::Document>) -> scip::types::Index {
    let tool_info = scip::types::ToolInfo {
        name: "scip-go".to_owned(),
        version: "0.2.7".to_owned(),
        ..Default::default()
    };
    let metadata = scip::types::Metadata {
        tool_info: protobuf::MessageField::some(tool_info),
        project_root: "file:///repo".to_owned(),
        ..Default::default()
    };
    scip::types::Index {
        metadata: protobuf::MessageField::some(metadata),
        documents,
        ..Default::default()
    }
}

fn bytes(i: &scip::types::Index) -> Vec<u8> {
    protobuf::Message::write_to_bytes(i).expect("synthetic index serialises")
}

const PKG: &str = "scip-go gomod example.com/drivers v0 ";

// ---------------------------------------------------------------------------------------------
// gate order (RS2-1): the CG2.1 parser hook never sees an ungated value

/// RS2-1 through the whole public pipeline (RU1-2): decode and gate, admit, `Rows::derive`,
/// `publish`. The CG2.1 parser hook records every string `symbol::parse` saw; the list must be
/// non-empty (the clean symbol was parsed by derivation, so the hook is live) and no entry may
/// carry the canary (the gated symbol never reached the grammar).
#[test]
fn writer_gate_precedes_parse() {
    let canary_symbol = format!("{PKG}drivers/`{CANARY}`#");
    let clean_symbol = format!("{PKG}drivers/Conn#");
    let i = index(vec![document(
        "drivers/driver.go",
        vec![sym_info(&canary_symbol), sym_info(&clean_symbol)],
        vec![
            occurrence(&canary_symbol, &[1, 5, 12]),
            occurrence(&clean_symbol, &[2, 5, 9]),
        ],
    )]);
    let policy = test_policy();
    let recorder = kaban_codegraph::symbol::testing::record_inputs();
    let gated = kaban_index::decode_and_gate(&bytes(&i), &policy).expect("decodes");
    let admitted = kaban_index::gate::admit_index(&gated);
    assert!(
        recorder.inputs().is_empty(),
        "decode and admission never call the grammar"
    );
    let file = &admitted.files()[0];
    let symbols: Vec<&str> = file
        .symbols()
        .iter()
        .map(kaban_index::gate::AdmittedSymbol::symbol)
        .collect();
    assert_eq!(
        symbols,
        vec![clean_symbol.as_str()],
        "the owning row is skipped"
    );
    let occ: Vec<&str> = file
        .occurrences()
        .iter()
        .map(kaban_index::gate::AdmittedOccurrence::symbol)
        .collect();
    assert_eq!(
        occ,
        vec![clean_symbol.as_str()],
        "its occurrence goes with it"
    );
    assert_eq!(
        admitted.counts().policy_skipped_row,
        2,
        "the row and its occurrence"
    );
    assert_eq!(admitted.counts().policy_dropped, 0);
    assert_eq!(admitted.counts().policy_redacted_meta, 0);
    assert!(
        !format!("{admitted:?}").contains(CANARY),
        "no admitted value carries the canary"
    );

    // derivation and publication: the grammar runs now, over admitted values only
    let rows = kaban_index::writer::Rows::derive(&admitted, None, &policy);
    let dir = common::Scratch::new();
    let path = kaban_index::writer::publish(
        dir.path(),
        &rows,
        &common::meta(&common::commit(0x2a)),
        &policy,
    )
    .expect("publishes");
    let seen = recorder.inputs();
    assert!(
        !seen.is_empty(),
        "the parser hook recorded nothing: the test would prove nothing"
    );
    assert!(
        seen.iter().any(|s| s == &clean_symbol),
        "the clean symbol was parsed by derivation: {seen:?}"
    );
    for s in &seen {
        assert!(!s.contains(CANARY), "the parser hook saw an ungated value");
    }
    let file_bytes = std::fs::read(&path).expect("read snapshot");
    assert!(
        !file_bytes
            .windows(CANARY.len())
            .any(|w| w == CANARY.as_bytes()),
        "the canary is on no byte of the snapshot"
    );
    assert_eq!(
        rows.symbols().iter().filter(|r| r.name == "Conn").count(),
        1
    );
}

// ---------------------------------------------------------------------------------------------
// RS3-1: the fields the first inventory missed each skip the owning row

#[test]
fn enclosing_symbol_canary_skips_owning_row() {
    let member = format!("{PKG}drivers/Conn#Close().");
    let mut s = sym_info(&member);
    s.enclosing_symbol = format!("{PKG}drivers/`{CANARY}`#");
    let i = index(vec![document(
        "drivers/driver.go",
        vec![s, sym_info(&format!("{PKG}drivers/Conn#"))],
        vec![],
    )]);
    let gated = kaban_index::decode_and_gate(&bytes(&i), &test_policy()).expect("decodes");
    let admitted = kaban_index::admit_index(&gated);
    let symbols: Vec<&str> = admitted.files()[0]
        .symbols()
        .iter()
        .map(kaban_index::gate::AdmittedSymbol::symbol)
        .collect();
    assert_eq!(symbols.len(), 1);
    assert!(!symbols.contains(&member.as_str()));
    assert_eq!(admitted.counts().policy_skipped_row, 1);
    assert!(!format!("{admitted:?}").contains(CANARY));
}

#[test]
fn display_name_canary_skips_owning_row() {
    let member = format!("{PKG}drivers/Conn#Close().");
    let mut s = sym_info(&member);
    s.display_name = format!("Close {CANARY}");
    let i = index(vec![document(
        "drivers/driver.go",
        vec![s],
        vec![occurrence(&member, &[0, 0, 5])],
    )]);
    let gated = kaban_index::decode_and_gate(&bytes(&i), &test_policy()).expect("decodes");
    let admitted = kaban_index::admit_index(&gated);
    assert_eq!(admitted.files()[0].symbols().len(), 0, "the row is skipped");
    assert!(
        admitted.files()[0].occurrences().is_empty(),
        "the row's occurrence goes with it"
    );
    assert_eq!(admitted.counts().policy_skipped_row, 2);
    assert!(!format!("{admitted:?}").contains(CANARY));
}

// ---------------------------------------------------------------------------------------------
// meta: a hit in tool_info.arguments is masked and counted, never a refusal (DF-3); the
// admitted value and the rule ids carry no value

#[test]
fn writer_meta_hit_is_masked_and_counted() {
    let mut i = index(vec![document("drivers/driver.go", vec![], vec![])]);
    i.metadata
        .as_mut()
        .expect("metadata set")
        .tool_info
        .as_mut()
        .expect("tool_info set")
        .arguments = vec!["--skip-tests".to_owned(), format!("--token={CANARY}")];
    let gated = kaban_index::decode_and_gate(&bytes(&i), &test_policy()).expect("decodes");
    let admitted = kaban_index::admit_index(&gated);
    assert_eq!(
        admitted.metadata().tool_arguments(),
        ["--skip-tests", "[policy:canary-cred]"]
    );
    assert_eq!(admitted.counts().policy_redacted_meta, 1);
    assert_eq!(admitted.policy_redacted_meta_rules(), ["canary-cred"]);
    assert_eq!(
        admitted.counts().warnings(),
        [kaban_index::gate::WarningClass::PolicyRedactedMeta]
    );
    let text = format!("{admitted:?}");
    assert!(
        !text.contains(CANARY) && !text.contains("--token="),
        "no value anywhere: {text}"
    );
}

/// The real-world shape (DF-3 diagnosis): a `token=<digits>` argument hits the builtin
/// `weak-credential-assignment` rule; the value is masked with the rule id, the run goes on.
#[test]
fn builtin_weak_credential_in_tool_argument_is_masked_not_refused() {
    let policy = kaban_policy::Policy::empty();
    let mut i = index(vec![document("drivers/driver.go", vec![], vec![])]);
    i.metadata
        .as_mut()
        .expect("metadata set")
        .tool_info
        .as_mut()
        .expect("tool_info set")
        .arguments = vec!["--token=abc123-def456".to_owned()];
    let gated = kaban_index::decode_and_gate(&bytes(&i), &policy).expect("decodes");
    let admitted = kaban_index::admit_index(&gated);
    assert_eq!(
        admitted.metadata().tool_arguments(),
        ["[policy:weak-credential-assignment]"]
    );
    assert_eq!(admitted.counts().policy_redacted_meta, 1);
    assert_eq!(
        admitted.policy_redacted_meta_rules(),
        ["weak-credential-assignment"]
    );
}

// ---------------------------------------------------------------------------------------------
// free fields: redact keeps the masked value, reject drops the field

#[test]
fn writer_free_field_redact_drop() {
    let sym = format!("{PKG}drivers/DatabaseDriver#");
    let mut s = sym_info(&sym);
    s.documentation = vec![
        "DatabaseDriver opens connections.".to_owned(),
        "host REDACT_ME kept".to_owned(),
        format!("key {CANARY} dropped"),
    ];
    let mut sig = scip::types::Signature::new();
    sig.text = format!("type DatabaseDriver interface // {CANARY}");
    s.signature_documentation = protobuf::MessageField::some(sig);
    let i = index(vec![document("drivers/driver.go", vec![s], vec![])]);
    let gated = kaban_index::decode_and_gate(&bytes(&i), &test_policy()).expect("decodes");
    let admitted = kaban_index::admit_index(&gated);
    let row = &admitted.files()[0].symbols()[0];
    assert_eq!(row.symbol(), sym);
    assert_eq!(
        row.documentation(),
        vec![
            "DatabaseDriver opens connections.",
            "host [REDACTED:redact-me] kept"
        ]
    );
    assert_eq!(row.signature(), None, "a reject hit drops the signature");
    assert_eq!(
        admitted.counts().policy_dropped,
        2,
        "one doc element and the signature"
    );
    assert_eq!(admitted.counts().policy_skipped_row, 0);
    assert!(!format!("{admitted:?}").contains(CANARY));
}

// ---------------------------------------------------------------------------------------------
// relationships: a target that hit the gate is dropped, no placeholder appears

#[test]
fn writer_relationship_to_gated_target_dropped() {
    let iface_hit = format!("{PKG}drivers/`{CANARY}`#");
    let iface_ok = format!("{PKG}drivers/DatabaseDriver#");
    let impl_sym = format!("{PKG}drivers/PostgresDriver#");
    let mut implementer = sym_info(&impl_sym);
    implementer.relationships = vec![relationship(&iface_hit), relationship(&iface_ok)];
    // a clean symbol whose row is skipped through display_name: relationships to it drop too
    let skipped_by_name = format!("{PKG}drivers/MemDriver#");
    let mut by_name = sym_info(&skipped_by_name);
    by_name.display_name = CANARY.to_owned();
    let mut other = sym_info(&format!("{PKG}drivers/Wrapped#"));
    other.relationships = vec![relationship(&skipped_by_name)];
    let i = index(vec![document(
        "drivers/driver.go",
        vec![
            sym_info(&iface_hit),
            sym_info(&iface_ok),
            implementer,
            by_name,
            other,
        ],
        vec![],
    )]);
    let gated = kaban_index::decode_and_gate(&bytes(&i), &test_policy()).expect("decodes");
    let admitted = kaban_index::admit_index(&gated);
    let rows = admitted.files()[0].symbols();
    let symbols: Vec<&str> = rows
        .iter()
        .map(kaban_index::gate::AdmittedSymbol::symbol)
        .collect();
    assert_eq!(
        symbols,
        vec![
            iface_ok.as_str(),
            impl_sym.as_str(),
            "scip-go gomod example.com/drivers v0 drivers/Wrapped#"
        ]
    );
    let pg = rows
        .iter()
        .find(|s| s.symbol() == impl_sym)
        .expect("implementer survives");
    let targets: Vec<&str> = pg
        .relationships()
        .iter()
        .map(kaban_index::gate::AdmittedRelationship::target)
        .collect();
    assert_eq!(
        targets,
        vec![iface_ok.as_str()],
        "the relationship to the gated target is gone"
    );
    let wrapped = rows
        .iter()
        .find(|s| s.symbol().ends_with("Wrapped#"))
        .expect("survives");
    assert!(
        wrapped.relationships().is_empty(),
        "a relationship to a skipped row is gone"
    );
    assert!(
        admitted.external_symbols().is_empty(),
        "no placeholder for the gated target"
    );
    // skipped: iface_hit row, MemDriver row, two relationships
    assert_eq!(admitted.counts().policy_skipped_row, 4);
    assert!(!format!("{admitted:?}").contains(CANARY));
}

// ---------------------------------------------------------------------------------------------
// negative paths

#[test]
fn decode_failure_is_a_class_without_bytes() {
    let garbage = b"\xff\xff\xffnot a scip index CANARY_IN_BYTES";
    let err = kaban_index::decode_and_gate(garbage, &test_policy()).expect_err("not an index");
    assert_eq!(err, kaban_index::IndexError::Decode);
    assert_eq!(err.to_string(), "index decode failed");
}

#[test]
fn file_path_hit_skips_file_and_everything_in_it() {
    let sym = format!("{PKG}drivers/Conn#");
    let i = index(vec![
        document(
            &format!("drivers/{CANARY}.go"),
            vec![sym_info(&sym)],
            vec![occurrence(&sym, &[0, 0, 4])],
        ),
        document(
            "drivers/postgres.go",
            vec![],
            vec![occurrence(&sym, &[3, 0, 4])],
        ),
    ]);
    let gated = kaban_index::decode_and_gate(&bytes(&i), &test_policy()).expect("decodes");
    let admitted = kaban_index::admit_index(&gated);
    assert_eq!(admitted.files().len(), 1);
    assert_eq!(admitted.files()[0].relative_path(), "drivers/postgres.go");
    assert!(
        admitted.files()[0].occurrences().is_empty(),
        "the reference to a symbol of the skipped file is dropped"
    );
    assert_eq!(
        admitted.counts().policy_skipped_row,
        2,
        "the file row and the dangling occurrence"
    );
    assert!(!format!("{admitted:?}").contains(CANARY));
}

#[test]
fn warning_classes_serialise_as_snake_case() {
    let json =
        serde_json::to_string(&kaban_index::WarningClass::PolicySkippedRow).expect("serialises");
    assert_eq!(json, "\"policy_skipped_row\"");
}

// ---------------------------------------------------------------------------------------------
// the committed Go fixture (scip-go 0.2.7): decodes, gates, and every admitted symbol agrees
// with the reference parser (stage1-plan §3: differential test against `scip::symbol`)

fn go_fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/codegraph/go-drivers")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn go_fixture_decodes_gates_and_symbols_agree_with_reference_grammar() {
    for name in ["index.scip", "index-notest.scip"] {
        let gated = kaban_index::decode_and_gate(&go_fixture(name), &test_policy())
            .expect("fixture decodes");
        let admitted = kaban_index::admit_index(&gated);
        let paths: Vec<&str> = admitted
            .files()
            .iter()
            .map(kaban_index::gate::AdmittedFile::relative_path)
            .collect();
        assert!(paths.contains(&"drivers/driver.go"), "{name}: {paths:?}");
        assert!(
            paths.contains(&"drivers/memory.go"),
            "{name}: C2 has the renamed file"
        );
        assert_eq!(
            paths.contains(&"drivers/postgres_test.go"),
            name == "index.scip",
            "{name}"
        );
        assert_eq!(
            admitted.counts().policy_skipped_row,
            0,
            "{name}: no structural canary in the fixture"
        );
        assert!(
            admitted.counts().policy_dropped >= 1,
            "{name}: CANARY_DOC in a doc comment is dropped"
        );
        assert!(
            !format!("{admitted:?}").contains("CANARY_"),
            "{name}: no canary survives admission"
        );
        let mut checked = 0;
        for row in admitted.symbols() {
            let ours = kaban_codegraph::symbol::parse(row.symbol());
            let reference = scip::symbol::parse_symbol(row.symbol());
            assert_eq!(
                ours.is_ok(),
                reference.is_ok(),
                "{name}: grammar disagreement on a fixture symbol class"
            );
            if let Ok(sym) = &ours {
                assert_eq!(
                    kaban_codegraph::symbol::format(sym),
                    row.symbol(),
                    "{name}: round trip"
                );
            }
            checked += 1;
        }
        assert!(checked > 20, "{name}: {checked} symbols checked");
    }
}
