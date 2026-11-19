//! The text index's class on the catalog (spec 11 T7, KER-01, TXT-06): every commit that
//! changes a node stages one text pack (`kaban_index_tantivy::segment`): a segment of the
//! changed rids' current active rows, built in RAM, and the identity tombstones of every rid
//! it changed, which hide the older documents of those rids (a delete stages tombstones and no
//! document). A search reads the tiers of `main` and its pending packs from one pin
//! ([`text_inputs`]), so a commit is searchable by every store once it returns. A seal merges
//! the pending packs compaction hands it into one tier (the trigger is compaction's
//! `seal_ops.text`); the merge leaves out every document a newer pack's tombstone hides, and a
//! tombstone that commits during the merge stays pending and hides the merged document from
//! every read.
//!
//! ```text
//!   commit ==> stage: changed rids => active rows => RAM segment + tombstones => cat_sediment
//!   read   ==> pin(main) => tiers (kbx spans) + text sediment => TextSnapshot => search_at
//!   seal   ==> due pending packs => merge (hidden documents dropped) => one tier in main
//!   fold   ==> consecutive tiers => one tier; a tombstone kept while an older tier needs it
//!   purge  ==> each tier holding a purged rid => rewritten without its documents/tombstones
//!   rebuild==> every active row => one tier in main; old tiers and packs retire (one commit)
//! ```

use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_core::text_index::{ExtentSpan, TextSnapshot};
use kaban_core::{Durability, Result, StoreError, ViewName};
use rusqlite::Connection;
use sha2::{Digest as _, Sha256};
use tantivy::directory::OwnedBytes;

use super::Catalog;
use super::capture::BatchChanges;
use super::class::{ExtentClass, ExtentLocation, IndexClass};
use super::commit::Stage;
use super::kbx;
use super::pin::{Pin, PinnedView};
use super::tables::{ExtentRef, SedimentRow, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// The classes a text read loads: the text tiers and their pending packs.
const TEXT: &[ExtentClass] = &[ExtentClass::TextTier];

/// The memory ceiling of one commit's staging (its RAM segment and the rows it reads).
const STAGE_MEMORY_BYTES: u64 = 256 << 20;

/// The text index's class.
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

    /// One pack for the commit's changed nodes; nothing for a commit that changed none.
    fn stage(&self, txn: &mut Stage<'_>, changes: &BatchChanges) -> Result<()> {
        let mut rids: Vec<i64> = changes.nodes().iter().map(|c| c.rid).collect();
        rids.sort_unstable();
        rids.dedup();
        if rids.is_empty() {
            return Ok(());
        }
        let budget = QueryBudget::new(STAGE_MEMORY_BYTES, None);
        let (rows, _charged) = crate::index_rows::staged_rows(&txn.reader(), &rids, &budget)?;
        let pack = kaban_index_tantivy::segment(&rows, &rids, &budget)?;
        txn.put_sediment(ExtentClass::TextTier, &pack)?;
        Ok(())
    }

    /// Due when more than `ops` pending packs carry a segment (`seal_ops.text`, default 32): a
    /// pack of tombstones only adds no segment to a search.
    fn seal_due(&self, rows: &[&SedimentRow], ops: u32) -> Result<bool> {
        let segments = rows
            .iter()
            .filter(|r| kaban_index_tantivy::has_segment(&r.payload))
            .count();
        Ok(segments > ops as usize)
    }

    /// The pending packs `rows` merged into one tier.
    fn seal(
        &self,
        _pin: &Pin,
        rows: &[&SedimentRow],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        // the copies the merge reads, charged before they are made
        let copies = rows
            .iter()
            .map(|r| r.payload.len() + size_of::<OwnedBytes>())
            .sum::<usize>();
        let _copies = budget.reserve(u64::try_from(copies).unwrap_or(u64::MAX))?;
        let packs: Vec<OwnedBytes> = rows
            .iter()
            .map(|r| OwnedBytes::new(r.payload.clone()))
            .collect();
        kaban_index_tantivy::merge(&packs, &[], None, budget)
    }

    /// The consecutive tiers `extents` of `main` merged into one: a document a newer tier's
    /// tombstone hides is left out, and a tombstone is kept only while some tier older than
    /// `extents` in the chain holds a document of its rid.
    fn fold(
        &self,
        pin: &Pin,
        extents: &[ExtentRef],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let main = pin
            .loaded(&ViewName::Main, TEXT)
            .ok_or_else(|| StoreError::NotFound("the pinned text of main".into()))?;
        let tiers: Vec<&ExtentRef> = main
            .chain()
            .iter()
            .filter(|e| e.class == ExtentClass::TextTier)
            .collect();
        let at = extents
            .first()
            .and_then(|first| tiers.iter().position(|e| *e == first))
            .ok_or_else(|| StoreError::NotFound("a folded text tier in main".into()))?;
        let (older, _older) = read_tiers(pin, &tiers[..at], budget)?;
        let run: Vec<&ExtentRef> = extents.iter().collect();
        let (run, _run) = read_tiers(pin, &run, budget)?;
        kaban_index_tantivy::merge(&run, &[], Some(&older), budget)
    }

    /// The tier `extent` without the documents of `rids` and their tombstones (the purge
    /// rewrites every tier that holds them in one switch, so no older tier needs them);
    /// `None` when it holds no document of them. The pending packs that name them are left
    /// to their seal (amendment 4.2): the purge commit's own pack hides them from every read,
    /// and a seal that takes that pack with them leaves them out.
    fn purge(
        &self,
        pin: &Pin,
        extent: &ExtentRef,
        rids: &[i64],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let (tier, _tier) = read_tiers(pin, &[extent], budget)?;
        kaban_index_tantivy::merge(&tier, rids, None, budget)
    }

    /// Rebuild `main`'s text from the rows (spec 11 7.2.7), in one commit on the writer
    /// connection `rows`, under the maintenance budget (7.6): one tier of every active row
    /// replaces every text tier and pending text pack ([`retire_text`]). A catalog without a
    /// container (in memory) holds only sediment: nothing to rebuild.
    fn rebuild(&self, catalog: &Catalog, rows: &mut Connection) -> Result<()> {
        if catalog.container.is_none() {
            return Ok(());
        }
        let maintenance = catalog.maintenance_budget();
        let budget = QueryBudget::new(maintenance.memory_bytes, Some(maintenance.deadline));
        catalog.commit(rows, Durability::Normal, |txn| {
            let (active, _charged) = crate::index_rows::active_rows(txn.conn(), &budget)?;
            let tier = kaban_index_tantivy::segment(&active, &[], &budget)?;
            let generation = sql_int(txn.generation()?.0).sql()?;
            retire_text(&catalog.kbx, txn.conn(), generation)?;
            let extent = catalog.append_extent(ExtentClass::TextTier, &tier)?;
            txn.publish(extent, &[ViewName::Main])?;
            Ok(())
        })
    }
}

/// The bytes of the tiers `extents` from `pin`, their length reserved on `budget` before
/// they are read (amendment 4.2: a consumer charges the spans it retains), with the lease
/// that pays for them.
fn read_tiers(
    pin: &Pin,
    extents: &[&ExtentRef],
    budget: &QueryBudget,
) -> Result<(Vec<OwnedBytes>, BudgetLease)> {
    let bytes = extents
        .iter()
        .map(|e| match e.location {
            ExtentLocation::Kbx { len, .. } => len,
            ExtentLocation::File { .. } => 0,
        })
        .sum::<u64>()
        .saturating_add(u64::try_from(extents.len() * size_of::<OwnedBytes>()).unwrap_or(u64::MAX));
    let lease = budget.reserve(bytes)?;
    let tiers = extents
        .iter()
        .map(|e| pin.bytes(e))
        .collect::<Result<Vec<_>>>()?;
    Ok((tiers, lease))
}

/// One text tier row as the rebuild judges it: id, range, content hash and liveness.
type TierRow = (u64, Option<(u64, u64)>, Option<Vec<u8>>, bool);

/// Take every text tier and pending text pack out of the catalog in the open writer
/// transaction on `conn` (generation `generation`): chains and claims lose the tiers, a tier
/// whose bytes are intact in the container `kbx` (its content hash matches) retires (and is
/// reclaimed once no pin reaches it), one whose bytes are gone (a lost `kaban.kbx`) loses its
/// row, so no reclamation ever releases a range the new container reuses; the pending packs
/// are consumed.
fn retire_text(kbx: &Path, conn: &Connection, generation: i64) -> Result<()> {
    let class = ExtentClass::TextTier.as_str();
    let tiers: Vec<TierRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, kbx_offset, kbx_len, content_sha256, dead_since IS NULL
                 FROM cat_extent WHERE class = ?1",
            )
            .sql()?;
        let found = stmt
            .query_map([class], |r| {
                let range = match r.get::<_, Option<i64>>(1)? {
                    Some(_) => Some((uint(r, 1)?, uint(r, 2)?)),
                    None => None,
                };
                Ok((uint(r, 0)?, range, r.get(3)?, r.get(4)?))
            })
            .sql()?;
        found.collect::<rusqlite::Result<_>>().sql()?
    };
    let file = kbx::open_existing(kbx).ok();
    for (id, range, sha, live) in tiers {
        let intact = match (&file, range, sha) {
            (Some(file), Some((offset, len)), Some(sha)) => holds(file, offset, len, &sha),
            _ => false,
        };
        let id = sql_int(id).sql()?;
        conn.execute("DELETE FROM cat_view_extent WHERE extent_id = ?1", [id])
            .sql()?;
        conn.execute("DELETE FROM cat_claim WHERE extent = ?1", [id])
            .sql()?;
        if !intact {
            conn.execute("DELETE FROM cat_extent WHERE id = ?1", [id])
                .sql()?;
        } else if live {
            conn.execute(
                "UPDATE cat_extent SET dead_since = ?1 WHERE id = ?2",
                rusqlite::params![generation, id],
            )
            .sql()?;
        }
    }
    conn.execute(
        "UPDATE cat_sediment SET dead = ?1 WHERE class = ?2 AND dead IS NULL",
        rusqlite::params![generation, class],
    )
    .sql()?;
    Ok(())
}

/// Whether `file` holds `len` bytes at `offset` whose SHA-256 is `sha`.
fn holds(file: &File, offset: u64, len: u64, sha: &[u8]) -> bool {
    const CHUNK: u64 = 1 << 16;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1 << 16];
    let mut done = 0_u64;
    while done < len {
        let n = (len - done).min(CHUNK);
        let Some(chunk) = usize::try_from(n).ok().and_then(|n| buf.get_mut(..n)) else {
            return false;
        };
        if file.read_exact_at(chunk, offset + done).is_err() {
            return false;
        }
        hasher.update(&*chunk);
        done += n;
    }
    hasher.finalize().as_slice() == sha
}

impl Catalog {
    /// Run `f` over the text of the latest generation (spec 11 T7): `main`'s tiers and pending
    /// text packs, pinned once under `budget` with `conn` locked only for the pin. The pin
    /// outlives `f`, so every tier `f` maps stays protected from reclamation.
    ///
    /// # Errors
    /// The pin's or `f`'s.
    pub(crate) fn with_text<T>(
        &self,
        conn: &Mutex<Connection>,
        budget: &QueryBudget,
        f: impl FnOnce(&TextSnapshot<'_>) -> Result<T>,
    ) -> Result<T> {
        // `main`'s last use is never consulted (retention never retires it): no write per read
        let (pin, inputs) = {
            let conn = conn.lock().unwrap_or_else(PoisonError::into_inner);
            let pin = self.pin_untouched(&conn, budget, &[(ViewName::Main, TEXT)])?;
            let inputs = text_inputs(&pin, &conn, self.kbx.clone())?;
            (pin, inputs)
        };
        let sediment = inputs.sediment();
        let out = f(&inputs.snapshot(&sediment));
        drop(pin);
        out
    }
}

/// The text inputs of a pin's `main` view: the tiers in its chain and its pending text packs.
#[derive(Debug)]
struct TextInputs {
    generation: u64,
    kbx: PathBuf,
    view: Arc<PinnedView>,
    tiers: Vec<ExtentSpan>,
}

/// The text inputs of `pin`'s `main` view, loaded through `conn` (the pin's own transaction,
/// or any connection once the pin loaded `main`), in the container `kbx`.
///
/// # Errors
/// As `Pin::load_view`.
fn text_inputs(pin: &Pin, conn: &Connection, kbx: PathBuf) -> Result<TextInputs> {
    let view = pin.load_view(conn, &ViewName::Main, TEXT)?;
    let tiers = view
        .chain()
        .iter()
        .filter(|e| e.class == ExtentClass::TextTier)
        .filter_map(|e| match e.location {
            ExtentLocation::Kbx { offset, len } => Some(ExtentSpan { offset, len }),
            ExtentLocation::File { .. } => None,
        })
        .collect();
    Ok(TextInputs {
        generation: pin.generation().0,
        kbx,
        view,
        tiers,
    })
}

impl TextInputs {
    /// The pending text packs, oldest first.
    fn sediment(&self) -> Vec<&[u8]> {
        self.view
            .sediment()
            .iter()
            .map(|r| r.payload.as_slice())
            .collect()
    }

    /// The snapshot over these inputs and `sediment` ([`Self::sediment`]).
    fn snapshot<'a>(&'a self, sediment: &'a [&'a [u8]]) -> TextSnapshot<'a> {
        TextSnapshot {
            generation: self.generation,
            kbx: &self.kbx,
            tiers: &self.tiers,
            sediment,
        }
    }
}
