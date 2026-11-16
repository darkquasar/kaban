//! The phases of one catalog commit and their enforcement (spec 11 T5; midpoint review
//! MAJ-01): after the capture freeze no canonical row may change.
//!
//! ```text
//!   Writer ===> drain (frozen once) ===> Derived ===> Stage ===> Writer ===> COMMIT
//!   any statement      | capture empty     | derived     | sediment and   |
//!                      v                   v tables only v generation only v queue empty,
//!                 BatchChanges        (authorizer)      (authorizer)      generation checked
//! ```
//!
//! Two checks close the hook and staging phases. The writer connection's authorizer refuses
//! a statement those phases prepare unless it reads, or writes a table of the phase
//! ([`DERIVED_TABLES`], [`STAGING_TABLES`]); a savepoint or transaction statement is refused
//! as well. A statement prepared earlier runs without a fresh authorization, so after each
//! phase the capture queue must still be empty: a canonical change it holds came after the
//! freeze. Either breach refuses the commit with [`CommitRefused`] and rolls it back; no
//! second drain hides it.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::{BackendSource, Result, StoreError};
use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

use super::tables::{Generation, SedimentSeq, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// The tables the derived-row hook may write. T5 has no derived rows; T13 adds its rollup
/// tables here.
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
    refused: Mutex<Option<String>>,
}

impl Phases {
    fn get(&self) -> Phase {
        Phase::of(self.phase.load(Ordering::Acquire))
    }
    fn set(&self, phase: Phase) {
        self.phase.store(phase.raw(), Ordering::Release);
    }
    fn take_refused(&self) -> Option<String> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
    /// The authorizer's answer for `ctx` in the current phase.
    fn authorize(&self, ctx: &AuthContext<'_>) -> Authorization {
        let Some(writable) = self.get().writable() else {
            return Authorization::Allow;
        };
        if permitted(ctx, writable) {
            return Authorization::Allow;
        }
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| format!("{:?}", ctx.action));
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
        if let Some(action) = self.0.take_refused() {
            return Err(CommitRefused {
                phase,
                refusal: Refusal::Statement(action),
            }
            .into());
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
