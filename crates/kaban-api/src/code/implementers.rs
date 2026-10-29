//! `code_implementers` (stage1-spec G1-F12, §8.2): the `IMPLEMENTS` sources of every node the
//! selector matches, external rows only when asked. A bare `path` target is refused: a file
//! implements nothing.

use std::collections::BTreeSet;

use crate::api::KabanApi;
use crate::error::ApiError;
use kaban_codegraph::CodeGraphReader;

use super::{CodeError, CodeImplementersRequest, CodeListResponse, ranking, selector};

const TOOL: &str = "code_implementers";

impl KabanApi {
    /// Implementers of the target (`why.rs` pipeline: gate, flush, check `current_path`,
    /// resolve, select, read, project, rank, cut).
    ///
    /// # Errors
    /// `Rejected` (a content string hits the denylist), `Invalid` with a fixed `CodeError`
    /// class (stage1-spec G1-S8; a `current_path` failing the repository-relative path rule is
    /// `selector_invalid`), store errors from the audit flush.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn code_implementers(
        &self,
        req: CodeImplementersRequest,
    ) -> Result<CodeListResponse, ApiError> {
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
        if selector::is_bare_path(&target) {
            return Err(CodeError::SelectorInvalid.into());
        }
        let inputs = ranking::RankInputs::checked(current_path)?;
        let resolved = self.resolve_current()?;
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let selector::Found { selection, note } = self.select(TOOL, &target, reader)?;
        let targets = selection.into_rows();
        // the union over every matched target, de-duplicated by ref (RV-7: complete, uncut)
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        for t in &targets {
            for row in reader
                .implementers(t.id, req.include_external)
                .map_err(CodeError::from)?
            {
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
