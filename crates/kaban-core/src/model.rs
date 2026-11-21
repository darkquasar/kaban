//! Graph model types: nodes, edges, schema entries, queries and outcomes.

use crate::audit::WorkspaceId;
use crate::ids::NodeId;
use crate::lifecycle::{Lifecycle, StoreScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A SQL scalar used in rollup keys and results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SqlValue {
    /// SQL NULL.
    Null,
    /// Signed 64-bit integer.
    Integer(i64),
    /// Binary64 real.
    Real(f64),
    /// UTF-8 text, including text timestamps.
    Text(String),
    /// Arbitrary bytes.
    Blob(Vec<u8>),
}

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
    /// Which database the record lives in.
    pub store_scope: StoreScope,
    /// Globs/paths a governing record covers (distinct from `store_scope`).
    pub path_scope: Vec<String>,
    /// Monotonic revision, bumped on every change (F13).
    pub rev: i64,
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
    /// JSON properties (`{}` when none).
    pub props: Value,
    /// Dedup hash over (workspace, kind, trimmed text).
    pub content_hash: String,
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
    /// The declaration columns (`rules`, `rank`, `column_weights`, `extract`, `display`),
    /// flattened into the entry; empty for rels and for a kind that declares none (engine-plan
    /// E1.8, E1.7 R2).
    #[serde(default, flatten)]
    pub declaration: TypeDeclaration,
}

/// The JSON declaration columns of a kind (engine-spec §3.2 `schema_type`; EN-F4, EN-F29,
/// EN-F25): written at the load seam from the resolved ontology as canonical JSON (E1.1
/// `canon.rs`), `None` where the kind declares none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TypeDeclaration {
    /// Relationship edges live in a commit graph, without a SQL row view.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub per_commit: bool,
    /// Typed columns projected from this label's properties.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<crate::ontology::ColumnDecl>,
    /// Named aggregates over active rows of the label.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollups: Vec<crate::ontology::RollupDecl>,
    /// Extra property paths searchable through the text index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fts_fields: Vec<String>,
    /// The `rules` block (EN-F4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Value>,
    /// The `rank` block (EN-F29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<Value>,
    /// The `column_weights` block (EN-F29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_weights: Option<Value>,
    /// The `extract` block (EN-F25, EN-F36).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<Value>,
    /// The display labels (`rules.display`).
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
            columns: kind.columns.clone(),
            per_commit: false,
            rollups: kind.rollups.clone(),
            fts_fields: kind.fts_fields.clone(),
            rules: kind.rules.as_ref().and_then(canon),
            rank: kind.rank.as_ref().and_then(canon),
            column_weights: kind.column_weights.as_ref().and_then(canon),
            extract: kind.extract.as_ref().and_then(canon),
            display: kind
                .rules
                .as_ref()
                .and_then(|r| r.display.as_ref())
                .and_then(canon),
        }
    }

    /// Each column as its canonical JSON text, in table order `(rules, rank, column_weights,
    /// extract, display)`.
    #[must_use]
    pub fn texts(&self) -> [Option<String>; 5] {
        let text = |v: &Option<Value>| v.as_ref().map(crate::canon::canonical_json);
        [
            text(&self.rules),
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

/// Recall match modes (PC-16): how the text index matches the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RecallMode {
    /// Lexical full-text retrieval.
    Lexical,
    /// Fuzzy (typo-tolerant) retrieval by the text index (spec 05 OWN-M).
    Fuzzy,
}

/// Parameters of a recall.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallQuery {
    /// Explicit match modes; absent uses the enabled defaults.
    #[serde(default)]
    pub modes: Option<Vec<RecallMode>>,
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
    /// Also return archived nodes (superseded records, archived notes) reached through graph
    /// expansion (core-spec §4 "`include_archived` opts in", M3-12); the text index holds active
    /// rows only, so archived nodes never seed a recall.
    pub include_archived: bool,
    /// Restrict the lexical seeds to notes linked `ABOUT` or `EVIDENCE_FOR` to a `CodeRef`
    /// matching this filter (core-spec F3 "about this `CodeRef`"; `None` = no restriction).
    pub r#ref: Option<CodeRefFilter>,
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
    /// Keep only edges whose `score` is at least this.
    #[serde(default)]
    pub min_score: Option<Fixed>,
    /// Keep only edges of these producers (empty = any).
    #[serde(default)]
    pub producers: Vec<Producer>,
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
    /// EN-F46: the edge-level filter, appended to every neighbour query before the hub cut.
    #[serde(default)]
    pub edge_filter: Option<EdgeFilter>,
    /// Max nodes (0 = default 100, capped at 500).
    pub limit: usize,
    /// Restrict to this workspace (`None` = any); start ids outside it are not found and
    /// neighbours outside it are not returned (core-spec S6).
    #[serde(default)]
    pub workspace_id: Option<String>,
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
            edge_filter: None,
            limit: 0,
            workspace_id: None,
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
}

/// What `forget` left behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetOutcome {
    /// Lifecycle after the call.
    pub lifecycle: Lifecycle,
    /// Highest `changes.seq` written by this call (F11).
    pub outbox_seq: u64,
    /// A purge committed: the content is unavailable to every read from now on (core-spec S5).
    /// The other modes keep the content and revoke nothing.
    pub revoked: bool,
    /// The purge's freed pages were reclaimed by this call. The post-commit cleanup (index
    /// segments merged, the WAL folded into the main file and truncated) reclaims none, so a
    /// purge reports `false` with `cleanup` naming the residue; `false` for the other modes.
    pub physically_cleaned: bool,
    /// Why a purge is not physically cleaned: the free-page residue a VACUUM removes (`kaban
    /// doctor --optimize --vacuum`), after `busy` (a reader still held the WAL or the lock) or
    /// the failure findings when a cleanup step did not run through. The revocation stands.
    pub cleanup: Option<String>,
}

/// How to forget a node (core-spec F5).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ForgetMode {
    /// Keep history, hide from every default read, never re-propose as a duplicate (default).
    #[default]
    Tombstone,
    /// The same lifecycle effect as `Tombstone`: the record is hidden from default reads.
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
/// be shipped to a store elsewhere; `edge_diff` defaults when absent, so a batch written
/// before it existed still parses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteBatch {
    /// Tenant of every row the batch touches (F17). A node's own `workspace_id` field is
    /// ignored; an existing row with the same id in another workspace is `NotFound` (WD-5).
    pub workspace_id: WorkspaceId,
    /// Operations, applied node ops first (in order), then edge ops, then the edge diff.
    pub ops: Vec<Op>,
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
    /// A batch under the default [`EdgeDiff::None`] rule (no deletion).
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, ops: Vec<Op>) -> Self {
        Self {
            workspace_id,
            ops,
            artifacts: Vec::new(),
            gazetteer_claim_sets: Vec::new(),
            edge_diff: EdgeDiff::None,
        }
    }
}

/// The end-of-batch edge diff rule (engine-spec EN-E4; docs-ingest-spec D5 [A-3]; OWN-83
/// (4)): which outgoing edges of an upserted node that the batch did not name are deleted.
/// Producer isolation: both deleting rules reach only the unnamed rows whose stored producer
/// is the batch's, so a producer rerun never deletes another producer's row. Wire form:
/// `{"mode": "complete", "producer": "document_ingest"}`, `{"mode": "owned", "producer":
/// "extractor:llm"}`, `{"mode": "none"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EdgeDiff {
    /// The batch names the complete outgoing edge set of each upserted node for `producer`
    /// (MD-23 under EN-E4): the unnamed rows of that producer are deleted.
    Complete {
        /// The producer whose complete set this is, for example [`Producer::DocumentIngest`].
        producer: Producer,
    },
    /// Only the unnamed outgoing edges `producer` wrote are deleted.
    Owned {
        /// The producer, for example an [`Producer::Extractor`].
        producer: Producer,
    },
    /// No deletion at all (the [`WriteBatch::new`] default); the edges are authored.
    #[default]
    None,
}

impl EdgeDiff {
    /// The producer every edge of the batch is written under unless an edge names its own
    /// (`props.producer`, the trusted batch boundary): the named producer, else
    /// [`Producer::Authored`].
    #[must_use]
    pub fn producer(&self) -> Producer {
        match self {
            Self::Complete { producer } | Self::Owned { producer } => *producer,
            Self::None => Producer::Authored,
        }
    }

    /// The producer whose unnamed rows the diff deletes, if any.
    #[must_use]
    pub fn deleting(&self) -> Option<Producer> {
        match self {
            Self::Complete { producer } | Self::Owned { producer } => Some(*producer),
            Self::None => None,
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
    /// store accepts it only under [`crate::reconciliation_transition`] (an
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
    /// Edges the batch named on a `(src, dst, rel)` triple another producer wrote first: the
    /// first writer keeps the row and nothing was written (owner decision 2026-11-14).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub edges_kept_by_first_writer: u32,
    /// Per node op, in batch order, what the store did (DI1.8, RD2-8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<OpOutcome>,
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
            || self.dup_key != n.dup_key
    }
}

/// What the rescan job decided for one node (core-spec S4; core-plan C2.7). The store applies
/// the action inside the batch's transaction.
#[derive(Debug, Clone)]
pub enum RescanAction {
    /// The node passes the loaded policy: nothing changes.
    Keep,
    /// Redact hits: the masked strings replace the stored ones in place (new `rev`, new
    /// `content_hash`).
    Redact {
        /// Every rewritable string after masking (boxed: the variant is much larger than
        /// `Keep`).
        strings: Box<RedactedStrings>,
    },
    /// A reject hit: the node is tombstoned (lifecycle only) with a `policy-reject` event.
    Reject,
}

/// What the rescan job decided for one stored edge: its `props` are the only content an edge
/// persists beyond schema names (MED-01 of the T3 review). The store applies the action inside
/// the batch's transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeRescanAction {
    /// The edge passes the loaded policy: nothing changes.
    Keep,
    /// Redact hits: the masked `props` replace the stored ones in place.
    Redact {
        /// The whole `props` object after masking.
        props: Value,
    },
    /// A reject hit: the edge is deleted (an edge has no lifecycle to hide it with).
    Reject,
}

/// Decides, per stored row, what a rescan batch does (core-spec S4). Runs inside the store's
/// batch transaction and must not call back into the store.
pub trait RescanJudge {
    /// The action for one non-purged node.
    fn node(&mut self, node: &Node) -> RescanAction;
    /// The action for one edge.
    fn edge(&mut self, edge: &Edge) -> EdgeRescanAction;
}

/// A node-only judge: the closure decides per node and every edge is kept.
impl<F: FnMut(&Node) -> RescanAction> RescanJudge for F {
    fn node(&mut self, node: &Node) -> RescanAction {
        self(node)
    }
    fn edge(&mut self, _edge: &Edge) -> EdgeRescanAction {
        EdgeRescanAction::Keep
    }
}

/// Where a rescan resumes. A full walk visits every node, then every edge, then catches the
/// change log up; once the walk completed (the `policy_hash` marker matches) a run starts at
/// the change log directly. The change-log position itself is persisted by the store (a
/// walk's own position, then [`IndexMeta::POLICY_SCAN_SEQ`] once the walk completes), so it
/// survives a cancelled or crashed run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub enum RescanCursor {
    /// Before the first batch of a run: the store picks the full walk or the change log.
    #[default]
    Start,
    /// Walking nodes after this row id.
    Nodes(u64),
    /// Walking edges after this row id (every node is walked).
    Edges(u64),
    /// The walk is complete; rows the change log names after the watermark are judged.
    Changes,
    /// Nothing is left to judge.
    Done,
}

/// What one `rescan` batch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RescanReport {
    /// Rows (nodes and edges) judged in this batch.
    pub scanned: u64,
    /// Rows that passed unchanged.
    pub kept: u64,
    /// Rows masked in place.
    pub redacted: u64,
    /// Nodes tombstoned and edges deleted.
    pub rejected: u64,
    /// Where the next batch starts ([`RescanCursor::Done`] when nothing is left).
    pub cursor: RescanCursor,
}

impl std::ops::AddAssign for RescanReport {
    fn add_assign(&mut self, rhs: Self) {
        self.scanned += rhs.scanned;
        self.kept += rhs.kept;
        self.redacted += rhs.redacted;
        self.rejected += rhs.rejected;
        self.cursor = rhs.cursor;
    }
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
    /// Key of the change-log watermark the [`Self::POLICY_HASH`] policy is certified to: every
    /// node and edge change with a `seq` at or below it has been judged by that policy. It
    /// moves with the marker, at a complete walk or in a run of the same policy, never in a
    /// walk under another policy (a walk keeps its own position until it completes). A
    /// mutation freshness signal independent of the rows (no per-record policy stamp,
    /// REC-08): rows written later, by any process under any policy, are judged by the next
    /// run.
    pub const POLICY_SCAN_SEQ: &'static str = "policy_scan_seq";
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

// Producer vocabulary moved out of the deleted reconciliation module (T3, PC-16): `Producer`
// replaces the old `Layer` and `Owner` vocabularies; `Method` keeps its non-governance values.
macro_rules! vocabulary {
    ($name:ident, $doc:literal, {$($variant:ident => $text:literal),+ $(,)?}) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema)]
        #[non_exhaustive]
        pub enum $name { $(#[doc = concat!("`", $text, "`")] #[serde(rename = $text)] $variant),+ }
        impl $name {
            /// The stored vocabulary spelling.
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $text),+ } }
            /// Parse only a known stored spelling.
            pub fn parse(value: &str) -> Option<Self> { match value { $($text => Some(Self::$variant)),+, _ => None } }
            /// All currently supported vocabulary values.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}
/// Which extractor produced an `extractor:*` record (PC-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtractorKind {
    /// A language-model extractor.
    Llm,
    /// The `GLiNER` span extractor.
    Gliner,
    /// A user script.
    Script,
}

/// Who produced an edge or a derived artifact, named for the source its records come from
/// (PC-16; who acted stays in the actor fields). One identity per row, stored in the
/// `edge.producer` and `derived_artifact.producer` columns. A
/// producer rerun deletes only the rows it wrote, and the first producer to write a
/// `(src, dst, rel)` triple keeps the row (owner decision 2026-11-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Producer {
    /// `remember` and `link`: records a person or an agent wrote directly.
    Authored,
    /// The hook event stream.
    HookEvents,
    /// The document ingest pipeline (documents, sections, chunks and their structure).
    DocumentIngest,
    /// The mention dictionary run over ingested chunks (`gazetteer@1`).
    Gazetteer,
    /// A structured extractor submission.
    Extractor(ExtractorKind),
    /// A SCIP code index import.
    ScipImport,
    /// The git history producer.
    GitHistory,
}

impl Producer {
    /// Every producer, in the `stats.producers` order (one `Extractor` per kind).
    pub const ALL: &'static [Self] = &[
        Self::Authored,
        Self::HookEvents,
        Self::DocumentIngest,
        Self::Gazetteer,
        Self::Extractor(ExtractorKind::Llm),
        Self::Extractor(ExtractorKind::Gliner),
        Self::Extractor(ExtractorKind::Script),
        Self::ScipImport,
        Self::GitHistory,
    ];

    /// The stored and wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authored => "authored",
            Self::HookEvents => "hook_events",
            Self::DocumentIngest => "document_ingest",
            Self::Gazetteer => "gazetteer",
            Self::Extractor(ExtractorKind::Llm) => "extractor:llm",
            Self::Extractor(ExtractorKind::Gliner) => "extractor:gliner",
            Self::Extractor(ExtractorKind::Script) => "extractor:script",
            Self::ScipImport => "scip_import",
            Self::GitHistory => "git_history",
        }
    }

    /// Parse only a known stored spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.as_str() == value)
    }

    /// The `stats.producers` row name: the producer without its extractor kind.
    #[must_use]
    pub const fn family(self) -> &'static str {
        match self {
            Self::Extractor(_) => "extractor",
            other => other.as_str(),
        }
    }
}

impl std::fmt::Display for Producer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Producer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Producer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| serde::de::Error::custom("unknown_producer"))
    }
}

impl schemars::JsonSchema for Producer {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Producer".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let values: Vec<&str> = Self::ALL.iter().map(|p| p.as_str()).collect();
        schemars::json_schema!({
            "description": "The producer of an edge (PC-16).",
            "type": "string",
            "enum": values,
        })
    }
}
vocabulary!(
    Method,
    "Closed edge method vocabulary: how a producer matched or wrote an edge.",
    {
        Id => "id",
        Path => "path",
        Exact => "exact",
        Alias => "alias",
        Symbol => "symbol",
        Tag => "tag",
        Gliner => "gliner",
        Llm => "llm",
        Measured => "measured",
        Rule => "rule",
        Heuristic => "heuristic",
        Link => "link",
        Remember => "remember",
        IngestSubmit => "ingest_submit",
    }
);

/// A confidence in units of 1/10000. Use `new` or `from_f64` at input seams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Fixed(pub u32);

/// An invalid confidence or signed weight at the input seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("fixed_point_out_of_range")]
pub struct FixedError;

impl Fixed {
    /// Zero confidence.
    pub const ZERO: Self = Self(0);
    /// Unit confidence.
    pub const ONE: Self = Self(10_000);

    /// Validate integer confidence without clamping.
    /// # Errors
    /// Returns `FixedError` above 10000.
    pub const fn new(value: u32) -> Result<Self, FixedError> {
        if value <= 10_000 {
            Ok(Self(value))
        } else {
            Err(FixedError)
        }
    }

    /// Round half to even once at the floating input seam.
    /// # Errors
    /// Rejects non-finite values and values outside [0, 1].
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn from_f64(value: f64) -> Result<Self, FixedError> {
        if !(0.0..=1.0).contains(&value) {
            return Err(FixedError);
        }
        Self::new((value * 10_000.0).round_ties_even() as u32)
    }
}
/// The four-decimal rendering of EN-F12a (`6556` is `0.6556`, `10000` is `1.0000`): the one
/// text form of a weight in `explain` and `doctor`; the integer stays the compared value.
impl std::fmt::Display for Fixed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:04}", self.0 / 10_000, self.0 % 10_000)
    }
}
impl TryFrom<u32> for Fixed {
    type Error = FixedError;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<Fixed> for u32 {
    fn from(value: Fixed) -> Self {
        value.0
    }
}
