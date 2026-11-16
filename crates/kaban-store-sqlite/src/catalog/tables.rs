//! The catalog's row vocabulary (spec 11 KER-01, INV-3): generations, extent references and
//! sediment rows. The `cat_generation`, `cat_extent`, `cat_sediment`, `cat_claim`, `cat_view`
//! and `cat_meta` tables (`reclaim_floor` among the latter) join `schema.sql` with the commit
//! implementation (T5 Step 3); nothing here reads them yet.

use super::class::{ExtentClass, ExtentLocation};

/// A catalog generation: every commit that changes the catalog publishes the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub(crate) struct Generation(pub(crate) u64);

/// The sequence number of one sediment row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub(crate) struct SedimentSeq(pub(crate) u64);

/// One published extent as a pin sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtentRef {
    /// The extent's class.
    pub(crate) class: ExtentClass,
    /// The generation that published it.
    pub(crate) generation: Generation,
    /// Where its bytes live.
    pub(crate) location: ExtentLocation,
}

/// One sediment row: a small committed payload not yet sealed into an extent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SedimentRow {
    /// The row's sequence.
    pub(crate) seq: SedimentSeq,
    /// The class that staged it.
    pub(crate) class: ExtentClass,
    /// The class-defined payload.
    pub(crate) payload: Vec<u8>,
}
