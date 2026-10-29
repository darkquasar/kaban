//! Public ontology selection and immutable props projection coverage.
#![allow(clippy::unwrap_used)] // fixture failures are test failures
use kaban_core::{NamespaceSet, OntologyFile, Schema, ValidatorLimits, resolve, shipped};
use kaban_ingest::extract::structured_prep::schema::{
    INTERNAL_PROPERTIES, SelectionError, path_claims, props_fragment, select_kinds, stored_names,
};
use kaban_ingest::gate_path;
use kaban_policy::Policy;
use serde_json::json;

#[test]
fn shipped_fragments_parse_without_internal_properties_or_source_mutation() {
    let ontology = shipped::resolved().unwrap();
    let limits = ValidatorLimits::default();
    for kind in ontology.kinds.values() {
        let original = serde_json::to_vec(kind.props_schema.as_value()).unwrap();
        let projected = props_fragment(kind, &limits).unwrap();
        Schema::parse(projected.as_value(), &limits).unwrap();
        for name in INTERNAL_PROPERTIES {
            assert!(
                projected
                    .as_value()
                    .get("properties")
                    .and_then(|v| v.get(name))
                    .is_none()
            );
            assert!(
                !projected
                    .as_value()
                    .get("required")
                    .and_then(|v| v.as_array())
                    .is_some_and(|items| items.contains(&json!(name)))
            );
        }
        assert_eq!(
            original,
            serde_json::to_vec(kind.props_schema.as_value()).unwrap()
        );
    }
    let fact = ontology
        .kinds
        .values()
        .find(|kind| kind.id.name == "Fact")
        .unwrap();
    assert_eq!(
        props_fragment(fact, &limits).unwrap().as_value(),
        &json!({"type":"object"})
    );
    assert_eq!(
        stored_names(&select_kinds(&ontology, "adr").unwrap()),
        ["Decision", "Fact"]
    );
    assert_eq!(
        stored_names(&select_kinds(&ontology, "unknown").unwrap()),
        ["Fact"]
    );
}

#[test]
fn ontology_extension_unions_namespaces_and_claims_without_code_kind_table() {
    let mut files = shipped::files().unwrap();
    files
        .push(OntologyFile::from_yaml(include_str!("fixtures/structured/incidents.yaml")).unwrap());
    let ontology = resolve(&files, &NamespaceSet::new()).unwrap();
    let selected = select_kinds(&ontology, "postmortem").unwrap();
    assert_eq!(
        stored_names(&selected),
        ["Failure", "Improvement", "incidents:Incident"]
    );
    for id in &selected {
        props_fragment(&ontology.kinds[id], &ValidatorLimits::default()).unwrap();
    }
    assert_eq!(
        stored_names(&select_kinds(&ontology, "missing").unwrap()),
        ["Fact", "incidents:Incident"]
    );
    let path = gate_path(&Policy::empty(), "incidents/outage.md").unwrap();
    assert_eq!(path_claims(&ontology, &path).unwrap(), ["postmortem"]);
    let path = gate_path(&Policy::empty(), "docs/adr/0001-choice.md").unwrap();
    assert_eq!(path_claims(&ontology, &path).unwrap(), ["adr"]);
    let path = gate_path(&Policy::empty(), "other/incidents/outage.md").unwrap();
    assert_eq!(path_claims(&ontology, &path).unwrap(), Vec::<String>::new());
}

#[test]
fn disabled_mapped_kinds_never_fall_back_or_reenable() {
    let mut ontology = shipped::resolved().unwrap();
    let decision = ontology
        .kinds
        .values_mut()
        .find(|kind| kind.id.name == "Decision")
        .unwrap();
    decision.extract.as_mut().unwrap().enabled = Some(false);
    assert_eq!(
        stored_names(&select_kinds(&ontology, "adr").unwrap()),
        ["Fact"]
    );
    for types in ontology.source_types.values_mut() {
        if let Some(mapping) = types.get_mut("adr") {
            mapping.kinds.retain(|id| id.name == "Decision");
        }
    }
    assert_eq!(
        select_kinds(&ontology, "adr"),
        Err(SelectionError::NoExtractableKinds)
    );
    assert_eq!(
        stored_names(&select_kinds(&ontology, "unknown").unwrap()),
        ["Fact"]
    );
    ontology.default_kind.clear();
    assert_eq!(
        stored_names(&select_kinds(&ontology, "unknown").unwrap()),
        ["Fact"]
    );
    ontology.kinds.retain(|id, _| id.name != "Fact");
    assert_eq!(
        select_kinds(&ontology, "unknown"),
        Err(SelectionError::MissingDefaultKind)
    );
}

#[test]
fn projection_removes_matching_required_entries_but_preserves_nested_names() {
    let mut ontology = shipped::resolved().unwrap();
    let kind = ontology
        .kinds
        .values_mut()
        .find(|kind| kind.id.name == "Fact")
        .unwrap();
    let value = json!({"type":"object", "additionalProperties":false,
        "required":["evidence","pending_fix","command_fingerprint","evidence_runs","accepted_commit","public"],
        "properties":{"evidence":{"type":"string"},"pending_fix":{"type":"boolean"},
        "command_fingerprint":{"type":"string"},"evidence_runs":{"type":"array","items":{"type":"string"}},
        "accepted_commit":{"type":"string"},"public":{"type":"object","required":["evidence"],"properties":{"evidence":{"type":"string"}}}}});
    kind.props_schema = Schema::parse(&value, &ValidatorLimits::default()).unwrap();
    let fragment = props_fragment(kind, &ValidatorLimits::default()).unwrap();
    assert_eq!(fragment.as_value()["required"], json!(["public"]));
    assert_eq!(
        fragment.as_value()["properties"]["public"],
        value["properties"]["public"]
    );
    assert_eq!(kind.props_schema.as_value(), &value);
}

#[test]
fn unsupported_schema_features_and_invalid_globs_refuse() {
    let limits = ValidatorLimits::default();
    for value in [
        json!({"type":"object","$ref":"https://invalid.test/schema"}),
        json!({"type":"object","if":{"type":"object"}}),
        json!({"type":"object","not":{"type":"object"}}),
        json!({"type":"array","items":{"type":"string"},"uniqueItems":true}),
        json!({"type":"string","pattern":"["}),
    ] {
        assert!(Schema::parse(&value, &limits).is_err());
    }
    let schema = Schema::parse(&json!({"type":"string","pattern":"abc"}), &limits).unwrap();
    assert_eq!(kaban_core::validate(&schema, &json!("abc")), Vec::new());
    assert_ne!(kaban_core::validate(&schema, &json!("xabcx")), Vec::new());
    let mut ontology = shipped::resolved().unwrap();
    let kind = ontology
        .kinds
        .values_mut()
        .find(|kind| kind.id.name == "Fact")
        .unwrap();
    kind.props_schema = Schema::parse(
        &json!({"type":"object","properties":{"public":{"type":"string","pattern":"abc"}}}),
        &limits,
    )
    .unwrap();
    assert!(
        props_fragment(
            kind,
            &ValidatorLimits {
                pattern_size_limit_kib: 0
            }
        )
        .is_err()
    );
    let mapping = ontology
        .source_types
        .values_mut()
        .find_map(|types| types.get_mut("adr"))
        .unwrap();
    mapping.paths.push("[".to_owned());
    let path = gate_path(&Policy::empty(), "notes.md").unwrap();
    assert_eq!(
        path_claims(&ontology, &path),
        Err(SelectionError::InvalidPathGlob)
    );
}
