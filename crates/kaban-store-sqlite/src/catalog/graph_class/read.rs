//! The pinned graph view loader (spec 11 T11; INV-2, PC-17, amendments 3.6, 4.1, 7.3, 7.5): a
//! view's graph chain opened from its pin, its pending graph sediment prepared once, the
//! relation dictionary, and the node id mapping through the pin's own transaction.
//!
//! ```text
//!   load_view(rows, pin, view) => pin.load_view (chain + pending rows, charged by the pin)
//!     => cache (kbx, view): same generation and chain? reuse the opened extents
//!                           chain unchanged, rows a superset? extend the prepared sediment
//!                           a seal's leftover fits? extend that; otherwise prepare afresh
//!     => LoadedView { opened extents, prepared sediment, relations, the transaction }
//! ```
//!
//! The cache (one entry per container and view, in this process) keeps what the newest
//! generation seen opened: validated extents are reused only at that generation (their bytes
//! hold that pin's lock, so an entry is replaced as soon as a newer generation loads, and an
//! entry idle for [`IDLE`] is dropped at the next load), while the prepared sediment, which holds
//! no bytes of the container, is carried forward and extended with the newer rows only
//! (amendment 3.6). What a load builds is charged to the budget of the query that builds it, and
//! stays charged while the cache keeps it; a query that reuses it is charged nothing more. A
//! store without a container (in memory) caches nothing.

#![cfg_attr(
    not(feature = "failpoints"),
    expect(dead_code, reason = "T14 and T16 load graph views")
)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kaban_core::ontology::{Ns, Qualified, stored_name};
use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_core::{BackendSource, NodeId, Result, StoreError, ViewName};
use kaban_index_graph::{
    Base, GraphError, Layer, LocalId, PreparedSediment, Records, RelId, StableOrder, TieRank, View,
};
use rusqlite::{Connection, OptionalExtension, Transaction};

use super::{CLASSES, GraphRefused, PURGED, SEDIMENT, graph_error};
use crate::catalog::pin::{OutsidePin, Pin, PinnedView, budget_exceeded};
use crate::catalog::tables::{ExtentId, ExtentRef, Generation, SedimentRow, SedimentSeq};
use crate::sql_error::SqlResultExt;

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
    /// The dictionary of the rows `conn` sees.
    ///
    /// # Errors
    /// SQLite errors; a stored namespace that is not one.
    pub(super) fn read(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare(REL_SQL).sql()?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .sql()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .sql()?;
        Self::from_rows(rows)
    }

    /// The dictionary of the `(ns, name)` rows of [`REL_SQL`].
    ///
    /// # Errors
    /// A stored namespace that is not one.
    pub(super) fn from_rows(rows: Vec<(String, String)>) -> Result<Self> {
        let mut seen = HashSet::with_capacity(rows.len());
        let mut names = Vec::with_capacity(rows.len());
        for (ns, name) in rows {
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

    /// The bytes the dictionary holds, for its charge.
    fn bytes(&self) -> u64 {
        let names: usize = self.names.iter().map(String::capacity).sum();
        (names + self.names.capacity() * size_of::<String>()) as u64
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

/// One view's opened state as the newest generation that loaded it left it.
#[derive(Debug, Clone)]
struct Entry {
    kbx: PathBuf,
    view: String,
    generation: Generation,
    chain: Vec<ExtentId>,
    opened: Arc<Opened>,
    /// The pending rows `sediment` holds, oldest first; the first is batch `first_batch`.
    seqs: Vec<SedimentSeq>,
    first_batch: usize,
    sediment: Arc<PreparedSediment>,
    /// What a seal from `sediment` left, to read over `chain` plus the sealed layer.
    rest: Option<Rest>,
    used: Instant,
}

/// The sediment a seal left over, with the chain it sealed over.
#[derive(Debug, Clone)]
struct Rest {
    over: Vec<ExtentId>,
    seqs: Vec<SedimentSeq>,
    first_batch: usize,
    sediment: Arc<PreparedSediment>,
}

/// The process's cache of opened views (see the module documentation).
static CACHE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// The cache entry of `pin`'s container and `view` (a clone of its handles), `None` for a pin
/// without a container. Entries idle past [`IDLE`] are dropped first.
fn cached(pin: &Pin, view: &str) -> Option<Entry> {
    if pin.kbx_path().as_os_str().is_empty() {
        return None;
    }
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|e| e.used.elapsed() < IDLE);
    cache
        .iter()
        .find(|e| e.kbx == pin.kbx_path() && e.view == view)
        .cloned()
}

/// Keep `entry` as its container's and view's entry, unless the cache already holds a newer
/// generation of them.
fn keep(entry: Entry) {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    match cache
        .iter_mut()
        .find(|e| e.kbx == entry.kbx && e.view == entry.view)
    {
        Some(e) if e.generation > entry.generation => {}
        Some(e) => *e = entry,
        None => cache.push(entry),
    }
}

/// `prior` extended with the rows `pending[from..]`, under `budget`.
fn extend(
    prior: &PreparedSediment,
    pending: &[&SedimentRow],
    budget: &QueryBudget,
) -> Result<PreparedSediment> {
    let decoded = pending
        .iter()
        .map(|r| Records::decode(&r.payload, budget))
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
    let Some(mut entry) = cached(pin, &main) else {
        return Ok(None);
    };
    if entry.generation != pin.generation() || entry.chain != ids || !entry.seqs.starts_with(&seqs)
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
    entry.rest = Some(Rest {
        over: ids,
        seqs: entry.seqs[seqs.len()..].to_vec(),
        first_batch: upto,
        sediment: Arc::new(sealed.rest),
    });
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
    let pinned = pin.load_view(rows, view, CLASSES)?;
    let chain = graph_chain(&pinned);
    let pending: Vec<&SedimentRow> = pinned
        .sediment()
        .iter()
        .filter(|r| r.class == SEDIMENT)
        .collect();
    let ids: Vec<ExtentId> = chain.iter().map(|e| e.id).collect();
    let seqs: Vec<SedimentSeq> = pending.iter().map(|r| r.seq).collect();
    let name = view.to_string();
    let prior = cached(pin, &name);
    let same = prior
        .as_ref()
        .filter(|e| e.generation == pin.generation() && e.chain == ids);
    let opened = match same {
        Some(e) => Arc::clone(&e.opened),
        None => Arc::new(Opened::open(pin, &chain, budget)?),
    };
    // the prepared sediment to carry forward: the cached one, or a seal's leftover
    let carried = prior.as_ref().and_then(|e| {
        if e.chain == ids && seqs.starts_with(&e.seqs) {
            return Some((Arc::clone(&e.sediment), e.first_batch, e.seqs.len()));
        }
        e.rest
            .as_ref()
            .filter(|r| ids.len() == r.over.len() + 1 && ids.starts_with(&r.over))
            .filter(|r| seqs.starts_with(&r.seqs))
            .map(|r| (Arc::clone(&r.sediment), r.first_batch, r.seqs.len()))
    });
    let (prepared, first_batch, held) =
        carried.unwrap_or_else(|| (Arc::new(PreparedSediment::empty()), 0, 0));
    let sediment = if held == pending.len() {
        prepared
    } else {
        Arc::new(extend(&prepared, &pending[held..], budget)?)
    };
    let rels = Rels::read(rows)?;
    let rels_charge = budget.reserve(rels.bytes()).map_err(budget_exceeded)?;
    if !pin.kbx_path().as_os_str().is_empty() {
        keep(Entry {
            kbx: pin.kbx_path().to_path_buf(),
            view: name,
            generation: pin.generation(),
            chain: ids,
            opened: Arc::clone(&opened),
            seqs,
            first_batch,
            sediment: Arc::clone(&sediment),
            rest: same.and_then(|e| e.rest.clone()),
            used: Instant::now(),
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
