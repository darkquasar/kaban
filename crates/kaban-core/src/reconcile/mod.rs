//! Reconciliation contracts, canonical identities and input seams (EN-E9/F12a).
mod explain;
pub use explain::{
    CandidateView, Contribution, Explanation, LineageView, PriorsObs, PriorsOutcome, SupporterView,
    WEIGHTED_VOTE_V1,
};
mod fixed;
pub use fixed::{Fixed, FixedError, LogWeight};
mod priors;
pub use priors::{Priors, PriorsError};

use crate::ontology::resolve::{ResolvedKind, ResolvedRel};
use crate::{Ontology, WorkspaceId, canonical_json, stored_name};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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
/// Entity resolution: blocking and Fellegi-Sunter scoring (EN-F51; E2.10).
pub mod er;
vocabulary!(
    ClaimClass,
    "Conflict classes in the assertion CHECK set.",
    {
        Type => "type",
        CanonicalId => "canonical_id",
        Edge => "edge",
        KindOfRecord => "kind_of_record",
        PropValue => "prop_value",
        Identity => "identity",
    }
);
vocabulary!(
    Rank,
    "Assertion rank (EN-F12b): `preferred` claims vote alone when one exists, `deprecated` claims never vote.",
    {
        Preferred => "preferred",
        Normal => "normal",
        Deprecated => "deprecated",
    }
);
vocabulary!(
    Layer,
    "Assertion source layers in strongest-source order (engine-spec EN-E7); the text index layers `fts` and `fuzzy` (EN-F22, spec 05 OWN-M) close the list.",
    {
        Human => "human",
        V4 => "v4",
        Reconcile => "reconcile",
        Script => "script",
        Llm => "llm",
        Gliner => "gliner",
        Gazetteer => "gazetteer",
        Embed => "embed",
        Fts => "fts",
        Fuzzy => "fuzzy",
    }
);
vocabulary!(
    Method,
    "Closed assertion method vocabulary.",
    {
        Id => "id",
        Path => "path",
        Exact => "exact",
        Alias => "alias",
        Symbol => "symbol",
        Tag => "tag",
        Embed => "embed",
        Gliner => "gliner",
        Llm => "llm",
        Human => "human",
        Measured => "measured",
        Rule => "rule",
        Heuristic => "heuristic",
        Link => "link",
        Accept => "accept",
        Remember => "remember",
        Er => "er",
        ErJudge => "er_judge",
        Reconciled => "reconciled",
        IngestSubmit => "ingest_submit",
    }
);
vocabulary!(
    AssertionStatus,
    "Mutable projection disposition, separate from immutable assertions.",
    {
        Pending => "pending",
        Winner => "winner",
        Dissent => "dissent",
        Candidate => "candidate",
        Unresolved => "unresolved",
        Rejected => "rejected",
        Superseded => "superseded",
    }
);
vocabulary!(
    CandidateReason,
    "Why v1 leaves a claim for review.",
    {
        BelowMargin => "below_margin",
        BelowMinConfidence => "below_min_confidence",
        Governed => "governed",
        DiscoveredKind => "discovered_kind",
        GovernedSemantics => "governed_semantics",
        Merge => "merge",
        Proposed => "proposed",
    }
);
vocabulary!(
    UnresolvedReason,
    "Conflicts whose resolution requires an explicit action.",
    {
        PreferredConflict => "preferred_conflict",
        HumanProtected => "human_protected",
        AcceptedRecordProtected => "accepted_record_protected",
        IdentityConflict => "identity_conflict",
    }
);

/// Canonical JSON bytes cached at construction, with parsed data for validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Value", into = "Value")]
pub struct CanonicalJson {
    bytes: String,
    value: Value,
}
impl CanonicalJson {
    /// Canonicalize once at the assertion input seam.
    pub fn new(value: Value) -> Self {
        Self {
            bytes: canonical_json(&value),
            value,
        }
    }
    /// Byte-wise comparison representation.
    pub fn as_str(&self) -> &str {
        &self.bytes
    }
    /// Parsed contents, borrowed without allocation.
    pub fn as_value(&self) -> &Value {
        &self.value
    }
}
impl From<Value> for CanonicalJson {
    fn from(v: Value) -> Self {
        Self::new(v)
    }
}
impl From<CanonicalJson> for Value {
    fn from(v: CanonicalJson) -> Self {
        v.value
    }
}

/// Stable blake3 hex identity, never the storage row id.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssertionKey(pub String);

/// The assertion subject, in storage coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Subject {
    /// An exact mention span; overlapping spans remain distinct.
    Span {
        /// Chunk row identifier.
        chunk_rid: i64,
        /// Byte offset.
        offset: u32,
        /// Byte length.
        len: u32,
    },
    /// An existing node.
    Node(i64),
    /// An edge triple. `with_key` synchronizes predicate and object fields.
    Edge {
        /// Source row.
        src: i64,
        /// Stored relationship name.
        rel: String,
        /// Target row.
        dst: i64,
    },
    /// A record whose kind is being proposed.
    Record(i64),
}

/// Immutable evidence. Call `with_key` after assembling input fields, before submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assertion {
    /// Stable identity.
    pub key: AssertionKey,
    /// Workspace scope.
    pub workspace_id: WorkspaceId,
    /// Subject coordinates.
    pub subject: Subject,
    /// Conflict class.
    pub claim_class: ClaimClass,
    /// Property or relationship name.
    pub predicate: Option<String>,
    /// Target node, if present.
    pub object_rid: Option<i64>,
    /// Canonical claim value. Record-kind claims use {kind, props}; types use a kind string.
    pub value: CanonicalJson,
    /// Source layer.
    pub layer: Layer,
    /// Extraction method.
    pub method: Method,
    /// Extractor identity.
    pub extractor_id: Option<String>,
    /// Extractor version.
    pub extractor_version: Option<String>,
    /// Source identity.
    pub source_id: Option<String>,
    /// Shared evidence lineage.
    pub lineage_id: String,
    /// Raw confidence, validated by `with_key`.
    pub raw_score: Fixed,
    /// Rank partition seam for E2.3.
    pub rank: Rank,
    /// Explicit reason for changing rank.
    pub rank_reason: Option<String>,
    /// Calibration version, excluded from identity.
    pub calibration_version: u32,
    /// Previous evidence key.
    pub supersedes: Option<AssertionKey>,
    /// Timestamp, excluded from identity.
    pub ts: i64,
}
impl Assertion {
    /// Validate the score and compute the sixteen-field identity (EN-F12a).
    /// # Errors
    /// Rejects a score above 10000, including direct tuple construction.
    pub fn with_key(mut self) -> Result<Self, FixedError> {
        Fixed::new(self.raw_score.0)?;
        if let Subject::Edge { rel, dst, .. } = &self.subject {
            self.predicate = Some(rel.clone());
            self.object_rid = Some(*dst);
        }
        let (kind, rid, chunk, offset, len) = match &self.subject {
            Subject::Span {
                chunk_rid,
                offset,
                len,
            } => ("span", None, Some(*chunk_rid), Some(*offset), Some(*len)),
            Subject::Node(rid) => ("node", Some(*rid), None, None, None),
            Subject::Record(rid) => ("record", Some(*rid), None, None, None),
            Subject::Edge { src, .. } => ("edge", Some(*src), None, None, None),
        };
        let identity = json!({"workspace_id":self.workspace_id, "subject_kind":kind,
            "subject_rid":rid, "chunk_rid":chunk, "offset":offset, "len":len,
            "claim_class":self.claim_class, "predicate":self.predicate, "object_rid":self.object_rid,
            "value":self.value.as_value(), "layer":self.layer, "method":self.method,
            "extractor_id":self.extractor_id, "extractor_version":self.extractor_version,
            "source_id":self.source_id, "lineage_id":self.lineage_id});
        self.key = AssertionKey(
            blake3::hash(canonical_json(&identity).as_bytes())
                .to_hex()
                .to_string(),
        );
        Ok(self)
    }
    /// Kind used for ontology gates. Canonical-id claims may carry `{kind, ...}`.
    pub fn kind_name(&self) -> Option<&str> {
        self.value
            .as_value()
            .as_str()
            .filter(|_| {
                matches!(
                    self.claim_class,
                    ClaimClass::Type | ClaimClass::KindOfRecord
                )
            })
            .or_else(|| self.value.as_value().get("kind").and_then(Value::as_str))
    }
}

/// The fourth coordinate of a conflict group: the property of a `prop_value` claim, the
/// second node of an `identity` pair (E2.10b BR-2: an identity group is one unordered pair,
/// stored with the lesser rid as the subject and the greater as the object, so A-B and A-C
/// are two groups and a distinction about one is no vote on the other), nothing otherwise.
fn group_detail(a: &Assertion) -> Value {
    match a.claim_class {
        ClaimClass::PropValue => json!(a.predicate),
        ClaimClass::Identity => json!(a.object_rid),
        _ => Value::Null,
    }
}

/// Canonical, workspace-qualified conflict key. The batch caller decides ascending keys.
pub fn group_key(a: &Assertion) -> String {
    canonical_json(&json!([
        a.workspace_id,
        a.claim_class,
        a.subject,
        group_detail(a)
    ]))
}

/// Compare conflict coordinates without allocating a group key.
pub fn same_group(a: &Assertion, b: &Assertion) -> bool {
    a.workspace_id == b.workspace_id
        && a.claim_class == b.claim_class
        && a.subject == b.subject
        && group_detail(a) == group_detail(b)
}

/// The evidence lineage of a claim (EN-F12b): what counts once in a vote.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineageParts<'a> {
    /// A machine layer: one source document (its content hash or source id) read by one
    /// extractor version; repeated extraction or copied text never adds weight.
    Machine {
        /// The source document's content hash, or its `source_id` when it has none.
        content_hash_or_source: &'a str,
        /// Extractor identity.
        extractor_id: &'a str,
        /// Extractor version.
        version: &'a str,
    },
    /// A human action: the actor and, when the store assigns one, the event it was recorded
    /// under (`None` is the actor's standing lineage, so an identical repeated `link` is the
    /// same claim, EN-F12a N5).
    Human {
        /// The actor's stored spelling (`cli`, `agent`).
        actor: &'a str,
        /// The `events.seq` or `ChangeLog` `seq` the action was recorded under.
        event_id: Option<u64>,
    },
    /// A snapshot claim of one repository commit.
    V4 {
        /// Repository identity.
        repo: &'a str,
        /// Commit hash.
        commit: &'a str,
    },
}

/// The stored `lineage_id` of a claim (EN-F12b): `blake3` hex over the machine parts,
/// `human:<actor>[:<event_id>]`, `v4:<repo>:<commit>`. `layer` names the partition only; the
/// parts carry the identity, so a human claim never collides with a machine hash.
pub fn lineage_id(layer: Layer, parts: &LineageParts<'_>) -> String {
    match parts {
        LineageParts::Machine {
            content_hash_or_source,
            extractor_id,
            version,
        } => {
            let identity = canonical_json(&json!([
                layer,
                content_hash_or_source,
                extractor_id,
                version
            ]));
            blake3::hash(identity.as_bytes()).to_hex().to_string()
        }
        LineageParts::Human { actor, event_id } => match event_id {
            Some(id) => format!("human:{actor}:{id}"),
            None => format!("human:{actor}"),
        },
        LineageParts::V4 { repo, commit } => format!("v4:{repo}:{commit}"),
    }
}

/// Whether `a` votes in `group` (EN-F12b rank partition): a `deprecated` claim never votes, a
/// `normal` claim votes only when no `preferred` supporter exists, and a claim another member
/// of the group `supersedes` is retired from the vote.
pub fn votes(a: &Assertion, group: &[Assertion]) -> bool {
    if a.rank == Rank::Deprecated {
        return false;
    }
    if group.iter().any(|b| b.supersedes.as_ref() == Some(&a.key)) {
        return false;
    }
    a.rank == Rank::Preferred || !group.iter().any(|b| b.rank == Rank::Preferred)
}

/// What a group's projection must not overwrite (EN-F12c), built by the store before each
/// decision from the group's current statuses and its materialised edge.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProtectionSet {
    /// The human supporter whose claim currently wins the group: a non-human value against it
    /// is `Unresolved{HumanProtected}`, a non-human supporter agreeing with it is dissent.
    pub human_winner: Option<AssertionKey>,
    /// The group is an `edge` whose materialised row touches an accepted governed record
    /// over a `governed_semantics` rel or `GOVERNS`/`ABOUT`/`EVIDENCE_FOR`: no non-human
    /// verdict changes the row.
    pub accepted_record_edge: bool,
}
impl ProtectionSet {
    /// Whether any protection was supplied.
    pub fn is_empty(&self) -> bool {
        self.human_winner.is_none() && !self.accepted_record_edge
    }
}

/// Loaded policy with floating ontology thresholds converted once at construction.
pub struct ReconcileCtx<'a> {
    /// Source ontology; float fields are never used in a decision.
    pub ontology: &'a Ontology,
    /// Loaded reliability table.
    pub priors: &'a Priors,
    /// Type margin gate.
    pub tau_type: Fixed,
    /// Canonical-id margin gate.
    pub tau_id: Fixed,
    /// What this group's projection must not overwrite (EN-F12c).
    pub protection: &'a ProtectionSet,
    kinds: BTreeMap<String, (&'a ResolvedKind, Fixed)>,
    rels: BTreeMap<String, (&'a ResolvedRel, Fixed)>,
}
impl<'a> ReconcileCtx<'a> {
    /// Freeze config and ontology numbers at the input seam.
    /// # Errors
    /// Rejects invalid thresholds rather than clamping them.
    pub fn new(
        ontology: &'a Ontology,
        priors: &'a Priors,
        policy: &crate::config::Reconcile,
        protection: &'a ProtectionSet,
    ) -> Result<Self, FixedError> {
        let kinds = ontology
            .kinds
            .iter()
            .map(|(q, k)| {
                Ok((
                    stored_name(q),
                    (k, Fixed::from_f64(k.min_confidence.unwrap_or(0.0))?),
                ))
            })
            .collect::<Result<_, FixedError>>()?;
        let rels = ontology
            .rels
            .iter()
            .map(|(q, r)| {
                Ok((
                    stored_name(q),
                    (r, Fixed::from_f64(r.confidence_required.unwrap_or(0.0))?),
                ))
            })
            .collect::<Result<_, FixedError>>()?;
        Ok(Self {
            ontology,
            priors,
            tau_type: Fixed::from_f64(policy.tau_type)?,
            tau_id: Fixed::from_f64(policy.tau_id)?,
            protection,
            kinds,
            rels,
        })
    }
    /// A declared kind and its fixed minimum confidence.
    pub fn kind(&self, name: &str) -> Option<(&'a ResolvedKind, Fixed)> {
        self.kinds.get(name).copied()
    }
    /// A relationship and its fixed confidence threshold.
    pub fn rel(&self, name: &str) -> Option<(&'a ResolvedRel, Fixed)> {
        self.rels.get(name).copied()
    }
}

/// The complete pure decision. Ranked probabilities and confidence are fixed point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Verdict {
    /// May be projected by the caller.
    Winner {
        /// Representative evidence key.
        assertion_key: AssertionKey,
        /// Normalized vote, or maximum edge/property confidence.
        confidence: Fixed,
        /// Difference from second place.
        margin: Fixed,
        /// Non-winning evidence, in key order.
        dissent: Vec<AssertionKey>,
    },
    /// Requires review. Record-kind winners are always proposed.
    Candidate {
        /// Representatives in decision order.
        ranked: Vec<(AssertionKey, Fixed)>,
        /// Why review is needed.
        reason: CandidateReason,
    },
    /// A conflict requiring an explicit resolution (E2.3).
    Unresolved {
        /// Representatives in decision order.
        ranked: Vec<(AssertionKey, Fixed)>,
        /// Conflict category.
        reason: UnresolvedReason,
    },
    /// Invalid or unsupported input.
    Rejected {
        /// Stable machine-readable reason.
        reason: String,
    },
}

/// The sole decision entry, injected into the store by E2.2.
pub trait Reconciler: Send + Sync {
    /// Decide one conflict group without I/O or mutation.
    fn decide(&self, group: &[Assertion], ctx: &ReconcileCtx<'_>) -> Verdict;
}
