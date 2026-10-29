//! The typed `code_*` tools over the resolved code-graph snapshot (stage1-spec §6, G1-F12,
//! G1-S2, G1-S8; stage1-plan CG7.1): the request and response shapes of §6 with the field
//! classes of §6.1, the closed [`CodeError`] classes, and the read pipeline every handler
//! follows (`why.rs` is the template):
//!
//! ```text
//!   Code*Request
//!     | gate every content string under the tool name (S1), flush the audit entries
//!     v
//!   KabanApi::resolve_current  (head.rs: rev-parse HEAD ==> HeadCache by sha ==> SnapshotProvider)
//!     | NoSnapshot | SnapshotTooNew
//!     v
//!   selector::resolve(target, reader)  ==> One(row) | Many(rows)        (selector.rs)
//!     v
//!   reader.implementers | members | references | tests_for | neighbours | the row itself
//!     v                                                       (complete sets, RV-7)
//!   cards::project(row) each ==> ranking::order_then_limit (every candidate's distance from
//!     v                            current_path, sort by (distance, path, ref), then cut; G1-F16)
//!   CodeListResponse { snapshot, count, results, truncated, ranking: "full", follow_ups: [] }
//!   CodeReferencesResponse | CodeTestsForResponse | CodeNeighboursResponse (CG7.2, §6 shapes)
//! ```
//!
//! Cards are not re-scanned here: the adapter's `gate_response` is the one read gate for these
//! tools (snapshot strings were gated at index time; `policy: stale` says when the live policy
//! differs). `current_path` (CG8.1) is gated, then checked by the repository-relative path rule
//! before any lookup (`selector_invalid`), then read by the ranking: `distance` is the G1-F16
//! path distance of every card, 0 without it. `include_snippets` (CG8.2, G1-F17) puts the
//! stored snippet on every card of a list tool; `code_definition` returns it always.

mod cards;
mod context;
mod coverage;
mod definition;
mod head;
mod implementers;
mod index;
mod members;
mod neighbours;
mod ranking;
mod references;
mod selector;
mod tests_for;

pub use coverage::{CoverageIndex, CoverageSummary, CrateCoverage};
pub(crate) use head::HeadSource;
pub use index::CodeIndexResponse;

use crate::api::KabanApi;
use crate::control::ControlPath;
use crate::enrich::FollowUp;
use crate::error::ApiError;
use crate::types::WhyCard;
use kaban_codegraph::{CodeGraphReader, ReadError, Resolved, SnapshotRef};
use kaban_core::WriteCtx;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The target of a `code_*` tool (stage1-spec §6, G1-F12, RS-12): exactly one of `symbol`,
/// `ref`, `{label, name}` (with an optional `path` keeping one file), `{path, line}` or `path`
/// (where the tool allows it); anything else is `selector_invalid`. Name lookups are exact and
/// case-sensitive; a name matching several nodes is `ambiguous_symbol` with the candidates
/// (Q-2), never a merged answer; a wrong label corrects to the one other kind of that name
/// (`target_note.label_corrected`) or suggests `did_you_mean`; an external placeholder
/// (a dependency's trait) answers by name whatever label it lacks (`target_note.external`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolSelector {
    /// The normalised cross-commit SCIP key (content; grammar-checked after the scan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The exact row identity in this snapshot: `scip:<scip>`, `local:<path>:<scip>` or
    /// `file:<path>` (content; the discriminated parse runs after the scan).
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// A SCIP kind name or `File`, together with `name` (control: closed enum).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The exact display name, together with `label` (content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A repository-relative path: with `line` the nodes defined at that line, with `{label,
    /// name}` the file the named node must be defined in, alone the file node (content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A 0-based line in `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// `code_implementers` input (stage1-spec §6).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CodeImplementersRequest {
    /// The interface or type whose `IMPLEMENTS` sources are listed (`path` alone is not
    /// allowed here).
    pub target: SymbolSelector,
    /// Include implementers defined outside the repository (default false).
    #[serde(default)]
    pub include_external: bool,
    /// Max rows (default 50, max 200); `truncated: true` says more existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// The caller's current file, a repository-relative path: results are ranked by path
    /// distance from it (content; G1-F16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
    /// Include each card's snippet (G1-F17).
    #[serde(default)]
    pub include_snippets: bool,
}

/// `code_members` input (stage1-spec §6): direct `CONTAINS` children.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CodeMembersRequest {
    /// The enclosing symbol or file.
    pub target: SymbolSelector,
    /// Keep only members with these labels (SCIP kind names; empty keeps every member).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Max rows (default 50, max 200); `truncated: true` says more existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// The caller's current file, a repository-relative path: results are ranked by path
    /// distance from it (content; G1-F16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
    /// Include each card's snippet (G1-F17).
    #[serde(default)]
    pub include_snippets: bool,
}

/// `code_definition` input (stage1-spec §6): one node with its definition range, signature
/// and doc.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CodeDefinitionRequest {
    /// The node (a selector matching several nodes is `ambiguous_symbol`).
    pub target: SymbolSelector,
    /// The caller's current file, a repository-relative path: the card's `distance` is measured
    /// from it (content; G1-F16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
}

/// `code_references` input (stage1-spec §6): the reference sites of the target, grouped by
/// enclosing node.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CodeReferencesRequest {
    /// The referenced symbol or file.
    pub target: SymbolSelector,
    /// Include sites in test files (default false).
    #[serde(default)]
    pub include_tests: bool,
    /// Max sites (default 50, max 200); `truncated: true` says more existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// The caller's current file, a repository-relative path: groups are ranked by the path
    /// distance of their enclosing node from it (content; G1-F16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
    /// Include each card's snippet (G1-F17).
    #[serde(default)]
    pub include_snippets: bool,
}

/// `code_tests_for` input (stage1-spec §6, G1-F6): the `TESTED_BY` targets of a symbol (the
/// reference form) or of a file named by `path` (the naming form).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CodeTestsForRequest {
    /// The symbol under test, or the file (`path`) whose test file is wanted.
    pub target: SymbolSelector,
    /// Max rows (default 50, max 200); `truncated: true` says more existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// The caller's current file, a repository-relative path: links are ranked by the path
    /// distance of the test from it (content; G1-F16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
    /// Include each card's snippet (G1-F17).
    #[serde(default)]
    pub include_snippets: bool,
}

/// The `depth` of a `code_neighbours` call that names none: one hop.
const DEFAULT_DEPTH: u8 = 1;

fn default_depth() -> u8 {
    DEFAULT_DEPTH
}

/// `code_neighbours` input (stage1-spec §6, G1-F12): a bounded walk from exactly one node.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeNeighboursRequest {
    /// The start node (a selector matching several nodes is `ambiguous_symbol`).
    pub target: SymbolSelector,
    /// Hops from the start, `1..=3` (default 1); any other value is `selector_invalid`.
    #[serde(default = "default_depth")]
    pub depth: u8,
    /// Keep only edges of these types (the closed edge-type names, for example
    /// `["IMPLEMENTS", "TESTED_BY"]`; empty keeps every type).
    #[serde(default)]
    pub edge_types: Vec<String>,
    /// Also walk to document-local symbols (parameters and variables, `local N` named by the
    /// id); by default they are hidden and counted in `locals_hidden` (G4).
    #[serde(default)]
    pub include_locals: bool,
    /// Max nodes after the start (default 50, max 200); an edge is kept when both of its ends
    /// survive the cut, `truncated: true` says the cut or a traversal bound dropped something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// The caller's current file, a repository-relative path: every node's `distance` is
    /// measured from it; the `(depth, ref)` order stands (content; G1-F16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
    /// Include each card's snippet (G1-F17).
    #[serde(default)]
    pub include_snippets: bool,
}

impl Default for CodeNeighboursRequest {
    fn default() -> Self {
        Self {
            target: SymbolSelector::default(),
            depth: DEFAULT_DEPTH,
            edge_types: Vec::new(),
            include_locals: false,
            limit: None,
            current_path: None,
            include_snippets: false,
        }
    }
}

/// `code_context` input (stage1-spec §6, G1-F14): one node and everything an agent asks about
/// it first, in one call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CodeContextRequest {
    /// The node (a selector matching several nodes is `ambiguous_symbol`).
    pub target: SymbolSelector,
    /// The caller's current file, a repository-relative path: every card's `distance` is
    /// measured from it and the legs are ranked by it (content; G1-F16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
}

/// A source range in SCIP coordinates: 0-based lines and columns, end exclusive
/// (stage1-spec §6 `Range`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeRange {
    /// Start line.
    pub start_line: u32,
    /// Start column.
    pub start_col: u32,
    /// End line.
    pub end_line: u32,
    /// End column.
    pub end_col: u32,
}

impl From<kaban_codegraph::Range> for CodeRange {
    fn from(r: kaban_codegraph::Range) -> Self {
        Self {
            start_line: r.sl,
            start_col: r.sc,
            end_line: r.el,
            end_col: r.ec,
        }
    }
}

/// One node card (stage1-spec §6 `CodeNode`). Field classes per §6.1: `kind`, `label` and
/// `language` are control (closed enums), every other string is content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeNode {
    /// `symbol | local | file`.
    pub kind: String,
    /// Exact row identity in this snapshot (pass it back as `target.ref`).
    #[serde(rename = "ref")]
    pub r#ref: String,
    /// The normalised cross-commit key, `kind = symbol` and not external only (G1-F9); pass
    /// it to `remember` or back as `target.symbol`.
    pub symbol: Option<String>,
    /// The versioned SCIP symbol as the indexer emitted it.
    pub raw: Option<String>,
    /// SCIP kind name or `File`.
    pub label: String,
    /// Display name.
    pub name: String,
    /// Repository-relative path of the defining file (`null` for an external placeholder).
    pub path: Option<String>,
    /// `go | typescript | javascript`.
    pub language: Option<String>,
    /// Definition range.
    pub range: Option<CodeRange>,
    /// Enclosing range (the body).
    pub enclosing: Option<CodeRange>,
    /// Signature text.
    pub signature: Option<String>,
    /// Documentation text.
    pub doc: Option<String>,
    /// Source snippet (G1-F17): `code_definition` always, the list tools with `include_snippets`; `null` when the snapshot holds none.
    pub snippet: Option<String>,
    /// Defined outside the repository.
    pub external: bool,
    /// Defined in a test file.
    pub is_test: bool,
    /// Path distance from the request's `current_path` (G1-F16): 0 for the same file (and
    /// without `current_path`, or for a card without a path), 1 for the same directory, else 2
    /// plus the directory components of both outside their longest common prefix.
    pub distance: u32,
}

/// How the name selector reached its node when not by an exact `(label, name)` match (Q-2,
/// backlog 21; R-3 consumer request 2): the one node of another label that carries the
/// name (`label_corrected` names the label that answered), or the external placeholder matched
/// by name (`external`). A response carries it only in those two cases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TargetNote {
    /// The node that answered (pass it back as `target.ref`).
    #[serde(rename = "ref")]
    pub r#ref: String,
    /// The label that answered when the request named another one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_corrected: Option<String>,
    /// The node is an external placeholder (defined outside the repository, label unknown).
    pub external: bool,
}

/// The resolved snapshot every `code_*` response reports (stage1-spec G1-F8, §6
/// `SnapshotRef`; cg-join-v1 §4): the wire form of [`kaban_codegraph::SnapshotRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotRefWire {
    /// The snapshot's commit, 40 lowercase hex.
    pub commit: String,
    /// Whether it is the snapshot of the current `HEAD` (else the nearest ancestor's).
    pub exact: bool,
    /// The snapshot's schema major.
    pub schema_version: u32,
    /// `current` when the snapshot was built under the live denylist policy, `off` when the
    /// content scan was disabled at index time (nothing was gated), else `stale`.
    pub policy: String,
    /// The isolation tier the indexer ran under: `namespaces | container | none`.
    pub isolation: String,
}

/// A list tool's result (stage1-spec §6 `CodeListResponse`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeListResponse {
    /// The snapshot the rows come from.
    pub snapshot: SnapshotRefWire,
    /// What the name selector did beyond an exact match (Q-2): the node of another label it
    /// answered (`label_corrected`), or an external placeholder matched by name (`external`);
    /// absent when the target matched as asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_note: Option<TargetNote>,
    /// What the snapshot does not cover (R-1): zeros and no note without coverage rows.
    pub coverage: CoverageSummary,
    /// Rows returned.
    pub count: usize,
    /// The cards, ranked (G1-F16) before the limit was applied.
    pub results: Vec<CodeNode>,
    /// Rows beyond the limit existed.
    pub truncated: bool,
    /// `full` (the complete candidate set was ranked) or `partial`.
    pub ranking: String,
    /// Suggested next calls (none are synthesised in 3.0.0, OWN-68 SC-09).
    pub follow_ups: Vec<FollowUp>,
}

/// `code_definition`'s result: the one card (mirrors `CodeContextResponse.definition`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeDefinitionResponse {
    /// The snapshot the card comes from.
    pub snapshot: SnapshotRefWire,
    /// What the name selector did beyond an exact match (Q-2): the node of another label it
    /// answered (`label_corrected`), or an external placeholder matched by name (`external`);
    /// absent when the target matched as asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_note: Option<TargetNote>,
    /// What the snapshot does not cover (R-1): zeros and no note without coverage rows.
    pub coverage: CoverageSummary,
    /// The node with its definition range, signature and doc.
    pub definition: CodeNode,
    /// Suggested next calls (none are synthesised in 3.0.0, OWN-68 SC-09).
    pub follow_ups: Vec<FollowUp>,
}

/// One reference site (stage1-spec §6 `ReferenceSite`): the occurrence range in `path` and the
/// names of its SCIP `SymbolRole` bits (`roles[]` is control, the closed `ROLE_VALUES`; `path`
/// is content). Its enclosing node is the group's (CG7.2 default 2: the §6 per-site
/// `enclosing` is carried once, on [`ReferenceGroup`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReferenceSite {
    /// The occurrence range.
    pub site: CodeRange,
    /// Repository-relative path of the document.
    pub path: String,
    /// SCIP `SymbolRole` names of the occurrence (`Definition` never: definition sites are not
    /// references).
    pub roles: Vec<String>,
}

/// The sites of one enclosing node (stage1-spec §6 `by_enclosing`): a symbol, or the file node
/// when the occurrence has no enclosing symbol (G1-F4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReferenceGroup {
    /// The enclosing node's card.
    pub enclosing: CodeNode,
    /// Its sites, in range order.
    pub sites: Vec<ReferenceSite>,
}

/// `code_references`' result (stage1-spec §6 `CodeReferencesResponse`): groups by the enclosing
/// node's `(distance, path, ref)` (the reader's `(path, ref)` order without `current_path`),
/// sites in range order; `count` and the limit count sites.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeReferencesResponse {
    /// The snapshot the sites come from.
    pub snapshot: SnapshotRefWire,
    /// What the name selector did beyond an exact match (Q-2): the node of another label it
    /// answered (`label_corrected`), or an external placeholder matched by name (`external`);
    /// absent when the target matched as asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_note: Option<TargetNote>,
    /// What the snapshot does not cover (R-1): zeros and no note without coverage rows.
    pub coverage: CoverageSummary,
    /// Sites returned, over every group.
    pub count: usize,
    /// The sites grouped by enclosing node.
    pub by_enclosing: Vec<ReferenceGroup>,
    /// Sites beyond the limit existed.
    pub truncated: bool,
    /// Suggested next calls (none are synthesised in 3.0.0, OWN-68 SC-09).
    pub follow_ups: Vec<FollowUp>,
}

/// One test link (stage1-spec §6 `TestLink`, G1-F6): how a `TESTED_BY` edge was derived, and
/// which tier of `code_tests_for` it comes from (TESTS-1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TestLink {
    /// The test function, method or test file node.
    pub test: CodeNode,
    /// `reference` (the test's body references the symbol) or `naming` (file stems match, or
    /// the textual tier's names match).
    pub matched_by: String,
    /// `true` for the naming form and the textual tier, which may be wrong by construction.
    pub heuristic: bool,
    /// The tier.
    pub how: TestTier,
    /// The ref of the caller a `via` link reaches the target through; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// The tier of a test link (TESTS-1; `tests[].how`, a `ctl Enum`): the wire form of
/// [`kaban_codegraph::How`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestTier {
    /// The target's own `TESTED_BY` edge.
    Direct,
    /// A `TESTED_BY` edge of a caller of the target, one `REFERENCES` hop; `via` names the
    /// caller.
    Via,
    /// A test whose path or snippet spells every word of the target's name: a heuristic,
    /// answered only when `direct` and `via` are both empty.
    Textual,
}

impl From<kaban_codegraph::How> for TestTier {
    fn from(how: kaban_codegraph::How) -> Self {
        match how {
            kaban_codegraph::How::Direct => Self::Direct,
            kaban_codegraph::How::Via => Self::Via,
            kaban_codegraph::How::Textual => Self::Textual,
        }
    }
}

/// `code_tests_for`'s result (stage1-spec §6 `CodeTestsForResponse`): links by
/// `(test.distance, test.path, test.ref)` before the cut (G1-F16).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeTestsForResponse {
    /// The snapshot the links come from.
    pub snapshot: SnapshotRefWire,
    /// What the name selector did beyond an exact match (Q-2): the node of another label it
    /// answered (`label_corrected`), or an external placeholder matched by name (`external`);
    /// absent when the target matched as asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_note: Option<TargetNote>,
    /// What the snapshot does not cover (R-1): zeros and no note without coverage rows.
    pub coverage: CoverageSummary,
    /// The links.
    pub tests: Vec<TestLink>,
    /// Links beyond the limit existed.
    pub truncated: bool,
    /// Suggested next calls (none are synthesised in 3.0.0, OWN-68 SC-09).
    pub follow_ups: Vec<FollowUp>,
}

/// One edge of a neighbourhood, by node refs (stage1-spec §6 `Edge`): `from` and `to` are
/// content refs, `type` the closed edge-type name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Edge {
    /// Source ref.
    pub from: String,
    /// `CONTAINS | DEFINED_IN | IMPLEMENTS | IMPLEMENTS_MEMBER | REFERENCES | TESTED_BY | TYPE_OF`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Target ref.
    pub to: String,
}

/// `code_neighbours`' result (stage1-spec §6 `CodeNeighboursResponse`): the start node first,
/// then the nodes reached by `(depth, ref)`, and the edges between the nodes returned.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeNeighboursResponse {
    /// The snapshot the graph comes from.
    pub snapshot: SnapshotRefWire,
    /// What the name selector did beyond an exact match (Q-2): the node of another label it
    /// answered (`label_corrected`), or an external placeholder matched by name (`external`);
    /// absent when the target matched as asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_note: Option<TargetNote>,
    /// What the snapshot does not cover (R-1): zeros and no note without coverage rows.
    pub coverage: CoverageSummary,
    /// The start node, then every node reached, by `(depth, ref)`.
    pub nodes: Vec<CodeNode>,
    /// Every edge between two returned nodes, once, in expansion order.
    pub edges: Vec<Edge>,
    /// The limit or a traversal bound (200 per hub, 500 in total) dropped something.
    pub truncated: bool,
    /// Edges to document-local symbols left out because `include_locals` was not set (G4);
    /// 0 when it was.
    pub locals_hidden: usize,
}

/// The last commit that touched the definition's file (stage1-spec §6 `LastChange`, G1-F14):
/// `git log -1 -- <path>` through the bounded runner. `commit` is control (`Hex`); the date
/// and the summary are content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LastChange {
    /// The commit, 40 lowercase hex.
    pub commit: String,
    /// The author date, RFC 3339 as git prints it (`%aI`).
    pub author_date: String,
    /// The subject line.
    pub summary: String,
}

/// `code_context`'s result (stage1-spec §6 `CodeContextResponse`, G1-F14): the definition
/// card, up to 3 callers and 3 callees over `REFERENCES` (approximate until a language's
/// `CALLS` derivation is proven, stage 2), up to 3 test links, the governing records of
/// `why(symbol)` with their currency and `superseded_by`, and the file's last change (`null`
/// when git is slow, absent or the row has no path: never an error).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeContextResponse {
    /// The snapshot the cards come from.
    pub snapshot: SnapshotRefWire,
    /// What the name selector did beyond an exact match (Q-2): the node of another label it
    /// answered (`label_corrected`), or an external placeholder matched by name (`external`);
    /// absent when the target matched as asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_note: Option<TargetNote>,
    /// What the snapshot does not cover (R-1): zeros and no note without coverage rows.
    pub coverage: CoverageSummary,
    /// The node with its definition range, signature, doc and snippet.
    pub definition: CodeNode,
    /// Up to 3 nodes whose body references the target, by `(distance, path, ref)`.
    pub callers: Vec<CodeNode>,
    /// Up to 3 nodes the target's body references, by `(distance, path, ref)`.
    pub callees: Vec<CodeNode>,
    /// `true`: `callers` and `callees` are `REFERENCES` legs, not a proven call graph.
    pub approximate: bool,
    /// Up to 3 `TESTED_BY` links, by `(test.distance, test.path, test.ref)`.
    pub tests: Vec<TestLink>,
    /// The governing records of the target's symbol key (`why(symbol)`: currency and
    /// `superseded_by` on each); empty for a file, local or external row.
    pub governing: Vec<WhyCard>,
    /// The last commit touching the definition's file; `null` when unknown.
    pub last_change: Option<LastChange>,
    /// Suggested next calls (none are synthesised in 3.0.0, OWN-68 SC-09).
    pub follow_ups: Vec<FollowUp>,
}

/// The `ranking` value when the complete candidate set was ordered before the cut.
const RANKING_FULL: &str = "full";

/// One node a name selector could have meant (Q-2, backlog 16, 21, 27): the `candidates` of
/// `ambiguous_symbol` and the `did_you_mean` of `unknown_symbol`, enough to pick a `ref`
/// without a second call. Every field is a snapshot value (a `CodeNode` subset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SymbolCandidate {
    /// SCIP kind name or `File`.
    pub label: String,
    /// Display name.
    pub name: String,
    /// Repository-relative path of the defining file (`null` for an external placeholder).
    pub path: Option<String>,
    /// The first line of the definition range (0-based); `null` without a range.
    pub line: Option<u32>,
    /// Enclosing range (the body).
    pub enclosing: Option<CodeRange>,
    /// Defined outside the repository.
    pub external: bool,
    /// Exact row identity in this snapshot (pass it back as `target.ref`).
    #[serde(rename = "ref")]
    pub r#ref: String,
}

impl From<&kaban_codegraph::NodeRow> for SymbolCandidate {
    fn from(row: &kaban_codegraph::NodeRow) -> Self {
        Self {
            label: row.label.clone(),
            name: row.name.clone(),
            path: row.path.clone(),
            line: row.def.map(|r| r.sl),
            enclosing: row.enc.map(CodeRange::from),
            external: row.external,
            r#ref: row.r#ref.clone(),
        }
    }
}

/// The rows a hint line of `unknown_symbol` or `ambiguous_symbol` carries (q2-1): the matched
/// rows, or nothing because the live policy rejected one of them. The rows are snapshot values
/// like the fields of a card, so they pass the same field-level response gate a card passes
/// ([`KabanApi::code_error`]) before they are serialized into the error text: a redact rule
/// masks the field, a reject hit withholds the whole list, as the adapter withholds a whole
/// result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    /// The rows to list (empty: no hint line).
    Rows(Vec<SymbolCandidate>),
    /// A row hit a reject rule of the live policy: the line says so and lists nothing.
    Withheld,
}

impl Hint {
    /// No rows.
    fn none() -> Self {
        Self::Rows(Vec::new())
    }
}

/// The hint value after a reject hit (fixed on purpose, like the adapter's withheld text).
const HINT_WITHHELD: &str = "withheld (a row contains a value the denylist policy forbids)";

/// Why a `code_*` call produced no result (stage1-spec G1-S8; cg-join-v1 §6). Every variant is
/// a fixed class with a fixed first sentence that never carries a symbol, a ref, a path or any
/// other request byte. The hint lines of `unknown_symbol` (`did_you_mean: [...]`) and
/// `ambiguous_symbol` (`candidates: [...]`, Q-2) carry [`SymbolCandidate`] rows as one JSON
/// array: snapshot values listed only because the snapshot holds a row of the requested name,
/// so a request byte reaches the text only when it names something indexed. The handlers
/// convert through [`KabanApi::code_error`], which passes every field of those rows through
/// the live response gate first (q2-1); the plain `From` conversion renders a [`Hint`] as it
/// stands and is for the hint-free classes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodeError {
    /// No snapshot resolves for this repository: no provider, no repository, git failed, or
    /// `NoSnapshot::{NoDirectory, NoFile, CorruptOnly}`.
    NoSnapshot,
    /// Every candidate snapshot was written by a newer `kaban-index` (`NoSnapshot::TooNewOnly`).
    SnapshotTooNew,
    /// The selector sets no form, several forms, or a form the tool does not allow.
    SelectorInvalid,
    /// `target.symbol` fails the SCIP grammar.
    SymbolInvalid,
    /// `target.ref` fails the discriminated ref parse.
    RefInvalid,
    /// Nothing in the resolved snapshot matches the selector; for a name form, the first
    /// `DID_YOU_MEAN_SHOWN` (selector.rs) same-named non-external rows of other labels.
    UnknownSymbol {
        /// Same-named rows of other labels, bounded; none for every other form.
        did_you_mean: Hint,
    },
    /// Several nodes match where one is required: the first `CANDIDATES_SHOWN` (selector.rs)
    /// of them in `(path, ref)` order, and how many matched in all.
    AmbiguousSymbol {
        /// The matched rows, bounded.
        candidates: Hint,
        /// Every matched row, counted.
        total: usize,
    },
    /// The reader failed (a driver error, a malformed column); the class only, never a value.
    Read {
        /// The reader's failure class.
        class: &'static str,
    },
}

impl std::fmt::Display for CodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSnapshot => f.write_str(
                "no_snapshot: no code-graph snapshot for this repository and commit; run `kaban-index index`",
            ),
            Self::SnapshotTooNew => f.write_str(
                "snapshot_too_new: the code-graph snapshot was written by a newer kaban-index; upgrade kaban or re-index",
            ),
            Self::SelectorInvalid => f.write_str(
                "selector_invalid: target must set exactly one of symbol, ref, {label, name} (with an optional path), {path, line} or path",
            ),
            Self::SymbolInvalid => f.write_str("symbol_invalid: target.symbol is not a SCIP symbol"),
            Self::RefInvalid => {
                f.write_str("ref_invalid: target.ref is not a scip:, local: or file: ref")
            }
            Self::UnknownSymbol { did_you_mean } => {
                f.write_str("unknown_symbol: no node matches the target in the resolved snapshot")?;
                hint_line(f, "did_you_mean", did_you_mean)
            }
            Self::AmbiguousSymbol { candidates, total } => {
                f.write_str("ambiguous_symbol: several nodes match the target; select by ref")?;
                hint_line(f, "candidates", candidates)?;
                let shown = match candidates {
                    Hint::Rows(rows) => rows.len(),
                    Hint::Withheld => 0,
                };
                if *total > shown {
                    write!(f, "\ncandidates_total: {total}")?;
                }
                Ok(())
            }
            Self::Read { class } => write!(f, "code graph read failed ({class})"),
        }
    }
}

/// `\n<key>: <json array>` after the class sentence, `\n<key>: withheld (...)` for a withheld
/// hint; nothing for an empty list.
fn hint_line(f: &mut std::fmt::Formatter<'_>, key: &str, hint: &Hint) -> std::fmt::Result {
    match hint {
        Hint::Rows(rows) if rows.is_empty() => Ok(()),
        Hint::Rows(rows) => {
            // a list of plain strings, integers and booleans always serializes
            let json = serde_json::to_string(rows).unwrap_or_default();
            write!(f, "\n{key}: {json}")
        }
        Hint::Withheld => write!(f, "\n{key}: {HINT_WITHHELD}"),
    }
}

impl From<CodeError> for ApiError {
    /// Every class is a request-shaped failure for the adapters (`cli/output.rs` matches
    /// `ApiError` exhaustively: no new variant); `NotFound` echoes and is not used.
    fn from(e: CodeError) -> Self {
        Self::Invalid(e.to_string())
    }
}

impl KabanApi {
    /// The adapters' form of a `code_*` failure under `tool`: the hint rows of
    /// `unknown_symbol` and `ambiguous_symbol` pass the live response gate field by field, as
    /// the cards of an ordinary answer do (q2-1), and only then is the text rendered. A reject
    /// hit on any row withholds the whole list ([`Hint::Withheld`]); a mask that leaves a row
    /// outside its declared shape withholds it too (the adapter's R3-9 rule for a result).
    pub(crate) fn code_error(&self, tool: &str, e: CodeError) -> ApiError {
        match e {
            CodeError::UnknownSymbol { did_you_mean } => CodeError::UnknownSymbol {
                did_you_mean: self.gate_hint(tool, "did_you_mean", did_you_mean),
            },
            CodeError::AmbiguousSymbol { candidates, total } => CodeError::AmbiguousSymbol {
                candidates: self.gate_hint(tool, "candidates", candidates),
                total,
            },
            other => other,
        }
        .into()
    }

    /// `hint` under `key` through `tool`'s response gate: the rows with every content field
    /// masked where a redact rule hit, or `Withheld`.
    fn gate_hint(&self, tool: &str, key: &str, hint: Hint) -> Hint {
        let Hint::Rows(rows) = hint else {
            return hint;
        };
        if rows.is_empty() {
            return Hint::Rows(rows);
        }
        let Ok(rows) = serde_json::to_value(&rows) else {
            return Hint::Withheld;
        };
        let mut object = serde_json::Map::new();
        object.insert(key.to_owned(), rows);
        let Ok(clean) = self.gate_response(tool, &serde_json::Value::Object(object)) else {
            return Hint::Withheld;
        };
        match clean.get(key).cloned().map(serde_json::from_value) {
            Some(Ok(rows)) => Hint::Rows(rows),
            _ => Hint::Withheld,
        }
    }

    /// [`selector::resolve`] for `tool`'s handler: a refusal is converted through
    /// [`Self::code_error`].
    pub(crate) fn select(
        &self,
        tool: &str,
        sel: &SymbolSelector,
        reader: &dyn CodeGraphReader,
    ) -> Result<selector::Found, ApiError> {
        selector::resolve(sel, reader).map_err(|e| self.code_error(tool, e))
    }

    /// [`selector::Selection::into_one`] for `tool`'s handler, converted the same way.
    pub(crate) fn select_one(
        &self,
        tool: &str,
        selection: selector::Selection,
    ) -> Result<kaban_codegraph::NodeRow, ApiError> {
        selection.into_one().map_err(|e| self.code_error(tool, e))
    }
}

impl From<ReadError> for CodeError {
    /// `Ambiguous` is the `ambiguous_symbol` class (cg-join-v1 §6); everything else is a read
    /// failure named by class.
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Ambiguous => Self::AmbiguousSymbol {
                candidates: Hint::none(),
                total: 0,
            },
            ReadError::Malformed { .. } => Self::Read { class: "malformed" },
            ReadError::OutOfRange { .. } => Self::Read {
                class: "out_of_range",
            },
            ReadError::Sql(_) => Self::Read { class: "sqlite" },
            _ => Self::Read { class: "unknown" },
        }
    }
}

impl KabanApi {
    /// Gate every content string of a selector under `tool` (S1): the gated (possibly
    /// redacted) selector, or the rejection. `label` is a control path: a value in the closed
    /// set is validated, anything else is scanned as content (the selector then refuses it).
    fn gate_selector(
        &self,
        ctx: &mut WriteCtx,
        tool: &'static str,
        sel: &SymbolSelector,
        red: &mut Vec<String>,
    ) -> Result<SymbolSelector, ApiError> {
        let label = match sel.label.as_deref() {
            Some(l) => Some(self.gate_control(
                ctx,
                tool,
                ControlPath::request(tool, "target.label"),
                "target.label",
                l,
                red,
            )?),
            None => None,
        };
        Ok(SymbolSelector {
            symbol: self.gate_opt(ctx, tool, "target.symbol", sel.symbol.as_deref(), red)?,
            r#ref: self.gate_opt(ctx, tool, "target.ref", sel.r#ref.as_deref(), red)?,
            label,
            name: self.gate_opt(ctx, tool, "target.name", sel.name.as_deref(), red)?,
            path: self.gate_opt(ctx, tool, "target.path", sel.path.as_deref(), red)?,
            line: sel.line,
        })
    }

    /// The wire form of a resolved snapshot: `policy` compares the snapshot's policy version
    /// with the live one (cg-join-v1 §4; `off` when the scan was off at index time).
    fn snapshot_wire(&self, r#ref: &SnapshotRef) -> SnapshotRefWire {
        let policy = crate::snapshot::policy_of(r#ref, &self.policy_version());
        SnapshotRefWire {
            commit: r#ref.commit.clone(),
            exact: r#ref.exact,
            schema_version: r#ref.schema_version,
            policy: policy.as_str().to_owned(),
            isolation: r#ref.isolation_tier.as_str().to_owned(),
        }
    }

    /// A list response from the complete candidate rows: project (with the snippet when
    /// `include_snippets`), rank by `inputs`, cut (RV-7: `rows` is the complete set, no handler
    /// calls a limited query).
    fn list_response(
        &self,
        resolved: &Resolved,
        rows: &[kaban_codegraph::NodeRow],
        limit: Option<usize>,
        inputs: &ranking::RankInputs,
        include_snippets: bool,
        target_note: Option<TargetNote>,
    ) -> Result<CodeListResponse, CodeError> {
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let cards: Vec<CodeNode> = rows
            .iter()
            .map(|row| cards::project(row, include_snippets))
            .collect();
        let (results, truncated) = ranking::order_then_limit(cards, inputs, limit);
        Ok(CodeListResponse {
            snapshot: self.snapshot_wire(&resolved.r#ref),
            target_note,
            coverage: coverage::summary_of(reader)?,
            count: results.len(),
            results,
            truncated,
            ranking: RANKING_FULL.to_owned(),
            follow_ups: Vec::new(),
        })
    }
}
