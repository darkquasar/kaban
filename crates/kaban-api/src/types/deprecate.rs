//! `kaban explain <key> --deprecate` (CLI only).

use super::WriteReceipt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `kaban explain <key> --deprecate <reason>` input (engine-spec EN-F12b; CLI only, actor
/// supplied by the adapter).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DeprecateRequest {
    /// The claim's stable key (64 hex digits, EN-F12a).
    pub key: String,
    /// Why the claim is deprecated; free text, content-gated, stored as `rank_reason`.
    pub reason: String,
}

/// `--deprecate` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeprecateResponse {
    /// The claim's key.
    pub key: String,
    /// The rank now stored: always `deprecated`.
    pub rank: String,
    /// The claim's status after its group was re-decided without its vote.
    pub disposition: String,
    /// `field:rule` entries for every redaction applied to the reason.
    pub redactions: Vec<String>,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog` position.
    pub receipt: WriteReceipt,
}
