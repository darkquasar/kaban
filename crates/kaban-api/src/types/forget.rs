//! `forget`.

use super::WriteReceipt;
use kaban_core::{ForgetMode, Governance, Lifecycle};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `forget` input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ForgetRequest {
    /// Node id.
    pub id: String,
    /// tombstone (default: hidden, never re-proposed) | invalidate (tombstone plus governance
    /// `invalidated` with an event) | archive | purge (destructive: removes text, index entries
    /// and edges; the id never reappears)
    pub mode: Option<ForgetMode>,
}

/// `forget` output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ForgetResponse {
    /// Node id.
    pub id: String,
    /// Mode applied.
    pub mode: ForgetMode,
    /// Lifecycle after the call.
    pub lifecycle: Lifecycle,
    /// Governance after the call.
    pub governance: Governance,
    /// Outbox position of the write (F11).
    pub outbox_seq: u64,
    /// A purge committed: the content is unavailable to every read from now on.
    pub revoked: bool,
    /// The purge's freed pages were reclaimed by this call; the post-commit cleanup (index
    /// merged, WAL truncated) reclaims none, so a purge reports `false` with `cleanup` naming
    /// the residue. `false` for the other modes.
    pub physically_cleaned: bool,
    /// Why a purge is not physically cleaned: the free-page residue until a VACUUM, after
    /// `busy` or the failure findings when a cleanup step did not run through; the revocation
    /// stands and `kaban doctor --optimize --vacuum` completes the cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<String>,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog` position.
    pub receipt: WriteReceipt,
}
