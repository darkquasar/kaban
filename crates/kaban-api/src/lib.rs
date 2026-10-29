//! kaban service layer: [`KabanApi`] is the one typed capability surface that every adapter
//! (MCP stdio, CLI, loopback HTTP) exposes without adding behaviour.
//!
//! Every string that enters or leaves the service passes through the denylist gate
//! ([`KabanApi::sanitize`]); writes are rejected or redacted before anything is stored and
//! outgoing text is re-scanned when `scan_on_read` is set. Verdicts are recorded in the
//! store's append-only audit chain inside the same transaction as the write (core-spec S3).
//! Adapters call exactly two seam entries around a tool call: [`KabanApi::prepare_args`] on the
//! raw arguments and [`KabanApi::gate_response`] on the serialized result; the [`control`]
//! registry decides which string leaves are kaban-shaped control values (validated, not scanned).

mod accept;
mod api;
mod code;
mod coderef;
pub mod control;
pub mod currency;
pub mod enrich;
mod error;
mod explain;
pub mod export;
mod git;
mod governance_history; // `governance_history` (stage1-spec G1-F13; v4 CG6.2): types re-exported below
pub mod hooks;
mod inventory;
pub mod layers;
pub mod layout;
pub mod ontology;
mod project;
mod rescan;
mod review; // `kaban review` (engine-plan E2.5; EN-F14a)
mod sanitize;
pub mod serialize;
mod snapshot; // the CG-JOIN v1 §2/§4 adapter: the one `SnapshotAttach` constructor (E2.6)
pub mod sync;
mod types;
mod why; // private: the request/response types are re-exported below (R0-7)

pub use accept::{Caller, ConfirmationToken, InvalidToken};
pub use api::DEPRECATE_HUMAN_ONLY;
pub use api::doctor::{DoctorCheck, DoctorContext, DoctorReport};
pub use api::{ApiConfig, KabanApi, UnmergeResponse};
pub use api::{RESOLVE_SYMBOL_HUMAN_ONLY, RESOLVE_SYMBOL_INVALID, ResolveSymbolResponse};
pub use api::{RESOLVE_WINNER_HUMAN_ONLY, ResolveWinnerResponse};
pub use code::{
    CodeContextRequest, CodeContextResponse, CodeDefinitionRequest, CodeDefinitionResponse,
    CodeError, CodeImplementersRequest, CodeIndexResponse, CodeListResponse, CodeMembersRequest,
    CodeNeighboursRequest, CodeNeighboursResponse, CodeNode, CodeRange, CodeReferencesRequest,
    CodeReferencesResponse, CodeTestsForRequest, CodeTestsForResponse, CoverageIndex,
    CoverageSummary, CrateCoverage, Edge, Hint, LastChange, ReferenceGroup, ReferenceSite,
    SnapshotRefWire, SymbolCandidate, SymbolSelector, TargetNote, TestLink, TestTier,
};
pub use coderef::coderefs_from_serena;
pub use enrich::{EnrichBudget, Enricher, Enrichment, FollowUp, FollowUpTool, NoopEnricher};
pub use error::ApiError;
pub use git::{GitError, GitRunner};
pub use governance_history::{GovernanceHistoryRequest, GovernanceHistoryResponse, HistoryRecord};
pub use hooks::{HookEvent, HookHost, HookOutcome};
pub use layers::{BudgetSource, BudgetView, RunFlags, budgets};
// `kaban explain <key>` (engine-plan E2.4; EN-F14) and the one-stop status `explain`
// (engine-plan E2.5; EN-F14a)
pub use explain::{
    Addressable, ExplainRequest, ExplainResponse, ExplainStatusRequest, LineageSource, NextAction,
    PendingStatus, PendingStatusKind, PolicyView, SettledStatus, Status, StatusExplanation,
    parse_address, templates,
};
pub use ontology::{
    DiscoveredKnobs, LoadReport, OntologyRegistry, OntologySummary, validate_kind_props,
};
pub use rescan::RESCAN_BATCH;
pub use review::{ReviewItem, ReviewQuery, ReviewResponse, review};
pub use sanitize::{AUDIT_UNAVAILABLE, RawSecret};
pub use types::*;
