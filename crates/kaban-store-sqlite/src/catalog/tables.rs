//! The catalog's row vocabulary (spec 11 KER-01, INV-3): generations, extent references and
//! sediment rows. The `cat_generation`, `cat_extent`, `cat_sediment`, `cat_claim`, `cat_view`,
//! `cat_view_extent` (each view's ordered chain) and `cat_meta` tables (`reclaim_floor` among
//! the latter) of `schema.sql` hold them; the commit entry writes them and a pin reads them.

use std::fs::File;
use std::os::unix::fs::FileExt as _;

use kaban_core::Result;
use kaban_core::query_budget::QueryBudget;
use sha2::{Digest as _, Sha256};

use super::class::{ExtentClass, ExtentLocation};
use crate::sql_error::SqlResultExt;
use rusqlite::Connection;

/// Close gaps after class retirement without changing the order of surviving extents.
pub(super) fn renumber_chains(conn: &Connection, budget: &QueryBudget) -> Result<()> {
    budget.check()?;
    // Move the existing nonnegative positions out of the destination range first. The
    // materialized window retains their original order while assigning contiguous slots;
    // no uncharged Rust buffer proportional to every view is copied here.
    conn.execute("UPDATE cat_view_extent SET position = -position - 1", [])
        .sql()?;
    budget.check()?;
    conn.execute(
        "WITH positions AS MATERIALIZED (
            SELECT view, extent_id,
                   row_number() OVER (PARTITION BY view ORDER BY position DESC) - 1 AS slot
            FROM cat_view_extent
         )
         UPDATE cat_view_extent SET position = (
             SELECT slot FROM positions p
             WHERE p.view = cat_view_extent.view AND p.extent_id = cat_view_extent.extent_id
         )",
        [],
    )
    .sql()?;
    budget.check()?;
    Ok(())
}

/// Whether `file` holds `len` bytes at `offset` whose SHA-256 is `sha`, `budget` checked per
/// chunk: a rebuild's judgement of an old extent before it keeps its range.
///
/// # Errors
/// The budget's deadline or cancellation.
pub(crate) fn holds(
    file: &File,
    offset: u64,
    len: u64,
    sha: &[u8],
    budget: &QueryBudget,
) -> Result<bool> {
    const CHUNK: u64 = 1 << 16;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1 << 16];
    let mut done = 0_u64;
    while done < len {
        budget.check()?;
        let n = (len - done).min(CHUNK);
        let Some(chunk) = usize::try_from(n).ok().and_then(|n| buf.get_mut(..n)) else {
            return Ok(false);
        };
        if file.read_exact_at(chunk, offset + done).is_err() {
            return Ok(false);
        }
        hasher.update(&*chunk);
        done += n;
    }
    Ok(hasher.finalize().as_slice() == sha)
}

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

/// An extent's stable identity (`cat_extent.id`, never reused): what a compaction claims
/// and retires (T6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExtentId(pub(crate) u64);

/// One published extent as a pin sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtentRef {
    /// The extent's identity.
    pub(crate) id: ExtentId,
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
