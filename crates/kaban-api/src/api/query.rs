//! `KabanApi::recall`, `KabanApi::traverse` and `KabanApi::stats`: the read paths, every
//! request string gated before any lookup and every card projected on the way out.

use super::KabanApi;
use crate::control::ControlPath;
use crate::enrich::{self, EnrichBudget, Enricher, Enrichment};
use crate::error::ApiError;
use crate::types::{
    PolicyStats, RecallRequest, RecallResponse, StatsRequest, StatsResponse, TraverseRequest,
    TraverseResponse,
};
use kaban_core::{
    CodeRef, CodeRefFilter, Fixed, Method, NodeId, Producer, RecallQuery, StoreScope, TraverseQuery,
};

const MAX_IDS: usize = 64;

impl KabanApi {
    /// Hybrid recall; cards are projected (and re-scanned) on the way out, `follow_ups` are
    /// derived from the projected `CodeRef` pointers linked to the visible cards (one bounded
    /// store query, R3-10) and the enrichers named in `enrich[]` run after card assembly
    /// (core-spec F19).
    ///
    /// # Errors
    /// `Rejected` when the query itself hits the denylist, `Invalid` for an `enrich[]` id no
    /// configured enricher answers to, store errors otherwise.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn recall(&self, p: RecallRequest) -> Result<RecallResponse, ApiError> {
        // every string of the request is gated before it reaches the index or a filter
        // (review R1-3): the sanitized query is what FTS sees, and masked filter values match
        // the masked values that were stored
        let t = "recall";
        let mut red = Vec::new();
        let query = self.gate(t, "query", &p.query, &mut red)?;
        let kinds = self.gate_all(t, "kinds", &p.kinds, &mut red)?;
        let tags = self.gate_all(t, "tags", &p.tags, &mut red)?;
        let workspace_id = self.gate_opt(t, "workspace_id", p.workspace_id.as_deref(), &mut red)?;
        // the CodeRef filter fields are content (S1 lists CodeRef fields): gated like the rest
        let r#ref = match &p.r#ref {
            Some(f) => Some(CodeRefFilter {
                relative_path: self.gate(t, "ref.relative_path", &f.relative_path, &mut red)?,
                name_path: self.gate_opt(t, "ref.name_path", f.name_path.as_deref(), &mut red)?,
                repo: self.gate_opt(t, "ref.repo", f.repo.as_deref(), &mut red)?,
            }),
            None => None,
        };
        // enricher ids are content (gated, so the error can only echo the masked id) and are
        // resolved before any lookup
        let enrich_ids = self.gate_all(t, "enrich", &p.enrich, &mut red)?;
        let enrichers = self.resolve_enrichers(&enrich_ids)?;
        let q = RecallQuery {
            modes: p.modes,
            fuzzy: p.fuzzy,
            query,
            kinds,
            tags,
            workspace_id: self.read_workspace(workspace_id.as_deref()),
            limit: p.limit.unwrap_or(10),
            hops: p.hops.unwrap_or(1),
            include_archived: p.include_archived,
            r#ref,
        };
        // the read gate decides eligibility inside the recall (spec 05 SS-11): a record a
        // reject rule hides is no deliverable result and the store refills in its place. The
        // predicate runs under the store's lock, so it reads nothing from the store; the
        // shown cards are projected below with their `why`
        let hits = self
            .store
            .recall_eligible(&q, &|node| !self.hidden_read(node))?;
        let mut results = Vec::with_capacity(hits.len());
        let mut visible: Vec<NodeId> = Vec::with_capacity(hits.len());
        for h in &hits {
            let Some(mut c) = self.project(&h.node, p.full_text, Some(&h.why)) else {
                continue; // hidden by a reject rule (S4): no card, no pointer
            };
            c.score = Some((h.score * 1000.0).round() / 1000.0);
            visible.extend(NodeId::parse(&h.node.id).ok());
            results.push(c);
        }
        let pointers = self.linked_pointers(&visible)?;
        let follow_ups = enrich::follow_ups(&pointers);
        let enrichments = self.run_enrichers(&enrichers, &pointers)?;
        Ok(RecallResponse {
            count: results.len(),
            results,
            follow_ups,
            enrichments,
        })
    }

    /// The configured enrichers named by `ids` (gated), each once; an unknown id is `Invalid`
    /// and names the configured ids (M3-27).
    fn resolve_enrichers(&self, ids: &[String]) -> Result<Vec<&dyn Enricher>, ApiError> {
        let mut out: Vec<&dyn Enricher> = Vec::new();
        for id in ids {
            let Some(e) = self.enrichers.iter().find(|e| e.id() == id) else {
                let configured: Vec<&str> = self.enrichers.iter().map(|e| e.id()).collect();
                return Err(enrich::unknown_enricher(id, &configured));
            };
            if !out.iter().any(|o| o.id() == id) {
                out.push(e.as_ref());
            }
        }
        Ok(out)
    }

    /// Run the selected enrichers on the pointers within the default budget; every string of
    /// their output passes the read gate (a reject hit drops that enrichment, S4).
    fn run_enrichers(
        &self,
        enrichers: &[&dyn Enricher],
        pointers: &[CodeRef],
    ) -> Result<Vec<Enrichment>, ApiError> {
        if enrichers.is_empty() || pointers.is_empty() {
            return Ok(Vec::new());
        }
        let budget = EnrichBudget::default();
        let refs = &pointers[..pointers.len().min(budget.max_refs)];
        let mut out = Vec::new();
        for e in enrichers {
            for item in e.enrich(refs, budget)? {
                if let Some(gated) = self.project_enrichment(&item) {
                    out.push(gated);
                }
            }
        }
        Ok(out)
    }

    /// Bounded graph traversal.
    ///
    /// # Errors
    /// `Invalid` for an empty or oversized `start_ids`, `NotFound` for an unknown start id,
    /// store errors otherwise.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn traverse(&self, p: TraverseRequest) -> Result<TraverseResponse, ApiError> {
        if p.start_ids.is_empty() || p.start_ids.len() > MAX_IDS {
            return Err(ApiError::Invalid(
                "start_ids must have 1..64 entries".into(),
            ));
        }
        // every string is gated before any lookup, so a `NotFound` can only echo the masked
        // value (review R1-3)
        let t = "traverse";
        let mut red = Vec::new();
        // start ids are control (validated by shape, scanned only when malformed)
        let mut start_ids = Vec::with_capacity(p.start_ids.len());
        for s in &p.start_ids {
            let gated = self.gate_control(
                t,
                ControlPath::request(t, "start_ids[]"),
                "start_ids",
                s,
                &mut red,
            )?;
            start_ids.push(Self::node_id(&gated)?);
        }
        let rels = self.gate_all(t, "rels", &p.rels, &mut red)?;
        let node_kinds = self.gate_all(t, "node_kinds", &p.node_kinds, &mut red)?;
        // EN-F46: the filter vocabulary is gated like `rels` (a masked value then fails the
        // closed-vocabulary match with a fixed word, never an echo)
        let edge_filter = match &p.edge_filter {
            Some(f) => Some((
                f.min_score,
                self.gate_all(t, "edge_filter.producers", &f.producers, &mut red)?,
                self.gate_all(t, "edge_filter.methods", &f.methods, &mut red)?,
            )),
            None => None,
        };
        let workspace_id = self.gate_opt(t, "workspace_id", p.workspace_id.as_deref(), &mut red)?;
        let edge_filter = match edge_filter {
            Some((min_score, producers, methods)) => {
                Some(Self::edge_filter(min_score, &producers, &methods)?)
            }
            None => None,
        };
        let q = TraverseQuery {
            start_ids,
            rels,
            // a closed enum (MT-5): validated by the typed parse, registered as control
            direction: p.direction.unwrap_or_default(),
            max_depth: p.max_depth.unwrap_or(1),
            node_kinds,
            edge_filter,
            limit: p.limit.unwrap_or(100),
            workspace_id: self.read_workspace(workspace_id.as_deref()),
        };
        let r = self.store.traverse(&q)?;
        let nodes: Vec<_> = r
            .nodes
            .iter()
            .filter_map(|n| self.project(n, false, None))
            .collect();
        let ids: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let edges: Vec<_> = r
            .edges
            .iter()
            .filter(|e| ids.contains(e.src.as_str()) && ids.contains(e.dst.as_str()))
            .filter_map(|e| self.project_edge(t, e))
            .collect();
        Ok(TraverseResponse { nodes, edges })
    }

    /// The wire edge filter as the store takes it (EN-F46): the closed vocabularies by their
    /// stored spelling, the score bound as a `Fixed`; an unknown spelling is refused with a
    /// fixed word (the gated value is never echoed).
    fn edge_filter(
        min_score: Option<f64>,
        producers: &[String],
        methods: &[String],
    ) -> Result<kaban_core::EdgeFilter, ApiError> {
        let min_score = match min_score {
            Some(v) => Some(Fixed::from_f64(v).map_err(|_| {
                ApiError::Invalid("edge_filter.min_score must be within 0..1".into())
            })?),
            None => None,
        };
        let producers = producers
            .iter()
            .map(|p| Producer::parse(p).ok_or_else(|| ApiError::Invalid("unknown_producer".into())))
            .collect::<Result<Vec<_>, _>>()?;
        let methods = methods
            .iter()
            .map(|m| Method::parse(m).ok_or_else(|| ApiError::Invalid("unknown_method".into())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(kaban_core::EdgeFilter {
            min_score,
            producers,
            methods,
        })
    }

    /// Counts of nodes/edges by kind and relationship inside the bound (or requested)
    /// workspace, database size (global), denylist policy summary.
    ///
    /// # Errors
    /// Store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn stats(&self, p: StatsRequest) -> Result<StatsResponse, ApiError> {
        let requested = self.gate_opt(
            "stats",
            "workspace_id",
            p.workspace_id.as_deref(),
            &mut Vec::new(),
        )?;
        let v = self
            .store
            .stats(self.read_workspace(requested.as_deref()).as_deref())?;
        let mut store = match v {
            serde_json::Value::Object(m) => m,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("store".into(), other);
                m
            }
        };
        // kind and relationship names are stored strings: gated like every other (review R1-4)
        for field in ["by_kind", "by_rel"] {
            if let Some(map) = store.get(field) {
                let gated = self.project_stat_keys(field, map);
                store.insert(field.into(), gated);
            }
        }
        // the scope label names a workspace: masked, or dropped on a reject hit
        if let Some(scope) = store.remove("scope")
            && let Some(gated) = self.project_stats_scope(&scope)
        {
            store.insert("scope".into(), gated);
        }
        let pol = self.pol();
        let policy = PolicyStats {
            version: pol.version_hash.clone(),
            effective_hash: pol.effective_hash().to_owned(),
            rules: pol.file.rules.len(),
            words: pol.file.words.len(),
            phrases: pol.file.phrases.len(),
            scan_on_read: pol.file.defaults.scan_on_read,
        };
        Ok(StatsResponse {
            store,
            stores: vec![StoreScope::Project],
            policy,
            // Optional bookkeeping must not hide usable counters or the other producers.
            producers: self.store.index_meta().map_or_else(
                |_| crate::producers::ProducerRegistry.enabled(self.engine()),
                |meta| crate::producers::ProducerRegistry.with_runs(self.engine(), &meta),
            ),
            // the catalog's index states; a failed verification hides no other counter
            indexes: self.catalog.as_ref().map_or_else(Vec::new, |catalog| {
                catalog.verify().map_or_else(
                    |e| {
                        tracing::warn!(error = %e, "stats: the catalog could not be verified");
                        Vec::new()
                    },
                    |found| found.indexes,
                )
            }),
        })
    }
}
