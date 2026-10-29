//! `schema_describe` and `schema_extend`.

use kaban_core::TypeDef;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `schema_extend` input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SchemaExtendRequest {
    /// "label" (node kind) or "rel" (relationship type)
    pub kind: String,
    /// `PascalCase` for labels, `UPPER_SNAKE_CASE` for rels.
    pub name: String,
    /// Human description (required).
    pub description: String,
    /// For rels: allowed source kinds (empty = any).
    #[serde(default)]
    pub from_kinds: Vec<String>,
    /// For rels: allowed destination kinds (empty = any).
    #[serde(default)]
    pub to_kinds: Vec<String>,
    /// Directed relationship (default true).
    pub directed: Option<bool>,
    /// Optional JSON schema for props.
    pub props_schema: Option<Value>,
}

/// Naming conventions echoed by `schema_describe`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Naming {
    /// Label convention.
    pub label: String,
    /// Relationship type convention.
    pub rel: String,
}

/// `schema_describe` output (also the `kaban://schema` resource).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SchemaResponse {
    /// Node kinds.
    pub labels: Vec<TypeDef>,
    /// Relationship types.
    pub relationships: Vec<TypeDef>,
    /// Naming conventions.
    pub naming: Naming,
    /// The discovered-kind counters of the request's workspace (engine-spec EN-F6; engine-plan
    /// E1.8), empty when no label was ever discovered there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kind_stats: Vec<kaban_core::KindStat>,
}
