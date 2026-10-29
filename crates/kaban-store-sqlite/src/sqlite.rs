//! `SqliteStore`: the spike's `GraphStore` implementation plus the M1 audit chain
//! (`audit_table`), key handling and `db_id` in `index_meta`.
//!
//! Module map (OWN-79): this file keeps the store type, its fields and `lock`, construction
//! and migration (`open*`, `assemble`, `init_schema`), the shared link and governance
//! machinery (`LinkEffect`, `link_in`, `edge_rule`, `state_of`, `govern`, `mark_target`, the
//! RID helpers, `NodeState`, `Tenant`), the query helpers (`fts_expr`, `WS_COL`,
//! `in_workspace`), the code-graph attach (`cg_read`) and every role-trait impl, which
//! forwards the moved bodies:
//! - `nodes.rs`: the `remember` workflow, node placement and upsert, the content hashes;
//! - `claims.rs`: rescans, deprecation, replay, forget, purge, reactivation;
//! - `batch.rs`: the `apply_batch` transaction, the edge diff and `check_layer_edges`;
//! - `governance.rs`: the governance transition, the `governing` lookup and the
//!   `supersession_chain` walk (v4 CG5.3).

#[path = "artifacts.rs"]
pub(crate) mod artifacts;
mod batch;
mod claims;
mod governance;
mod nodes;

use crate::anchors;
use crate::audit_table;
use crate::candidates;
use crate::changelog;
use crate::confirmations;
use crate::connection::{CgRead, LiveGuard, NoFileBacking, prepare_database_files};
use crate::connection::{database_file_modes, open_existing_as_is};
use crate::doctor;
use crate::events;
use crate::leases;
use crate::ontology::{self, valid_stored_label, valid_stored_rel};
use crate::recall;
use crate::reconcile::{self, persist, project};
use crate::rows::{NODE_COLS, bad_enum, row_edge, row_node};
use crate::schema::{MigrationReport, Migrator, configure_connection};
use crate::session;
use crate::sql_error::SqlResultExt;
use crate::tantivy::{self, Attach, Attached, SharedConnection};
use crate::traverse;
use kaban_core::Result;
use kaban_core::{
    ActorId, AuditEntry, AuditStore, AuditVerification, BatchReport, Change, ChangeLogStore,
    Confirmation, DiscoveredRules, Edge, Event, FailurePatch, ForgetMode, ForgetOutcome,
    Governance, GovernanceStore, GovernedSet, GoverningQuery, Hit, IndexMeta, IngestStore, Lease,
    LeaseStore, Lifecycle, LinkReq, MemoryStore, Method, NewConfirmation, NewEvent, NewNode, Node,
    NodeId, Ontology, Owner, PendingFailuresQuery, RecallQuery, Reconciler, RememberOutcome,
    RescanAction, RescanReport, SchemaStore, SessionEdit, SessionEditKind, StoreError,
    TraverseQuery, TraverseResult, TypeDef, WorkspaceId, WriteBatch, WriteCtx, new_id, now_iso,
};
use kaban_core::{AnchorPredicates, SnapshotAttach, SnapshotSymbol, SymbolPage};
use kaban_core::{Chain, ChainDirection, WorkspaceFilter};
use kaban_core::{
    DatabaseFileModes, DocumentIndexSummary, FailedDocumentRow, MaintenanceOutcome,
    StoreCapabilities,
};
use kaban_policy::{AuditSigner, KeySigner};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) use nodes::content_hash;

/// How long a hook's connection waits for a lock (M4-6): well inside the host's 2 s budget,
/// unlike the server's 5 s.
const HOOK_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// The rel whose edge invalidates (tombstones) its target (OD-4). The ontology's `on_link`
/// vocabulary has `archive_target` only (engine-spec §3.1), so this side effect is still keyed
/// by name; [`GovernedSet::archives_on_link`] decides the archive side effect.
const INVALIDATES_REL: &str = "INVALIDATES";

/// The lifecycle side effect a new edge has on its target (EN-F4 `on_link`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkEffect {
    /// `on_link: archive_target` (`SUPERSEDES` in `adr.yaml`): the target becomes `superseded`
    /// when governed, `archived` otherwise.
    ArchiveTarget,
    /// [`INVALIDATES_REL`]: the target becomes `invalidated` when governed, `tombstoned`
    /// otherwise.
    InvalidateTarget,
}

impl LinkEffect {
    /// The effect of a `rel` edge under `governed`, if any.
    fn of(governed: &GovernedSet, rel: &str) -> Option<Self> {
        if governed.archives_on_link(rel) {
            Some(Self::ArchiveTarget)
        } else if rel == INVALIDATES_REL {
            Some(Self::InvalidateTarget)
        } else {
            None
        }
    }
    /// The governance a governed target takes.
    fn governance(self) -> Governance {
        match self {
            Self::ArchiveTarget => Governance::Superseded,
            Self::InvalidateTarget => Governance::Invalidated,
        }
    }
    /// The lifecycle a non-governed active target takes.
    fn lifecycle(self) -> Lifecycle {
        match self {
            Self::ArchiveTarget => Lifecycle::Archived,
            Self::InvalidateTarget => Lifecycle::Tombstoned,
        }
    }
}

/// SQLite graph store. One connection guarded by a mutex (single writer per process). Audit
/// rows are chained with `signer` inside the same transactions as the writes. Recall's seeds
/// come from the store's own text index (the Tantivy adapter it opens, admits or rebuilds at
/// its open and projects its commits into, spec 05 OWN-M; `crate::tantivy`) or, for an open
/// that attaches none and a database below the FTS5 retirement (spec 05 SS-16), from the
/// in-database FTS5 index the write path still maintains there.
#[derive(Debug)]
pub struct SqliteStore {
    /// The projector thread of a serving store (`crate::projector_thread`; [`Self::serving`]),
    /// `None` for every other store. Declared first: it stops and joins, releasing lease and
    /// lock, before the index and the connection it works over close.
    pub(crate) projector: Option<crate::projector_thread::ProjectorThread>,
    /// The seed index ordinary recall searches (spec 05 SS-07): the store's own adapter
    /// (`crate::tantivy`, set by every open that found or built one), or an injected one
    /// ([`Self::with_text_index`]); `None` only for an open that attaches nothing (a
    /// diagnosis of a database without an index, a pack, a fixture below the change log),
    /// where the in-database FTS5 index serves below the retirement (SS-16) and nothing above.
    /// Declared first: a file store's adapter closes its read-only connection before `conn`
    /// closes, so the last connection out checkpoints and removes the WAL as it always did.
    pub(crate) text_index: Option<Attached>,
    /// Shared with the in-memory adapter's `Rows` (`crate::tantivy`), never lent across an
    /// adapter call: the lock order is this lock, then the adapter.
    conn: SharedConnection,
    db_id: String,
    signer: Arc<dyn AuditSigner>,
    migration: MigrationReport,
    /// The reconciliation policy of this store (engine-spec EN-E9): the injected
    /// `Reconciler`, the ontology and priors its thresholds come from.
    reconcile: reconcile::Setup,
    /// The dedicated read connection the code-graph snapshot is attached on (CG-JOIN v1 §2
    /// SO-4; E2.6): opened lazily by the first `attach_snapshot`, closed by `detach_snapshot`
    /// and at drop (declared before `_live`, so it closes before the registration goes).
    cg_read: Mutex<Option<CgRead>>,
    /// The lease owner of this store's projections (`crate::tantivy::owner`).
    pub(crate) owner: String,
    /// How long a projection on the store's own account may run (`crate::tantivy::PUBLISH_BUDGET`;
    /// a hook's share for a hook open).
    pub(crate) publish_budget: std::time::Duration,
    /// This store's session position (`crate::tantivy`; astra, projection research): the
    /// change-log head after its last commit, 0 while it wrote nothing. A read of this store
    /// drains only while the index does not cover it; a close publishes toward it.
    pub(crate) receipt: std::sync::atomic::AtomicU64,
    /// The database file, `None` for an in-memory store (which has no second connection to
    /// open and refuses an attach).
    path: Option<std::path::PathBuf>,
    /// Registration in the live-store registry ([`LiveGuard`]) for file-backed stores; dropped after
    /// `conn`.
    _live: Option<LiveGuard>,
}

/// A store with pending work closes after one bounded projection (spec 05 OWN-M (d): a
/// one-shot command's exit publishes what it wrote); an in-memory index dies with its store.
impl Drop for SqliteStore {
    fn drop(&mut self) {
        if self.path.is_some() {
            self.publish_pending();
        }
    }
}

#[cfg(feature = "text-index-override")]
impl SqliteStore {
    /// Route ordinary recall's lexical and fuzzy seed searches through `index` in place of the
    /// store's own adapter (spec 05 SS-07): the graph expansion, the fusion, the
    /// materialization and the final order stay this store's. The feature exists for the
    /// adapter goldens, a scripted fake and the bench; the store projects nothing into an
    /// injected index (its publication is the injector's).
    #[must_use]
    pub fn with_text_index(
        mut self,
        index: Box<dyn kaban_core::text_index::TextIndex + Send + Sync>,
    ) -> Self {
        self.text_index = Some(Attached::Override(index));
        self
    }
}

impl SqliteStore {
    /// This adapter's capabilities (spec 05 SS-23): a VACUUM reclaims a purge's freed pages
    /// (`kaban doctor --optimize --vacuum`; the post-commit hygiene does not, so a purge still
    /// reports the residue, SS-03), and SQLite serializes the writers of several processes on
    /// one file itself (shared mode B, SS-21: no daemon, no projector for FTS5). Read by the
    /// purge report (`purge_cleanup`) and by the shared-open admission of `connection.rs`
    /// (`admit_open`, reached by every writable open: ordinary, hook, diagnostic and pack
    /// source), nowhere else.
    pub const CAPABILITIES: StoreCapabilities = StoreCapabilities {
        physical_cleanup: true,
        shared_multiprocess: true,
    };

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // a panic in one call must not take the server down with a poisoned mutex
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The connection as the in-memory adapter shares it (`crate::tantivy`).
    pub(crate) fn connection(&self) -> &SharedConnection {
        &self.conn
    }

    /// The seed index behind the text-search seam, when one is attached.
    pub(crate) fn seed_index(
        &self,
    ) -> Option<&(dyn kaban_core::text_index::TextIndex + Send + Sync)> {
        self.text_index.as_ref().map(Attached::text_index)
    }

    /// The store's own Tantivy adapter (spec 05 OWN-M), `None` for an open that attached
    /// nothing, a pack's captured index or an injected index: the seam the adapter suites of
    /// this crate's tests read the composed index through.
    #[must_use]
    pub fn text_index_adapter(&self) -> Option<Arc<kaban_index_tantivy::TantivyIndex>> {
        self.text_index
            .as_ref()
            .and_then(Attached::tantivy)
            .cloned()
    }

    /// The ontology this store reconciles, ranks and projects under.
    pub(crate) fn ontology(&self) -> &Ontology {
        self.reconcile.ontology()
    }

    /// The recall configuration this store ranks under.
    pub(crate) fn recall_config(&self) -> &kaban_core::config::Recall {
        &self.reconcile.recall
    }

    /// Rank the store's own adapter with the ontology and recall configuration bound now.
    fn configure_index(&self) {
        if let Some(index) = self.text_index.as_ref().and_then(Attached::adapter) {
            index.configure(self.ontology(), self.recall_config());
        }
    }

    /// The same store serving (spec 05 SS-21; `serving` is what the binary knows: this
    /// process is the server): one persistent projector thread competes for the `text-index`
    /// lease and, as the elected publisher, publishes every process's commits as they land,
    /// this store's within their window or at its barrier. A store that does not serve, an
    /// in-memory store (its publication runs inline, synchronously) and a pack (immutable)
    /// start no thread. Call after the configuration seams: the thread ranks with what the
    /// index is configured with when it starts.
    ///
    /// # Errors
    /// The thread's observation connection or the thread itself could not be started.
    pub fn serving(mut self, serving: bool) -> Result<Self> {
        if !serving {
            return Ok(self);
        }
        let (Some(db), Some(index)) = (
            self.path.clone(),
            self.text_index
                .as_ref()
                .and_then(Attached::tantivy)
                .cloned(),
        ) else {
            return Ok(self);
        };
        self.projector = Some(crate::projector_thread::ProjectorThread::start(
            index,
            Arc::clone(&self.conn),
            &db,
        )?);
        Ok(self)
    }

    /// This store as the index component of a pack build (spec 05 SS-18;
    /// `crate::pack::BuildRequest::index`): `Some` when the store owns a Tantivy index whose
    /// projection the builder may hold and drain, `None` for a store with nothing to capture.
    #[must_use]
    pub fn text_index_component(&self) -> Option<&dyn crate::pack::TextIndexComponent> {
        self.text_index
            .as_ref()
            .and_then(Attached::tantivy)
            .map(|_| self as &dyn crate::pack::TextIndexComponent)
    }

    pub(crate) fn cg_read(&self) -> std::sync::MutexGuard<'_, Option<CgRead>> {
        self.cg_read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The schema names of `PRAGMA database_list` on the main connection and on the dedicated
    /// read connection (empty when it was never opened or was closed): the diagnostic behind
    /// CG-JOIN v1 §11 `attach_is_read_only` ("the main connection has no `cg` schema") and
    /// `why_symbol_too_new` ("nothing attached").
    ///
    /// # Errors
    /// Backend errors.
    pub fn attached_schemas(&self) -> Result<(Vec<String>, Vec<String>)> {
        fn names(conn: &Connection) -> Result<Vec<String>> {
            let mut st = conn.prepare("PRAGMA database_list").sql()?;
            let rows = st
                .query_map([], |r| r.get::<_, String>(1))
                .sql()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .sql()?;
            Ok(rows)
        }
        let main = names(&self.lock())?;
        let read = match self.cg_read().as_ref() {
            Some(cg) => names(cg.conn())?,
            None => Vec::new(),
        };
        Ok((main, read))
    }

    /// Conformance seam of CG-JOIN v1 §11 `attach_is_read_only`: run one statement on the
    /// dedicated read connection and return its change count. The connection is read-only by
    /// its open flags and the attached snapshot by its URI, so nothing can be written through
    /// here whatever the statement; a store that never attached has no read connection and
    /// refuses with the fixed `Backend` class.
    ///
    /// # Errors
    /// Backend errors (a write attempt among them).
    #[doc(hidden)]
    pub fn cg_read_execute(&self, sql: &str) -> Result<usize> {
        let slot = self.cg_read();
        let Some(cg) = slot.as_ref() else {
            return Err(StoreError::Backend {
                source: kaban_core::BackendSource::new(NoFileBacking),
            });
        };
        cg.conn().execute(sql, []).sql()
    }

    /// Stable database identity (`UUIDv7` in `index_meta`, created at first open); names the
    /// audit key file.
    pub fn db_id(&self) -> &str {
        &self.db_id
    }

    /// Bind `actor` as the writer of a connection kaban did not open (a test fixture, a repair
    /// tool) before it writes a canonical row (spec 05 SS-26): the schema's stamp trigger
    /// refuses a canonical write from a connection without a writer. Every `open*` binds its
    /// own connection; this is the only other door.
    ///
    /// # Errors
    /// Backend errors.
    pub fn bind_writer(conn: &Connection, actor: &ActorId) -> Result<()> {
        crate::stamp::bind(conn, actor)
    }

    /// The signer behind this database's audit chain (also used for `input_hmac` stubs).
    pub fn signer(&self) -> Arc<dyn AuditSigner> {
        Arc::clone(&self.signer)
    }

    /// What the schema step did when this store was opened (the version found and served).
    pub fn migration(&self) -> &MigrationReport {
        &self.migration
    }

    /// The same store deciding its conflict groups with `reconciler` instead of
    /// `WeightedVoteV1` (engine-spec EN-E9: the trait is the only entry; test injection).
    #[must_use]
    pub fn with_reconciler(mut self, reconciler: Box<dyn Reconciler>) -> Self {
        self.reconcile.set_reconciler(reconciler);
        self
    }

    /// The same store reconciling against `ontology` (the service's loaded ontology: its
    /// thresholds, governed semantics and `priors` block) instead of the shipped one. The
    /// injected reconciler and the `[er]` section are kept (RW5-1): order with
    /// [`Self::with_reconciler`] and [`Self::with_er_priors`] does not matter.
    ///
    /// # Errors
    /// A `priors` value outside `[0, 1]` or an unknown priors key.
    pub fn with_ontology(mut self, ontology: Arc<Ontology>) -> Result<Self> {
        self.reconcile.set_ontology(ontology)?;
        {
            let mut conn = self.write();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .sql()?;
            crate::ident::deploy(&tx, self.reconcile.ontology(), false)?;
            tx.commit().sql()?;
        }
        // a redeployed projection is another binding (SS-09): the index is rebuilt under it
        // here (the write's own publication never rebuilds), then ranks with the new weights
        self.catch_up()?;
        self.configure_index();
        Ok(self)
    }

    /// The same store scoring entity resolution with `er` (E2.10 boundary review BR-7): the
    /// startup seam for the loaded `EngineConfig.er`. The caller validates the section once with
    /// `kaban_core::ErPriors::from_config`, so the error category (`er_unknown_field`,
    /// `er_probability_out_of_range`, `er_threshold_invalid`) and the field path
    /// (`m.<field>`, `possible_threshold`) are reported from the config loader, never from the
    /// store; a store never sees an invalid section. Order with [`Self::with_ontology`] does not
    /// matter: each keeps the other's setting.
    #[must_use]
    pub fn with_er_priors(mut self, er: kaban_core::ErPriors) -> Self {
        self.reconcile.set_er(er);
        self
    }

    /// Bind validated recall settings.
    /// # Errors
    /// Invalid ranking configuration.
    pub fn with_recall_config(mut self, recall: kaban_core::config::Recall) -> Result<Self> {
        recall.effective_rrf_k().map_err(|_| {
            StoreError::InvalidName("recall.fusion".into(), "invalid_configuration")
        })?;
        self.reconcile.recall = recall;
        self.configure_index();
        Ok(self)
    }
}

impl SqliteStore {
    /// Open (creating if missing) the database at `path`, apply the baseline schema and open or
    /// create the audit key `keys_dir/<db-id>.key` (0600, `O_EXCL`; concurrent openers reuse).
    /// The writer is the default `local`; a production open is [`Self::open_as`].
    ///
    /// # Errors
    /// SQLite open/DDL errors, or `StoreError::Audit` when the key cannot be read or created
    /// (including a key file of the wrong length).
    pub fn open(path: &Path, keys_dir: &Path) -> Result<Self> {
        Self::open_as(path, keys_dir, &ActorId::default())
    }
    /// [`Self::open`] writing as `actor` (spec 05 SS-26: the process's `--actor-id`). The
    /// writer is bound on the connection before the migrator runs and fixed for the store's
    /// life: every canonical write, a migration's included, stamps it with this database's
    /// `db_id` as its instance and its `pack_lineage_id`.
    ///
    /// # Errors
    /// As [`Self::open`].
    pub fn open_as(path: &Path, keys_dir: &Path, actor: &ActorId) -> Result<Self> {
        let (conn, db_id, report, live) = Self::open_file(path, actor, Self::CAPABILITIES)?;
        let has_chain: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM audit)", [], |r| r.get(0))
            .sql()?;
        // a database that already carries a chain must never get a fresh key: that would fork
        // the chain silently, so a lost key is a startup error, not a new beginning
        let signer = if has_chain {
            KeySigner::open_existing(keys_dir, &db_id)
        } else {
            KeySigner::open_or_create(keys_dir, &db_id)
        }
        .map_err(|e| StoreError::Audit(Box::new(e)))?;
        Self::assemble(
            conn,
            db_id,
            Arc::new(signer),
            report,
            Some(live),
            Some(path),
        )
        .with_index_attached(Attach::Own)
    }
    /// Open for verification: the key must already exist (never created here) and the
    /// database is read as it is: no migration, no backup, no seeding (MD-28). A file from a
    /// newer kaban is refused. `actor` is the writer, as for every open ([`Self::open_as`]).
    ///
    /// # Errors
    /// SQLite open errors, `StoreError::SchemaUnsupported`, `StoreError::Migration` for a database
    /// bound to a body dictionary this binary does not ship (SS-17), or `StoreError::Audit`
    /// when the key is missing or malformed or the database carries no `db_id` (nothing to
    /// verify).
    pub fn open_existing_key(path: &Path, keys_dir: &Path, actor: &ActorId) -> Result<Self> {
        let (conn, live) = prepare_database_files(path, Self::CAPABILITIES)?;
        configure_connection(&conn)?;
        crate::stamp::bind(&conn, actor)?;
        let version = Migrator::check(&conn)?;
        // the bodies are served only under their own dictionary (SS-17, t7-4)
        crate::body_codec::verify(&conn)?;
        let db_id: Option<String> = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'db_id'",
                [],
                |r| r.get(0),
            )
            .optional()
            .sql()?;
        let db_id = db_id.ok_or_else(|| {
            StoreError::Audit("the database has no db_id (never opened by kaban >= M1)".into())
        })?;
        let signer = KeySigner::open_existing(keys_dir, &db_id)
            .map_err(|e| StoreError::Audit(Box::new(e)))?;
        let report = MigrationReport::unchanged(version);
        Self::assemble(
            conn,
            db_id,
            Arc::new(signer),
            report,
            Some(live),
            Some(path),
        )
        .with_index_attached(Attach::Existing)
    }
    /// Open for `kaban doctor` (core-spec §7; M4-19): the database is read as it is, so the
    /// report describes what is on disk. Nothing is created (a missing file is an error, not a
    /// new database), no file mode is tightened (the modes are `stat`ed first and returned
    /// with the store), no migration, backup or seed runs, and the audit key is never created:
    /// it must already exist, like [`Self::open_existing_key`] (a fresh key would let a gated
    /// rejection during the report write a row the real key cannot verify). A file from a
    /// newer kaban is refused. The store's `migration()` reports the version found. `actor`
    /// is the writer, as for every open ([`Self::open_as`]).
    ///
    /// # Errors
    /// `StoreError::FileMode` (absent, symlink or non-regular file), SQLite open errors,
    /// `StoreError::SchemaUnsupported`, `StoreError::Migration` for a database bound to a body
    /// dictionary this binary does not ship (SS-17), or `StoreError::Audit` when the key is
    /// missing or malformed or the database carries no `db_id`.
    pub fn open_diagnostic(
        path: &Path,
        keys_dir: &Path,
        actor: &ActorId,
    ) -> Result<(Self, DatabaseFileModes)> {
        Self::open_diagnostic_with(path, keys_dir, actor, Self::CAPABILITIES)
    }
    /// [`Self::open_diagnostic`] as the store `capabilities` describes: the admission of the
    /// open is decided by them (`connection::shared_open_tests` exercises the refusal through
    /// this seam; production passes [`Self::CAPABILITIES`]).
    pub(crate) fn open_diagnostic_with(
        path: &Path,
        keys_dir: &Path,
        actor: &ActorId,
        capabilities: StoreCapabilities,
    ) -> Result<(Self, DatabaseFileModes)> {
        let modes = database_file_modes(path)?;
        let (conn, live) = open_existing_as_is(path, capabilities)?;
        configure_connection(&conn)?;
        crate::stamp::bind(&conn, actor)?;
        let version = Migrator::check(&conn)?;
        // the bodies are served only under their own dictionary (SS-17, t7-4)
        crate::body_codec::verify(&conn)?;
        let db_id: Option<String> = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'db_id'",
                [],
                |r| r.get(0),
            )
            .optional()
            .sql()?;
        let db_id = db_id.ok_or_else(|| {
            StoreError::Audit("the database has no db_id (never opened by kaban >= M1)".into())
        })?;
        let signer = KeySigner::open_existing(keys_dir, &db_id)
            .map_err(|e| StoreError::Audit(Box::new(e)))?;
        let report = MigrationReport::unchanged(version);
        let store = Self::assemble(
            conn,
            db_id,
            Arc::new(signer),
            report,
            Some(live),
            Some(path),
        )
        .with_index_attached(Attach::Existing)?;
        Ok((store, modes))
    }
    /// Open for a host hook (core-plan C4.3 as amended by M4-6; review R4-2): a hook fired in a
    /// repository must never create a stray database, migrate one (a `VACUUM INTO` backup and
    /// a schema rebuild do not fit a 2 s budget), seed it or tighten its file modes (a hook is
    /// best-effort and runs as whoever the host runs as; the server tightens at its own open,
    /// C1.6), and must not wait the usual 5 s for a lock. So: the file is opened exactly as it
    /// is through the existing-only open of [`Self::open_diagnostic`] (no create flag, no
    /// `chmod`, a symlink or non-regular file refused: a path that is a dangling name can never
    /// produce a database, whatever happens between a caller's own existence check and this
    /// open; an existing `-wal`/`-shm` sidecar that is a symlink or not a regular file is
    /// refused the same way, RL-4), a database that is not current ([`Migrator::is_current`]:
    /// below the version every open reaches) is refused (open it with `kaban serve` or `kaban
    /// doctor` first), the key must exist, and the busy
    /// timeout is [`HOOK_BUSY_TIMEOUT`]. `actor` is the writer, as for every open
    /// ([`Self::open_as`]).
    ///
    /// # Errors
    /// `StoreError::FileMode` (absent, symlink or non-regular database file or sidecar), `StoreError::Migration`
    /// (version below the latest, or a body dictionary this binary does not ship: a hook
    /// writes bodies, SS-17), `StoreError::SchemaUnsupported`, `StoreError::Audit` (key or
    /// `db_id` missing) or SQLite errors.
    pub fn open_for_hook(path: &Path, keys_dir: &Path, actor: &ActorId) -> Result<Self> {
        Self::open_for_hook_with(path, keys_dir, actor, Self::CAPABILITIES)
    }
    /// [`Self::open_for_hook`] as the store `capabilities` describes (the admission seam of
    /// [`Self::open_diagnostic_with`]).
    pub(crate) fn open_for_hook_with(
        path: &Path,
        keys_dir: &Path,
        actor: &ActorId,
        capabilities: StoreCapabilities,
    ) -> Result<Self> {
        let (conn, live) = open_existing_as_is(path, capabilities)?;
        configure_connection(&conn)?;
        crate::stamp::bind(&conn, actor)?;
        conn.busy_timeout(HOOK_BUSY_TIMEOUT).sql()?;
        let version = Migrator::check(&conn)?;
        // a hook writes bodies: only under the dictionary the database records (SS-17, t7-4)
        crate::body_codec::verify(&conn)?;
        let db_id: Option<String> = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'db_id'",
                [],
                |r| r.get(0),
            )
            .optional()
            .sql()?;
        let db_id = db_id.ok_or_else(|| {
            StoreError::Audit("the database has no db_id (never opened by kaban >= M1)".into())
        })?;
        let signer = KeySigner::open_existing(keys_dir, &db_id)
            .map_err(|e| StoreError::Audit(Box::new(e)))?;
        let report = MigrationReport::unchanged(version);
        let mut store = Self::assemble(
            conn,
            db_id,
            Arc::new(signer),
            report,
            Some(live),
            Some(path),
        );
        // a hook's write publishes its own page within its watchdog's share, or leaves the
        // receipt pending (spec 05 SS-10)
        store.publish_budget = tantivy::HOOK_PUBLISH_BUDGET;
        store.with_index_attached(Attach::Existing)
    }
    /// Open the database at `path` with an explicit signer (the key is managed by the caller).
    ///
    /// # Errors
    /// SQLite open/DDL errors.
    pub fn open_with_signer(path: &Path, signer: Arc<dyn AuditSigner>) -> Result<Self> {
        let (conn, db_id, report, live) =
            Self::open_file(path, &ActorId::default(), Self::CAPABILITIES)?;
        Self::assemble(conn, db_id, signer, report, Some(live), Some(path))
            .with_index_attached(Attach::Own)
    }
    /// An in-memory store with an ephemeral random audit key (tests); the writer is `local`.
    ///
    /// # Errors
    /// SQLite DDL errors or a failing OS random source.
    pub fn open_memory() -> Result<Self> {
        Self::open_memory_as(&ActorId::default())
    }
    /// [`Self::open_memory`] writing as `actor` ([`Self::open_as`]).
    ///
    /// # Errors
    /// As [`Self::open_memory`].
    pub fn open_memory_as(actor: &ActorId) -> Result<Self> {
        let mut conn = Connection::open_in_memory().sql()?;
        let (db_id, report) = Self::init_schema(&mut conn, actor)?;
        let signer = KeySigner::ephemeral().map_err(|e| StoreError::Audit(Box::new(e)))?;
        Self::assemble(conn, db_id, Arc::new(signer), report, None, None)
            .with_index_attached(Attach::Own)
    }
    /// Open the database file with owner-only permissions (core-spec S5; see
    /// [`prepare_database_files`] for the file rules), then create or check its schema and
    /// seed it. The open is admitted as the store `capabilities` describes (production:
    /// [`Self::CAPABILITIES`]).
    pub(crate) fn open_file(
        path: &Path,
        actor: &ActorId,
        capabilities: StoreCapabilities,
    ) -> Result<(Connection, String, MigrationReport, LiveGuard)> {
        let (mut conn, live) = prepare_database_files(path, capabilities)?;
        let (db_id, report) = Self::init_schema(&mut conn, actor)?;
        Ok((conn, db_id, report, live))
    }
    /// The store over a configured connection whose writer is bound.
    pub(crate) fn assemble(
        conn: Connection,
        db_id: String,
        signer: Arc<dyn AuditSigner>,
        migration: MigrationReport,
        live: Option<LiveGuard>,
        path: Option<&Path>,
    ) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            db_id,
            signer,
            migration,
            reconcile: reconcile::Setup::shipped(),
            cg_read: Mutex::new(None),
            projector: None,
            text_index: None,
            owner: tantivy::owner(),
            publish_budget: tantivy::PUBLISH_BUDGET,
            receipt: std::sync::atomic::AtomicU64::new(0),
            path: path.map(Path::to_path_buf),
            _live: live,
        }
    }
    /// The assembled store with its text index (spec 05 OWN-M; `crate::tantivy`): a file
    /// store's index beside the database, admitted or rebuilt as `attach` says, then caught
    /// up to the head; an in-memory store's RAM index, built. A database below the change
    /// log (schema 0004; an older fixture opened at its version) has nothing to project from
    /// and keeps its in-database index.
    fn with_index_attached(mut self, attach: Attach) -> Result<Self> {
        let index = match self.path.clone() {
            Some(db) => self.open_file_index(&db, attach)?,
            None => Some(self.open_memory_index()?),
        };
        self.text_index = index.map(Attached::Tantivy);
        if attach == Attach::Own {
            self.catch_up()?;
        }
        Ok(self)
    }
    /// Configure the connection, bind its writer (SS-26), create the schema on a fresh
    /// database or refuse one at another version (spec 05 OWN-P, [`Migrator`]), seed builtin
    /// types and return the database id (created once).
    fn init_schema(conn: &mut Connection, actor: &ActorId) -> Result<(String, MigrationReport)> {
        configure_connection(conn)?;
        crate::stamp::bind(conn, actor)?;
        let report = Migrator::ensure_current(conn)?;
        // the bodies are served only under their own dictionary (SS-17, t8c-6)
        crate::body_codec::verify(conn)?;
        // builtin seeds, the db_id insert and its read in one write transaction (R2-5): one
        // lock acquisition instead of dozens of contended autocommit writes, and the seed and
        // identity result are atomic; the first opener wins the db_id
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        // the builtin rows are the shipped ontology files (engine-plan E1.3), written in the
        // shape of the schema version reached (E1.7)
        ontology::seed(&tx, &now_iso())?;
        crate::ident::ensure(
            &tx,
            &kaban_core::shipped::resolved().map_err(StoreError::Ontology)?,
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO index_meta(key, value) VALUES ('db_id', ?)",
            [new_id()],
        )
        .sql()?;
        // the instance identity is `db_id`; the pack lineage is set once beside it (SS-26)
        crate::stamp::ensure_pack_lineage(&tx)?;
        let db_id: String = tx
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'db_id'",
                [],
                |r| r.get(0),
            )
            .sql()?;
        tx.commit().sql()?;
        Ok((db_id, report))
    }

    pub(crate) fn kind_exists(conn: &Connection, kind: &str) -> Result<bool> {
        ontology::kind_exists(conn, kind)
    }
    fn rel_def(conn: &Connection, rel: &str) -> Result<Option<TypeDef>> {
        ontology::rel_def(conn, rel)
    }
    /// Create or refresh an edge; the flag is `true` when the edge was inserted. Both
    /// endpoints are resolved inside the tenant's workspace before any write (M3 review
    /// R3-3): an id from another workspace is `NotFound`, so no cross-workspace edge and no
    /// `SUPERSEDES`/`INVALIDATES` side effect on another tenant's node can happen (S6, F17).
    ///
    /// Since E2.2 (engine-spec EN-F9) the edge is not written here: the call is a human claim
    /// (`layer = human`, `method` as the caller says, the caller's confidence as the raw
    /// score, the gated props as the value), recorded as an `assertion` row and projected
    /// through the reconciliation of its triple, so `link`, the inline links of `remember`
    /// and the M5 import all produce one human assertion and one edge. A store below schema
    /// 0004 (fixtures) writes the edge directly, as before.
    #[allow(clippy::too_many_arguments)] // the one transactional edge write; every caller passes all of them
    fn link_in(
        conn: &Connection,
        setup: &reconcile::Setup,
        tenant: &Tenant<'_>,
        src: &str,
        dst: &str,
        rel: &str,
        confidence: Option<f64>,
        props: Option<Value>,
        method: Method,
    ) -> Result<(Edge, bool)> {
        let (srid, skind) = Self::rid_kind_in_workspace(conn, src, tenant.ws)?;
        let (drid, dkind) = Self::rid_kind_in_workspace(conn, dst, tenant.ws)?;
        if srid == drid {
            return Err(StoreError::SelfLink(src.to_string()));
        }
        Self::edge_rule(conn, tenant.governed, rel, &skind, &dkind)?;
        let now = now_iso();
        let props = props.unwrap_or_else(|| json!({}));
        // an existing (src, dst, rel) is refreshed, not created (review R1-8)
        let existed: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM edge WHERE src=? AND dst=? AND rel=?)",
                params![srid, drid, rel],
                |r| r.get(0),
            )
            .sql()?;
        if reconcile::engine_tables(conn)? {
            reconcile::human_edge(
                conn,
                setup,
                tenant.ws,
                reconcile::HumanEdge {
                    src: srid,
                    dst: drid,
                    rel,
                    confidence,
                    props,
                    method,
                    actor: tenant.actor,
                },
                persist::now_unix(),
            )?;
        } else {
            project::write_edge(
                conn,
                &project::EdgeWrite {
                    src: srid,
                    dst: drid,
                    rel,
                    confidence,
                    props: &project::owned_props(Some(&props), Owner::Human),
                    layer: None,
                    method: None,
                    score: None,
                    source_id: None,
                    offset: None,
                    len: None,
                },
            )?;
        }
        let edge = conn
            .query_row(
                "SELECT e.id, s.id, d.id, e.rel, e.confidence, e.created_at, e.props FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst WHERE e.src=? AND e.dst=? AND e.rel=?",
                params![srid, drid, rel],
                row_edge,
            )
            .optional()
            .sql()?
            // the claim's group has no winner (a zero-confidence claim): nothing materialised
            .ok_or_else(|| StoreError::ClaimRejected(rel.to_owned()))?;
        if let Some(effect) = LinkEffect::of(tenant.governed, rel) {
            Self::mark_target(conn, tenant, dst, effect, rel, src, &now)?;
        }
        Ok((edge, !existed))
    }
    /// The declaration of `rel` applied to an edge between a node of `skind` and one of
    /// `dkind` (`UnknownRel`, `EndpointNotAllowed`); the one judgement every edge write
    /// shares: `link`, the inline links of `remember`, a batch's structural edges and the
    /// `edge` claims of a batch (E1.7a RE2-8).
    pub(crate) fn edge_rule(
        conn: &Connection,
        governed: &GovernedSet,
        rel: &str,
        skind: &str,
        dkind: &str,
    ) -> Result<()> {
        let def =
            Self::rel_def(conn, rel)?.ok_or_else(|| StoreError::UnknownRel(rel.to_string()))?;
        // the declared endpoints, and the `GOVERNS` source rule (engine-spec EN-F4, E1.5): the
        // source is a record of a kind that governs paths in the tenant's set. Judged here, in
        // the one transactional edge write, so `link`, the inline links of `remember` and every
        // other producer agree (E1.7a RE2-8); both endpoints are already known to be the
        // tenant's, so no existence leaks across workspaces
        // E1.8 (E1.7 R1): the declared lists are enforced as data for every rel; for `GOVERNS`
        // the source side is the dynamic `governs_paths` rule alone (a user ontology's
        // governing kind is not in the shipped `from:` list, which stays documentary on the
        // row; coordinator decision 2026-10-26), the `to:` list stays enforced
        let from_allowed = if rel == GOVERNS_REL {
            governed.governs_paths(skind)
        } else {
            def.from_kinds.is_empty() || def.from_kinds.iter().any(|k| k == skind)
        };
        if !from_allowed || (!def.to_kinds.is_empty() && !def.to_kinds.iter().any(|k| k == dkind)) {
            return Err(StoreError::EndpointNotAllowed {
                rel: rel.into(),
                from: skind.to_owned(),
                to: dkind.to_owned(),
            });
        }
        Ok(())
    }
    /// Current state of a non-purged node (`NotFound` for an unknown or purged id, D3).
    fn state_of(conn: &Connection, id: &str) -> Result<NodeState> {
        let st = conn
            .query_row(
                "SELECT rid, kind, lifecycle, governance, workspace_id FROM node WHERE id=? AND lifecycle <> 'purged'",
                [id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional().sql()?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        Ok(NodeState {
            rid: st.0,
            id: id.to_owned(),
            kind: st.1,
            lifecycle: Lifecycle::parse(&st.2)
                .ok_or_else(|| bad_enum(2, &st.2))
                .sql()?,
            governance: Governance::parse(&st.3)
                .ok_or_else(|| bad_enum(3, &st.3))
                .sql()?,
            workspace_id: st.4,
        })
    }
    /// The strict governance transition of core-spec §4 on an active governed record, with its
    /// event in the same transaction: `accepted` records `accepted_commit`, the event's path
    /// scope and the process repo identity when `repo` is still unset (M3-23), `superseded`
    /// also archives, `invalidated` also tombstones (`valid_to` set). Every path bumps `rev`
    /// (D4). Returns the event's `seq`.
    fn govern(
        conn: &Connection,
        governed: &GovernedSet,
        st: &NodeState,
        to: Governance,
        ev: &NewEvent,
        repo: Option<&str>,
        now: &str,
    ) -> Result<i64> {
        governed.governance_transition(&st.kind, st.governance, to)?;
        if st.lifecycle != Lifecycle::Active {
            return Err(kaban_core::InvalidTransition::NotActive(st.lifecycle).into());
        }
        match to {
            Governance::Accepted => {
                let path_scope = if ev.path_scope.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&ev.path_scope)?)
                };
                conn.execute(
                    "UPDATE node SET governance='accepted', accepted_commit=?1, path_scope=COALESCE(?2, path_scope), repo=COALESCE(repo, ?5), updated_at=?3, rev=rev+1 WHERE rid=?4",
                    params![ev.commit_sha, path_scope, now, st.rid, repo],
                ).sql()?;
            }
            Governance::Superseded | Governance::Invalidated => {
                let lifecycle = if to == Governance::Superseded {
                    Lifecycle::Archived
                } else {
                    Lifecycle::Tombstoned
                };
                conn.execute(
                    "UPDATE node SET governance=?1, lifecycle=?2, valid_to=?3, updated_at=?3, rev=rev+1 WHERE rid=?4",
                    params![to.as_str(), lifecycle.as_str(), now, st.rid],
                ).sql()?;
            }
            // `none` and `proposed` are never targets; the table above has already refused them
            _ => {
                return Err(kaban_core::InvalidTransition::Governance {
                    from: st.governance,
                    to,
                }
                .into());
            }
        }
        events::append(conn, &st.id, &st.workspace_id, to.as_str(), ev)
    }
    /// The `on_link` side effect of a `rel` edge on its target (MD-7 as decided in wave A Q1 =
    /// option B; EN-F4: `archive_target` rels such as `SUPERSEDES`, plus [`INVALIDATES_REL`]).
    /// Governed targets (the tenant's set) take the strict §4 transition through
    /// [`Self::govern`]: an archive effect on a proposed record fails the whole link.
    /// Non-governed active targets are archived or tombstoned (OD-4), each with an event;
    /// non-active targets stay untouched.
    fn mark_target(
        conn: &Connection,
        tenant: &Tenant<'_>,
        dst: &str,
        effect: LinkEffect,
        rel: &str,
        src: &str,
        now: &str,
    ) -> Result<()> {
        let st = Self::state_of(conn, dst)?;
        let to = effect.governance();
        let ev = NewEvent {
            reason: format!("{rel} edge from {src}"),
            signer_kind: tenant.actor.clone().into(),
            ..Default::default()
        };
        if tenant.governed.contains(&st.kind) {
            // the table refuses superseded, invalidated and (for an archive effect) proposed
            // targets whatever their lifecycle; a non-active proposed/accepted target is left
            // untouched
            tenant
                .governed
                .governance_transition(&st.kind, st.governance, to)?;
            if st.lifecycle == Lifecycle::Active {
                Self::govern(conn, tenant.governed, &st, to, &ev, None, now)?;
            }
            return Ok(());
        }
        if st.lifecycle != Lifecycle::Active {
            return Ok(());
        }
        let lifecycle = effect.lifecycle();
        conn.execute(
            "UPDATE node SET lifecycle=?1, valid_to=?2, updated_at=?2, rev=rev+1 WHERE rid=?3",
            params![lifecycle.as_str(), now, st.rid],
        )
        .sql()?;
        candidates::recount_stats(conn, &st.workspace_id, &st.kind)?;
        events::append(conn, &st.id, &st.workspace_id, to.as_str(), &ev)?;
        Ok(())
    }
    /// Row id and kind of a non-purged node (a purged id names nothing any more, D3).
    pub(crate) fn rid_of(conn: &Connection, id: &str) -> Result<(i64, String)> {
        conn.query_row(
            "SELECT rid, kind FROM node WHERE id=? AND lifecycle <> 'purged'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .sql()?
        .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }
    /// [`Self::rid_of`] restricted to workspace `ws`, with the node's kind (the endpoint check
    /// of a link, R3-3): a batch resolves every id it names inside its own workspace, so
    /// another tenant's node is `NotFound` (R2-1, WD-5, F17: no existence leak, no
    /// cross-workspace edge or side effect).
    pub(crate) fn rid_kind_in_workspace(
        conn: &Connection,
        id: &str,
        ws: &str,
    ) -> Result<(i64, String)> {
        conn.query_row(
            "SELECT rid, kind FROM node WHERE id=? AND workspace_id=? AND lifecycle <> 'purged'",
            params![id, ws],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .sql()?
        .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }
}

/// The state of a stored node that transitions read before writing.
struct NodeState {
    rid: i64,
    id: String,
    kind: String,
    lifecycle: Lifecycle,
    governance: Governance,
    workspace_id: String,
}

/// SQL for a node's workspace (`NOT NULL` since schema version 2; rows written before
/// workspaces were bound were moved to `default` by migration 0002).
pub(crate) const WS_COL: &str = "n.workspace_id";

/// Rust twin of [`WS_COL`]: does `node` belong to `ws` (`None` = no filter)?
pub(crate) fn in_workspace(node: &Node, ws: Option<&str>) -> bool {
    ws.is_none_or(|ws| node.workspace_id == ws)
}

/// Who writes, and in which workspace: every endpoint of an edge write is resolved inside
/// `ws` (R3-3) and the `SUPERSEDES`/`INVALIDATES` event on the target is signed by `actor`.
/// The rel whose source must govern paths (the shipped `adr:GOVERNS`, stored bare).
const GOVERNS_REL: &str = "GOVERNS";

struct Tenant<'a> {
    ws: &'a str,
    actor: &'a kaban_core::Actor,
    /// The governed kinds and `on_link` rels this write decides by (`ctx.governed`, EN-F4).
    governed: &'a GovernedSet,
    /// The discovered-labels policy and cap this write places undeclared labels by
    /// (`ctx.discovered`, EN-F5).
    discovered: &'a DiscoveredRules,
}

impl<'a> Tenant<'a> {
    /// The tenant of a write context in workspace `ws`.
    fn of(ctx: &'a WriteCtx, ws: &'a str) -> Self {
        Self {
            ws,
            actor: &ctx.actor,
            governed: &ctx.governed,
            discovered: &ctx.discovered,
        }
    }
}

impl MemoryStore for SqliteStore {
    fn remember_with(
        &self,
        n: NewNode,
        links: &[LinkReq],
        refs: &[NewNode],
        ctx: &WriteCtx,
    ) -> Result<RememberOutcome> {
        self.remember_with_impl(n, links, refs, ctx)
    }
    fn find_dup_key(&self, workspace_id: &str, dup_key: &str) -> Result<Vec<Node>> {
        let conn = self.lock();
        let mut st = conn.prepare_cached(&format!(
            "SELECT {NODE_COLS} FROM node WHERE lifecycle='active' AND workspace_id=? AND dup_key=? ORDER BY created_at, id"
        )).sql()?;
        let rows = st
            .query_map(params![workspace_id, dup_key], row_node)
            .sql()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().sql()
    }
    fn get(&self, id: &NodeId, include_hidden: bool) -> Result<Option<Node>> {
        let id = id.as_str();
        let conn = self.lock();
        // tombstoned and purged rows are hidden by default; archived stays reachable by id
        let hidden = if include_hidden {
            ""
        } else {
            " AND lifecycle NOT IN ('tombstoned','purged')"
        };
        conn.query_row(
            &format!("SELECT {NODE_COLS} FROM node WHERE id=?{hidden}"),
            [id],
            row_node,
        )
        .optional()
        .sql()
    }
    fn link(
        &self,
        src: &NodeId,
        dst: &NodeId,
        rel: &str,
        confidence: Option<f64>,
        props: Option<Value>,
        ctx: &WriteCtx,
    ) -> Result<Edge> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
        let (e, _) = Self::link_in(
            &tx,
            &self.reconcile,
            &Tenant::of(ctx, ctx.workspace_id.as_str()),
            src.as_str(),
            dst.as_str(),
            rel,
            confidence,
            props,
            Method::Link,
        )?;
        changelog::record_terminal(&tx, ctx)?;
        tx.commit().sql()?;
        Ok(e)
    }
    fn recall(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        self.recall_eligible(q, &|_| true)
    }
    /// The refill counts deliverable results (spec 05 SS-11): `eligible` is applied inside
    /// the recall, before the page is cut, through the one pinned generation.
    fn recall_eligible(
        &self,
        q: &RecallQuery,
        eligible: &dyn Fn(&Node) -> bool,
    ) -> Result<Vec<Hit>> {
        // this store's own commits are read back (spec 05 OWN-M): its receipt position is
        // published first when the index does not cover it, bounded, with no lock held
        self.publish_before_read();
        // the attached adapter may read the store back (an in-memory adapter shares this
        // connection): recall takes the lock per statement and never across an adapter call
        // a store with no index attached has nothing to seed from: refused, never empty
        let index = self.seed_index().ok_or(StoreError::TextIndexUnavailable)?;
        recall::recall_with(
            &recall::Access(&self.conn),
            self.reconcile.ontology(),
            &self.reconcile.recall,
            q,
            index,
            eligible,
        )
    }
    /// The attached index's watermark (spec 05 SS-10). A store with nothing attached has no
    /// watermark to read: the barrier then acknowledges `not_yet_searchable` and doctor
    /// reports the index as not checked, rather than either claiming currency.
    fn text_visible(&self) -> Result<kaban_core::text_index::Visibility> {
        self.seed_index()
            .ok_or(StoreError::TextIndexUnavailable)?
            .visible()
    }
    /// The barrier's drain (spec 05 SS-10, OWN-M): this store's pending commits toward
    /// `target` within the caller's deadline (`crate::tantivy`).
    fn text_publish(&self, target: u64, deadline: std::time::Instant) -> Result<()> {
        self.publish_until(target, deadline)
    }
    fn traverse(&self, q: &TraverseQuery) -> Result<TraverseResult> {
        // lock order main then read (E2.7 default 2): `attach_snapshot` and `snapshot_symbols`
        // take the read slot alone, so no path takes the two reversed
        let conn = self.lock();
        let cg = self.cg_read();
        traverse::traverse(&conn, cg.as_ref(), q)
    }
    fn explain_group(&self, group_key: &str) -> Result<kaban_core::Explanation> {
        crate::explain::explain_group(&self.lock(), &self.reconcile, group_key)
    }
    fn claim_group(&self, key: &str) -> Result<String> {
        crate::explain::claim_group(&self.lock(), key)
    }
    fn claim_group_in(&self, workspace: &str, key: &str) -> Result<String> {
        crate::explain::claim_group_in(&self.lock(), workspace, key)
    }
    fn review_rows(
        &self,
        workspace: &str,
        subject: Option<&NodeId>,
    ) -> Result<Vec<kaban_core::ReviewRow>> {
        candidates::review_rows(&self.lock(), workspace, subject)
    }
    fn proposed_nodes_in(&self, workspace: &str, offset: usize, limit: usize) -> Result<Vec<Node>> {
        crate::explain::proposed_nodes_in(&self.lock(), workspace, offset, limit)
    }
    fn chunk_covering(&self, workspace: &str, path: &str, offset: u64) -> Result<Option<Node>> {
        crate::explain::chunk_covering(&self.lock(), workspace, path, offset)
    }
    fn forget(&self, id: &NodeId, mode: ForgetMode, ctx: &WriteCtx) -> Result<ForgetOutcome> {
        self.forget_impl(id, mode, ctx)
    }
    fn linked_refs(&self, owners: &[NodeId], limit: usize) -> Result<Vec<Node>> {
        if owners.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        // the owners as a positional values table, so the rows come back in owner order (then
        // edge id); both edge directions use their (src|dst, rel) index, the pointer must be
        // an active CodeRef of the owner's workspace (S6: a legacy cross-workspace edge row
        // never surfaces a pointer of another workspace)
        let values = (1..=owners.len())
            .map(|i| format!("(?{i},{})", i - 1))
            .collect::<Vec<_>>()
            .join(",");
        let cols = NODE_COLS
            .split(',')
            .map(|c| format!("c.{c}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "WITH owners(id, pos) AS (VALUES {values}), \
             hop(pos, ws, eid, cref) AS ( \
               SELECT w.pos, o.workspace_id, e.id, e.dst FROM owners w \
                 JOIN node o ON o.id = w.id JOIN edge e ON e.src = o.rid AND e.rel = 'ABOUT' \
               UNION ALL \
               SELECT w.pos, o.workspace_id, e.id, e.src FROM owners w \
                 JOIN node o ON o.id = w.id JOIN edge e ON e.dst = o.rid AND e.rel = 'EVIDENCE_FOR') \
             SELECT {cols} FROM hop h JOIN node c ON c.rid = h.cref \
             WHERE c.kind = 'CodeRef' AND c.lifecycle = 'active' AND c.workspace_id = h.ws \
             ORDER BY h.pos, h.eid LIMIT ?{}",
            owners.len() + 1
        );
        let ids: Vec<&str> = owners.iter().map(NodeId::as_str).collect();
        let mut params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        params.push(&lim);
        let mut st = conn.prepare_cached(&sql).sql()?;
        let rows = st.query_map(params.as_slice(), row_node).sql()?;
        let mut out: Vec<Node> = Vec::new();
        for n in rows {
            let n = n.sql()?;
            if !out.iter().any(|o| o.id == n.id) {
                out.push(n);
            }
        }
        Ok(out)
    }
    // CG-JOIN v1 §2 and §7 (engine-plan E2.6): the attach lives on the dedicated read
    // connection (`CgRead`), every `cg` statement in `anchors.rs`; `coderefs_by_symbol` reads
    // `node_anchor` on the main connection (no `cg` in it).
    fn attach_snapshot(&self, attach: &SnapshotAttach) -> Result<()> {
        let mut slot = self.cg_read();
        if let Some(cg) = slot.as_mut() {
            return cg.ensure(attach);
        }
        let Some(path) = self.path.as_deref() else {
            return Err(StoreError::Backend {
                source: kaban_core::BackendSource::new(NoFileBacking),
            });
        };
        slot.insert(CgRead::open(path)?).ensure(attach)
    }
    fn detach_snapshot(&self) -> Result<()> {
        let mut slot = self.cg_read();
        if let Some(mut cg) = slot.take() {
            cg.detach()?;
        }
        Ok(())
    }
    fn coderefs_by_symbol(&self, p: &AnchorPredicates<'_>, key: &str) -> Result<Vec<Node>> {
        anchors::coderefs_by_symbol(&self.lock(), p, key)
    }
    fn snapshot_symbols(
        &self,
        commit: &str,
        page: SymbolPage,
    ) -> Result<(Vec<SnapshotSymbol>, Option<i64>)> {
        let slot = self.cg_read();
        let Some(cg) = slot.as_ref() else {
            return Err(StoreError::SnapshotMismatch);
        };
        anchors::snapshot_symbols(cg, commit, page)
    }
    fn unmerge(
        &self,
        workspace: &str,
        id: &NodeId,
        reason: &str,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::UnmergeOutcome> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let outcome = crate::reconcile::er::unmerge(
            &tx,
            &self.reconcile,
            workspace,
            id.as_str(),
            reason,
            crate::reconcile::persist::now_unix(),
        )?;
        crate::audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
        changelog::record_terminal(&tx, ctx)?;
        tx.commit().sql()?;
        Ok(outcome)
    }
    fn canonical_of(&self, workspace: &str, id: &NodeId) -> Result<Option<NodeId>> {
        let conn = self.lock();
        // a store without the engine tables has no redirects (the same guard as `recall`)
        if !crate::reconcile::engine_tables(&conn)? {
            return Ok(None);
        }
        // the join drops a `canonical_rid NULL` row (a purged canonical): no redirect
        conn.query_row(
            "SELECT c.id FROM node_identity ni \
             JOIN node n ON n.rid = ni.rid \
             JOIN node c ON c.rid = ni.canonical_rid \
             WHERE n.id = ?1 AND n.workspace_id = ?2",
            rusqlite::params![id.as_str(), workspace],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .sql()?
        .map(|c| NodeId::parse(&c).map_err(Into::into))
        .transpose()
    }
    fn stats(&self, workspace_id: Option<&str>) -> Result<Value> {
        // the cursors (`changes_lag`) follow this process's own publication (spec 05 OWN-M)
        self.publish_before_read();
        let conn = self.lock();
        // node counts inside the workspace; an edge counts when both endpoints are inside
        // (review R1-5); the file size is global and labelled as such
        let ws = format!("(?1 IS NULL OR {WS_COL} = ?1)");
        let edge_ws = "(?1 IS NULL OR (s.workspace_id = ?1 AND d.workspace_id = ?1))";
        let nodes: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM node n WHERE {ws}"),
                [workspace_id],
                |r| r.get(0),
            )
            .sql()?;
        let edges: i64 = conn.query_row(
            &format!("SELECT count(*) FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst WHERE {edge_ws}"),
            [workspace_id],
            |r| r.get(0),
        ).sql()?;
        let mut st = conn
            .prepare(&format!(
                "SELECT n.kind, count(*) FROM node n WHERE {ws} GROUP BY n.kind"
            ))
            .sql()?;
        let by_kind: HashMap<String, i64> = st
            .query_map([workspace_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .sql()?
            .collect::<rusqlite::Result<_>>()
            .sql()?;
        let mut st = conn.prepare(&format!(
            "SELECT e.rel, count(*) FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst WHERE {edge_ws} GROUP BY e.rel"
        )).sql()?;
        let by_rel: HashMap<String, i64> = st
            .query_map([workspace_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .sql()?
            .collect::<rusqlite::Result<_>>()
            .sql()?;
        let page: i64 = conn
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |r| r.get(0),
            )
            .sql()?;
        // the `ChangeLog` lag is database-wide like the file size (F11: max(seq) - min(last_seq));
        // `outbox_lag` is the EN-E10b deprecated alias of `changes_lag`, same value, removed in 3.2
        let changes_lag = changelog::lag(&conn)?;
        // v3 superset (M3-26): the engine, the migrated schema version and the open conflicts
        // (F13), all database-wide
        let schema_version: i64 = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = ?",
                [IndexMeta::SCHEMA_VERSION],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .sql()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let conflicts: i64 = conn
            .query_row("SELECT count(*) FROM conflicts", [], |r| r.get(0))
            .sql()?;
        Ok(json!({
            "nodes": nodes, "edges": edges, "by_kind": by_kind, "by_rel": by_rel, "db_bytes": page,
            "changes_lag": changes_lag, "outbox_lag": changes_lag,
            "engine": "sqlite", "schema_version": schema_version, "conflicts": conflicts,
            "scope": { "workspace": workspace_id.unwrap_or(WorkspaceId::ALL), "db_bytes": "global", "changes_lag": "global", "outbox_lag": "global" },
        }))
    }
}

impl SchemaStore for SqliteStore {
    fn schema(&self) -> Result<Vec<TypeDef>> {
        ontology::all_defs(&self.lock())
    }
    fn schema_extend(&self, def: TypeDef) -> Result<TypeDef> {
        match def.kind.as_str() {
            // the stored-name rule (engine-plan E1.3): bare PascalCase or `ns:PascalCase`
            "label" if !valid_stored_label(&def.name) => {
                return Err(StoreError::InvalidName(
                    def.name,
                    "labels are PascalCase alphanumerics, <=64 chars",
                ));
            }
            "rel" if !valid_stored_rel(&def.name) => {
                return Err(StoreError::InvalidName(
                    def.name,
                    "relationship types are UPPER_SNAKE_CASE, <=64 chars",
                ));
            }
            "label" | "rel" => {}
            _ => {
                return Err(StoreError::InvalidName(
                    def.kind,
                    "kind must be 'label' or 'rel'",
                ));
            }
        }
        if def.description.trim().is_empty() {
            return Err(StoreError::InvalidName(
                def.name,
                "a description is required",
            ));
        }
        let conn = self.lock();
        for k in def.from_kinds.iter().chain(def.to_kinds.iter()) {
            if !Self::kind_exists(&conn, k)? {
                return Err(StoreError::UnknownKind(k.clone()));
            }
        }
        // the gate normalises the spelling (E1.7a RE2-1): `user:Widget` is `Widget`, so one
        // identity has one row on either table shape; the upsert target is the table's
        // primary key: `(kind, name)` before 0004, `(ns, kind, name)` with `ns`/`name` from
        // `split_stored` since (engine-plan E1.7)
        let def = TypeDef {
            name: ontology::normalise_stored(&def.name),
            ..def
        };
        if !ontology::extend(&conn, &def, &now_iso())? {
            return Err(StoreError::Builtin(def.name));
        }
        Ok(def)
    }
}

impl GovernanceStore for SqliteStore {
    fn pruning_protection_get(
        &self,
        target: &kaban_core::PruningTarget,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::PruningProtection> {
        self.protection(target, ctx, None)
    }
    fn pruning_protection_set(
        &self,
        target: &kaban_core::PruningTarget,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::PruningProtection> {
        self.protection(target, ctx, Some(true))
    }
    fn pruning_protection_clear(
        &self,
        target: &kaban_core::PruningTarget,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::PruningProtection> {
        self.protection(target, ctx, Some(false))
    }
    fn transition(
        &self,
        id: &NodeId,
        to: Governance,
        ev: NewEvent,
        ctx: &WriteCtx,
    ) -> Result<(Node, u64)> {
        self.transition_impl(id, to, ev, ctx)
    }
    fn is_candidate(&self, id: &NodeId) -> Result<bool> {
        let conn = self.lock();
        match Self::rid_of(&conn, id.as_str()) {
            Ok((rid, _)) => candidates::is_candidate(&conn, rid),
            Err(StoreError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
    fn confirmation_create(&self, c: NewConfirmation) -> Result<()> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        confirmations::create(&tx, &c)?;
        tx.commit().sql()?;
        Ok(())
    }
    fn confirmation_consume(
        &self,
        token_hash: &str,
        node_id: &NodeId,
    ) -> Result<Option<Confirmation>> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let row = confirmations::consume(&tx, token_hash, node_id.as_str())?;
        tx.commit().sql()?;
        Ok(row)
    }
    fn confirmation_prune(&self) -> Result<u64> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let n = confirmations::prune(&tx)?;
        tx.commit().sql()?;
        Ok(n)
    }
    fn governing(&self, q: &GoverningQuery) -> Result<Vec<Node>> {
        self.governing_impl(q)
    }
    fn events(&self, node: &NodeId) -> Result<Vec<Event>> {
        let conn = self.lock();
        events::list(&conn, node.as_str())
    }
    fn supersession_chain(
        &self,
        id: &NodeId,
        direction: ChainDirection,
        limit: usize,
        ws: &WorkspaceFilter,
    ) -> Result<Chain> {
        self.supersession_chain_impl(id, direction, limit, ws)
    }

    // --- Hook session scratch, briefing and failure pairing (C4.3/C4.4; `session.rs`) ---

    fn session_edit_record(&self, edit: &SessionEdit) -> Result<()> {
        let conn = self.lock();
        session::record(&conn, edit)
    }
    fn session_edits(
        &self,
        session_id: &str,
        kind: SessionEditKind,
        after_ts: i64,
        limit: usize,
    ) -> Result<Vec<SessionEdit>> {
        let conn = self.lock();
        session::list(&conn, session_id, kind, after_ts, limit)
    }
    fn session_edits_prune(&self, older_than_ts: i64, limit: usize) -> Result<u64> {
        let conn = self.lock();
        session::prune(&conn, older_than_ts, limit)
    }
    fn briefing(&self, workspace_id: &str, kinds: &[&str], limit: usize) -> Result<Vec<Node>> {
        let conn = self.lock();
        session::briefing(&conn, kinds, workspace_id, limit)
    }
    fn pending_failures(&self, q: &PendingFailuresQuery) -> Result<Vec<Node>> {
        let conn = self.lock();
        session::pending_failures(&conn, q)
    }
    fn update_failure_props(
        &self,
        id: &NodeId,
        patch: &FailurePatch,
        ctx: &WriteCtx,
    ) -> Result<Node> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        audit_table::append(&tx, self.signer.as_ref(), &ctx.audit)?;
        let node = session::update_failure_props(
            &tx,
            id.as_str(),
            ctx.workspace_id.as_str(),
            patch,
            &now_iso(),
        )?;
        changelog::record_terminal(&tx, ctx)?;
        tx.commit().sql()?;
        Ok(node)
    }
}

impl AuditStore for SqliteStore {
    fn audit_append(&self, entries: &[AuditEntry]) -> Result<()> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        audit_table::append(&tx, self.signer.as_ref(), entries)?;
        tx.commit().sql()?;
        Ok(())
    }
    fn audit_append_read(&self, entries: &[AuditEntry]) -> Result<()> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        audit_table::append_read(&tx, self.signer.as_ref(), entries)?;
        tx.commit().sql()?;
        Ok(())
    }
    fn audit_verify(&self) -> Result<AuditVerification> {
        let conn = self.lock();
        audit_table::verify(&conn, self.signer.as_ref())
    }
}

impl ChangeLogStore for SqliteStore {
    fn changes_read(&self, after: u64, limit: usize) -> Result<Vec<Change>> {
        let conn = self.lock();
        changelog::read(&conn, after, limit)
    }
    fn changes_head(&self) -> Result<u64> {
        changelog::head(&self.lock())
    }
    fn changes_ack(&self, consumer: &str, upto: u64, fence: u64) -> Result<()> {
        let mut conn = self.write();
        // a write path: BEGIN IMMEDIATE like every other (N4, prep brief risk 7)
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        changelog::ack(&tx, consumer, upto, fence)?;
        tx.commit().sql()?;
        Ok(())
    }
    fn changes_register(&self, consumer: &str, fence: u64) -> Result<u64> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let cursor = changelog::register(&tx, consumer, fence)?;
        tx.commit().sql()?;
        Ok(cursor)
    }
    fn changes_lag_by_consumer(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        changelog::lag_by_consumer(&self.lock())
    }
    fn changes_prune(&self, tail: u64) -> Result<u64> {
        // the prune floor is the slowest cursor: this process's own publication first
        self.publish_before_read();
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let deleted = changelog::prune(&tx, tail)?;
        tx.commit().sql()?;
        Ok(deleted)
    }
}

impl LeaseStore for SqliteStore {
    fn lease_acquire(&self, name: &str, owner: &str, ttl_s: u32) -> Result<Option<Lease>> {
        let mut conn = self.write();
        // one BEGIN IMMEDIATE per acquisition (core-spec F15): the fence increment and the
        // read-back are atomic against every other process
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let lease = leases::acquire(&tx, name, owner, ttl_s)?;
        tx.commit().sql()?;
        Ok(lease)
    }
    fn lease_renew(&self, lease: &Lease, ttl_s: u32) -> Result<()> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        leases::renew(&tx, lease, ttl_s)?;
        tx.commit().sql()?;
        Ok(())
    }
    fn lease_release(&self, lease: &Lease) -> Result<()> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        leases::release(&tx, lease)?;
        tx.commit().sql()?;
        Ok(())
    }
}

impl IngestStore for SqliteStore {
    fn job_finish(
        &self,
        request: kaban_core::JobFinishRequest,
        ctx: &WriteCtx,
        report: &mut dyn FnMut(
            &kaban_core::IngestJob,
            &kaban_core::JobFinishOutcome,
        ) -> Result<kaban_core::JobFinishStateUpdate>,
    ) -> Result<String> {
        self.job_finish_impl(&request, ctx, report)
    }
    fn jobs_pending(
        &self,
        workspace: &WorkspaceId,
        dialect: &str,
        prefix: Option<&str>,
        limit: u32,
    ) -> Result<Vec<kaban_core::PendingJob>> {
        crate::jobs::pending(&self.lock(), workspace, dialect, prefix, limit)
    }
    fn job_claim(&self, request: kaban_core::JobClaimRequest) -> Result<Option<kaban_core::Lease>> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let lease = crate::jobs::claim(&tx, request)?;
        tx.commit().sql()?;
        Ok(lease)
    }
    fn job_holder(
        &self,
        job: &kaban_core::JobId,
        workspace: &WorkspaceId,
        fence: Option<u64>,
        renew: bool,
    ) -> Result<()> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let row = crate::jobs::get(&tx, job, workspace)?
            .ok_or_else(|| StoreError::NotFound("job_revoked".into()))?;
        crate::jobs::holder(&tx, &row, fence, renew)?;
        tx.commit().sql()
    }
    fn jobs_exhausted(&self, workspace: &WorkspaceId, attempts: u32) -> Result<u64> {
        crate::jobs::exhausted(&self.lock(), workspace, attempts)
    }
    fn jobs_reindex(
        &self,
        layer: kaban_core::Layer,
        limit: u32,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::RetireReport> {
        self.jobs_reindex_impl(layer, limit, ctx)
    }
    fn jobs_prune(&self, workspace: &WorkspaceId, now: i64, limit: u32) -> Result<u32> {
        crate::jobs::prune(&self.lock(), workspace, now, limit)
    }
    fn job_submit(
        &self,
        request: kaban_core::JobSubmitRequest,
        ctx: &WriteCtx,
        accumulate: &mut dyn FnMut(
            &kaban_core::IngestJob,
            &BatchReport,
        ) -> Result<kaban_core::JobSubmitStateUpdate>,
    ) -> Result<kaban_core::JobSubmitReceipt> {
        self.job_submit_impl(request, ctx, accumulate)
    }
    fn job_open(&self, job: &kaban_core::IngestJob) -> Result<kaban_core::IngestJob> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let row = crate::jobs::open(&tx, job)?;
        tx.commit().sql()?;
        Ok(row)
    }
    fn job_open_checked(
        &self,
        request: kaban_core::JobOpenRequest,
    ) -> Result<kaban_core::IngestJob> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let row = crate::jobs::open_checked(&tx, &request)?;
        tx.commit().sql()?;
        Ok(row)
    }
    fn job_get(
        &self,
        job: &kaban_core::JobId,
        workspace: &WorkspaceId,
    ) -> Result<Option<kaban_core::IngestJob>> {
        crate::jobs::get(&self.lock(), job, workspace)
    }
    fn job_mark_stale(&self, job: &kaban_core::JobId, workspace: &WorkspaceId) -> Result<bool> {
        crate::jobs::mark_stale(&self.lock(), job, workspace)
    }
    fn retire_producer(
        &self,
        authority: kaban_core::CurrentProducerAuthority,
        old_producer: &str,
        batch_rows: u32,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::RetireReport> {
        artifacts::retire_producer(
            &mut self.write(),
            &artifacts::MaintenanceContext {
                setup: &self.reconcile,
                signer: self.signer.as_ref(),
                write: ctx,
            },
            authority,
            old_producer,
            batch_rows,
        )
    }
    fn artifact_diagnostics(
        &self,
        workspace: &WorkspaceId,
        authority: kaban_core::CurrentProducerAuthority,
    ) -> Result<kaban_core::ArtifactDiagnostics> {
        doctor::artifact_diagnostics(&self.lock(), workspace, authority)
    }
    fn maintain_layer(
        &self,
        authority: kaban_core::CurrentProducerAuthority,
        operation: kaban_core::LayerMaintenance,
        document_path: Option<&str>,
        batch_rows: u32,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::RetireReport> {
        artifacts::maintain_layer(
            &mut self.write(),
            &artifacts::MaintenanceContext {
                setup: &self.reconcile,
                signer: self.signer.as_ref(),
                write: ctx,
            },
            authority,
            operation,
            document_path,
            batch_rows,
        )
    }
    fn artifact_source(
        &self,
        id: &NodeId,
        workspace: &WorkspaceId,
    ) -> Result<Option<kaban_core::ArtifactSource>> {
        self.lock().query_row("SELECT rid,content_hash,rev FROM node WHERE id=?1 AND workspace_id=?2 AND lifecycle='active'",params![id.as_str(),workspace.as_str()],|r|Ok(kaban_core::ArtifactSource { source_rid:r.get(0)?,source_rev:kaban_core::SourceRev {content_hash:r.get(1)?,rev:r.get(2)?} })).optional().sql()
    }
    fn gazetteer_generation(&self, document: &NodeId, workspace: &WorkspaceId) -> Result<u64> {
        artifacts::generation(&self.lock(), document, workspace)
    }
    fn gazetteer_set_fresh(&self, document: &NodeId, workspace: &WorkspaceId) -> Result<bool> {
        artifacts::set_fresh(&self.lock(), document, workspace)
    }
    /// The rebuild of the store's text index from the canonical rows; a store with nothing
    /// attached is refused.
    fn reindex_text(&self) -> Result<u64> {
        self.seed_index()
            .ok_or(StoreError::TextIndexUnavailable)?
            .rebuild()?;
        self.lock()
            .query_row(
                "SELECT count(*) FROM node WHERE lifecycle = 'active'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .sql()
            .map(|rows| u64::try_from(rows).unwrap_or_default())
    }
    fn apply_batch(&self, batch: WriteBatch, ctx: &WriteCtx) -> Result<BatchReport> {
        self.apply_batch_impl(batch, ctx)
    }
    fn rescan(
        &self,
        policy_version: &str,
        batch: usize,
        f: &mut dyn FnMut(&Node) -> RescanAction,
    ) -> Result<RescanReport> {
        self.rescan_impl(policy_version, batch, f)
    }
    fn rescan_assertions(
        &self,
        policy_version: &str,
        f: &mut dyn FnMut(&kaban_core::Assertion, &mut Vec<AuditEntry>) -> Option<Value>,
    ) -> Result<RescanReport> {
        self.rescan_assertions_impl(policy_version, f)
    }
    fn deprecate_assertion(
        &self,
        key: &str,
        reason: &str,
        ctx: &WriteCtx,
    ) -> Result<kaban_core::AssertionStatus> {
        self.deprecate_assertion_impl(key, reason, ctx)
    }
    fn resolve_winner(
        &self,
        workspace: &str,
        node: &NodeId,
        value: &str,
        group: Option<&str>,
        ctx: &WriteCtx,
    ) -> Result<String> {
        candidates::resolve_winner(
            &mut self.write(),
            &self.reconcile,
            self.signer.as_ref(),
            workspace,
            node,
            value,
            group,
            ctx,
        )
    }
    fn priors_obs_bump(&self, node: &NodeId, outcome: kaban_core::PriorsOutcome) -> Result<u32> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let (rid, _) = Self::rid_of(&tx, node.as_str())?;
        let counted = crate::explain::priors_obs_bump(&tx, rid, outcome)?;
        tx.commit().sql()?;
        Ok(counted)
    }
    fn priors_obs(&self) -> Result<Vec<kaban_core::PriorsObs>> {
        crate::explain::priors_obs(&self.lock(), &self.reconcile)
    }
    fn unresolved_groups(&self, workspace: &str) -> Result<Vec<String>> {
        crate::explain::unresolved_groups(&self.lock(), workspace)
    }
    fn index_meta(&self) -> Result<IndexMeta> {
        let conn = self.lock();
        let mut st = conn
            .prepare_cached("SELECT key, value FROM index_meta ORDER BY key")
            .sql()?;
        let entries = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .sql()?
            .collect::<rusqlite::Result<_>>()
            .sql()?;
        Ok(IndexMeta { entries })
    }
    fn list_nodes(
        &self,
        kind: &str,
        workspace_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Node>> {
        // the page cap mirrors `traverse`'s; `offset` past the end is an empty page
        let limit = i64::try_from(limit.min(500)).unwrap_or(500);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let conn = self.lock();
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT {NODE_COLS} FROM node n WHERE n.kind = ?1 AND {WS_COL} = ?2 AND n.lifecycle = 'active' ORDER BY n.id LIMIT ?3 OFFSET ?4"
            ))
            .sql()?;
        let rows = st
            .query_map(params![kind, workspace_id, limit, offset], row_node)
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        Ok(rows)
    }
    fn document_index_summary(
        &self,
        kind: &str,
        workspace_id: &str,
    ) -> Result<DocumentIndexSummary> {
        // DI2.0 (c): three aggregate statements over the JSON props of the active rows of the
        // kind; no row is decoded into a `Node` (the listing pages through `list_nodes`)
        let conn = self.lock();
        let scope = format!("n.kind = ?1 AND {WS_COL} = ?2 AND n.lifecycle = 'active'");
        let mut summary = DocumentIndexSummary::default();
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT COALESCE(json_extract(n.props, '$.status'), 'unknown'), count(*), \
                 COALESCE(SUM(json_array_length(n.props, '$.chunk_ids')), 0), \
                 COALESCE(SUM(json_extract(n.props, '$.blocked_chunks')), 0), \
                 COALESCE(SUM(json_extract(n.props, '$.table_unknown_chunks')), 0) \
                 FROM node n WHERE {scope} GROUP BY 1"
            ))
            .sql()?;
        let rows = st
            .query_map(params![kind, workspace_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        for (status, rows, chunks, blocked, table_unknown) in rows {
            summary
                .by_status
                .insert(status, u64::try_from(rows).unwrap_or(0));
            summary.chunks += u64::try_from(chunks).unwrap_or(0);
            summary.blocked_chunks += u64::try_from(blocked).unwrap_or(0);
            summary.table_unknown_chunks += u64::try_from(table_unknown).unwrap_or(0);
        }
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT r.key, SUM(r.value) FROM node n, json_each(n.props, '$.rule_ids') r \
                 WHERE {scope} GROUP BY r.key"
            ))
            .sql()?;
        let hits = st
            .query_map(params![kind, workspace_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        for (rule, n) in hits {
            summary.rule_ids.insert(rule, u64::try_from(n).unwrap_or(0));
        }
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT COALESCE(json_extract(n.props, '$.path'), ''), \
                 COALESCE(json_extract(n.props, '$.error'), '') \
                 FROM node n WHERE {scope} AND json_extract(n.props, '$.status') = 'failed' \
                 ORDER BY 1"
            ))
            .sql()?;
        summary.failed = st
            .query_map(params![kind, workspace_id], |r| {
                Ok(FailedDocumentRow {
                    path: r.get(0)?,
                    error: r.get(1)?,
                })
            })
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        Ok(summary)
    }
    fn set_index_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        ).sql()?;
        Ok(())
    }

    // C4.2 doctor checks (M4-19): see `doctor.rs`
    fn integrity_check(&self) -> Result<MaintenanceOutcome> {
        let conn = self.lock();
        doctor::integrity_check(&conn)
    }
    /// The text index's check: the attached adapter is sound when it serves a payload (a
    /// watermark at 0, an index built over an empty history, is one; an adapter serving none
    /// is a finding, T6 review t6-5); a store with nothing attached is refused.
    fn fts_integrity_check(&self) -> Result<MaintenanceOutcome> {
        use kaban_core::text_index::Visibility;
        let index = self.seed_index().ok_or(StoreError::TextIndexUnavailable)?;
        Ok(match index.visible()? {
            Visibility::Unavailable => MaintenanceOutcome::Failed(vec![
                "the attached text index serves no payload: not built, or not of this database"
                    .into(),
            ]),
            Visibility::Current | Visibility::Through(_) => MaintenanceOutcome::Ok,
        })
    }
    fn vacuum(&self) -> Result<MaintenanceOutcome> {
        let conn = self.lock();
        doctor::vacuum(&conn)
    }
    /// The explicit maintenance pass (spec 05 SS-03): the database hygiene (the TRUNCATE
    /// checkpoint). The store's own adapter merges its segments as it commits, so it has
    /// nothing to run; a store with nothing attached is refused.
    fn optimize_fts(&self) -> Result<MaintenanceOutcome> {
        if self.text_index.is_none() {
            return Err(StoreError::TextIndexUnavailable);
        }
        doctor::optimize_fts(&self.lock())
    }
    fn kind_stats(&self, workspace: &WorkspaceId) -> Result<Vec<kaban_core::KindStat>> {
        let conn = self.lock();
        candidates::kind_stats(&conn, workspace.as_str())
    }
    fn promote_kind(
        &self,
        workspace: &WorkspaceId,
        from: &str,
        to: &kaban_core::Qualified,
        actor: &kaban_core::Actor,
    ) -> Result<kaban_core::PromoteReport> {
        // human-only (EN-F5): judged before the transaction opens
        if *actor != kaban_core::Actor::HumanCli {
            return Err(StoreError::HumanOnly("promote"));
        }
        let to_stored = kaban_core::stored_name(to);
        let ev = NewEvent {
            reason: to_stored.clone(),
            path_scope: Vec::new(),
            signer: None,
            signer_kind: actor.clone().into(),
            host: None,
            session_id: None,
            commit_sha: None,
        };
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let report = candidates::promote(&tx, workspace.as_str(), from, to, &ev, &now_iso())?;
        tx.commit().sql()?;
        Ok(report)
    }
    fn prune_kinds(
        &self,
        workspace: &WorkspaceId,
        below: &kaban_core::config::PromoteWhen,
        older_than: std::time::Duration,
        dry_run: bool,
    ) -> Result<kaban_core::PruneReport> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let report = candidates::prune(
            &tx,
            workspace.as_str(),
            below,
            older_than,
            dry_run,
            &now_iso(),
        )?;
        tx.commit().sql()?;
        Ok(report)
    }
    fn resolve_symbol(
        &self,
        workspace: &str,
        node: &NodeId,
        key: &str,
        ctx: &WriteCtx,
    ) -> Result<()> {
        reconcile::resolve_symbol(
            &mut self.write(),
            &self.reconcile,
            self.signer.as_ref(),
            workspace,
            node,
            key,
            ctx,
        )
    }
}

#[cfg(test)]
// test code: failures are the assertion
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

/// Entity resolution (E2.10): the one pass E2.11's tool and the ingest hook call.
impl SqliteStore {
    /// Resolve `workspace` once (engine-spec EN-F51 (1)-(5)): block, score and decide every pair
    /// the blocks name, write the `identity` claims and the `node_identity` rows of the `Match`
    /// pairs, leave the `Possible` pairs as `merge` candidates, in one write transaction.
    ///
    /// # Errors
    /// Backend errors (the transaction is rolled back).
    pub fn er_resolve(&self, workspace: &str) -> Result<kaban_core::ErReport> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .sql()?;
        let report = crate::reconcile::er::resolve(
            &tx,
            &self.reconcile,
            workspace,
            crate::reconcile::persist::now_unix(),
        )?;
        tx.commit().sql()?;
        Ok(report)
    }
}
