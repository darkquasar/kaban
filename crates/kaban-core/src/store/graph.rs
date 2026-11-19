//! [`GraphStore`]: the umbrella over every role trait and its blanket impl.

use super::{ChangeLogStore, IngestStore, LeaseStore, MemoryStore, SchemaStore, SessionStore};

/// Portability seam. Keep this small and graph-shaped.
///
/// Contract notes for other backends: `remember` is atomic including `links`; dedup is on
/// active nodes only; `forget(Purge)` must erase the node's content, its edges and every
/// derived index entry while keeping the id as a `purged` row; `link` with rel `SUPERSEDES`
/// archives its target and `INVALIDATES` tombstones it, by lifecycle. Ordering and
/// multi-process safety come from the backend's single writer (core-spec S3).
///
/// The umbrella over every role trait: `Arc<dyn GraphStore>` is the one capability type normal
/// callers hold, and a backend that implements every role is a `GraphStore` through the blanket
/// impl below (MT-9). Components that need one role only take that role's trait object.
pub trait GraphStore:
    MemoryStore + SchemaStore + SessionStore + ChangeLogStore + LeaseStore + IngestStore
{
}

impl<S> GraphStore for S where
    S: MemoryStore
        + SchemaStore
        + SessionStore
        + ChangeLogStore
        + LeaseStore
        + IngestStore
        + ?Sized
{
}
