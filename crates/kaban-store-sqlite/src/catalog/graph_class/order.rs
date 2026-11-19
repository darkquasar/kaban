//! Stable public order for a graph snapshot. Supported writers assign increasing row ids and
//! retain them with the same UUID through purge. Even an older reader can therefore share
//! committed keys, provided it restricts lookup to its own snapshot's maximum rid.

use std::collections::{BTreeMap, LinkedList};
use std::sync::{Arc, PoisonError, RwLock};

use kaban_core::Result;
use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_index_graph::{GraphError, LocalId, StableOrder, TieRank};
use rusqlite::Connection;

use super::local_id;
use crate::catalog::pin::budget_exceeded;
use crate::sql_error::SqlResultExt;

/// One view cache's identity storage, also retained by outstanding orders.
#[derive(Debug)]
pub(super) struct Identities {
    keys: RwLock<Keys>,
    _charge: BudgetLease,
}

#[derive(Debug, Default)]
struct Keys {
    by_rid: BTreeMap<LocalId, TieRank>,
    #[expect(
        clippy::linkedlist,
        reason = "adding a tail must never relocate existing leases"
    )]
    charges: LinkedList<BudgetLease>,
}

/// Reserve a shared object's value, reference counts and allocation metadata before allocation.
fn shared_charge<T>(budget: &QueryBudget) -> Result<BudgetLease> {
    let bytes = (size_of::<T>() + 2 * size_of::<usize>()) as u64;
    let charge = QueryBudget::allocation_charge(bytes, align_of::<usize>() as u64)
        .map_err(budget_exceeded)?;
    budget.reserve(charge).map_err(budget_exceeded)
}

impl Identities {
    pub(super) fn new(budget: &QueryBudget) -> Result<Arc<Self>> {
        let charge = shared_charge::<Self>(budget)?;
        Ok(Arc::new(Self {
            keys: RwLock::default(),
            _charge: charge,
        }))
    }

    pub(super) fn order(
        self: &Arc<Self>,
        rows: &Connection,
        budget: &QueryBudget,
    ) -> Result<Arc<NodeOrder>> {
        let charge = shared_charge::<NodeOrder>(budget)?;
        let max: Option<i64> = rows
            .query_row("SELECT max(rid) FROM node", [], |r| r.get(0))
            .sql()?;
        let ceiling = max.map(local_id).transpose()?;
        let last = self
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .last();
        if ceiling > last {
            let mut keys = self.keys.write().unwrap_or_else(PoisonError::into_inner);
            if ceiling > keys.last() {
                keys.extend(rows, budget)?;
            }
        }
        budget.check().map_err(budget_exceeded)?;
        Ok(Arc::new(NodeOrder {
            identities: Arc::clone(self),
            ceiling,
            _charge: charge,
        }))
    }
}

impl Keys {
    fn last(&self) -> Option<LocalId> {
        self.by_rid.last_key_value().map(|(&rid, _)| rid)
    }

    /// Stream only the selected snapshot's committed tail, including retained ghosts. The last
    /// inserted key is the coverage cursor, so refusal leaves the entire missing tail retryable.
    fn extend(&mut self, rows: &Connection, budget: &QueryBudget) -> Result<()> {
        let after = self.last().map_or(-1, |rid| i64::from(rid.0));
        let mut stmt = rows
            .prepare("SELECT rid, unhex(id, '-') FROM node WHERE rid > ?1 ORDER BY rid")
            .sql()?;
        let mut rows = stmt.query([after]).sql()?;
        let mut batch = [(LocalId(0), TieRank([0; 16])); 128];
        loop {
            budget.check().map_err(budget_exceeded)?;
            let mut count = 0;
            for slot in &mut batch {
                let Some(row) = rows.next().sql()? else { break };
                *slot = (local_id(row.get(0).sql()?)?, TieRank(row.get(1).sql()?));
                count += 1;
            }
            if count != 0 {
                // On the pinned toolchain a full B-tree node holds 11 four-byte rids and
                // 11 sixteen-byte keys, at most 12 pointers and its header (<384 bytes with
                // allocation metadata). Every non-root node has >=5 keys. Cover the root
                // separately and allow 64 bytes per linked-list allocation and its lease.
                let root = if self.by_rid.is_empty() { 384 } else { 0 };
                let charge = budget
                    .reserve(root + 80 * count as u64 + 64)
                    .map_err(budget_exceeded)?;
                self.charges.push_back(charge);
                for &(rid, key) in &batch[..count] {
                    self.by_rid.insert(rid, key);
                }
            }
            if count < batch.len() {
                return Ok(());
            }
        }
    }
}

/// The identity ordering selected by one snapshot. Presence belongs to the selected graph.
#[derive(Debug)]
pub(crate) struct NodeOrder {
    identities: Arc<Identities>,
    ceiling: Option<LocalId>,
    _charge: BudgetLease,
}

impl StableOrder for NodeOrder {
    fn rank(&self, n: LocalId) -> std::result::Result<TieRank, GraphError> {
        if Some(n) <= self.ceiling {
            let keys = self
                .identities
                .keys
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(&key) = keys.by_rid.get(&n) {
                return Ok(key);
            }
        }
        Err(GraphError::Corrupt("a node without a rank".into()))
    }
}
