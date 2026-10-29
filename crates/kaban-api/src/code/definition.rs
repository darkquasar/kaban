//! `code_definition` (stage1-spec G1-F12): the one node the selector names, with its
//! definition range, signature, doc and snippet (G1-F17: this tool always returns it). A
//! selector matching several nodes is `ambiguous_symbol`.

use crate::api::KabanApi;
use crate::error::ApiError;
use kaban_codegraph::CodeGraphReader;

use super::{CodeDefinitionRequest, CodeDefinitionResponse, cards, coverage, ranking, selector};

const TOOL: &str = "code_definition";

impl KabanApi {
    /// The definition card of the target (`why.rs` pipeline: gate, flush, check `current_path`,
    /// resolve, select, project; `distance` is measured from `current_path`).
    ///
    /// # Errors
    /// `Rejected` (a content string hits the denylist), `Invalid` with a fixed `CodeError`
    /// class (stage1-spec G1-S8; a `current_path` failing the repository-relative path rule is
    /// `selector_invalid`), store errors from the audit flush.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn code_definition(
        &self,
        req: CodeDefinitionRequest,
    ) -> Result<CodeDefinitionResponse, ApiError> {
        let mut ctx = self.write_ctx(None);
        let mut red = Vec::new();
        let target = self.gate_selector(&mut ctx, TOOL, &req.target, &mut red)?;
        let current_path = self.gate_opt(
            &mut ctx,
            TOOL,
            "current_path",
            req.current_path.as_deref(),
            &mut red,
        )?;
        self.flush_audit(&mut ctx)?;
        let inputs = ranking::RankInputs::checked(current_path)?;
        let resolved = self.resolve_current()?;
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let selector::Found { selection, note } = self.select(TOOL, &target, reader)?;
        let row = self.select_one(TOOL, selection)?;
        let mut definition = cards::project(&row, true);
        inputs.fill(&mut definition);
        Ok(CodeDefinitionResponse {
            snapshot: self.snapshot_wire(&resolved.r#ref),
            target_note: note,
            coverage: coverage::summary_of(reader)?,
            definition,
            follow_ups: Vec::new(),
        })
    }
}
