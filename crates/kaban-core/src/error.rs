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
    /// The aggregate exceeds its declared numeric range.
    #[error("rollup numeric overflow")]
    RollupOverflow,
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
    /// An ingestion batch names a workspace other than its write context's: its rows would be
    /// attributed to a tenant other than the context's (R2-1), so the batch is refused before
    /// any write.
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
    /// A query or maintenance budget refused the work (spec 11 QRY-06, 7.6): its memory
    /// ceiling, its deadline or its cancellation. The text is the budget's own; it carries
    /// only the limit and its measured value.
    #[error(transparent)]
    BudgetExceeded(#[from] crate::query_budget::BudgetExceeded),
    /// JSON (de)serialization of props/tags failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The database identity or its HMAC key could not be read (key file missing, unreadable
    /// or malformed). Writes must not proceed without it (core-spec S3).
    #[error("database key unavailable")]
    Audit(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The database file (or a `-wal`/`-shm` sidecar) could not be created or restricted to
    /// owner-only permissions (core-spec S5). The store refuses to open rather than leave a
    /// world-readable database behind.
    #[error("database file could not be created or made owner-only (0600)")]
    FileMode(#[source] std::io::Error),
    /// A sync of a store file to stable storage failed (spec 11 KER-11), so the operation
    /// `op` it served was aborted: nothing it wrote is published or acknowledged, and the sync
    /// is never retried in place (after a failed sync the kernel may have dropped the dirty
    /// pages, so a later success would prove nothing). The process reports it; the text names
    /// the operation, the source is the operating system's error.
    #[error("storage_fatal: {op} could not sync to stable storage")]
    StorageFatal {
        /// The operation that synced (`append_extent`, `create_container`, ...).
        op: &'static str,
        /// The failed sync's error.
        #[source]
        source: std::io::Error,
    },
    /// The database is at another schema version than the one this kaban serves (spec 05
    /// OWN-P: one schema, no migrations; an older and a newer file alike). It is left
    /// untouched: no upgrade path exists (spec 11 CMP-01, OD-1).
    #[error(
        "database schema version {found} is not the version {supported} this kaban serves: no upgrade path exists; create a fresh project and re-ingest"
    )]
    SchemaUnsupported {
        /// Version recorded in the database (1 for an unversioned spike file).
        found: u32,
        /// The one version this binary serves.
        supported: u32,
    },
    /// The database file records no kaban schema while a non-empty `-wal` lies beside it
    /// (spec 11 CMP-01): an unknown writer's log, never replayed. Both files are left
    /// untouched.
    #[error(
        "the database is not a kaban v{supported} project: its main file records no kaban schema and a non-empty -wal lies beside it; move both aside or create a fresh project"
    )]
    NotAKabanProject {
        /// The one version this binary serves.
        supported: u32,
    },
    /// A new node would get a row id above `max`, the highest local id the graph can address
    /// (spec 11 PC-1: `node.rid` is the graph's `u32` local id). The whole write was rolled
    /// back; row ids are never reused, so the project cannot take more nodes.
    #[error(
        "the database has used every node id the graph can address (the highest is {max}); create a fresh project and re-ingest"
    )]
    NodeIdCeiling {
        /// The highest node row id the graph can address.
        max: u32,
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
    /// A lifecycle transition the spec does not allow (core-spec §4).
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
    /// An ontology error raised at the record seam (EN-F7 texts).
    #[error(transparent)]
    Ontology(#[from] crate::ontology::OntologyError),
    /// A producer's edge set or derived artifact does not match its producer, workspace or
    /// source (a gazetteer set the store refuses), so the batch is refused before any write. The text is the fixed class; the triple is caller data and
    /// is not echoed.
    #[error("edge_set_refused")]
    EdgeSetRefused,
    /// A fixed-point input is out of range (EN-F12a: a value above 10000); nothing is written.
    #[error(transparent)]
    Fixed(#[from] crate::FixedError),
}

impl std::fmt::Debug for StoreError {
    /// The derive's rendering for every variant but the two whose payload is the name a caller
    /// looked up (RW3-6: `UnknownKind`, `UnknownRel` render without it).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKind(_) => f.write_str("UnknownKind(<name withheld>)"),
            Self::RollupOverflow => f.write_str("RollupOverflow"),
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
            Self::BudgetExceeded(e) => f.debug_tuple("BudgetExceeded").field(e).finish(),
            Self::Json(e) => f.debug_tuple("Json").field(e).finish(),
            Self::Audit(e) => f.debug_tuple("Audit").field(e).finish(),
            Self::FileMode(e) => f.debug_tuple("FileMode").field(e).finish(),
            Self::StorageFatal { op, source } => f
                .debug_struct("StorageFatal")
                .field("op", op)
                .field("source", source)
                .finish(),
            Self::SchemaUnsupported { found, supported } => f
                .debug_struct("SchemaUnsupported")
                .field("found", found)
                .field("supported", supported)
                .finish(),
            Self::NotAKabanProject { supported } => f
                .debug_struct("NotAKabanProject")
                .field("supported", supported)
                .finish(),
            Self::NodeIdCeiling { max } => {
                f.debug_struct("NodeIdCeiling").field("max", max).finish()
            }
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
            Self::TextIndexUnavailable => f.write_str("TextIndexUnavailable"),
            Self::TextIndexRebuildRefused(why) => {
                f.debug_tuple("TextIndexRebuildRefused").field(why).finish()
            }
            Self::Ontology(e) => f.debug_tuple("Ontology").field(e).finish(),
            Self::EdgeSetRefused => f.write_str("EdgeSetRefused"),
            Self::Fixed(e) => f.debug_tuple("Fixed").field(e).finish(),
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
