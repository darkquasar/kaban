//! Store error type shared by every [`crate::GraphStore`] implementation.

use thiserror::Error;

/// The driver error behind [`StoreError::Backend`], held so that no generic renderer reaches
/// its text (review RM-1; core-spec S1): a driver message may quote SQL text or bound values,
/// so `Display`, `Debug` and the `source()` chain all stop here with fixed text. The driver
/// error itself stays intact and downcastable through [`BackendSource::driver`] for an operator
/// or a test that asks for it explicitly.
///
/// `source()` is deliberately `None`: an error chain walker (`anyhow`'s alternate `{:#}`,
/// `Debug` of an enclosing error) would otherwise render the driver's `Display`.
pub struct BackendSource(Box<dyn std::error::Error + Send + Sync + 'static>);

impl BackendSource {
    /// Hold `driver` behind the redacted renderers.
    pub fn new<E>(driver: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self(Box::new(driver))
    }

    /// The driver's own error object (downcast with `Error::downcast_ref`). Its text is the
    /// caller's to keep off any terminal or log (core-spec S1).
    #[must_use]
    pub fn driver(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self.0.as_ref()
    }
}

impl std::fmt::Debug for BackendSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackendSource(<driver error withheld>)")
    }
}

impl std::fmt::Display for BackendSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("driver error details withheld")
    }
}

impl std::error::Error for BackendSource {}

/// Why a store operation failed. Messages are safe to echo to the caller: they never carry
/// note text, only kind/relationship/id names, and the two schema-lookup refusals carry only
/// their class (RW3-6: the name looked up is caller data; `Debug` is written by hand below so
/// that it withholds those two payloads and renders every other variant as the derive would).
#[derive(Error)]
#[non_exhaustive] // later milestones add variants (migrations, leases, governance); match with a fallback arm
pub enum StoreError {
    /// The node kind (label) is not registered in the schema. The text is the fixed class
    /// (RW3-6); the payload is the name looked up, kept for the callers that match on it and
    /// rendered by neither `Display` nor `Debug`.
    #[error("unknown_kind")]
    UnknownKind(String),
    /// The relationship type is not registered in the schema. The text is the fixed class
    /// (RW3-6); the payload is rendered by neither `Display` nor `Debug`.
    #[error("unknown_rel")]
    UnknownRel(String),
    /// A label/relationship name (or kind selector) violates the naming rules.
    #[error("invalid name '{0}': {1}")]
    InvalidName(String, &'static str),
    /// No node with that id.
    #[error("node not found: {0}")]
    NotFound(String),
    /// The relationship's endpoint constraints reject this (from, to) kind pair.
    #[error("relationship {rel} not allowed from {from} to {to}")]
    EndpointNotAllowed {
        /// Relationship type.
        rel: String,
        /// Kind of the source node.
        from: String,
        /// Kind of the destination node.
        to: String,
    },
    /// An inline link spec did not name exactly one of `from` / `to`.
    #[error("link spec must have exactly one of `from` or `to`")]
    BadLink,
    /// A relationship's two endpoints are the same node (m2 N11).
    #[error("a node cannot link to itself: {0}")]
    SelfLink(String),
    /// An ingestion upsert named a node whose stored kind differs: kind is immutable per id
    /// (WD-2), so the whole batch fails.
    #[error("node {id} is stored as kind '{stored}', not '{requested}'; kind is immutable per id")]
    KindMismatch {
        /// The node id.
        id: String,
        /// Kind on the stored row.
        stored: String,
        /// Kind the batch asked for.
        requested: String,
    },
    /// An ingestion batch names a workspace other than its write context's: the audit rows
    /// would be attributed to a tenant other than the mutated rows (R2-1), so the batch is
    /// refused before any write.
    #[error("batch workspace '{batch}' differs from the write context's workspace '{ctx}'")]
    WorkspaceMismatch {
        /// Workspace of the write context.
        ctx: String,
        /// Workspace the batch asked for.
        batch: String,
    },
    /// Builtin schema entries cannot be changed.
    #[error("type '{0}' is builtin and cannot be changed")]
    Builtin(String),
    /// The storage backend (the SQLite driver today) failed. The displayed message is
    /// deliberately content-free (core-spec S1): a driver message may quote SQL text or bound
    /// values, so it stays behind the [`BackendSource`] holder for operators (RM-1) and never
    /// in the text echoed to a caller, by `Display`, `Debug` or a chain walker. The holder
    /// boxes the driver error so this crate depends on no driver (MT-8); the backend crate
    /// owns the conversion.
    #[error("storage backend error")]
    Backend {
        /// The driver's error, behind redacted renderers.
        #[source]
        source: BackendSource,
    },
    /// JSON (de)serialization of props/tags failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The audit chain could not be initialised (key file missing, unreadable or malformed).
    /// Writes must not proceed without it (core-spec S3).
    #[error("audit chain unavailable")]
    Audit(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The database file (or a `-wal`/`-shm` sidecar) could not be created or restricted to
    /// owner-only permissions (core-spec S5). The store refuses to open rather than leave a
    /// world-readable database behind.
    #[error("database file could not be created or made owner-only (0600)")]
    FileMode(#[source] std::io::Error),
    /// The database is at another schema version than the one this kaban serves (spec 05
    /// OWN-P: one schema, no migrations; an older and a newer file alike). It is left
    /// untouched.
    #[error(
        "database schema version {found} is not the version {supported} this kaban serves: the database was created by another kaban version; export it with that version and re-ingest here, or start a new database"
    )]
    SchemaUnsupported {
        /// Version recorded in the database (1 for an unversioned spike file).
        found: u32,
        /// The one version this binary serves.
        supported: u32,
    },
    /// Another process holds the database open and this store cannot share one between
    /// processes (spec 05 SS-23: the shared open, mode B, is refused). Nothing was opened.
    #[error(
        "the database is open in another process and this store cannot share it; close the other kaban process and retry"
    )]
    SharedOpenUnsupported,
    /// Another process holds the database under its exclusive maintenance condition (spec 05
    /// SS-12: an erasure that must know no other process maps the index) and the opener could
    /// not mark its presence before giving up. Nothing was opened: an opener is never admitted
    /// unmarked, since the condition's claim rests on every opener being visible.
    #[error(
        "the database is held for exclusive maintenance by another kaban process; retry once it is released"
    )]
    ExclusiveMaintenance,
    /// A snapshot read named a commit other than the attached snapshot's, or nothing is
    /// attached (CG-JOIN v1 §7 `snapshot_symbols`; engine E2.6); or an admitted handle was
    /// presented for a file that no longer has the generation it was admitted at, so nothing
    /// was attached (spec 05 SS-25). The text is the fixed class: no commit, no path.
    #[error("snapshot_mismatch")]
    SnapshotMismatch,
    /// A lifecycle or governance transition the spec does not allow (core-spec §4).
    #[error(transparent)]
    InvalidTransition(#[from] crate::lifecycle::InvalidTransition),
    /// A string that is not a node id.
    #[error("invalid node id: {0}")]
    InvalidId(#[from] crate::ids::InvalidNodeId),
    /// A consumer's fence is stale: a successor acquired its lease (core-spec F15, SR-2), or
    /// it never held one. The cursor advance or consumer write was rejected.
    #[error("lease '{name}' lost: fence {fence} is not the current holder's; stop consuming")]
    LeaseLost {
        /// Lease (consumer) name.
        name: String,
        /// Fence the caller presented.
        fence: u64,
    },
    /// A schema migration failed; the database stays at version `from` (one transaction per
    /// migration). The source is boxed so this crate does not grow a second SQLite coupling
    /// (MD-27).
    #[error("schema migration from version {from} to {to} failed")]
    Migration {
        /// Version the database was at.
        from: u32,
        /// Version the failed migration targeted.
        to: u32,
        /// Underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// An undeclared label the discovered-labels policy refuses (engine-spec EN-F5: policy
    /// `deny`, or a label that would shadow a governed kind under any policy). The record is
    /// skipped; the text is the fixed class and nothing else: the label is caller data and
    /// reaches neither `Display` nor `Debug` (core S3, E1.9a RE3-8).
    #[error("kind_not_allowed")]
    KindNotAllowed,
    /// An operation reserved to a human actor (EN-F5 `promote`), called by an agent.
    #[error("human_only:{0}")]
    HumanOnly(&'static str),
    /// The database retired its in-database FTS5 index (spec 05 SS-16) and this store has no
    /// text index attached (a build without the Tantivy adapter, or a library open): recall,
    /// the text index rebuild and its diagnostics have nothing to work on, so they are refused
    /// rather than answered empty or as a no-op.
    #[error(
        "no text index is attached to a database that retired FTS5 (spec 05 SS-16); open it with a kaban built with the text-index-tantivy feature"
    )]
    TextIndexUnavailable,
    /// The text index beside the database does not admit and the rebuild an open owes it (spec
    /// 05 OWN-M: a store without a serving index does not exist) could not run, for the fixed
    /// reason given (another session holds the database). Nothing was opened.
    #[error("the text index must be rebuilt and cannot be at this open: {0}")]
    TextIndexRebuildRefused(&'static str),
    /// `promote_kind` would give a renamed record the dedup identity (workspace, kind, text)
    /// of an existing active record of the target kind (E1.9a RE3-3). The whole promotion is
    /// refused; nothing is renamed.
    #[error("promote_conflict")]
    PromoteConflict,
    /// The promotion target cannot take the records as they are (E1.9a RE3-4): a governed
    /// kind (its lifecycle starts `proposed`, its `dedup` key is derived at write time), or a
    /// kind whose `props_schema` some record of the group does not satisfy. Decided by the
    /// service from the loaded ontology before any mutation.
    #[error("promote_target_incompatible")]
    PromoteTargetIncompatible,
    /// An ontology error raised at the record seam (EN-F7 texts: `discovered_cap_reached`,
    /// `invalid_identifier` for a discovered label, `unknown_kind` for a promotion target).
    #[error(transparent)]
    Ontology(#[from] crate::ontology::OntologyError),
    /// A producer `UpsertEdge` carrying a `layer` names no matching `edge` assertion in its
    /// batch (engine-spec EN-F9; engine-plan E2.2): no producer writes an edge directly, the
    /// projection of a verdict does, so the batch is refused before any write. The text is
    /// the fixed class; the triple is caller data and is not echoed.
    #[error("edge_requires_assertion")]
    EdgeRequiresAssertion,
    /// A batch assertion names a subject, span or object the batch's workspace does not hold (EN-S3, F17): unknown, purged or another tenant's; the whole
    /// batch fails before any claim row is written. The id is echoed as the other
    /// `NotFound` paths do (an id is control, not content).
    #[error("assertion subject not found in the batch's workspace: {0}")]
    AssertionSubjectNotFound(String),
    /// A batch assertion's fixed-point input is out of range (EN-F12a: a `raw_score` above
    /// 10000); nothing is written.
    #[error(transparent)]
    Fixed(#[from] crate::FixedError),
    /// A human edge claim (`link`, an inline link of `remember`) was recorded but its group
    /// has no winner to materialise (a zero-confidence claim is `no_valid_support`), so no
    /// edge exists; the rel is control, not content.
    #[error("claim_rejected: no edge materialised for '{0}'")]
    ClaimRejected(String),
    /// A claim's `supersedes` is refused (OWN-86 (3); EN-F8): it names no claim of the
    /// batch's workspace, a claim of another conflict group, or closes a cycle inside the
    /// batch. The whole batch fails before any claim row is written; the text is the fixed
    /// class, never the key.
    #[error("{0}")]
    SupersedesInvalid(SupersedesClass),
}

/// Why a claim's `supersedes` was refused ([`StoreError::SupersedesInvalid`]): a fixed class,
/// so the refusal carries no key and no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive] // a later rule adds a class; match with a fallback arm
pub enum SupersedesClass {
    /// The key names no claim the batch's workspace holds: unknown, purged, or another
    /// tenant's (one class for all three, so the refusal is no existence oracle across tenants).
    NotFound,
    /// The named claim is of another conflict group (workspace, claim class, subject, detail).
    CrossGroup,
    /// The batch's `supersedes` references form a cycle (A supersedes B supersedes A, or longer).
    Cycle,
}

impl SupersedesClass {
    /// The class as the refusal renders it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "supersedes_not_found",
            Self::CrossGroup => "supersedes_cross_group",
            Self::Cycle => "supersedes_cycle",
        }
    }
}

impl std::fmt::Display for SupersedesClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for StoreError {
    /// The derive's rendering for every variant but the two whose payload is the name a caller
    /// looked up (RW3-6: `UnknownKind`, `UnknownRel` render without it).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKind(_) => f.write_str("UnknownKind(<name withheld>)"),
            Self::UnknownRel(_) => f.write_str("UnknownRel(<name withheld>)"),
            Self::InvalidName(name, why) => {
                f.debug_tuple("InvalidName").field(name).field(why).finish()
            }
            Self::NotFound(id) => f.debug_tuple("NotFound").field(id).finish(),
            Self::EndpointNotAllowed { rel, from, to } => f
                .debug_struct("EndpointNotAllowed")
                .field("rel", rel)
                .field("from", from)
                .field("to", to)
                .finish(),
            Self::BadLink => f.write_str("BadLink"),
            Self::SelfLink(id) => f.debug_tuple("SelfLink").field(id).finish(),
            Self::KindMismatch {
                id,
                stored,
                requested,
            } => f
                .debug_struct("KindMismatch")
                .field("id", id)
                .field("stored", stored)
                .field("requested", requested)
                .finish(),
            Self::WorkspaceMismatch { ctx, batch } => f
                .debug_struct("WorkspaceMismatch")
                .field("ctx", ctx)
                .field("batch", batch)
                .finish(),
            Self::Builtin(name) => f.debug_tuple("Builtin").field(name).finish(),
            Self::Backend { source } => f.debug_struct("Backend").field("source", source).finish(),
            Self::Json(e) => f.debug_tuple("Json").field(e).finish(),
            Self::Audit(e) => f.debug_tuple("Audit").field(e).finish(),
            Self::FileMode(e) => f.debug_tuple("FileMode").field(e).finish(),
            Self::SchemaUnsupported { found, supported } => f
                .debug_struct("SchemaUnsupported")
                .field("found", found)
                .field("supported", supported)
                .finish(),
            Self::SharedOpenUnsupported => f.write_str("SharedOpenUnsupported"),
            Self::ExclusiveMaintenance => f.write_str("ExclusiveMaintenance"),
            Self::SnapshotMismatch => f.write_str("SnapshotMismatch"),
            Self::InvalidTransition(e) => f.debug_tuple("InvalidTransition").field(e).finish(),
            Self::InvalidId(e) => f.debug_tuple("InvalidId").field(e).finish(),
            Self::LeaseLost { name, fence } => f
                .debug_struct("LeaseLost")
                .field("name", name)
                .field("fence", fence)
                .finish(),
            Self::Migration { from, to, source } => f
                .debug_struct("Migration")
                .field("from", from)
                .field("to", to)
                .field("source", source)
                .finish(),
            Self::KindNotAllowed => f.write_str("KindNotAllowed"),
            Self::HumanOnly(what) => f.debug_tuple("HumanOnly").field(what).finish(),
            Self::TextIndexUnavailable => f.write_str("TextIndexUnavailable"),
            Self::TextIndexRebuildRefused(why) => {
                f.debug_tuple("TextIndexRebuildRefused").field(why).finish()
            }
            Self::PromoteConflict => f.write_str("PromoteConflict"),
            Self::PromoteTargetIncompatible => f.write_str("PromoteTargetIncompatible"),
            Self::Ontology(e) => f.debug_tuple("Ontology").field(e).finish(),
            Self::EdgeRequiresAssertion => f.write_str("EdgeRequiresAssertion"),
            Self::AssertionSubjectNotFound(id) => {
                f.debug_tuple("AssertionSubjectNotFound").field(id).finish()
            }
            Self::Fixed(e) => f.debug_tuple("Fixed").field(e).finish(),
            Self::ClaimRejected(rel) => f.debug_tuple("ClaimRejected").field(rel).finish(),
            Self::SupersedesInvalid(class) => {
                f.debug_tuple("SupersedesInvalid").field(class).finish()
            }
        }
    }
}

/// Result alias used throughout the store layer.
pub type Result<T> = std::result::Result<T, StoreError>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::{BackendSource, StoreError};

    /// A string no policy rule matches, so an assertion on its absence proves the rendering
    /// itself is content-free, not that a gate caught it.
    const CANARY: &str = "CANARY-driver-text-7f3a9c";

    /// An invented driver error whose message carries the canary, as a SQL text or bound
    /// value quoted by a real driver would.
    #[derive(Debug)]
    struct Driver(String);
    impl std::fmt::Display for Driver {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "no such column: {}", self.0)
        }
    }
    impl std::error::Error for Driver {}

    /// RW3-6: the two schema-lookup refusals render as their class alone; the name looked up
    /// (caller data) reaches neither `Display` nor `Debug`.
    #[test]
    fn a_schema_lookup_refusal_renders_its_class_and_not_the_name() {
        for (err, class) in [
            (StoreError::UnknownKind(CANARY.to_owned()), "unknown_kind"),
            (StoreError::UnknownRel(CANARY.to_owned()), "unknown_rel"),
        ] {
            assert!(err.to_string() == class, "{class}: Display is the class");
            assert!(
                !format!("{err:?}").contains(CANARY),
                "{class}: Debug rendered the name"
            );
            assert!(
                !format!("{err:#?}").contains(CANARY),
                "{class}: alternate Debug rendered the name"
            );
        }
    }

    /// Review RM-1 (core-spec S1): neither `Display` nor `Debug` of a backend failure renders
    /// the driver's text, which may quote SQL or bound values.
    #[test]
    fn a_backend_failure_renders_no_driver_text_in_display_or_debug() {
        let err = StoreError::Backend {
            source: BackendSource::new(Driver(CANARY.to_owned())),
        };
        let display = err.to_string();
        let debug = format!("{err:?}");
        assert_eq!(display, "storage backend error");
        assert!(
            !debug.contains(CANARY),
            "Debug rendered the driver text: {debug}"
        );
        assert!(
            !format!("{err:#?}").contains(CANARY),
            "alternate Debug rendered it"
        );
    }

    /// RM-1: the chain still reaches the driver error object, explicitly, through the holder.
    #[test]
    fn the_driver_error_stays_downcastable_through_the_backend_source_holder() {
        let err = StoreError::Backend {
            source: BackendSource::new(Driver(CANARY.to_owned())),
        };
        let source = std::error::Error::source(&err).expect("Backend keeps a source");
        // the holder ends the chain: nothing below it renders by walking `source()`
        assert!(source.source().is_none());
        assert!(
            !source.to_string().contains(CANARY),
            "holder Display: {source}"
        );
        let holder = source
            .downcast_ref::<BackendSource>()
            .expect("the source is the holder");
        let driver = holder
            .driver()
            .downcast_ref::<Driver>()
            .expect("the held error is the driver error itself");
        assert_eq!(driver.0, CANARY);
    }
}
