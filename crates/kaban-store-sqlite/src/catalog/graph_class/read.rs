//! The pinned graph view loader (spec 11 T11; INV-2, PC-17, amendments 3.6, 4.1, 7.3, 7.5): a
//! view's graph chain opened from its pin, its pending graph sediment prepared once, the
//! relation dictionary, and the node id mapping through the pin's own transaction.
//!
//! ```text
//!   load_view(rows, pin, view) => pin.load_view (the chain, charged by the pin)
//!                                 + the pending graph seqs of the snapshot
//!     => cache (kbx, view): same generation and chain? reuse the opened extents
//!                           chain unchanged, rows a superset? extend the prepared sediment
//!                           (only the newer rows' payloads are read)
//!                           a seal's leftover fits? extend that; otherwise prepare afresh
//!     => LoadedView { opened extents, prepared sediment, relations, the transaction }
//! ```
//!
//! The cache (one entry per container and view, in this process) keeps what the newest
//! generation seen opened, for one incarnation of the container ([`Incarnation`]: a database
//! recreated at the same path restarts its generations, extent ids and sediment seqs, so an
//! entry of the old one is dropped, never served). Validated extents are reused only at that
//! generation: their bytes hold that pin's lock, so an entry is replaced as soon as a newer
//! generation loads, and an entry idle for [`IDLE`] is dropped at the next load or commit
//! ([`sweep`]). The prepared sediment, which holds no bytes of the container, is carried
//! forward and extended with the newer rows only (amendment 3.6). What a load builds is charged
//! to the budget of the query that builds it, before it is built, and stays charged while the
//! cache keeps it (the entry's own lists too); a query that reuses it is charged nothing more.
//! A store without a container (in memory) caches nothing.

#![cfg_attr(
    not(feature = "failpoints"),
    expect(dead_code, reason = "T14 and T16 load graph views")
)]

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "failpoints"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use kaban_core::ontology::{Ns, Qualified, stored_name};
use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_core::{BackendSource, NodeId, Result, StoreError, ViewName};
use kaban_index_graph::{
    Base, GraphError, Layer, LocalId, PreparedSediment, Records, RelId, StableOrder, TieRank, View,
};
use rusqlite::{Connection, OptionalExtension, Transaction};

use super::{CLASSES, GraphRefused, PURGED, SEDIMENT, graph_error};
use crate::catalog::class::ExtentClass;
use crate::catalog::pin::{OutsidePin, Pin, PinnedView, budget_exceeded};
use crate::catalog::tables::{
    ExtentId, ExtentRef, Generation, SedimentRow, SedimentSeq, sql_int, uint,
};
use crate::sql_error::SqlResultExt;

/// The pending rows of the class `?1`: its rows (index `cat_sediment_class`) less the consumed
/// ones (the partial index `cat_sediment_dead`), so the scan never reads a row itself (`dead`
/// is stored after the payload).
const PENDING_SQL: &str = "FROM cat_sediment WHERE class = ?1
     AND seq NOT IN (SELECT seq FROM cat_sediment WHERE dead IS NOT NULL)";

/// How long a cache entry may go unused before a load drops it.
const IDLE: Duration = Duration::from_secs(60);

/// The declared relations in declaration order (`schema_type` row order).
pub(super) const REL_SQL: &str =
    "SELECT ns, name FROM schema_type WHERE kind = 'rel' ORDER BY rowid";

/// The relation dictionary: a relation's id is the rank of its stored name among the declared
/// relations in declaration order, deduplicated. Declarations are only ever added (and keep their
/// order through `VACUUM`), so an id never changes or passes to another relation; a declared but
/// unused relation has its id too. Past `u16::MAX` a relation has none.
#[derive(Debug, Default)]
pub(crate) struct Rels {
    names: Vec<String>,
}

impl Rels {
    /// The dictionary of the rows `conn` sees, with its charge to `budget`, reserved before
    /// any of it is built: the names kept and the set that deduplicates them while they are
    /// built, each a copy of the declared names' bytes with its string headers. The rows are
    /// streamed, never collected.
    ///
    /// # Errors
    /// The budget's and SQLite's errors; a stored namespace that is not one.
    pub(super) fn read(conn: &Connection, budget: &QueryBudget) -> Result<(Self, BudgetLease)> {
        let (n, text): (i64, i64) = conn
            .query_row(
                "SELECT count(*), COALESCE(sum(length(CAST(ns AS BLOB))
                     + length(CAST(name AS BLOB)) + 1), 0)
                 FROM schema_type WHERE kind = 'rel'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .sql()?;
        let n = usize::try_from(n).unwrap_or(0);
        let text = usize::try_from(text).unwrap_or(0);
        let building = 2 * (text + n * (size_of::<String>() + size_of::<usize>()));
        let charge = budget.reserve(building as u64).map_err(budget_exceeded)?;
        let mut stmt = conn.prepare(REL_SQL).sql()?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).sql()?;
        Ok((Self::from_rows(rows.take(n), n)?, charge))
    }

    /// The dictionary of the `(ns, name)` rows of [`REL_SQL`] (about `n` of them).
    ///
    /// # Errors
    /// SQLite errors of a row; a stored namespace that is not one.
    pub(super) fn from_rows(
        rows: impl IntoIterator<Item = rusqlite::Result<(String, String)>>,
        n: usize,
    ) -> Result<Self> {
        let mut seen = HashSet::with_capacity(n);
        let mut names = Vec::with_capacity(n);
        for row in rows {
            let (ns, name) = row.sql()?;
            let ns = Ns::parse(&ns).map_err(|e| StoreError::Backend {
                source: BackendSource::new(e),
            })?;
            let stored = stored_name(&Qualified { ns, name });
            if seen.insert(stored.clone()) {
                names.push(stored);
            }
        }
        Ok(Self { names })
    }

    /// The id of the relation `rel`, `None` when it is not declared or has no id.
    pub(crate) fn get(&self, rel: &str) -> Option<RelId> {
        let rank = self.names.iter().position(|n| n == rel)?;
        u16::try_from(rank).ok().map(RelId)
    }

    /// The id of the relation `rel`.
    ///
    /// # Errors
    /// [`GraphRefused::UnknownRelation`]; [`GraphRefused::RelationIdExhausted`] when every id
    /// is taken before it.
    pub(super) fn id(&self, rel: &str) -> Result<RelId> {
        let rank = self
            .names
            .iter()
            .position(|n| n == rel)
            .ok_or_else(|| GraphRefused::UnknownRelation(rel.to_owned()))?;
        u16::try_from(rank)
            .map(RelId)
            .map_err(|_| GraphRefused::RelationIdExhausted(rel.to_owned()).into())
    }
}

/// A view's graph extents opened (validated) from a pin: the first as the base, the rest as
/// layers, oldest first.
#[derive(Debug)]
pub(super) struct Opened {
    base: Option<Base>,
    layers: Vec<Layer>,
    _charge: Option<BudgetLease>,
}

impl Opened {
    /// Open `chain` (graph extents of a view the pin loaded, oldest first) under `budget`.
    ///
    /// # Errors
    /// The pin's, the budget's and the graph format's errors.
    pub(super) fn open(pin: &Pin, chain: &[ExtentRef], budget: &QueryBudget) -> Result<Self> {
        let charge = chain.len() * (size_of::<Layer>() + size_of::<Base>());
        let charge = budget.reserve(charge as u64).map_err(budget_exceeded)?;
        let mut base = None;
        let mut layers = Vec::with_capacity(chain.len().saturating_sub(1));
        for (i, e) in chain.iter().enumerate() {
            let bytes = pin.bytes(e)?;
            if i == 0 {
                base = Some(Base::open(bytes, budget).map_err(graph_error)?);
            } else {
                layers.push(Layer::open(bytes, budget).map_err(graph_error)?);
            }
        }
        Ok(Self {
            base,
            layers,
            _charge: Some(charge),
        })
    }

    /// The base, `None` for an empty chain.
    pub(super) fn base(&self) -> Option<&Base> {
        self.base.as_ref()
    }

    /// The records these extents hold (`Base::records`, `Layer::records`).
    pub(super) fn records(&self) -> u64 {
        self.base.as_ref().map_or(0, Base::records)
            + self.layers.iter().map(Layer::records).sum::<u64>()
    }

    /// Whether any of these extents names `n` (a removal included).
    pub(super) fn mentions(&self, n: LocalId) -> bool {
        self.base.as_ref().is_some_and(|b| b.mentions(n))
            || self.layers.iter().any(|l| l.mentions(n))
    }

    /// The view of these extents over `sediment`.
    ///
    /// # Errors
    /// The budget's; a chain whose newest union misses a layer's node.
    pub(super) fn view<'a>(
        &'a self,
        sediment: &'a PreparedSediment,
        budget: &QueryBudget,
    ) -> Result<View<'a>> {
        View::new(self.base.as_ref(), &self.layers, sediment, budget).map_err(graph_error)
    }
}

/// The graph extents of `view`'s chain, oldest first.
fn graph_chain(view: &PinnedView) -> Vec<ExtentRef> {
    view.chain()
        .iter()
        .filter(|e| CLASSES.contains(&e.class))
        .cloned()
        .collect()
}

/// The graph extents of `main`'s chain at `pin`, which must have loaded `main` already (as
/// compaction's pins do).
///
/// # Errors
/// `OutsidePin` when the pin has not loaded `main` for the graph's classes.
pub(super) fn main_chain(pin: &Pin) -> Result<Vec<ExtentRef>> {
    let main = pin
        .loaded(&ViewName::Main, CLASSES)
        .ok_or_else(|| StoreError::Backend {
            source: BackendSource::new(OutsidePin),
        })?;
    Ok(graph_chain(&main))
}

/// One incarnation of a container file: its device, inode and birth time, read when a load
/// keeps an entry and again on every reuse. A database recreated at the same path (its
/// container with it, as every open creates the container beside a new database) has a new
/// container file whose birth time differs even where the file system reuses the inode. Where
/// the file system records no birth time, device and inode alone identify it. A database
/// recreated beside a container kept from the old one is not told apart (the container is
/// then foreign to it: verification reports its old ranges as leaks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Incarnation {
    dev: u64,
    ino: u64,
    born: Option<SystemTime>,
}

impl Incarnation {
    /// The incarnation of the container at `kbx`, `None` when it cannot be read (nothing is
    /// cached then).
    fn of(kbx: &Path) -> Option<Self> {
        let meta = std::fs::symlink_metadata(kbx).ok()?;
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            born: meta.created().ok(),
        })
    }
}

/// One view's opened state as the newest generation that loaded it left it. A clone clones
/// handles only.
#[derive(Debug, Clone)]
struct Entry {
    key: Arc<Key>,
    generation: Generation,
    opened: Arc<Opened>,
    /// The batch number of `key.seqs[0]` in `sediment`.
    first_batch: usize,
    sediment: Arc<PreparedSediment>,
    /// What a seal from `sediment` left, to read over `key.chain` plus the sealed layer.
    rest: Option<Arc<Rest>>,
    used: Instant,
}

/// What an entry names and lists besides its opened extents and sediment, with its charges to
/// the budget of the load that kept it (the entry's own slot included), released when the cache
/// and every clone have dropped it.
#[derive(Debug)]
struct Key {
    kbx: PathBuf,
    incarnation: Incarnation,
    view: String,
    chain: Vec<ExtentId>,
    /// The pending rows `sediment` holds, oldest first.
    seqs: Vec<SedimentSeq>,
    _seqs: Option<BudgetLease>,
    _charge: BudgetLease,
}

/// The sediment a seal left over, with the chain it sealed over, charged to the seal's budget.
#[derive(Debug)]
struct Rest {
    over: Vec<ExtentId>,
    seqs: Vec<SedimentSeq>,
    first_batch: usize,
    sediment: Arc<PreparedSediment>,
    _charge: BudgetLease,
}

/// The process's cache of opened views (see the module documentation).
static CACHE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// A test's skew of the cache's clock, in milliseconds ([`advance_clock`]).
#[cfg(any(test, feature = "failpoints"))]
static SKEW_MS: AtomicU64 = AtomicU64::new(0);

/// The cache's clock: now, plus a test's skew.
#[cfg(any(test, feature = "failpoints"))]
fn now() -> Instant {
    Instant::now() + Duration::from_millis(SKEW_MS.load(Ordering::Relaxed))
}

/// The cache's clock.
#[cfg(not(any(test, feature = "failpoints")))]
fn now() -> Instant {
    Instant::now()
}

/// Whether `e` has gone unused for [`IDLE`].
fn idle(e: &Entry) -> bool {
    now().saturating_duration_since(e.used) >= IDLE
}

/// Drop every entry idle past [`IDLE`] (and the pin locks its bytes hold). Every commit of a
/// writer runs it while it stages ([`super::GraphClass`]'s `stage`), so a process that writes
/// but no longer loads graphs still lets maintenance reclaim the extents its idle entries
/// held; a lookup sweeps too.
pub(super) fn sweep() {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|e| !idle(e));
}

/// Advance the cache's clock by `by`, without a real wait (tests only).
#[cfg(any(test, feature = "failpoints"))]
pub(crate) fn advance_clock(by: Duration) {
    let ms = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
    SKEW_MS.fetch_add(ms, Ordering::Relaxed);
}

/// Whether the cache holds an entry of the container `kbx` (tests only).
#[cfg(any(test, feature = "failpoints"))]
pub(crate) fn cache_holds(kbx: &Path) -> bool {
    let cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.iter().any(|e| e.key.kbx == kbx)
}

/// The incarnation of `pin`'s container, `None` for a pin without one (or one that cannot be
/// read): such a pin's loads are not cached.
fn incarnation(pin: &Pin) -> Option<Incarnation> {
    if pin.kbx_path().as_os_str().is_empty() {
        return None;
    }
    Incarnation::of(pin.kbx_path())
}

/// The cache entry of `pin`'s container (at `incarnation`) and `view` (a clone of its
/// handles). Entries idle past [`IDLE`], and an entry of another incarnation of the container,
/// are dropped first.
fn cached(pin: &Pin, incarnation: Incarnation, view: &str) -> Option<Entry> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|e| !idle(e) && (e.key.kbx != pin.kbx_path() || e.key.incarnation == incarnation));
    cache
        .iter()
        .find(|e| e.key.kbx == pin.kbx_path() && e.key.view == view)
        .cloned()
}

/// Keep `entry` as its container's and view's entry, unless the cache already holds a newer
/// generation of the same incarnation of them.
fn keep(entry: Entry) {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    match cache
        .iter_mut()
        .find(|e| e.key.kbx == entry.key.kbx && e.key.view == entry.key.view)
    {
        Some(e)
            if e.key.incarnation == entry.key.incarnation && e.generation > entry.generation => {}
        Some(e) => *e = entry,
        None => cache.push(entry),
    }
}

/// `prior` extended with the rows of `payloads` (oldest first), under `budget`.
fn extend(
    prior: &PreparedSediment,
    payloads: &[Vec<u8>],
    budget: &QueryBudget,
) -> Result<PreparedSediment> {
    let decoded = payloads
        .iter()
        .map(|p| Records::decode(p, budget))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(graph_error)?;
    prior.extend(&decoded, budget).map_err(graph_error)
}

/// Seal `rows` (main's pending graph rows, oldest first) over `parent` (the graph chain `chain`
/// at `pin`) from the cached prepared sediment, when it is this generation's and holds exactly
/// these rows first; the rest is kept for the generation the seal publishes. `None` when the
/// cache cannot serve the seal.
///
/// # Errors
/// The budget's and the graph's errors.
pub(super) fn seal_cached(
    pin: &Pin,
    chain: &[ExtentRef],
    rows: &[&SedimentRow],
    parent: &View<'_>,
    budget: &QueryBudget,
) -> Result<Option<Vec<u8>>> {
    let main = ViewName::Main.to_string();
    let ids: Vec<ExtentId> = chain.iter().map(|e| e.id).collect();
    let seqs: Vec<SedimentSeq> = rows.iter().map(|r| r.seq).collect();
    let Some(mut entry) = incarnation(pin).and_then(|i| cached(pin, i, &main)) else {
        return Ok(None);
    };
    if entry.generation != pin.generation()
        || entry.key.chain != ids
        || !entry.key.seqs.starts_with(&seqs)
    {
        return Ok(None);
    }
    let upto = entry.first_batch + seqs.len();
    let sealed = entry
        .sediment
        .seal(parent, upto, budget)
        .map_err(graph_error)?;
    if sealed.through != upto {
        return Ok(None);
    }
    let left = entry.key.seqs.get(seqs.len()..).unwrap_or_default();
    let listed = (ids.len() + left.len()) * size_of::<SedimentSeq>();
    let charge = budget
        .reserve((size_of::<Rest>() + listed) as u64)
        .map_err(budget_exceeded)?;
    entry.rest = Some(Arc::new(Rest {
        over: ids,
        seqs: left.to_vec(),
        first_batch: upto,
        sediment: Arc::new(sealed.rest),
        _charge: charge,
    }));
    keep(entry);
    Ok(Some(sealed.layer))
}

/// Load `view`'s graph at `pin` through its own transaction `rows` (spec 11 7.5; consumed by
/// T14 and T16): its chain opened, its pending graph rows prepared once per snapshot and
/// reused across queries (see the module documentation), and the relation dictionary, all
/// charged to `budget` (PC-17).
///
/// # Errors
/// The pin's (`OutsidePin` among them), the budget's, the graph format's and SQLite's errors.
pub(crate) fn load_view<'t>(
    rows: &'t Transaction<'_>,
    pin: &Pin,
    view: &ViewName,
    budget: &QueryBudget,
) -> Result<LoadedView<'t>> {
    // the chain alone: the base class stages no sediment, so the pin copies no payload; the
    // pending graph rows are read here, and only those the cache has not prepared yet
    let pinned = pin.load_view(rows, view, &[ExtentClass::GraphBase])?;
    let chain = graph_chain(&pinned);
    let ids: Vec<ExtentId> = chain.iter().map(|e| e.id).collect();
    let (seqs, seqs_charge) = pending_seqs(rows, view, budget)?;
    let name = view.to_string();
    let incarnation = incarnation(pin);
    let prior = incarnation.and_then(|i| cached(pin, i, &name));
    let same = prior
        .as_ref()
        .filter(|e| e.generation == pin.generation() && e.key.chain == ids);
    let opened = match same {
        Some(e) => Arc::clone(&e.opened),
        None => Arc::new(Opened::open(pin, &chain, budget)?),
    };
    // the prepared sediment to carry forward: the cached one, or a seal's leftover
    let carried = prior.as_ref().and_then(|e| {
        if e.key.chain == ids && seqs.starts_with(&e.key.seqs) {
            return Some((Arc::clone(&e.sediment), e.first_batch, e.key.seqs.len()));
        }
        e.rest
            .as_ref()
            .filter(|r| ids.len() == r.over.len() + 1 && ids.starts_with(&r.over))
            .filter(|r| seqs.starts_with(&r.seqs))
            .map(|r| (Arc::clone(&r.sediment), r.first_batch, r.seqs.len()))
    });
    let (prepared, first_batch, held) =
        carried.unwrap_or_else(|| (Arc::new(PreparedSediment::empty()), 0, 0));
    let sediment = match seqs.get(held) {
        None => prepared,
        Some(&from) => {
            let (payloads, _charge) = payloads_from(rows, from, budget)?;
            Arc::new(extend(&prepared, &payloads, budget)?)
        }
    };
    let (rels, rels_charge) = Rels::read(rows, budget)?;
    if let Some(incarnation) = incarnation {
        // the entry's slot, its key and what the key names and lists
        let held = size_of::<Entry>()
            + size_of::<Key>()
            + pin.kbx_path().as_os_str().len()
            + name.len()
            + ids.len() * size_of::<ExtentId>();
        let charge = budget.reserve(held as u64).map_err(budget_exceeded)?;
        keep(Entry {
            key: Arc::new(Key {
                kbx: pin.kbx_path().to_path_buf(),
                incarnation,
                view: name,
                chain: ids,
                seqs,
                _seqs: seqs_charge,
                _charge: charge,
            }),
            generation: pin.generation(),
            opened: Arc::clone(&opened),
            first_batch,
            sediment: Arc::clone(&sediment),
            rest: same.and_then(|e| e.rest.clone()),
            used: now(),
        });
    }
    Ok(LoadedView {
        rows,
        opened,
        sediment,
        rels,
        _rels_charge: rels_charge,
    })
}

/// The pending graph sediment rows of `view` in the snapshot `rows` (only `main` has any), in
/// sequence order, with their charge to `budget`. Read from the indexes alone
/// ([`PENDING_SQL`]): no row's payload is touched.
///
/// # Errors
/// The budget's and SQLite's errors.
fn pending_seqs(
    rows: &Connection,
    view: &ViewName,
    budget: &QueryBudget,
) -> Result<(Vec<SedimentSeq>, Option<BudgetLease>)> {
    if *view != ViewName::Main {
        return Ok((Vec::new(), None));
    }
    let n: i64 = rows
        .query_row(
            &format!("SELECT count(*) {PENDING_SQL}"),
            [SEDIMENT.as_str()],
            |r| r.get(0),
        )
        .sql()?;
    let n = usize::try_from(n).unwrap_or(0);
    let charge = budget
        .reserve((n * size_of::<SedimentSeq>()) as u64)
        .map_err(budget_exceeded)?;
    let mut seqs = Vec::with_capacity(n);
    let mut stmt = rows
        .prepare(&format!("SELECT seq {PENDING_SQL} ORDER BY seq"))
        .sql()?;
    let mut found = stmt.query([SEDIMENT.as_str()]).sql()?;
    while let Some(r) = found.next().sql()? {
        if seqs.len() == n {
            break;
        }
        seqs.push(SedimentSeq(uint(r, 0).sql()?));
    }
    Ok((seqs, Some(charge)))
}

/// The payloads of the pending graph rows from `from` on in the snapshot `rows`, oldest
/// first, with their charge to `budget`.
///
/// # Errors
/// The budget's and SQLite's errors.
fn payloads_from(
    rows: &Connection,
    from: SedimentSeq,
    budget: &QueryBudget,
) -> Result<(Vec<Vec<u8>>, BudgetLease)> {
    let from = sql_int(from.0).sql()?;
    let (n, bytes): (i64, i64) = rows
        .query_row(
            "SELECT count(*), COALESCE(sum(length(payload)), 0) FROM cat_sediment
             WHERE dead IS NULL AND class = ?1 AND seq >= ?2",
            rusqlite::params![SEDIMENT.as_str(), from],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .sql()?;
    let n = usize::try_from(n).unwrap_or(0);
    let held = n * size_of::<Vec<u8>>() + usize::try_from(bytes).unwrap_or(0);
    let charge = budget.reserve(held as u64).map_err(budget_exceeded)?;
    let mut out = Vec::with_capacity(n);
    let mut stmt = rows
        .prepare(
            "SELECT payload FROM cat_sediment WHERE dead IS NULL AND class = ?1 AND seq >= ?2
             ORDER BY seq",
        )
        .sql()?;
    let mut found = stmt
        .query(rusqlite::params![SEDIMENT.as_str(), from])
        .sql()?;
    while let Some(r) = found.next().sql()? {
        if out.len() == n {
            break;
        }
        out.push(r.get::<_, Vec<u8>>(0).sql()?);
    }
    Ok((out, charge))
}

/// One view's graph at a pin, loaded through the pin's own transaction (spec 11 T11): the
/// opened extents, the prepared sediment and the relation dictionary. A local id never leaves
/// the store (INV-2): callers map it through [`Self::rid_to_node_id`] and
/// [`Self::node_id_to_rid`], which read the same snapshot.
#[derive(Debug)]
pub(crate) struct LoadedView<'t> {
    rows: &'t Connection,
    opened: Arc<Opened>,
    sediment: Arc<PreparedSediment>,
    rels: Rels,
    _rels_charge: BudgetLease,
}

impl LoadedView<'_> {
    /// The merged view (base, layers, sediment) for one query under `budget`.
    ///
    /// # Errors
    /// The budget's; a chain whose newest union misses a layer's node.
    pub(crate) fn view(&self, budget: &QueryBudget) -> Result<View<'_>> {
        self.opened.view(&self.sediment, budget)
    }

    /// The id of the declared relation `rel` (`None`: not declared, or no id left for it).
    pub(crate) fn rel_id(&self, rel: &str) -> Option<RelId> {
        self.rels.get(rel)
    }

    /// The public id of the present node `n`, `None` when no present node has it.
    ///
    /// # Errors
    /// SQLite errors; a stored id that is not a node id.
    pub(crate) fn rid_to_node_id(&self, n: LocalId) -> Result<Option<NodeId>> {
        let id: Option<String> = self
            .rows
            .query_row(
                "SELECT id FROM node WHERE rid = ?1 AND lifecycle <> ?2",
                rusqlite::params![n.0, PURGED],
                |r| r.get(0),
            )
            .optional()
            .sql()?;
        id.map(|id| {
            NodeId::parse(&id).map_err(|e| StoreError::Backend {
                source: BackendSource::new(e),
            })
        })
        .transpose()
    }

    /// The local id of the present node `id`, `None` when no present node has it.
    ///
    /// # Errors
    /// SQLite errors; a `rid` outside the local id range.
    pub(crate) fn node_id_to_rid(&self, id: &NodeId) -> Result<Option<LocalId>> {
        let rid: Option<i64> = self
            .rows
            .query_row(
                "SELECT rid FROM node WHERE id = ?1 AND lifecycle <> ?2",
                rusqlite::params![id.as_str(), PURGED],
                |r| r.get(0),
            )
            .optional()
            .sql()?;
        rid.map(super::local_id).transpose()
    }

    /// The public `NodeId` order of the present nodes (the tie order of the scored functions),
    /// prepared from the same snapshot and charged to `budget`.
    ///
    /// # Errors
    /// The budget's and SQLite's errors.
    pub(crate) fn stable_order(&self, budget: &QueryBudget) -> Result<NodeOrder> {
        let n: i64 = self
            .rows
            .query_row(
                "SELECT count(*) FROM node WHERE lifecycle <> ?1",
                [PURGED],
                |r| r.get(0),
            )
            .sql()?;
        let n = usize::try_from(n).unwrap_or(0);
        let lease = budget
            .reserve((n * size_of::<(u32, u32)>()) as u64)
            .map_err(budget_exceeded)?;
        let mut ranks = Vec::with_capacity(n);
        let mut stmt = self
            .rows
            .prepare("SELECT rid FROM node WHERE lifecycle <> ?1 ORDER BY id")
            .sql()?;
        let mut rows = stmt.query([PURGED]).sql()?;
        let mut rank = 0_u32;
        while let Some(r) = rows.next().sql()? {
            if ranks.len() == n {
                break;
            }
            if ranks.len() % 256 == 0 {
                budget.check().map_err(budget_exceeded)?;
            }
            ranks.push((super::local_id(r.get(0).sql()?)?.0, rank));
            rank += 1;
        }
        ranks.sort_unstable();
        Ok(NodeOrder {
            ranks,
            _lease: lease,
        })
    }
}

/// Local ids ranked in public `NodeId` order ([`LoadedView::stable_order`]).
#[derive(Debug)]
pub(crate) struct NodeOrder {
    /// `(local id, rank)`, sorted by local id.
    ranks: Vec<(u32, u32)>,
    _lease: BudgetLease,
}

impl StableOrder for NodeOrder {
    fn rank(&self, n: LocalId) -> std::result::Result<TieRank, GraphError> {
        self.ranks
            .binary_search_by_key(&n.0, |&(id, _)| id)
            .ok()
            .and_then(|i| self.ranks.get(i))
            .map(|&(_, rank)| TieRank(rank))
            .ok_or_else(|| GraphError::Corrupt("a node without a rank".into()))
    }
}
