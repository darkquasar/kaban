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
use crate::types::{
    CatalogStatus, CatalogVerify, CompactReport, ReceiptIdentities, ReindexReport, ReindexTarget, WriteReceipt,
};
use kaban_codegraph::SnapshotProvider;
use kaban_core::config::EngineConfig;
use kaban_core::{
    Actor, Durability, GraphStore, NodeId, RuleSet, StoreError, WorkspaceId, WriteCtx,
};
use kaban_policy::{AuditSigner, Policy};
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
    /// Root of the git repository this process runs in, if any (M3-23): the code side resolves
    /// the head and its snapshot there.
    pub repo_root: Option<std::path::PathBuf>,
    /// Repository identity (the root directory's name, the same rule as the default
    /// workspace, M3-23): stamped on records this process creates.
    pub repo: Option<String>,
    /// The engine configuration (engine-spec §15.1; engine-plan E1.1, SO-11): loaded once at
    /// startup from `<KABAN_HOME>/config.yaml`, the register defaults otherwise. Every
    /// threshold the service applies is read from here and nowhere else.
    pub engine: Arc<EngineConfig>,
}

/// The catalog maintenance of the store a service writes through (spec 11 T6): the store's
/// operations the role traits do not carry, wired by the binary ([`KabanApi::with_catalog`]).
pub trait CatalogMaintenance: Send + Sync {
    /// Seal, fold and reclaim by the configured policy, then vacuum and checkpoint.
    ///
    /// # Errors
    /// Store errors (a unit its budget refuses is a skip, not an error).
    fn compact(&self) -> Result<CompactReport, StoreError>;
    /// Clear what a crashed process left (stale claims, unpublished ranges); a one-line
    /// summary of what it did.
    ///
    /// # Errors
    /// Store errors.
    fn recover(&self) -> Result<String, StoreError>;
    /// Verify the catalog: the index states and every inconsistency found.
    ///
    /// # Errors
    /// Store errors.
    fn verify(&self) -> Result<CatalogVerify, StoreError>;
    /// The cheap summary (`stats`, doctor's budget row): the index states and the units the
    /// last compaction refused at their smallest size, with no container scan and no hashing.
    ///
    /// # Errors
    /// Store errors.
    fn status(&self) -> Result<CatalogStatus, StoreError>;
    /// The bounded checkpoint after a one-shot write's acknowledgement (KER-09): whether it
    /// ran (it runs once its interval elapsed).
    ///
    /// # Errors
    /// Store errors.
    fn checkpoint_if_due(&self) -> Result<bool, StoreError>;
}

/// The capability surface: store + denylist policy + keyed-hash signer.
#[derive(Clone)]
pub struct KabanApi {
    pub(crate) store: Arc<dyn GraphStore>,
    pub(crate) policy: Arc<RwLock<Policy>>,
    pub(crate) signer: Arc<dyn AuditSigner>,
    pub(crate) config: ApiConfig,
    pub(crate) git: crate::git::GitRunner,
    /// Enrichers this process offers to `recall.enrich[]` (core-spec F19); none by default.
    pub(crate) enrichers: Vec<Arc<dyn Enricher>>,
    /// The ontology registry every ontology decision is read from (engine-spec EN-F4;
    /// engine-plan E1.5): the rule set of every write, `remember`'s dedup rule and
    /// `props_schema`. The
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
    /// The store's catalog maintenance (`None`: compact, verify and recover are refused and
    /// `stats.indexes` is empty).
    pub(crate) catalog: Option<Arc<dyn CatalogMaintenance>>,
    /// How hard every write's commit waits for the disk (`--durable`: `Full`, KER-04).
    pub(crate) durability: Durability,
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
    /// Assemble the service over a store, a compiled policy and the store's signer (the keyed
    /// hashes of command fingerprints and ingest path ids).
    pub fn new(store: Arc<dyn GraphStore>, policy: Policy, signer: Arc<dyn AuditSigner>) -> Self {
        Self::with_config(store, policy, signer, ApiConfig::default())
    }

    /// [`Self::new`] with an explicit configuration (bound workspace).
    pub fn with_config(
        store: Arc<dyn GraphStore>,
        policy: Policy,
        signer: Arc<dyn AuditSigner>,
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
            config,
            git: crate::git::GitRunner::default(),
            enrichers: Vec::new(),
            registry: Arc::new(OntologyRegistry::shipped_default()),
            codegraph_dir: None,
            snapshot_provider: None,
            head: Arc::new(crate::code::HeadSource::default()),
            clock: Arc::new(SystemClock),
            catalog: None,
            durability: Durability::Normal,
        }
    }

    /// The same service maintaining the store's catalog through `catalog`.
    #[must_use]
    pub fn with_catalog(mut self, catalog: Arc<dyn CatalogMaintenance>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// The same service committing every write with `durability` (`--durable`: `Full`).
    #[must_use]
    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// The store's catalog maintenance, if wired.
    pub fn catalog(&self) -> Option<&Arc<dyn CatalogMaintenance>> {
        self.catalog.as_ref()
    }

    /// Compact the catalog (`kaban compact`, PC-7).
    ///
    /// # Errors
    /// `ApiError::Invalid` when no catalog is wired; store errors.
    pub fn compact(&self) -> Result<CompactReport, ApiError> {
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| ApiError::Invalid("compact needs the store's catalog".into()))?;
        Ok(catalog.compact()?)
    }

    /// Rebuild the derived indexes `which` names from the canonical rows (`kaban reindex`,
    /// PC-7); canonical records are never changed.
    ///
    /// # Errors
    /// Store errors.
    pub fn reindex(&self, which: ReindexTarget) -> Result<ReindexReport, ApiError> {
        let rows = match which {
            ReindexTarget::Text | ReindexTarget::All => Some(self.store.reindex_text()?),
            ReindexTarget::Graph => None,
        };
        // the memory graph has no index on the catalog to rebuild until T11
        Ok(ReindexReport {
            rows,
            graph_rebuilt: false,
        })
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
        self.store.adopt_rules(registry.rule_set());
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

    /// The keyed-hash signer of this process (shared with the docs-ingest `Ingestor`, DI1.6).
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

    /// The rule set this process decides writes by: the registry's (the shipped set until a
    /// load adds rules).
    pub fn rules(&self) -> Arc<RuleSet> {
        self.registry.rule_set()
    }

    /// A write context for the given (or bound) workspace.
    pub(crate) fn write_ctx(&self, workspace: Option<&str>) -> WriteCtx {
        WriteCtx::new(
            workspace.map_or_else(|| self.config.workspace.clone(), WorkspaceId::new),
            Actor::Agent,
        )
        .with_durability(self.durability)
    }

    /// A gated id string as a [`NodeId`] (D2): a string that is not a node id names no node,
    /// so it is `NotFound` echoing the gated (masked) form, never a parse error.
    pub(crate) fn node_id(gated: &str) -> Result<NodeId, ApiError> {
        NodeId::parse(gated).map_err(|_| ApiError::NotFound(gated.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: failures are the assertion

    use super::*;
    use kaban_store_sqlite::SqliteStore;

    #[test]
    fn a_durable_service_commits_every_write_with_a_full_sync() {
        // Arrange
        let store = Arc::new(SqliteStore::open_memory().unwrap());
        let signer = store.signer();
        let api = KabanApi::new(store, Policy::empty(), signer).with_durability(Durability::Full);

        // Act
        let ctx = api.write_ctx(None);

        // Assert
        assert_eq!(ctx.durability, Durability::Full);
    }

    /// A catalog whose verification always fails and whose cheap status names one index.
    struct StatusOnly;

    impl CatalogMaintenance for StatusOnly {
        fn compact(&self) -> Result<CompactReport, StoreError> {
            Ok(CompactReport::default())
        }
        fn recover(&self) -> Result<String, StoreError> {
            Ok(String::new())
        }
        fn verify(&self) -> Result<CatalogVerify, StoreError> {
            Err(StoreError::NotFound("verify is not for stats".into()))
        }
        fn status(&self) -> Result<CatalogStatus, StoreError> {
            Ok(CatalogStatus {
                indexes: vec![crate::types::CatalogIndexStatus {
                    index: crate::types::IndexName::Text,
                    sediment_rows: 7,
                    layers: 0,
                    base_generation: None,
                    last_compaction: None,
                }],
                refused_units: 0,
            })
        }
        fn checkpoint_if_due(&self) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    #[test]
    fn stats_reads_the_cheap_catalog_status_not_its_verification() {
        // Arrange
        let store = Arc::new(SqliteStore::open_memory().unwrap());
        let signer = store.signer();
        let api = KabanApi::new(store, Policy::empty(), signer).with_catalog(Arc::new(StatusOnly));

        // Act
        let stats = api.stats(crate::types::StatsRequest::default()).unwrap();

        // Assert
        assert_eq!(stats.indexes.len(), 1, "{:?}", stats.indexes);
        assert_eq!(stats.indexes[0].sediment_rows, 7);
    }
}
