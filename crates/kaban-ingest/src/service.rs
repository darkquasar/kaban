//! The service-level ingest lock (docs-ingest-plan.md §16 item 3 (a); DI2.0): one run lock per
//! `(store, workspace)` of a process, shared by every [`Ingestor`](crate::Ingestor) built over
//! that store handle for that workspace, whichever adapter built it.
//!
//! ```text
//!   kaban ingest ----> cli::ingest::index_docs --+--> Ingestor::new(api, store, ..)
//!   MCP index_docs --> cli::ingest::index_docs --+      |  run_lock(store, workspace)
//!                                                       v
//!                        LOCKS: { (store handle, workspace) => Weak<tokio Mutex<()>> }
//!                                                       |
//!                        run(): lock().await (cancellable) ... whole run ... drop
//! ```
//!
//! Every adapter entry point of a process constructs its own `Ingestor` per call (the CLI verb
//! and the MCP tool both go through `cli::ingest::index_docs`), so a per-instance mutex could
//! not stop two `index_docs` calls of one MCP server from ingesting one workspace at the same
//! time (DI1 milestone review). The registry below hands every instance bound to the same store
//! handle and workspace the same `tokio::sync::Mutex`, so the second run waits for the first
//! (honouring its own cancellation token while it waits) and then sees the first run's commits;
//! runs over different workspaces, or over different store handles, do not wait for each other.
//!
//! Scope. The lock is in-process by design: across processes the store's single writer
//! (`BEGIN IMMEDIATE` per document batch, core-spec S3) and the existing lease serialise the
//! commits; no file lock is added. `kaban forget --document` runs one `forget_document`
//! transaction without an `Ingestor` and is serialised by the same store writer, not by this
//! lock; the `kaban://documents` resource and `index_status` are lock-free reads.
//!
//! Locks. The registry's own `std::sync::Mutex` is held for a map lookup only, never across an
//! `.await` (`async-no-lock-await`). The run lock is a `tokio::sync::Mutex` held for the whole
//! run on purpose: it *is* the serialisation (the documented exception of that rule), and it is
//! runtime-agnostic, so the CLI's private current-thread runtime and the MCP server's blocking
//! pool share it. Entries are weak: a lock lives exactly as long as some `Ingestor` holds it, and
//! dead entries are dropped on the next lookup.

use kaban_core::{GraphStore, WorkspaceId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The shared run lock of one `(store, workspace)`.
pub(crate) type RunLock = Arc<tokio::sync::Mutex<()>>;

/// Identity of a store handle inside this process: the address of the shared allocation behind
/// the `Arc` (every clone of one `Arc<dyn GraphStore>`, however it was coerced, has the same
/// one), plus the workspace name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LockKey {
    store: usize,
    workspace: String,
}

impl LockKey {
    fn new(store: &Arc<dyn GraphStore>, workspace: &WorkspaceId) -> Self {
        Self {
            store: Arc::as_ptr(store).cast::<()>() as usize,
            workspace: workspace.as_str().to_owned(),
        }
    }
}

static LOCKS: OnceLock<Mutex<HashMap<LockKey, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();

/// The run lock every `Ingestor` over `store` bound to `workspace` shares in this process.
///
/// A live entry keeps its store alive through the `Ingestor` that holds it, so an address is
/// never reused while its entry can still be upgraded; a dead entry is replaced.
pub(crate) fn run_lock(store: &Arc<dyn GraphStore>, workspace: &WorkspaceId) -> RunLock {
    let key = LockKey::new(store, workspace);
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(live) = locks.get(&key).and_then(Weak::upgrade) {
        return live;
    }
    locks.retain(|_, weak| weak.strong_count() > 0);
    let fresh = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&fresh));
    fresh
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test code: failures are the assertion
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_store_allocation_and_the_workspace() {
        let store: Arc<dyn GraphStore> =
            Arc::new(kaban_store_sqlite::SqliteStore::open_memory().unwrap());
        let same = Arc::clone(&store);
        let other: Arc<dyn GraphStore> =
            Arc::new(kaban_store_sqlite::SqliteStore::open_memory().unwrap());
        let ws = WorkspaceId::new("w");
        assert_eq!(LockKey::new(&store, &ws), LockKey::new(&same, &ws));
        assert_ne!(LockKey::new(&store, &ws), LockKey::new(&other, &ws));
        assert_ne!(
            LockKey::new(&store, &ws),
            LockKey::new(&store, &WorkspaceId::new("v"))
        );
    }

    #[test]
    fn a_lock_is_shared_while_held_and_replaced_once_dropped() {
        let store: Arc<dyn GraphStore> =
            Arc::new(kaban_store_sqlite::SqliteStore::open_memory().unwrap());
        let ws = WorkspaceId::new("w");
        let first = run_lock(&store, &ws);
        let again = run_lock(&store, &ws);
        assert!(
            Arc::ptr_eq(&first, &again),
            "one lock per (store, workspace)"
        );
        assert!(
            !Arc::ptr_eq(&first, &run_lock(&store, &WorkspaceId::new("v"))),
            "another workspace has its own lock"
        );
        drop((first, again));
        // the dead entry was replaced: the lock is unique again
        let fresh = run_lock(&store, &ws);
        assert_eq!(Arc::strong_count(&fresh), 1);
    }
}
