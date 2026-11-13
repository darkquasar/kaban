//! `KabanApi`: the service layer behind every adapter. Spike `Brain` semantics, minus rmcp.
//!
//! This file holds the configuration, the service struct, its constructors and the workspace
//! and validation helpers shared by the operations; the operation bodies live in the private
//! child modules `remember`, `mutate` (link, forget), `query` (recall, traverse, stats) and
//! `schema` (`schema_describe`, `schema_extend`), each an `impl KabanApi` block (MT-4), and
//! `doctor` (the CLI-only health report, C4.2).

pub(crate) mod doctor;
mod mutate;
mod query;
mod remember;
mod schema;
mod write_barrier;

use crate::api::write_barrier::{
    Clock, HOOK_SEARCH_BARRIER, SEARCH_BARRIER, StoreDrain, SystemClock,
};
use crate::enrich::Enricher;
use crate::error::ApiError;
use crate::ontology::OntologyRegistry;
use crate::types::{ReceiptIdentities, WriteReceipt};
use kaban_codegraph::SnapshotProvider;
use kaban_core::config::EngineConfig;
use kaban_core::{Actor, GovernedSet, GraphStore, NodeId, WorkspaceId, WriteCtx};
use kaban_policy::{AuditExport, AuditSigner, Policy};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};

const MAX_PROPS_BYTES: usize = 16 * 1024;
pub(crate) const MAX_PATH_SCOPE: usize = 32;

/// Per-process service configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApiConfig {
    /// The workspace this process is bound to (core-spec S6): writes without an explicit
    /// `workspace_id` land here, reads stay inside it unless the request names another
    /// workspace or the `"*"` selector.
    pub workspace: WorkspaceId,
    /// Root of the git repository this process runs in, if any (M3-23): `accept` stamps
    /// `accepted_commit` from `git rev-parse HEAD` there.
    pub repo_root: Option<std::path::PathBuf>,
    /// Repository identity (the root directory's name, the same rule as the default
    /// workspace, M3-23): stamped on records this process creates or accepts.
    pub repo: Option<String>,
    /// Whether `accept` records the git identity (`user.email`) as `signer` when the caller
    /// gives none (core-spec F4). No flag sets it in M3 (M3-19): always `false`.
    pub record_signer_identity: bool,
    /// The engine configuration (engine-spec §15.1; engine-plan E1.1, SO-11): loaded once at
    /// startup from `<KABAN_HOME>/config.yaml`, the register defaults otherwise. Every
    /// threshold the service applies is read from here and nowhere else.
    pub engine: Arc<EngineConfig>,
}

/// The capability surface: store + denylist policy + audit signer (+ optional JSONL export).
#[derive(Clone)]
pub struct KabanApi {
    pub(crate) store: Arc<dyn GraphStore>,
    pub(crate) policy: Arc<RwLock<Policy>>,
    pub(crate) signer: Arc<dyn AuditSigner>,
    pub(crate) export: Option<Arc<AuditExport>>,
    pub(crate) config: ApiConfig,
    pub(crate) git: crate::git::GitRunner,
    /// Enrichers this process offers to `recall.enrich[]` (core-spec F19); none by default.
    pub(crate) enrichers: Vec<Arc<dyn Enricher>>,
    /// The ontology registry every mixin decision is read from (engine-spec EN-F4; engine-plan
    /// E1.5): the governed set of every write context, `why`'s governing kinds, `remember`'s
    /// dedup rule and `props_schema`, `accept`'s tier and `link`'s `GOVERNS` endpoints. The
    /// shipped files only until the binary hands over the one it loads at startup
    /// ([`Self::with_registry`]); shared by every clone of the service.
    pub(crate) registry: Arc<OntologyRegistry>,
    /// `<KABAN_HOME>/codegraph/<repo-id>`, the one directory the code side reads snapshots
    /// from (stage1-spec G1-F8, G1-S7; stage1-plan CG5.2): set by `kaban serve` together with
    /// the provider, `None` in every other process (hooks, tests without a code graph).
    pub(crate) codegraph_dir: Option<PathBuf>,
    /// The snapshot resolver the engine's adapter (`snapshot.rs`, E2.6) calls with
    /// `ApiConfig.repo` as the join identity (cg-join-v1 §4, SD-5); `None` means every
    /// `code_*` call reports `no_snapshot`. Shared by every clone of the service.
    pub(crate) snapshot_provider: Option<Arc<dyn SnapshotProvider>>,
    /// The per-process head state of the code side (`code/head.rs`, CG7.1): the cached
    /// `HeadInput` (the ancestors of the last seen sha; `rev-parse HEAD` runs every call).
    /// Shared by every clone of the service.
    pub(crate) head: Arc<crate::code::HeadSource>,
    /// The store's receipt identities (spec 05 SS-10, SS-26), read once here: a write path
    /// never reads them again, so a committed write cannot fail on its receipt.
    pub(crate) receipt_identities: Arc<ReceiptIdentities>,
    /// The barrier's clock (the process clock; a fake in the barrier's tests).
    pub(crate) clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for KabanApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KabanApi")
            .field("policy_version", &self.policy_version())
            .field("workspace", &self.config.workspace)
            .finish_non_exhaustive()
    }
}

fn check_unit(name: &str, v: Option<f64>) -> Result<(), ApiError> {
    match v {
        Some(x) if !(0.0..=1.0).contains(&x) || x.is_nan() => {
            Err(ApiError::Invalid(format!("{name} must be within 0..1")))
        }
        _ => Ok(()),
    }
}

impl KabanApi {
    /// Assemble the service over a store, a compiled policy, the store's audit signer (for
    /// `input_hmac` stubs) and an optional JSONL audit export.
    pub fn new(
        store: Arc<dyn GraphStore>,
        policy: Policy,
        signer: Arc<dyn AuditSigner>,
        export: Option<AuditExport>,
    ) -> Self {
        Self::with_config(store, policy, signer, export, ApiConfig::default())
    }

    /// [`Self::new`] with an explicit configuration (bound workspace).
    pub fn with_config(
        store: Arc<dyn GraphStore>,
        policy: Policy,
        signer: Arc<dyn AuditSigner>,
        export: Option<AuditExport>,
        config: ApiConfig,
    ) -> Self {
        // the only read of the identities: a store that cannot say them (a schema older than
        // SS-26 left unmigrated, a driver failure) still serves, with empty identities
        let receipt_identities = ReceiptIdentities::read(store.as_ref()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "the store's receipt identities could not be read");
            ReceiptIdentities::unknown()
        });
        Self {
            receipt_identities: Arc::new(receipt_identities),
            store,
            policy: Arc::new(RwLock::new(policy)),
            signer,
            export: export.map(Arc::new),
            config,
            git: crate::git::GitRunner::default(),
            enrichers: Vec::new(),
            registry: Arc::new(OntologyRegistry::shipped_default()),
            codegraph_dir: None,
            snapshot_provider: None,
            head: Arc::new(crate::code::HeadSource::default()),
            clock: Arc::new(SystemClock),
        }
    }

    /// The same service resolving code-graph snapshots through `provider` over
    /// `codegraph_dir` (`kaban serve` after it derived `<repo-id>`; tests inject
    /// `kaban_codegraph::testing::FixedSnapshot`). Nothing is opened here.
    #[must_use]
    pub fn with_snapshot_provider(
        mut self,
        codegraph_dir: PathBuf,
        provider: Arc<dyn SnapshotProvider>,
    ) -> Self {
        self.codegraph_dir = Some(codegraph_dir);
        self.snapshot_provider = Some(provider);
        self
    }

    /// The same service with its code-side head pinned to `commit` (40 lowercase hex, no
    /// ancestors): `resolve_current` hands the provider this head and never runs git
    /// (stage1-plan CG7.3: the bench harness over a `FixedSnapshot`; `kaban serve` never pins).
    /// Set before the service is cloned, like the provider.
    #[must_use]
    pub fn with_pinned_head(mut self, commit: &str) -> Self {
        self.head = Arc::new(crate::code::HeadSource::pinned(commit));
        self
    }

    /// The same service reading its ontology from `registry` (the binary's startup registry;
    /// a load through it is seen by every clone of the service).
    #[must_use]
    pub fn with_registry(mut self, registry: Arc<OntologyRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// The same service with another git runner (tests point `program` at a wrapper).
    #[must_use]
    pub fn with_git(mut self, git: crate::git::GitRunner) -> Self {
        self.git = git;
        self
    }

    /// The same service with these enrichers configured (core-spec F19). No enricher ships in
    /// v3.0: the default is none, and `recall.enrich[]` then refuses every id.
    #[must_use]
    pub fn with_enrichers(mut self, enrichers: Vec<Arc<dyn Enricher>>) -> Self {
        self.enrichers = enrichers;
        self
    }

    /// The workspace this process is bound to.
    pub fn workspace(&self) -> &WorkspaceId {
        &self.config.workspace
    }

    /// The store this service writes through, for a sibling service that shares it (the
    /// docs-ingest `Ingestor`, DI1.6): storage is still reached only through the `kaban-core`
    /// role traits.
    pub fn store(&self) -> &Arc<dyn GraphStore> {
        &self.store
    }

    /// The audit signer of this process (shared with the docs-ingest `Ingestor`, DI1.6).
    pub fn signer(&self) -> &Arc<dyn AuditSigner> {
        &self.signer
    }

    /// The receipt of a write that committed (spec 05 SS-10), at `seq` when the store
    /// captured the call's terminal sequence inside its transaction, otherwise at the log
    /// head read now (at or after the call's rows). Never an error: a head that cannot be read
    /// leaves the position unknown and is logged, the outcome stays `committed`. Reads no
    /// store state besides that optional head, so a caller inside a store callback passes
    /// `Some`.
    #[must_use]
    pub fn committed_receipt(&self, seq: Option<u64>) -> WriteReceipt {
        let position = seq.or_else(|| match self.store.changes_head() {
            Ok(head) => Some(head),
            Err(e) => {
                tracing::warn!(error = %e, "the log head could not be read after a commit");
                None
            }
        });
        WriteReceipt::committed(&self.receipt_identities, position)
    }

    /// The receipt after the write barrier (SS-10): a `committed` receipt waits at most
    /// [`SEARCH_BARRIER`] for the serving search reader to cover its position, driving the
    /// local drain when one is wired, and becomes `committed_not_yet_searchable` when it does
    /// not; any other receipt is returned as it is. Called once per public write, after the
    /// call's transactions and locks are released; a no-op write waits for the position it
    /// returned like any other. Never an error: a post-commit failure is a pending outcome.
    #[must_use]
    pub fn await_searchable(&self, receipt: WriteReceipt) -> WriteReceipt {
        self.settle(receipt, SEARCH_BARRIER)
    }

    /// [`Self::await_searchable`] inside a hook's budget ([`HOOK_SEARCH_BARRIER`]): the hook
    /// answers within its watchdog and still never claims readiness it did not verify.
    pub(crate) fn await_searchable_in_hook(&self, receipt: WriteReceipt) -> WriteReceipt {
        self.settle(receipt, HOOK_SEARCH_BARRIER)
    }

    fn settle(&self, receipt: WriteReceipt, budget: std::time::Duration) -> WriteReceipt {
        // the store publishes its own pending commits within the barrier's deadline (spec 05
        // OWN-M; `GraphStore::text_publish`): nothing else drains
        let store = StoreDrain(self.store.as_ref());
        write_barrier::settle(
            receipt,
            budget,
            &|| self.store.text_visible(),
            Some(&store),
            self.clock.as_ref(),
        )
    }

    /// The receipt of a write this service refused before anything committed (SS-10
    /// `rejected`): the identities and no position. The transports attach it beside the
    /// reason-specific error.
    #[must_use]
    pub fn rejected_receipt(&self) -> WriteReceipt {
        WriteReceipt::rejected(&self.receipt_identities)
    }

    /// The engine configuration this process runs under (engine-spec §15.1).
    pub fn engine(&self) -> &EngineConfig {
        &self.config.engine
    }

    /// The code-graph snapshot resolver, when `serve` wired one (cg-join-v1 §4).
    pub fn snapshot_provider(&self) -> Option<&Arc<dyn SnapshotProvider>> {
        self.snapshot_provider.as_ref()
    }

    /// `<KABAN_HOME>/codegraph/<repo-id>` of this process, when `serve` derived one.
    pub fn codegraph_dir(&self) -> Option<&Path> {
        self.codegraph_dir.as_deref()
    }

    /// Workspace filter for a read: the request's explicit workspace, `None` (no filter) for
    /// the `"*"` selector, otherwise the bound workspace (core-spec S6).
    pub(crate) fn read_workspace(&self, requested: Option<&str>) -> Option<String> {
        match requested {
            Some(WorkspaceId::ALL) => None,
            Some(ws) => Some(ws.to_owned()),
            None => Some(self.config.workspace.as_str().to_owned()),
        }
    }

    pub(crate) fn pol(&self) -> RwLockReadGuard<'_, Policy> {
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Version of the active denylist policy: the hash of the effective rule set (builtin +
    /// user rules), so a binary upgrade that ships new builtin rules changes it (m2 N12).
    pub fn policy_version(&self) -> String {
        self.pol().effective_hash().to_string()
    }

    /// The ontology registry this service reads (`kaban ontology load` loads through it).
    pub fn registry(&self) -> &OntologyRegistry {
        &self.registry
    }

    /// The governed set this process decides governance by: the registry's (the shipped set
    /// until a load adds a governed kind).
    pub fn governed(&self) -> Arc<GovernedSet> {
        self.registry.governed_set()
    }

    /// A write context for the current policy, the given (or bound) workspace and the
    /// governed set of the loaded ontologies.
    pub(crate) fn write_ctx(&self, workspace: Option<&str>) -> WriteCtx {
        WriteCtx::new(
            self.policy_version(),
            workspace.map_or_else(|| self.config.workspace.clone(), WorkspaceId::new),
            Actor::Agent,
        )
        .with_governed(self.governed())
        .with_discovered(self.registry.discovered(self.engine()).rules)
    }

    /// A gated id string as a [`NodeId`] (D2): a string that is not a node id names no node,
    /// so it is `NotFound` echoing the gated (masked) form, never a parse error.
    pub(crate) fn node_id(gated: &str) -> Result<NodeId, ApiError> {
        NodeId::parse(gated).map_err(|_| ApiError::NotFound(gated.to_owned()))
    }
}
