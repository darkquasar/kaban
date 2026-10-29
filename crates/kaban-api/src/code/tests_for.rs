//! `code_tests_for` (stage1-spec G1-F12, G1-F6; §4.3 `TESTED_BY`): the tests of every node the
//! selector matches, in three labelled tiers (TESTS-1, backlog 20/24). `direct`: the node's
//! own `TESTED_BY` links; a symbol row takes the reference form (`matched_by: reference`), a
//! file row (the `path` target, or a `file:` ref) the naming form (`matched_by: naming`,
//! `heuristic: true`). `via`: the `TESTED_BY` links of the first [`VIA_INTERMEDIATES`] callers
//! of a symbol row (one `REFERENCES` hop, callers only), each naming its caller. `textual`:
//! only when the two graph tiers are empty, the test functions whose path or snippet spell
//! every word of the target's name (a CLI handler `ingest_finish` is reached by the black-box
//! tests that spawn `ingest ... finish`), `heuristic: true`. Links are unioned by test ref
//! with the strongest tier winning (every target's `direct` links are collected before any
//! `via` link, so the order of a multi-target selection never changes the answer; winA-1),
//! ranked within their tier by the test card's `(distance, path, ref)` (G1-F16, CG8.1; the
//! textual tier measures from the target's own file when no `current_path` is given), tiers
//! kept in order, then cut.

use std::collections::BTreeSet;

use crate::api::KabanApi;
use crate::error::ApiError;
use kaban_codegraph::reader::TestsTarget;
use kaban_codegraph::{CodeGraphReader, NodeKind, NodeRow};

use super::{
    CodeError, CodeTestsForRequest, CodeTestsForResponse, TestLink, cards, coverage, ranking,
    selector,
};

const TOOL: &str = "code_tests_for";

/// The callers consulted by the `via` tier, in `(path, ref)` order.
const VIA_INTERMEDIATES: usize = 50;

/// The shortest word of a name the textual tier matches on (`of`, `id` and `ok` would match
/// nearly every test path).
const TEXTUAL_MIN_WORD: usize = 3;

impl KabanApi {
    /// The tests linked to the target (`why.rs` pipeline: gate, flush, check `current_path`,
    /// resolve, select, read, project, rank, cut).
    ///
    /// # Errors
    /// `Rejected` (a content string hits the denylist), `Invalid` with a fixed `CodeError`
    /// class (stage1-spec G1-S8; a `current_path` failing the repository-relative path rule is
    /// `selector_invalid`), store errors from the audit flush.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn code_tests_for(
        &self,
        req: CodeTestsForRequest,
    ) -> Result<CodeTestsForResponse, ApiError> {
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
        let targets = selection.into_rows();
        let forms: Vec<TestsTarget> = targets
            .iter()
            .map(|t| match (&t.kind, t.file_id) {
                (NodeKind::File, Some(file)) => TestsTarget::File(file),
                _ => TestsTarget::Symbol(t.id),
            })
            .collect();
        // a tier is complete over every target before the next tier starts: with two targets,
        // one referencing the other, the referenced target's `via` pass would otherwise claim
        // the other's direct test first and its direct pass drop it as seen (winA-1)
        let mut seen = BTreeSet::new();
        let mut direct = Vec::new();
        for form in &forms {
            let links = reader.tests_for(*form).map_err(CodeError::from)?;
            direct.extend(project(links, &mut seen, req.include_snippets));
        }
        let mut via = Vec::new();
        for form in &forms {
            if let TestsTarget::Symbol(id) = form {
                let links = reader
                    .tests_via(*id, VIA_INTERMEDIATES)
                    .map_err(CodeError::from)?;
                via.extend(project(links, &mut seen, req.include_snippets));
            }
        }
        // the textual tier is ranked from the caller's file when given, else from the
        // target's own file: the black-box tests beside a CLI handler's crate come before the
        // same words in a far crate
        let mut textual = Vec::new();
        let mut textual_inputs = None;
        if direct.is_empty() && via.is_empty() {
            for t in &targets {
                let words = name_words(&t.name);
                let words: Vec<&str> = words.iter().map(String::as_str).collect();
                let links = reader.tests_textual(&words).map_err(CodeError::from)?;
                textual.extend(project(links, &mut seen, req.include_snippets));
            }
            if req.current_path.is_none()
                && let Some(path) = targets.first().and_then(|t| t.path.clone())
            {
                textual_inputs = ranking::RankInputs::checked(Some(path)).ok();
            }
        }
        let total = direct.len() + via.len() + textual.len();
        let limit = ranking::effective_limit(req.limit);
        let mut links = Vec::with_capacity(total.min(limit));
        for (tier, inputs) in [
            (direct, &inputs),
            (via, &inputs),
            (textual, textual_inputs.as_ref().unwrap_or(&inputs)),
        ] {
            let (ranked, _) = ranking::order_then_limit(tier, inputs, req.limit);
            links.extend(ranked);
        }
        links.truncate(limit);
        Ok(CodeTestsForResponse {
            snapshot: self.snapshot_wire(&resolved.r#ref),
            target_note: note,
            coverage: coverage::summary_of(reader)?,
            tests: links,
            truncated: total > limit,
            follow_ups: Vec::new(),
        })
    }
}

/// The wire links of the reader links not seen before (unioned by test ref across targets and
/// tiers, so a test linked directly is not repeated as `via`).
fn project(
    links: Vec<kaban_codegraph::reader::TestLink>,
    seen: &mut BTreeSet<String>,
    include_snippets: bool,
) -> Vec<TestLink> {
    links
        .into_iter()
        .filter(|link| seen.insert(link.test.r#ref.clone()))
        .map(|link| TestLink {
            test: cards::project(&link.test, include_snippets),
            matched_by: link.matched_by.as_str().to_owned(),
            heuristic: link.heuristic,
            how: link.how.into(),
            via: link
                .via
                .as_ref()
                .map(|caller: &NodeRow| caller.r#ref.clone()),
        })
        .collect()
}

/// The words of a symbol name the textual tier matches on: split at `_` and at a lower-to-upper
/// case change (`ingest_finish` -> `ingest`, `finish`; `FromFullName` -> `from`, `full`,
/// `name`), lowercased, words shorter than [`TEXTUAL_MIN_WORD`] dropped; empty when nothing
/// qualifies (the tier then answers nothing).
fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        let boundary = c == '_' || !c.is_alphanumeric() || (c.is_uppercase() && prev_lower);
        if boundary && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
    }
    if !word.is_empty() {
        words.push(word);
    }
    words.retain(|w| w.chars().count() >= TEXTUAL_MIN_WORD);
    words
}

#[cfg(test)]
mod tests {
    use super::name_words;

    #[test]
    fn name_words_split_snake_and_camel_case_and_drop_short_words() {
        assert_eq!(name_words("ingest_finish"), ["ingest", "finish"]);
        assert_eq!(name_words("FromFullName"), ["from", "full", "name"]);
        assert_eq!(
            name_words("admit_inherited_fds"),
            ["admit", "inherited", "fds"]
        );
        assert_eq!(name_words("a_of_b"), Vec::<String>::new());
        assert_eq!(name_words("HTTPServer2"), ["httpserver2"]);
    }
}
