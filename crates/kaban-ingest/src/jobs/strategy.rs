//! EN-F45 advisory strategy table. Choices never fill records or confer acceptance authority.

use crate::GatedPath;
use crate::extract::structured_prep::schema::{self, SelectionError};
use kaban_core::Ontology;

/// Source class, distinct from the heuristic source-type suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClass {
    /// Ordinary prose and Markdown.
    Prose,
    /// Architecture decisions.
    Adr,
    /// Line-oriented logs and traces.
    Logs,
    /// Turn-oriented chat and sessions.
    Chat,
    /// Structured config key paths.
    Config,
    /// Code belongs to the v4 indexer, never structured prose ingestion.
    Code,
    /// Hunk-oriented diffs and patches.
    Diff,
    /// Ticket/export fields.
    Ticket,
}

/// Advisory table row; not a record producer and not a guarantee of automatic extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strategy {
    /// Human-readable default preparation methods; current structured formats remain bounded.
    pub default_methods: Vec<&'static str>,
    /// Available retained layer suggestions; linking is opt-in, never silently enabled.
    pub default_layers: Vec<&'static str>,
    /// External extractor dialects, never spawned by kaban.
    pub richer: Vec<&'static str>,
    /// Ontology-derived canonical allowed kind names.
    pub kinds: Vec<String>,
    /// Source type whose explicit ontology path claim/suggestion selected those kinds.
    pub source_type: String,
}

/// Fixed advisory refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StrategyError {
    /// Source code must be routed to v4.
    #[error("structured_code_unsupported")]
    Code,
    /// Loaded mapping cannot select safe extractable kinds.
    #[error(transparent)]
    Selection(#[from] SelectionError),
}

/// Choose kinds from loaded ontology data, never from a literal source-type-to-kind table.
/// Explicit ontology path claims take precedence over the heuristic suggestion, in stable order.
///
/// # Errors
/// Refuses code or unsafe/unavailable ontology mappings.
pub fn strategy_for(
    source: SourceClass,
    source_type: &str,
    path: &GatedPath,
    ontology: &Ontology,
) -> Result<Strategy, StrategyError> {
    if source == SourceClass::Code {
        return Err(StrategyError::Code);
    }
    let claims = schema::path_claims(ontology, path)?;
    let selected = claims.first().map_or(source_type, String::as_str);
    let kinds = schema::stored_names(&schema::select_kinds(ontology, selected)?);
    let (default_methods, default_layers, richer) = match source {
        SourceClass::Prose => (
            vec!["chunks", "fts", "optional_gazetteer"],
            vec!["L0", "L1_opt_in"],
            vec!["llm"],
        ),
        SourceClass::Adr => (
            vec!["chunks", "fts", "adr_suggestion", "candidates"],
            vec!["L0", "L1_opt_in"],
            vec!["llm", "gliner"],
        ),
        SourceClass::Logs => (
            vec!["line_chunks", "regex_ids"],
            vec!["L0", "L1_opt_in"],
            vec!["gliner", "llm"],
        ),
        SourceClass::Chat => (
            vec!["turn_chunks", "optional_gazetteer"],
            vec!["L0", "L1_opt_in"],
            vec!["llm"],
        ),
        SourceClass::Config => (
            vec!["structural_keys", "path_mentions"],
            vec!["L0", "L1_opt_in"],
            vec![],
        ),
        SourceClass::Diff => (
            vec!["hunk_chunks", "path_mentions"],
            vec!["L0", "L1_opt_in"],
            vec!["llm"],
        ),
        SourceClass::Ticket => (
            vec!["typed_export_fields"],
            vec!["L0"],
            vec!["llm", "optional_gazetteer"],
        ),
        SourceClass::Code => return Err(StrategyError::Code),
    };
    Ok(Strategy {
        default_methods,
        default_layers,
        richer,
        kinds,
        source_type: selected.to_owned(),
    })
}
