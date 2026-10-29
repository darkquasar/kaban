//! `remember` and `link`: the write surface of the memory tools.

use super::WriteReceipt;
use super::card::Card;
use kaban_core::{BodyLocation, Location, StoreScope};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `remember` input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RememberRequest {
    /// Node kind (label). Must exist in the schema; see `schema_describe` / `schema_extend`.
    pub kind: String,
    /// The memory text. Scanned by the denylist before anything is stored.
    pub text: String,
    /// Optional one-line summary used for compact recall cards.
    pub summary: Option<String>,
    /// Free-form tags (max 32).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Code references the memory is about, stored as `CodeRef` nodes linked `ABOUT` (deduplicated
    /// by content). Serena output can be converted with the `coderefs_from_serena` adapter.
    #[serde(default)]
    pub refs: Vec<CodeRefWire>,
    /// Which store the memory lives in: `project` (default). `personal` arrives in M6.
    pub scope: Option<StoreScope>,
    /// Globs or paths a governing record (Decision, Convention) covers.
    #[serde(default)]
    pub path_scope: Vec<String>,
    /// 0..1 how sure you are.
    pub confidence: Option<f64>,
    /// 0..1 how important this is to recall later.
    pub importance: Option<f64>,
    /// Workspace the memory belongs to.
    pub workspace_id: Option<String>,
    /// Session that produced the memory (`source_session` is accepted as an alias, MD-26).
    #[serde(alias = "source_session")]
    pub session_id: Option<String>,
    /// Agent that produced the memory.
    pub agent_id: Option<String>,
    /// Kind-specific properties (every string leaf is scanned by the denylist). Decision,
    /// Convention and Failure props are typed: unknown keys are rejected and a Decision needs
    /// `rationale`.
    pub props: Option<Value>,
    /// Links created atomically with the node: `[{"rel":"FIXED_BY","to":"<id>"}]` or `{"rel":..,"from":"<id>"}`.
    #[serde(default)]
    pub links: Vec<LinkSpec>,
}

/// A code reference on the wire, exactly as core-spec §6 defines it (Serena field names;
/// nested `location` and `body_location`, never a flat `line`). `info` is scanned by the
/// denylist and then dropped; `body`, `content_around_reference` and `children` are not accepted
/// here (the Serena adapter drops them before anything else).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeRefWire {
    /// Repository identity.
    pub repo: String,
    /// Commit the reference was taken at.
    pub commit: String,
    /// Tool of origin: `serena`, `manual` (agent-typed), `kaban-adr`.
    pub tool: String,
    /// Path relative to the repository root (backslashes are normalised to `/`).
    pub relative_path: String,
    /// Serena-style symbol path (`Class/method`; the `[i]` overload suffix is kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    /// `SymbolKind` enum name, e.g. `"Function"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Body span `{start_line, end_line}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_location: Option<BodyLocation>,
    /// Position `{line, column}`, 0-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// Line of a reference site, 0-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_line: Option<u32>,
    /// SCIP symbol (stage1-spec G1-F9): content, scanned first, then grammar-checked
    /// (`symbol_invalid`); derived from `relative_path` and `name_path` when absent and a
    /// snapshot resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Snapshot commit, 7 to 64 lowercase hex (SO-2): a short value is canonicalised to the
    /// 40-hex commit it uniquely prefixes; stored as 40 hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_commit: Option<String>,
    /// When the reference was observed, RFC 3339 (stage1-spec §4.4); `remember` sets it when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Free text about the symbol: scanned, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
}

/// An inline link in a `remember` call; exactly one of `to` / `from` names the other node.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LinkSpec {
    /// Relationship type.
    pub rel: String,
    /// Destination node id (the new node is the source).
    pub to: Option<String>,
    /// Source node id (the new node is the destination).
    pub from: Option<String>,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
}

/// `link` input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LinkRequest {
    /// Source node id.
    pub from: String,
    /// Relationship type (must be registered).
    pub rel: String,
    /// Destination node id.
    pub to: String,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
    /// Arbitrary JSON properties (scanned by the denylist).
    pub props: Option<Value>,
}

/// Whether `remember` created a node or found an existing duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RememberStatus {
    /// A new node was stored.
    Created,
    /// An active node with identical content already existed; nothing was stored.
    Existing,
}

/// `remember` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RememberResponse {
    /// Created or existing.
    pub status: RememberStatus,
    /// Node id.
    pub id: String,
    /// Node kind.
    pub kind: String,
    /// `field:rule` entries for every redaction applied.
    pub redactions: Vec<String>,
    /// Number of inline links created (only when `created`; kept for the spike contract).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<usize>,
    /// Number of requested links created or refreshed, on a new or an existing node (m2 N5).
    #[serde(default)]
    pub links_created: usize,
    /// Always `false`: links are never dropped on dedup (m2 N5). Present for the contract.
    #[serde(default)]
    pub links_ignored: bool,
    /// On `existing`: request fields whose values differed from the stored node and were not
    /// applied (the first write wins).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields_ignored: Vec<String>,
    /// Active Decisions of the workspace with the same normalized title and `path_scope` (F1
    /// near-duplicates); the node was still created, nothing is merged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_duplicates: Vec<Card>,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog` position.
    pub receipt: WriteReceipt,
}

/// `link` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkResponse {
    /// Edge id.
    pub id: String,
    /// Source node id.
    pub from: String,
    /// Relationship type.
    pub rel: String,
    /// Destination node id.
    pub to: String,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
    /// `field:rule` entries for every redaction applied.
    pub redactions: Vec<String>,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog` position.
    pub receipt: WriteReceipt,
}
