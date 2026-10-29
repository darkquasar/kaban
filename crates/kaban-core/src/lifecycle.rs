//! The two orthogonal state fields of a node (core-spec §4, SR-4): `lifecycle` is storage
//! visibility for every kind, `governance` exists only for the governed kinds: those whose
//! ontology declaration carries the `governed:` mixin (engine-spec EN-F4, OD-E-2), which for
//! the shipped `adr.yaml` are `Decision`, `Convention` and `Failure`. [`GovernedSet`] is that
//! membership read from a resolved ontology, by stored kind name. The transition tables are
//! the spec's, exactly; anything else is an [`InvalidTransition`] that adapters surface as a
//! tool-level error.

use crate::ontology::meta::{Acceptance, Display, Governed, OnLink, PromoteWhen};
use crate::ontology::{Ontology, OntologyError, stored_name};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use thiserror::Error;

/// `props.owner` of every node and edge the docs-ingest pipeline writes (docs-ingest-spec D5
/// [A-3], engine-spec EN-E4): the owner string `EdgeDiff::Owned` compares against and the only
/// owner [`GovernedSet::reconciliation_transition`] reactivates for.
pub const INGEST_OWNER: &str = "kaban-ingest";

/// The non-governed kinds the docs-ingest pipeline owns, the only kinds the reconciliation
/// transition may bring back from `tombstoned` (docs-ingest-spec D5, DI1.8).
const INGEST_RECONCILED_KINDS: [&str; 3] = ["Document", "Section", "Chunk"];

/// Storage visibility of a node. Reads default to [`Lifecycle::Active`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Lifecycle {
    /// Visible to recall, traverse and get.
    Active,
    /// Kept and reachable through graph expansion or `include_archived`; not in the text index.
    Archived,
    /// Hidden everywhere; never re-proposed as a duplicate. Terminal except for purge.
    Tombstoned,
    /// Text, index entries and edges removed; only the id row remains so it never reappears.
    Purged,
}

impl Lifecycle {
    /// Stable lowercase name used in the `node.lifecycle` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Tombstoned => "tombstoned",
            Self::Purged => "purged",
        }
    }
    /// Parse a column value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            "tombstoned" => Some(Self::Tombstoned),
            "purged" => Some(Self::Purged),
            _ => None,
        }
    }
}

/// Governance state of a `Decision`, `Convention` or `Failure`; [`Governance::None`] for every
/// other kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Governance {
    /// The kind is not governed.
    None,
    /// Recorded, not yet accepted (ranked below accepted in `why`).
    Proposed,
    /// Accepted with reason and path scope (event recorded).
    Accepted,
    /// Replaced by a newer record (`SUPERSEDES` edge plus event).
    Superseded,
    /// Declared no longer valid (event recorded).
    Invalidated,
}

impl Governance {
    /// Stable lowercase name used in the `node.governance` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Proposed => "proposed",
            Self::Accepted => "accepted",
            Self::Superseded => "superseded",
            Self::Invalidated => "invalidated",
        }
    }
    /// Parse a column value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "proposed" => Some(Self::Proposed),
            "accepted" => Some(Self::Accepted),
            "superseded" => Some(Self::Superseded),
            "invalidated" => Some(Self::Invalidated),
            _ => None,
        }
    }
}

/// Which database a record lives in (core-spec §4 `store_scope`, F18).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Default,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum StoreScope {
    /// The per-project database (default).
    #[default]
    Project,
    /// The personal global database.
    Personal,
}

impl StoreScope {
    /// Stable lowercase name used in the `node.store_scope` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Personal => "personal",
        }
    }
    /// Parse a column value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "project" => Some(Self::Project),
            "personal" => Some(Self::Personal),
            _ => None,
        }
    }
}

/// The governed kinds of a resolved ontology, by stored kind name (engine-spec EN-F4): every
/// kind whose declaration carries the `governed:` mixin, the kinds among them whose records
/// govern paths (`governs_paths: true`) and the rels whose `on_link: archive_target` archives
/// the target of a new edge. Built once per loaded ontology (the registry) and carried into
/// every write through `WriteCtx::governed`; [`GovernedSet::shipped`] is the embedded `adr`
/// contract. There is no empty default on purpose: a write context without a set would turn a
/// `Decision` into an ungoverned record.
///
/// The set also carries the acceptance tier of the kinds it does not govern (OWN-84 (1),
/// core-spec F4, E2.5g): a declared kind's top-level `acceptance` key and the loaded
/// `discovered.acceptance`, read by [`GovernedSet::tier`] for the EN-F5a candidate and the
/// EN-F13 `proposed` record an agent asks to accept. [`GovernedSet::kinds`],
/// [`GovernedSet::contains`] and [`GovernedSet::acceptance`] keep their meaning: governed only.
/// `auto_accept` is the loaded `discovered.auto_accept_when` (OWN-84 (2)), read by placement.
#[derive(Debug, Clone, PartialEq)]
pub struct GovernedSet {
    kinds: BTreeMap<String, Governed>,
    archive_on_link: BTreeSet<String>,
    governing: BTreeSet<String>,
    /// The declared ungoverned kinds carrying a top-level `acceptance`, by stored name.
    tiers: BTreeMap<String, Acceptance>,
    /// The `acceptance` of the last loaded `discovered` block (`Agent` when no block declares
    /// one): the tier of an undeclared `x:` label and of a declared kind without its own key.
    discovered_tier: Acceptance,
    /// The `auto_accept_when` of the last loaded `discovered` block (`None`: nothing is
    /// accepted without a queue item).
    auto_accept: Option<PromoteWhen>,
}

impl GovernedSet {
    /// The set of `ontology`: kinds with a `governed` block and rels with an `on_link`, under
    /// their stored names.
    #[must_use]
    pub fn from_ontology(ontology: &Ontology) -> Self {
        let kinds: BTreeMap<String, Governed> = ontology
            .kinds
            .values()
            .filter_map(|k| Some((stored_name(&k.id), k.governed.clone()?)))
            .collect();
        let governing = kinds
            .iter()
            .filter(|(_, g)| g.governs_paths)
            .map(|(name, _)| name.clone())
            .collect();
        let archive_on_link = ontology
            .rels
            .values()
            .filter(|r| matches!(r.on_link, Some(OnLink::ArchiveTarget)))
            .map(|r| stored_name(&r.id))
            .collect();
        // OWN-84 (1): the tier of the kinds the set does not govern. A kind's own key (the
        // resolver refuses it beside the `governed` mixin); the `discovered` tier is the last
        // loaded namespace's block, the order `OntologyRegistry::discovered` applies to the
        // policy, so the two knobs of one block never come from two files.
        let tiers = ontology
            .kinds
            .values()
            .filter(|k| k.governed.is_none())
            .filter_map(|k| Some((stored_name(&k.id), k.acceptance?)))
            .collect();
        let block = ontology
            .namespaces
            .iter()
            .filter_map(|ns| ontology.discovered.get(ns))
            .next_back();
        let discovered_tier = block
            .and_then(|block| block.acceptance)
            .unwrap_or(Acceptance::Agent);
        let auto_accept = block.and_then(|block| block.auto_accept_when.clone());
        Self {
            kinds,
            archive_on_link,
            governing,
            tiers,
            discovered_tier,
            auto_accept,
        }
    }

    /// The set of the shipped ontologies (`kaban-core.yaml`, `adr.yaml`), resolved once.
    ///
    /// # Errors
    /// Never for the shipped bytes (`shipped_set_matches_todays_governed_kinds` asserts it);
    /// kept as a `Result` for callers that already propagate an [`OntologyError`].
    pub fn try_shipped() -> Result<Arc<Self>, OntologyError> {
        static SHIPPED: OnceLock<Result<Arc<GovernedSet>, OntologyError>> = OnceLock::new();
        SHIPPED
            .get_or_init(|| {
                crate::shipped::resolved().map(|ontology| Arc::new(Self::from_ontology(&ontology)))
            })
            .clone()
    }

    /// [`Self::try_shipped`] for the callers that cannot propagate: the embedded files are
    /// part of the binary and resolving them is a build invariant, so a failure here is a
    /// defect that must not degrade into "nothing is governed".
    #[must_use]
    #[allow(clippy::expect_used)] // documented invariant: the embedded files resolve (tested)
    pub fn shipped() -> Arc<Self> {
        Self::try_shipped()
            .expect("the shipped ontology files resolve (kaban-core build invariant)")
    }

    /// Whether records of the kind stored under `stored_kind` carry a governance state.
    #[must_use]
    pub fn contains(&self, stored_kind: &str) -> bool {
        self.kinds.contains_key(stored_kind)
    }

    /// The mixin of `stored_kind`, when governed.
    #[must_use]
    pub fn governed(&self, stored_kind: &str) -> Option<&Governed> {
        self.kinds.get(stored_kind)
    }

    /// Who accepts a record of `stored_kind` (core-spec F4 tiers), when governed.
    #[must_use]
    pub fn acceptance(&self, stored_kind: &str) -> Option<Acceptance> {
        self.kinds.get(stored_kind).map(|g| g.acceptance)
    }

    /// The acceptance tier of any `stored_kind` (OWN-84 (1); core-spec F4 on every acceptance
    /// surface): the mixin's when governed, the kind's top-level `acceptance` when declared,
    /// else the loaded `discovered.acceptance`, else `Agent`. Read for the EN-F5a candidate and
    /// the EN-F13 `proposed` record: at `Human` an agent is refused `human_only`, never offered
    /// a token (the handshake stays the governed kinds', OWN-84 (3)).
    #[must_use]
    pub fn tier(&self, stored_kind: &str) -> Acceptance {
        self.acceptance(stored_kind)
            .or_else(|| self.tiers.get(stored_kind).copied())
            .unwrap_or(self.discovered_tier)
    }

    /// The loaded `discovered.auto_accept_when` (OWN-84 (2)): the thresholds above which
    /// placement writes no queue item (`candidates::record_discovered`) and the reconciler's
    /// `proposed` record is written `accepted` (`materialise::proposed_record`); `None` when
    /// no loaded block declares them.
    #[must_use]
    pub fn auto_accept(&self) -> Option<&PromoteWhen> {
        self.auto_accept.as_ref()
    }

    /// Whether records of `stored_kind` govern paths (`why`, the briefing).
    #[must_use]
    pub fn governs_paths(&self, stored_kind: &str) -> bool {
        self.governing.contains(stored_kind)
    }

    /// The stored names of the kinds whose records govern paths, sorted.
    pub fn governing(&self) -> impl Iterator<Item = &str> {
        self.governing.iter().map(String::as_str)
    }

    /// The stored names of every governed kind, sorted.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.kinds.keys().map(String::as_str)
    }

    /// The display labels of `stored_kind`, when governed.
    #[must_use]
    pub fn display(&self, stored_kind: &str) -> Option<&Display> {
        self.kinds.get(stored_kind).map(|g| &g.display)
    }

    /// DI1.8 (RD2-5): the ingestion reconciliation transition, the single narrow exception to
    /// core-spec F5 ("tombstones are terminal"): a node the docs-ingest pipeline owns
    /// (`owner == Some(INGEST_OWNER)`) of a non-governed kind it writes (Document, Section,
    /// Chunk) whose deterministic id re-appears in a later preparation of the same source may
    /// go `tombstoned -> active`. Source reconciliation is distinct from user forget: a human
    /// or agent tombstone on any other kind or owner stays terminal.
    ///
    /// # Errors
    /// [`InvalidTransition::Reactivate`] with the fixed refusal class for every other
    /// `(lifecycle, kind, owner)` combination.
    pub fn reconciliation_transition(
        &self,
        from: Lifecycle,
        stored_kind: &str,
        owner: Option<&str>,
    ) -> Result<(), InvalidTransition> {
        let refuse = |refusal| InvalidTransition::Reactivate {
            kind: stored_kind.to_owned(),
            refusal,
        };
        if from != Lifecycle::Tombstoned {
            return Err(refuse(ReactivateRefusal::NotTombstoned(from)));
        }
        if self.contains(stored_kind) || !INGEST_RECONCILED_KINDS.contains(&stored_kind) {
            return Err(refuse(ReactivateRefusal::KindNotReconciled));
        }
        if owner != Some(INGEST_OWNER) {
            return Err(refuse(ReactivateRefusal::NotIngestOwned));
        }
        Ok(())
    }

    /// Whether a new `rel` edge archives its target (`on_link: archive_target`).
    #[must_use]
    pub fn archives_on_link(&self, rel: &str) -> bool {
        self.archive_on_link.contains(rel)
    }

    /// Governance table (core-spec §4) for a record of `stored_kind` under this set:
    /// `proposed -> accepted`, `accepted -> superseded`, `accepted | proposed -> invalidated`.
    ///
    /// # Errors
    /// [`InvalidTransition::NotGoverned`] when the kind is not in the set,
    /// [`InvalidTransition::Governance`] for every pair outside the table.
    pub fn governance_transition(
        &self,
        stored_kind: &str,
        from: Governance,
        to: Governance,
    ) -> Result<(), InvalidTransition> {
        if !self.contains(stored_kind) {
            return Err(InvalidTransition::NotGoverned(stored_kind.to_owned()));
        }
        governance_table(from, to)
    }
}

/// Why a `Reactivate` was refused (DI1.8, RD2-5): a fixed class, never the node's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ReactivateRefusal {
    /// Only a tombstoned node can be reconciled (`purged` never reappears, `active` and
    /// `archived` are not tombstones).
    #[error("the record is {} and not tombstoned", .0.as_str())]
    NotTombstoned(Lifecycle),
    /// A governed kind, or a kind the pipeline does not write.
    #[error("the kind is not an ingest-reconciled kind")]
    KindNotReconciled,
    /// `props.owner` is not the ingest pipeline.
    #[error("the record is not owned by the ingest pipeline")]
    NotIngestOwned,
}

/// A transition the spec does not allow (surfaced as a tool-level error).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum InvalidTransition {
    /// `from -> to` is not in the lifecycle table.
    #[error("lifecycle transition {} -> {} is not allowed", from.as_str(), to.as_str())]
    Lifecycle {
        /// Current state.
        from: Lifecycle,
        /// Requested state.
        to: Lifecycle,
    },
    /// `from -> to` is not in the governance table.
    #[error("governance transition {} -> {} is not allowed", from.as_str(), to.as_str())]
    Governance {
        /// Current state.
        from: Governance,
        /// Requested state.
        to: Governance,
    },
    /// The kind has no governance state at all.
    #[error("kind '{0}' has no governance state (only Decision, Convention and Failure do)")]
    NotGoverned(String),
    /// A `Reactivate` outside the ingestion reconciliation table (DI1.8, RD2-5).
    #[error("reactivation of a {kind} refused: {refusal}")]
    Reactivate {
        /// Stored kind of the node.
        kind: String,
        /// The fixed refusal class.
        refusal: ReactivateRefusal,
    },
    /// Governance can only change on an active record.
    #[error("the record is {} and its governance can no longer change", .0.as_str())]
    NotActive(Lifecycle),
}

/// Lifecycle table (core-spec §4): `active -> archived`, `active -> tombstoned`,
/// `archived -> tombstoned`, any non-purged state `-> purged`. Tombstoned and purged are terminal
/// except for purge; a transition to the current state is not a transition.
///
/// # Errors
/// [`InvalidTransition::Lifecycle`] for every other pair.
pub fn lifecycle_transition(from: Lifecycle, to: Lifecycle) -> Result<(), InvalidTransition> {
    use Lifecycle::{Active, Archived, Purged, Tombstoned};
    match (from, to) {
        (Active, Archived | Tombstoned | Purged)
        | (Archived, Tombstoned | Purged)
        | (Tombstoned, Purged) => Ok(()),
        _ => Err(InvalidTransition::Lifecycle { from, to }),
    }
}

/// The governance table itself (core-spec §4), membership already decided.
fn governance_table(from: Governance, to: Governance) -> Result<(), InvalidTransition> {
    use Governance::{Accepted, Invalidated, Proposed, Superseded};
    match (from, to) {
        (Proposed, Accepted | Invalidated) | (Accepted, Superseded | Invalidated) => Ok(()),
        _ => Err(InvalidTransition::Governance { from, to }),
    }
}

/// [`GovernedSet::governance_transition`] under the shipped set: `proposed -> accepted`,
/// `accepted -> superseded`, `accepted | proposed -> invalidated`. The store reads the set of
/// its write context instead; this form serves the callers that have no context yet
/// (`accept`, M5 `sync.rs`).
///
/// # Errors
/// [`InvalidTransition::NotGoverned`] when `kind` carries no governance,
/// [`InvalidTransition::Governance`] for every pair outside the table.
pub fn governance_transition(
    kind: &str,
    from: Governance,
    to: Governance,
) -> Result<(), InvalidTransition> {
    GovernedSet::shipped().governance_transition(kind, from, to)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    #[test]
    fn lifecycle_table_is_exactly_the_specs() {
        use Lifecycle::{Active, Archived, Purged, Tombstoned};
        let all = [Active, Archived, Tombstoned, Purged];
        let allowed = [
            (Active, Archived),
            (Active, Tombstoned),
            (Active, Purged),
            (Archived, Tombstoned),
            (Archived, Purged),
            (Tombstoned, Purged),
        ];
        for from in all {
            for to in all {
                let ok = lifecycle_transition(from, to).is_ok();
                assert_eq!(ok, allowed.contains(&(from, to)), "{from:?} -> {to:?}");
            }
        }
    }

    /// DI1.8 (RD2-5): the single, narrow exception to "tombstones are terminal" (core F5):
    /// `Tombstoned -> Active` for an ingest-owned (`props.owner == INGEST_OWNER`) node of a
    /// non-governed kind the pipeline writes (Document, Section, Chunk). Every other
    /// `(lifecycle, kind, owner)` combination is refused with a fixed class.
    #[test]
    fn reconciliation_table_is_the_narrow_ingest_exception_only() {
        use Lifecycle::{Active, Archived, Purged, Tombstoned};
        let set = GovernedSet::shipped();
        let kinds = [
            "Document",
            "Section",
            "Chunk",
            "Decision",
            "Fact",
            "Tag",
            "Convention",
        ];
        let owners = [Some(INGEST_OWNER), Some("kaban-parse"), None];
        for from in [Active, Archived, Tombstoned, Purged] {
            for kind in kinds {
                for owner in owners {
                    let ok = set.reconciliation_transition(from, kind, owner).is_ok();
                    let expect = from == Tombstoned
                        && owner == Some(INGEST_OWNER)
                        && matches!(kind, "Document" | "Section" | "Chunk");
                    assert_eq!(ok, expect, "{from:?} {kind} {owner:?}");
                }
            }
        }
        assert_eq!(
            set.reconciliation_transition(Tombstoned, "Decision", Some(INGEST_OWNER)),
            Err(InvalidTransition::Reactivate {
                kind: "Decision".into(),
                refusal: ReactivateRefusal::KindNotReconciled,
            })
        );
        assert_eq!(
            set.reconciliation_transition(Tombstoned, "Chunk", None),
            Err(InvalidTransition::Reactivate {
                kind: "Chunk".into(),
                refusal: ReactivateRefusal::NotIngestOwned,
            })
        );
        assert_eq!(
            set.reconciliation_transition(Archived, "Chunk", Some(INGEST_OWNER)),
            Err(InvalidTransition::Reactivate {
                kind: "Chunk".into(),
                refusal: ReactivateRefusal::NotTombstoned(Archived),
            })
        );
    }

    #[test]
    fn governance_table_is_exactly_the_specs_and_only_for_governed_kinds() {
        use Governance::{Accepted, Invalidated, None, Proposed, Superseded};
        let all = [None, Proposed, Accepted, Superseded, Invalidated];
        let allowed = [
            (Proposed, Accepted),
            (Proposed, Invalidated),
            (Accepted, Superseded),
            (Accepted, Invalidated),
        ];
        for from in all {
            for to in all {
                let ok = governance_transition("Decision", from, to).is_ok();
                assert_eq!(ok, allowed.contains(&(from, to)), "{from:?} -> {to:?}");
            }
        }
        assert_eq!(
            governance_transition("Fact", Proposed, Accepted),
            Err(InvalidTransition::NotGoverned("Fact".into()))
        );
        assert!(governance_transition("Convention", Proposed, Accepted).is_ok());
        assert!(governance_transition("Failure", Accepted, Superseded).is_ok());
    }

    /// The golden: the governed truth table over the shipped kinds (and four names outside
    /// them) as observed at `0b1274f` before the set existed (`e14-slice1-golden-capture.log`).
    /// EN-F4: the set read from `adr.yaml` must reproduce it exactly.
    const GOLDEN: &[(&str, bool)] = &[
        ("Chunk", false),
        ("CodeRef", false),
        ("Convention", true),
        ("Decision", true),
        ("Document", false),
        ("Entity", false),
        ("Episode", false),
        ("Fact", false),
        ("Failure", true),
        ("Idea", false),
        ("Improvement", false),
        ("Insight", false),
        ("Section", false),
        ("Tag", false),
        ("infra:Decision", false),
        ("Vendor", false),
        ("decision", false),
        ("", false),
    ];

    #[test]
    fn shipped_set_matches_todays_governed_kinds() {
        // Arrange: the fourteen shipped kinds are all in the golden
        let ontology = crate::shipped::resolved().expect("shipped files resolve");
        let shipped_kinds: Vec<String> = ontology
            .kinds
            .values()
            .map(|k| stored_name(&k.id))
            .collect();
        for kind in &shipped_kinds {
            assert!(GOLDEN.iter().any(|(k, _)| k == kind), "golden lacks {kind}");
        }

        // Act
        let set = GovernedSet::shipped();

        // Assert: the same truth table and the same three governed names (the deprecated
        // E1.4 shim that used to agree here was deleted by E1.6)
        for (kind, governed) in GOLDEN {
            assert_eq!(set.contains(kind), *governed, "{kind}");
        }
        assert_eq!(
            set.kinds().collect::<Vec<_>>(),
            ["Convention", "Decision", "Failure"]
        );
        assert_eq!(
            set.governing().collect::<Vec<_>>(),
            ["Convention", "Decision"]
        );
        assert!(set.archives_on_link("SUPERSEDES"));
        assert!(!set.archives_on_link("INVALIDATES"));
        assert_eq!(set.acceptance("Decision"), Some(Acceptance::Human));
        assert_eq!(set.acceptance("Failure"), Some(Acceptance::Agent));
        assert_eq!(
            set.display("Decision").map(|d| d.plural.as_str()),
            Some("Decisions")
        );
    }

    #[test]
    fn a_kind_without_the_mixin_is_not_governed_even_under_a_governed_name() {
        // Arrange: `infra:Decision` has no `governed` block; `infra:Incident` has one
        let file = crate::OntologyFile::from_yaml(
            "ontology: infra\nversion: 1\nkinds:\n  Decision:\n    description: d\n  Incident:\n    description: i\n    governed: {acceptance: human, serialize: none, export: {digest: false, marker: false}, display: {label: Incident, plural: Incidents}, governs_paths: true, dedup: none}\nrels:\n  REPLACES: {from: [], to: [], directed: true, on_link: archive_target}\n  MENTIONED_IN: {from: [], to: [], directed: true}\n",
        )
        .expect("fixture parses");
        let ontology = crate::resolve(&[file], &crate::NamespaceSet::new()).expect("resolves");

        // Act
        let set = GovernedSet::from_ontology(&ontology);

        // Assert
        assert!(!set.contains("infra:Decision"));
        assert!(
            !set.contains("Decision"),
            "the shipped names are not in a user set"
        );
        assert!(set.contains("infra:Incident"));
        assert!(set.governs_paths("infra:Incident"));
        assert!(set.archives_on_link("infra:REPLACES"));
        assert!(!set.archives_on_link("infra:MENTIONED_IN"));
        assert_eq!(
            set.governance_transition("infra:Decision", Governance::Proposed, Governance::Accepted),
            Err(InvalidTransition::NotGoverned("infra:Decision".into()))
        );
        assert!(
            set.governance_transition("infra:Incident", Governance::Proposed, Governance::Accepted)
                .is_ok()
        );
    }

    /// OWN-84 (1): `tier` answers every kind: the mixin's for a governed kind, a declared
    /// kind's own `acceptance`, else the last loaded `discovered` block's, else `agent`; the
    /// governed-only readers do not widen; a kind declaring both keys is a resolver conflict.
    #[test]
    fn tier_is_the_mixin_the_kinds_own_key_the_discovered_block_or_agent() {
        // Arrange: `Rack` declares `human`, `Vendor` nothing, the block `human`
        let file = crate::OntologyFile::from_yaml(
            "ontology: infra\nversion: 1\nkinds:\n  Rack:\n    description: r\n    acceptance: human\n  Vendor:\n    description: v\n  Incident:\n    description: i\n    governed: {acceptance: agent, serialize: none, export: {digest: false, marker: false}, display: {label: Incident, plural: Incidents}, governs_paths: false, dedup: none}\nrels: {}\ndiscovered: {policy: review, cap: 10, prune_after_days: 1, promote_when: {min_count: 1, min_sources: 1, min_mean_score: 0.5}, acceptance: human}\n",
        )
        .expect("fixture parses");
        let ontology = crate::resolve(&[file], &crate::NamespaceSet::new()).expect("resolves");

        // Act
        let set = GovernedSet::from_ontology(&ontology);

        // Assert
        assert_eq!(set.tier("infra:Incident"), Acceptance::Agent, "the mixin's");
        assert_eq!(
            set.tier("infra:Rack"),
            Acceptance::Human,
            "the kind's own key"
        );
        assert_eq!(set.tier("infra:Vendor"), Acceptance::Human, "the block's");
        assert_eq!(set.tier("x:Widget"), Acceptance::Human, "the block's");
        assert_eq!(set.acceptance("infra:Rack"), None, "governed only");
        assert!(!set.contains("infra:Rack"));
        assert_eq!(set.kinds().collect::<Vec<_>>(), ["infra:Incident"]);
        // the shipped set: no block declares a tier, every ungoverned kind is `agent`
        let shipped = GovernedSet::shipped();
        assert_eq!(shipped.tier("Decision"), Acceptance::Human);
        assert_eq!(shipped.tier("Fact"), Acceptance::Agent);
        assert_eq!(shipped.tier("x:Widget"), Acceptance::Agent);
        // both keys on one kind: refused by the resolver
        let both = crate::OntologyFile::from_yaml(
            "ontology: infra\nversion: 1\nkinds:\n  Incident:\n    description: i\n    acceptance: human\n    governed: {acceptance: agent, serialize: none, export: {digest: false, marker: false}, display: {label: Incident, plural: Incidents}, governs_paths: false, dedup: none}\nrels: {}\n",
        )
        .expect("fixture parses");
        assert!(matches!(
            crate::resolve(&[both], &crate::NamespaceSet::new()),
            Err(OntologyError::Conflict { detail, .. }) if detail == "acceptance_governed"
        ));
    }
}
