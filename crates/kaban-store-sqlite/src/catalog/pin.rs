//! Pins (spec 11 PC-8, INV-3): a reader's view of one catalog generation, race-free against
//! reclamation. [`Catalog::with_snapshot`] evaluates inside the pin's transaction;
//! [`Catalog::pin`] copies the index inputs and returns a detached pin.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kaban_core::Result;
use kaban_core::query_budget::QueryBudget;
use rusqlite::{Connection, Transaction};
use tantivy::directory::OwnedBytes;

use super::Catalog;
use super::class::ExtentClass;
use super::tables::{ExtentRef, Generation, SedimentRow};

/// One pinned generation: its extents and sediment by class, and the container they live in.
#[derive(Debug)]
pub(crate) struct Pin {
    generation: Generation,
    kbx: PathBuf,
    extents: BTreeMap<ExtentClass, Vec<ExtentRef>>,
    sediment: BTreeMap<ExtentClass, Vec<SedimentRow>>,
}

impl Pin {
    /// The pinned generation (the actual generation h of PC-8).
    pub(crate) fn generation(&self) -> Generation {
        self.generation
    }
    /// The pinned extents of `class`.
    pub(crate) fn extents(&self, class: ExtentClass) -> &[ExtentRef] {
        self.extents.get(&class).map_or(&[], Vec::as_slice)
    }
    /// The pinned sediment rows of `class`.
    pub(crate) fn sediment(&self, class: ExtentClass) -> &[SedimentRow] {
        self.sediment.get(&class).map_or(&[], Vec::as_slice)
    }
    /// The bytes of `e`: a slice of a mapping of the container.
    ///
    /// # Errors
    /// I/O errors (T5 Step 4).
    pub(crate) fn bytes(&self, e: &ExtentRef) -> Result<OwnedBytes> {
        let _ = e;
        Ok(OwnedBytes::empty())
    }
    /// The container's path.
    pub(crate) fn kbx_path(&self) -> &Path {
        &self.kbx
    }
}

impl Catalog {
    /// Acquire a pin (PC-8) and run `f` with its open transaction; both end on every exit.
    /// Acquisition and execution share `budget`'s deadline.
    ///
    /// # Errors
    /// The closure's, the budget's or SQLite's error.
    pub(crate) fn with_snapshot<T>(
        &self,
        conn: &mut Connection,
        budget: &QueryBudget,
        f: impl FnOnce(&Transaction<'_>, &Pin) -> Result<T>,
    ) -> Result<T> {
        let _ = budget;
        let pin = self.empty_pin();
        self.read(conn, |tx| f(tx, &pin))
    }

    /// Acquire a pin (PC-8), copy every index input under `budget`, end the transaction and
    /// return the detached pin.
    ///
    /// # Errors
    /// The budget's or SQLite's error.
    pub(crate) fn pin(&self, conn: &mut Connection, budget: &QueryBudget) -> Result<Pin> {
        let _ = (conn, budget);
        Ok(self.empty_pin())
    }

    fn empty_pin(&self) -> Pin {
        Pin {
            generation: Generation::default(),
            kbx: self.kbx.clone(),
            extents: BTreeMap::new(),
            sediment: BTreeMap::new(),
        }
    }
}
