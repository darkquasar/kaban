//! `code_members` (stage1-spec G1-F12, §8.2): the direct `CONTAINS` children of every node the
//! selector matches, restricted to `labels` when any are given; for a file node (the `path`
//! form) the file's top-level symbols in range order (G3: file nodes have no `CONTAINS`
//! children, the containment parents are packages and modules). `labels[]` is a control path
//! (closed SCIP kind names, §6.1): a value outside the set is `Invalid`, never echoed.

use std::collections::BTreeSet;

use crate::api::KabanApi;
use crate::control::ControlPath;
use crate::error::ApiError;
use kaban_codegraph::{CodeGraphReader, NodeKind, SCIP_KIND_NAMES};

use super::{
    CodeError, CodeListResponse, CodeMembersRequest, CodeNode, RANKING_FULL, cards, coverage,
    ranking, selector,
};

const TOOL: &str = "code_members";

/// The fixed text for a `labels[]` value outside the closed set.
const LABELS_INVALID: &str = "labels[] must name SCIP kinds";

impl KabanApi {
    /// Members of the target (`why.rs` pipeline: gate, flush, check `current_path`, resolve,
    /// select, read, project, rank, cut).
    ///
    /// # Errors
    /// `Rejected` (a content string hits the denylist), `Invalid` for an unknown label or a
    /// fixed `CodeError` class (stage1-spec G1-S8; a `current_path` failing the
    /// repository-relative path rule is `selector_invalid`), store errors from the audit flush.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn code_members(&self, req: CodeMembersRequest) -> Result<CodeListResponse, ApiError> {
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let target = self.gate_selector(&mut ctx, TOOL, &req.target, &mut red)?;
        let mut labels = Vec::with_capacity(req.labels.len());
        for l in &req.labels {
            labels.push(self.gate_control(
                &mut ctx,
                TOOL,
                ControlPath::request(TOOL, "labels[]"),
                "labels",
                l,
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
        if labels
            .iter()
            .any(|l| !SCIP_KIND_NAMES.contains(&l.as_str()))
        {
            return Err(ApiError::Invalid(LABELS_INVALID.to_owned()));
        }
        let inputs = ranking::RankInputs::checked(current_path)?;
        let resolved = self.resolve_current()?;
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let selector::Found { selection, note } = self.select(TOOL, &target, reader)?;
        let targets = selection.into_rows();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        // a file target (one row: the `path` form resolves to the file node) answers with its
        // top-level symbols in range order, which the `(distance, path, ref)` ranking would undo
        if let [file] = targets.as_slice()
            && file.kind == NodeKind::File
            && let Some(file_id) = file.file_id
        {
            let rows = reader
                .file_members(file_id, &label_refs)
                .map_err(CodeError::from)?;
            let cards: Vec<CodeNode> = rows
                .iter()
                .map(|row| cards::project(row, req.include_snippets))
                .collect();
            let (results, truncated) = ranking::fill_then_limit(cards, &inputs, req.limit);
            return Ok(CodeListResponse {
                snapshot: self.snapshot_wire(&resolved.r#ref),
                target_note: note,
                coverage: coverage::summary_of(reader)?,
                count: results.len(),
                results,
                truncated,
                ranking: RANKING_FULL.to_owned(),
                follow_ups: Vec::new(),
            });
        }
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        for t in &targets {
            for row in reader.members(t.id, &label_refs).map_err(CodeError::from)? {
                if seen.insert(row.r#ref.clone()) {
                    rows.push(row);
                }
            }
        }
        Ok(self.list_response(
            &resolved,
            &rows,
            req.limit,
            &inputs,
            req.include_snippets,
            note,
        )?)
    }
}
