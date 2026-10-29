//! kaban memory core: the labelled-property-graph model, the [`GraphStore`] portability seam
//! (an umbrella over the role traits [`MemoryStore`], [`SchemaStore`], [`GovernanceStore`],
//! [`AuditStore`], [`ChangeLogStore`], [`LeaseStore`], [`IngestStore`] and [`SyncStore`]), the
//! [`Sink`] seam over the `ChangeLog`, id/time helpers and the store error type.
//!
//! Everything above this crate (policy, service layer, adapters) speaks in nodes, typed edges,
//! labels and traversals, never SQL. `kaban-store-sqlite` is the first implementation; a
//! Cypher-capable engine would implement the same trait.

// Modules are private; the supported surface is the flat re-export list below
// (`proj-pub-use-reexport`, review R0-7).
mod artifact;
mod audit;
mod canon;
mod changelog;
pub mod config;
mod error;
mod events;
pub mod ident;
mod ids;
mod lease;
mod lifecycle;
mod model;
pub mod ontology;
/// The one-shot process runner (stage1-plan CG1.1): public as a module so the spawn lint can
/// name `kaban_core::process` and so `Exec`, `EnvAllowlist`, `Monitor` keep their own namespace.
pub mod process;
mod sink;
mod store;
pub mod text_index;
mod validator;

mod reconcile;

pub use artifact::{
    ArtifactDiagnostics, ArtifactKey, ArtifactSource, BuiltinProducer, CurrentProducerAuthority,
    DerivedArtifact, GazetteerClaimSet, LayerMaintenance, MaintenanceRefusal, RetireReport,
    SourceRev, artifact_key,
};
pub use audit::{
    Actor, ActorId, AuditEntry, AuditOp, AuditVerification, InvalidActorId, RemovalIntent,
    WorkspaceId, WriteCtx,
};
// canonical JSON, `props_schema` validator subset (engine-plan E1.1)
pub use canon::canonical_json;
pub use error::{BackendSource, Result, StoreError, SupersedesClass};
pub use events::{Confirmation, Event, ForgetOutcome, NewConfirmation, NewEvent, SignerKind};
pub use ids::{InvalidNodeId, NodeId, new_id, now_iso};
pub use lease::Lease;
// `GovernedSet` (engine-plan E1.4): the EN-F4 governance membership read from the ontology
// (the deprecated E1.4 shims over the shipped set were deleted by E1.6)
pub use lifecycle::{
    Governance, GovernedSet, INGEST_OWNER, InvalidTransition, Lifecycle, ReactivateRefusal,
    StoreScope, governance_transition, lifecycle_transition,
};
pub use model::{
    AnchoredKind, AnchoredRow, BatchReport, BodyLocation, CodeRef, CodeRefFilter, ConventionProps,
    DecisionProps, Edge, EdgeDiff, EdgeFilter, Evidence, FailureProps, ForgetMode, GoverningQuery,
    Hit, IndexMeta, LinkReq, Location, NewNode, Node, Op, OpOutcome, OutcomeKind, RecallLayer,
    RecallQuery, RedactedStrings, RememberOutcome, RescanAction, RescanReport, TraverseDirection,
    TraverseQuery, TraverseResult, TypeDeclaration, TypeDef, WorkspaceFilter, WriteBatch,
};
// discovered labels, candidates and `kind_stats` (engine-plan E1.8)
pub use model::{DiscoveredRules, KindStat, PromoteReport, PruneReport, ReadFilters};
pub use validator::{
    Error as PropsError, Keyword, Schema, SchemaError, ValidatorLimits, first_error_text, validate,
};
// hook session scratch, briefing and failure pairing (core-plan C4.3/C4.4; M4-12 to M4-17)
pub use model::{
    EvidenceRun, FailurePatch, PendingFailuresQuery, PendingScope, SessionEdit, SessionEditKind,
};
// `.kaban/*.md` serialization (core-plan C5.1; M5-18)
pub use model::{ConflictRow, ImportedRecord};
// batch assertions and typed edge owners (engine-plan E2.2; EN-E4, EN-F8)
pub use model::{BatchAssertion, BatchSubject, Owner};
// ontologies as data (engine-plan E1.2)
pub use ontology::{
    KindName, NamespaceSet, Ns, ONTOLOGY_ERROR_TEXTS, Ontology, OntologyError, OntologyFile,
    Qualified, RelName, resolve, split_stored, stored_name,
};
// the one normalisation contract before registry lookups (E1.9a RE3-2)
pub use ontology::resolve::normalise_stored;
// the `ChangeLog` and its sinks (engine-plan E2.0; EN-E10b)
pub use changelog::{Change, ChangeEntity, ChangeOp};
pub use sink::{Ack, Sink, SinkId, SinkReport, run_sink};

/// The shipped ontologies (engine-spec §3.1, EN-F2, EN-N1; engine-plan E1.3), embedded in the
/// binary as strings and loaded first by every store and registry: `kaban-core.yaml`
/// (namespace `core`) and `adr.yaml` (namespace `adr`). The files are the source of truth for
/// the builtin `schema_type` seed rows (OWN-38); the store derives them from [`shipped::resolved`].
pub mod shipped {
    use crate::ontology::{NamespaceSet, Ontology, OntologyError, OntologyFile, resolve};

    /// `kaban-core.yaml`: the `core` namespace.
    pub const KABAN_CORE_YAML: &str = include_str!("../ontology/kaban-core.yaml");
    /// `adr.yaml`: the `adr` namespace (extends `core`).
    pub const ADR_YAML: &str = include_str!("../ontology/adr.yaml");
    /// The meta-schema every ontology file is validated against (EN-F1).
    pub const META_SCHEMA_YAML: &str = crate::ontology::meta::META_SCHEMA_YAML;
    /// The shipped files in load order, with the name each is known by.
    pub const FILES: [(&str, &str); 2] =
        [("kaban-core.yaml", KABAN_CORE_YAML), ("adr.yaml", ADR_YAML)];

    /// The shipped files parsed, in load order.
    ///
    /// # Errors
    /// Never for the shipped bytes (a test asserts it); the type is kept so a caller does not
    /// unwrap.
    pub fn files() -> Result<Vec<OntologyFile>, OntologyError> {
        FILES
            .iter()
            .map(|(_, text)| OntologyFile::from_yaml(text))
            .collect()
    }

    /// The shipped files resolved into one ontology (no other namespace loaded).
    ///
    /// # Errors
    /// As [`files`]; never for the shipped bytes.
    pub fn resolved() -> Result<Ontology, OntologyError> {
        resolve(&files()?, &NamespaceSet::new())
    }
}
#[allow(deprecated)] // the EN-E10b alias, re-exported for one minor version (removed in 3.2)
pub use store::OutboxStore;
pub use store::{
    AuditStore, ChangeLogStore, GovernanceStore, GraphStore, IngestStore, LeaseStore, MemoryStore,
    SchemaStore, SyncStore,
};
// the `supersession_chain` types (v4 stage1-plan CG5.3; cg-join-v1 §7)
pub use store::{Chain, ChainDirection, PruningProtection, PruningTarget};
pub use store::{
    DatabaseFileModes, DocumentIndexSummary, FailedDocumentRow, ImportedPurge, MaintenanceOutcome,
    ReviewRow, StoreCapabilities, UnstampedCounts, UnstampedReason,
};
pub use store::{
    IngestJob, InvalidJobId, JobClaimRequest, JobFinishOutcome, JobFinishRequest,
    JobFinishStateUpdate, JobId, JobLease, JobOpenRequest, JobStatus, JobSubmitContract,
    JobSubmitReceipt, JobSubmitRequest, JobSubmitStateUpdate, PendingJob,
};
// the CG-JOIN v1 attach and anchor types (engine-plan E2.6; cg-join-v1 §2, §7)
pub use store::{
    AnchorPredicates, AttachHandle, SNAPSHOT_SYMBOL_PAGE, SnapshotAttach, SnapshotSymbol,
    SymbolPage,
};

pub use reconcile::{
    Assertion, AssertionKey, AssertionStatus, CandidateReason, CanonicalJson, ClaimClass, Fixed,
    FixedError, Layer, LineageParts, LogWeight, Method, Priors, PriorsError, ProtectionSet, Rank,
    ReconcileCtx, Reconciler, Subject, UnresolvedReason, Verdict, group_key, lineage_id,
    same_group, votes,
};
// entity resolution (engine-plan E2.10; EN-F51): the pure scorer and its vocabulary
pub use reconcile::er;
pub use reconcile::er::{
    Agreement, Block, BlockKey, ErConfigError, ErContribution, ErDecision, ErPriors, ErReport,
    ErScore, Field, FuzzyIndex, JudgeFilter, NodeView, PairJudgement, PairRef, UnmergeOutcome,
};
// the `explain` document and the evaluation counters (engine-plan E2.4; EN-F14, EN-F16)
pub use reconcile::{
    CandidateView, Contribution, Explanation, LineageView, PriorsObs, PriorsOutcome, SupporterView,
    WEIGHTED_VOTE_V1,
};
