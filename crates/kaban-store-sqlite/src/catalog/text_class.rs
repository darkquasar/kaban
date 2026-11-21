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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_core::text_index::{ExtentSpan, IndexRow, TextSnapshot};
use kaban_core::{Durability, Result, StoreError, ViewName};
use rusqlite::Connection;
use tantivy::directory::OwnedBytes;

use super::Catalog;
use super::capture::BatchChanges;
use super::class::{ExtentClass, ExtentLocation, IndexClass};
use super::commit::Stage;
use super::kbx;
use super::pin::{Pin, PinnedView};
use super::tables::{ExtentRef, SedimentRow, holds, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// The classes a text read loads: the text tiers and their pending packs.
const TEXT: &[ExtentClass] = &[ExtentClass::TextTier];

/// The memory ceiling of one commit's staging (its RAM segment and the rows it reads): a body
/// at the 64 MiB decode bound stages within it (its row charged twice, the writer reserved a
/// byte per indexed byte, the body indexed three times).
const STAGE_MEMORY_BYTES: u64 = 512 << 20;

/// The maximum rows a rebuild indexes per writer (PC-17): each batch is its own segment under its own
/// reservation, and the batches are merged under the budget.
const REBUILD_BATCH_ROWS: usize = 1024;

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
        if !rids.is_empty() {
            let budget = QueryBudget::new(STAGE_MEMORY_BYTES, None);
            stage_rows(txn, &rids, &budget)?;
        }
        if let Some((label, maintenance)) = txn.text_projection {
            // Refuse the declaration together with its partially staged rows on exhaustion.
            // The caller can retry under a larger configured maintenance budget.
            let budget = QueryBudget::new(maintenance.memory_bytes, Some(maintenance.deadline));
            let limit = u32::try_from(REBUILD_BATCH_ROWS).unwrap_or(u32::MAX);
            let _ids = budget.reserve(u64::from(limit) * 2 * size_of::<i64>() as u64)?;
            let mut first = i64::MIN;
            loop {
                budget.check()?;
                let rids = txn
                    .reader()
                    .prepare(
                        "SELECT rid FROM node WHERE kind=?1 AND lifecycle='active' AND rid>=?2
                     ORDER BY rid LIMIT ?3",
                    )?
                    .query_map(rusqlite::params![label, first, limit], |row| {
                        row.get::<_, i64>(0)
                    })?;
                let Some(last) = rids.last() else { break };
                stage_rows(txn, &rids, &budget)?;
                let Some(next) = last.checked_add(1) else {
                    break;
                };
                first = next;
            }
        }
        Ok(())
    }

    /// Due when more than `ops` pending packs carry a segment (`seal_ops.text`, default 32),
    /// or once a pack of tombstones only is pending (a delete or a purge): its seal leaves out
    /// the documents it hides in the older packs, so a purged record's text never waits for
    /// later writes to leave the pending packs (KER-05).
    fn seal_due(&self, rows: &[&SedimentRow], ops: u32) -> Result<bool> {
        let segments = rows
            .iter()
            .filter(|r| kaban_index_tantivy::has_segment(&r.payload))
            .count();
        Ok(segments > ops as usize || segments < rows.len())
    }

    /// The pending packs `rows` merged into one tier (one pack that hides nothing in itself
    /// is the tier as it is).
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
        let merged = kaban_index_tantivy::merge(&packs, &[], None, budget)?;
        Ok(merged.or_else(|| match rows {
            [one] => Some(one.payload.clone()),
            _ => None,
        }))
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
        let hold = catalog.maintenance_hold()?.map(Arc::new);
        catalog.commit(rows, Durability::Normal, |txn| {
            rebuild_in(catalog, txn, &budget, |class, bytes| {
                kbx::append(&catalog.kbx, class, bytes, hold)
            })
        })
    }
}

/// Replace the text of these row identities in this commit's sediment.
fn stage_rows(txn: &mut Stage<'_>, rids: &[i64], budget: &QueryBudget) -> Result<()> {
    let (rows, _charged) = crate::index_rows::staged_rows(&txn.reader(), rids, budget)?;
    let pack = kaban_index_tantivy::segment(&rows, rids, budget)?;
    txn.put_sediment(ExtentClass::TextTier, &pack)?;
    Ok(())
}

/// Rebuild in the caller's transaction and maintenance ownership.
pub(super) fn rebuild_in(
    catalog: &Catalog,
    txn: &mut super::commit::Txn<'_>,
    budget: &QueryBudget,
    append: impl FnOnce(ExtentClass, &[u8]) -> Result<kbx::Unpublished>,
) -> Result<()> {
    let (active, _charged) = crate::index_rows::active_rows(txn.conn(), budget)?;
    let tier = rebuilt_tier(&active, REBUILD_BATCH_ROWS, budget)?;
    let generation = sql_int(txn.generation()?.0).sql()?;
    retire_text(&catalog.kbx, txn.conn(), generation, budget)?;
    let extent = append(ExtentClass::TextTier, &tier)?;
    // a deadline or cancellation since the last check publishes nothing
    budget.check()?;
    txn.publish(extent, &[ViewName::Main])?;
    crate::ident::mark_indexed(txn.conn())
}

/// One tier of `rows`, indexed in batches bounded by row count and projected bytes under
/// `budget`. Each pack is charged while held for the merge.
///
/// # Errors
/// The budget's refusal; Tantivy failures.
fn rebuilt_tier(mut rows: &[IndexRow], batch: usize, budget: &QueryBudget) -> Result<Vec<u8>> {
    let mut packs = Vec::new();
    let mut held = Vec::new();
    loop {
        let (pack, remaining) = kaban_index_tantivy::rebuild_batch(rows, batch, budget)?;
        if packs.is_empty() && remaining.is_empty() {
            return Ok(pack);
        }
        let bytes = pack.len() + size_of::<OwnedBytes>();
        held.push(budget.reserve(u64::try_from(bytes).unwrap_or(u64::MAX))?);
        packs.push(OwnedBytes::new(pack));
        if remaining.is_empty() {
            break;
        }
        rows = remaining;
    }
    kaban_index_tantivy::merge(&packs, &[], None, budget)?
        .ok_or_else(|| StoreError::NotFound("the merged rebuild batches".into()))
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
/// are consumed. The hashing checks `budget` per tier and per chunk (PC-17).
fn retire_text(kbx: &Path, conn: &Connection, generation: i64, budget: &QueryBudget) -> Result<()> {
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
        #[cfg(test)]
        ON_TIER.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(budget);
            }
        });
        let intact = match (&file, range, sha) {
            (Some(file), Some((offset, len)), Some(sha)) => holds(file, offset, len, &sha, budget)?,
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
    super::tables::renumber_chains(conn, budget)?;
    conn.execute(
        "UPDATE cat_sediment SET dead = ?1 WHERE class = ?2 AND dead IS NULL",
        rusqlite::params![generation, class],
    )
    .sql()?;
    Ok(())
}

/// A test's hook on every old tier a rebuild judges, given the rebuild's budget.
#[cfg(test)]
type TierHook = Box<dyn FnMut(&QueryBudget)>;

#[cfg(test)]
thread_local! {
    /// Called with the rebuild's budget on every old tier it judges: the tests cancel there.
    static ON_TIER: std::cell::RefCell<Option<TierHook>> = const { std::cell::RefCell::new(None) };
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
        index: &crate::tantivy::Attached,
        budget: &QueryBudget,
        f: impl FnOnce(&dyn kaban_core::text_index::TextIndex, &TextSnapshot<'_>) -> Result<T>,
    ) -> Result<T> {
        // `main`'s last use is never consulted (retention never retires it): no write per read
        let (pin, inputs, selected) = {
            let conn = conn.lock().unwrap_or_else(PoisonError::into_inner);
            let pin = self.pin_untouched(&conn, budget, &[(ViewName::Main, TEXT)])?;
            let inputs = self.pinned_text(&pin, &conn)?;
            let selected = inputs.select(index);
            (pin, inputs, selected)
        };
        let out = inputs.with_reader(index, selected, budget, f);
        drop(pin);
        out
    }

    /// Resolve the text inputs through the held transaction and pin.
    pub(crate) fn pinned_text(&self, pin: &Pin, conn: &Connection) -> Result<TextInputs> {
        text_inputs(pin, conn, self.kbx.clone())
    }
}

/// The text inputs of a pin's `main` view: the tiers in its chain and its pending text packs.
#[derive(Debug)]
pub(crate) struct TextInputs {
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
    pub(crate) fn select(&self, index: &crate::tantivy::Attached) -> crate::tantivy::Selection {
        let mut tiers = self
            .view
            .chain()
            .iter()
            .filter(|e| e.class == ExtentClass::TextTier);
        let tier = match (tiers.next(), tiers.next(), self.tiers.as_slice()) {
            (Some(e), None, [span]) if self.view.sediment().is_empty() => {
                Some((e.id.0, e.generation.0, *span))
            }
            _ => None,
        };
        index.observe(self.generation, &self.kbx, tier)
    }

    pub(crate) fn with_reader<T>(
        &self,
        index: &crate::tantivy::Attached,
        selected: crate::tantivy::Selection,
        budget: &QueryBudget,
        f: impl FnOnce(&dyn kaban_core::text_index::TextIndex, &TextSnapshot<'_>) -> Result<T>,
    ) -> Result<T> {
        let _sediment = budget.reserve((self.view.sediment().len() * size_of::<&[u8]>()) as u64)?;
        let sediment = self.sediment();
        index.with_reader(selected, budget, |reader| {
            f(reader, &self.snapshot(&sediment))
        })
    }

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use kaban_core::config::{CatalogCfg, SealOps};
    use kaban_core::query_budget::{BudgetExceeded, Limit, QueryBudget};
    use kaban_core::{Actor, MemoryStore, NewNode, StoreError, WorkspaceId, WriteCtx};

    use super::{IndexClass, ON_TIER, TextClass};
    use crate::SqliteStore;
    use kaban_core::text_index::{
        IndexRow, MatchMode, Searched, SeedFilter, TextIndex, TextQuery, TextSnapshot,
    };
    use std::fmt::Write as _;

    /// The live text tiers of `store`, by id.
    fn tiers(store: &SqliteStore) -> Vec<i64> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id FROM cat_extent WHERE class = 'text_tier' AND dead_since IS NULL \
                 ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// PC-17 (T7 end review M2): a rebuild cancelled while it judges the old tiers publishes
    /// nothing: the old tier stays the live one and the rebuild returns the cancellation.
    #[test]
    fn a_rebuild_cancelled_while_it_hashes_the_old_tiers_publishes_nothing() {
        // Arrange: a file store whose text sealed into one tier
        let dir = std::env::temp_dir().join(format!("kaban-t7e-rebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys"))
            .unwrap()
            .with_catalog_config(CatalogCfg {
                seal_ops: SealOps {
                    text: 1,
                    ..SealOps::default()
                },
                ..CatalogCfg::default()
            })
            .unwrap();
        let ctx = WriteCtx::new(WorkspaceId::default(), Actor::Agent);
        for text in ["quokka by the shore", "wombat in the dunes"] {
            let node = NewNode {
                kind: "Fact".into(),
                text: text.into(),
                ..Default::default()
            };
            store.remember(node, &[], &ctx).unwrap();
        }
        store.compact().unwrap();
        let before = tiers(&store);
        assert_eq!(before.len(), 1, "the fixture seals one tier");
        ON_TIER.set(Some(Box::new(QueryBudget::cancel)));

        // Act
        let rebuilt = TextClass::new().rebuild(store.catalog(), &mut store.lock());

        // Assert
        ON_TIER.set(None);
        let after = tiers(&store);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(
                rebuilt,
                Err(StoreError::BudgetExceeded(BudgetExceeded {
                    limit: Limit::Cancelled,
                    ..
                }))
            ),
            "expected the budget's cancellation, got {rebuilt:?}"
        );
        assert_eq!(after, before, "the cancelled rebuild published nothing");
    }

    /// An active row holding `word` and about `bytes` bytes of words no other row holds.
    fn large_row(rid: i64, word: &str, bytes: usize) -> IndexRow {
        let mut text = format!("{word} ");
        let mut state = u64::try_from(rid)
            .unwrap()
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            | 1;
        while text.len() < bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            write!(text, "w{:012x} ", state & 0xFFFF_FFFF_FFFF).unwrap();
        }
        IndexRow {
            id: format!("id-{rid}"),
            rid,
            revision: 1,
            kind: "Fact".into(),
            workspace_id: "default".into(),
            tags: vec![],
            text,
            summary: String::new(),
            stored_summary: None,
            ident_values: vec![],
            anchor_keys: vec![],
        }
    }

    /// PC-17 (T7 end review M1): a rebuild indexes its rows in batches, each writer reserved
    /// for its own batch only, so rows too large for one writer under the budget still
    /// rebuild into one tier that holds every row.
    #[test]
    fn a_rebuild_too_large_for_one_writer_indexes_its_rows_in_batches() {
        // Arrange: six rows of 256 KiB of words no other row holds: one writer over all of
        // them grows past 20 MiB (measured: 24 MiB), one over two of them to 14 MiB
        let rows: Vec<IndexRow> = (1..=6)
            .map(|rid| large_row(rid, "quokka", 256 << 10))
            .collect();
        let budget = QueryBudget::new(20 << 20, None);

        // Act
        let tier = super::rebuilt_tier(&rows, 2, &budget);

        // Assert
        let tier = tier.unwrap_or_else(|e| panic!("expected the batched tier, got {e:?}"));
        assert_eq!(budget.live_bytes(), 0, "every charge was given back");
        let index = kaban_index_tantivy::TantivyIndex::new(
            &kaban_core::shipped::resolved().unwrap(),
            &kaban_core::config::Recall::default(),
        );
        let q = kaban_core::RecallQuery::default();
        let at = TextSnapshot {
            generation: 1,
            kbx: std::path::Path::new("unused.kbx"),
            tiers: &[],
            sediment: &[tier.as_slice()],
        };
        let query = TextQuery {
            text: "quokka",
            mode: MatchMode::Lexical,
            filter: SeedFilter::of(&q),
            limit: 10,
            sufficient: 10,
            skip: 0,
        };
        let searched = index
            .search(&at, &query, &QueryBudget::new(u64::MAX, None))
            .unwrap();
        let Searched::Hits { hits, .. } = searched else {
            panic!("the tier is unavailable");
        };
        let mut rids: Vec<i64> = hits.iter().map(|h| h.rid).collect();
        rids.sort_unstable();
        assert_eq!(rids, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_rebuild_below_the_row_limit_indexes_large_rows_and_refuses_an_unaffordable_row() {
        let rows: Vec<_> = (1..=2)
            .map(|rid| large_row(rid, "quokka", 384 << 10))
            .collect();
        let low = QueryBudget::new(8 << 20, None);
        let refused = super::rebuilt_tier(&rows[..1], super::REBUILD_BATCH_ROWS, &low);
        assert!(matches!(
            refused,
            Err(StoreError::BudgetExceeded(BudgetExceeded {
                limit: Limit::Memory,
                ..
            }))
        ));
        assert_eq!(low.live_bytes(), 0);

        let budget = QueryBudget::new(20 << 20, None);
        let tier = super::rebuilt_tier(&rows, super::REBUILD_BATCH_ROWS, &budget).unwrap();
        assert!(kaban_index_tantivy::has_segment(&tier));
        assert_eq!(budget.live_bytes(), 0);
    }
}
