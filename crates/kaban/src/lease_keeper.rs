//! `LeaseKeeper` (core-spec F15 "leases are renewed every ttl/3"; core-plan C2.5; MD-14): a
//! Tokio task that renews a held [`Lease`] through the synchronous store on `spawn_blocking`,
//! stops when its `CancellationToken` fires, and stops with a report when the lease is lost
//! (a successor acquired it: the consumer must stop writing immediately, SR-2).
//!
//! The store mutex lives inside the synchronous call; nothing is held across an `.await`.
//!
//! The first lease consumer is the `export` projection of `kaban sync` (M5, C5.1b; M5-19): a
//! short synchronous run that acquires `export` for its default-sink bookkeeping and finishes well
//! within the ttl, so it uses no keeper. The keeper is exercised by its test here, and the
//! fencing it relies on by the C2.5 lease tests and the 8-process test (the contenders there
//! are synchronous threads, MD-15); its first long-running consumer is still to come.
#![allow(dead_code)] // no long-running consumer yet (`sync` needs no keeper); kept unit-tested

use kaban_core::{Lease, LeaseStore, StoreError};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Why the keeper stopped.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum KeeperExit {
    /// The token was cancelled; the lease was still ours at the last renewal.
    Stopped,
    /// A renewal was rejected: a successor holds the lease. Stop consuming.
    Lost {
        /// Lease name.
        name: String,
        /// The fence this keeper was renewing.
        fence: u64,
    },
}

/// Renews one lease every `ttl / 3` until cancelled or lost.
#[derive(Debug)]
pub(crate) struct LeaseKeeper {
    cancel: CancellationToken,
    task: JoinHandle<KeeperExit>,
}

impl LeaseKeeper {
    /// Start renewing `lease` every `ttl_s / 3` seconds (at least every 100 ms). `cancel` stops
    /// the keeper; a child token works too, so one shutdown token can stop every consumer.
    #[must_use]
    pub(crate) fn spawn(
        store: Arc<dyn LeaseStore>,
        lease: Lease,
        ttl_s: u32,
        cancel: CancellationToken,
    ) -> Self {
        let every =
            Duration::from_millis(u64::from(ttl_s) * 1000 / 3).max(Duration::from_millis(100));
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = token.cancelled() => return KeeperExit::Stopped,
                    () = tokio::time::sleep(every) => {}
                }
                let store = Arc::clone(&store);
                let renewed = lease.clone();
                let result =
                    tokio::task::spawn_blocking(move || store.lease_renew(&renewed, ttl_s)).await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(StoreError::LeaseLost { name, fence })) => {
                        tracing::warn!(lease = %name, fence, "lease lost to a successor; stopping");
                        return KeeperExit::Lost { name, fence };
                    }
                    Ok(Err(err)) => {
                        // a transient store error (for example a busy timeout) is not a loss:
                        // the database decides; if renewals keep failing a successor takes
                        // the lease and the next renewal reports the loss
                        tracing::warn!(lease = %lease.name, fence = lease.fence, error = %err, "lease renewal failed; retrying");
                    }
                    Err(join) => {
                        tracing::error!(lease = %lease.name, fence = lease.fence, error = %join, "lease renewal task failed; stopping");
                        return KeeperExit::Lost {
                            name: lease.name.clone(),
                            fence: lease.fence,
                        };
                    }
                }
            }
        });
        Self { cancel, task }
    }

    /// `true` once the keeper has stopped (cancelled or lost).
    #[must_use]
    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Cancel and wait for the keeper; returns why it stopped.
    pub(crate) async fn stop(self) -> KeeperExit {
        self.cancel.cancel();
        self.join().await
    }

    /// Wait for the keeper to stop on its own (loss or an external cancellation).
    pub(crate) async fn join(self) -> KeeperExit {
        match self.task.await {
            Ok(exit) => exit,
            Err(_) => KeeperExit::Stopped,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;
    use kaban_core::GraphStore;
    use kaban_store_sqlite::SqliteStore;
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("kaban-keeper-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn db(&self) -> PathBuf {
            self.0.join("kaban.db")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn expires_at(db: &std::path::Path) -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT expires_at FROM leases WHERE name = 'export'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// C2.5 slice 3 (F15, MD-14): the keeper renews on its own (the expiry moves forward),
    /// stops cleanly on cancellation, and reports the loss when a successor takes the lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_keeper_renews_until_cancelled_and_reports_a_lost_lease() {
        let s = Scratch::new("renew");
        let store: Arc<dyn GraphStore> =
            Arc::new(SqliteStore::open(&s.db(), &s.0.join("keys")).unwrap());
        let lease = store
            .lease_acquire("export", "host:1:k", 3)
            .unwrap()
            .unwrap();
        // make the renewal observable: pull the expiry back so the next renewal must raise it
        rusqlite::Connection::open(s.db())
            .unwrap()
            .execute(
                "UPDATE leases SET expires_at = unixepoch() + 1 WHERE name = 'export'",
                [],
            )
            .unwrap();
        let pulled = expires_at(&s.db());
        let cancel = CancellationToken::new();
        // an `Arc<dyn GraphStore>` upcasts to the keeper's `Arc<dyn LeaseStore>` at the call
        // (MT-9); `store.clone()` rather than `Arc::clone(&store)` so the clone keeps the
        // caller's type and the coercion happens on the argument
        let keeper = LeaseKeeper::spawn(store.clone(), lease.clone(), 3, cancel.child_token());
        // ttl 3 s: a renewal every second
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(
            expires_at(&s.db()) >= pulled + 2,
            "the keeper renewed: {} vs {pulled}",
            expires_at(&s.db())
        );
        assert!(!keeper.is_finished());
        let exit = keeper.stop().await;
        assert!(matches!(exit, KeeperExit::Stopped), "{exit:?}");

        // a successor takes the lease (expired by clock skew): the keeper reports the loss
        let keeper = LeaseKeeper::spawn(store.clone(), lease.clone(), 3, cancel.child_token());
        rusqlite::Connection::open(s.db())
            .unwrap()
            .execute(
                "UPDATE leases SET expires_at = unixepoch() - 1 WHERE name = 'export'",
                [],
            )
            .unwrap();
        let successor = store
            .lease_acquire("export", "host:2:s", 30)
            .unwrap()
            .unwrap();
        assert_eq!(successor.fence, lease.fence + 1);
        let exit = tokio::time::timeout(Duration::from_secs(5), keeper.join())
            .await
            .expect("the keeper stops within a renewal period");
        assert!(
            matches!(&exit, KeeperExit::Lost { name, fence } if name == "export" && *fence == lease.fence),
            "{exit:?}"
        );
        // the successor's lease is untouched by the fenced-out keeper
        assert_eq!(expires_at(&s.db()), successor.expires_at);
    }
}
