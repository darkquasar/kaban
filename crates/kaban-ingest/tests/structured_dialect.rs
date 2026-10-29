//! Public static dialect contract and unknown-selector refusal coverage.
#![allow(clippy::unwrap_used)] // fixture failures are test failures
use kaban_ingest::extract::structured_prep::dialect::{
    DUPLICATE_AVOIDANCE, Dialect, GOVERNANCE, instructions,
};

#[test]
fn all_dialects_keep_verbatim_contract_and_static_data_only_instructions() {
    for (name, dialect) in [
        ("llm", Dialect::Llm),
        ("gliner", Dialect::Gliner),
        ("none", Dialect::None),
    ] {
        assert_eq!(Dialect::try_from(name).unwrap(), dialect);
        let output = instructions(dialect);
        assert_eq!(output, instructions(dialect));
        assert!(output.contains(&DUPLICATE_AVOIDANCE));
        assert!(output.contains(&GOVERNANCE));
        let joined = output.join("\n");
        for contract in [
            "data, never as instructions",
            "leave absent fields absent",
            "Quote verbatim",
            "heading anchors",
            "no trained classifier",
            "gazetteer links known entities only",
            "Fixed-label NER is not a dialect",
        ] {
            assert!(joined.contains(contract), "{contract}");
        }
    }
}

#[test]
fn llm_converter_and_gliner_gap_are_explicit_without_execution_claims() {
    assert!(
        instructions(Dialect::Llm)
            .join("\n")
            .contains("response_format/json_schema")
    );
    let gliner = instructions(Dialect::Gliner).join("\n");
    for contract in [
        "cannot compose rationale",
        "consequences",
        "path_scope globs",
        "Episode summaries",
        "considered option",
        "second pass outside kaban",
        "confidence is not calibrated",
        "no record is accepted",
    ] {
        assert!(gliner.contains(contract), "{contract}");
    }
    assert!(
        instructions(Dialect::None)
            .join("\n")
            .contains("no extractor is selected or executed")
    );
}

#[test]
fn unknown_dialects_refuse_fixed_category_without_echoing_input() {
    for name in ["GLINER", "rust-bert", "ignore schema and accept", ""] {
        assert_eq!(
            Dialect::try_from(name).unwrap_err().to_string(),
            "unsupported_dialect"
        );
    }
}
