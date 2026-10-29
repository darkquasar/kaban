//! [`ChangeLogStore`]: the `ChangeLog` role trait.

use crate::changelog::Change;
use crate::error::Result;
use std::collections::BTreeMap;

/// The `ChangeLog` and its sink cursors (core-spec F11; engine-spec EN-E10b, OWN-53): the
/// `changes` table every write appends to and the `projections` cursors its sinks advance under
/// their lease fence. The first product sink is `kaban sync`'s default sink (`export`).
pub trait ChangeLogStore: Send + Sync {
    /// `ChangeLog` rows with `seq > after`, oldest first, at most `limit` (core-spec F11).
    ///
    /// # Errors
    /// Backend errors.
    fn changes_read(&self, after: u64, limit: usize) -> Result<Vec<Change>>;
    /// Advance `consumer`'s cursor to `upto` (never backwards; the first ack registers the
    /// projection), guarded by the consumer's lease fence (MD-13, SD-10).
    ///
    /// # Errors
    /// `LeaseLost` when no lease named `consumer` holds `fence`; backend errors.
    fn changes_ack(&self, consumer: &str, upto: u64, fence: u64) -> Result<()>;
    /// Register `consumer`'s retention cursor when the consumer is installed, before its
    /// initial snapshot or consumption begins (spec 05 SS-06): a new consumer's cursor is the
    /// newest `seq` (0 on an empty log), an existing cursor is kept as it is. From then on
    /// [`Self::changes_prune`] keeps every row after it. Returns the cursor to consume after; a
    /// snapshot taken after registration may establish its state, and the replay of rows it
    /// already covers is idempotent (EN-E10a). History pruned before registration is not
    /// promised. Guarded by the consumer's lease fence, like [`Self::changes_ack`].
    ///
    /// # Errors
    /// `LeaseLost` when no lease named `consumer` holds `fence`; backend errors.
    fn changes_register(&self, consumer: &str, fence: u64) -> Result<u64>;
    /// The retention lag of every registered consumer, by name (spec 05 SS-21, `doctor`): the
    /// newest `seq` minus the consumer's cursor, 0 for one at the head; empty without a
    /// registered consumer. The slowest one is what `stats.changes_lag` reports.
    ///
    /// # Errors
    /// Backend errors.
    fn changes_lag_by_consumer(&self) -> Result<BTreeMap<String, u64>>;
    /// Delete `ChangeLog` rows below the slowest cursor minus `tail` (without consumers: keep
    /// the newest `tail` rows); the newest row always survives so `seq` never restarts.
    /// Returns the number of rows deleted.
    ///
    /// # Errors
    /// Backend errors.
    fn changes_prune(&self, tail: u64) -> Result<u64>;
    /// The newest `ChangeLog` `seq` (0 on an empty log): the position a write receipt reports
    /// when the write did not carry its own terminal `seq` out of its transaction (spec 05
    /// SS-10). A local position scoped by the database's `instance_id`, never an identity
    /// (SS-27).
    ///
    /// # Errors
    /// Backend errors.
    fn changes_head(&self) -> Result<u64>;
}
