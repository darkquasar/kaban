//! The one writer entry and the one read primitive (spec 11 KER-07, REC-06): every writer
//! transaction of the store opens in [`Catalog::commit`], every deferred read in
//! [`Catalog::read`].
//!
//! ```text
//!   BEGIN IMMEDIATE => writer closure [Txn] => capture drain (frozen once)
//!       => derived-row hook [Derived] => every IndexClass::stage [Stage] (same changes)
//!       => generation check => COMMIT
//!   any error or phase breach on the way => ROLLBACK (rows, derived rows, sediment and the
//!   capture queue)
//! ```
//!
//! Each phase has its own handle and reaches only what it may change (`super::phase`): the
//! writer closure any row, the derived-row hook the derived tables, a class's staging its
//! sediment. A breach refuses the commit by name (`CommitRefused`).
//!
//! No row write of the store commits elsewhere on the writer connection: the open-time schema
//! and seed transactions run before the catalog attaches, and the one maintenance statement
//! that cannot run in a transaction (`VACUUM`) runs in [`Catalog::outside_commit`]. Test builds
//! (`cfg(test)`, `failpoints`) enforce it with a commit hook ([`guard_writer`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kaban_core::{Durability, Result};
use rusqlite::{Connection, Params, Transaction, TransactionBehavior};

use super::Catalog;
use super::capture::{self, BatchChanges};
use super::class::{ExtentClass, ExtentLocation};
use super::kbx::Unpublished;
use super::phase::{Phase, Phases, Reader, Staged};
use super::tables::{ExtentId, Generation, SedimentSeq, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// The writer phase's handle on the open writer transaction: the writer closure's statements.
/// It holds no change set (the set is frozen only after the closure) and stages nothing.
pub(crate) struct Txn<'c> {
    tx: Transaction<'c>,
    /// The generation this commit publishes, once the writer phase allocated it.
    generation: Option<Generation>,
}

impl Txn<'_> {
    /// The transaction's connection, for the writer's own statements.
    pub(crate) fn conn(&self) -> &Connection {
        &self.tx
    }

    /// Publish `extent` in this commit: its catalog row names the generation the commit
    /// publishes, so a pin sees it once the commit is visible and never before (INV-3).
    ///
    /// # Errors
    /// SQLite errors.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "publishing consumes the unpublished extent: it cannot be published twice"
    )]
    pub(crate) fn publish(&mut self, extent: Unpublished) -> Result<ExtentId> {
        let generation = sql_int(self.generation()?.0).sql()?;
        let (offset, len, relpath) = match &extent.location {
            ExtentLocation::Kbx { offset, len } => (
                Some(sql_int(*offset).sql()?),
                Some(sql_int(*len).sql()?),
                None,
            ),
            ExtentLocation::File { relpath } => (None, None, Some(relpath.as_str())),
        };
        let id = self
            .tx
            .query_row(
                "INSERT INTO cat_extent(class, generation, kbx_offset, kbx_len, relpath)
                 VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
                rusqlite::params![extent.class.as_str(), generation, offset, len, relpath],
                |r| uint(r, 0),
            )
            .sql()?;
        Ok(ExtentId(id))
    }

    /// The generation this commit publishes: the one after the latest, allocated once and
    /// shared with the staging phase (one commit publishes one generation).
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn generation(&mut self) -> Result<Generation> {
        if let Some(g) = self.generation {
            return Ok(g);
        }
        let g = next_generation(&self.tx)?;
        self.generation = Some(g);
        Ok(g)
    }
}

/// Allocate the generation after the latest in the open transaction on `conn`.
fn next_generation(conn: &Connection) -> Result<Generation> {
    conn.query_row(
        "INSERT INTO cat_generation(generation)
           SELECT COALESCE(max(generation), 0) + 1 FROM cat_generation
         RETURNING generation",
        [],
        |r| uint(r, 0),
    )
    .sql()
    .map(Generation)
}

/// The derived-row hook's handle, after the change set is frozen: reads of the commit's rows
/// ([`Self::reader`]) and statements over the derived tables ([`Self::execute`], T13's
/// rollups). No raw connection: every statement is prepared in the hook phase, so a statement
/// another phase prepared (and cached) never runs here unjudged.
pub(crate) struct Derived<'t> {
    conn: &'t Connection,
    phases: &'t Phases,
}

impl<'t> Derived<'t> {
    /// The hook's reads of this commit's rows.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "T13's rollup maintenance reads through it")
    )]
    pub(crate) fn reader(&self) -> Reader<'t> {
        Reader::new(self.conn, self.phases)
    }
    /// Run the statement `sql` over the derived tables ([`super::phase::DERIVED_TABLES`]): a
    /// fresh preparation in the hook phase (never one the statement cache holds), so the
    /// authorizer refuses a write of any other table, and the commit with it.
    ///
    /// # Errors
    /// SQLite's, the authorizer's refusal among them.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "T13's rollup maintenance writes through it")
    )]
    pub(crate) fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        let mut stmt = self.conn.prepare(sql).sql()?;
        stmt.execute(params).sql()
    }
    /// The connection itself, for a unit test that plays a hook breaking the phase contract
    /// with a raw (and cached) statement: never reachable from production code.
    #[cfg(test)]
    pub(crate) fn conn_for_test(&self) -> &Connection {
        self.conn
    }
}

/// An index class's handle in the staging phase: it reads the commit's rows ([`Self::reader`])
/// and stages sediment ([`Self::put_sediment`]), nothing else. No raw connection, as for
/// [`Derived`].
pub(crate) struct Stage<'t> {
    conn: &'t Connection,
    phases: &'t Phases,
    staged: &'t mut Staged,
}

impl<'t> Stage<'t> {
    /// The class's reads of this commit's rows.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "T7's and T11's classes read through it")
    )]
    pub(crate) fn reader(&self) -> Reader<'t> {
        Reader::new(self.conn, self.phases)
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
            .conn
            .query_row(
                "INSERT INTO cat_sediment(class, generation, payload) VALUES (?1, ?2, ?3)
                 RETURNING seq",
                rusqlite::params![class.as_str(), sql_int(generation.0).sql()?, payload],
                |r| uint(r, 0),
            )
            .sql()?;
        let seq = SedimentSeq(seq);
        self.staged.first.get_or_insert(seq);
        Ok(seq)
    }
    /// The connection itself, for a unit test that plays a class breaking the staging
    /// contract: never reachable from a production class.
    #[cfg(test)]
    pub(crate) fn conn_for_test(&self) -> &Connection {
        self.conn
    }
    /// The generation this commit publishes: the one after the latest, allocated once.
    fn generation(&mut self) -> Result<Generation> {
        if let Some(g) = self.staged.generation {
            return Ok(g);
        }
        let g = next_generation(self.conn)?;
        self.staged.generation = Some(g);
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
        // SQLite refuses a change of the safety level inside a transaction: set around it, and
        // restored however the commit ends (an unwound writer included)
        let mut level = SafetyLevel::raise(conn, durability)?;
        let out = self.commit_in(level.conn(), stage);
        let restored = level.restore();
        // the commit's own error wins; a failed restore of a committed write is reported
        let out = out?;
        restored?;
        Ok(out)
    }

    fn commit_in<T>(
        &self,
        conn: &mut Connection,
        stage: impl FnOnce(&mut Txn<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .sql()?;
        let mut txn = Txn {
            tx,
            generation: None,
        };
        // declared after `txn`, so it drops first: an early exit's rollback runs unrestricted
        let phase = self.phases.enter();
        // the writer phase; an error drops `txn`, which rolls everything back
        let out = stage(&mut txn)?;
        // the hook phase: one freeze, then every consumer over the same immutable set; each
        // phase closes with the queue still empty (nothing canonical changed after the freeze)
        let changes = capture::drain(&txn.tx)?;
        phase.set(Phase::Derived);
        let derived = self.derived_rows(
            &Derived {
                conn: &txn.tx,
                phases: &self.phases,
            },
            &changes,
        );
        phase.close(&txn.tx, derived)?;
        phase.set(Phase::Stage);
        // the writer phase's publication (if any) fixed the generation the staging names
        let mut staged = Staged {
            generation: txn.generation,
            first: None,
        };
        for class in self.classes() {
            let mut handle = Stage {
                conn: &txn.tx,
                phases: &self.phases,
                staged: &mut staged,
            };
            let staging = class.stage(&mut handle, &changes);
            phase.close(&txn.tx, staging)?;
        }
        staged.check(&txn.tx)?;
        drop(phase);
        let admitted = Admitted::enter(&self.committing);
        let committed = txn.tx.commit().sql();
        drop(admitted);
        committed?;
        Ok(out)
    }

    /// Run `f`, a statement on the writer connection that commits on its own and cannot run
    /// in a transaction (`VACUUM`), as an admitted exception to [`Self::commit`]. It writes no
    /// row: nothing is captured or staged.
    pub(crate) fn outside_commit<T>(&self, f: impl FnOnce() -> T) -> T {
        self.outside_commits().run(f)
    }

    /// The admission of [`Self::outside_commit`] as a handle for a thread that commits on the
    /// writer connection without the catalog: the serving projector's cursor and lease
    /// transactions (`crate::projector_thread`), removed by T7.
    pub(crate) fn outside_commits(&self) -> OutsideCommit {
        OutsideCommit(Arc::clone(&self.committing))
    }

    /// Run `f` in one deferred (read) transaction on `conn`.
    ///
    /// # Errors
    /// The closure's or SQLite's error.
    #[expect(
        clippy::unused_self,
        reason = "the one read primitive is the catalog's: its pins and snapshots build on it"
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

/// See [`Catalog::outside_commits`].
#[derive(Debug, Clone)]
pub(crate) struct OutsideCommit(Arc<AtomicBool>);

impl OutsideCommit {
    /// Run `f`, whose commits on the writer connection the writer guard admits. The caller
    /// holds the writer connection's lock, so no catalog commit runs meanwhile.
    pub(crate) fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let _admitted = Admitted::enter(&self.0);
        f()
    }
}

/// The writer guard's admission of one commit, withdrawn however the commit ends (an unwind
/// included).
struct Admitted<'a>(&'a AtomicBool);

impl<'a> Admitted<'a> {
    fn enter(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag)
    }
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The safety level of the writer connection around one commit: raised to FULL for a
/// [`Durability::Full`] commit and restored once it ends, an unwind included (SQLite refuses
/// the change inside a transaction).
struct SafetyLevel<'c> {
    conn: &'c mut Connection,
    restore: Option<i64>,
}

impl<'c> SafetyLevel<'c> {
    fn raise(conn: &'c mut Connection, durability: Durability) -> Result<Self> {
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
        Ok(Self { conn, restore })
    }
    fn conn(&mut self) -> &mut Connection {
        self.conn
    }
    /// Restore the level the commit found (once).
    fn restore(&mut self) -> Result<()> {
        match self.restore.take() {
            Some(level) => self.conn.pragma_update(None, "synchronous", level).sql(),
            None => Ok(()),
        }
    }
}

impl Drop for SafetyLevel<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// The writer guard of test builds: a commit hook on the writer connection that refuses
/// (rolls back) every commit outside [`Catalog::commit`] and [`Catalog::outside_commit`], so an
/// autocommit row write or a transaction the catalog did not open fails where it runs. It
/// sees this connection only: another connection's writes (the projector's own, a test's raw
/// connection) and the open-time writes before the catalog attaches are not its.
///
/// # Errors
/// SQLite errors registering the hook.
#[cfg(any(test, feature = "failpoints"))]
pub(super) fn guard_writer(conn: &mut Connection, committing: &Arc<AtomicBool>) -> Result<()> {
    let committing = Arc::clone(committing);
    conn.commit_hook(Some(move || !committing.load(Ordering::Acquire)))
        .sql()
}

/// The writer guard is a test-build check (see the test-build [`guard_writer`]).
#[cfg(not(any(test, feature = "failpoints")))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the test build's twin registers a hook and can fail"
)]
pub(super) fn guard_writer(conn: &mut Connection, committing: &Arc<AtomicBool>) -> Result<()> {
    let _ = (conn, committing);
    Ok(())
}

impl Catalog {
    /// The derived-row hook: rows derived from the commit's changes, written in its transaction
    /// before the index classes stage. T5 has none; T13 installs rollup maintenance here. A
    /// unit test installs its own ([`Self::set_test_derived_rows`]).
    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "the hook's shape: T13 writes rollups through `derived` and can fail"
        )
    )]
    fn derived_rows(&self, derived: &Derived<'_>, changes: &BatchChanges) -> Result<()> {
        #[cfg(test)]
        if let Some(hook) = self
            .test_derived
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            return hook(derived, changes);
        }
        let _ = (derived, changes);
        Ok(())
    }

    /// Install a unit test's derived-row hook: every later commit runs it in the hook phase.
    #[cfg(test)]
    pub(crate) fn set_test_derived_rows(&self, hook: TestDerivedRows) {
        *self
            .test_derived
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }
}

/// A unit test's derived-row hook ([`Catalog::set_test_derived_rows`]).
#[cfg(test)]
pub(crate) type TestDerivedRows = Box<dyn Fn(&Derived<'_>, &BatchChanges) -> Result<()> + Send>;
