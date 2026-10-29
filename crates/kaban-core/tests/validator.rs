//! `props_schema` validator subset, meta-validation and canonical JSON (engine-spec EN-F3,
//! EN-F3a, EN-S11 `$ref`; engine-plan E1.1). Table-driven over `fixtures/validator/*.json`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::fs;
use std::path::PathBuf;

use kaban_core::{
    Keyword, PropsError, Schema, SchemaError, ValidatorLimits, canonical_json, first_error_text,
    validate,
};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/validator")
        .join(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn canonical_json_sorts_keys_bytewise_drops_whitespace_and_is_idempotent() {
    let table = fixture("canonical.json");
    for case in table.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let expected = case["canonical"].as_str().unwrap();
        let got = canonical_json(&case["value"]);
        assert_eq!(got, expected, "{name}");
        let reparsed: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(canonical_json(&reparsed), expected, "{name}: idempotent");
    }
    // byte order, not locale order: uppercase before lowercase, `_` between them
    let v = json!({"b": 1, "B": 2, "_": 3, "a": 4});
    assert_eq!(canonical_json(&v), r#"{"B":2,"_":3,"a":4,"b":1}"#);
}

fn parse(schema: &Value) -> Result<Schema, SchemaError> {
    Schema::parse(schema, &ValidatorLimits::default())
}

fn parsed(schema: &Value) -> Schema {
    parse(schema).unwrap_or_else(|e| panic!("{e} in {schema}"))
}

#[test]
fn every_supported_keyword_is_accepted_and_judges_an_instance_each_way() {
    let table = fixture("accepted_keywords.json");
    let mut seen = Vec::new();
    for case in table.as_array().unwrap() {
        let keyword = case["keyword"].as_str().unwrap();
        let schema = parsed(&case["schema"]);
        assert_eq!(
            validate(&schema, &case["accepts"]),
            Vec::new(),
            "{keyword}: {} should be accepted",
            case["accepts"]
        );
        let errors = validate(&schema, &case["rejects"]);
        assert_ne!(
            errors,
            Vec::new(),
            "{keyword}: {} should be rejected",
            case["rejects"]
        );
        // the failing keyword is the one under test unless it is a `type` mismatch below it
        assert!(
            errors
                .iter()
                .any(|e| e.keyword.as_str() == keyword || e.keyword == Keyword::Type),
            "{keyword}: {errors:?}"
        );
        seen.push(keyword);
    }
    for required in [
        "type",
        "required",
        "enum",
        "additionalProperties",
        "properties",
        "items",
        "minLength",
        "maxLength",
        "pattern",
    ] {
        assert!(seen.contains(&required), "no fixture for {required}");
    }
}

#[test]
fn every_keyword_outside_the_subset_is_rejected_by_name_including_ref() {
    let table = fixture("rejected_keywords.json");
    for case in table.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let expected = case["error"].as_str().unwrap();
        match parse(&case["schema"]) {
            Ok(_) => panic!("{name}: accepted, expected {expected}"),
            Err(e) => assert_eq!(e.to_string(), expected, "{name}"),
        }
    }
    // the pointer locates the node without entering the text
    let err = parse(&json!({"type": "object", "properties": {"a": {"$ref": "#/x"}}})).unwrap_err();
    assert_eq!(
        err,
        SchemaError::Unsupported {
            keyword: "$ref".into(),
            pointer: "/properties/a".into()
        }
    );
}

#[test]
fn instance_errors_are_sorted_by_depth_pointer_and_keyword_and_the_first_is_the_text() {
    let table = fixture("instances.json");
    for case in table.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let schema = parsed(&case["schema"]);
        let errors = validate(&schema, &case["value"]);
        let got: Vec<(String, String)> = errors
            .iter()
            .map(|e| (e.pointer.clone(), e.keyword.to_string()))
            .collect();
        let expected: Vec<(String, String)> = case["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().unwrap().to_owned(),
                    pair[1].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(got, expected, "{name}");
        let first = first_error_text(&errors);
        assert_eq!(first.as_deref(), case["first"].as_str(), "{name}");
        // the detail never echoes the instance
        for e in &errors {
            assert!(!e.detail.contains("zz"), "{name}: {e:?}");
        }
    }
}

#[test]
fn a_pattern_over_the_size_limit_fails_pattern_size_and_a_tighter_limit_is_honoured() {
    // a counted repetition compiles into a large program; 1 KiB cannot hold it, 1 MiB can
    let schema = json!({"type": "string", "pattern": "(?:[a-z]{50}){50}"});
    let tight = ValidatorLimits {
        pattern_size_limit_kib: 1,
    };
    let err = Schema::parse(&schema, &tight).unwrap_err();
    assert_eq!(err.to_string(), "schema_invalid:pattern_size");
    assert_eq!(
        err,
        SchemaError::Invalid {
            rule: "pattern_size".into(),
            pointer: "/pattern".into()
        }
    );
    assert!(Schema::parse(&schema, &ValidatorLimits::default()).is_ok());
    // the default limit is the §15.1 register value
    assert_eq!(ValidatorLimits::default().pattern_size_limit_kib, 1024);
}

#[test]
fn a_non_finite_number_cannot_reach_the_schema_through_serde_json() {
    // the EN-F3a rule `schema_invalid:number` is stated for a future parser; today's parser
    // refuses the literals before the validator sees them
    for text in [
        "NaN",
        "Infinity",
        "-Infinity",
        r#"{"type":"number","enum":[NaN]}"#,
    ] {
        assert!(serde_json::from_str::<Value>(text).is_err(), "{text}");
    }
    // the largest finite f64 is still a number
    let v: Value = serde_json::from_str("1.7976931348623157e308").unwrap();
    assert!(parse(&json!({"type": "number", "enum": [v]})).is_ok());
}

#[test]
fn errors_expose_pointer_keyword_and_a_value_free_detail() {
    let schema = parsed(&json!({"type": "object", "required": ["rationale"]}));
    let errors = validate(&schema, &json!({}));
    assert_eq!(
        errors,
        vec![PropsError {
            pointer: "/rationale".into(),
            keyword: Keyword::Required,
            detail: "missing".into()
        }]
    );
    assert_eq!(first_error_text(&[]), None);
}

/// The struct's verdict on `props`: `Ok` when the typed struct round-trips it, `Err` with
/// serde's text otherwise (the text is only used for the failure message).
fn struct_verdict<T: serde::de::DeserializeOwned + serde::Serialize>(
    props: &Value,
) -> Result<(), String> {
    let typed: T = serde_json::from_value(props.clone()).map_err(|e| e.to_string())?;
    // what the struct serializes back must still be accepted (round trip)
    let back = serde_json::to_value(&typed).map_err(|e| e.to_string())?;
    serde_json::from_value::<T>(back)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Every fixture of `props_<kind>.json` judged by the struct and by the golden schema; the
/// disagreements as `(fixture name, struct verdict, schema first error)`.
fn disagreements<T: serde::de::DeserializeOwned + serde::Serialize>(
    props_fixture: &str,
    golden: &Schema,
) -> Vec<(String, bool, Option<String>)> {
    let table = fixture(props_fixture);
    let mut out = Vec::new();
    for case in table.as_array().unwrap() {
        let name = case["name"].as_str().unwrap().to_owned();
        let struct_ok = struct_verdict::<T>(&case["props"]).is_ok();
        let errors = validate(golden, &case["props"]);
        if struct_ok != errors.is_empty() {
            out.push((name, struct_ok, first_error_text(&errors)));
        }
    }
    out
}

#[test]
fn the_golden_schemas_agree_with_the_three_structs_with_golden_strict_false() {
    let decision = parsed(&fixture("golden_decision.json"));
    let convention = parsed(&fixture("golden_convention.json"));
    let failure = parsed(&fixture("golden_failure.json"));
    let evidence = fixture("golden_evidence.json");
    // the Evidence golden is embedded by value under `evidence.items` of both schemas
    assert_eq!(
        decision.as_value()["properties"]["evidence"]["items"],
        evidence
    );
    assert_eq!(
        convention.as_value()["properties"]["evidence"]["items"],
        evidence
    );
    assert!(parse(&evidence).is_ok());
    let d = disagreements::<kaban_core::DecisionProps>("props_decision.json", &decision);
    assert_eq!(d, Vec::new(), "Decision");
    let c = disagreements::<kaban_core::ConventionProps>("props_convention.json", &convention);
    assert_eq!(c, Vec::new(), "Convention");
    let f = disagreements::<kaban_core::FailureProps>("props_failure.json", &failure);
    assert_eq!(f, Vec::new(), "Failure");
}

#[test]
fn a_decision_without_rationale_yields_exactly_the_text_validate_props_emits_today() {
    let decision = parsed(&fixture("golden_decision.json"));
    let errors = validate(&decision, &json!({"decision": "x"}));
    assert_eq!(
        first_error_text(&errors).as_deref(),
        Some("invalid_props props.rationale")
    );
    // an unknown key renders like the struct's `unknown field` today
    let errors = validate(&decision, &json!({"rationale": "why", "ratonale": "typo"}));
    assert_eq!(
        first_error_text(&errors).as_deref(),
        Some("invalid_props props.ratonale")
    );
}

#[test]
fn golden_strict_true_tightenings_disagree_with_the_structs_only_where_recorded() {
    // §9.1: the agreement test with the tightened goldens, pass/fail recorded (not a gate on
    // the shipped file: `[ontology] golden_strict = false` keeps the tightenings out)
    let decision = parsed(&fixture("golden_decision_strict.json"));
    let convention = parsed(&fixture("golden_convention_strict.json"));
    let d = disagreements::<kaban_core::DecisionProps>("props_decision.json", &decision);
    let names: Vec<&str> = d.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "accepted_commit not hex is accepted by the struct and the loose golden",
            "long quote accepted by the struct and the loose golden",
            "long id accepted by the struct and the loose golden",
        ]
    );
    let c = disagreements::<kaban_core::ConventionProps>("props_convention.json", &convention);
    let names: Vec<&str> = c.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["accepted_commit not hex is accepted by the struct and the loose golden"]
    );
}

#[test]
fn min_length_one_on_rationale_and_evidence_id_is_stricter_than_the_structs_recorded() {
    // engine-spec EN-F3a spells `minLength: 1` on `rationale` and `Evidence.id` as part of the
    // golden (not as a `golden_strict` tightening); the structs accept the empty string. Recorded
    // divergence for E1.5 (the seam that replaces `validate_props`).
    let decision = parsed(&fixture("golden_decision.json"));
    for props in [
        json!({"rationale": ""}),
        json!({"rationale": "why", "evidence": [{"id": ""}]}),
    ] {
        assert!(struct_verdict::<kaban_core::DecisionProps>(&props).is_ok());
        assert_eq!(validate(&decision, &props).len(), 1, "{props}");
    }
}
