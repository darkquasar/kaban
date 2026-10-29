//! Closed converter projection of the canonical payload; core props validation is unchanged.

use crate::extract::structured_prep::schema::{props_fragment, select_kinds, stored_names};
use kaban_api::ingest_jobs::TargetSchema;
use kaban_core::{Ontology, ValidatorLimits, stored_name};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Fixed refusal for a schema outside the published converter subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unsupported_extraction_schema")]
pub struct SchemaError;

/// Project exact enabled kind branches from the loaded ontology and canonical wire DTO.
/// Undeclared props are not offered by the converter, even on permissive core kinds.
///
/// # Errors
/// Refuses unavailable kinds, invalid fragments or unsupported converter schemas.
pub fn target_schema(
    ontology: &Ontology,
    source_type: &str,
    limits: &ValidatorLimits,
) -> Result<TargetSchema, SchemaError> {
    let kinds = select_kinds(ontology, source_type).map_err(|_| SchemaError)?;
    let original = kaban_api::ingest_jobs::structured_return_schema();
    let mut wire = inline(&original, &original, 0)?;
    let record = wire
        .pointer("/properties/records/items")
        .ok_or(SchemaError)?
        .clone();
    let mut branches = Vec::new();
    let mut required = BTreeMap::new();
    for id in &kinds {
        let kind = ontology.kinds.get(id).ok_or(SchemaError)?;
        let name = stored_name(id);
        let props = props_fragment(kind, limits).map_err(|_| SchemaError)?;
        let props = inline(props.as_value(), props.as_value(), 0)?;
        let mut fields = vec!["local_id".into(), "kind".into(), "text".into()];
        if let Some(names) = props.get("required").and_then(Value::as_array) {
            fields.extend(
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|n| format!("props.{n}")),
            );
        }
        let mut branch = record.clone();
        branch["properties"]["kind"] = json!({"type":"string","enum":[name]});
        branch["properties"]["props"] = props;
        // A required props field cannot be satisfied by omitting the optional props object.
        if fields.len() > 3 {
            branch["required"]
                .as_array_mut()
                .ok_or(SchemaError)?
                .push(json!("props"));
        }
        required.insert(name, fields);
        branches.push(branch);
    }
    wire["properties"]["records"]["items"] = json!({"oneOf":branches});
    lint(&wire)?;
    let names = stored_names(&kinds);
    let mut allowed_edges = Vec::new();
    for rel in ontology.rels.values().filter(|rel| {
        !rel.governed_semantics
            && !matches!(
                rel.id.name.as_str(),
                "SUPERSEDES" | "INVALIDATES" | "CONTRADICTS" | "GOVERNS"
            )
    }) {
        let ends = |ends: &[kaban_core::ontology::resolve::RelEnd]| -> Vec<String> {
            if ends.is_empty() {
                return names.clone();
            }
            ends.iter()
                .filter_map(|end| match end {
                    kaban_core::ontology::resolve::RelEnd::Kind(id) => Some(stored_name(id)),
                    kaban_core::ontology::resolve::RelEnd::Path => None,
                })
                .collect()
        };
        for from in ends(&rel.from) {
            for to in ends(&rel.to) {
                allowed_edges.push([from.clone(), stored_name(&rel.id), to]);
            }
        }
    }
    Ok(TargetSchema {
        kinds: names,
        json_schema: wire,
        required,
        semantics: "Fill fields only from source text; absent fields stay absent; quotes are verbatim. Published props are restricted to declared non-internal fields, including empty props for undeclared schemas; core validation is unchanged.".into(),
        allowed_edges,
        kaban_adds: vec![["record".into(), "DERIVED_FROM".into(), "Document".into()], ["Chunk".into(), "EVIDENCE_FOR".into(), "record".into()]],
        governance: crate::extract::structured_prep::dialect::GOVERNANCE.into(),
    })
}

fn inline(value: &Value, root: &Value, depth: usize) -> Result<Value, SchemaError> {
    if depth > 64 {
        return Err(SchemaError);
    }
    let Some(object) = value.as_object() else {
        return Ok(value.clone());
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let target = root
            .pointer(reference.strip_prefix('#').ok_or(SchemaError)?)
            .ok_or(SchemaError)?;
        return inline(target, root, depth + 1);
    }
    if let Some(options) = object.get("anyOf").and_then(Value::as_array) {
        let mut nonnull = options
            .iter()
            .filter(|option| option.get("type") != Some(&json!("null")));
        let option = nonnull.next().ok_or(SchemaError)?;
        if nonnull.next().is_some() {
            return Err(SchemaError);
        }
        return inline(option, root, depth + 1);
    }
    let mut output = object.clone();
    // Numeric ranges remain enforced by the canonical typed transport/runtime, not by
    // unsupported converter keywords (the EN-F3 subset has no numeric bounds).
    for key in [
        "$schema",
        "$defs",
        "title",
        "description",
        "default",
        "minimum",
        "maximum",
        "format",
    ] {
        output.remove(key);
    }
    if let Some(types) = output.get("type").and_then(Value::as_array) {
        let types: Vec<_> = types
            .iter()
            .filter(|value| value.as_str() != Some("null"))
            .cloned()
            .collect();
        if types.len() != 1 {
            return Err(SchemaError);
        }
        output.insert("type".into(), types[0].clone());
    }
    if let Some(properties) = output.get_mut("properties").and_then(Value::as_object_mut) {
        for property in properties.values_mut() {
            *property = inline(property, root, depth + 1)?;
        }
    }
    if let Some(items) = output.get_mut("items") {
        *items = inline(items, root, depth + 1)?;
    }
    if output.get("type").and_then(Value::as_str) == Some("object") {
        output.insert("additionalProperties".into(), json!(false));
        output.entry("properties").or_insert_with(|| json!({}));
    }
    Ok(Value::Object(output))
}

/// Check the converter subset, admitting `oneOf` only at records.items with unique kind enums.
///
/// # Errors
/// Refuses arbitrary unions, refs, nonclosed objects, unanchored patterns or unknown keywords.
pub fn lint(schema: &Value) -> Result<(), SchemaError> {
    fn walk(value: &Value, records: bool, root: bool) -> Result<(), SchemaError> {
        const KEYS: &[&str] = &[
            "type",
            "properties",
            "required",
            "additionalProperties",
            "items",
            "enum",
            "pattern",
            "minLength",
            "maxLength",
            "oneOf",
        ];
        let object = value.as_object().ok_or(SchemaError)?;
        if object.keys().any(|key| !KEYS.contains(&key.as_str())) {
            return Err(SchemaError);
        }
        if let Some(pattern) = object.get("pattern").and_then(Value::as_str)
            && !(pattern.starts_with('^') && pattern.ends_with('$'))
        {
            return Err(SchemaError);
        }
        if let Some(branches) = object.get("oneOf") {
            if !records || object.len() != 1 {
                return Err(SchemaError);
            }
            let branches = branches
                .as_array()
                .filter(|v| !v.is_empty())
                .ok_or(SchemaError)?;
            let mut names = std::collections::BTreeSet::new();
            for branch in branches {
                let kind = branch
                    .pointer("/properties/kind/enum")
                    .and_then(Value::as_array)
                    .filter(|v| v.len() == 1)
                    .ok_or(SchemaError)?;
                if !names.insert(kind[0].as_str().ok_or(SchemaError)?)
                    || branch
                        .pointer("/properties/props/type")
                        .and_then(Value::as_str)
                        != Some("object")
                {
                    return Err(SchemaError);
                }
                walk(branch, false, false)?;
            }
            return Ok(());
        }
        if object.get("type").and_then(Value::as_str) == Some("object")
            && object.get("additionalProperties") != Some(&json!(false))
        {
            return Err(SchemaError);
        }
        if let Some(properties) = object.get("properties") {
            for (name, property) in properties.as_object().ok_or(SchemaError)? {
                if root && name == "records" {
                    let mut copy = property.clone();
                    let items = copy
                        .as_object_mut()
                        .ok_or(SchemaError)?
                        .remove("items")
                        .ok_or(SchemaError)?;
                    walk(&copy, false, false)?;
                    walk(&items, true, false)?;
                } else {
                    walk(property, false, false)?;
                }
            }
        }
        if let Some(items) = object.get("items") {
            walk(items, false, false)?;
        }
        Ok(())
    }
    walk(schema, false, true)
}
