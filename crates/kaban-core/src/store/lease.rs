//! [`LeaseStore`]: the fenced-lease role trait.

use crate::error::Result;
use crate::lease::Lease;

/// Fenced leases for consumers (core-spec F15). Used by the `LeaseKeeper`.
pub trait LeaseStore: Send + Sync {
    /// Take the lease `name` for `owner` for `ttl_s` seconds of database time when it is free
    /// or expired, incrementing its fence in one write transaction (core-spec F15). `None` when
    /// another owner holds an unexpired lease, and also when `owner` itself does (the fence
    /// must not move under a live holder: renew instead).
    ///
    /// # Errors
    /// Backend errors.
    fn lease_acquire(&self, name: &str, owner: &str, ttl_s: u32) -> Result<Option<Lease>>;
    /// Extend `lease` by `ttl_s` seconds of database time, only while its fence is current.
    ///
    /// # Errors
    /// `LeaseLost` when a successor acquired the lease (or it never existed); backend errors.
    fn lease_renew(&self, lease: &Lease, ttl_s: u32) -> Result<()>;
    /// Give `lease` back (R5-11): its expiry moves into the past while its fence is current,
    /// so the next acquirer takes it at once with the next fence. The row and its monotonic
    /// fence are kept; nothing is deleted.
    ///
    /// # Errors
    /// `LeaseLost` when a successor already acquired the lease (its row is left alone);
    /// backend errors.
    fn lease_release(&self, lease: &Lease) -> Result<()>;
}
