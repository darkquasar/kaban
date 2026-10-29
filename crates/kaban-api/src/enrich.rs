//! Composition hints and the enrichment seam (core-spec F19; core-plan C3.9, M3-27).
//!
//! `recall` and `why` responses carry `follow_ups[]`: structured suggestions the agent may run
//! with other tools. kaban never executes them (S8: no network, no process spawn here; this crate
//! constructs no `Command` since v4 CG1.1, the git runner goes through `kaban_core::process`;
//! the workspace's only sites are that runner and the allowlisted M3-22 bench corpus builder,
//! lint `no_spawn_outside_runner`). They are derived from the `CodeRef`
//! pointers of the visible result cards: the `CodeRef` nodes linked `ABOUT` from or
//! `EVIDENCE_FOR` into a card (`remember.refs` and `props.evidence[]`), batch-loaded in one
//! bounded store query (`GraphStore::linked_refs`, M3 review R3-10) and each projected through
//! the read gate (S4) before its **projected** (masked) props become a hint, so a masked
//! `relative_path` stays masked in `args` and a pointer hidden by a reject rule yields nothing.
//! Every string is content: scanned on the way out like the rest of the response.
//!
//! ```text
//!   visible cards [id..]  =>  store.linked_refs(ids, MAX_POINTERS)   -- one SQL, owner order
//!         |                          |  active CodeRef nodes, same workspace as the card
//!         v                          v
//!   project_card (S4) each  ->  Option<CodeRef> (masked props; None when hidden)
//!         |
//!         v
//!   follow_ups(pointers)    ->  serena.find_symbol [+ find_referencing_symbols], deduplicated
//! ```
//!
//! The [`Enricher`] trait is the reserved seam for a later enricher (for example an
//! `McpEnricher` calling Serena): it runs after card assembly, only when the request names it
//! in `enrich[]` and the process configured it, and its output passes the read gate like any
//! other outgoing string. No enricher ships in v3.0; [`NoopEnricher`] is the default
//! implementation.

use crate::api::KabanApi;
use crate::error::ApiError;
use kaban_core::{CodeRef, NodeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// Tools a follow-up may name (closed set; the wire form is the dotted tool name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum FollowUpTool {
    /// Serena `find_symbol` (`name_path` + `relative_path`).
    #[serde(rename = "serena.find_symbol")]
    SerenaFindSymbol,
    /// Serena `find_referencing_symbols` (`name_path` + `relative_path`).
    #[serde(rename = "serena.find_referencing_symbols")]
    SerenaFindReferencingSymbols,
}

/// A suggestion the agent may execute with another tool (core-spec F19). kaban never runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FollowUp {
    /// The tool to call.
    pub tool: FollowUpTool,
    /// Arguments for that tool, built from the masked `CodeRef` props.
    pub args: Value,
    /// Why the suggestion is made (names the pointer, never a node id).
    pub reason: String,
}

/// Bounds an [`Enricher`] run must respect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnrichBudget {
    /// At most this many refs are handed to the enricher.
    pub max_refs: usize,
    /// Wall-clock bound for the whole run.
    pub max_time: Duration,
}

impl Default for EnrichBudget {
    fn default() -> Self {
        Self {
            max_refs: 16,
            max_time: Duration::from_millis(200),
        }
    }
}

/// One piece of enrichment about a `CodeRef` (every string is content, gated on read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Enrichment {
    /// Id of the enricher that produced it.
    pub enricher: String,
    /// The pointer it is about.
    pub r#ref: CodeRef,
    /// The enrichment text.
    pub text: String,
}

/// The enrichment seam (core-spec F19): applied after card assembly, gated by
/// `recall(..., enrich: [id])` and by configuration. Implementations must stay within `budget`.
pub trait Enricher: Send + Sync {
    /// Stable id named in `recall.enrich[]`.
    fn id(&self) -> &str;

    /// Enrich `refs` within `budget`.
    ///
    /// # Errors
    /// Implementation-defined; the error text is gated before it leaves the service.
    fn enrich(&self, refs: &[CodeRef], budget: EnrichBudget) -> Result<Vec<Enrichment>, ApiError>;
}

/// The default enricher: produces nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoopEnricher;

impl Enricher for NoopEnricher {
    #[allow(clippy::unnecessary_literal_bound)] // the trait signature (core-plan C3.9) binds the lifetime
    fn id(&self) -> &str {
        "noop"
    }

    fn enrich(
        &self,
        _refs: &[CodeRef],
        _budget: EnrichBudget,
    ) -> Result<Vec<Enrichment>, ApiError> {
        Ok(Vec::new())
    }
}

/// Upper bound on the pointer rows one response loads (the enrichment budget hands an enricher
/// at most `EnrichBudget::max_refs` of them; the rest only feed `follow_ups`).
const MAX_POINTERS: usize = 64;

/// The `CodeRef` pointer a projected card carries: the masked props of a `CodeRef` card parsed
/// back into the stored form; `None` for every other kind (or props that are not a `CodeRef`).
pub(crate) fn pointer_of(kind: &str, props: &Value) -> Option<CodeRef> {
    if kind != "CodeRef" {
        return None;
    }
    serde_json::from_value(props.clone()).ok()
}

impl KabanApi {
    /// The `CodeRef` pointers of the visible cards `ids`, in card order (core-spec F19; M3
    /// review R3-10): their linked `CodeRef` nodes, loaded in one bounded store query and each
    /// projected through the read gate like a card (S4), so a pointer is masked exactly as its
    /// card would be and absent when a reject rule hides it.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn linked_pointers(&self, ids: &[NodeId]) -> Result<Vec<CodeRef>, ApiError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .store
            .linked_refs(ids, MAX_POINTERS)?
            .iter()
            .filter_map(|n| self.project_card(n, false, None).and_then(|(_, p)| p))
            .collect())
    }
}

/// Follow-ups for the pointers of one response, in card order, duplicates dropped (N5):
/// `serena.find_symbol` for every pointer, plus `serena.find_referencing_symbols` when the
/// pointer names a symbol.
pub(crate) fn follow_ups(pointers: &[CodeRef]) -> Vec<FollowUp> {
    let mut out: Vec<FollowUp> = Vec::new();
    for p in pointers {
        let args = match &p.name_path {
            Some(n) => json!({ "name_path": n, "relative_path": p.relative_path }),
            None => json!({ "relative_path": p.relative_path }),
        };
        let at = match &p.name_path {
            Some(n) => format!("{n} in {}", p.relative_path),
            None => p.relative_path.clone(),
        };
        let mut suggested = vec![FollowUp {
            tool: FollowUpTool::SerenaFindSymbol,
            args: args.clone(),
            reason: format!("a card in this result points at {at}; read the symbol"),
        }];
        if p.name_path.is_some() {
            suggested.push(FollowUp {
                tool: FollowUpTool::SerenaFindReferencingSymbols,
                args,
                reason: format!("a card in this result points at {at}; find its references"),
            });
        }
        for f in suggested {
            if !out.iter().any(|o| o.tool == f.tool && o.args == f.args) {
                out.push(f);
            }
        }
    }
    out
}

/// The error for an `enrich[]` id no configured enricher answers to (M3-27); `configured` are
/// the ids this process knows, `none` when there are none.
pub(crate) fn unknown_enricher(id: &str, configured: &[&str]) -> ApiError {
    let list = if configured.is_empty() {
        "none".to_owned()
    } else {
        configured.join(", ")
    };
    ApiError::Invalid(format!("unknown enricher '{id}'; configured: {list}"))
}
