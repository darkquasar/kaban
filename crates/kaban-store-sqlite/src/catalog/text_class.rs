//! The text index's class on the catalog (spec 11 T6 seam, filled by T7): registered with
//! every writer catalog (`super::classes`), so compaction and purge already visit it. Until
//! T7 lands it stages nothing, and its seal, fold and purge write nothing.

use kaban_core::Result;
use kaban_core::query_budget::QueryBudget;
use rusqlite::Connection;

use super::Catalog;
use super::capture::BatchChanges;
use super::class::{ExtentClass, IndexClass};
use super::commit::Stage;
use super::pin::Pin;
use super::tables::{ExtentRef, SedimentRow};

/// The text index's class (a no-op until T7).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TextClass;

impl TextClass {
    /// The class every writer catalog registers (`super::classes`).
    pub(crate) fn new() -> Self {
        Self
    }
}

impl IndexClass for TextClass {
    fn classes(&self) -> &[ExtentClass] {
        &[ExtentClass::TextTier]
    }
    /// Stages nothing until T7.
    fn stage(&self, _txn: &mut Stage<'_>, _changes: &BatchChanges) -> Result<()> {
        Ok(())
    }
    /// Counts pending segments; the plan's 32-segment trigger is the `seal_ops.text` default
    /// (T7 leaves tombstone rows out of the count).
    fn seal_due(&self, rows: &[&SedimentRow], ops: u32) -> Result<bool> {
        Ok(rows.len() >= ops as usize)
    }
    /// Seals nothing until T7.
    fn seal(
        &self,
        _pin: &Pin,
        _rows: &[&SedimentRow],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Folds nothing until T7.
    fn fold(
        &self,
        _pin: &Pin,
        _extents: &[ExtentRef],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Purges nothing until T7: it holds no extent.
    fn purge(
        &self,
        _pin: &Pin,
        _extent: &ExtentRef,
        _rids: &[i64],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Rebuilds nothing until T7.
    fn rebuild(&self, _catalog: &Catalog, _rows: &mut Connection) -> Result<()> {
        Ok(())
    }
}
