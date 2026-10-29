//! `code_context` (stage1-spec G1-F14, G1-N7, §6; stage1-plan CG9.1): the one-call view of a
//! node, composed from the CG7 queries, CG6.1 `why(symbol)` and one bounded `git log`.
//!
//! ```text
//!   CodeContextRequest
//!     | gate, flush, check current_path, resolve, select        (definition.rs pipeline)
//!     v
//!   definition = cards::project(row, true)
//!   legs       = reader.neighbours(row, depth 1, [REFERENCES], no locals)   one call, both ways
//!                  callers: nodes at e.from where e.to == row     (REFERENCES is E -> S)
//!                  callees: nodes at e.to   where e.from == row
//!                  each ranked by (distance, path, ref), cut to 3; approximate: true
//!   tests      = reader.tests_for(row), ranked, cut to 3
//!   governing  = why(symbol) when the card carries a key, else []
//!   last_change = git log -1 --format=%H%x00%aI%x00%s -- <path>  (runner deadline; None on
//!                  any failure, never an error)
//! ```
//!
//! The span records the three budgets of G1-N7 (`reader_ms`, `governance_ms`,
//! `last_change_ms`) and nothing else: no target, no path, no commit. The same three
//! durations come back from the hidden `code_context_timed` (CG10.1: the bench reads the split
//! without a subscriber and at microsecond resolution; `code_context` is its one-line wrapper).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::api::KabanApi;
use crate::error::ApiError;
use crate::git::{self, GitError};
use crate::types::{WhyRequest, WhyTarget};
use kaban_codegraph::reader::TestsTarget;
use kaban_codegraph::{
    CodeGraphReader, Depth, EdgeType, Limits, NodeKind, NodeRow, check_repo_relative_path,
};
use tracing::field::Empty;

use super::{
    CodeContextRequest, CodeContextResponse, CodeError, CodeNode, LastChange, TestLink, cards,
    coverage, ranking, selector,
};

const TOOL: &str = "code_context";

/// Cards per leg and per test list (G1-F14: "up to 3").
const LEG_LIMIT: usize = 3;

/// The `git log` format: commit, author date, subject, NUL-separated (`%x00` never occurs in
/// a commit id or an ISO date, and `%s` is a single line).
const LOG_FORMAT: &str = "--format=%H%x00%aI%x00%s";

impl KabanApi {
    /// The context of the target: definition, approximate call legs, tests, governing records
    /// and the file's last change (`why.rs` pipeline: gate, flush, check `current_path`,
    /// resolve, select, then compose).
    ///
    /// # Errors
    /// `Rejected` (a content string hits the denylist), `Invalid` with a fixed `CodeError`
    /// class (stage1-spec G1-S8; a `current_path` failing the repository-relative path rule is
    /// `selector_invalid`), store errors from the audit flush or from `why`. A git failure is
    /// never one: `last_change` is `None`.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    #[tracing::instrument(
        name = "code_context",
        skip_all,
        fields(reader_ms = Empty, governance_ms = Empty, last_change_ms = Empty)
    )]
    pub fn code_context(&self, req: CodeContextRequest) -> Result<CodeContextResponse, ApiError> {
        self.code_context_timed(req).map(|(response, _)| response)
    }

    /// `code_context` with the three G1-N7 durations of the call (reader, governance,
    /// `last_change`) beside the response; the bench's seam (`kaban bench cg run`,
    /// `code_context_split`), not a tool surface. Errors as `code_context`.
    #[doc(hidden)]
    #[allow(clippy::needless_pass_by_value)] // the `code_context` request contract, by value
    pub fn code_context_timed(
        &self,
        req: CodeContextRequest,
    ) -> Result<(CodeContextResponse, CodeContextTimings), ApiError> {
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

        let started = Instant::now();
        let resolved = self.resolve_current()?;
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let selector::Found { selection, note } = self.select(TOOL, &target, reader)?;
        let row = self.select_one(TOOL, selection)?;
        let mut definition = cards::project(&row, true);
        inputs.fill(&mut definition);
        let (inbound, outbound) = legs(reader, &row, &inputs)?;
        let tests = tests(reader, &row, &inputs)?;
        let reader_took = started.elapsed();
        tracing::Span::current().record("reader_ms", whole_ms(reader_took));

        let started = Instant::now();
        let governing = match definition.symbol.as_deref() {
            Some(key) => {
                self.why(WhyRequest {
                    targets: vec![WhyTarget::Symbol(key.to_owned())],
                    ..WhyRequest::default()
                })?
                .results
            }
            None => Vec::new(),
        };
        let governance_took = started.elapsed();
        tracing::Span::current().record("governance_ms", whole_ms(governance_took));

        let started = Instant::now();
        let last_change = definition
            .path
            .as_deref()
            .and_then(|path| self.last_change(path));
        let last_change_took = started.elapsed();
        tracing::Span::current().record("last_change_ms", whole_ms(last_change_took));

        let response = CodeContextResponse {
            snapshot: self.snapshot_wire(&resolved.r#ref),
            target_note: note,
            coverage: coverage::summary_of(reader)?,
            definition,
            callers: inbound,
            callees: outbound,
            approximate: true,
            tests,
            governing,
            last_change,
            follow_ups: Vec::new(),
        };
        Ok((
            response,
            CodeContextTimings {
                reader: reader_took,
                governance: governance_took,
                last_change: last_change_took,
            },
        ))
    }

    /// The last commit touching `path` under the repository root, through one bounded
    /// `GitRunner::run`; `None` for a path failing the repository-relative rule, a missing
    /// root, a slow or failing git, or output that does not parse (the class is traced, the
    /// path never).
    fn last_change(&self, path: &str) -> Option<LastChange> {
        if check_repo_relative_path(path).is_err() {
            tracing::debug!(class = "path_invalid", "last_change not read");
            return None;
        }
        let Some(root) = self.config.repo_root.as_deref() else {
            tracing::debug!(class = "no_repo", "last_change not read");
            return None;
        };
        let out = match self.git.run(
            root,
            &["--literal-pathspecs", "log", "-1", LOG_FORMAT, "--", path],
        ) {
            Ok(out) => out,
            Err(e) => {
                tracing::debug!(class = git_class(&e), "last_change not read");
                return None;
            }
        };
        let parsed = parse_log(&out);
        if parsed.is_none() {
            tracing::debug!(class = "log_malformed", "last_change not read");
        }
        parsed
    }
}

/// `%H%x00%aI%x00%s` into a [`LastChange`]: three fields, a full 40-hex commit, a non-empty
/// date; anything else (an empty log for an untracked path included) is `None`.
fn parse_log(out: &str) -> Option<LastChange> {
    let mut fields = out.splitn(3, '\0');
    let commit = fields.next()?.trim();
    let author_date = fields.next()?.trim();
    let summary = fields.next()?.trim_end_matches('\n');
    if commit.len() != 40 || !git::is_commit_hex(commit) || author_date.is_empty() {
        return None;
    }
    Some(LastChange {
        commit: commit.to_owned(),
        author_date: author_date.to_owned(),
        summary: summary.to_owned(),
    })
}

/// The runner's failure class for the trace (`head.rs` vocabulary).
fn git_class(e: &GitError) -> &'static str {
    match e {
        GitError::Spawn(_) => "git_spawn",
        GitError::Timeout(_) => "git_timeout",
        GitError::Status(_) => "git_status",
        GitError::Output => "git_output",
    }
}

/// The two `REFERENCES` legs of `row` from one depth-1 neighbourhood: callers are the nodes at
/// `from` of an edge into the row, callees the nodes at `to` of an edge out of it; each leg is
/// ranked by `inputs` and cut to [`LEG_LIMIT`]. The traversal's own `truncated` is not
/// surfaced (the cut to 3 says less than the whole either way).
fn legs(
    reader: &dyn CodeGraphReader,
    row: &NodeRow,
    inputs: &ranking::RankInputs,
) -> Result<(Vec<CodeNode>, Vec<CodeNode>), CodeError> {
    let depth = Depth::new(1, &Limits::DEFAULT).ok_or(CodeError::SelectorInvalid)?;
    let graph = reader
        .neighbours_with(row.id, depth, &[EdgeType::References], false)
        .map_err(CodeError::from)?;
    let by_ref: BTreeMap<&str, &NodeRow> =
        graph.nodes.iter().map(|n| (n.r#ref.as_str(), n)).collect();
    let mut inbound = Vec::new();
    let mut outbound = Vec::new();
    for e in &graph.edges {
        if e.r#type != EdgeType::References || e.from == e.to {
            continue;
        }
        // the far end of an edge touching the row, and which leg it belongs to
        let (leg, far) = if e.to == row.r#ref {
            (&mut inbound, e.from.as_str())
        } else if e.from == row.r#ref {
            (&mut outbound, e.to.as_str())
        } else {
            continue;
        };
        if let Some(n) = by_ref.get(far) {
            leg.push(cards::project(n, false));
        }
    }
    let (inbound, _) = ranking::order_then_limit(inbound, inputs, Some(LEG_LIMIT));
    let (outbound, _) = ranking::order_then_limit(outbound, inputs, Some(LEG_LIMIT));
    Ok((inbound, outbound))
}

/// The `TESTED_BY` links of `row` (`tests_for.rs` forms: a file row takes the naming form),
/// ranked and cut to [`LEG_LIMIT`].
fn tests(
    reader: &dyn CodeGraphReader,
    row: &NodeRow,
    inputs: &ranking::RankInputs,
) -> Result<Vec<TestLink>, CodeError> {
    let form = match (&row.kind, row.file_id) {
        (NodeKind::File, Some(file)) => TestsTarget::File(file),
        _ => TestsTarget::Symbol(row.id),
    };
    let links: Vec<TestLink> = reader
        .tests_for(form)
        .map_err(CodeError::from)?
        .into_iter()
        .map(|link| TestLink {
            test: cards::project(&link.test, false),
            matched_by: link.matched_by.as_str().to_owned(),
            heuristic: link.heuristic,
            how: link.how.into(),
            via: None,
        })
        .collect();
    Ok(ranking::order_then_limit(links, inputs, Some(LEG_LIMIT)).0)
}

/// The three G1-N7 budgets of one `code_context` call, as measured around each part (the
/// reader part: resolve, select, definition, both legs, tests; the governance part:
/// `why(symbol)`; `last_change`: the one git call).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeContextTimings {
    pub reader: Duration,
    pub governance: Duration,
    pub last_change: Duration,
}

/// Whole milliseconds of `took`, saturating (a span field, never a wall clock).
fn whole_ms(took: Duration) -> u64 {
    u64::try_from(took.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // test code: failures are the assertion

    use super::parse_log;

    /// The parser admits exactly the three-field form with a full commit id; a short id,
    /// a missing field, an empty log and a stray trailing newline behave as documented.
    #[test]
    fn parse_log_admits_only_the_three_field_form() {
        let sha = "a".repeat(40);
        let ok = parse_log(&format!(
            "{sha}\x002026-10-26T09:04:36+00:00\x00C2: subject\n"
        ))
        .expect("well-formed");
        assert_eq!(ok.commit, sha);
        assert_eq!(ok.author_date, "2026-10-26T09:04:36+00:00");
        assert_eq!(ok.summary, "C2: subject");
        assert!(parse_log("").is_none(), "empty log");
        assert!(parse_log("abc\n").is_none(), "one field");
        assert!(
            parse_log(&format!("{sha}\x002026-10-26T09:04:36+00:00")).is_none(),
            "two fields"
        );
        assert!(
            parse_log("abcdef1\x002026-10-26T09:04:36+00:00\x00s\n").is_none(),
            "short id"
        );
        assert!(
            parse_log(&format!(
                "{}\x002026-10-26T09:04:36+00:00\x00s\n",
                "G".repeat(40)
            ))
            .is_none(),
            "not hex"
        );
        assert!(
            parse_log(&format!("{sha}\x00\x00s\n")).is_none(),
            "empty date"
        );
    }
}
