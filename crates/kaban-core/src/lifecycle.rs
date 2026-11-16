//! The state of a node (core-spec §4, SR-4): `lifecycle` is storage visibility for every kind
//! and the only state a node row carries (the v14 format has no governance column).
//! [`RuleSet`] holds the live ontology rules the store and the service
//! decide writes by, read from a resolved ontology by stored kind name: each kind's `rules:`
//! block (display labels, the near-duplicate key; engine-spec EN-F4) and the rels whose
//! `on_link` archives the target. The transition tables are the spec's, exactly; anything
//! else is an [`InvalidTransition`] that adapters surface as a tool-level error.

use crate::ontology::meta::{Dedup, Display, OnLink, Rules};
use crate::ontology::{Ontology, OntologyError, stored_name};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use thiserror::Error;

/// `props.owner` of every node and edge the docs-ingest pipeline writes (docs-ingest-spec D5
/// [A-3], engine-spec EN-E4): the owner string `EdgeDiff::Owned` compares against and the only
/// owner [`reconciliation_transition`] reactivates for.
pub const INGEST_OWNER: &str = "kaban-ingest";

/// The kinds the docs-ingest pipeline owns, the only kinds the reconciliation
/// transition may bring back from `tombstoned` (docs-ingest-spec D5, DI1.8).
const INGEST_RECONCILED_KINDS: [&str; 3] = ["Document", "Section", "Chunk"];

/// The rels whose edges change another record's lifecycle or standing: `SUPERSEDES` archives
/// its target, `INVALIDATES` tombstones it, `CONTRADICTS` disputes it. The extractor trust rule
/// (PC-20): extractor output never carries one; an authored `link` may write them.
pub const LIFECYCLE_RELS: [&str; 3] = ["SUPERSEDES", "INVALIDATES", "CONTRADICTS"];

/// Whether `rel` (a stored or local name) is one of the [`LIFECYCLE_RELS`].
#[must_use]
pub fn is_lifecycle_rel(rel: &str) -> bool {
    LIFECYCLE_RELS.contains(&rel)
}

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

/// The live ontology rules of a resolved ontology, by stored name (engine-spec EN-F4): the
/// `rules:` block of every kind that declares one and the rels whose `on_link: archive_target`
/// archives the target of a new edge. Built once per loaded ontology (the registry) and
/// carried into every write; [`RuleSet::shipped`] is the embedded files' set. There is no
/// empty default on purpose: a write without a set would skip `SUPERSEDES` archiving.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleSet {
    kinds: BTreeMap<String, Rules>,
    archive_on_link: BTreeSet<String>,
}

impl RuleSet {
    /// The set of `ontology`: kinds with a `rules` block and rels with an `on_link`, under
    /// their stored names.
    #[must_use]
    pub fn from_ontology(ontology: &Ontology) -> Self {
        let kinds = ontology
            .kinds
            .values()
            .filter_map(|k| Some((stored_name(&k.id), k.rules.clone()?)))
            .collect();
        let archive_on_link = ontology
            .rels
            .values()
            .filter(|r| matches!(r.on_link, Some(OnLink::ArchiveTarget)))
            .map(|r| stored_name(&r.id))
            .collect();
        Self {
            kinds,
            archive_on_link,
        }
    }

    /// The set of the shipped ontologies (`kaban-core.yaml`, `adr.yaml`), resolved once.
    ///
    /// # Errors
    /// Never for the shipped bytes (`shipped_set_matches_the_shipped_rules` asserts it);
    /// kept as a `Result` for callers that already propagate an [`OntologyError`].
    pub fn try_shipped() -> Result<Arc<Self>, OntologyError> {
        static SHIPPED: OnceLock<Result<Arc<RuleSet>, OntologyError>> = OnceLock::new();
        SHIPPED
            .get_or_init(|| {
                crate::shipped::resolved().map(|ontology| Arc::new(Self::from_ontology(&ontology)))
            })
            .clone()
    }

    /// [`Self::try_shipped`] for the callers that cannot propagate: the embedded files are
    /// part of the binary and resolving them is a build invariant, so a failure here is a
    /// defect that must not degrade into "no rule applies".
    #[must_use]
    #[allow(clippy::expect_used)] // documented invariant: the embedded files resolve (tested)
    pub fn shipped() -> Arc<Self> {
        Self::try_shipped()
            .expect("the shipped ontology files resolve (kaban-core build invariant)")
    }

    /// The `rules` block of `stored_kind`, when it declares one.
    #[must_use]
    pub fn rules(&self, stored_kind: &str) -> Option<&Rules> {
        self.kinds.get(stored_kind)
    }

    /// The stored names of every kind with a `rules` block, sorted.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.kinds.keys().map(String::as_str)
    }

    /// The display labels of `stored_kind`, when its rules give them.
    #[must_use]
    pub fn display(&self, stored_kind: &str) -> Option<&Display> {
        self.kinds.get(stored_kind).and_then(|r| r.display.as_ref())
    }

    /// Whether records of `stored_kind` carry a near-duplicate key (`dedup: title+scope`).
    #[must_use]
    pub fn dedups(&self, stored_kind: &str) -> bool {
        self.kinds
            .get(stored_kind)
            .is_some_and(|r| r.dedup == Dedup::TitleScope)
    }

    /// Whether a new `rel` edge archives its target (`on_link: archive_target`).
    #[must_use]
    pub fn archives_on_link(&self, rel: &str) -> bool {
        self.archive_on_link.contains(rel)
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
    /// A kind the pipeline does not write.
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
    /// A `Reactivate` outside the ingestion reconciliation table (DI1.8, RD2-5).
    #[error("reactivation of a {kind} refused: {refusal}")]
    Reactivate {
        /// Stored kind of the node.
        kind: String,
        /// The fixed refusal class.
        refusal: ReactivateRefusal,
    },
}

/// DI1.8 (RD2-5): the ingestion reconciliation transition, the single narrow exception to
/// core-spec F5 ("tombstones are terminal"): a node the docs-ingest pipeline owns
/// (`owner == Some(INGEST_OWNER)`) of a kind it writes (Document, Section,
/// Chunk) whose deterministic id re-appears in a later preparation of the same source may
/// go `tombstoned -> active`. Source reconciliation is distinct from user forget: a human
/// or agent tombstone on any other kind or owner stays terminal.
///
/// # Errors
/// [`InvalidTransition::Reactivate`] with the fixed refusal class for every other
/// `(lifecycle, kind, owner)` combination.
pub fn reconciliation_transition(
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
    if !INGEST_RECONCILED_KINDS.contains(&stored_kind) {
        return Err(refuse(ReactivateRefusal::KindNotReconciled));
    }
    if owner != Some(INGEST_OWNER) {
        return Err(refuse(ReactivateRefusal::NotIngestOwned));
    }
    Ok(())
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
    /// kind the pipeline writes (Document, Section, Chunk). Every other
    /// `(lifecycle, kind, owner)` combination is refused with a fixed class.
    #[test]
    fn reconciliation_table_is_the_narrow_ingest_exception_only() {
        use Lifecycle::{Active, Archived, Purged, Tombstoned};
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
                    let ok = reconciliation_transition(from, kind, owner).is_ok();
                    let expect = from == Tombstoned
                        && owner == Some(INGEST_OWNER)
                        && matches!(kind, "Document" | "Section" | "Chunk");
                    assert_eq!(ok, expect, "{from:?} {kind} {owner:?}");
                }
            }
        }
        assert_eq!(
            reconciliation_transition(Tombstoned, "Decision", Some(INGEST_OWNER)),
            Err(InvalidTransition::Reactivate {
                kind: "Decision".into(),
                refusal: ReactivateRefusal::KindNotReconciled,
            })
        );
        assert_eq!(
            reconciliation_transition(Tombstoned, "Chunk", None),
            Err(InvalidTransition::Reactivate {
                kind: "Chunk".into(),
                refusal: ReactivateRefusal::NotIngestOwned,
            })
        );
        assert_eq!(
            reconciliation_transition(Archived, "Chunk", Some(INGEST_OWNER)),
            Err(InvalidTransition::Reactivate {
                kind: "Chunk".into(),
                refusal: ReactivateRefusal::NotTombstoned(Archived),
            })
        );
    }

    #[test]
    fn shipped_set_matches_the_shipped_rules() {
        // Act
        let set = RuleSet::shipped();

        // Assert: the three `adr` kinds with a rules block, the one near-duplicate kind and
        // the one archiving rel
        assert_eq!(
            set.kinds().collect::<Vec<_>>(),
            ["Convention", "Decision", "Failure"]
        );
        assert!(set.dedups("Decision"));
        assert!(!set.dedups("Convention"));
        assert!(!set.dedups("Fact"));
        assert!(set.archives_on_link("SUPERSEDES"));
        assert!(!set.archives_on_link("INVALIDATES"));
        assert_eq!(
            set.display("Decision").map(|d| d.plural.as_str()),
            Some("Decisions")
        );
        assert!(set.display("Fact").is_none());
    }

    #[test]
    fn a_kind_without_a_rules_block_has_no_rules_even_under_a_shipped_name() {
        // Arrange: `infra:Decision` has no `rules` block; `infra:Incident` has one
        let file = crate::OntologyFile::from_yaml(
            "ontology: infra\nversion: 1\nkinds:\n  Decision:\n    description: d\n  Incident:\n    description: i\n    rules: {display: {label: Incident, plural: Incidents}, dedup: title+scope}\nrels:\n  REPLACES: {from: [], to: [], directed: true, on_link: archive_target}\n  MENTIONED_IN: {from: [], to: [], directed: true}\n",
        )
        .expect("fixture parses");
        let ontology = crate::resolve(&[file], &crate::NamespaceSet::new()).expect("resolves");

        // Act
        let set = RuleSet::from_ontology(&ontology);

        // Assert
        assert!(set.rules("infra:Decision").is_none());
        assert!(
            set.rules("Decision").is_none(),
            "the shipped names are not in a user set"
        );
        assert!(set.dedups("infra:Incident"));
        assert!(set.archives_on_link("infra:REPLACES"));
        assert!(!set.archives_on_link("infra:MENTIONED_IN"));
    }
}
