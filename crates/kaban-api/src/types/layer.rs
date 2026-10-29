//! `retire_producer`, `reindex_layer` and `forget_layer` (CLI-only layer maintenance): the
//! store's bounded report with the write's receipt (spec 05 SS-10).

use super::receipt::WriteReceipt;
use kaban_core::RetireReport;
use serde::{Deserialize, Serialize};

/// A layer maintenance report with its receipt: the report's own fields at the top level of
/// the document and `receipt` beside them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerMaintenanceResponse {
    /// The store's accounting of the maintenance writes.
    #[serde(flatten)]
    pub report: RetireReport,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog`
    /// position.
    pub receipt: WriteReceipt,
}
