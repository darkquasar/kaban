//! `why` (core-spec F2; core-plan C3.4): the accepted (then proposed) records of the governing
//! kinds (`governed.governs_paths: true` in the loaded ontology, engine-spec EN-F4: Decisions
//! and Conventions in the shipped `adr`) whose `path_scope` governs a path, or whose text
//! matches a query, each with its currency (F14), and the gated read of a record's governance
//! events (M3-6, WG-2).
//!
//! ```text
//!   WhyRequest { targets, limit, workspace, include_superseded }
//!     | gate every string (S1), resolve the read workspace (S6)
//!     v
//!   per target: GraphStore::governing(GoverningQuery)   -- SQL prefilter (literal entries,
//!     | exact: ScopeMatcher per glob entry                  GLOB superset for glob entries)
//!     | include_superseded: supersession_chain(card, Predecessors, 50) per current card,
//!     |   the archived hops appended after the target's current cards (G1-F13, CG6.1)
//!     v
//!   group by anchor (accepted_commit | accepted_at)      -- M3-22: one git log per group over
//!     | currency::snapshot(union of entries), scoped        the union of the group's entries,
//!     | threads, at most 4                                 attribute() per record
//!     v
//!   project() each card (S4), WhyCard { node, governance, currency, matched_target, matched_by,
//!     conflict, code, superseded_by }   -- conflicts_for(visible ids): one bounded query (M5-6);
//!     |                                    superseded_by: supersession_chain(card, Successors, 50)
//!     v
//!   follow_ups from the CodeRef nodes linked to the visible cards (F19, R3-10; `enrich.rs`)
//! ```

//! A `symbol` target (engine-spec EN-F17, EN-F18, EN-F18a; CG-JOIN v1 §3, §6; engine-plan E2.6)
//! runs two legs under one resolution per call:
//!
//! ```text
//!   gate(targets[]) ==> symbol::parse (symbol_invalid)        -- content before grammar (G1-S1)
//!   resolve_for_card(api) once  ==> attach_snapshot(attach_from(&r))   -- the lease drops at return
//!   per symbol target:  r.snapshot.symbol_unique(key)
//!     | one row      ==> code: {snapshot}            legs A and B
//!     | none         ==> code: snapshot_mismatch     leg A (commit: Some)
//!     | Ambiguous    ==> code: ambiguous_symbol      leg A
//!     | Absent/TooNew==> code: snapshot_absent | snapshot_too_new, leg A with commit: None, nothing attached
//!   leg A: coderefs_by_symbol(predicates §3) ==> traverse 1 hop: ABOUT in, GOVERNS in (matched_by: symbol),
//!          EVIDENCE_FOR out only (matched_by: evidence); kinds = governing kinds, accepted | proposed
//!   leg B: governing(path_candidates(defining file)) exact-matched, matched_by: path_scope
//! ```

use crate::api::KabanApi;
use crate::code::CodeError;
use crate::currency::{self, Anchor, Currency, ScopeMatcher, Snapshot};
use crate::error::ApiError;
use crate::snapshot::{self, SnapshotOutcome};
use crate::types::{
    CodeField, CodeSnapshot, EventView, MatchedBy, WhyCard, WhyRequest, WhyResponse, WhyTarget,
};
use kaban_codegraph::{CodeGraphReader, ReadError};
use kaban_core::{
    AnchorPredicates, ChainDirection, Event, Governance, GoverningQuery, Node, NodeId, StoreError,
    TraverseDirection, TraverseQuery, WorkspaceFilter, WriteCtx,
};
use std::collections::BTreeMap;
use time::OffsetDateTime;

const MAX_TARGETS: usize = 16;
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;
/// The hop bound of one `SUPERSEDES` chain walk (stage1-spec G1-F13: "each chain is bounded to
/// 50 hops"), for the `include_superseded` predecessors and the `superseded_by` successors.
const CHAIN_HOPS: usize = 50;
/// Scoped threads for the per-anchor git queries (M3-22).
const MAX_GIT_THREADS: usize = 4;
/// The one-hop node bound of the record hop of leg A (the store's own ceiling).
const HOP_LIMIT: usize = 500;
/// The fixed class of a `symbol` target that fails the grammar (no echo).
const SYMBOL_INVALID: &str = "targets[].symbol: symbol_invalid";

/// The shape of one gated target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Path,
    Query,
    Symbol,
}

/// The target and its ancestor directories (`src/a/b.rs` -> `src/a/b.rs`, `src/a`, `src`).
fn path_candidates(target: &str) -> Vec<String> {
    let mut out = vec![target.to_owned()];
    let mut rest = target;
    while let Some(i) = rest.rfind('/') {
        rest = &rest[..i];
        if !rest.is_empty() {
            out.push(rest.to_owned());
        }
    }
    out
}

/// A governing record matched to one target, before currency and projection.
struct Governing<'a> {
    node: Node,
    target: &'a str,
    matched_by: MatchedBy,
    /// The code-graph side of a `symbol` target; `None` on path and query targets.
    code: Option<CodeField>,
    /// The commit the anchor row was scoped to, when no snapshot resolved (OWN-83 (1)).
    anchor_commit: Option<String>,
}

/// The 40-hex `snapshot_commit` of a `CodeRef`'s props, lowercased, as `node_anchor` scopes it.
fn scoped_commit(props: &serde_json::Value) -> Option<String> {
    props
        .get("snapshot_commit")
        .and_then(serde_json::Value::as_str)
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

/// The `why` order inside one target (core-spec F2; N5): accepted before proposed, then
/// importance, then newest first, then id.
fn why_order(a: &Node, b: &Node) -> std::cmp::Ordering {
    (a.governance != Governance::Accepted)
        .cmp(&(b.governance != Governance::Accepted))
        .then_with(|| {
            b.importance
                .unwrap_or(0.0)
                .total_cmp(&a.importance.unwrap_or(0.0))
        })
        .then_with(|| b.created_at.cmp(&a.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

/// The history anchor of a record's currency, or why it has none.
enum RecordAnchor {
    Known {
        anchor: Anchor,
        fallback: Option<OffsetDateTime>,
    },
    None(&'static str),
}

impl KabanApi {
    /// Governing records for every target (core-spec F2), each with its currency (F14).
    ///
    /// # Errors
    /// `Invalid` (no targets, more than 16, an empty target), `Rejected` (a target string hits
    /// the denylist), store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn why(&self, p: WhyRequest) -> Result<WhyResponse, ApiError> {
        if p.targets.is_empty() || p.targets.len() > MAX_TARGETS {
            return Err(ApiError::Invalid(format!(
                "targets must have 1..={MAX_TARGETS} entries"
            )));
        }
        let t = "why";
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let mut targets = Vec::with_capacity(p.targets.len());
        for target in &p.targets {
            let (s, kind) = match target {
                WhyTarget::Path(s) => (s, TargetKind::Path),
                WhyTarget::Query(s) => (s, TargetKind::Query),
                WhyTarget::Symbol(s) => (s, TargetKind::Symbol),
            };
            let gated = self.gate(&mut ctx, t, "targets", s, &mut red)?;
            if gated.trim().is_empty() {
                return Err(ApiError::Invalid("targets must not be empty".into()));
            }
            // content before grammar (G1-S1, SO-13): the parser only ever sees a gated string,
            // and a key the gate masked fails the grammar like any other malformed input
            if kind == TargetKind::Symbol && kaban_codegraph::symbol::parse(&gated).is_err() {
                return Err(ApiError::Invalid(SYMBOL_INVALID.into()));
            }
            targets.push((gated, kind));
        }
        let workspace_id = self.gate_opt(
            &mut ctx,
            t,
            "workspace_id",
            p.workspace_id.as_deref(),
            &mut red,
        )?;
        self.flush_audit(&mut ctx)?;
        let workspace = WorkspaceFilter::from(self.read_workspace(workspace_id.as_deref()));
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        // the kinds `why` answers with (core-spec F2): those whose mixin governs paths
        let kinds = self.registry.governing_kinds();
        // one resolution per call when a symbol target is present (cg-join-v1 §4): the
        // lease inside `Resolved` lives until this function returns; the attachment on the
        // store's read connection persists across requests
        let outcome = targets
            .iter()
            .any(|(_, kind)| *kind == TargetKind::Symbol)
            .then(|| snapshot::resolve_for_card(self));
        if let Some(SnapshotOutcome::Resolved(r)) = &outcome {
            self.store.attach_snapshot(&snapshot::attach_from(r))?;
        }

        let mut found: Vec<Governing<'_>> = Vec::new();
        let mut partial = false;
        for (target, kind) in &targets {
            let (mut records, more) = match kind {
                TargetKind::Symbol => {
                    self.symbol_legs(target, outcome.as_ref(), &workspace, &kinds, limit)?
                }
                TargetKind::Path | TargetKind::Query => {
                    let (path_candidates, query, matcher_target, matched_by) = match kind {
                        TargetKind::Path => (
                            path_candidates(target),
                            None,
                            Some(target.as_str()),
                            MatchedBy::PathScope,
                        ),
                        _ => (Vec::new(), Some(target.clone()), None, MatchedBy::Query),
                    };
                    let (nodes, more) = self.governing_page(
                        &kinds,
                        &workspace,
                        path_candidates,
                        query,
                        matcher_target,
                        limit,
                    )?;
                    let records = nodes
                        .into_iter()
                        .map(|node| Governing {
                            node,
                            target,
                            matched_by,
                            code: None,
                            anchor_commit: None,
                        })
                        .collect();
                    (records, more)
                }
            };
            partial |= more;
            if p.include_superseded {
                partial |= self.append_superseded(&mut records, limit, &workspace)?;
            }
            found.extend(records);
        }

        let currencies = self.currencies(&found)?;
        let mut results = Vec::with_capacity(found.len());
        let mut visible = Vec::with_capacity(found.len());
        for (g, currency) in found.into_iter().zip(currencies) {
            let Some(node) = self.project(&g.node, false, None) else {
                continue; // hidden by a reject rule, like every read of it (S4)
            };
            let id = NodeId::parse(&g.node.id).ok();
            let superseded_by = match &id {
                Some(id) => self.superseded_by(id, &workspace)?,
                None => Vec::new(),
            };
            visible.extend(id);
            results.push(WhyCard {
                governance: g.node.governance,
                node,
                currency,
                matched_target: g.target.to_owned(),
                matched_by: g.matched_by,
                conflict: false,
                code: g.code,
                anchor_commit: g.anchor_commit,
                superseded_by,
            });
        }
        // F13 "surfaced by `why`" (M5-6): one bounded query over the visible cards marks the
        // records whose file and database disagree at the same rev
        let in_conflict = self.store.conflicts_for(&visible)?;
        for card in &mut results {
            card.conflict = in_conflict.iter().any(|c| c.node_id == card.node.id);
        }
        // F19: the CodeRef pointers linked to the visible records (`refs`, `props.evidence[]`),
        // one bounded store query, each projected before it becomes a hint (R3-10)
        let follow_ups = crate::enrich::follow_ups(&self.linked_pointers(&visible)?);
        Ok(WhyResponse {
            results,
            partial,
            follow_ups,
        })
    }

    /// `include_superseded` (stage1-spec G1-F13; v4 CG6.1): the `SUPERSEDES` predecessors of
    /// every current card of one target, appended after the current cards in chain order,
    /// each with its successor's target, `matched_by` and `code`, each id once per target,
    /// the whole counted against `limit` (`true` when the limit cut). A superseded record is
    /// archived and neither store leg returns one, so it is reached only through the chain of
    /// a current card that governs the target; every hop is checked against `workspace` by the
    /// store (RS-9, S6). A seed the store no longer finds contributes nothing, a hop that is
    /// gone between the two reads is skipped (default 2).
    fn append_superseded(
        &self,
        records: &mut Vec<Governing<'_>>,
        limit: usize,
        workspace: &WorkspaceFilter,
    ) -> Result<bool, ApiError> {
        let mut reached = Vec::new();
        for g in records.iter() {
            let Ok(seed) = NodeId::parse(&g.node.id) else {
                continue;
            };
            let chain = match self.store.supersession_chain(
                &seed,
                ChainDirection::Predecessors,
                CHAIN_HOPS,
                workspace,
            ) {
                Ok(chain) => chain,
                Err(StoreError::NotFound(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            for hop in &chain.ids {
                let is_hop = |r: &Governing<'_>| r.node.id == hop.as_str();
                if records.iter().any(is_hop) || reached.iter().any(is_hop) {
                    continue;
                }
                let Some(node) = self.store.get(hop, false)? else {
                    continue;
                };
                reached.push(Governing {
                    node,
                    target: g.target,
                    matched_by: g.matched_by,
                    code: g.code.clone(),
                    anchor_commit: g.anchor_commit.clone(),
                });
            }
        }
        records.append(&mut reached);
        if records.len() > limit {
            records.truncate(limit);
            return Ok(true);
        }
        Ok(false)
    }

    /// `WhyCard.superseded_by` (stage1-spec §6): the `SUPERSEDES` successors of `id`, nearest
    /// first, under `workspace` (RS-9: a successor in an unselected workspace ends the chain and
    /// is not listed); empty for a current record, and empty when the store no longer finds the
    /// seed (default 2: never a caller error).
    fn superseded_by(
        &self,
        id: &NodeId,
        workspace: &WorkspaceFilter,
    ) -> Result<Vec<String>, ApiError> {
        match self
            .store
            .supersession_chain(id, ChainDirection::Successors, CHAIN_HOPS, workspace)
        {
            Ok(chain) => Ok(chain.ids.iter().map(|id| id.as_str().to_owned()).collect()),
            Err(StoreError::NotFound(_)) => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// The governing records of one path or query target, at most `limit` of them in the
    /// store's order, and whether more existed. The SQL glob prefilter may admit false
    /// positives (every `{}` or `[]` entry passes); page through the deterministic candidate
    /// order until `limit + 1` exact matches or exhaustion (R3-5), so a lower-ranked true match
    /// behind many false positives is still found and the `partial` flag is exact.
    fn governing_page(
        &self,
        kinds: &[String],
        workspace: &WorkspaceFilter,
        path_candidates: Vec<String>,
        query: Option<String>,
        matcher_target: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Node>, bool), ApiError> {
        let page = limit.saturating_mul(2);
        let mut query = GoverningQuery {
            kinds: kinds.to_vec(),
            governance: vec![Governance::Accepted, Governance::Proposed],
            workspace: workspace.clone(),
            path_candidates,
            query,
            limit: page,
            offset: 0,
            filters: kaban_core::ReadFilters::default(),
            anchor_prefilter: self.config.engine.why.anchor_prefilter,
        };
        let mut kept = Vec::new();
        loop {
            let nodes = self.store.governing(&query)?;
            let exhausted = nodes.len() < page;
            for node in nodes {
                if let Some(target) = matcher_target
                    && !node
                        .path_scope
                        .iter()
                        .any(|e| ScopeMatcher::new(e).matches(target))
                {
                    continue;
                }
                if kept.len() == limit {
                    return Ok((kept, true));
                }
                kept.push(node);
            }
            if exhausted {
                return Ok((kept, false));
            }
            query.offset += page;
        }
    }

    /// The two legs of a `symbol` target (module doc): the records anchored on a `CodeRef`
    /// with that key under the CG-JOIN v1 §3 predicates (leg A), then the records governing
    /// the key's defining file in the resolved snapshot (leg B), deduplicated, in `why` order
    /// per leg, cut at `limit`; every card carries the same `code` field.
    fn symbol_legs<'a>(
        &self,
        key: &'a str,
        outcome: Option<&SnapshotOutcome>,
        workspace: &WorkspaceFilter,
        kinds: &[String],
        limit: usize,
    ) -> Result<(Vec<Governing<'a>>, bool), ApiError> {
        let live = self.policy_version();
        let (commit, code, defining_file) = match outcome {
            Some(SnapshotOutcome::Resolved(r)) => {
                let snap = CodeSnapshot {
                    commit: r.r#ref.commit.clone(),
                    exact: r.r#ref.exact,
                    policy: snapshot::policy_of(&r.r#ref, &live).as_str().to_owned(),
                };
                let commit = Some(r.r#ref.commit.clone());
                match r.snapshot.symbol_unique(key) {
                    Ok(Some(row)) => (commit, CodeField::Snapshot(snap), row.path),
                    Ok(None) => (commit, CodeField::SnapshotMismatch { snapshot: snap }, None),
                    Err(ReadError::Ambiguous) => {
                        (commit, CodeField::AmbiguousSymbol { snapshot: snap }, None)
                    }
                    Err(e) => return Err(CodeError::from(e).into()),
                }
            }
            Some(SnapshotOutcome::TooNew) => (None, CodeField::SnapshotTooNew, None),
            Some(SnapshotOutcome::Absent) | None => (None, CodeField::SnapshotAbsent, None),
        };
        // leg A: the anchored CodeRefs, then one hop to the records they point at
        let predicates = AnchorPredicates {
            workspace,
            repo: self.config.repo.as_deref().unwrap_or_default(),
            commit: commit.as_deref(),
        };
        let mut records: Vec<(Node, MatchedBy)> = Vec::new();
        // OWN-83 (1): with no resolved snapshot every scope matches; a record reached through
        // a commit-scoped pointer carries that commit so the reader sees the missing index
        let mut anchor_commits: BTreeMap<String, String> = BTreeMap::new();
        for coderef in self.store.coderefs_by_symbol(&predicates, key)? {
            let Ok(start) = NodeId::parse(&coderef.id) else {
                continue;
            };
            let anchor_commit = if commit.is_none() {
                scoped_commit(&coderef.props)
            } else {
                None
            };
            let hop = match self.store.traverse(&TraverseQuery {
                start_ids: vec![start],
                rels: vec!["ABOUT".into(), "GOVERNS".into(), "EVIDENCE_FOR".into()],
                direction: TraverseDirection::Both,
                max_depth: 1,
                node_kinds: Vec::new(),
                governance: Vec::new(),
                limit: HOP_LIMIT,
                workspace_id: workspace.as_option().map(str::to_owned),
                ..Default::default()
            }) {
                Ok(hop) => hop,
                // the pointer went away between the two reads: no records through it
                Err(StoreError::NotFound(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            for edge in &hop.edges {
                let (record_id, matched_by) = match edge.rel.as_str() {
                    "ABOUT" | "GOVERNS" if edge.dst == coderef.id => (&edge.src, MatchedBy::Symbol),
                    "EVIDENCE_FOR" if edge.src == coderef.id => (&edge.dst, MatchedBy::Evidence),
                    _ => continue,
                };
                let Some(node) = hop.nodes.iter().find(|n| n.id == *record_id) else {
                    continue;
                };
                if !kinds.contains(&node.kind)
                    || !matches!(node.governance, Governance::Accepted | Governance::Proposed)
                {
                    continue;
                }
                if let Some(c) = &anchor_commit {
                    anchor_commits
                        .entry(node.id.clone())
                        .or_insert_with(|| c.clone());
                }
                match records.iter_mut().find(|(n, _)| n.id == node.id) {
                    // a record reached through an anchor edge and an evidence edge is anchored
                    Some((_, by)) if matched_by == MatchedBy::Symbol => *by = MatchedBy::Symbol,
                    Some(_) => {}
                    None => records.push((node.clone(), matched_by)),
                }
            }
        }
        records.sort_by(|(a, _), (b, _)| why_order(a, b));
        // leg B: the records governing the defining file, after leg A, never twice
        let mut more = false;
        if let Some(path) = defining_file.as_deref() {
            let (nodes, over) = self.governing_page(
                kinds,
                workspace,
                path_candidates(path),
                None,
                Some(path),
                limit,
            )?;
            more |= over;
            for node in nodes {
                if !records.iter().any(|(n, _)| n.id == node.id) {
                    records.push((node, MatchedBy::PathScope));
                }
            }
        }
        if records.len() > limit {
            records.truncate(limit);
            more = true;
        }
        Ok((
            records
                .into_iter()
                .map(|(node, matched_by)| Governing {
                    anchor_commit: anchor_commits.get(&node.id).cloned(),
                    node,
                    target: key,
                    matched_by,
                    code: Some(code.clone()),
                })
                .collect(),
            more,
        ))
    }

    /// Where a record's history starts: `accepted_commit` (validated by shape before it can
    /// reach argv, M3-6) with the acceptance time as the once-only fallback, the acceptance time
    /// alone when no commit was stamped, nothing for a record that was never accepted.
    fn record_anchor(&self, node: &Node) -> Result<RecordAnchor, ApiError> {
        if node.governance != Governance::Accepted {
            return Ok(RecordAnchor::None("not accepted"));
        }
        let id =
            kaban_core::NodeId::parse(&node.id).map_err(|_| ApiError::NotFound(node.id.clone()))?;
        let accepted_at = self
            .store
            .events(&id)?
            .iter()
            .rev()
            .find(|e| e.kind == "accepted")
            .and_then(|e| OffsetDateTime::from_unix_timestamp(e.ts).ok());
        Ok(match (&node.accepted_commit, accepted_at) {
            (Some(sha), fallback) => RecordAnchor::Known {
                anchor: Anchor::Commit(sha.clone()),
                fallback,
            },
            (None, Some(at)) => RecordAnchor::Known {
                anchor: Anchor::Since(at),
                fallback: None,
            },
            (None, None) => RecordAnchor::None("no acceptance event"),
        })
    }

    /// Currency per found record, in order: records grouped by anchor, one git query per group
    /// over the union of the group's `path_scope` entries, groups on at most 4 scoped threads
    /// (M3-22), exact attribution per record.
    fn currencies(&self, found: &[Governing<'_>]) -> Result<Vec<Currency>, ApiError> {
        let mut out: Vec<Option<Currency>> = vec![None; found.len()];
        let root = self.config.repo_root.as_deref();
        // key: the anchor's argv form; value: (anchor, fallback, member indexes)
        let mut groups: BTreeMap<String, (Anchor, Option<OffsetDateTime>, Vec<usize>)> =
            BTreeMap::new();
        for (i, g) in found.iter().enumerate() {
            if g.node.governance != Governance::Accepted {
                out[i] = Some(Currency::uncheckable("not accepted"));
                continue;
            }
            if root.is_none() {
                out[i] = Some(Currency::uncheckable("no repository"));
                continue;
            }
            if let (Some(recorded), Some(current)) = (g.node.repo.as_deref(), &self.config.repo)
                && recorded != current
            {
                out[i] = Some(Currency::uncheckable("another repository"));
                continue;
            }
            match self.record_anchor(&g.node)? {
                RecordAnchor::None(reason) => out[i] = Some(Currency::uncheckable(reason)),
                RecordAnchor::Known { anchor, fallback } => {
                    let key = match &anchor {
                        Anchor::Commit(sha) => format!("c:{sha}"),
                        Anchor::Since(at) => format!("s:{}", at.unix_timestamp()),
                    };
                    groups
                        .entry(key)
                        .or_insert_with(|| (anchor, fallback, Vec::new()))
                        .2
                        .push(i);
                }
            }
        }
        let groups: Vec<_> = groups.into_values().collect();
        let root = root.unwrap_or_else(|| std::path::Path::new("."));
        let snapshots: Vec<Result<Snapshot, &'static str>> = if groups.len() <= 1 {
            groups
                .iter()
                .map(|(anchor, fallback, members)| {
                    self.group_snapshot(root, anchor, *fallback, members, found)
                })
                .collect()
        } else {
            let chunk = groups.len().div_ceil(MAX_GIT_THREADS);
            std::thread::scope(|s| {
                let handles: Vec<_> = groups
                    .chunks(chunk)
                    .map(|part| {
                        s.spawn(move || {
                            part.iter()
                                .map(|(anchor, fallback, members)| {
                                    self.group_snapshot(root, anchor, *fallback, members, found)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            })
        };
        for ((_, _, members), snap) in groups.iter().zip(snapshots) {
            for &i in members {
                out[i] = Some(match &snap {
                    Ok(snap) => currency::attribute(snap, &found[i].node.path_scope),
                    Err(reason) => Currency::uncheckable(reason),
                });
            }
        }
        Ok(out
            .into_iter()
            .map(|c| c.unwrap_or_else(|| Currency::uncheckable("not computed")))
            .collect())
    }

    fn group_snapshot(
        &self,
        root: &std::path::Path,
        anchor: &Anchor,
        fallback: Option<OffsetDateTime>,
        members: &[usize],
        found: &[Governing<'_>],
    ) -> Result<Snapshot, &'static str> {
        let mut entries: Vec<String> = members
            .iter()
            .flat_map(|&i| found[i].node.path_scope.iter().cloned())
            .collect();
        entries.sort();
        entries.dedup();
        currency::snapshot(root, anchor, fallback, &entries, None, &self.git)
    }

    /// Governance events of one record, oldest first, every string gated on the read path
    /// (M3-6, WG-2): a redact hit masks, a reject hit hides the event (audited once per
    /// `event:<seq>` per policy version). Events are returned only for a record visible in the
    /// read workspace (S6).
    ///
    /// # Errors
    /// `NotFound` (unknown id, hidden record or another workspace), `Rejected` (the id string
    /// hits the denylist), store errors.
    pub fn events(&self, id: &str, workspace_id: Option<&str>) -> Result<Vec<EventView>, ApiError> {
        let t = "events";
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let id = self.gate(&mut ctx, t, "id", id, &mut red)?;
        let workspace_id = self.gate_opt(&mut ctx, t, "workspace_id", workspace_id, &mut red)?;
        self.flush_audit(&mut ctx)?;
        let node_id = Self::node_id(&id)?;
        let ws = self.read_workspace(workspace_id.as_deref());
        let node = self
            .store
            .get(&node_id, false)?
            .filter(|n| ws.as_deref().is_none_or(|ws| n.workspace_id == ws))
            .ok_or_else(|| ApiError::NotFound(id.clone()))?;
        Ok(self
            .store
            .events(&node_id)?
            .iter()
            .filter(|e| e.workspace_id == node.workspace_id)
            .filter_map(|e| self.project_event(e))
            .collect())
    }

    /// Outgoing projection of one event (M3-6): every string of the event (`kind`, `reason`,
    /// every `path_scope` entry, `signer`, `host`, `session_id`, `commit_sha`) is content and
    /// scanned (review R3-4: the events table does not constrain `kind`, and a commit-shaped
    /// value gets no exemption outside the control registry; the argv guard for an event's
    /// commit lives in `currency`). `None` when any string hits a reject rule.
    pub(crate) fn project_event(&self, e: &Event) -> Option<EventView> {
        if !self.scan_on_read() {
            return Some(EventView::raw(e));
        }
        let t = "read";
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let gated = (|ctx: &mut WriteCtx| -> Result<EventView, ApiError> {
            let path_scope = e
                .path_scope
                .iter()
                .map(|p| self.scan_str(ctx, t, "event.path_scope", p, &mut red))
                .collect::<Result<Vec<_>, _>>()?;
            let mut opt = |field: &str, v: Option<&str>| -> Result<Option<String>, ApiError> {
                v.map(|s| self.scan_str(ctx, t, field, s, &mut red))
                    .transpose()
            };
            let signer = opt("event.signer", e.signer.as_deref())?;
            let host = opt("event.host", e.host.as_deref())?;
            let session_id = opt("event.session_id", e.session_id.as_deref())?;
            let commit_sha = opt("event.commit_sha", e.commit_sha.as_deref())?;
            let kind = self.scan_str(ctx, t, "event.kind", &e.kind, &mut red)?;
            let reason = self.scan_str(ctx, t, "event.reason", &e.reason, &mut red)?;
            Ok(EventView {
                seq: e.seq,
                kind,
                reason,
                path_scope,
                signer,
                signer_kind: e.signer_kind,
                host,
                session_id,
                ts: e.ts,
                commit_sha,
            })
        })(&mut ctx);
        self.record_read(&mut ctx, &format!("event:{}", e.seq));
        gated.ok()
    }
}

impl EventView {
    /// The event's strings as stored (no re-scan).
    fn raw(e: &Event) -> Self {
        Self {
            seq: e.seq,
            kind: e.kind.clone(),
            reason: e.reason.clone(),
            path_scope: e.path_scope.clone(),
            signer: e.signer.clone(),
            signer_kind: e.signer_kind,
            host: e.host.clone(),
            session_id: e.session_id.clone(),
            ts: e.ts,
            commit_sha: e.commit_sha.clone(),
        }
    }
}
