//! `code_neighbours` (stage1-spec G1-F12, §6): a bounded walk from exactly one node over the
//! requested edge types in both directions, under `Limits::DEFAULT` (depth `1..=3`, 200 edges
//! per hub, 500 in total). `edge_types[]` is a control path (the closed edge-type names, §6.1):
//! a value outside the set is `Invalid`, never echoed. The list limit cuts the nodes after the
//! start (the start is the target, not a result row); an edge survives when both of its ends do.
//! Document-local symbols are hidden and counted (`locals_hidden`) unless `include_locals` (G4).
//! Nodes keep the §6 `(depth, ref)` order; `distance` is measured from `current_path` (G1-F16).

use std::collections::BTreeSet;
use std::str::FromStr;

use crate::api::KabanApi;
use crate::control::ControlPath;
use crate::error::ApiError;
use kaban_codegraph::{CodeGraphReader, Depth, EdgeType, Limits};

use super::{
    CodeError, CodeNeighboursRequest, CodeNeighboursResponse, Edge, cards, coverage, ranking,
    selector,
};

const TOOL: &str = "code_neighbours";

/// The fixed text for an `edge_types[]` value outside the closed set.
const EDGE_TYPES_INVALID: &str = "edge_types[] must name edge types";

impl KabanApi {
    /// The neighbourhood of the target (`why.rs` pipeline: gate, flush, check `current_path`,
    /// resolve, select, read, project, cut).
    ///
    /// # Errors
    /// `Rejected` (a content string hits the denylist), `Invalid` for an unknown edge type or a
    /// fixed `CodeError` class (stage1-spec G1-S8; a depth outside `1..=3` or a `current_path`
    /// failing the repository-relative path rule is `selector_invalid`), store errors from the
    /// audit flush.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn code_neighbours(
        &self,
        req: CodeNeighboursRequest,
    ) -> Result<CodeNeighboursResponse, ApiError> {
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let target = self.gate_selector(&mut ctx, TOOL, &req.target, &mut red)?;
        let mut names = Vec::with_capacity(req.edge_types.len());
        for t in &req.edge_types {
            names.push(self.gate_control(
                &mut ctx,
                TOOL,
                ControlPath::request(TOOL, "edge_types[]"),
                "edge_types",
                t,
                &mut red,
            )?);
        }
        let current_path = self.gate_opt(
            &mut ctx,
            TOOL,
            "current_path",
            req.current_path.as_deref(),
            &mut red,
        )?;
        self.flush_audit(&mut ctx)?;
        let inputs = ranking::RankInputs::checked(current_path)?;
        let mut edge_types = Vec::with_capacity(names.len());
        for name in &names {
            edge_types.push(
                EdgeType::from_str(name)
                    .map_err(|_| ApiError::Invalid(EDGE_TYPES_INVALID.to_owned()))?,
            );
        }
        let depth = Depth::new(req.depth, &Limits::DEFAULT).ok_or(CodeError::SelectorInvalid)?;
        let resolved = self.resolve_current()?;
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let selector::Found { selection, note } = self.select(TOOL, &target, reader)?;
        let start = self.select_one(TOOL, selection)?;
        let graph = reader
            .neighbours_with(start.id, depth, &edge_types, req.include_locals)
            .map_err(CodeError::from)?;
        // the start node is never counted: the limit cuts what follows it
        let keep = ranking::effective_limit(req.limit).saturating_add(1);
        let cut = graph.nodes.len() > keep;
        let nodes: Vec<_> = graph
            .nodes
            .iter()
            .take(keep)
            .map(|row| {
                let mut card = cards::project(row, req.include_snippets);
                inputs.fill(&mut card);
                card
            })
            .collect();
        let kept: BTreeSet<&str> = nodes.iter().map(|n| n.r#ref.as_str()).collect();
        let edges = graph
            .edges
            .iter()
            .filter(|e| kept.contains(e.from.as_str()) && kept.contains(e.to.as_str()))
            .map(|e| Edge {
                from: e.from.clone(),
                r#type: e.r#type.as_str().to_owned(),
                to: e.to.clone(),
            })
            .collect();
        Ok(CodeNeighboursResponse {
            snapshot: self.snapshot_wire(&resolved.r#ref),
            target_note: note,
            coverage: coverage::summary_of(reader)?,
            nodes,
            edges,
            truncated: graph.truncated || cut,
            locals_hidden: graph.locals_hidden,
        })
    }
}
