//! The code graph's class on the catalog (spec 11 T6 seam, filled by T16): registered with
//! every writer catalog (`super::classes`), so compaction and purge already visit it. Until
//! T16 lands it stages nothing, and its seal, fold and purge write nothing.

use kaban_core::Result;
use kaban_core::query_budget::QueryBudget;
use rusqlite::Connection;

use super::Catalog;
use super::capture::BatchChanges;
use super::class::{ExtentClass, IndexClass};
use super::commit::Stage;
use super::pin::Pin;
use super::tables::{ExtentRef, SedimentRow};

/// The code graph's class (a no-op until T16).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CodeClass;

impl IndexClass for CodeClass {
    fn classes(&self) -> &[ExtentClass] {
        &[ExtentClass::CodeLayer, ExtentClass::CodeBase]
    }
    /// Stages nothing until T16.
    fn stage(&self, _txn: &mut Stage<'_>, _changes: &BatchChanges) -> Result<()> {
        Ok(())
    }
    /// Seals nothing until T16.
    fn seal(
        &self,
        _pin: &Pin,
        _rows: &[&SedimentRow],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Folds nothing until T16.
    fn fold(
        &self,
        _pin: &Pin,
        _extents: &[ExtentRef],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Purges nothing until T16: it holds no extent.
    fn purge(
        &self,
        _pin: &Pin,
        _extent: &ExtentRef,
        _rids: &[i64],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Rebuilds nothing until T16.
    fn rebuild(&self, _catalog: &Catalog, _rows: &Connection) -> Result<()> {
        Ok(())
    }
}
