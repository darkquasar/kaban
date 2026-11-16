//! The catalog's row vocabulary (spec 11 KER-01, INV-3): generations, extent references and
//! sediment rows. The `cat_generation`, `cat_extent`, `cat_sediment`, `cat_claim`, `cat_view`
//! and `cat_meta` tables (`reclaim_floor` among the latter) of `schema.sql` hold them; the
//! commit entry writes them and a pin reads them.

use super::class::{ExtentClass, ExtentLocation};

/// Column `idx` of `row` as the unsigned number the catalog stores in an `INTEGER`.
pub(crate) fn uint(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let v: i64 = row.get(idx)?;
    u64::try_from(v).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(idx, v))
}

/// `v` as an SQLite `INTEGER` parameter.
pub(crate) fn sql_int(v: u64) -> rusqlite::Result<i64> {
    i64::try_from(v).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

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
