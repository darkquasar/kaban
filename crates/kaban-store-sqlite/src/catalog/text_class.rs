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
//!   rebuild==> every active row => one tier in main; old tiers and packs retire (one commit)
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::query_budget::QueryBudget;
use kaban_core::text_index::{ExtentSpan, TextSnapshot};
use kaban_core::{Durability, Result, ViewName};
use rusqlite::Connection;
use tantivy::directory::OwnedBytes;

use super::Catalog;
use super::capture::BatchChanges;
use super::class::{ExtentClass, ExtentLocation, IndexClass};
use super::commit::Stage;
use super::pin::{Pin, PinnedView};
use super::tables::{ExtentRef, SedimentRow, sql_int};
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
        kaban_index_tantivy::merge(&packs, &[], budget)
    }

    /// Folds nothing yet: the tiers stay as sealed.
    fn fold(
        &self,
        _pin: &Pin,
        _extents: &[ExtentRef],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Rewrites nothing yet: a purged record's tombstones hide it from every read.
    fn purge(
        &self,
        _pin: &Pin,
        _extent: &ExtentRef,
        _rids: &[i64],
        _budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Rebuild `main`'s text from the rows (spec 11 7.2.7), in one commit on the writer
    /// connection `rows`: one tier of every active row replaces every text tier and pending
    /// text pack, which retire (and are reclaimed once no pin reaches them). A catalog without
    /// a container (in memory) holds only sediment: nothing to rebuild.
    fn rebuild(&self, catalog: &Catalog, rows: &mut Connection) -> Result<()> {
        if catalog.container.is_none() {
            return Ok(());
        }
        catalog.commit(rows, Durability::Normal, |txn| {
            let budget = QueryBudget::new(STAGE_MEMORY_BYTES, None);
            let (active, _charged) = crate::index_rows::active_rows(txn.conn(), &budget)?;
            let tier = kaban_index_tantivy::segment(&active, &[], &budget)?;
            let generation = sql_int(txn.generation()?.0).sql()?;
            retire_text(txn.conn(), generation)?;
            let extent = catalog.append_extent(ExtentClass::TextTier, &tier)?;
            txn.publish(extent, &[ViewName::Main])?;
            Ok(())
        })
    }
}

/// Retire every text tier (out of every chain, its claim cleared, dead since `generation`) and
/// every pending text pack (dead at `generation`).
fn retire_text(conn: &Connection, generation: i64) -> Result<()> {
    let class = ExtentClass::TextTier.as_str();
    let extents = "SELECT id FROM cat_extent WHERE class = ?1";
    conn.execute(
        &format!("DELETE FROM cat_view_extent WHERE extent_id IN ({extents})"),
        [class],
    )
    .sql()?;
    conn.execute(
        &format!("DELETE FROM cat_claim WHERE extent IN ({extents})"),
        [class],
    )
    .sql()?;
    conn.execute(
        "UPDATE cat_extent SET dead_since = ?1 WHERE class = ?2 AND dead_since IS NULL",
        rusqlite::params![generation, class],
    )
    .sql()?;
    conn.execute(
        "UPDATE cat_sediment SET dead = ?1 WHERE class = ?2 AND dead IS NULL",
        rusqlite::params![generation, class],
    )
    .sql()?;
    Ok(())
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
