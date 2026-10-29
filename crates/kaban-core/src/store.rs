//! The `GraphStore` portability seam: role traits and the umbrella trait over them.

// MT-9 (review RB-9): the former single 30-method trait is split into role traits grouped by
// their actual users; `GraphStore` below is the umbrella every backend gets for free. Method
// names, signatures and contracts are unchanged.
//
// One file per role trait (OWN-79 (1)): a lane that extends a trait owns that trait's file. The
// children are private; every public item is re-exported here by name, so the
// `crate::store::X` paths and the `lib.rs` re-export list are unchanged. The deprecated
// `OutboxStore` alias stays in this file: the EN-E10b vocabulary test
// (`crates/kaban/tests/changelog_vocabulary.rs`) names it as the alias's one definition site.

use crate::changelog::Change;
use crate::error::{Result, StoreError};

mod anchors;
mod audit;
mod changelog;
mod governance;
mod graph;
mod ingest;
mod jobs;
mod lease;
mod memory;
mod schema;
mod sync;
mod types;

pub use anchors::{
    AnchorPredicates, AttachHandle, SNAPSHOT_SYMBOL_PAGE, SnapshotAttach, SnapshotSymbol,
    SymbolPage,
};
pub use audit::AuditStore;
pub use changelog::ChangeLogStore;
pub use governance::{Chain, ChainDirection, GovernanceStore, PruningProtection, PruningTarget};
pub use graph::GraphStore;
pub use ingest::{DocumentIndexSummary, FailedDocumentRow, IngestStore, MaintenanceOutcome};
pub use jobs::{
    IngestJob, InvalidJobId, JobClaimRequest, JobFinishOutcome, JobFinishRequest,
    JobFinishStateUpdate, JobId, JobLease, JobOpenRequest, JobStatus, JobSubmitContract,
    JobSubmitReceipt, JobSubmitRequest, JobSubmitStateUpdate, PendingJob,
};
pub use lease::LeaseStore;
pub use memory::MemoryStore;
pub use schema::SchemaStore;
pub use sync::{ImportedPurge, SyncStore, UnstampedCounts, UnstampedReason};
pub use types::{DatabaseFileModes, ReviewRow};

/// What a store adapter can do beyond the portable contract (spec 05 SS-23): one `const` per
/// adapter, declared where the adapter is, read by exactly two consumers, the purge report
/// ([`Self::physical_cleanup_report`]) and the admission of a shared open
/// ([`Self::admit_shared_open`]). Every other portability limit (the synchronization policy,
/// durability, the foreign-key and migration checks) is documentation and a refused
/// operation, never a capability branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCapabilities {
    /// The adapter has a step that reclaims a purge's freed pages (a VACUUM), so a purge may
    /// ever be reported physically cleaned.
    pub physical_cleanup: bool,
    /// Several processes may hold one database open at once (shared mode B, SS-21): the
    /// engine serializes their writes itself.
    pub shared_multiprocess: bool,
}

impl StoreCapabilities {
    /// The reason a store without [`Self::physical_cleanup`] gives, before the step's residue.
    pub const CLEANUP_UNSUPPORTED: &'static str = "physical cleanup is not supported by this store";

    /// The purge report's `(physically_cleaned, cleanup)` (`ForgetOutcome`, SS-03) when the
    /// post-commit step `reclaimed` the freed pages or not and `residue` names what it left.
    /// Without the capability the report is incomplete whatever the step claims: never
    /// physically cleaned, the detail opening with [`Self::CLEANUP_UNSUPPORTED`]. With it the
    /// step's word stands: cleaned with no detail, or not cleaned with the residue.
    #[must_use]
    pub fn physical_cleanup_report(
        self,
        reclaimed: bool,
        residue: String,
    ) -> (bool, Option<String>) {
        if !self.physical_cleanup {
            return (
                false,
                Some(format!("{}; {residue}", Self::CLEANUP_UNSUPPORTED)),
            );
        }
        (reclaimed, (!reclaimed).then_some(residue))
    }

    /// Admit one store's open of a database file. A store with [`Self::shared_multiprocess`]
    /// shares the file with other processes (mode B, SS-21: the engine serializes them) and is
    /// admitted at once, owning nothing: `take_ownership` is never called. A store without
    /// must own the file alone: `take_ownership` is the adapter's OS-level exclusion, held for
    /// the store's lifetime and released by the kernel with its process (so nothing a crash
    /// leaves behind refuses the next opener); `Some(owner)` admits the open and the owner
    /// lives with the store, `None` is a file another process owns.
    ///
    /// # Errors
    /// `StoreError::SharedOpenUnsupported` when another process owns the database and this
    /// store cannot share one (nothing is opened); `take_ownership`'s own error.
    pub fn admit_shared_open<O>(
        self,
        take_ownership: impl FnOnce() -> Result<Option<O>>,
    ) -> Result<Option<O>> {
        if self.shared_multiprocess {
            return Ok(None);
        }
        take_ownership()?
            .map(Some)
            .ok_or(StoreError::SharedOpenUnsupported)
    }
}

/// The pre-E2.0 name of [`ChangeLogStore`] with its `outbox_*` method names, kept for one
/// minor version (EN-E10b, OWN-53; removed in 3.2). Every `ChangeLogStore` is an `OutboxStore`
/// through the blanket impl below; the shims delegate. A `#[deprecated] pub use` alias would
/// be silent at use sites on the current toolchain, so the alias is a deprecated supertrait.
#[deprecated(
    since = "3.1.0",
    note = "renamed to ChangeLogStore (EN-E10b); removed in 3.2"
)]
pub trait OutboxStore: ChangeLogStore {
    /// [`ChangeLogStore::changes_read`].
    ///
    /// # Errors
    /// As [`ChangeLogStore::changes_read`].
    #[deprecated(
        since = "3.1.0",
        note = "renamed to changes_read (EN-E10b); removed in 3.2"
    )]
    fn outbox_read(&self, after: u64, limit: usize) -> Result<Vec<Change>> {
        self.changes_read(after, limit)
    }
    /// [`ChangeLogStore::changes_ack`].
    ///
    /// # Errors
    /// As [`ChangeLogStore::changes_ack`].
    #[deprecated(
        since = "3.1.0",
        note = "renamed to changes_ack (EN-E10b); removed in 3.2"
    )]
    fn outbox_ack(&self, consumer: &str, upto: u64, fence: u64) -> Result<()> {
        self.changes_ack(consumer, upto, fence)
    }
    /// [`ChangeLogStore::changes_prune`].
    ///
    /// # Errors
    /// As [`ChangeLogStore::changes_prune`].
    #[deprecated(
        since = "3.1.0",
        note = "renamed to changes_prune (EN-E10b); removed in 3.2"
    )]
    fn outbox_prune(&self, tail: u64) -> Result<u64> {
        self.changes_prune(tail)
    }
}

#[allow(deprecated)] // the one definition site of the alias (EN-E10b)
impl<S: ChangeLogStore + ?Sized> OutboxStore for S {}
