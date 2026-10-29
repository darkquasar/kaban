//! `accept`: the governance handshake.

use super::card::Card;
use super::receipt::WriteReceipt;
use crate::accept::ConfirmationToken;
use kaban_core::SignerKind;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `accept` input (core-spec F4; core-plan C3.2). The actor is not on the wire: the adapter
/// supplies it (M3-17).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AcceptRequest {
    /// Id of the Decision, Convention or Failure to accept.
    pub id: String,
    /// Why it is accepted. Required, except when a confirmation token is redeemed (the
    /// confirmed request's reason applies and this one must be absent or equal).
    pub reason: Option<String>,
    /// Paths or globs the record governs, relative to the repository root. Required, except
    /// when a confirmation token is redeemed (same rule as `reason`).
    #[serde(default)]
    pub path_scope: Vec<String>,
    /// Declared signer identity (free text, recorded on the event; not proof).
    pub signer: Option<String>,
    /// The one-time token of an earlier `confirmation_required` response. A kaban-issued
    /// secret: validated by shape, never scanned, logged, echoed or stored (only its keyed
    /// hash is compared).
    #[serde(default, skip_serializing)]
    pub confirmation_token: Option<ConfirmationToken>,
    /// The user confirmed the acceptance in chat; together with `confirmation_token` this
    /// redeems the token (event `signer_kind: human-via-chat`).
    #[serde(default)]
    pub confirmed_by_user: bool,
}

/// `accept` output (core-spec F4).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // one response per call; the card is the payload
pub enum AcceptResponse {
    /// The record is accepted; the event was appended.
    Accepted {
        /// The accepted record.
        node: Card,
        /// Sequence number of the `accepted` event.
        event_seq: u64,
        /// Who signed the acceptance (`agent`, `human-cli`, `human-via-chat`).
        signer_kind: SignerKind,
        /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog`
        /// position.
        receipt: WriteReceipt,
    },
    /// A Decision needs human confirmation: ask the user with `prompt`, then call `accept`
    /// again with `confirmation_token` and `confirmed_by_user: true` (or redeem it on the CLI).
    ConfirmationRequired {
        /// Human-readable question (never contains the token).
        prompt: String,
        /// The one-time token, shown here once (the issuing response) and never again.
        confirmation_token: ConfirmationToken,
        /// Seconds until the token expires.
        expires_in_s: u32,
    },
}
