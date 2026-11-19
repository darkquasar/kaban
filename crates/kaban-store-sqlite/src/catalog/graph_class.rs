//! The memory graph's class on the catalog (spec 11 T6 seam, filled by T11): registered with
//! every writer catalog (`super::classes`), so compaction and purge already visit it. Until
//! T11 lands it stages nothing, and its seal, fold and purge write nothing.

use kaban_core::Result;
use kaban_core::query_budget::QueryBudget;
use rusqlite::Connection;

use super::Catalog;
use super::capture::BatchChanges;
use super::class::{ExtentClass, IndexClass};
use super::commit::Stage;
use super::pin::Pin;
use super::tables::{ExtentRef, SedimentRow};

/// The memory graph's class (a no-op until T11).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GraphClass;

impl GraphClass {
    /// The class every writer catalog registers (`super::classes`).
    pub(crate) fn new() -> Self {
        Self
    }
}

impl IndexClass for GraphClass {
    fn classes(&self) -> &[ExtentClass] {
        &[ExtentClass::GraphLayer, ExtentClass::GraphBase]
    }
    /// Stages nothing until T11.
    fn stage(&self, _txn: &mut Stage<'_>, _changes: &BatchChanges) -> Result<()> {
        Ok(())
    }
    /// One row counts one operation until T11 and T16 count the operations inside the rows
    /// (one commit is one row).
    fn seal_due(&self, rows: &[&SedimentRow], ops: u32) -> Result<bool> {
        Ok(rows.len() >= ops as usize)
    }
    /// Seals nothing until T11.
    fn seal(
        &self,
        _pin: &Pin,
        _rows: &[&SedimentRow],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Folds nothing until T11.
    fn fold(
        &self,
        _pin: &Pin,
        _extents: &[ExtentRef],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Purges nothing until T11: it holds no extent.
    fn purge(
        &self,
        _pin: &Pin,
        _extent: &ExtentRef,
        _rids: &[i64],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Rebuilds nothing until T11.
    fn rebuild(&self, _catalog: &Catalog, _rows: &Connection) -> Result<()> {
        Ok(())
    }
}
