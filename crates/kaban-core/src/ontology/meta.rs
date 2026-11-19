//! The ontology file form (engine-spec §3.1 meta-schema, EN-F1; engine-plan E1.2): identifier
//! newtypes with the spec regexes, the typed bodies with `deny_unknown_fields` at every level,
//! and the loading order: the E1.1 validator against the embedded `ontology-meta.yaml` first,
//! then the typed parse. A file-form failure is reported as `schema_invalid:file:<path>` with the
//! dotted path of the offending key (never a value); a name failing its regex is
//! `invalid_identifier`. The keys of the removed governance pipeline (`priors`,
//! `confidence_required`, `min_confidence`, the `governed` mixin with its `acceptance`,
//! `serialize`, `export` and `governs_paths`, a kind's `acceptance` and a rel's
//! `governed_semantics`, and the `index` block with its commit `stamp` knob) are unknown keys
//! like any other, refused with their dotted path.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::OntologyError;
use crate::validator::{Schema, ValidatorLimits, first_error_text, validate};

/// The meta-schema bytes shipped in the binary.
pub const META_SCHEMA_YAML: &str = include_str!("../../ontology/ontology-meta.yaml");

/// The parsed meta-schema (parsed once; `Schema::parse` of it is itself a test).
fn meta_schema() -> &'static Schema {
    static META: OnceLock<Schema> = OnceLock::new();
    META.get_or_init(|| {
        let value: Value = yaml_serde::from_str(META_SCHEMA_YAML)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
        Schema::parse(&value, &ValidatorLimits::default()).unwrap_or_else(|_| {
            // unreachable for the shipped file (`meta_schema_parses` asserts it); an empty
            // object schema is the total fallback
            Schema::parse(
                &serde_json::json!({"type": "object"}),
                &ValidatorLimits::default(),
            )
            .unwrap_or_else(|_| unreachable!("`{{type: object}}` is in the subset"))
        })
    })
}

/// A namespace: `^[a-z][a-z0-9-]{0,31}$`; `x` is reserved for discovered kinds (EN-F5).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Ns(String);

/// A kind name: `^[A-Z][A-Za-z0-9]{0,63}$`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct KindName(String);

/// A relationship name: `^[A-Z][A-Z0-9_]{0,63}$`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct RelName(String);

/// Is `s` a namespace per the spec regex (`x` excluded)?
fn is_ns(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && s != "x"
}

/// Is `s` a kind name per the spec regex?
pub(crate) fn is_kind_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Is `s` a rel name per the spec regex?
pub(crate) fn is_rel_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A name failing its regex. Content-free: the caller that knows the field adds its path as
/// [`OntologyError::InvalidIdentifier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid_identifier")]
pub struct IdentifierError;

impl IdentifierError {
    /// The ontology error for the identifier at `path`.
    #[must_use]
    pub fn at(self, path: impl Into<String>) -> OntologyError {
        OntologyError::InvalidIdentifier { path: path.into() }
    }
}

macro_rules! identifier {
    ($ty:ident, $check:ident) => {
        impl $ty {
            /// Parse and validate.
            ///
            /// # Errors
            /// [`IdentifierError`] when the regex does not match.
            pub fn parse(s: &str) -> Result<Self, IdentifierError> {
                if $check(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(IdentifierError)
                }
            }

            /// The validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $ty {
            type Error = String;
            fn try_from(s: String) -> Result<Self, String> {
                Self::parse(&s).map_err(|_| "invalid_identifier".to_owned())
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

identifier!(Ns, is_ns);
identifier!(KindName, is_kind_name);
identifier!(RelName, is_rel_name);

/// One ontology file (engine-spec §3.1): `ontology`, `version`, `kinds` and `rels` are required
/// by the meta-schema; every other block is optional. Bodies are plain serde data.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OntologyFile {
    /// The file's namespace.
    pub ontology: Ns,
    /// Schema version of the ontology (`>= 1`).
    pub version: u32,
    /// Namespaces whose kinds and rels are visible to bare names in this file.
    #[serde(default)]
    pub extends: Vec<Ns>,
    /// `LinkML` export prefixes; opaque strings, never resolved (EN-S11).
    #[serde(default)]
    pub prefixes: BTreeMap<Ns, String>,
    /// Reusable kind bodies.
    #[serde(default)]
    pub mixins: BTreeMap<KindName, KindBody>,
    /// The kinds this file declares.
    pub kinds: BTreeMap<KindName, KindBody>,
    /// The relationship types this file declares.
    pub rels: BTreeMap<RelName, RelBody>,
    /// Source-type to kinds mapping (EN-F36, EN-F45).
    #[serde(default)]
    pub source_types: BTreeMap<String, SourceType>,
    /// Fallback kind when no source type maps (EN-F36).
    #[serde(default)]
    pub default_kind: Option<String>,
    /// Question templates (EN-F47).
    #[serde(default)]
    pub questions: BTreeMap<String, QuestionTemplate>,
}

/// A kind or mixin body. Every key is optional in the struct (a mixin carries a subset); the
/// resolver requires `description` on a kind and refuses `is_a`/`mixins` on a mixin.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindBody {
    /// Human description.
    #[serde(default)]
    pub description: Option<String>,
    /// Single-inheritance parent (qualified or bare).
    #[serde(default)]
    pub is_a: Option<String>,
    /// Mixins applied in list order after `is_a`.
    #[serde(default)]
    pub mixins: Vec<String>,
    /// EN-F3 subset schema of `props`; default `{type: object, additionalProperties: false}`.
    #[serde(default)]
    pub props_schema: Option<Value>,
    /// The kind's record rules: display labels and the near-duplicate key (EN-F4).
    #[serde(default)]
    pub rules: Option<Rules>,
    /// Rank hints (EN-F29).
    #[serde(default)]
    pub rank: Option<RankHints>,
    /// Extra prop paths folded into FTS (`Some` when declared, an empty list included: a
    /// mixin's declaration counts for the conflict rule, E1.3a RE1-5).
    #[serde(default)]
    pub fts_fields: Option<Vec<String>>,
    /// Prop paths indexed as whole names and code identifier parts (`Some` when declared).
    #[serde(default)]
    pub ident_fields: Option<Vec<String>>,
    /// BM25F column weights.
    #[serde(default)]
    pub column_weights: Option<ColumnWeights>,
    /// Extraction hints (EN-F25, EN-F36).
    #[serde(default)]
    pub extract: Option<Extract>,
}

/// Display labels.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    /// Singular label.
    pub label: String,
    /// Plural label.
    pub plural: String,
    /// Optional icon.
    #[serde(default)]
    pub icon: Option<String>,
}

/// Near-duplicate key rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
pub enum Dedup {
    /// `title+scope`
    #[serde(rename = "title+scope")]
    TitleScope,
    /// `none` (the default)
    #[default]
    #[serde(rename = "none")]
    None,
}

/// The record rules of a kind (EN-F4): how records of the kind are labelled and when two of
/// them are near-duplicates. Per kind, never inherited through `is_a` (a mixin's block counts
/// once, OD-E-2); closed like every other mapping of the file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    /// Display labels, when the kind has them.
    #[serde(default)]
    pub display: Option<Display>,
    /// Near-duplicate key rule (`none` when absent).
    #[serde(default)]
    pub dedup: Dedup,
}

/// Rank hints (EN-F29).
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RankHints {
    /// Score multiplier.
    #[serde(default)]
    pub boost: Option<f64>,
    /// Rels expanded during recall.
    #[serde(default)]
    pub expand_rels: Vec<String>,
    /// Expansion depth (`0..=4`).
    #[serde(default)]
    pub expand_depth: Option<u8>,
}

/// BM25F column weights (EN-F29).
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnWeights {
    /// `text`
    #[serde(default)]
    pub text: Option<f64>,
    /// `summary`
    #[serde(default)]
    pub summary: Option<f64>,
    /// `tags`
    #[serde(default)]
    pub tags: Option<f64>,
    /// `ident_terms`
    #[serde(default)]
    pub ident_terms: Option<f64>,
}

/// Extraction hints (EN-F25, EN-F36).
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Extract {
    /// Extraction on.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Extractor labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Prop paths feeding the gazetteer.
    #[serde(default)]
    pub gazetteer: Vec<String>,
}

/// `on_link` behaviour of a rel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnLink {
    /// Archive the target when the edge is written.
    ArchiveTarget,
}

/// A relationship body. `from`, `to` and `directed` are required by the meta-schema; they are
/// optional in the struct so a malformed body is located key by key, and the resolver requires
/// them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelBody {
    /// Allowed source kinds (empty = any).
    #[serde(default)]
    pub from: Option<Vec<String>>,
    /// Allowed target kinds or `path` (empty = any).
    #[serde(default)]
    pub to: Option<Vec<String>>,
    /// Directed edge.
    #[serde(default)]
    pub directed: Option<bool>,
    /// Human description.
    #[serde(default)]
    pub description: Option<String>,
    /// Lifecycle side effect.
    #[serde(default)]
    pub on_link: Option<OnLink>,
}

/// One `source_types` entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceType {
    /// Kinds an extractor may fill for this type.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// Path globs that select the type.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A question's grouping (EN-F47): the closed vocabulary `dup_key | cluster | prop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// By the near-duplicate key.
    DupKey,
    /// By cluster.
    Cluster,
    /// By a property.
    Prop,
}

/// One question template (EN-F47); `kinds`, `where` and `order` are required by the resolver,
/// optional in the struct so a malformed body is located key by key.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionTemplate {
    /// The target kind list.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// Predicate block: a mapping of plain data.
    #[serde(default, rename = "where")]
    pub where_: Option<serde_json::Map<String, Value>>,
    /// Rel excluding a record from the answer.
    #[serde(default)]
    pub exclude_via: Option<String>,
    /// Grouping.
    #[serde(default)]
    pub group_by: Option<GroupBy>,
    /// Window in days.
    #[serde(default)]
    pub window: Option<u32>,
    /// Ordering keys.
    #[serde(default)]
    pub order: Option<Vec<String>>,
    /// Depth (`0..=3`).
    #[serde(default)]
    pub depth: Option<u8>,
}

/// A file-form failure at `path` (dotted, never a value) as the EN-F7 text
/// `schema_invalid:file:<path>`.
fn file_error(path: &str) -> OntologyError {
    OntologyError::SchemaInvalid {
        rule: format!("file:{path}"),
    }
}

impl OntologyFile {
    /// Load one ontology file from its YAML text: the E1.1 validator against the meta-schema
    /// first (top-level shape), identifiers next, then the typed parse.
    ///
    /// # Errors
    /// [`OntologyError::SchemaInvalid`] `file:<path>` for a malformed document, an unknown key
    /// at any depth (the removed governance keys included), a wrong type or a missing required
    /// key;
    /// [`OntologyError::InvalidIdentifier`] for a namespace, kind or rel name failing its regex.
    pub fn from_yaml(text: &str) -> Result<OntologyFile, OntologyError> {
        let data: Value = yaml_serde::from_str(text).map_err(|_| file_error(""))?;
        let errors = validate(meta_schema(), &data);
        if let Some(text) = first_error_text(&errors) {
            // `invalid_props props.discovered.cap` -> `discovered.cap`
            let path = text
                .strip_prefix("invalid_props props")
                .map(|p| p.trim_start_matches('.'))
                .unwrap_or_default();
            return Err(file_error(path));
        }
        check_identifiers(&data)?;
        serde_json::from_value::<OntologyFile>(data.clone())
            .map_err(|_| file_error(&locate_in_file(&data)))
    }
}

/// The engine-spec §3.1 admission ranges of one file (E1.3a RE1-4), checked by the resolver
/// before anything is merged: `version >= 1`; `rank.expand_depth <= 4`; question `depth <= 3`.
///
/// # Errors
/// [`OntologyError::OutOfRange`] with the dotted path of the first offending field.
pub fn check_ranges(file: &OntologyFile) -> Result<(), OntologyError> {
    let out_of_range = |path: String| OntologyError::OutOfRange { path };
    if file.version < 1 {
        return Err(out_of_range("version".to_owned()));
    }
    for (block, bodies) in [("mixins", &file.mixins), ("kinds", &file.kinds)] {
        for (name, body) in bodies {
            if body
                .rank
                .as_ref()
                .and_then(|r| r.expand_depth)
                .is_some_and(|d| d > 4)
            {
                return Err(out_of_range(format!("{block}.{name}.rank.expand_depth")));
            }
        }
    }
    for (name, template) in &file.questions {
        if template.depth.is_some_and(|d| d > 3) {
            return Err(out_of_range(format!("questions.{name}.depth")));
        }
    }
    Ok(())
}

/// Every identifier the newtypes would refuse, reported as `invalid_identifier` with the field's
/// dotted path before the typed parse (the first in document order).
fn check_identifiers(data: &Value) -> Result<(), OntologyError> {
    let ns = data["ontology"].as_str().unwrap_or_default();
    Ns::parse(ns).map_err(|e| e.at("ontology"))?;
    if let Some(items) = data["extends"].as_array() {
        for item in items.iter().filter_map(Value::as_str) {
            Ns::parse(item).map_err(|e| e.at("extends"))?;
        }
    }
    if let Some(map) = data["prefixes"].as_object() {
        for key in map.keys() {
            Ns::parse(key).map_err(|e| e.at(format!("prefixes.{key}")))?;
        }
    }
    for block in ["mixins", "kinds"] {
        if let Some(map) = data[block].as_object() {
            for key in map.keys() {
                KindName::parse(key).map_err(|e| e.at(format!("{block}.{key}")))?;
            }
        }
    }
    if let Some(map) = data["rels"].as_object() {
        for key in map.keys() {
            RelName::parse(key).map_err(|e| e.at(format!("rels.{key}")))?;
        }
    }
    Ok(())
}

/// The dotted path of the key that makes the typed parse fail: each body under the keyed
/// blocks is tried alone with its own type, and the failing one is narrowed key by key.
fn locate_in_file(data: &Value) -> String {
    let Some(root) = data.as_object() else {
        return String::new();
    };
    for (block, child) in root {
        let Some(bodies) = child.as_object() else {
            continue;
        };
        let located = match block.as_str() {
            "mixins" | "kinds" => first_failing::<KindBody>(block, bodies),
            "rels" => first_failing::<RelBody>(block, bodies),
            "source_types" => first_failing::<SourceType>(block, bodies),
            "questions" => first_failing::<QuestionTemplate>(block, bodies),
            "prefixes" => bodies
                .iter()
                .find(|(_, v)| !v.is_string())
                .map(|(k, _)| format!("{block}.{k}")),
            _ => None,
        };
        if let Some(path) = located {
            return path;
        }
    }
    String::new()
}

fn first_failing<T: DeserializeOwned>(
    block: &str,
    bodies: &serde_json::Map<String, Value>,
) -> Option<String> {
    bodies
        .iter()
        .find(|(_, body)| !deserializes::<T>(body))
        .map(|(name, body)| {
            let inner = locate::<T>(body);
            if inner.is_empty() {
                format!("{block}.{name}")
            } else {
                format!("{block}.{name}.{inner}")
            }
        })
}

fn deserializes<T: DeserializeOwned>(v: &Value) -> bool {
    serde_json::from_value::<T>(v.clone()).is_ok()
}

/// The relative dotted path of the offending key inside `body` (which fails as `T`): the
/// deepest key anywhere in the tree whose removal alone makes the body deserialize is the
/// culprit (an unknown key, or an optional block holding the fault; a required mapping cannot
/// be removed, so the search descends into it regardless); when
/// the culprit is a mapping with no single removable key inside, or nothing is removable, the
/// `missing field` / `unknown field` serde names is appended. Never a value.
fn locate<T: DeserializeOwned>(body: &Value) -> String {
    let mut path: Vec<String> = Vec::new();
    let mut found: Option<Vec<String>> = None;
    deepest_removable::<T>(body, &mut path, &mut found);
    let mut path = found.unwrap_or_default();
    let ends_in_mapping = node_at(body, &path).is_none_or(Value::is_object);
    if ends_in_mapping
        && let Err(e) = serde_json::from_value::<T>(body.clone())
        && let Some(field) = named_field(&e.to_string())
        && path.last() != Some(&field)
    {
        if let Some(owner) = mapping_lacking::<T>(body, &path, &field) {
            path = owner;
        }
        path.push(field);
    }
    path.join(".")
}

/// The mapping at or below `from` that the `missing field` named `field` belongs to: the first
/// one (depth-first) where inserting `field: null` changes serde's complaint to something other
/// than that field being missing or unknown (a mapping that merely does not allow the key
/// answers "unknown field"). `None` when no mapping qualifies.
fn mapping_lacking<T: DeserializeOwned>(
    body: &Value,
    from: &[String],
    field: &str,
) -> Option<Vec<String>> {
    let node = node_at(body, from)?.as_object()?;
    let mut probe = body.clone();
    if let Some(target) = node_at_mut(&mut probe, from).and_then(Value::as_object_mut) {
        target.insert(field.to_owned(), Value::Null);
    }
    let still_named = match serde_json::from_value::<T>(probe) {
        Ok(_) => false,
        Err(e) => named_field(&e.to_string()).is_some_and(|named| named == field),
    };
    if !still_named {
        return Some(from.to_vec());
    }
    for (key, child) in node {
        if child.is_object() {
            let mut deeper = from.to_vec();
            deeper.push(key.clone());
            if let Some(owner) = mapping_lacking::<T>(body, &deeper, field) {
                return Some(owner);
            }
        }
    }
    None
}

/// Depth-first over the mappings of `body`: record the longest path whose single removal makes
/// `body` deserialize as `T`.
fn deepest_removable<T: DeserializeOwned>(
    body: &Value,
    path: &mut Vec<String>,
    found: &mut Option<Vec<String>>,
) {
    let Some(node) = node_at(body, path).and_then(Value::as_object) else {
        return;
    };
    for (key, child) in node {
        path.push(key.clone());
        let mut probe = body.clone();
        if let Some(parent) =
            node_at_mut(&mut probe, &path[..path.len() - 1]).and_then(Value::as_object_mut)
        {
            parent.remove(key.as_str());
        }
        if deserializes::<T>(&probe) && found.as_ref().is_none_or(|f| f.len() < path.len()) {
            *found = Some(path.clone());
        }
        if child.is_object() {
            deepest_removable::<T>(body, path, found);
        }
        path.pop();
    }
}

fn node_at<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(root, |node, key| node.get(key))
}

fn node_at_mut<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    path.iter().try_fold(root, |node, key| node.get_mut(key))
}

/// The backticked field of a serde `missing field` or `unknown field` message (the only two
/// messages that name a key; neither carries a value).
fn named_field(message: &str) -> Option<String> {
    message
        .strip_prefix("missing field `")
        .or_else(|| message.strip_prefix("unknown field `"))
        .and_then(|rest| rest.split('`').next())
        .map(str::to_owned)
}
