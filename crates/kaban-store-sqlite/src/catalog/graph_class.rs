//! The memory graph's class on the catalog (spec 11 T11; GRA-02, GRA-03, KER-01, INV-2,
//! INV-4): every commit's node and edge transitions become one sediment row of graph records,
//! compaction seals that sediment into layers over `main`'s graph chain and folds the chain
//! into a base, and the rows rebuild the base from scratch (`read` loads a pinned view).
//!
//! ```text
//!   commit [Stage] => one GraphLayer sediment row: Records::encode(node + edge ops)
//!   compact        => seal: pending rows -> layer over the chain (the first seal -> base)
//!                  => fold: the whole chain from its first extent -> one base
//!   purge rids     => base: the whole chain folded without the rids; layers: empty layers
//!   rebuild        => node truth + edge rows of one snapshot -> base; old graph rows go
//! ```
//!
//! Mapping, fixed here (GRA-03): a node's local id is its `rid` (PC-1); it is present while
//! its row exists and is not `purged`, whatever its lifecycle or workspace (read eligibility is
//! the rows', not the graph's). A relation's id is its rank among the declared relations in
//! declaration order (`read::Rels`). An edge carries `Attr::weight = round(coalesce(
//! confidence, 1.0) * 1e6)`, never negative (amendment 3.1), and `Attr::ts = created_at` as
//! epoch milliseconds (RFC 3339); a value that maps to neither refuses the write by name.
//!
//! Main's graph chain always starts with a base (a seal over an empty chain writes its layer
//! flattened), so the first graph extent of a chain is opened as a base and the rest as layers,
//! whatever class name compaction gave them.

mod read;

#[cfg_attr(
    not(feature = "failpoints"),
    expect(unused_imports, reason = "T14 and T16 load graph views")
)]
pub(crate) use read::{LoadedView, NodeOrder, load_view};

use std::fs::File;
use std::os::unix::fs::FileExt;

use kaban_core::query_budget::{BudgetExceeded, BudgetLease, Limit, QueryBudget};
use kaban_core::{BackendSource, Durability, Result, StoreError, ViewName};
use kaban_index_graph::{
    Attr, DecodedRecords, EdgeOp, GraphError, Layer, LocalId, NodeOp, OpKind, PreparedSediment,
    Records, View, build_base, fold, seal,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tantivy::directory::OwnedBytes;

use super::Catalog;
use super::capture::{BatchChanges, CaptureOp, NodeImage};
use super::class::{ExtentClass, IndexClass};
use super::commit::Stage;
use super::kbx;
use super::pin::{Pin, budget_exceeded};
use super::tables::{ExtentRef, SedimentRow, sql_int, uint};
use crate::sql_error::SqlResultExt;

/// The classes of the graph's extents: compaction names a seal's extent after the first.
const CLASSES: &[ExtentClass] = &[ExtentClass::GraphLayer, ExtentClass::GraphBase];

/// The class of the graph's sediment rows.
const SEDIMENT: ExtentClass = ExtentClass::GraphLayer;

/// The lifecycle of a purged node's ghost row: absent from the graph.
const PURGED: &str = "purged";

/// What the graph engine holds in memory per record a fold, a purge or a base build handles
/// (a node's presence, an edge, a fact): its maps, section vectors and output buffer. Wave 3
/// amendment 3.2 measured about 300 B per edge for a fold; a node or a fact is charged the same,
/// so the estimate errs high.
const ENGINE_BYTES_PER_RECORD: u64 = 300;

/// Reserve the engine's working set for `records` on `budget`, before the engine runs: a fold
/// or build that does not fit is refused by the budget instead of outgrowing it.
fn reserve_engine(budget: &QueryBudget, records: u64) -> Result<BudgetLease> {
    budget
        .reserve(records.saturating_mul(ENGINE_BYTES_PER_RECORD))
        .map_err(budget_exceeded)
}

/// The memory graph's class.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GraphClass;

impl GraphClass {
    /// The class every writer catalog registers (`super::classes`).
    pub(crate) fn new() -> Self {
        Self
    }

    /// Rebuild `main`'s graph from the rows (INV-4, KER-08), in one commit on the writer
    /// connection `conn`: a base built from node truth (every row not `purged`, isolated ones
    /// included) and every edge row of that snapshot replaces the graph's extents and pending
    /// sediment. The build's working set is reserved on the maintenance budget before any row
    /// is read ([`ENGINE_BYTES_PER_RECORD`] per node and edge). An old extent whose bytes are still intact in the container retires (and is
    /// reclaimed later); one whose bytes are gone (a deleted `kaban.kbx`) loses its rows, so no
    /// reclamation ever releases a range the new container reuses. A catalog without a
    /// container (in memory) holds only sediment: nothing to rebuild.
    ///
    /// # Errors
    /// The mapping's refusals ([`GraphRefused`]); `StoreError::BudgetExceeded` when the build
    /// does not fit the maintenance budget; container and SQLite errors.
    pub(crate) fn rebuild_main(catalog: &Catalog, conn: &mut Connection) -> Result<()> {
        if catalog.container.is_none() {
            return Ok(());
        }
        let maintenance = catalog.maintenance_budget();
        let budget = QueryBudget::new(maintenance.memory_bytes, Some(maintenance.deadline));
        catalog.commit(conn, Durability::Normal, |txn| {
            let rows = txn.conn();
            let records: i64 = rows
                .query_row(
                    "SELECT (SELECT count(*) FROM node WHERE lifecycle <> ?1)
                          + (SELECT count(*) FROM edge)",
                    [PURGED],
                    |r| r.get(0),
                )
                .sql()?;
            let _engine = reserve_engine(&budget, u64::try_from(records).unwrap_or(0))?;
            let (rels, _rels) = read::Rels::read(rows, &budget)?;
            let mut records = Records::default();
            {
                let mut stmt = rows
                    .prepare("SELECT rid FROM node WHERE lifecycle <> ?1 ORDER BY rid")
                    .sql()?;
                let rids = stmt.query_map([PURGED], |r| r.get::<_, i64>(0)).sql()?;
                for rid in rids {
                    records.nodes.push(NodeOp {
                        node: local_id(rid.sql()?)?,
                        kind: OpKind::Add,
                    });
                }
                let mut stmt = rows
                    .prepare("SELECT src, dst, rel, confidence, created_at FROM edge")
                    .sql()?;
                let mut edges = stmt.query([]).sql()?;
                while let Some(r) = edges.next().sql()? {
                    let rel: String = r.get(2).sql()?;
                    let created_at: String = r.get(4).sql()?;
                    records.edges.push(EdgeOp {
                        src: local_id(r.get(0).sql()?)?,
                        rel: rels.id(&rel)?,
                        dst: local_id(r.get(1).sql()?)?,
                        kind: OpKind::Add,
                        attr: attr(r.get(3).sql()?, &created_at)?,
                    });
                }
            }
            let base = build_base(&records, 0);
            let generation = sql_int(txn.generation()?.0).sql()?;
            retire_graph_rows(&catalog.kbx, txn.conn(), generation)?;
            let extent = catalog.append_extent(ExtentClass::GraphBase, &base)?;
            txn.publish(extent, &[ViewName::Main])?;
            Ok(())
        })
    }
}

impl IndexClass for GraphClass {
    fn classes(&self) -> &[ExtentClass] {
        CLASSES
    }

    /// One sediment row of this commit's graph records, in capture order: a node that comes
    /// into presence (inserted, not `purged`) is a node add, one that leaves it (deleted or
    /// purged) a node removal; an inserted or updated edge is an edge add with its row's
    /// attributes (an edge gone again by the end of the commit adds nothing), a deleted one an
    /// edge removal. Losing an edge never removes a node. Nothing for a commit without any.
    fn stage(&self, txn: &mut Stage<'_>, changes: &BatchChanges) -> Result<()> {
        let mut records = Records::default();
        for n in changes.nodes() {
            let present = |i: &Option<NodeImage>| i.as_ref().is_some_and(|i| i.lifecycle != PURGED);
            let kind = match (present(&n.before), present(&n.after)) {
                (false, true) => OpKind::Add,
                (true, false) => OpKind::Remove,
                _ => continue,
            };
            records.nodes.push(NodeOp {
                node: local_id(n.rid)?,
                kind,
            });
        }
        if !changes.edges().is_empty() {
            let reader = txn.reader();
            let rels = {
                let mut stmt = reader.prepare(read::REL_SQL)?;
                let rows: Vec<(String, String)> =
                    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                let n = rows.len();
                read::Rels::from_rows(rows.into_iter().map(Ok), n)?
            };
            let mut row = reader.prepare(
                "SELECT confidence, created_at FROM edge WHERE src = ?1 AND dst = ?2 AND rel = ?3",
            )?;
            for e in changes.edges() {
                let (kind, attr) = match e.op {
                    CaptureOp::Delete => (OpKind::Remove, Attr::default()),
                    CaptureOp::Insert | CaptureOp::Update => {
                        let found = row.query_row(rusqlite::params![e.src, e.dst, e.rel], |r| {
                            Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, String>(1)?))
                        })?;
                        let Some((confidence, created_at)) = found else {
                            continue;
                        };
                        (OpKind::Add, attr(confidence, &created_at)?)
                    }
                };
                records.edges.push(EdgeOp {
                    src: local_id(e.src)?,
                    rel: rels.id(&e.rel)?,
                    dst: local_id(e.dst)?,
                    kind,
                    attr,
                });
            }
        }
        if records.nodes.is_empty() && records.edges.is_empty() {
            return Ok(());
        }
        txn.put_sediment(SEDIMENT, &records.encode())?;
        Ok(())
    }
    /// Due once the node and edge operations the rows hold (read from each row's header, not
    /// decoded) reach `ops`.
    fn seal_due(&self, rows: &[&SedimentRow], ops: u32) -> Result<bool> {
        let mut held = 0_u64;
        for r in rows.iter().filter(|r| r.class == SEDIMENT) {
            held += Records::count_ops(&r.payload).map_err(graph_error)?;
            if held >= u64::from(ops) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The layer of `rows` (main's pending graph rows, oldest first) over main's graph chain at
    /// the pin; over an empty chain, that layer flattened into a base. Seals from the cached
    /// prepared sediment when it holds exactly these rows first (amendment 3.6), otherwise
    /// prepares them under `budget`. A node the rows name that the chain does not hold and the
    /// rows do not leave added (a purged one) is left out with its edges ([`vanished`]), so its
    /// id never returns into an extent.
    fn seal(
        &self,
        pin: &Pin,
        rows: &[&SedimentRow],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let rows: Vec<&SedimentRow> = rows
            .iter()
            .copied()
            .filter(|r| r.class == SEDIMENT)
            .collect();
        if rows.is_empty() {
            return Ok(None);
        }
        // the layer's bytes, about the size of the rows it seals
        let held: usize = rows.iter().map(|r| r.payload.len()).sum();
        let _held = budget.reserve(held as u64).map_err(budget_exceeded)?;
        let chain = read::main_chain(pin)?;
        let opened = read::Opened::open(pin, &chain, budget)?;
        let empty = PreparedSediment::empty();
        let parent = opened.view(&empty, budget)?;
        let decoded = rows
            .iter()
            .map(|r| Records::decode(&r.payload, budget))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(graph_error)?;
        let gone = vanished(&decoded, &parent);
        let cached = if gone.is_empty() {
            read::seal_cached(pin, &chain, &rows, &parent, budget)?
        } else {
            None
        };
        let layer = if let Some(layer) = cached {
            layer
        } else {
            let kept = |n: LocalId| gone.binary_search(&n).is_err();
            let batches: Vec<Records> = decoded
                .iter()
                .map(|d| {
                    let r = d.records();
                    Records {
                        edges: r
                            .edges
                            .iter()
                            .filter(|e| kept(e.src) && kept(e.dst))
                            .copied()
                            .collect(),
                        nodes: r.nodes.iter().filter(|o| kept(o.node)).copied().collect(),
                        facts: r.facts.iter().filter(|f| kept(f.node)).cloned().collect(),
                    }
                })
                .collect();
            let prepared = PreparedSediment::prepare(&batches, budget).map_err(graph_error)?;
            prepared
                .seal(&parent, batches.len(), budget)
                .map_err(graph_error)?
                .layer
        };
        if !chain.is_empty() {
            return Ok(Some(layer));
        }
        // the first extent of a chain is its base
        let layer = [Layer::open(OwnedBytes::new(layer), budget).map_err(graph_error)?];
        let _engine = reserve_engine(budget, layer.iter().map(Layer::records).sum())?;
        let view = View::new(None, &layer, &empty, budget).map_err(graph_error)?;
        Ok(Some(fold(&view, &[])))
    }

    /// The base of the graph chain run `extents` (oldest first) when it starts at the chain's
    /// first extent; `None` otherwise (a run that keeps an older graph extent below it cannot
    /// become a base, and a layer is never rewritten alone). The fold's working set is reserved
    /// on `budget` before it runs ([`ENGINE_BYTES_PER_RECORD`] per record of the extents).
    fn fold(
        &self,
        pin: &Pin,
        extents: &[ExtentRef],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let chain = read::main_chain(pin)?;
        if extents.len() < 2 || chain.get(..extents.len()) != Some(extents) {
            return Ok(None);
        }
        let opened = read::Opened::open(pin, extents, budget)?;
        let _engine = reserve_engine(budget, opened.records())?;
        let empty = PreparedSediment::empty();
        let view = opened.view(&empty, budget)?;
        Ok(Some(fold(&view, &[])))
    }

    /// `extent` rewritten without the purged `rids` (explicit omission, amendment 7.8) when it
    /// is in main's graph chain and some extent of that chain names one of them; `None`
    /// otherwise. The chain's base becomes the whole chain folded with `omit` = the rids, and
    /// each layer above it an empty layer at its depth: the new base already holds what the
    /// layers held, and a layer is replaced one for one.
    fn purge(
        &self,
        pin: &Pin,
        extent: &ExtentRef,
        rids: &[i64],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>> {
        let chain = read::main_chain(pin)?;
        let Some(at) = chain.iter().position(|e| e == extent) else {
            return Ok(None);
        };
        let omit: Vec<LocalId> = rids.iter().filter_map(|&r| local_id(r).ok()).collect();
        let opened = read::Opened::open(pin, &chain, budget)?;
        if !omit.iter().any(|&n| opened.mentions(n)) {
            return Ok(None);
        }
        let empty = PreparedSediment::empty();
        if at == 0 {
            let _engine = reserve_engine(budget, opened.records())?;
            let view = opened.view(&empty, budget)?;
            return Ok(Some(fold(&view, &omit)));
        }
        let mut layers: Vec<Layer> = Vec::with_capacity(at);
        let mut layer = Vec::new();
        for _ in 0..at {
            let parent = View::new(opened.base(), &layers, &empty, budget).map_err(graph_error)?;
            layer = seal(&parent, &Records::default());
            layers.push(Layer::open(OwnedBytes::new(layer.clone()), budget).map_err(graph_error)?);
        }
        Ok(Some(layer))
    }

    /// [`GraphClass::rebuild_main`].
    fn rebuild(&self, catalog: &Catalog, rows: &mut Connection) -> Result<()> {
        Self::rebuild_main(catalog, rows)
    }
}

/// Why the graph refused a write: the mapping of a row into graph records failed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GraphRefused {
    /// An edge's `confidence` maps to no non-negative weight (amendment 3.1).
    Weight(f64),
    /// An edge's `created_at` is not an RFC 3339 time.
    Timestamp(String),
    /// A node's `rid` does not fit a local id.
    LocalId(i64),
    /// An edge names a relation the ontology does not declare.
    UnknownRelation(String),
    /// Every relation id is taken: the relation has none.
    RelationIdExhausted(String),
}

impl std::fmt::Display for GraphRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Weight(v) => write!(
                f,
                "graph_weight_refused: edge attribute `confidence` = {v} maps to no non-negative weight"
            ),
            Self::Timestamp(v) => write!(
                f,
                "graph_timestamp_refused: edge attribute `created_at` = {v:?} is not an RFC 3339 time"
            ),
            Self::LocalId(rid) => write!(
                f,
                "graph_local_id_refused: node rid {rid} does not fit a local id"
            ),
            Self::UnknownRelation(rel) => {
                write!(
                    f,
                    "graph_relation_unknown: relation {rel:?} is not declared"
                )
            }
            Self::RelationIdExhausted(rel) => write!(
                f,
                "graph_relation_ids_exhausted: relation {rel:?} has no id (all {} are in use)",
                u32::from(u16::MAX) + 1
            ),
        }
    }
}

impl std::error::Error for GraphRefused {}

impl From<GraphRefused> for StoreError {
    fn from(e: GraphRefused) -> Self {
        Self::Backend {
            source: BackendSource::new(e),
        }
    }
}

/// `e` as the store's error; a budget refusal is the store's own
/// [`StoreError::BudgetExceeded`], so compaction treats it as a skip and retries smaller.
pub(crate) fn graph_error(e: GraphError) -> StoreError {
    match e {
        GraphError::BudgetExceeded { limit, value } => {
            let limit = match limit {
                "time" => Limit::Time,
                "cancelled" => Limit::Cancelled,
                _ => Limit::Memory,
            };
            StoreError::BudgetExceeded(BudgetExceeded { limit, value })
        }
        other => StoreError::Backend {
            source: BackendSource::new(other),
        },
    }
}

/// The nodes `batches` (one seal unit, oldest first) name that `parent` does not hold and
/// whose newest node op in the unit is not an add (a removal, or none at all): sorted. Such a
/// node is purged or about to be: an edge row needs both of its nodes, and a unit is always the
/// oldest pending rows, so a live node is either in the chain or added by the unit before its
/// edges. Its ops change nothing the chain holds, so a seal leaves it, its edges and its facts
/// out, whichever unit holds them: a unit that ends with its edge, before the row that removes
/// it, never writes the purged id back into a layer (the rule holds per unit, so no split of
/// the pending rows resurrects it).
fn vanished(batches: &[DecodedRecords], parent: &View<'_>) -> Vec<LocalId> {
    let mut newest = std::collections::BTreeMap::new();
    for b in batches {
        let r = b.records();
        for e in &r.edges {
            newest.entry(e.src).or_insert(None);
            newest.entry(e.dst).or_insert(None);
        }
        for f in &r.facts {
            newest.entry(f.node).or_insert(None);
        }
        for op in &r.nodes {
            newest.insert(op.node, Some(op.kind));
        }
    }
    newest
        .into_iter()
        .filter(|&(n, kind)| kind != Some(OpKind::Add) && !parent.contains(n))
        .map(|(n, _)| n)
        .collect()
}

/// The local id of the node row `rid` (PC-1).
fn local_id(rid: i64) -> Result<LocalId> {
    u32::try_from(rid)
        .map(LocalId)
        .map_err(|_| GraphRefused::LocalId(rid).into())
}

/// The attributes of an edge row (GRA-03): its weight from `confidence` (1.0 when unset) in
/// millionths, refused when negative or out of range, and its time from `created_at`.
fn attr(confidence: Option<f64>, created_at: &str) -> Result<Attr> {
    let c = confidence.unwrap_or(1.0);
    let scaled = (c * 1e6).round();
    // 2^63: no i64 holds it, every smaller integral f64 fits
    #[expect(
        clippy::cast_precision_loss,
        reason = "the bound itself, 2^63, is exact"
    )]
    let bound = i64::MAX as f64;
    if !(0.0..bound).contains(&scaled) {
        return Err(GraphRefused::Weight(c).into());
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "an integral value checked in range"
    )]
    let weight = scaled as i64;
    let ts =
        rfc3339_ms(created_at).ok_or_else(|| GraphRefused::Timestamp(created_at.to_owned()))?;
    Ok(Attr { weight, ts })
}

/// `s` as epoch milliseconds when it is an RFC 3339 date-time (`YYYY-MM-DDTHH:MM:SS`, an
/// optional fraction, then `Z` or `±HH:MM`); `None` otherwise. Digits below a millisecond are
/// truncated.
#[expect(
    clippy::many_single_char_names,
    reason = "the date fields and the civil-date algorithm's own names"
)]
fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        b.get(r)?.iter().try_fold(0_i64, |acc, d| {
            d.is_ascii_digit().then(|| acc * 10 + i64::from(d - b'0'))
        })
    };
    let at = |i: usize, c: &[u8]| b.get(i).is_some_and(|x| c.contains(x));
    if !(at(4, b"-") && at(7, b"-") && at(10, b"Tt ") && at(13, b":") && at(16, b":")) {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut ms = 0;
    if at(i, b".") {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let digits = (i - start).min(3);
        if digits == 0 {
            return None;
        }
        ms = num(start..start + digits)? * 10_i64.pow(u32::try_from(3 - digits).ok()?);
    }
    let offset_min = match b.get(i..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let m = num(i + 1..i + 3)? * 60 + num(i + 4..i + 6)?;
            if *sign == b'+' { m } else { -m }
        }
        _ => return None,
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=month_days).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // days from the civil date (Howard Hinnant's algorithm)
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3_600 + mi * 60 + sec - offset_min * 60;
    Some(secs * 1_000 + ms)
}

/// One graph extent row as the rebuild judges it: id, range, content hash and liveness.
type ExtentRow = (u64, Option<(u64, u64)>, Option<Vec<u8>>, bool);

/// Take every graph extent row and pending graph sediment row out of the catalog in the open
/// writer transaction on `rows` (generation `generation`): chains and claims lose them, an
/// extent whose bytes are intact in the container `kbx_path` retires, one whose bytes are not
/// loses its row, and the pending sediment is consumed.
fn retire_graph_rows(kbx_path: &std::path::Path, rows: &Connection, generation: i64) -> Result<()> {
    let extents: Vec<ExtentRow> = {
        let mut stmt = rows
            .prepare(
                "SELECT id, kbx_offset, kbx_len, content_sha256, dead_since IS NULL
                 FROM cat_extent WHERE class IN (?1, ?2)",
            )
            .sql()?;
        let found = stmt
            .query_map([CLASSES[0].as_str(), CLASSES[1].as_str()], |r| {
                let range = match r.get::<_, Option<i64>>(1)? {
                    Some(_) => Some((uint(r, 1)?, uint(r, 2)?)),
                    None => None,
                };
                Ok((uint(r, 0)?, range, r.get(3)?, r.get(4)?))
            })
            .sql()?;
        found.collect::<rusqlite::Result<_>>().sql()?
    };
    let file = kbx::open_existing(kbx_path).ok();
    for (id, range, sha, live) in extents {
        let intact = match (&file, range, sha) {
            (Some(file), Some((offset, len)), Some(sha)) => holds(file, offset, len, &sha),
            _ => false,
        };
        let id = sql_int(id).sql()?;
        rows.execute("DELETE FROM cat_view_extent WHERE extent_id = ?1", [id])
            .sql()?;
        rows.execute("DELETE FROM cat_claim WHERE extent = ?1", [id])
            .sql()?;
        if !intact {
            rows.execute("DELETE FROM cat_extent WHERE id = ?1", [id])
                .sql()?;
        } else if live {
            rows.execute(
                "UPDATE cat_extent SET dead_since = ?1 WHERE id = ?2",
                rusqlite::params![generation, id],
            )
            .sql()?;
        }
    }
    rows.execute(
        "UPDATE cat_sediment SET dead = ?1 WHERE dead IS NULL AND class = ?2",
        rusqlite::params![generation, SEDIMENT.as_str()],
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
