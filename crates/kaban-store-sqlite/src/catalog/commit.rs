//! The one writer entry and the one read primitive (spec 11 KER-07, REC-06): every writer
//! transaction of the store opens in [`Catalog::commit`], every deferred read in
//! [`Catalog::read`].
//!
//! ```text
//!   BEGIN IMMEDIATE => capture reset => writer closure => capture drain (frozen once)
//!       => derived-row hook => every IndexClass::stage (same changes) => COMMIT
//!   any error on the way => ROLLBACK (rows, derived rows and sediment together)
//! ```

use std::rc::Rc;

use kaban_core::{Durability, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::Catalog;
use super::capture::{self, BatchChanges, NO_CHANGES};
use super::class::ExtentClass;
use super::tables::{Generation, SedimentSeq, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// The open writer transaction as a writer closure and the index classes see it.
pub(crate) struct Txn<'c> {
    tx: Transaction<'c>,
    /// The frozen change set: `None` in the writer phase, set once before the hook phase.
    changes: Option<Rc<BatchChanges>>,
    /// The generation this commit publishes, allocated by its first catalog write.
    generation: Option<Generation>,
}

impl Txn<'_> {
    /// The transaction's connection, for the writer's own statements.
    pub(crate) fn conn(&self) -> &Connection {
        &self.tx
    }
    /// Stage one sediment row of `class` in this transaction; the commit publishes the next
    /// generation with it.
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn put_sediment(
        &mut self,
        class: ExtentClass,
        payload: &[u8],
    ) -> Result<SedimentSeq> {
        let generation = self.generation()?;
        let seq = self
            .tx
            .query_row(
                "INSERT INTO cat_sediment(class, generation, payload) VALUES (?1, ?2, ?3)
                 RETURNING seq",
                rusqlite::params![class.as_str(), sql_int(generation.0).sql()?, payload],
                |r| uint(r, 0),
            )
            .sql()?;
        Ok(SedimentSeq(seq))
    }
    /// The commit's frozen change set. In the hook phase (the derived-row hook, every index
    /// class's `stage`) it is complete and immutable; in the writer phase it is empty.
    #[expect(
        dead_code,
        reason = "T13's derived-row hook reads the frozen set through it"
    )]
    pub(crate) fn changes(&self) -> &BatchChanges {
        self.changes.as_deref().unwrap_or(&NO_CHANGES)
    }
    /// The generation this commit publishes: the one after the latest, allocated once.
    fn generation(&mut self) -> Result<Generation> {
        if let Some(g) = self.generation {
            return Ok(g);
        }
        let g = self
            .tx
            .query_row(
                "INSERT INTO cat_generation(generation)
                   SELECT COALESCE(max(generation), 0) + 1 FROM cat_generation
                 RETURNING generation",
                [],
                |r| uint(r, 0),
            )
            .sql()?;
        let g = Generation(g);
        self.generation = Some(g);
        Ok(g)
    }
}

impl Catalog {
    /// Run `stage` in one writer transaction: the writer closure, one capture freeze, the
    /// derived-row hook, then every index class's `stage` over the same changes, then COMMIT
    /// (`synchronous=FULL` for that transaction under [`Durability::Full`], restored after).
    /// Any error rolls rows and sediment back.
    ///
    /// # Errors
    /// The closure's, the hook's, a class's or SQLite's error.
    pub(crate) fn commit<T>(
        &self,
        conn: &mut Connection,
        durability: Durability,
        stage: impl FnOnce(&mut Txn<'_>) -> Result<T>,
    ) -> Result<T> {
        // SQLite refuses a change of the safety level inside a transaction: set around it
        let restore = match durability {
            Durability::Normal => None,
            Durability::Full => {
                let level: i64 = conn
                    .query_row("PRAGMA synchronous", [], |r| r.get(0))
                    .sql()?;
                conn.execute_batch("PRAGMA synchronous = FULL").sql()?;
                Some(level)
            }
        };
        let out = self.commit_in(conn, stage);
        if let Some(level) = restore {
            let restored = conn.pragma_update(None, "synchronous", level).sql();
            // the commit's own error wins; a failed restore of a committed write is reported
            if out.is_ok() {
                restored?;
            }
        }
        out
    }

    fn commit_in<T>(
        &self,
        conn: &mut Connection,
        stage: impl FnOnce(&mut Txn<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .sql()?;
        capture::reset(&tx)?;
        let mut txn = Txn {
            tx,
            changes: None,
            generation: None,
        };
        // the writer phase; an error drops `txn`, which rolls everything back
        let out = stage(&mut txn)?;
        // the hook phase: one freeze, then every consumer over the same immutable set
        let changes = Rc::new(capture::drain(&txn.tx)?);
        txn.changes = Some(Rc::clone(&changes));
        derived_rows(&mut txn, &changes)?;
        for class in self.classes() {
            class.stage(&mut txn, &changes)?;
        }
        txn.tx.commit().sql()?;
        Ok(out)
    }

    /// Run `f` in one deferred (read) transaction on `conn`.
    ///
    /// # Errors
    /// The closure's or SQLite's error.
    #[expect(
        clippy::unused_self,
        reason = "the one read primitive is the catalog's: Step 4 takes its pin lock here"
    )]
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

/// The derived-row hook: rows derived from the commit's changes, written in its transaction
/// before the index classes stage. T5 has none; T13 installs rollup maintenance here.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the hook's shape: T13 writes rollups through the transaction and can fail"
)]
fn derived_rows(txn: &mut Txn<'_>, changes: &BatchChanges) -> Result<()> {
    let _ = (txn, changes);
    Ok(())
}
