//! Statement-local search and graph scans.
use super::{guard::Guard, refused};
use crate::SqliteStore;
use crate::catalog::{graph_class, pin::Pin, text_class::TextInputs};
use crate::sql_error::SqlResultExt;
use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_core::text_index::{MatchMode, Searched, SeedFilter, TextHit, TextQuery};
use kaban_core::{NodeId, Result, StoreError, ViewName};
use kaban_index_graph::{Caps, Charged, Direction, Reached, RelSelection};
use kaban_vtab::{
    Arg, ColumnDef, ColumnKind, ExecutionPayload, FnError, Plan, PlanInput, QueryConnection, Scan,
    Value,
};
use rusqlite::Transaction;
use std::cell::{Cell, OnceCell, RefCell};

const fn column(name: &'static str, kind: ColumnKind, hidden: bool) -> ColumnDef {
    ColumnDef {
        name,
        kind,
        argument: if hidden {
            Some(Arg { required: true })
        } else {
            None
        },
    }
}

pub(super) fn register(conn: &QueryConnection<'_>) -> rusqlite::Result<()> {
    use ColumnKind::{Integer, Real, Text};
    const SEARCH: &[ColumnDef] = &[
        column("id", Text, false),
        column("score", Real, false),
        column("q", Text, true),
        column("k", Integer, true),
    ];
    const EXPAND: &[ColumnDef] = &[
        column("id", Text, false),
        column("depth", Integer, false),
        column("src", Text, true),
        column("rels", Text, true),
        column("max_depth", Integer, true),
        column("cap", Integer, true),
    ];
    kaban_vtab::register_execution(conn, "search", SEARCH)?;
    kaban_vtab::register_execution(conn, "expand", EXPAND)
}

pub(super) struct Payload<'a, 'conn> {
    tx: &'a Transaction<'conn>,
    pin: &'a Pin,
    view: &'a ViewName,
    node_ceiling: i64,
    store: &'a SqliteStore,
    guard: &'a Guard,
    text: OnceCell<TextInputs>,
    graph: OnceCell<graph_class::LoadedView<'a>>,
    uses_search: Cell<bool>,
    uses_expand: Cell<bool>,
    pub(super) truncated: Cell<bool>,
    pub(super) error: RefCell<Option<StoreError>>,
}

impl<'a, 'conn> Payload<'a, 'conn> {
    pub(super) fn new(
        tx: &'a Transaction<'conn>,
        pin: &'a Pin,
        view: &'a ViewName,
        store: &'a SqliteStore,
        guard: &'a Guard,
    ) -> Result<Self> {
        // The INTEGER PRIMARY KEY's rightmost entry bounds the number of public node IDs.
        // Capture this statistic in the pin's transaction before SQLite invokes xBestIndex.
        let node_ceiling = tx
            .query_row("SELECT coalesce(max(rid),0) FROM main.node", [], |row| {
                row.get(0)
            })
            .sql()?;
        Ok(Self {
            tx,
            pin,
            view,
            node_ceiling,
            store,
            guard,
            text: OnceCell::new(),
            graph: OnceCell::new(),
            uses_search: Cell::new(false),
            uses_expand: Cell::new(false),
            truncated: Cell::new(false),
            error: RefCell::new(None),
        })
    }
    pub(super) fn plan(&self, name: &'static str, input: &PlanInput<'_>) -> Plan {
        let cap = if name == "search" {
            self.uses_search.set(true);
            "k"
        } else {
            self.uses_expand.set(true);
            "cap"
        };
        let rows = input
            .bound
            .iter()
            .find_map(|b| match (&b.value, b.name) {
                (Some(Value::Integer(n)), key) if key == cap => Some((*n).max(1)),
                _ => None,
            })
            .unwrap_or(50)
            .min(self.node_ceiling);
        Plan {
            rows,
            strategy: i32::from(name == "search" && self.node_ceiling == 0),
            cost: 10.0 + f64::from(u32::try_from(rows).unwrap_or(u32::MAX)),
            ..Default::default()
        }
    }
    /// Only the classes requested by the prepared statement, through its held transaction.
    pub(super) fn load(&self, budget: &QueryBudget) -> Result<()> {
        if self.uses_search.get() && self.node_ceiling != 0 {
            let text = self.store.catalog().pinned_text(self.pin, self.tx)?;
            self.text
                .set(text)
                .map_err(|_| refused("search", "text was already loaded"))?;
        }
        if self.uses_expand.get() {
            let graph =
                graph_class::load_view(self.tx, self.pin, self.view, graph_class::CLASSES, budget)?;
            self.graph
                .set(graph)
                .map_err(|_| refused("expand", "graph was already loaded"))?;
        }
        Ok(())
    }
    fn scan(&self, name: &str, scan: &Scan<'_>, budget: &QueryBudget) -> Result<Cursor> {
        // The next row holds two Values and a public UUID string during transfer to SQLite.
        let mut storage = budget.reserve(256)?;
        let data = match (name, scan.args) {
            ("search", [Value::Text(q), Value::Integer(k)]) => {
                let limit = usize::try_from(*k).map_err(|_| refused("search", "invalid k"))?;
                if scan.strategy == 1 {
                    return Ok(Cursor {
                        data: Data::Empty,
                        _storage: storage,
                    });
                }
                super::charge(
                    &mut storage,
                    (limit as u64).saturating_mul(size_of::<TextHit>() as u64),
                )?;
                let inputs = self
                    .text
                    .get()
                    .ok_or_else(|| refused("search", "no pinned text inputs"))?;
                let query = TextQuery {
                    text: q,
                    mode: MatchMode::Lexical,
                    filter: SeedFilter {
                        kinds: &[],
                        tags: &[],
                        workspace_id: None,
                        code_ref: None,
                    },
                    limit,
                    sufficient: limit,
                    skip: 0,
                };
                if limit == 0 {
                    Data::Empty
                } else {
                    let index = &self.store.text_index;
                    match inputs.with_reader(
                        index,
                        inputs.select(index),
                        budget,
                        |reader, snapshot| reader.search(snapshot, &query, budget),
                    )? {
                        Searched::Hits { hits, .. } => Data::Search(hits.into_iter()),
                        Searched::Unavailable => {
                            return Err(refused("search", "text index unavailable"));
                        }
                    }
                }
            }
            (
                "expand",
                [
                    Value::Text(src),
                    Value::Text(rel),
                    Value::Integer(depth),
                    Value::Integer(cap),
                ],
            ) => {
                let graph = self
                    .graph
                    .get()
                    .ok_or_else(|| refused("expand", "no pinned graph inputs"))?;
                let caps = Caps {
                    depth: u8::try_from(*depth).map_err(|_| refused("expand", "invalid depth"))?,
                    rows: u32::try_from(*cap).map_err(|_| refused("expand", "invalid cap"))?,
                };
                let source =
                    NodeId::parse(src).map_err(|_| refused("expand", "invalid source NodeId"))?;
                let source = self.guard.id_lookup(|| graph.node_id_to_rid(&source))?;
                match (source, graph.rel_id(rel)) {
                    (Some(source), Some(rel)) => {
                        let reached = kaban_index_graph::expand(
                            &graph.view(budget)?,
                            &[source],
                            &RelSelection::only(vec![rel]),
                            Direction::Out,
                            caps,
                            budget,
                        )
                        .map_err(graph_class::graph_error)?;
                        if reached.truncated {
                            self.truncated.set(true);
                        }
                        Data::Expand(reached, 0)
                    }
                    _ => Data::Empty,
                }
            }
            _ => return Err(refused("arguments", "invalid table function arguments")),
        };
        Ok(Cursor {
            data,
            _storage: storage,
        })
    }
    fn record(&self, error: StoreError) -> FnError {
        *self.error.borrow_mut() = Some(error);
        FnError {
            construct: "query".into(),
            reason: "table function failed".into(),
        }
    }
}

pub(super) struct Cursor {
    data: Data,
    _storage: BudgetLease,
}
enum Data {
    Empty,
    Search(std::vec::IntoIter<TextHit>),
    Expand(Charged<Reached>, usize),
}
impl ExecutionPayload for Payload<'_, '_> {
    type Cursor = Cursor;
    fn open(
        &self,
        name: &'static str,
        scan: &Scan<'_>,
        budget: &QueryBudget,
    ) -> std::result::Result<Cursor, FnError> {
        self.scan(name, scan, budget).map_err(|e| self.record(e))
    }
    fn next(
        &self,
        cursor: &mut Cursor,
        _budget: &QueryBudget,
    ) -> Option<std::result::Result<Vec<Value>, FnError>> {
        let (id, value) = match &mut cursor.data {
            Data::Empty => return None,
            Data::Search(hits) => {
                let hit = hits.next()?;
                let id: Result<String> = self.guard.id_lookup(|| {
                    self.tx
                        .query_row(
                            "SELECT id FROM main.node WHERE rid=?1 AND lifecycle <> 'purged'",
                            [hit.rid],
                            |r| r.get(0),
                        )
                        .sql()
                });
                (id, Value::Real(hit.score))
            }
            Data::Expand(reached, at) => {
                let (rid, depth) = *reached.nodes.get(*at)?;
                *at += 1;
                let id = self
                    .graph
                    .get()
                    .ok_or_else(|| refused("expand", "missing graph"))
                    .and_then(|graph| {
                        self.guard
                            .id_lookup(|| graph.rid_to_node_id(rid))?
                            .map(|id| id.to_string())
                            .ok_or_else(|| refused("expand", "missing public identity"))
                    });
                (id, Value::Integer(i64::from(depth)))
            }
        };
        Some(
            id.map(|id| vec![Value::Text(id), value])
                .map_err(|e| self.record(e)),
        )
    }
}
