//! The `props_schema` validator subset (engine-spec EN-F3, EN-F3a; EN-S11 `$ref`; engine-plan
//! E1.1): a hand-written validator for exactly `type` in `{object, string, number, integer,
//! boolean, array}`, `required`, `enum`, `additionalProperties: false`, `properties`, `items`
//! (one schema), `minLength`/`maxLength` (UTF-8 bytes) and `pattern` (anchored as `^(?:p)$`,
//! compiled under a size limit). Everything else fails meta-validation by name
//! (`schema_unsupported:<keyword>`, `$ref` included); malformed uses of a supported keyword fail
//! `schema_invalid:<rule>`. Instance validation is total and its error list is sorted by
//! (pointer depth, pointer bytes, keyword order), so the first error is the one a caller
//! reports as `invalid_props <dotted pointer>`.

use std::collections::BTreeMap;
use std::fmt;

use regex::{Regex, RegexBuilder};
use serde_json::Value;

use crate::canon::canonical_json;

/// Limits the meta-validation applies (engine-spec §15.1 `[ontology]`; built from
/// [`crate::config::OntologyCfg`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatorLimits {
    /// `RegexBuilder::size_limit` for a compiled `pattern`, in KiB (`[ontology]
    /// pattern_size_limit_kib`, default 1024).
    pub pattern_size_limit_kib: u32,
}

impl Default for ValidatorLimits {
    fn default() -> Self {
        Self {
            pattern_size_limit_kib: 1024,
        }
    }
}

/// Why a schema failed meta-validation (EN-F3a). `Display` is the fixed EN-F7 text
/// (`schema_unsupported:<keyword>` or `schema_invalid:<rule>`); the pointer locates the offending
/// node in the schema and is never part of the text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    /// A keyword outside the subset, or a supported keyword used in an unsupported form
    /// (`additionalProperties: true`, tuple `items`).
    #[error("schema_unsupported:{keyword}")]
    Unsupported {
        /// The keyword as written.
        keyword: String,
        /// RFC 6901 pointer of the schema node that carries it.
        pointer: String,
    },
    /// A supported keyword with a malformed value or an inconsistent combination.
    #[error("schema_invalid:{rule}")]
    Invalid {
        /// The rule name (`duplicate_required`, `items_required`, `pattern_size`, `number`, ...).
        rule: String,
        /// RFC 6901 pointer of the schema node that breaks the rule.
        pointer: String,
    },
}

/// A meta-validated `props_schema` (EN-F3a): constructed only through [`Schema::parse`], so
/// every schema a validator sees is inside the subset and its patterns compiled.
#[derive(Debug, Clone)]
pub struct Schema {
    value: Value,
    /// Compiled anchored patterns by schema pointer (`/properties/x`), filled at parse time.
    patterns: BTreeMap<String, Regex>,
}

/// The six `type` names of the subset.
const TYPES: [&str; 6] = ["object", "string", "number", "integer", "boolean", "array"];

/// Every keyword of the subset (EN-F3); applicability per type is checked after membership.
const SUBSET: [&str; 9] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minLength",
    "maxLength",
    "pattern",
    "enum",
];

impl Schema {
    /// Meta-validate `v` (EN-F3a) and compile its patterns under `limits`.
    ///
    /// # Errors
    /// [`SchemaError::Unsupported`] for a keyword outside the subset (including `$ref`,
    /// `additionalProperties: true` or a schema, tuple `items`); [`SchemaError::Invalid`] for a
    /// malformed supported keyword (`type_required`, `type`, `not_object`, `properties`,
    /// `required`, `duplicate_required`, `required_property`, `items_required`, `enum`,
    /// `enum_empty`, `length`, `length_range`, `pattern`, `pattern_size`, `number`).
    pub fn parse(v: &Value, limits: &ValidatorLimits) -> Result<Schema, SchemaError> {
        let mut patterns = BTreeMap::new();
        let size_limit = usize::try_from(limits.pattern_size_limit_kib)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024);
        check_finite(v, "")?;
        meta(v, "", size_limit, &mut patterns)?;
        Ok(Schema {
            value: v.clone(),
            patterns,
        })
    }

    /// The meta-validated schema as JSON.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.value
    }
}

/// `schema_invalid:number` for a non-finite number anywhere in the schema. `serde_json` rejects
/// `NaN` and infinities on parse already; the rule is enforced here so a future parser cannot
/// relax it.
fn check_finite(v: &Value, pointer: &str) -> Result<(), SchemaError> {
    match v {
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() || n.as_f64().is_some_and(f64::is_finite) {
                Ok(())
            } else {
                Err(invalid("number", pointer))
            }
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(i, item)| check_finite(item, &format!("{pointer}/{i}"))),
        Value::Object(map) => map
            .iter()
            .try_for_each(|(k, item)| check_finite(item, &format!("{pointer}/{}", escape(k)))),
        _ => Ok(()),
    }
}

fn invalid(rule: &str, pointer: &str) -> SchemaError {
    SchemaError::Invalid {
        rule: rule.to_owned(),
        pointer: pointer.to_owned(),
    }
}

fn unsupported(keyword: &str, pointer: &str) -> SchemaError {
    SchemaError::Unsupported {
        keyword: keyword.to_owned(),
        pointer: pointer.to_owned(),
    }
}

/// The recursive meta-validation of one schema node.
fn meta(
    v: &Value,
    pointer: &str,
    size_limit: usize,
    patterns: &mut BTreeMap<String, Regex>,
) -> Result<(), SchemaError> {
    let Value::Object(map) = v else {
        return Err(invalid("not_object", pointer));
    };
    // a keyword outside the subset fails by name before anything else, so `$ref` is reported
    // as `schema_unsupported:$ref` wherever it appears (EN-S11)
    for key in map.keys() {
        if !SUBSET.contains(&key.as_str()) {
            return Err(unsupported(key, pointer));
        }
    }
    let ty = match map.get("type") {
        None => return Err(invalid("type_required", pointer)),
        Some(Value::String(t)) if TYPES.contains(&t.as_str()) => t.as_str(),
        Some(_) => return Err(invalid("type", pointer)),
    };
    // keywords applicable per type; anything else is unsupported by name (`$ref` included)
    let allowed: &[&str] = match ty {
        "object" => &["type", "properties", "required", "additionalProperties"],
        "array" => &["type", "items"],
        "string" => &["type", "minLength", "maxLength", "pattern", "enum"],
        _ => &["type", "enum"],
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(unsupported(key, pointer));
        }
    }
    match ty {
        "object" => meta_object(map, pointer, size_limit, patterns),
        "array" => match map.get("items") {
            None => Err(invalid("items_required", pointer)),
            Some(Value::Array(_)) => Err(unsupported("items", pointer)),
            Some(items) => meta(items, &format!("{pointer}/items"), size_limit, patterns),
        },
        "string" => meta_string(map, pointer, size_limit, patterns),
        _ => meta_enum(map, pointer),
    }
}

fn meta_object(
    map: &serde_json::Map<String, Value>,
    pointer: &str,
    size_limit: usize,
    patterns: &mut BTreeMap<String, Regex>,
) -> Result<(), SchemaError> {
    let closed = match map.get("additionalProperties") {
        None => false,
        Some(Value::Bool(false)) => true,
        Some(_) => return Err(unsupported("additionalProperties", pointer)),
    };
    let properties = match map.get("properties") {
        None => None,
        Some(Value::Object(props)) => Some(props),
        Some(_) => return Err(invalid("properties", pointer)),
    };
    if let Some(Value::Array(required)) = map.get("required") {
        let mut seen = Vec::with_capacity(required.len());
        for name in required {
            let Value::String(name) = name else {
                return Err(invalid("required", &format!("{pointer}/required")));
            };
            if seen.contains(&name.as_str()) {
                return Err(invalid(
                    "duplicate_required",
                    &format!("{pointer}/required"),
                ));
            }
            seen.push(name.as_str());
            if closed && !properties.is_some_and(|p| p.contains_key(name)) {
                return Err(invalid("required_property", &format!("{pointer}/required")));
            }
        }
    } else if map.contains_key("required") {
        return Err(invalid("required", &format!("{pointer}/required")));
    }
    if let Some(props) = properties {
        for (name, sub) in props {
            meta(
                sub,
                &format!("{pointer}/properties/{}", escape(name)),
                size_limit,
                patterns,
            )?;
        }
    }
    Ok(())
}

fn meta_string(
    map: &serde_json::Map<String, Value>,
    pointer: &str,
    size_limit: usize,
    patterns: &mut BTreeMap<String, Regex>,
) -> Result<(), SchemaError> {
    let length = |key: &str| -> Result<Option<u64>, SchemaError> {
        match map.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => n
                .as_u64()
                .map(Some)
                .ok_or_else(|| invalid("length", &format!("{pointer}/{key}"))),
            Some(_) => Err(invalid("length", &format!("{pointer}/{key}"))),
        }
    };
    let min = length("minLength")?;
    let max = length("maxLength")?;
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return Err(invalid("length_range", pointer));
    }
    match map.get("pattern") {
        None => {}
        Some(Value::String(p)) => {
            let anchored = format!("^(?:{p})$");
            let compiled = RegexBuilder::new(&anchored)
                .size_limit(size_limit)
                .build()
                .map_err(|e| match e {
                    regex::Error::CompiledTooBig(_) => {
                        invalid("pattern_size", &format!("{pointer}/pattern"))
                    }
                    _ => invalid("pattern", &format!("{pointer}/pattern")),
                })?;
            patterns.insert(pointer.to_owned(), compiled);
        }
        Some(_) => return Err(invalid("pattern", &format!("{pointer}/pattern"))),
    }
    meta_enum(map, pointer)
}

fn meta_enum(map: &serde_json::Map<String, Value>, pointer: &str) -> Result<(), SchemaError> {
    match map.get("enum") {
        Some(Value::Array(values)) if values.is_empty() => {
            Err(invalid("enum_empty", &format!("{pointer}/enum")))
        }
        None | Some(Value::Array(_)) => Ok(()),
        Some(_) => Err(invalid("enum", &format!("{pointer}/enum"))),
    }
}

/// RFC 6901 escaping of one reference token.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// RFC 6901 unescaping of one reference token.
fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// The keyword an instance error belongs to, in the fixed sort order of EN-F3a.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Keyword {
    /// `type`
    Type,
    /// `required`
    Required,
    /// `additionalProperties`
    AdditionalProperties,
    /// `enum`
    Enum,
    /// `minLength`
    MinLength,
    /// `maxLength`
    MaxLength,
    /// `pattern`
    Pattern,
    /// `properties`
    Properties,
    /// `items`
    Items,
}

impl Keyword {
    /// The keyword as written in a schema.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Keyword::Type => "type",
            Keyword::Required => "required",
            Keyword::AdditionalProperties => "additionalProperties",
            Keyword::Enum => "enum",
            Keyword::MinLength => "minLength",
            Keyword::MaxLength => "maxLength",
            Keyword::Pattern => "pattern",
            Keyword::Properties => "properties",
            Keyword::Items => "items",
        }
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One instance-validation failure. `detail` names the rule, never the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// RFC 6901 pointer into the instance (`/rationale`, `/evidence/0/id`; `""` is the root).
    pub pointer: String,
    /// The keyword that failed.
    pub keyword: Keyword,
    /// A short fixed description (`expected string`, `missing`, `not allowed`, ...).
    pub detail: String,
}

/// Validate `value` against `schema` (EN-F3a): total, recursive, errors sorted by (pointer
/// depth ascending, pointer bytes ascending, keyword order).
#[must_use]
pub fn validate(schema: &Schema, value: &Value) -> Vec<Error> {
    let mut errors = Vec::new();
    walk(schema, &schema.value, "", value, "", &mut errors);
    errors.sort_by(|a, b| {
        depth(&a.pointer)
            .cmp(&depth(&b.pointer))
            .then_with(|| a.pointer.as_bytes().cmp(b.pointer.as_bytes()))
            .then_with(|| a.keyword.cmp(&b.keyword))
    });
    errors
}

fn depth(pointer: &str) -> usize {
    pointer.matches('/').count()
}

fn push(errors: &mut Vec<Error>, pointer: &str, keyword: Keyword, detail: &str) {
    errors.push(Error {
        pointer: pointer.to_owned(),
        keyword,
        detail: detail.to_owned(),
    });
}

/// Does `value` have the JSON type `ty`? `integer` is a number `serde_json` parsed as `i64` or
/// `u64` (`1.0` is not an integer); `number` is any finite number.
fn has_type(ty: &str, value: &Value) -> bool {
    match ty {
        "object" => value.is_object(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(|n| n.is_i64() || n.is_u64()),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        _ => false,
    }
}

fn walk(
    schema: &Schema,
    node: &Value,
    node_pointer: &str,
    value: &Value,
    pointer: &str,
    errors: &mut Vec<Error>,
) {
    // the schema is meta-validated: every node is an object with a known `type`
    let Value::Object(map) = node else { return };
    let ty = map.get("type").and_then(Value::as_str).unwrap_or("");
    if !has_type(ty, value) {
        push(errors, pointer, Keyword::Type, &format!("expected {ty}"));
        return;
    }
    if let Some(Value::Array(allowed)) = map.get("enum") {
        let text = canonical_json(value);
        if !allowed.iter().any(|a| canonical_json(a) == text) {
            push(errors, pointer, Keyword::Enum, "not one of the enum values");
        }
    }
    match (ty, value) {
        ("object", Value::Object(object)) => {
            let properties = map.get("properties").and_then(Value::as_object);
            if let Some(Value::Array(required)) = map.get("required") {
                for name in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(name) {
                        let p = format!("{pointer}/{}", escape(name));
                        push(errors, &p, Keyword::Required, "missing");
                    }
                }
            }
            let closed = matches!(map.get("additionalProperties"), Some(Value::Bool(false)));
            for (name, member) in object {
                let p = format!("{pointer}/{}", escape(name));
                match properties.and_then(|props| props.get(name)) {
                    Some(sub) => {
                        let sp = format!("{node_pointer}/properties/{}", escape(name));
                        walk(schema, sub, &sp, member, &p, errors);
                    }
                    None if closed => {
                        push(errors, &p, Keyword::AdditionalProperties, "not allowed");
                    }
                    None => {}
                }
            }
        }
        ("array", Value::Array(items)) => {
            if let Some(sub) = map.get("items") {
                let sp = format!("{node_pointer}/items");
                for (i, item) in items.iter().enumerate() {
                    walk(schema, sub, &sp, item, &format!("{pointer}/{i}"), errors);
                }
            }
        }
        ("string", Value::String(s)) => {
            let bytes = s.len() as u64;
            if let Some(min) = map.get("minLength").and_then(Value::as_u64)
                && bytes < min
            {
                push(
                    errors,
                    pointer,
                    Keyword::MinLength,
                    "shorter than minLength",
                );
            }
            if let Some(max) = map.get("maxLength").and_then(Value::as_u64)
                && bytes > max
            {
                push(errors, pointer, Keyword::MaxLength, "longer than maxLength");
            }
            if let Some(re) = schema.patterns.get(node_pointer)
                && !re.is_match(s)
            {
                push(errors, pointer, Keyword::Pattern, "does not match pattern");
            }
        }
        _ => {}
    }
}

/// The text a caller reports for a failed validation: `invalid_props <dotted pointer>` of the
/// first (sorted) error, where `/rationale` renders as `props.rationale`, `/evidence/0/id` as
/// `props.evidence.0.id` and the root as `props`. `None` when there is no error.
#[must_use]
pub fn first_error_text(errors: &[Error]) -> Option<String> {
    let first = errors.first()?;
    let mut text = String::from("invalid_props props");
    for token in first.pointer.split('/').skip(1) {
        text.push('.');
        text.push_str(&unescape(token));
    }
    Some(text)
}
