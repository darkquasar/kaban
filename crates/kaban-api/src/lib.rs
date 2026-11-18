//! kaban service layer: [`KabanApi`] is the one typed capability surface that every adapter
//! (MCP stdio, CLI, loopback HTTP) exposes without adding behaviour.
//!
//! Every string that enters or leaves the service passes through the denylist gate
//! ([`KabanApi::sanitize`]); writes are rejected or redacted before anything is stored and
//! outgoing text is re-scanned when `scan_on_read` is set.
//! Adapters call exactly two seam entries around a tool call: [`KabanApi::prepare_args`] on the
//! raw arguments and [`KabanApi::gate_response`] on the serialized result; the [`control`]
//! registry decides which string leaves are kaban-shaped control values (validated, not scanned).

mod api;
mod code;
mod coderef;
pub mod control;
pub mod enrich;
mod error;
mod git;
pub mod hooks;
mod inventory;
pub mod ontology;
pub mod path_scope;
pub mod producers;
mod project;
mod rescan;
mod sanitize;
mod snapshot; // the CG-JOIN v1 §2/§4 adapter: the one `SnapshotAttach` constructor (E2.6)
mod types;

pub use api::doctor::{DoctorCheck, DoctorContext, DoctorReport};
pub use api::{ApiConfig, CatalogMaintenance, KabanApi};
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
pub use hooks::{HookEvent, HookHost, HookOutcome};
pub use ontology::{LoadReport, OntologyRegistry, OntologySummary, validate_kind_props};
pub use producers::{BudgetSource, BudgetView, RunFlags, budgets};
pub use rescan::RESCAN_BATCH;

pub use types::*;
