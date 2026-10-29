//! Governance events (core-spec §4): the append-only record of `accepted`, `superseded` and
//! `invalidated` transitions, and the outcome type of `forget`.

use crate::lifecycle::{Governance, Lifecycle};
use serde::{Deserialize, Serialize};

/// Who signed a governance event; matches the `events.signer_kind` CHECK vocabulary.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SignerKind {
    /// A human on the CLI.
    HumanCli,
    /// A human through a reviewed pull request (committed bytes, OD-15).
    HumanPr,
    /// A human confirming through the chat handshake (F4).
    HumanViaChat,
    /// An agent acting on its own authority (default).
    #[default]
    Agent,
    /// kaban itself (migrations, `link` side effects, rescans).
    System,
}

impl SignerKind {
    /// Stable kebab-case name used in the `events.signer_kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HumanCli => "human-cli",
            Self::HumanPr => "human-pr",
            Self::HumanViaChat => "human-via-chat",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
    /// Parse a column value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "human-cli" => Some(Self::HumanCli),
            "human-pr" => Some(Self::HumanPr),
            "human-via-chat" => Some(Self::HumanViaChat),
            "agent" => Some(Self::Agent),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// Input of a governance transition: the reason and provenance recorded with the event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewEvent {
    /// Why the transition happened (never note text; gated like every other string).
    pub reason: String,
    /// Globs/paths the record covers after this event (empty = unchanged).
    pub path_scope: Vec<String>,
    /// Signer identity when recorded (`record_signer_identity`).
    pub signer: Option<String>,
    /// Who signed.
    pub signer_kind: SignerKind,
    /// Host name when recorded.
    pub host: Option<String>,
    /// Session that produced the event.
    pub session_id: Option<String>,
    /// Commit at the time of the event (becomes `accepted_commit` on acceptance).
    pub commit_sha: Option<String>,
}

impl From<crate::audit::Actor> for SignerKind {
    /// The signer kind of an event written on behalf of the write context's actor (`link`
    /// side effects, `forget(invalidate)`).
    fn from(actor: crate::audit::Actor) -> Self {
        match actor {
            crate::audit::Actor::HumanCli => Self::HumanCli,
            crate::audit::Actor::Agent => Self::Agent,
        }
    }
}

/// A stored governance event (append-only, F17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Position in the global event log.
    pub seq: i64,
    /// Node the event belongs to.
    pub node_id: String,
    /// `accepted`, `superseded` or `invalidated` (plus the migration's `merged-duplicate` and
    /// the rescan job's `tombstoned` with reason `policy-reject`).
    pub kind: String,
    /// Why.
    pub reason: String,
    /// Path scope recorded with the event.
    pub path_scope: Vec<String>,
    /// Signer identity when recorded.
    pub signer: Option<String>,
    /// Who signed.
    pub signer_kind: SignerKind,
    /// Host when recorded.
    pub host: Option<String>,
    /// Session when recorded.
    pub session_id: Option<String>,
    /// Unix time (seconds) of the event.
    pub ts: i64,
    /// Commit at the time of the event.
    pub commit_sha: Option<String>,
    /// Workspace of the node (F17 tenant key).
    pub workspace_id: String,
}

/// A confirmation row to create (core-plan C3.2, PR-3): the handshake token is kept as its
/// keyed hash only, so the MCP process that issued it and the CLI process that redeems it can
/// differ. `confirmations` is database-scoped (F17): no workspace column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewConfirmation {
    /// `HMAC(audit_key, "kaban-confirm/v1\0" + token)`, lowercase hex. Never the token.
    pub token_hash: String,
    /// The Decision to accept.
    pub node_id: String,
    /// Acceptance reason (gated content).
    pub reason: String,
    /// Acceptance scope (gated content).
    pub path_scope: Vec<String>,
    /// Who asked (the actor kind, `agent`).
    pub requested_by: String,
    /// Host the request came from, if known.
    pub host: Option<String>,
    /// Session the request came from, if known.
    pub session_id: Option<String>,
    /// Lifetime in seconds (600).
    pub ttl_s: u32,
}

/// A consumed confirmation row (its request strings; redeemed once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// The Decision the token was issued for.
    pub node_id: String,
    /// Acceptance reason as recorded at issuance.
    pub reason: String,
    /// Acceptance scope as recorded at issuance.
    pub path_scope: Vec<String>,
    /// Who asked.
    pub requested_by: String,
    /// Host at issuance.
    pub host: Option<String>,
    /// Session at issuance.
    pub session_id: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub expires_at: i64,
}

/// What `forget` left behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetOutcome {
    /// Lifecycle after the call.
    pub lifecycle: Lifecycle,
    /// Governance after the call.
    pub governance: Governance,
    /// Highest `changes.seq` written by this call (F11).
    pub outbox_seq: u64,
    /// A purge committed: the content is unavailable to every read from now on (core-spec S5).
    /// The other modes keep the content and revoke nothing.
    pub revoked: bool,
    /// The purge's freed pages were reclaimed by this call. The post-commit cleanup (index
    /// segments merged, the WAL folded into the main file and truncated) reclaims none, so a
    /// purge reports `false` with `cleanup` naming the residue; `false` for the other modes.
    pub physically_cleaned: bool,
    /// Why a purge is not physically cleaned: the free-page residue a VACUUM removes (`kaban
    /// doctor --optimize --vacuum`), after `busy` (a reader still held the WAL or the lock) or
    /// the failure findings when a cleanup step did not run through. The revocation stands.
    pub cleanup: Option<String>,
}
