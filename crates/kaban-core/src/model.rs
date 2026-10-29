//! Graph model types: nodes, edges, schema entries, queries and outcomes.

use crate::audit::{AuditEntry, WorkspaceId};
use crate::ids::NodeId;
use crate::lifecycle::{Governance, Lifecycle, StoreScope};
use crate::reconcile::{Fixed, Layer, Method};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Input for creating a node. Every string has already passed the denylist gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NewNode {
    /// Node kind (label); must be registered in the schema.
    pub kind: String,
    /// The memory text.
    pub text: String,
    /// Optional one-line summary used for compact recall cards.
    pub summary: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
    /// 0..1 importance.
    pub importance: Option<f64>,
    /// Start of validity (RFC 3339), if known.
    pub valid_from: Option<String>,
    /// Session that produced the memory (the request field is still `source_session`, MD-26).
    pub session_id: Option<String>,
    /// Agent that produced the memory.
    pub agent_id: Option<String>,
    /// Workspace the memory belongs to (`None` = the default workspace).
    pub workspace_id: Option<String>,
    /// Arbitrary JSON properties.
    pub props: Option<Value>,
    /// Globs/paths a governing record covers (F1; distinct from `store_scope`).
    #[serde(default)]
    pub path_scope: Vec<String>,
    /// Near-duplicate key of a Decision (F1: normalized title + sorted scope), if computed.
    #[serde(default)]
    pub dup_key: Option<String>,
    /// Repository the record was made in, if known (M3-23: the repo root's name).
    #[serde(default)]
    pub repo: Option<String>,
    /// Which database the record lives in (`project` until M6).
    #[serde(default)]
    pub store_scope: StoreScope,
}

/// Typed props of a `Decision` (core-spec §4; core-plan C3.1). `rationale` is required; every
/// other field is optional (M3-4). Unknown keys are rejected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionProps {
    /// The decision statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Alternative name of the statement (shared with Conventions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Why (required).
    pub rationale: String,
    /// Alternatives that were considered and rejected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_alternatives: Vec<String>,
    /// Consequences of the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consequences: Option<String>,
    /// Exceptions to the decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<String>,
    /// Evidence: `CodeRef` ids or Chunk ids with a quote.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    /// Commit at acceptance (derived; accepted on the wire for round trips).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_commit: Option<String>,
}

/// Typed props of a `Convention` (core-spec §4): the Decision fields with nothing required.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConventionProps {
    /// The rule statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Alternative name of the statement (shared with Decisions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// Alternatives that were considered and rejected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_alternatives: Vec<String>,
    /// Consequences of the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consequences: Option<String>,
    /// Exceptions to the rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<String>,
    /// Evidence: `CodeRef` ids or Chunk ids with a quote.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    /// Commit at acceptance (derived; accepted on the wire for round trips).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_commit: Option<String>,
}

/// One piece of evidence for a Decision or Convention: a node id (`CodeRef` or Chunk) and an
/// optional quote.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// The `CodeRef` or Chunk node id.
    pub id: String,
    /// The quoted passage, for Chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

/// Typed props of a `Failure` (core-spec §4): every field optional (M3-4).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureProps {
    /// The error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
    /// The command that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Head of the stack trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_head: Option<String>,
    /// The fix (text; or a `FIXED_BY` edge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Environment the failure happened in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Whether a fix is still pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_fix: Option<bool>,
    /// Keyed fingerprint of the failed command (core-plan C4.4 as amended by M4-11: 64 hex
    /// digits of `HMAC(audit key, "kaban-cmd/v1\0" + normalized command)`, never a bare hash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_fingerprint: Option<String>,
    /// Later successful runs of the same command in the session, with the files edited in
    /// between (evidence, not a conclusion; core-spec F8b, M4-12).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_runs: Vec<EvidenceRun>,
}

/// One successful run of a failed command after its Failure (core-spec F8b; M4-12, M4-14).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRun {
    /// When the command succeeded (unix seconds).
    pub ts: i64,
    /// Repository-relative paths edited in the session between the failure and this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_edited_since: Vec<String>,
}

/// A symbol body span as Serena emits it (core-spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BodyLocation {
    /// First line of the body.
    pub start_line: u32,
    /// Last line of the body.
    pub end_line: u32,
}

/// A position as Serena emits it (0-based line and column; core-spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Location {
    /// 0-based line.
    pub line: u32,
    /// 0-based column.
    pub column: u32,
}

/// The stored form of a code reference (the props of a `CodeRef` node; core-spec §4, §6): the
/// wire fields minus `info`, plus the derived `line` (`location.line`, else
/// `body_location.start_line`). Every string has passed the denylist gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeRef {
    /// Repository identity.
    pub repo: String,
    /// Commit the reference was taken at.
    pub commit: String,
    /// Tool of origin (`serena`, `manual`, `kaban-adr`).
    pub tool: String,
    /// Path relative to the repository root, `/`-separated.
    pub relative_path: String,
    /// Serena-style symbol path (`Class/method`, `[i]` overload suffix kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    /// `SymbolKind` enum name (`"Class"`, `"Function"`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Body span, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_location: Option<BodyLocation>,
    /// Position, when known (0-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// Line of a reference site (0-based), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_line: Option<u32>,
    /// The normalised SCIP symbol key (stage1-spec G1-F9; cg-join-v1 §5): grammar-checked
    /// after the content scan, stored in its normalised form; derived from `relative_path` and
    /// `name_path` through the resolved snapshot when absent. `name_path` is never a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The 40-hex commit of the snapshot `symbol` was taken from or verified against (SO-2;
    /// G1-F9): a short request value is canonicalised before it is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_commit: Option<String>,
    /// When the reference was observed, RFC 3339 (stage1-spec §4.4): set by `remember` when
    /// the request carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Derived line: `location.line`, else `body_location.start_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// A stored node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// `UUIDv7` id.
    pub id: String,
    /// Node kind (label).
    pub kind: String,
    /// The memory text.
    pub text: String,
    /// Optional one-line summary.
    pub summary: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
    /// 0..1 importance.
    pub importance: Option<f64>,
    /// Storage visibility (core-spec §4).
    pub lifecycle: Lifecycle,
    /// Governance state; `None` for every kind but Decision, Convention and Failure.
    pub governance: Governance,
    /// Which database the record lives in.
    pub store_scope: StoreScope,
    /// Globs/paths a governing record covers (distinct from `store_scope`).
    pub path_scope: Vec<String>,
    /// Monotonic revision, bumped on every change (F13).
    pub rev: i64,
    /// Revision last written to the `.kaban/*.md` serialization (F13).
    pub serialized_rev: Option<i64>,
    /// Near-duplicate key for Decisions (F1).
    pub dup_key: Option<String>,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// Last update time (RFC 3339).
    pub updated_at: String,
    /// Start of validity, if set.
    pub valid_from: Option<String>,
    /// End of validity, set when superseded/invalidated.
    pub valid_to: Option<String>,
    /// Session that produced the memory.
    pub session_id: Option<String>,
    /// Agent that produced the memory.
    pub agent_id: Option<String>,
    /// Workspace the memory belongs to (never empty: F17 tenant key on every row).
    pub workspace_id: String,
    /// Repository the record was made in, if known.
    pub repo: Option<String>,
    /// Commit the record was made at, if known.
    pub commit_sha: Option<String>,
    /// Commit at acceptance (governed kinds, F4).
    pub accepted_commit: Option<String>,
    /// JSON properties (`{}` when none).
    pub props: Value,
    /// Dedup hash over (workspace, kind, trimmed text).
    pub content_hash: String,
    /// Denylist policy version that gated the write.
    pub policy_version: String,
}

/// A stored typed edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// `UUIDv7` id.
    pub id: String,
    /// Source node id.
    pub src: String,
    /// Destination node id.
    pub dst: String,
    /// Relationship type.
    pub rel: String,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// JSON properties (`{}` when none).
    pub props: Value,
}

/// A schema entry: a node kind (`label`) or a relationship type (`rel`).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TypeDef {
    /// `"label"` or `"rel"`.
    pub kind: String,
    /// The **stored name** (engine-spec §3.1): `PascalCase` for labels, `UPPER_SNAKE_CASE` for
    /// rels, bare for the `core`, `adr` and `user` namespaces and `ns:Name` for every other
    /// namespace (`incidents:Decision`). This is the name `node.kind` and `edge.rel` carry.
    pub name: String,
    /// The declaring namespace (engine-plan E1.3, E1.7): since migration 0004 the store
    /// persists it in `schema_type.ns` (`PRIMARY KEY(ns, kind, name)`) and reads it back from
    /// the column (`core`, `adr`, `user`, or the prefix of a stored `ns:Name`); a store still
    /// at schema 0003 reports `None` and the API derives it from the stored name. Ignored on
    /// write: `schema_extend` takes the namespace from the stored name.
    #[serde(default)]
    pub ns: Option<String>,
    /// Human description.
    pub description: String,
    /// For rels: allowed source kinds (empty = any).
    #[serde(default)]
    pub from_kinds: Vec<String>,
    /// For rels: allowed destination kinds (empty = any).
    #[serde(default)]
    pub to_kinds: Vec<String>,
    /// Whether the relationship is directed (default true).
    #[serde(default = "default_directed")]
    pub directed: bool,
    /// Optional JSON schema for props.
    #[serde(default)]
    pub props_schema: Option<Value>,
    /// Builtin entries cannot be changed.
    #[serde(default)]
    pub builtin: bool,
    /// The declaration columns migration 0004 added (`governed`, `rank`, `column_weights`,
    /// `extract`, `display`), flattened into the entry; empty for rels and for a kind that
    /// declares none (engine-plan E1.8, E1.7 R2).
    #[serde(default, flatten)]
    pub declaration: TypeDeclaration,
}

/// The JSON declaration columns of a kind (engine-spec §3.2 `schema_type`; EN-F4, EN-F29,
/// EN-F25): written at the load seam from the resolved ontology as canonical JSON (E1.1
/// `canon.rs`), `None` where the kind declares none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TypeDeclaration {
    /// The `governed` block (EN-F4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governed: Option<Value>,
    /// The `rank` block (EN-F29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<Value>,
    /// The `column_weights` block (EN-F29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_weights: Option<Value>,
    /// The `extract` block (EN-F25, EN-F36).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<Value>,
    /// The display labels (`governed.display`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Value>,
}

impl TypeDeclaration {
    /// The declaration of a resolved kind, each block as canonical JSON. The one place both
    /// load seams (shipped seed, user load) derive the columns from.
    #[must_use]
    pub fn of(kind: &crate::ontology::resolve::ResolvedKind) -> Self {
        fn canon<T: Serialize>(v: &T) -> Option<Value> {
            let value = serde_json::to_value(v).ok()?;
            serde_json::from_str(&crate::canon::canonical_json(&value)).ok()
        }
        Self {
            governed: kind.governed.as_ref().and_then(canon),
            rank: kind.rank.as_ref().and_then(canon),
            column_weights: kind.column_weights.as_ref().and_then(canon),
            extract: kind.extract.as_ref().and_then(canon),
            display: kind.governed.as_ref().and_then(|g| canon(&g.display)),
        }
    }

    /// Each column as its canonical JSON text, in table order `(governed, rank,
    /// column_weights, extract, display)`.
    #[must_use]
    pub fn texts(&self) -> [Option<String>; 5] {
        let text = |v: &Option<Value>| v.as_ref().map(crate::canon::canonical_json);
        [
            text(&self.governed),
            text(&self.rank),
            text(&self.column_weights),
            text(&self.extract),
            text(&self.display),
        ]
    }
}
fn default_directed() -> bool {
    true
}

/// A recall hit: the node, its fused score and why it was returned.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    /// The matched node.
    pub node: Node,
    /// Fused (RRF + graph expansion, confidence-weighted) score.
    pub score: f64,
    /// Explanations such as `bm25:1` or `hop1:FIXED_BY`.
    pub why: Vec<String>,
}

/// Outcome of a `remember` call.
#[derive(Debug, Clone, Serialize)]
pub enum RememberOutcome {
    /// A new node was created.
    Created(Node),
    /// An active node with the same content hash already existed. The requested links were
    /// still created (m2 N5); metadata that differed from the stored node was ignored and is
    /// named in `fields_ignored` (the first write wins).
    Existing {
        /// The stored node (unchanged).
        node: Node,
        /// Number of requested links created or refreshed on the existing node.
        links_created: usize,
        /// Request fields whose values differed from the stored node and were not applied.
        fields_ignored: Vec<String>,
    },
}

/// Recall sources, distinct from assertion producer layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RecallLayer {
    /// Lexical full-text retrieval.
    Fts,
    /// Fuzzy (typo-tolerant) retrieval by the text index (spec 05 OWN-M: `fuzzy` on the wire).
    Fuzzy,
    /// Reserved similarity retrieval, unavailable in this release.
    Sim,
}

/// Parameters of a recall.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallQuery {
    /// Explicit retrieval sources; absent uses enabled defaults.
    #[serde(default)]
    pub layers: Option<Vec<RecallLayer>>,
    /// Request fuzzy retrieval even when lexical retrieval hits.
    #[serde(default)]
    pub fuzzy: bool,
    /// Free-text query.
    pub query: String,
    /// Restrict to these kinds (empty = any).
    pub kinds: Vec<String>,
    /// Require at least one of these tags (empty = any).
    pub tags: Vec<String>,
    /// Restrict to this workspace.
    pub workspace_id: Option<String>,
    /// Max results (0 = default 10, capped at 100).
    pub limit: usize,
    /// Graph expansion hops (capped at 2).
    pub hops: u8,
    /// Also return archived nodes (superseded records) reached through graph expansion; the text
    /// index holds active rows only, so archived nodes never seed a recall.
    pub include_superseded: bool,
    /// Also return archived nodes of any governance reached through graph expansion (core-spec
    /// §4 "`include_archived` opts in"; same frontier rule as `include_superseded`, M3-12).
    pub include_archived: bool,
    /// Restrict the lexical seeds to notes linked `ABOUT` or `EVIDENCE_FOR` to a `CodeRef`
    /// matching this filter (core-spec F3 "about this `CodeRef`"; `None` = no restriction).
    pub r#ref: Option<CodeRefFilter>,
    /// Candidate visibility (EN-F49; engine-plan E1.8): excluded unless `include_candidates`.
    #[serde(default)]
    pub filters: ReadFilters,
}

/// "About this `CodeRef`" filter of a recall (core-spec F3; M3-12): exact match on the stored
/// `CodeRef` props; an absent optional field matches any value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeRefFilter {
    /// Path relative to the repository root (as stored: backslashes normalised to `/`).
    pub relative_path: String,
    /// Serena-style symbol path (`Class/method`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_path: Option<String>,
    /// Repository identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Which workspace a `why` lookup reads (core-spec S6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceFilter {
    /// One workspace (the bound one or an explicitly named other one).
    Only(String),
    /// Every workspace (the explicit `"*"` selector).
    All,
}

impl WorkspaceFilter {
    /// The workspace name to filter on, `None` for every workspace.
    pub fn as_option(&self) -> Option<&str> {
        match self {
            Self::Only(ws) => Some(ws),
            Self::All => None,
        }
    }
}

impl From<Option<String>> for WorkspaceFilter {
    /// `None` (the `"*"` selector resolved by the API) is every workspace.
    fn from(ws: Option<String>) -> Self {
        ws.map_or(Self::All, Self::Only)
    }
}

/// Parameters of a governing-record lookup (core-spec F2; core-plan C3.4, M3-7): active records
/// of `kinds` in one of `governance` whose `path_scope` governs a path (an entry equal to one
/// of `path_candidates`, the target and its ancestor directories, or a glob entry that may match
/// the target: the store prefilters globs and the caller decides exactly) or that match `query`
/// in the text index. Ordered accepted before proposed, then `importance DESC`, `created_at
/// DESC`, id (N5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoverningQuery {
    /// Node kinds (`Decision`, `Convention`).
    pub kinds: Vec<String>,
    /// Governance states to include (`Accepted`, `Proposed`).
    pub governance: Vec<Governance>,
    /// Workspace filter.
    pub workspace: WorkspaceFilter,
    /// The target path and its ancestor directories (empty for a query target).
    pub path_candidates: Vec<String>,
    /// Free-text query (a query target); `None` for a path target.
    pub query: Option<String>,
    /// Maximum rows.
    pub limit: usize,
    /// Rows to skip in the deterministic order (the caller pages past prefilter false
    /// positives, M3 review R3-5).
    pub offset: usize,
    /// Candidate visibility (EN-F49; engine-plan E1.8): excluded unless `include_candidates`.
    pub filters: ReadFilters,
    /// EN-F10 `[why] anchor_prefilter` (`config.rs` `Why::anchor_prefilter`, default `true`):
    /// prefilter a path target through the `node_anchor` path rows on a store past migration
    /// 0004 before the `path_scope` scan decides; `false` runs the scan alone (the rollback of
    /// engine-plan E2.7).
    pub anchor_prefilter: bool,
}

/// Kind of a `session_edits` row (migration 0003; core-plan C4.3/C4.4 as amended by M4-13):
/// what the hook layer scribbled for a host session. Closed set, mirrored by the table's
/// `CHECK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionEditKind {
    /// `path` is a repository-relative path an edit hook announced (the attempted edit).
    Edit,
    /// `path` is the id of the Failure a failed command produced in the session (M4-14).
    Failure,
    /// `path` is empty: the Stop hook has prompted the agent once in this session (M4-16).
    Prompted,
}

impl SessionEditKind {
    /// Stable lowercase name used in the `session_edits.kind` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Failure => "failure",
            Self::Prompted => "prompted",
        }
    }
    /// The kind behind its column value, if it is one.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "edit" => Some(Self::Edit),
            "failure" => Some(Self::Failure),
            "prompted" => Some(Self::Prompted),
            _ => None,
        }
    }
}

/// One `session_edits` row (migration 0003). Every string has passed the denylist gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEdit {
    /// Host session the row belongs to.
    pub session_id: String,
    /// What the row records.
    pub kind: SessionEditKind,
    /// Path, Failure id or empty, per [`SessionEditKind`].
    pub path: String,
    /// Unix seconds.
    pub ts: i64,
}

/// Which pending Failures a lookup returns (M4-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingScope {
    /// Only Failures recorded in the named session.
    Session,
    /// Every pending Failure of the workspace, the named session's first.
    Workspace,
}

/// The pending-failure lookup behind the Stop prompt, the success match and the briefing
/// (M4-14): active Failures of `workspace_id` with no outgoing `FIXED_BY` edge and
/// `props.pending_fix` not `false`, restricted by `scope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFailuresQuery {
    /// Workspace the Failures belong to (core-spec S6).
    pub workspace_id: String,
    /// The host session (a `failure` row names the Failure in it).
    pub session_id: String,
    /// Session-only or whole workspace.
    pub scope: PendingScope,
    /// Maximum rows.
    pub limit: usize,
}

/// The props patch `update_failure_props` applies to a Failure (M4-15, M4-14): the closed set
/// of fields the hook layer may change after the fact. Every string has passed the gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailurePatch {
    /// Set `props.fix`.
    pub fix: Option<String>,
    /// Set `props.pending_fix`.
    pub pending_fix: Option<bool>,
    /// Append to `props.evidence_runs`.
    pub evidence_run: Option<EvidenceRun>,
}

/// Which edges a traversal follows from a node (MT-5, `rust-bp-review-dispositions.md` RB-4).
/// Closed set: an unknown wire value fails deserialization.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TraverseDirection {
    /// Outgoing edges only (`src` = the node).
    Out,
    /// Incoming edges only (`dst` = the node).
    In,
    /// Both directions (the default).
    #[default]
    Both,
}

impl TraverseDirection {
    /// Whether outgoing edges are followed.
    #[must_use]
    pub fn follows_out(self) -> bool {
        match self {
            Self::Out | Self::Both => true,
            Self::In => false,
        }
    }

    /// Whether incoming edges are followed.
    #[must_use]
    pub fn follows_in(self) -> bool {
        match self {
            Self::In | Self::Both => true,
            Self::Out => false,
        }
    }
}

/// The edge-level filter of a traversal (engine-spec EN-F46; engine-plan E2.8): every clause
/// runs on the stored scoring columns of `edge` (`layer`, `method`, `score`, migration 0004),
/// so an edge that carries no value in a filtered column (a structural batch edge, a pre-0004
/// row) is dropped by that clause and its far endpoint is not reached through it. An empty
/// list or `None` filters nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct EdgeFilter {
    /// Keep only edges whose `score` (the winning claim's raw score) is at least this.
    #[serde(default)]
    pub min_score: Option<Fixed>,
    /// Keep only edges of these source layers (empty = any).
    #[serde(default)]
    pub layers: Vec<Layer>,
    /// Keep only edges of these methods (empty = any).
    #[serde(default)]
    pub methods: Vec<Method>,
}

/// Parameters of a bounded traversal.
#[derive(Debug, Clone, Deserialize)]
pub struct TraverseQuery {
    /// Starting node ids.
    pub start_ids: Vec<NodeId>,
    /// Follow only these relationship types (empty = any; EN-F46 `rels`). The store refuses an
    /// entry the loaded ontology does not declare (`UnknownRel`) instead of matching nothing.
    pub rels: Vec<String>,
    /// Which edges to follow (default both).
    #[serde(default)]
    pub direction: TraverseDirection,
    /// Depth 1..4.
    pub max_depth: u8,
    /// Keep only these node kinds (empty = any; EN-F46 `kinds`). The store refuses an entry
    /// the loaded ontology does not declare (`UnknownKind`) instead of matching nothing.
    pub node_kinds: Vec<String>,
    /// EN-F46: an ontology-defined kind set (`governing` is the one set in 3.0.0). The API
    /// resolves it into `node_kinds` before the call (`kaban-api` `query.rs`); the store does
    /// not read it. The field stays on the core query for E2.9's templates.
    #[serde(default)]
    pub kind_set: Option<String>,
    /// EN-F46: the edge-level filter, appended to every neighbour query before the hub cut.
    #[serde(default)]
    pub edge_filter: Option<EdgeFilter>,
    /// Keep only nodes in these governance states (empty = any; core-spec F6 "of any governance
    /// state unless filtered").
    #[serde(default)]
    pub governance: Vec<Governance>,
    /// Max nodes (0 = default 100, capped at 500).
    pub limit: usize,
    /// Restrict to this workspace (`None` = any); start ids outside it are not found and
    /// neighbours outside it are not returned (core-spec S6).
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Candidate visibility (EN-F49; engine-plan E1.8): excluded unless `include_candidates`.
    #[serde(default)]
    pub filters: ReadFilters,
    /// EN-F19 (engine-plan E2.7): expand each reached node with a `symbol` or `path` anchor one
    /// step into the attached code-graph snapshot; the snapshot rows come back as
    /// [`TraverseResult::anchored`] cards (200 per anchored node, 500 in total). Off, the
    /// result is today's, byte for byte. The flag is this request's admission (E2.5e, S7):
    /// the API sets it only under a snapshot resolution it attached in the same call, never
    /// from the caller's wish alone, so an attachment an earlier request left on the read
    /// connection expands nothing on its own.
    #[serde(default)]
    pub follow_anchors: bool,
    /// The repository identity the anchor rows must carry (`ApiConfig.repo`, CG-JOIN v1 §3
    /// SD-5); `None` follows nothing.
    #[serde(default)]
    pub anchor_repo: Option<String>,
}

impl Default for TraverseQuery {
    /// No start, depth 1, both directions, the store's default limit (`0` = 100), no filter.
    fn default() -> Self {
        Self {
            start_ids: Vec::new(),
            rels: Vec::new(),
            direction: TraverseDirection::Both,
            max_depth: 1,
            node_kinds: Vec::new(),
            kind_set: None,
            edge_filter: None,
            governance: Vec::new(),
            limit: 0,
            workspace_id: None,
            filters: ReadFilters::default(),
            follow_anchors: false,
            anchor_repo: None,
        }
    }
}

/// Result of a traversal: active nodes and the edges among them.
#[derive(Debug, Clone, Serialize)]
pub struct TraverseResult {
    /// Nodes reached (start nodes are kept regardless of status).
    pub nodes: Vec<Node>,
    /// Edges whose endpoints are both in `nodes`.
    pub edges: Vec<Edge>,
    /// The code-graph cards of [`TraverseQuery::follow_anchors`] (EN-F19): empty when the
    /// flag is off, no snapshot is attached or no reached node has an anchor row.
    #[serde(default)]
    pub anchored: Vec<AnchoredRow>,
}

/// What kind of snapshot row an [`AnchoredRow`] is (the stage 1 card kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AnchoredKind {
    /// A `cg.symbols` row (`kind = 'symbol'`, `external = 0`).
    Symbol,
    /// A `cg.files` row.
    File,
}

/// One code-graph card reached through an anchor of a traversed node (EN-F19; CG-JOIN v1 §9:
/// complete candidate sets ordered by `(path, ref)`, truncated by the store after ordering).
/// Constructed by the store through [`AnchoredRow::new`] (`#[non_exhaustive]`: a later field is
/// additive for every reader).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct AnchoredRow {
    /// The traversed node whose anchor reached this row.
    pub from: NodeId,
    /// `symbol` or `file`.
    pub kind: AnchoredKind,
    /// The file's path (`cg.files.path`): the row itself for a `file`, the defining file for a
    /// `symbol`.
    pub path: String,
    /// The symbol's `ref` (`None` for a file).
    pub r#ref: Option<String>,
    /// The symbol's cross-commit `key` (`None` for a file or a key-less symbol).
    pub key: Option<String>,
    /// The symbol's `name` (`None` for a file).
    pub name: Option<String>,
}

impl AnchoredRow {
    /// A `file` card of `path` reached from `from`.
    #[must_use]
    pub fn file(from: NodeId, path: String) -> Self {
        Self {
            from,
            kind: AnchoredKind::File,
            path,
            r#ref: None,
            key: None,
            name: None,
        }
    }

    /// A `symbol` card defined in `path` reached from `from`.
    #[must_use]
    pub fn symbol(
        from: NodeId,
        path: String,
        r#ref: String,
        key: Option<String>,
        name: Option<String>,
    ) -> Self {
        Self {
            from,
            kind: AnchoredKind::Symbol,
            path,
            r#ref: Some(r#ref),
            key,
            name,
        }
    }
}

/// How to forget a node (core-spec F5).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ForgetMode {
    /// Keep history, hide from every default read, never re-propose as a duplicate (default).
    /// Lifecycle only: governance is untouched and no event is written.
    #[default]
    Tombstone,
    /// Tombstone and, for governed kinds, mark the record `invalidated` with an event (OD-4:
    /// non-governed kinds get the event too).
    Invalidate,
    /// Archive: reachable by id and through graph expansion, out of the text index.
    Archive,
    /// Destructive: remove text, index entries and edges; the id row stays as `purged`.
    Purge,
}

/// A link to create atomically with a new node. Exactly one of `from` / `to` is set; the
/// other endpoint is the new node.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct LinkReq {
    /// Relationship type.
    pub rel: String,
    /// Source node id (when the new node is the destination).
    pub from: Option<NodeId>,
    /// Destination node id (when the new node is the source).
    pub to: Option<NodeId>,
    /// 0..1 confidence.
    pub confidence: Option<f64>,
}

/// One transaction's worth of ingestion writes (core-spec F16 store side, core-plan C2.6): the
/// single-committer entry point docs-ingest uses, one batch per Document. Plain serializable
/// data (engine-spec EN-E10a; engine-plan E2.11): no handles, so a batch built on one node can
/// be shipped to a store elsewhere; `assertions` and `edge_diff` default when absent, so a
/// batch written before they existed still parses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteBatch {
    /// Tenant of every row the batch touches (F17). A node's own `workspace_id` field is
    /// ignored; an existing row with the same id in another workspace is `NotFound` (WD-5).
    pub workspace_id: WorkspaceId,
    /// Operations, applied node ops first (in order), then edge ops, then the edge diff.
    pub ops: Vec<Op>,
    /// Claims the batch asserts (engine-spec EN-F8; engine-plan E2.2): inserted as immutable
    /// `assertion` rows after the node ops, then reconciled group by group at the end of the
    /// batch; the projection of the verdicts writes the edges. Empty for every producer that
    /// has not opted in ([`WriteBatch::new`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<BatchAssertion>,
    /// Source-bound publications, validated before any batch side effects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<crate::DerivedArtifact>,
    /// Complete Gazetteer producer sets, guarded and retired in this transaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gazetteer_claim_sets: Vec<crate::GazetteerClaimSet>,
    /// Which unnamed outgoing edges of the upserted nodes the end-of-batch diff deletes
    /// (engine-spec EN-E4, DI1.8; OWN-83 (4)). [`WriteBatch::new`] sets [`EdgeDiff::None`]:
    /// a producer that does not name itself deletes nothing (E2.5e), so every fixture that
    /// does not opt in keeps today's bytes; the ingest pipeline opts in with `Owned`.
    #[serde(default)]
    pub edge_diff: EdgeDiff,
}

impl WriteBatch {
    /// A batch under the default [`EdgeDiff::None`] rule (no deletion), without assertions.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, ops: Vec<Op>) -> Self {
        Self {
            workspace_id,
            ops,
            assertions: Vec::new(),
            artifacts: Vec::new(),
            gazetteer_claim_sets: Vec::new(),
            edge_diff: EdgeDiff::None,
        }
    }
}

/// What a batch claim is about, in producer coordinates (engine-plan E2.2): producers name
/// nodes by their [`NodeId`]; the store resolves every id inside the batch's workspace (F17)
/// and builds the storage-coordinate [`crate::Subject`] inside the transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BatchSubject {
    /// An exact mention span of a chunk.
    Span {
        /// The chunk node.
        chunk: NodeId,
        /// Byte offset.
        offset: u32,
        /// Byte length.
        len: u32,
    },
    /// An existing node.
    Node(NodeId),
    /// An edge triple; the `edge` claim class.
    Edge {
        /// Source node.
        src: NodeId,
        /// Stored relationship name.
        rel: String,
        /// Target node.
        dst: NodeId,
    },
    /// A record whose kind is being proposed.
    Record(NodeId),
}

/// One claim inside a [`WriteBatch`] (engine-spec EN-F8, EN-F12a): the fields of a
/// [`crate::Assertion`] a producer knows. The store assigns the key (`Assertion::with_key`),
/// the workspace and the timestamp, and canonicalises `value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchAssertion {
    /// What the claim is about.
    pub subject: BatchSubject,
    /// Conflict class.
    pub claim_class: crate::ClaimClass,
    /// Property or relationship name (`Edge` subjects carry it in the subject).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    /// Target node of a `canonical_id` claim, if any (`Edge` subjects carry it in the subject).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<NodeId>,
    /// Claim value: a kind name, an id, a number or an already gated prop value, never
    /// surface text (EN-S9).
    pub value: Value,
    /// Source layer.
    pub layer: crate::Layer,
    /// Extraction method.
    pub method: crate::Method,
    /// Extractor identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extractor_id: Option<String>,
    /// Extractor version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extractor_version: Option<String>,
    /// Source identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Shared evidence lineage (EN-F12b).
    pub lineage_id: String,
    /// Raw confidence in units of 1/10000.
    pub raw_score: crate::Fixed,
    /// Rank partition (E2.3 applies it).
    pub rank: crate::Rank,
    /// Why the rank was changed, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank_reason: Option<String>,
    /// Calibration version, excluded from the identity.
    #[serde(default)]
    pub calibration_version: u32,
    /// The claim this one supersedes, by key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<crate::AssertionKey>,
}

/// Who wrote an edge (engine-spec EN-E4): the `props.owner` vocabulary [`EdgeDiff::Owned`]
/// compares against. The wire strings are the ones DI1.8 landed (`"kaban-ingest"` is
/// [`crate::INGEST_OWNER`]), so the JSON form of a batch is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Owner {
    /// The docs-ingest pipeline (L0/L1).
    #[serde(rename = "kaban-ingest")]
    KabanIngest,
    /// The parse layer (L3).
    #[serde(rename = "kaban-parse")]
    KabanParse,
    /// The embedding layer (L2).
    #[serde(rename = "kaban-embed")]
    KabanEmbed,
    /// The reconciler's own writes.
    #[serde(rename = "kaban-reconcile")]
    KabanReconcile,
    /// A human (`link`, `remember(links)`, `accept`, the M5 import).
    #[serde(rename = "human")]
    Human,
}

impl Owner {
    /// The stored `props.owner` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KabanIngest => "kaban-ingest",
            Self::KabanParse => "kaban-parse",
            Self::KabanEmbed => "kaban-embed",
            Self::KabanReconcile => "kaban-reconcile",
            Self::Human => "human",
        }
    }
    /// Parse a stored spelling; anything else is not an owner.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "kaban-ingest" => Some(Self::KabanIngest),
            "kaban-parse" => Some(Self::KabanParse),
            "kaban-embed" => Some(Self::KabanEmbed),
            "kaban-reconcile" => Some(Self::KabanReconcile),
            "human" => Some(Self::Human),
            _ => None,
        }
    }
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The end-of-batch edge diff rule (engine-spec EN-E4; docs-ingest-spec D5 [A-3]; OWN-83
/// (4)): which outgoing edges of an upserted node that the batch did not name are deleted. A
/// layer never deletes an edge it did not create: both deleting rules reach only the unnamed
/// rows tagged `props.owner = <the named producer>`, and never a human's row whatever its
/// `layer` (the store's predicate refuses `producer: human`, not the type). An untagged
/// legacy row (no `owner`) is no producer's. Wire form: `{"mode": "complete", "producer":
/// "kaban-ingest"}`, `{"mode": "owned", "owner": "kaban-ingest"}`, `{"mode": "none"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EdgeDiff {
    /// The batch names the complete outgoing edge set of each upserted node for `producer`
    /// (MD-23 under EN-E4): the unnamed rows tagged with that producer are deleted.
    Complete {
        /// The producer whose complete set this is, for example [`Owner::KabanIngest`].
        producer: Owner,
    },
    /// Only the unnamed outgoing edges whose `props.owner` equals `owner`: a producer never
    /// deletes an edge it did not create (a human `link`, `remember`, another layer).
    Owned {
        /// The producer, for example [`Owner::KabanIngest`].
        owner: Owner,
    },
    /// No deletion at all (the [`WriteBatch::new`] default).
    #[default]
    None,
}

impl EdgeDiff {
    /// The owner every edge of the batch is tagged with (EN-E4): the named producer, else a
    /// human (today's `link`, `remember(links)` and the `None` batches).
    #[must_use]
    pub fn owner(&self) -> Owner {
        match self {
            Self::Complete { producer } => *producer,
            Self::Owned { owner } => *owner,
            Self::None => Owner::Human,
        }
    }
}

/// One ingestion operation inside a [`WriteBatch`]. Wire form (EN-E10a): internally tagged
/// by `op`, `{"op": "upsert_node", "id": .., "node": {..}}`, `{"op": "upsert_edge", "src": ..,
/// "dst": .., "rel": .., "props": ..}`, `{"op": "tombstone", "id": .., "reason": ..}`,
/// `{"op": "reactivate", "id": ..}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // `NewNode` grew in C3.1; a batch holds a handful of ops and boxing would touch every ingest caller
pub enum Op {
    /// Insert or update a node under an explicit (derived) id. Unchanged content (WD-7 hash)
    /// writes nothing; archived or tombstoned rows are updated in place and stay hidden; a
    /// purged id or a different stored `kind` fails the batch (WD-1, WD-2).
    UpsertNode {
        /// The node's id, chosen by the caller.
        id: NodeId,
        /// Content; `workspace_id` is overridden by the batch's.
        node: NewNode,
    },
    /// Insert or refresh a typed edge. Outgoing edges of an upserted node that the batch does
    /// not name are deleted at the end of the batch (MD-23).
    UpsertEdge {
        /// Source node id.
        src: NodeId,
        /// Destination node id.
        dst: NodeId,
        /// Relationship type.
        rel: String,
        /// Edge properties (`None` = `{}`).
        props: Option<Value>,
    },
    /// Tombstone a node (lifecycle only, no event, edges kept; the `ForgetMode::Tombstone`
    /// path). Already tombstoned or purged: a no-op (WD-3); unknown: `NotFound`.
    Tombstone {
        /// The node's id.
        id: NodeId,
        /// Why (kept for the plan's shape; unused until docs-ingest records it, WD-4).
        reason: String,
    },
    /// The ingestion reconciliation transition (DI1.8, RD2-5; docs-ingest-spec D5): bring a
    /// tombstoned node whose deterministic id re-appears in the source back to `active`. The
    /// store accepts it only under [`crate::GovernedSet::reconciliation_transition`] (an
    /// ingest-owned Document, Section or Chunk): `rev` bumped, a `reactivated` event, an outbox
    /// row. Already active: a no-op counted `unchanged`; purged or unknown: `NotFound`; every
    /// other node: `InvalidTransition::Reactivate`, failing the batch.
    Reactivate {
        /// The node's id.
        id: NodeId,
    },
}

/// The typed outcome of one node op inside a batch (DI1.8, RD2-8): what the store actually
/// did, so a producer's statistics come from commit outcomes, not from the ops it submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpOutcome {
    /// The node the op named.
    pub id: NodeId,
    /// The node's stored kind (`Chunk`, `Section`, ...), so a producer can count per kind.
    pub node_kind: String,
    /// What happened to it.
    pub outcome: OutcomeKind,
}

/// What one node op did (see [`OpOutcome`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    /// The row was created.
    Inserted,
    /// The row's content changed.
    Updated,
    /// The content hash matched: nothing written.
    Unchanged,
    /// The row moved to `tombstoned`.
    Tombstoned,
    /// Already tombstoned or purged: nothing written (WD-3).
    AlreadyHidden,
    /// `tombstoned -> active` through `Op::Reactivate`.
    Reactivated,
    /// The record was skipped: its undeclared label is refused by the discovered-labels policy
    /// (engine-spec EN-F5 `kind_not_allowed`; engine-plan E1.8). Nothing written.
    KindNotAllowed,
    /// The record was skipped: the workspace's discovered-kinds cap is reached (EN-F5
    /// `discovered_cap_reached`); only `discovered_overflow.n_rejected` moved.
    CapReached,
}

/// What a [`WriteBatch`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BatchReport {
    /// Sources skipped because the producer's expected revision is no longer current.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stale_source: u32,
    /// Nodes created.
    pub inserted: u32,
    /// Nodes whose content changed.
    pub updated: u32,
    /// Nodes whose content hash matched (no row written, no outbox row).
    pub unchanged: u32,
    /// Nodes moved to `tombstoned` by this batch.
    pub tombstoned: u32,
    /// Nodes moved `tombstoned -> active` by `Op::Reactivate` (DI1.8).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reactivated: u32,
    /// Nodes skipped by the discovered-labels policy or cap (EN-F5; engine-plan E1.8).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped: u32,
    /// Per node op, in batch order, what the store did (DI1.8, RD2-8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<OpOutcome>,
    /// Claim rows the batch inserted (engine-spec EN-F8; E2.2).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub assertions_inserted: u32,
    /// Claims re-submitted with an existing key: no row, no `ChangeLog` row (EN-F12a N5).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub assertions_noop: u32,
    /// Verdicts of the end-of-batch reconciliation pass, counted by the disposition their
    /// statuses received (EN-E9, EN-F12): one entry per decided group.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub verdicts: std::collections::BTreeMap<crate::AssertionStatus, u32>,
    /// First outbox `seq` written by the batch's transaction, if any.
    pub outbox_first_seq: Option<u64>,
    /// Last outbox `seq` written by the batch's transaction, if any.
    pub outbox_last_seq: Option<u64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `skip_serializing_if` signature
fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl BatchReport {
    /// Record one node outcome and fold it into the counters.
    pub fn record(&mut self, id: &NodeId, node_kind: &str, outcome: OutcomeKind) {
        match outcome {
            OutcomeKind::Inserted => self.inserted += 1,
            OutcomeKind::Updated => self.updated += 1,
            OutcomeKind::Unchanged => self.unchanged += 1,
            OutcomeKind::Tombstoned => self.tombstoned += 1,
            OutcomeKind::Reactivated => self.reactivated += 1,
            OutcomeKind::KindNotAllowed | OutcomeKind::CapReached => self.skipped += 1,
            OutcomeKind::AlreadyHidden => {}
        }
        self.outcomes.push(OpOutcome {
            id: id.clone(),
            node_kind: node_kind.to_owned(),
            outcome,
        });
    }
}

/// The rewritable persisted strings of a node (M2 review R2-2): every string column of `node`
/// except `id`, the timestamps, the hashes, the closed vocabularies (`lifecycle`, `governance`,
/// `store_scope`) and the two the rescan does not rewrite in place, `kind` (a schema entry) and
/// `workspace_id` (a tenant name), which the read projection keeps masking. After an in-place
/// redact the store writes these columns and replaces `content_hash` by a hash over the node id
/// and these fields (`kaban-redacted/v1`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RedactedStrings {
    /// Masked text.
    pub text: String,
    /// Masked summary.
    pub summary: Option<String>,
    /// Masked tags.
    pub tags: Vec<String>,
    /// Masked props.
    pub props: Value,
    /// Masked path scope globs.
    pub path_scope: Vec<String>,
    /// Masked session id.
    pub session_id: Option<String>,
    /// Masked agent id.
    pub agent_id: Option<String>,
    /// Masked repository.
    pub repo: Option<String>,
    /// Masked commit.
    pub commit_sha: Option<String>,
    /// Masked accepted commit.
    pub accepted_commit: Option<String>,
    /// Masked duplicate key.
    pub dup_key: Option<String>,
}

impl RedactedStrings {
    /// Whether these strings differ from what `n` stores (so the row must be rewritten).
    #[must_use]
    pub fn differs_from(&self, n: &Node) -> bool {
        self.text != n.text
            || self.summary != n.summary
            || self.tags != n.tags
            || self.props != n.props
            || self.path_scope != n.path_scope
            || self.session_id != n.session_id
            || self.agent_id != n.agent_id
            || self.repo != n.repo
            || self.commit_sha != n.commit_sha
            || self.accepted_commit != n.accepted_commit
            || self.dup_key != n.dup_key
    }
}

/// What the rescan job decided for one node (core-spec S4; core-plan C2.7). The store applies
/// the action inside the batch's transaction; the audit entries ride in the same transaction.
#[derive(Debug, Clone)]
pub enum RescanAction {
    /// The node passes the loaded policy: only its `policy_version` is stamped.
    Keep,
    /// Redact hits: the masked strings replace the stored ones in place (new `rev`, new
    /// `content_hash`).
    Redact {
        /// Every rewritable string after masking (boxed: the variant is much larger than
        /// `Keep`).
        strings: Box<RedactedStrings>,
        /// Redact verdicts to record.
        audit: Vec<AuditEntry>,
    },
    /// A reject hit: the node is tombstoned (lifecycle only) with a `policy-reject` event.
    Reject {
        /// Reject verdicts to record.
        audit: Vec<AuditEntry>,
    },
}

/// What one `rescan` batch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RescanReport {
    /// Nodes judged in this batch (`0` = nothing left to rescan).
    pub scanned: u64,
    /// Nodes that passed (stamped only).
    pub kept: u64,
    /// Nodes masked in place.
    pub redacted: u64,
    /// Nodes tombstoned.
    pub rejected: u64,
}

impl std::ops::AddAssign for RescanReport {
    fn add_assign(&mut self, rhs: Self) {
        self.scanned += rhs.scanned;
        self.kept += rhs.kept;
        self.redacted += rhs.redacted;
        self.rejected += rhs.rejected;
    }
}

/// A record as a `.kaban/<dir>/<id>.md` file carries it, ready for `SyncStore::import_record`
/// (core-spec F13; core-plan C5.1 as amended by `reviews/m5-prep.md` M5-18): every content
/// column the file decides, with the file's `rev`. Every string has passed the denylist gate
/// and the typed parse (the importer's job); the store writes, it does not judge. The
/// workspace is the write context's (OWN-25: files carry none), `store_scope` is `project`.
#[derive(Debug, Clone)]
pub struct ImportedRecord {
    /// The node id (the file name).
    pub id: NodeId,
    /// Node kind (the directory); immutable per id.
    pub kind: String,
    /// The memory text (the body).
    pub text: String,
    /// One-line summary.
    pub summary: Option<String>,
    /// Tags in file order.
    pub tags: Vec<String>,
    /// 0..1.
    pub confidence: Option<f64>,
    /// 0..1.
    pub importance: Option<f64>,
    /// Storage visibility the file declares (never `purged`: that is `import_purged`).
    pub lifecycle: Lifecycle,
    /// Governance state the file declares.
    pub governance: Governance,
    /// Canonical path scope.
    pub path_scope: Vec<String>,
    /// Kind props (`{}` when none).
    pub props: Value,
    /// The file's revision: becomes both `rev` and `serialized_rev`.
    pub rev: i64,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// Last update time (RFC 3339).
    pub updated_at: String,
    /// Validity start.
    pub valid_from: Option<String>,
    /// Validity end.
    pub valid_to: Option<String>,
    /// Commit at acceptance.
    pub accepted_commit: Option<String>,
    /// Provenance: agent.
    pub agent_id: Option<String>,
    /// Provenance: session.
    pub session_id: Option<String>,
    /// Provenance: repository identity.
    pub repo: Option<String>,
    /// Provenance: stamped commit.
    pub commit_sha: Option<String>,
    /// Near-duplicate key, recomputed by the importer (F1).
    pub dup_key: Option<String>,
    /// The event to append when the import moves the record's governance (reason, signer and
    /// provenance decided by the importer: `human-pr` or `proposed`, OD-15). `None` lets the
    /// store write a `system` event with reason `import`.
    pub event: Option<crate::events::NewEvent>,
}

/// One `conflicts` row (core-spec F13 "equal rev with different content hash"; M5-5): the
/// file and the database disagree at the same `rev`, resolved by a human (`kaban resolve`).
/// The table is DB-scoped (F17); the workspace comes from the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRow {
    /// The record in conflict.
    pub node_id: String,
    /// The file's `rev`.
    pub file_rev: i64,
    /// The database's `rev`.
    pub db_rev: i64,
    /// Record hash of the file (`serialize::record_hash`).
    pub file_hash: String,
    /// Record hash of the database record.
    pub db_hash: String,
    /// Unix seconds when the conflict was detected (last upsert).
    pub detected_at: i64,
}

/// The database's bookkeeping entries (`index_meta`, DB-scoped: schema version, `db_id`,
/// policy hash, pipeline versions).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexMeta {
    /// Every key with its value.
    pub entries: std::collections::BTreeMap<String, String>,
}

impl IndexMeta {
    /// Key of the `effective_hash()` of the policy the whole database was last rescanned
    /// against (MD-19).
    pub const POLICY_HASH: &'static str = "policy_hash";
    /// Key of the schema version (core-spec §7).
    pub const SCHEMA_VERSION: &'static str = "schema_version";
    /// Key of the database identity.
    pub const DB_ID: &'static str = "db_id";
    /// Key of the pack lineage the database belongs to (spec 05 SS-26; set once at open, kept
    /// by copies and forks).
    pub const PACK_LINEAGE_ID: &'static str = "pack_lineage_id";

    /// The value stored under `key`, if any.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }
}

// ---- discovered labels, candidates and `kind_stats` (engine-spec EN-F5, EN-F5a, EN-F6, EN-F49;
// engine-plan E1.8) ----

/// The discovered-labels rules a write decides by (EN-F5): the policy of the loaded
/// ontologies and the per-workspace cap. Carried on [`WriteCtx`] like the governed set; the
/// default is the shipped `review` policy with the `[discovered] cap` register default, so a
/// context built without a registry behaves as the shipped ontology says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRules {
    /// What happens to an undeclared label.
    pub policy: crate::ontology::meta::DiscoveredPolicyName,
    /// At most this many distinct `x:` kinds per workspace.
    pub cap: u32,
}

impl Default for DiscoveredRules {
    fn default() -> Self {
        Self {
            policy: crate::ontology::meta::DiscoveredPolicyName::Review,
            cap: crate::config::Discovered::default().cap,
        }
    }
}

/// Read-path filters every read surface threads to the store (EN-F49): candidates
/// (`node_candidate` rows, EN-F5a) are excluded unless `include_candidates`. The API, CLI and
/// MCP flag is E2.8's; the store parameter lands here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFilters {
    /// Also return candidate nodes.
    #[serde(default)]
    pub include_candidates: bool,
}

/// One `kind_stats` row (EN-F6): the per-workspace counters of a discovered (or promoted)
/// kind. `mean_score` stays `NULL` until E2 scores claims.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KindStat {
    /// Workspace the counters belong to.
    pub workspace_id: String,
    /// Namespace (`x` for a discovered kind).
    pub ns: String,
    /// Bare kind name.
    pub kind: String,
    /// Active nodes of the kind in the workspace.
    pub n_nodes: u64,
    /// Distinct sources (session or agent identity) that wrote them.
    pub n_sources: u64,
    /// Mean extractor score (E2; `None` until then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_score: Option<f64>,
    /// Unix seconds of the first node.
    pub first_seen: i64,
    /// Unix seconds of the latest node.
    pub last_seen: i64,
}

impl KindStat {
    /// The stored name of the kind (`x:Vendor`, `infra:Vendor`, `Vendor` for a bare namespace).
    #[must_use]
    pub fn stored(&self) -> String {
        match crate::ontology::Ns::parse(&self.ns) {
            Ok(ns) => crate::ontology::stored_name(&crate::ontology::Qualified {
                ns,
                name: self.kind.clone(),
            }),
            Err(_) => format!("{}:{}", self.ns, self.kind),
        }
    }
}

/// What `promote_kind` did (EN-F5): one transaction, one `events` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PromoteReport {
    /// The discovered kind that was renamed (`x:Vendor`).
    pub from: String,
    /// The declared kind it became (stored name).
    pub to: String,
    /// Nodes whose `kind` changed.
    pub nodes: u64,
    /// `node_candidate` rows of reason `discovered_kind` deleted.
    pub candidates_released: u64,
    /// `seq` of the `events` row.
    pub event_seq: u64,
}

/// What `prune_kinds` listed or did (EN-F5): `x:` kinds below `promote_when.min_count` whose
/// `last_seen` is older than `prune_after_days`; `dry_run` lists and writes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PruneReport {
    /// Whether anything was written.
    pub dry_run: bool,
    /// The prunable kinds (stored names), listed in both modes.
    pub kinds: Vec<String>,
    /// Candidate nodes tombstoned (0 on a dry run).
    pub nodes_tombstoned: u64,
    /// Labels refused at the cap so far in the workspace (`discovered_overflow.n_rejected`;
    /// reported, never reset).
    pub n_rejected: u64,
}
