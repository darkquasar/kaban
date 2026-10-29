//! Write receipts (spec 05 SS-10): the one typed outcome every public write returns, whatever
//! the transport (MCP, CLI, hook, loopback HTTP). A response type embeds it as `receipt`; the
//! adapters serialize the response as it is, so the receipt is byte-identical across them.
//!
//! Building a receipt never fails and never reads the store: the identities are read once
//! when the service is built ([`ReceiptIdentities::read`]) and the position comes from the
//! store's own transaction, so a write that committed can never be reported as anything but
//! committed (SS-10 "post-commit failure never becomes rejection").

use crate::error::ApiError;
use kaban_core::{GraphStore, IndexMeta};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What became of the requested mutation (SS-10). On FTS5 the index is written in the
/// mutation's own transaction, so a commit is searchable at once. An index with its own
/// storage (Tantivy, SS-09) publishes behind the commit: the service waits one bounded
/// barrier for the serving reader to cover the receipt's position and otherwise reports the
/// third outcome, which is never a rejection and is never replayed. The enum is
/// `non_exhaustive` so a further outcome lands without touching a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WriteOutcome {
    /// The requested mutation did not commit (nothing stored; the existing rejection auditing
    /// and the reason-specific nonzero CLI status stand). The error that refused it says why.
    Rejected,
    /// Committed: the state is incorporated through the receipt's `seq`. CLI status 0.
    Committed,
    /// Committed, and the search index had not incorporated the receipt's position when the
    /// visibility barrier expired or failed: the write stands, a later search will show it,
    /// do not replay the mutation. CLI status 6.
    CommittedNotYetSearchable,
}

/// The receipt of one public write (SS-10), independent of transport.
///
/// SS-27 rule: every identifier that leaves a database is a UUID. `instance_id` and
/// `pack_lineage_id` are the store's UUIDs; the record references of the enclosing response
/// (node and edge ids, claim keys) are UUIDs or content hashes already; `seq` is a local
/// `ChangeLog` position scoped by `instance_id`, never a record identity. No SQLite row id
/// (`rid`, `rowid`) may appear in a receipt or beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriteReceipt {
    /// `rejected`, `committed` or `committed_not_yet_searchable` (see [`WriteOutcome`]).
    pub outcome: WriteOutcome,
    /// The database copy that answered: its `db_id` (SS-26 `instance_id`).
    pub instance_id: String,
    /// The pack lineage the database belongs to (SS-26; kept across forks and copies).
    pub pack_lineage_id: String,
    /// The `ChangeLog` position the outcome holds at. For `committed`, the call's terminal
    /// sequence: its last change row, read inside its transaction (the head at that moment
    /// when the call wrote no change row, so a no-op still names the state it saw); `None`
    /// only when the position could not be read after the commit (the write stands; the
    /// service logs the read failure). For `rejected`, `None`: nothing of the call is in the
    /// log.
    pub seq: Option<u64>,
}

/// The per-database identities every receipt carries (SS-26): `db_id` and the pack lineage.
/// Immutable for the life of a database (a fork mints a new `db_id` in a new file), so the
/// service reads them once at construction and never again on a write path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptIdentities {
    /// The store's `db_id`.
    pub instance_id: String,
    /// The store's pack lineage id.
    pub pack_lineage_id: String,
}

impl ReceiptIdentities {
    /// The identities as `store` holds them.
    ///
    /// # Errors
    /// Store errors reading `index_meta`, or a database that lacks one of the two keys (a
    /// schema older than SS-26 that was not migrated).
    pub fn read(store: &dyn GraphStore) -> Result<Self, ApiError> {
        let meta = store.index_meta()?;
        let identity = |key: &str| {
            meta.get(key).map(str::to_owned).ok_or_else(|| {
                ApiError::Store(kaban_core::StoreError::Audit(
                    format!("the database has no {key}").into(),
                ))
            })
        };
        Ok(Self {
            instance_id: identity(IndexMeta::DB_ID)?,
            pack_lineage_id: identity(IndexMeta::PACK_LINEAGE_ID)?,
        })
    }

    /// The identities of a service whose store could not say its own (logged where it
    /// happens): empty strings, never a made-up UUID.
    pub(crate) fn unknown() -> Self {
        Self {
            instance_id: String::new(),
            pack_lineage_id: String::new(),
        }
    }
}

impl WriteReceipt {
    /// The receipt of a write that committed, at `position`: the terminal sequence the store
    /// captured inside the transaction, or the head read after it. A failed read keeps the
    /// receipt committed with the position unknown; the caller logs the error.
    pub(crate) fn committed(identities: &ReceiptIdentities, position: Option<u64>) -> Self {
        Self::at(identities, WriteOutcome::Committed, position)
    }

    /// The receipt of a write that was refused before anything committed: the identities and
    /// no position.
    pub(crate) fn rejected(identities: &ReceiptIdentities) -> Self {
        Self::at(identities, WriteOutcome::Rejected, None)
    }

    /// The same receipt once the barrier gave up on visibility (SS-10): the write stands.
    pub(crate) fn not_yet_searchable(mut self) -> Self {
        self.outcome = WriteOutcome::CommittedNotYetSearchable;
        self
    }

    fn at(identities: &ReceiptIdentities, outcome: WriteOutcome, seq: Option<u64>) -> Self {
        Self {
            outcome,
            instance_id: identities.instance_id.clone(),
            pack_lineage_id: identities.pack_lineage_id.clone(),
            seq,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test code: failures are the assertion
mod tests {
    use super::*;

    fn identities() -> ReceiptIdentities {
        ReceiptIdentities {
            instance_id: "01a13fcc-97b8-747c-b7ee-154cda339108".into(),
            pack_lineage_id: "01a13fcc-97b8-747c-b7ee-154d36136106".into(),
        }
    }

    /// SS-10: a position that could not be read after the commit leaves the outcome
    /// committed with the position unknown; it is never an error and never a rejection.
    #[test]
    fn a_committed_receipt_without_a_readable_position_stays_committed() {
        let receipt = WriteReceipt::committed(&identities(), None);
        assert_eq!(receipt.outcome, WriteOutcome::Committed);
        assert_eq!(receipt.seq, None);
        assert_eq!(receipt.instance_id, identities().instance_id);
        assert_eq!(
            serde_json::to_value(&receipt).unwrap()["seq"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_rejected_receipt_holds_no_position() {
        let receipt = WriteReceipt::rejected(&identities());
        assert_eq!(receipt.outcome, WriteOutcome::Rejected);
        assert_eq!(receipt.seq, None);
    }
}
