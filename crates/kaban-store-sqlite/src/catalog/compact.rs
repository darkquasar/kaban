//! Compaction claims and their publication (spec 11 KER-07, PC-9, 7.4).

use kaban_core::Result;
use rusqlite::Connection;

use super::Catalog;
use super::kbx::Unpublished;
use super::tables::{ExtentId, Generation, SedimentSeq};

/// A compaction's claim on its input extents, recorded in `cat_claim`.
#[derive(Debug)]
pub(crate) struct Claim {
    inputs: Vec<ExtentId>,
}

impl Claim {
    /// The claimed extents.
    pub(crate) fn inputs(&self) -> &[ExtentId] {
        &self.inputs
    }
}

/// One compaction's switch.
#[derive(Debug)]
pub(crate) struct Publication {
    /// The claim the compaction holds.
    pub(crate) claim: Claim,
    /// The extents it wrote.
    pub(crate) added: Vec<Unpublished>,
    /// The extents it replaces.
    pub(crate) retired: Vec<ExtentId>,
    /// The sediment rows it sealed.
    pub(crate) sediment_consumed: Vec<SedimentSeq>,
}

impl Catalog {
    /// Claim `inputs`.
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn claim(&self, conn: &mut Connection, inputs: &[ExtentId]) -> Result<Option<Claim>> {
        let _ = (conn, inputs);
        Ok(None)
    }

    /// Publish `p`.
    ///
    /// # Errors
    /// SQLite errors.
    pub(crate) fn publish(&self, conn: &mut Connection, p: Publication) -> Result<Generation> {
        let _ = (conn, p);
        Ok(Generation::default())
    }
}
