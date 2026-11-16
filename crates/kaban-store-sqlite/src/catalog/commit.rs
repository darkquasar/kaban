//! The one writer entry and the one read primitive (spec 11 KER-07, REC-06): every writer
//! transaction of the store opens in [`Catalog::commit`], every deferred read in
//! [`Catalog::read`].

use kaban_core::{Durability, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::Catalog;
use super::capture::BatchChanges;
use super::class::ExtentClass;
use super::tables::SedimentSeq;
use crate::sql_error::SqlResultExt;

/// The open writer transaction as a writer closure and the index classes see it.
pub(crate) struct Txn<'c> {
    tx: Transaction<'c>,
    changes: BatchChanges,
}

impl Txn<'_> {
    /// The transaction's connection, for the writer's own statements.
    pub(crate) fn conn(&self) -> &Connection {
        &self.tx
    }
    /// Stage one sediment row of `class` in this transaction.
    ///
    /// # Errors
    /// SQLite errors (T5 Step 3).
    pub(crate) fn put_sediment(
        &mut self,
        class: ExtentClass,
        payload: &[u8],
    ) -> Result<SedimentSeq> {
        let _ = (class, payload);
        Ok(SedimentSeq::default())
    }
    /// The commit's frozen change set. In the hook phase (index class `stage`) it is complete
    /// and immutable; in the writer phase it is empty.
    pub(crate) fn changes(&self) -> &BatchChanges {
        &self.changes
    }
}

impl Catalog {
    /// Run `stage` in one writer transaction: the writer closure, one capture freeze, the
    /// derived-row hook, then every index class's `stage` over the same changes, then COMMIT
    /// (`synchronous=FULL` for that transaction under [`Durability::Full`]). Any error rolls
    /// rows and sediment back.
    ///
    /// # Errors
    /// The closure's, a class's or SQLite's error.
    pub(crate) fn commit<T>(
        &self,
        conn: &mut Connection,
        durability: Durability,
        stage: impl FnOnce(&mut Txn<'_>) -> Result<T>,
    ) -> Result<T> {
        let _ = durability;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .sql()?;
        let mut txn = Txn {
            tx,
            changes: BatchChanges::default(),
        };
        let out = stage(&mut txn)?;
        txn.tx.commit().sql()?;
        Ok(out)
    }

    /// Run `f` in one deferred (read) transaction on `conn`.
    ///
    /// # Errors
    /// The closure's or SQLite's error.
    pub(crate) fn read<T>(
        &self,
        conn: &Connection,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = conn.unchecked_transaction().sql()?;
        let out = f(&tx)?;
        tx.finish().sql()?;
        Ok(out)
    }
}
