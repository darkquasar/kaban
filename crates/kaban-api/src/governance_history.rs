//! The `governance_history` tool (stage1-spec G1-F13, OD-8; §6; v4 CG6.2): for each governing
//! record one selector reaches, its `SUPERSEDES` chain in both directions and its governance
//! events, newest first. Superseded (archived) records are returned here and through
//! `why(include_superseded)` only; both are read-gated like every other response.
//!
//! The three wire types live beside the handler (the request, one record, the response); the
//! control registry (`control/memory.rs`) names `id`, `records[].card.id`,
//! `records[].superseded_by[]` and `records[].supersedes[]` as `NodeId` paths.
//!
//! The walk: `id` seeds one record (`store.get`, filtered to the read workspace as `events`
//! does: archived visible, tombstoned hidden); `symbol` and `path` seed through the public
//! `why(.., include_superseded: true)`, so records are reached exactly as `why` reaches them
//! and an archived record nothing supersedes is reachable by `id` only. Per seed the order is
//! the seed, its predecessors, its successors (each chain nearest first, 50 hops); each id is
//! a record once; per record one `get`, two chain walks and one `events` read (no batching:
//! the CG6.1 residual stands). The store checks every seed and every hop against the resolved
//! `WorkspaceFilter` (RS-9, S6): a hop outside it ends the chain, counted, never returned.

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::types::{Card, EventView, WhyRequest, WhyTarget};
use kaban_core::{Chain, ChainDirection, Governance, NodeId, StoreError, WorkspaceFilter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Records returned when the request names no `limit` (G1-F13).
const DEFAULT_LIMIT: usize = 20;
/// The most records one call returns (G1-F13).
const MAX_LIMIT: usize = 100;
/// The hop bound of each chain walk (G1-F13: "each chain is bounded to 50 hops").
const CHAIN_HOPS: usize = 50;
/// The newest events kept per record (G1-F13).
const MAX_EVENTS: usize = 100;
/// Seeds one `symbol` or `path` selector may reach: `why`'s own maximum per target.
const SEED_LIMIT: usize = 100;

/// The error class of a request that sets no selector or several (stage1-spec §4 `selector_invalid`,
/// as the `code_*` tools spell it); exit 4 on the CLI.
const SELECTOR_ONE_OF: &str = "selector_invalid: exactly one of id, symbol or path";
/// The error class of a `path` that is not repository-relative (a `..` component or a leading
/// `/`): the same class as `code_*`'s `current_path` rule (`code/ranking.rs`); the value is
/// never echoed.
const PATH_NOT_RELATIVE: &str = "selector_invalid: path must be repository-relative without ..";

/// `governance_history` input (stage1-spec §6): exactly one selector, the record bound and the
/// workspace to read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GovernanceHistoryRequest {
    /// One record by id (archived records included; a record in another workspace is not
    /// found).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// A normalised SCIP symbol key: the records `why(symbol)` reaches, superseded ones
    /// included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// A repository-relative path (no `..` component, no leading `/`): the records whose
    /// `path_scope` governs it, superseded ones included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Max records (default 20, max 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Workspace to read (default: the bound workspace; `"*"` for all; core-spec S6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// One record of the history with its chain and its events.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HistoryRecord {
    /// The record (compact card; `props` are not included).
    pub card: Card,
    /// Governance state.
    pub governance: Governance,
    /// The ids of the records this one supersedes, nearest first, the whole chain (at most 50
    /// hops; a control-registry `NodeId` path). Always present: `[]` when it supersedes none.
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// The ids of the records that supersede this one, nearest first, the whole chain (at most
    /// 50 hops; a control-registry `NodeId` path). Always present: `[]` for a current record.
    #[serde(default)]
    pub superseded_by: Vec<String>,
    /// The record's governance events, newest first, at most 100.
    #[serde(default)]
    pub events: Vec<EventView>,
    /// `true` when a chain of this record did not reach every hop: the hop bound, a tombstoned
    /// hop or a hop in an unselected workspace (S6: counted, never returned).
    pub chain_cut: bool,
}

/// `governance_history` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GovernanceHistoryResponse {
    /// Per seed in `why` order: the seed, then its predecessors, then its successors, each
    /// record once.
    pub records: Vec<HistoryRecord>,
    /// `true` when any bound cut: more records than `limit`, a `chain_cut` record, or a
    /// record with more than 100 events.
    pub partial: bool,
}

/// The one selector a request sets, after the gate.
enum Selector {
    Id(String),
    Symbol(String),
    Path(String),
}

/// The two chain walks of one record: predecessors, successors.
type Chains = (Chain, Chain);

/// Whether `path` is repository-relative: no leading `/`, no `..` component. The gate ran
/// first (content before grammar); no shared validator exists outside the `code` module.
fn repo_relative(path: &str) -> bool {
    !path.starts_with('/') && !path.split('/').any(|component| component == "..")
}

/// The chain's ids as wire strings, nearest first.
fn ids(chain: &Chain) -> Vec<String> {
    chain.ids.iter().map(|id| id.as_str().to_owned()).collect()
}

impl KabanApi {
    /// The governance history of the records one selector reaches (stage1-spec G1-F13):
    /// `id` one record, `symbol` and `path` the records `why(.., include_superseded: true)`
    /// reaches for that target (so an archived record nothing supersedes is reachable by `id`
    /// only); per record the `SUPERSEDES` chain in both directions and the governance events,
    /// newest first. Every seed and every hop is read under the resolved workspace (RS-9, S6);
    /// every string is gated like `why`'s.
    ///
    /// # Errors
    /// `Invalid` (`selector_invalid` when not exactly one of `id`, `symbol`, `path` is set, or
    /// `path` is not repository-relative; `why`'s `symbol_invalid` for a bad `symbol`),
    /// `NotFound` (an unknown or hidden `id`, or one in another workspace), `Rejected` (a
    /// string hits the denylist), store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn governance_history(
        &self,
        p: GovernanceHistoryRequest,
    ) -> Result<GovernanceHistoryResponse, ApiError> {
        let t = "governance_history";
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let id = self.gate_opt(&mut ctx, t, "id", p.id.as_deref(), &mut red)?;
        let symbol = self.gate_opt(&mut ctx, t, "symbol", p.symbol.as_deref(), &mut red)?;
        let path = self.gate_opt(&mut ctx, t, "path", p.path.as_deref(), &mut red)?;
        let workspace_id = self.gate_opt(
            &mut ctx,
            t,
            "workspace_id",
            p.workspace_id.as_deref(),
            &mut red,
        )?;
        self.flush_audit(&mut ctx)?;
        // content before grammar (G1-S1): the selector rule judges gated strings only
        let selector = match (id, symbol, path) {
            (Some(id), None, None) => Selector::Id(id),
            (None, Some(symbol), None) => Selector::Symbol(symbol),
            (None, None, Some(path)) => Selector::Path(path),
            _ => return Err(ApiError::Invalid(SELECTOR_ONE_OF.into())),
        };
        if matches!(&selector, Selector::Path(path) if !repo_relative(path)) {
            return Err(ApiError::Invalid(PATH_NOT_RELATIVE.into()));
        }
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        // the resolved read workspace (S6): the seed `get` and every chain walk run under it
        let filter = WorkspaceFilter::from(self.read_workspace(workspace_id.as_deref()));
        // the seed cut (`why` at `SEED_LIMIT`) is a truncation of this answer too (RW3-4)
        let (seeds, mut partial) = self.history_seeds(selector, workspace_id, &filter)?;

        let mut records = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        'seeds: for seed in seeds {
            // a seed the store refuses (gone, tombstoned, outside the filter) contributes
            // nothing (as `why.rs` skips one, default 2)
            let Some(chains) = self.chains(&seed, &filter)? else {
                continue;
            };
            let (pred, succ) = &chains;
            let order: Vec<NodeId> = std::iter::once(seed.clone())
                .chain(pred.ids.iter().cloned())
                .chain(succ.ids.iter().cloned())
                .collect();
            let mut seed_chains = Some(chains);
            for id in order {
                if !seen.insert(id.as_str().to_owned()) {
                    continue;
                }
                // the seed's walks are reused; a hop walks its own chains
                let chains = match seed_chains.take() {
                    Some(c) if id == seed => c,
                    _ => match self.chains(&id, &filter)? {
                        Some(c) => c,
                        None => continue,
                    },
                };
                let Some(record) = self.history_record(&id, chains, &mut partial)? else {
                    continue;
                };
                if records.len() >= limit {
                    partial = true;
                    break 'seeds;
                }
                records.push(record);
            }
        }
        Ok(GovernanceHistoryResponse { records, partial })
    }

    /// The seed records of one selector: `id` the one node, visible in the read workspace
    /// (archived visible, tombstoned hidden) else `NotFound`; `symbol` and `path` the cards of
    /// one `why` target with `include_superseded`, in `why`'s order, with `why`'s `partial`
    /// (more governing records than `SEED_LIMIT`: the answer is partial before any walk).
    fn history_seeds(
        &self,
        selector: Selector,
        workspace_id: Option<String>,
        filter: &WorkspaceFilter,
    ) -> Result<(Vec<NodeId>, bool), ApiError> {
        let target = match selector {
            Selector::Id(id) => {
                let node_id = Self::node_id(&id)?;
                self.store
                    .get(&node_id, false)?
                    .filter(|n| filter.as_option().is_none_or(|ws| n.workspace_id == ws))
                    .ok_or(ApiError::NotFound(id))?;
                return Ok((vec![node_id], false));
            }
            Selector::Symbol(symbol) => WhyTarget::Symbol(symbol),
            Selector::Path(path) => WhyTarget::Path(path),
        };
        let found = self.why(WhyRequest {
            targets: vec![target],
            limit: Some(SEED_LIMIT),
            workspace_id,
            include_superseded: true,
        })?;
        let seeds = found
            .results
            .iter()
            .filter_map(|c| NodeId::parse(&c.node.id).ok())
            .collect();
        Ok((seeds, found.partial))
    }

    /// Both chain walks of `id` under `filter`; `None` when the store refuses the seed (gone
    /// between two reads, tombstoned, outside the filter): never a caller error (default 2).
    fn chains(&self, id: &NodeId, filter: &WorkspaceFilter) -> Result<Option<Chains>, ApiError> {
        let walk = |direction| match self
            .store
            .supersession_chain(id, direction, CHAIN_HOPS, filter)
        {
            Ok(chain) => Ok(Some(chain)),
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(e) => Err(ApiError::from(e)),
        };
        let (Some(pred), Some(succ)) = (
            walk(ChainDirection::Predecessors)?,
            walk(ChainDirection::Successors)?,
        ) else {
            return Ok(None);
        };
        Ok(Some((pred, succ)))
    }

    /// One record from its walked chains: the card (`None` when the read gate hides it, S4,
    /// or the node is gone), its governance, the chain ids and the events of its own
    /// workspace, projected, newest first, at most 100. `partial` is raised by a chain cut
    /// or an events cut of a returned record; an event the projection drops is not a cut.
    fn history_record(
        &self,
        id: &NodeId,
        (pred, succ): Chains,
        partial: &mut bool,
    ) -> Result<Option<HistoryRecord>, ApiError> {
        let Some(node) = self.store.get(id, false)? else {
            return Ok(None);
        };
        let Some(card) = self.project(&node, false, None) else {
            return Ok(None);
        };
        let mut events: Vec<EventView> = self
            .store
            .events(id)?
            .iter()
            .filter(|e| e.workspace_id == node.workspace_id)
            .filter_map(|e| self.project_event(e))
            .collect();
        events.reverse();
        let events_cut = events.len() > MAX_EVENTS;
        events.truncate(MAX_EVENTS);
        let chain_cut = pred.cut || succ.cut;
        *partial |= chain_cut || events_cut;
        Ok(Some(HistoryRecord {
            card,
            governance: node.governance,
            supersedes: ids(&pred),
            superseded_by: ids(&succ),
            events,
            chain_cut,
        }))
    }
}
