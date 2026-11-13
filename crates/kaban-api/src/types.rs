//! Request and response types of the service layer. Requests carry the field docs that
//! become the MCP tool input schemas; responses serialize to exactly the spike's JSON shapes.
//!
//! One child per tool surface; this file holds the module list and one explicit re-export
//! block per child, so a lane that adds a type touches its child and its block here only.

mod card;
mod forget;
mod ingest;
pub mod ingest_jobs;
mod layer;
mod recall;
mod receipt;
mod remember;
mod rescan;
mod schema;
mod stats;
mod symbol;
mod traverse;

pub use remember::{
    CodeRefWire, LinkRequest, LinkResponse, LinkSpec, RememberRequest, RememberResponse,
    RememberStatus,
};

pub use card::{Card, EdgeCard, Provenance};

pub use recall::{RecallRequest, RecallResponse};

pub use traverse::{AnchoredCard, AnchoredKind, EdgeFilter, TraverseRequest, TraverseResponse};

pub use schema::{Naming, SchemaExtendRequest, SchemaResponse};

pub use stats::{PolicyStats, StatsRequest, StatsResponse};

pub use forget::{ForgetRequest, ForgetResponse};

pub use layer::LayerMaintenanceResponse;
pub use receipt::{ReceiptIdentities, WriteOutcome, WriteReceipt};

pub use rescan::{RescanOutcome, RescanStatus};

pub use symbol::{CodeField, CodeSnapshot, MatchedBy};

pub use ingest::{
    DocumentSummary, DocumentsResource, FailedDocument, GazetteerTelemetry, IndexDocsRequest,
    IndexDocsResponse, IndexStatus,
};

pub use ingest_jobs::{IngestFinishReport, IngestFinishRequest};
pub use ingest_jobs::{IngestSubmitReport, IngestSubmitRequest};
