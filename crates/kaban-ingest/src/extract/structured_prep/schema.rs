//! Ontology-derived kind selection and props fragments, not a wire-schema generator.

use std::collections::BTreeSet;

use crate::GatedPath;
use kaban_core::ontology::resolve::ResolvedKind;
use kaban_core::ontology::{Ontology, Qualified, stored_name};
use kaban_core::{Schema, SchemaError, ValidatorLimits};

/// Internal root properties never offered to an external extractor.
pub const INTERNAL_PROPERTIES: [&str; 5] = [
    "evidence",
    "pending_fix",
    "command_fingerprint",
    "evidence_runs",
    "accepted_commit",
];

/// Preparation refusal categories; never include source content in error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    /// A mapping/default exists, but offers no enabled resolved kind.
    #[error("no_extractable_kinds")]
    NoExtractableKinds,
    /// No resolved default or shipped Fact exists in this ontology.
    #[error("missing_default_kind")]
    MissingDefaultKind,
    /// An ontology path claim is not a supported glob.
    #[error("invalid_source_path_glob")]
    InvalidPathGlob,
}

/// Union mappings across namespaces, sorting/deduplicating qualified identities.
/// Only a missing mapping may fall back to resolved defaults (or shipped `adr:Fact`).
/// A mapped empty/all-disabled set refuses instead of reviving an excluded kind.
///
/// # Errors
/// Refuses missing defaults or a selection with no enabled resolved kinds.
pub fn select_kinds(
    ontology: &Ontology,
    source_type: &str,
) -> Result<Vec<Qualified>, SelectionError> {
    let mut mapped = false;
    let mut kinds = BTreeSet::new();
    for types in ontology.source_types.values() {
        if let Some(mapping) = types.get(source_type) {
            mapped = true;
            kinds.extend(mapping.kinds.iter().cloned());
        }
    }
    if !mapped {
        kinds.extend(ontology.default_kind.values().cloned());
        if kinds.is_empty() {
            kinds.extend(
                ontology
                    .kinds
                    .keys()
                    .filter(|q| q.ns.as_str() == "adr" && q.name == "Fact")
                    .cloned(),
            );
        }
        if kinds.is_empty() {
            return Err(SelectionError::MissingDefaultKind);
        }
    }
    kinds.retain(|id| {
        ontology.kinds.get(id).is_some_and(|kind| {
            kind.extract.as_ref().and_then(|extract| extract.enabled) != Some(false)
        })
    });
    if kinds.is_empty() {
        return Err(SelectionError::NoExtractableKinds);
    }
    Ok(kinds.into_iter().collect())
}

/// Canonical stored names in qualified-identity order (not a second name registry).
#[must_use]
pub fn stored_names(kinds: &[Qualified]) -> Vec<String> {
    kinds.iter().map(stored_name).collect()
}

/// Source types whose ontology globs claim the gated relative path. Suggestions only:
/// neither matching nor a `GatedPath` grants filesystem capability or acceptance authority.
///
/// # Errors
/// Refuses any invalid ontology source-path glob with a fixed category.
pub fn path_claims(ontology: &Ontology, path: &GatedPath) -> Result<Vec<String>, SelectionError> {
    let mut claims = BTreeSet::new();
    for types in ontology.source_types.values() {
        for (name, mapping) in types {
            for pattern in &mapping.paths {
                let glob =
                    globset::Glob::new(pattern).map_err(|_| SelectionError::InvalidPathGlob)?;
                if glob.compile_matcher().is_match(path.relative_path()) {
                    claims.insert(name.clone());
                }
            }
        }
    }
    Ok(claims.into_iter().collect())
}

/// Clone and project one resolved props fragment, stripping only internal root properties
/// and matching required entries. Preserve all other semantics, including permissive Fact.
/// Reparse under the caller's existing validator limits; this is not full EN-F43 wire lint.
///
/// # Errors
/// Returns the existing validator's schema refusal, including pattern-limit failures.
pub fn props_fragment(
    kind: &ResolvedKind,
    limits: &ValidatorLimits,
) -> Result<Schema, SchemaError> {
    let mut value = kind.props_schema.as_value().clone();
    if let Some(properties) = value
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        for name in INTERNAL_PROPERTIES {
            properties.remove(name);
        }
    }
    if let Some(required) = value
        .get_mut("required")
        .and_then(serde_json::Value::as_array_mut)
    {
        required.retain(|name| {
            !name
                .as_str()
                .is_some_and(|name| INTERNAL_PROPERTIES.contains(&name))
        });
    }
    Schema::parse(&value, limits)
}
