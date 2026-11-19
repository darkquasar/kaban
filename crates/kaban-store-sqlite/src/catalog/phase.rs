//! The phases of one catalog commit and their enforcement (spec 11 T5; midpoint review
//! MAJ-01, decision review C15): after the capture freeze no canonical row may change.
//!
//! ```text
//!   Writer ===> drain (frozen once) ===> Derived ===> Stage ===> Writer ===> COMMIT
//!   any statement      | capture empty     | derived     | sediment and   |
//!                      v                   v tables only v generation only v queue empty,
//!                 BatchChanges        (authorizer)      (authorizer)      generation checked
//! ```
//!
//! | phase   | may read                    | may write                                 |
//! |---------|-----------------------------|-------------------------------------------|
//! | Writer  | anything                    | anything (canonical rows are captured)    |
//! | Derived | the transaction, [`Reader`] | [`DERIVED_TABLES`], `Derived::execute`    |
//! | Stage   | the transaction, [`Reader`] | [`STAGING_TABLES`], `Stage::put_sediment` |
//!
//! The restricted phases (Derived, Stage) reach the connection only through their handles
//! (`commit::Derived`, `commit::Stage`), and every statement a handle runs is prepared in
//! its phase, never taken from the connection's statement cache: SQLite authorizes a
//! statement when it is prepared, so a statement the writer phase cached would otherwise run
//! unjudged. The writer phase and everything outside a commit cache freely; nothing is
//! flushed at a phase change.
//!
//! Three checks close the hook and staging phases. The writer connection's authorizer refuses
//! a statement those phases prepare unless it reads, or writes a table of the phase; a
//! savepoint or transaction statement is refused as well. A [`Reader`] statement must also be
//! read-only. After each phase the capture queue must still be empty: a canonical change it
//! holds came after the freeze (a test playing a broken handle with a raw connection is
//! still caught there). Any breach refuses the commit with [`CommitRefused`] and rolls it
//! back, even if the handle's caller swallowed the error; no second drain hides it.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::{BackendSource, Result, StoreError};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, OptionalExtension, Params, Row, Statement};

use super::tables::{Generation, SedimentSeq, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// The tables the derived-row hook may write, by exact name (never a prefix: a listed name
/// admits that one table of `main`). T5 has no derived rows: the list is empty, so the hook
/// may only read. T13 adds exactly its rollup table (`rollup`) here, and nothing else: the
/// declared-column tables (`col_<label>`) are canonical rows, written by the writer phase and
/// captured (T13 extends the capture with their before/after values), never by the hook. The
/// hook reads through `Derived::reader` and writes through `Derived::execute`.
pub(crate) const DERIVED_TABLES: &[&str] = &[];

/// The tables an index class's staging may insert into.
pub(crate) const STAGING_TABLES: &[&str] = &["cat_sediment", "cat_generation"];

/// The phase of the writer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The writer closure of an open commit, and every statement outside a commit.
    Writer,
    /// The derived-row hook, after the freeze.
    Derived,
    /// The index classes' staging, after the hook.
    Stage,
}

impl Phase {
    fn of(raw: u8) -> Self {
        match raw {
            1 => Self::Derived,
            2 => Self::Stage,
            _ => Self::Writer,
        }
    }
    fn raw(self) -> u8 {
        match self {
            Self::Writer => 0,
            Self::Derived => 1,
            Self::Stage => 2,
        }
    }
    /// The tables a statement of this phase may write, `None` for any.
    fn writable(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Writer => None,
            Self::Derived => Some(DERIVED_TABLES),
            Self::Stage => Some(STAGING_TABLES),
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Writer => "writer",
            Self::Derived => "derived-row hook",
            Self::Stage => "staging",
        })
    }
}

/// Why a commit was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The authorizer refused a statement of the phase (the action as SQLite named it).
    Statement(String),
    /// A statement sent through the phase's [`Reader`] is not read-only (its SQL).
    ReaderWrite(String),
    /// The capture queue held a canonical change made after the freeze.
    CanonicalChange,
    /// The staged sediment does not name the generation the commit publishes.
    StrayGeneration {
        /// The generation the staging allocated.
        staged: Generation,
        /// The latest generation in the transaction.
        latest: Generation,
    },
}

/// A commit refused for breaking its phase contract: rolled back, nothing of it committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitRefused {
    /// The phase that broke the contract.
    pub(crate) phase: Phase,
    /// What it did.
    pub(crate) refusal: Refusal,
}

impl fmt::Display for CommitRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = self.phase;
        match &self.refusal {
            Refusal::Statement(action) => write!(
                f,
                "catalog_commit_refused: the {phase} phase may not run {action}"
            ),
            Refusal::ReaderWrite(sql) => write!(
                f,
                "catalog_commit_refused: the {phase} phase sent a write through its reader: {sql}"
            ),
            Refusal::CanonicalChange => write!(
                f,
                "catalog_commit_refused: the {phase} phase changed a canonical row after the freeze"
            ),
            Refusal::StrayGeneration { staged, latest } => write!(
                f,
                "catalog_commit_refused: the {phase} phase staged generation {} but the latest is {}",
                staged.0, latest.0
            ),
        }
    }
}

impl std::error::Error for CommitRefused {}

impl From<CommitRefused> for StoreError {
    fn from(e: CommitRefused) -> Self {
        Self::Backend {
            source: BackendSource::new(e),
        }
    }
}

/// The writer connection's phase as its authorizer reads it, and the first statement the
/// authorizer refused in the current phase.
#[derive(Debug, Default)]
pub(crate) struct Phases {
    phase: AtomicU8,
    refused: Mutex<Option<Refusal>>,
}

impl Phases {
    fn get(&self) -> Phase {
        Phase::of(self.phase.load(Ordering::Acquire))
    }
    fn set(&self, phase: Phase) {
        self.phase.store(phase.raw(), Ordering::Release);
    }
    fn take_refused(&self) -> Option<Refusal> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
    /// Record `refusal` as the current phase's breach (the first one wins) and return it as
    /// the commit's refusal: the phase closes refused even if its caller swallows the error.
    fn refuse(&self, refusal: Refusal) -> CommitRefused {
        let phase = self.get();
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| refusal.clone());
        CommitRefused { phase, refusal }
    }
    /// The authorizer's answer for `ctx` in the current phase.
    fn authorize(&self, ctx: &AuthContext<'_>) -> Authorization {
        let Some(writable) = self.get().writable() else {
            return Authorization::Allow;
        };
        if permitted(ctx, writable) {
            return Authorization::Allow;
        }
        let _ = self.refuse(Refusal::Statement(format!("{:?}", ctx.action)));
        Authorization::Deny
    }
    /// Enter the writer phase of a commit; the phase returns to `Writer` however it ends.
    pub(crate) fn enter(&self) -> PhaseGuard<'_> {
        self.set(Phase::Writer);
        let _ = self.take_refused();
        PhaseGuard(self)
    }
}

/// Whether a statement of a phase that may write `writable` (in `main`) may run `ctx`: reads,
/// and inserts, updates and deletes of those tables.
fn permitted(ctx: &AuthContext<'_>, writable: &[&str]) -> bool {
    let write_of = |table: &str| ctx.database_name == Some("main") && writable.contains(&table);
    match ctx.action {
        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => true,
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. } => write_of(table_name),
        _ => false,
    }
}

/// Install the phase authorizer on a writer connection.
///
/// # Errors
/// SQLite errors registering it.
pub(crate) fn guard_phases(conn: &mut Connection, phases: &Arc<Phases>) -> Result<()> {
    let phases = Arc::clone(phases);
    conn.authorizer(Some(move |ctx: AuthContext<'_>| phases.authorize(&ctx)))
        .sql()
}

/// The phase of one open commit; dropping it returns the connection to the writer phase, so
/// the rollback of an early exit or an unwind runs unrestricted.
pub(crate) struct PhaseGuard<'p>(&'p Phases);

impl PhaseGuard<'_> {
    /// Enter `phase`.
    pub(crate) fn set(&self, phase: Phase) {
        self.0.set(phase);
    }
    /// Close the current phase over its outcome `out` on `conn`: a statement the authorizer
    /// refused, or a canonical change in the capture queue, refuses the commit.
    ///
    /// # Errors
    /// [`CommitRefused`], else `out`'s error, else SQLite's.
    pub(crate) fn close(&self, conn: &Connection, out: Result<()>) -> Result<()> {
        let phase = self.0.get();
        if let Some(refusal) = self.0.take_refused() {
            return Err(CommitRefused { phase, refusal }.into());
        }
        out?;
        let changed: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM temp.cat_capture)", [], |r| {
                r.get(0)
            })
            .sql()?;
        if changed {
            return Err(CommitRefused {
                phase,
                refusal: Refusal::CanonicalChange,
            }
            .into());
        }
        Ok(())
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.0.set(Phase::Writer);
        let _ = self.0.take_refused();
    }
}

/// A restricted phase's reads of the commit's own transaction (decision review C15): the
/// canonical rows as the writer closure left them, through read-only statements. What a
/// class or the derived-row hook reads of a changed row (its text, an edge's confidence and
/// time) is read here, never through another connection, which would see the previous commit.
///
/// Every statement is prepared in the phase that runs it, never taken from the connection's
/// statement cache, so the authorizer judges it in that phase; one that is not read-only is
/// refused before it runs (`sqlite3_stmt_readonly`) and refuses the commit. Transaction and
/// savepoint statements count as read-only there; the authorizer refuses them first.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'t> {
    conn: &'t Connection,
    phases: &'t Phases,
}

impl<'t> Reader<'t> {
    /// The reader of the transaction on `conn` in the phase `phases` holds.
    pub(super) fn new(conn: &'t Connection, phases: &'t Phases) -> Self {
        Self { conn, phases }
    }

    /// Prepare the read-only statement `sql` in this phase (a fresh preparation: never one
    /// the statement cache holds).
    ///
    /// # Errors
    /// [`CommitRefused`] for a statement that is not read-only (recorded: the commit is
    /// refused even if the caller goes on); SQLite's, the authorizer's refusal among them.
    pub(crate) fn prepare(&self, sql: &str) -> Result<ReadStatement<'t>> {
        let stmt = self.conn.prepare(sql).sql()?;
        if !stmt.readonly() {
            return Err(self
                .phases
                .refuse(Refusal::ReaderWrite(sql.to_owned()))
                .into());
        }
        Ok(ReadStatement(stmt))
    }
}

/// One read-only statement of a [`Reader`], run any number of times in its phase.
pub(crate) struct ReadStatement<'t>(Statement<'t>);

impl ReadStatement<'_> {
    /// The first row for `params` through `f`, `None` when there is none.
    ///
    /// # Errors
    /// SQLite's, `f`'s.
    pub(crate) fn query_row<T, P: Params>(
        &mut self,
        params: P,
        f: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        self.0.query_row(params, f).optional().sql()
    }

    /// Every row for `params` through `f`, in order.
    ///
    /// # Errors
    /// SQLite's, `f`'s.
    pub(crate) fn query_map<T, P: Params>(
        &mut self,
        params: P,
        f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        self.0
            .query_map(params, f)
            .sql()?
            .collect::<rusqlite::Result<_>>()
            .sql()
    }
}

/// What the staging phase allocated: the generation the commit publishes and the first
/// sediment row it staged.
#[derive(Debug, Default)]
pub(crate) struct Staged {
    pub(crate) generation: Option<Generation>,
    pub(crate) first: Option<SedimentSeq>,
}

impl Staged {
    /// Check, before COMMIT, that the staged generation is the transaction's latest and that
    /// every sediment row the staging wrote names it (MED-01: a generation rolled back under
    /// the staging must not leave sediment behind).
    ///
    /// # Errors
    /// [`CommitRefused`] or SQLite errors.
    pub(crate) fn check(&self, conn: &Connection) -> Result<()> {
        let Some(staged) = self.generation else {
            return Ok(());
        };
        let first = self.first.map_or(Ok(i64::MAX), |s| sql_int(s.0)).sql()?;
        let (latest, stray) = conn
            .query_row(
                "SELECT (SELECT COALESCE(max(generation), 0) FROM cat_generation),
                        EXISTS(SELECT 1 FROM cat_sediment WHERE seq >= ?1 AND generation <> ?2)",
                rusqlite::params![first, sql_int(staged.0).sql()?],
                |r| Ok((uint(r, 0)?, r.get::<_, bool>(1)?)),
            )
            .sql()?;
        let latest = Generation(latest);
        if latest != staged || stray {
            return Err(CommitRefused {
                phase: Phase::Stage,
                refusal: Refusal::StrayGeneration { staged, latest },
            }
            .into());
        }
        Ok(())
    }
}
