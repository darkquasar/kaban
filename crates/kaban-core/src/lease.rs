//! Leases with fencing tokens (core-spec F15, SR-2): `ChangeLog` sinks that may run in any
//! process are guarded by a `leases(name, owner, fence, expires_at)` row. Acquisition
//! increments `fence` in one write transaction; every consumer write and cursor advance
//! carries the fence and is rejected once a successor has acquired the lease. Expiry is
//! decided by the database clock (`unixepoch()`), so clock skew between processes is harmless.

use serde::{Deserialize, Serialize};

/// A held lease: the proof a consumer presents with every fenced write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Lease (consumer) name, for example `export`.
    pub name: String,
    /// Holder, unique per process (for example `<host>:<pid>:<uuid>`).
    pub owner: String,
    /// Fencing token: incremented on every acquisition, never reused.
    pub fence: u64,
    /// Expiry as database time (unix seconds); renew before it passes.
    pub expires_at: i64,
}
