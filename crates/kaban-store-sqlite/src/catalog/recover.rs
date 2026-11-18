//! Recovery and verification of the catalog (spec 11 KER-02, KER-07, PC-9, 7.4, 7.8).
//!
//! ```text
//!   recover: maintenance lock EX (bounded wait)?
//!              no  => maintenance_live: clear nothing
//!              yes => delete every claim => [recover.mid] => punch every unpublished range
//!   verify:  pin => maintenance lock EX probe (held through the read)
//!            => one read transaction: classes, chains, claims, named ranges
//!            => unnamed ranges that hold data => missing cold files => hashes of live extents
//! ```
//!
//! The stale-claim rule: a claim is stale when no maintenance hold is live. Every compaction
//! pass and every unpublished extent holds the maintenance lock shared (`pins::MAINTENANCE`)
//! until its claim is published or released and its range is named, and a holder's lock dies
//! with its process. So once recovery holds the lock exclusive, every claim and every range no
//! row names was left by a process that is gone, and while it cannot take it, recovery clears
//! nothing (live maintenance). An unpublished range is a container range no catalog row names
//! that still holds a nonzero byte: a range a reclamation released reads as zeros (punched or
//! zero-filled). Recovery changes no canonical row and no live extent: every row is present or
//! absent as the last commit left it.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Duration;

use kaban_core::query_budget::QueryBudget;
use kaban_core::{Durability, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::Catalog;
use super::class::ExtentLocation;
use super::fail;
use super::failpoint::Window;
use super::kbx::{self, failed};
use super::pins;
use super::reclaim::coalesce;
use super::tables::uint;
use crate::sql_error::SqlResultExt;

/// What one recovery did ([`Catalog::recover`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverReport {
    /// A maintenance hold was live (a compaction, purge or unpublished extent of a running
    /// process): recovery cleared no claim and reclaimed no range.
    pub maintenance_live: bool,
    /// Stale claim rows it removed.
    pub claims_cleared: u64,
    /// The unpublished container ranges (`(offset, len)`) it reclaimed.
    pub ranges_reclaimed: Vec<(u64, u64)>,
}

/// The extents of one class in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassExtents {
    /// The class's catalog name.
    pub class: String,
    /// Live extents.
    pub live: u64,
    /// Retired extents not yet reclaimed.
    pub dead: u64,
    /// Pending (not yet sealed) sediment rows of the class.
    pub sediment_rows: u64,
    /// The newest generation that published a live extent of the class (`None`: no live
    /// extent).
    pub newest_generation: Option<u64>,
}

/// What one verification found ([`Catalog::verify`]). It names its checks: they are
/// structural and byte-level (rows, chains, ranges, hashes), not the semantic integrity of an
/// index's contents (7.8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Every class with an extent or a pending sediment row, in name order.
    pub classes: Vec<ClassExtents>,
    /// Container ranges (`(offset, len)`) no catalog row names that still hold data: appended
    /// and never published (a leak recovery reclaims).
    pub unreferenced_ranges: Vec<(u64, u64)>,
    /// Cold `File` locations whose file is missing, by relative path.
    pub missing_files: Vec<String>,
    /// Views in `needs_reimport` (a state, not an inconsistency).
    pub needs_reimport: Vec<String>,
    /// Chain inconsistencies, one line each: a view's positions not contiguous from 0, a
    /// retired extent still in a chain, a live extent in no chain.
    pub chain_inconsistencies: Vec<String>,
    /// Live extents whose bytes do not hash to their `content_sha256`.
    pub hash_mismatches: Vec<u64>,
    /// Live extents without a `content_sha256` (reported, not failed).
    pub unhashed: Vec<u64>,
    /// Extents claimed while no maintenance hold is live: claims of a process that is gone.
    pub stale_claims: Vec<u64>,
    /// A maintenance hold was live: unreferenced ranges and claims may be work in flight and
    /// are not counted.
    pub maintenance_live: bool,
}

impl VerifyReport {
    /// How many inconsistencies it found (0: the catalog is consistent).
    #[must_use]
    pub fn inconsistencies(&self) -> usize {
        let in_flight = if self.maintenance_live {
            0
        } else {
            self.unreferenced_ranges.len() + self.stale_claims.len()
        };
        in_flight
            + self.missing_files.len()
            + self.chain_inconsistencies.len()
            + self.hash_mismatches.len()
    }
}

/// How long recovery waits for live maintenance to end before it reports it and clears nothing.
const RECOVER_WAIT: Duration = Duration::from_secs(1);

/// How many container bytes one read of a range check or a hash takes.
const READ_CHUNK: usize = 64 * 1024;

impl Catalog {
    /// Recover after a crash, leaving every row present or absent (the module's rule): with
    /// the maintenance lock held exclusive, clear every claim and reclaim every unpublished
    /// range (one no row names that still holds data). While a maintenance hold is live it
    /// clears nothing and reports `maintenance_live`. Interrupted reclamations need nothing
    /// here: their rows are still dead, and the next reclamation punches them again.
    ///
    /// # Errors
    /// SQLite, lock and container errors; `StoreError::StorageFatal` for a failed sync of the
    /// reclaimed ranges.
    pub(crate) fn recover(&self, conn: &mut Connection) -> Result<RecoverReport> {
        let mut report = RecoverReport::default();
        let (Some(pins), Some(_)) = (self.pins.as_deref(), self.container) else {
            return Ok(report);
        };
        let Some(_exclusive) = pins::exclusive_maintenance(pins, RECOVER_WAIT)? else {
            report.maintenance_live = true;
            return Ok(report);
        };
        report.claims_cleared = self.commit(conn, Durability::Normal, |txn| {
            let n = txn.conn().execute("DELETE FROM cat_claim", []).sql()?;
            Ok(u64::try_from(n).unwrap_or(u64::MAX))
        })?;
        fail::point("recover.mid");
        let named = self.read(conn, |tx| named_ranges(tx))?;
        let leaks = self.unreferenced(&named)?;
        self.reclaim_ranges(&leaks)?;
        report.ranges_reclaimed = leaks;
        Ok(report)
    }

    /// Verify the catalog (read only): every extent class, unreferenced ranges, missing cold
    /// files, views in `needs_reimport`, chain inconsistencies, content hash mismatches of live
    /// extents (a row without a hash is reported apart, not failed) and stale claims, all from
    /// one read transaction. A pin taken before that transaction keeps every extent it reads
    /// from reclamation while it is hashed, and the maintenance probe is held through it, so
    /// claims and unnamed ranges count only while no maintenance can run. Retired extents are
    /// not hashed: a reclamation may already have punched them.
    ///
    /// # Errors
    /// SQLite, lock and container errors.
    pub(crate) fn verify(&self, conn: &Connection) -> Result<VerifyReport> {
        // taken before the read: every extent live in it stays unreclaimed while it is hashed
        let budget = QueryBudget::new(u64::MAX, Some(self.maintenance_budget().deadline));
        let _pin = self.pin_untouched(conn, &budget, &[])?;
        // held through the read: no maintenance starts while claims and ranges are read
        let probe = match (self.pins.as_deref(), self.container) {
            (Some(pins), Some(_)) => Some(pins::exclusive_maintenance(pins, Duration::ZERO)?),
            _ => None,
        };
        let maintenance_live = matches!(probe, Some(None));
        let (mut report, extents) = self.read(conn, |tx| {
            let mut report = VerifyReport {
                classes: query(
                    tx,
                    "SELECT class, sum(live), sum(dead), sum(sediment), max(newest) FROM (
                       SELECT class, dead_since IS NULL AS live, dead_since IS NOT NULL AS dead,
                              0 AS sediment,
                              CASE WHEN dead_since IS NULL THEN generation END AS newest
                       FROM cat_extent
                       UNION ALL
                       SELECT class, 0, 0, 1, NULL FROM cat_sediment WHERE dead IS NULL
                     ) GROUP BY class ORDER BY class",
                    |r| {
                        Ok(ClassExtents {
                            class: r.get(0)?,
                            live: uint(r, 1)?,
                            dead: uint(r, 2)?,
                            sediment_rows: uint(r, 3)?,
                            newest_generation: r
                                .get::<_, Option<i64>>(4)?
                                .map(|g| u64::try_from(g).unwrap_or(0)),
                        })
                    },
                )?,
                needs_reimport: query(
                    tx,
                    "SELECT name FROM cat_view WHERE state = 'needs_reimport' ORDER BY name",
                    |r| r.get(0),
                )?,
                chain_inconsistencies: chain_inconsistencies(tx)?,
                stale_claims: if maintenance_live {
                    Vec::new()
                } else {
                    query(tx, "SELECT extent FROM cat_claim ORDER BY extent", |r| {
                        uint(r, 0)
                    })?
                },
                maintenance_live,
                ..VerifyReport::default()
            };
            let extents = query(
                tx,
                "SELECT id, kbx_offset, kbx_len, relpath, content_sha256 FROM cat_extent
                 WHERE dead_since IS NULL ORDER BY id",
                |r| {
                    let location = match r.get::<_, Option<String>>(3)? {
                        Some(relpath) => ExtentLocation::File { relpath },
                        None => ExtentLocation::Kbx {
                            offset: uint(r, 1)?,
                            len: uint(r, 2)?,
                        },
                    };
                    Ok((uint(r, 0)?, location, r.get::<_, Option<Vec<u8>>>(4)?))
                },
            )?;
            let named = named_ranges(tx)?;
            report.unreferenced_ranges = self.unreferenced(&named)?;
            Ok((report, extents))
        })?;
        drop(probe);
        self.failpoint(Window::VerifyHashing);
        let container = self.open_container()?;
        for (id, location, sha256) in extents {
            match location {
                ExtentLocation::File { relpath } => {
                    let dir = self.kbx.parent().unwrap_or_else(|| Path::new("."));
                    if !dir.join(&relpath).exists() {
                        report.missing_files.push(relpath);
                    }
                }
                ExtentLocation::Kbx { offset, len } => {
                    let (Some(file), Some(expected)) = (&container, sha256) else {
                        report.unhashed.push(id);
                        continue;
                    };
                    let mut hash = Sha256::new();
                    read_chunks(file, offset, len, |chunk| {
                        hash.update(chunk);
                        true
                    })
                    .map_err(|e| failed("hash an extent of", &self.kbx, e))?;
                    if hash.finalize().as_slice() != expected.as_slice() {
                        report.hash_mismatches.push(id);
                    }
                }
            }
        }
        Ok(report)
    }

    /// The container opened for reading, `None` when this catalog has none.
    fn open_container(&self) -> Result<Option<File>> {
        if self.container.is_none() {
            return Ok(None);
        }
        kbx::open_existing(&self.kbx)
            .map(Some)
            .map_err(|e| failed("open", &self.kbx, e))
    }

    /// The container ranges no row of `named` covers that still hold a nonzero byte (a
    /// released range reads as zeros: punched or zero-filled), in offset order.
    fn unreferenced(&self, named: &[(u64, u64)]) -> Result<Vec<(u64, u64)>> {
        let Some(file) = self.open_container()? else {
            return Ok(Vec::new());
        };
        let end = file
            .metadata()
            .map_err(|e| failed("measure", &self.kbx, e))?
            .len();
        let mut gaps = Vec::new();
        let mut at = 0_u64;
        for (offset, len) in coalesce(named.to_vec()) {
            if offset > at {
                gaps.push((at, offset - at));
            }
            at = at.max(offset.saturating_add(len));
        }
        if end > at {
            gaps.push((at, end - at));
        }
        let mut leaks = Vec::new();
        for (offset, len) in gaps {
            let mut data = false;
            read_chunks(&file, offset, len, |chunk| {
                data = chunk.iter().any(|b| *b != 0);
                !data
            })
            .map_err(|e| failed("read a range of", &self.kbx, e))?;
            if data {
                leaks.push((offset, len));
            }
        }
        Ok(leaks)
    }
}

/// Every container range a catalog row names (live or retired), as `(offset, len)`.
fn named_ranges(tx: &Connection) -> Result<Vec<(u64, u64)>> {
    query(
        tx,
        "SELECT kbx_offset, kbx_len FROM cat_extent WHERE kbx_offset IS NOT NULL",
        |r| Ok((uint(r, 0)?, uint(r, 1)?)),
    )
}

/// The chain checks, one line per finding: each view's positions contiguous from 0, no
/// retired extent in a chain, no live extent outside every chain.
fn chain_inconsistencies(tx: &Connection) -> Result<Vec<String>> {
    let mut found = query(
        tx,
        "SELECT view FROM cat_view_extent GROUP BY view
         HAVING min(position) != 0 OR max(position) + 1 != count(*) ORDER BY view",
        |r| {
            Ok(format!(
                "view {}: positions not contiguous from 0",
                r.get::<_, String>(0)?
            ))
        },
    )?;
    found.extend(query(
        tx,
        "SELECT v.view, e.id FROM cat_view_extent v JOIN cat_extent e ON e.id = v.extent_id
         WHERE e.dead_since IS NOT NULL ORDER BY v.view, e.id",
        |r| {
            Ok(format!(
                "view {}: retired extent {} is still in its chain",
                r.get::<_, String>(0)?,
                uint(r, 1)?
            ))
        },
    )?);
    found.extend(query(
        tx,
        "SELECT id FROM cat_extent e WHERE dead_since IS NULL
           AND NOT EXISTS (SELECT 1 FROM cat_view_extent v WHERE v.extent_id = e.id)
         ORDER BY id",
        |r| Ok(format!("extent {} is live but in no chain", uint(r, 0)?)),
    )?);
    Ok(found)
}

/// The rows of `sql` (no parameters) mapped by `f`.
fn query<T>(
    tx: &Connection,
    sql: &str,
    f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = tx.prepare(sql).sql()?;
    let rows = stmt.query_map([], f).sql()?;
    rows.collect::<rusqlite::Result<_>>().sql()
}

/// Read `len` bytes of `file` at `offset` in chunks of [`READ_CHUNK`], handing each to `f`
/// until it returns `false`.
fn read_chunks(
    file: &File,
    offset: u64,
    len: u64,
    mut f: impl FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    let mut buf = vec![0_u8; READ_CHUNK];
    let mut done = 0_u64;
    while done < len {
        let want = usize::try_from(len - done).map_or(READ_CHUNK, |n| n.min(READ_CHUNK));
        let chunk = &mut buf[..want];
        file.read_exact_at(chunk, offset + done)?;
        if !f(chunk) {
            return Ok(());
        }
        done += want as u64;
    }
    Ok(())
}
