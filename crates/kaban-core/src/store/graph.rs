//! [`GraphStore`]: the umbrella over every role trait and its blanket impl.

use super::{
    AuditStore, ChangeLogStore, GovernanceStore, IngestStore, LeaseStore, MemoryStore, SchemaStore,
    SyncStore,
};

/// Portability seam. Keep this small and graph-shaped.
///
/// Contract notes for other backends: `remember` is atomic including `links` and the
/// `ctx.audit` rows; dedup is on active nodes only; `forget(Purge)` must erase the node's
/// content, its edges and every derived index entry (FTS/vector) while keeping the id as a
/// `purged` row; `link` with rel `SUPERSEDES`/`INVALIDATES` applies the strict core-spec §4
/// transition to its target (a governed target outside the table fails the whole link) and
/// records an event. Audit rows are an append-only HMAC chain inside the same database, so
/// ordering and multi-process safety come from the backend's single writer (core-spec S3).
///
/// The umbrella over every role trait: `Arc<dyn GraphStore>` is the one capability type normal
/// callers hold, and a backend that implements every role is a `GraphStore` through the blanket
/// impl below (MT-9). Components that need one role only take that role's trait object.
pub trait GraphStore:
    MemoryStore
    + SchemaStore
    + GovernanceStore
    + AuditStore
    + ChangeLogStore
    + LeaseStore
    + IngestStore
    + SyncStore
{
}

impl<S> GraphStore for S where
    S: MemoryStore
        + SchemaStore
        + GovernanceStore
        + AuditStore
        + ChangeLogStore
        + LeaseStore
        + IngestStore
        + SyncStore
        + ?Sized
{
}
