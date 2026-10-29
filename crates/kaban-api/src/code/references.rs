//! `code_references` (stage1-spec G1-F12, §6; §4.3 `REFERENCES`): the reference sites of every
//! node the selector matches, grouped by enclosing node (the file node when the occurrence has
//! no enclosing symbol, G1-F4), sites in test files only with `include_tests`. The complete set
//! of groups is ordered by the enclosing card's `(distance, path, ref)` (G1-F16, CG8.1; the
//! reader's `(path, ref)` order without `current_path`), sites keep their range order (RV-7);
//! the limit counts sites.

use std::collections::BTreeSet;

use crate::api::KabanApi;
use crate::control::ROLE_VALUES;
use crate::error::ApiError;
use kaban_codegraph::CodeGraphReader;

use super::{
    CodeError, CodeReferencesRequest, CodeReferencesResponse, ReferenceGroup, ReferenceSite, cards,
    coverage, ranking, selector,
};

const TOOL: &str = "code_references";

/// The identity of a site in the union over several targets: `(enclosing ref, path, range)`.
type SiteKey = (String, String, (u32, u32, u32, u32));

/// The SCIP `SymbolRole` names of a roles bit set: bit `i` is `ROLE_VALUES[i]` (`Definition` is
/// bit 0; the reader excludes definition sites, so it never appears in a response).
pub(crate) fn decode_roles(bits: u32) -> Vec<String> {
    ROLE_VALUES
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1_u32 << i) != 0)
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

impl KabanApi {
    /// Reference sites of the target by enclosing node (`why.rs` pipeline: gate, flush, check
    /// `current_path`, resolve, select, read, project, rank, cut).
    ///
    /// # Errors
    /// `Rejected` (a content string hits the denylist), `Invalid` with a fixed `CodeError`
    /// class (stage1-spec G1-S8; a `current_path` failing the repository-relative path rule is
    /// `selector_invalid`), store errors from the audit flush.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn code_references(
        &self,
        req: CodeReferencesRequest,
    ) -> Result<CodeReferencesResponse, ApiError> {
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
        // the union over every matched target: one group per enclosing ref in first-seen order,
        // one site per (enclosing, path, range); the reader's order is kept (RV-7)
        let mut groups: Vec<ReferenceGroup> = Vec::new();
        let mut seen: BTreeSet<SiteKey> = BTreeSet::new();
        for t in &targets {
            for group in reader
                .references(t.id, req.include_tests)
                .map_err(CodeError::from)?
            {
                let enclosing = group.enclosing.r#ref.clone();
                let found = groups.iter().position(|g| g.enclosing.r#ref == enclosing);
                let at = found.unwrap_or_else(|| {
                    groups.push(ReferenceGroup {
                        enclosing: cards::project(&group.enclosing, req.include_snippets),
                        sites: Vec::new(),
                    });
                    groups.len().saturating_sub(1)
                });
                let Some(slot) = groups.get_mut(at) else {
                    continue;
                };
                for site in group.sites {
                    let range = (site.site.sl, site.site.sc, site.site.el, site.site.ec);
                    if seen.insert((enclosing.clone(), site.path.clone(), range)) {
                        slot.sites.push(ReferenceSite {
                            site: site.site.into(),
                            path: site.path,
                            roles: decode_roles(site.roles),
                        });
                    }
                }
            }
        }
        // G1-F16 over the complete set of groups: the enclosing card's distance from
        // `current_path`, then (path, ref); without `current_path` this is the reader's order
        for group in &mut groups {
            inputs.fill(&mut group.enclosing);
        }
        groups.sort_by(|a, b| {
            let (a, b) = (&a.enclosing, &b.enclosing);
            a.distance
                .cmp(&b.distance)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.r#ref.cmp(&b.r#ref))
        });
        // the limit counts sites: whole groups while the budget lasts, the last group cut
        let limit = ranking::effective_limit(req.limit);
        let total: usize = groups.iter().map(|g| g.sites.len()).sum();
        let truncated = total > limit;
        let mut budget = limit;
        let mut by_enclosing = Vec::with_capacity(groups.len());
        for mut group in groups {
            if budget == 0 {
                break;
            }
            group.sites.truncate(budget);
            budget = budget.saturating_sub(group.sites.len());
            by_enclosing.push(group);
        }
        Ok(CodeReferencesResponse {
            snapshot: self.snapshot_wire(&resolved.r#ref),
            target_note: note,
            coverage: coverage::summary_of(reader)?,
            count: by_enclosing.iter().map(|g| g.sites.len()).sum(),
            by_enclosing,
            truncated,
            follow_ups: Vec::new(),
        })
    }
}
