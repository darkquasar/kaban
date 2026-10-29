//! Audit types shared by the service layer and every store: what gets recorded (rule ids,
//! field names, keyed hashes; never the offending value) and the write context that carries
//! the entries into the store's transaction (core-spec S3; core-plan C1.4).

use crate::lifecycle::GovernedSet;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Which gated operation an audit row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditOp {
    /// The denylist refused a write; nothing was stored.
    Reject,
    /// A redact-action hit was masked before storing or returning.
    Redact,
    /// A node was invalidated or archived.
    Forget,
    /// A node was purged (destructive).
    Purge,
    /// A governance acceptance (M3).
    Accept,
}

impl AuditOp {
    /// Stable lowercase name used in the `audit.op` column and exports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Redact => "redact",
            Self::Forget => "forget",
            Self::Purge => "purge",
            Self::Accept => "accept",
        }
    }
}

/// Workspace identifier (core-spec S6). Plain string newtype; `"*"` is the explicit wildcard
/// on reads and never a stored value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// The workspace used when none is bound or requested.
    pub const DEFAULT: &'static str = "default";
    /// The explicit cross-workspace selector on reads (core-spec S6); never stored.
    pub const ALL: &'static str = "*";

    /// Wrap a workspace name.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Whether this is the `"*"` read selector.
    pub fn is_all(&self) -> bool {
        self.0 == Self::ALL
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self(Self::DEFAULT.into())
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who performed a write (agent via MCP, human via CLI). Carried for later governance work;
/// never logged with argument values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Actor {
    /// An agent calling through MCP (default).
    #[default]
    Agent,
    /// A human using the CLI.
    HumanCli,
}

/// The configured writer of this process (spec 05 SS-26): set once by the global
/// `--actor-id` option (`KABAN_ACTOR_ID`) for the CLI and `kaban serve` alike, [`Self::LOCAL`]
/// when unset. Never inferred from an agent id, a session, a PID or the transport. The store
/// stamps it, with the database's instance and pack lineage, on every record and change row
/// the process writes; it is additional to `agent_id`, `session_id` and [`Actor`], never a
/// replacement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActorId(String);

impl ActorId {
    /// The stable default identity, so a setup without the option keeps working unchanged.
    pub const LOCAL: &'static str = "local";
    /// The longest accepted identity, in bytes.
    pub const MAX_LEN: usize = 128;

    /// Validate an identity: 1 to [`Self::MAX_LEN`] bytes, no whitespace and no control
    /// character.
    ///
    /// # Errors
    /// [`InvalidActorId`] for anything else.
    pub fn parse(s: &str) -> Result<Self, InvalidActorId> {
        if s.is_empty()
            || s.len() > Self::MAX_LEN
            || s.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(InvalidActorId);
        }
        Ok(Self(s.to_owned()))
    }
    /// The identity as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ActorId {
    fn default() -> Self {
        Self(Self::LOCAL.into())
    }
}

impl std::fmt::Display for ActorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An actor id outside the [`ActorId::parse`] rule (the value is never echoed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an actor id is 1 to {max} bytes without whitespace or control characters", max = ActorId::MAX_LEN)]
pub struct InvalidActorId;

/// One audit record. Carries rule ids, field names and keyed hashes only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// What happened.
    pub op: AuditOp,
    /// Tool or entry point (`remember`, `read`, `forget`, ...).
    pub tool: String,
    /// Field that was gated (`text`, `props`, `all`, ...).
    pub field: String,
    /// Workspace of the write.
    pub workspace_id: WorkspaceId,
    /// Effective policy version that produced the verdict.
    pub policy_version: String,
    /// `HMAC(audit_key, input)` of the gated string (a stub that cannot confirm a guess).
    pub input_hmac: String,
    /// Rule ids that fired.
    pub rules: Vec<String>,
    /// Node the entry refers to, when known. Read-path entries are deduplicated per
    /// `(node_id, policy_version)`; write-path entries carry `None` before the id exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// Why trusted code removes graph entries. This is not serialized caller authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemovalIntent {
    /// Producer omission: durable pruning protection applies.
    #[default]
    Automatic,
    /// An explicit forget operation overrides pruning protection.
    Explicit,
    /// Policy rejection/redaction overrides pruning protection.
    Policy,
}

/// Context of one gated write: the policy version to stamp, the workspace, the audit entries
/// to persist in the same transaction, the actor, the configured writer and the governed kinds the store decides
/// governance by (engine-spec EN-F4).
#[derive(Debug, Clone)]
pub struct WriteCtx {
    /// Effective policy version stamped on stored nodes.
    pub policy_version: String,
    /// Workspace the write belongs to.
    pub workspace_id: WorkspaceId,
    /// Audit rows to append atomically with the write.
    pub audit: Vec<AuditEntry>,
    /// Who is writing. The configured writer (`--actor-id`, SS-26) is not carried here: the
    /// store stamps the identity it was opened as, the one authority.
    pub actor: Actor,
    /// Trusted removal cause; never selected by a serialized request.
    pub removal_intent: RemovalIntent,
    /// Identity of the repository this process runs in (its root directory name, M3-23), if
    /// any: stamped on a record's `repo` column when it is still unset.
    pub repo: Option<String>,
    /// The governed kinds and `on_link` rels of the loaded ontology (engine-plan E1.4): a new
    /// record of a kind in the set starts `proposed`, every other kind has no governance. The
    /// service sets it from the registry; [`WriteCtx::new`] defaults to the shipped set.
    pub governed: Arc<GovernedSet>,
    /// The discovered-labels rules (engine-plan E1.8, EN-F5): the loaded ontologies' policy
    /// and the per-workspace cap. The service sets it from the registry and the engine
    /// config; [`WriteCtx::new`] defaults to the shipped `review` policy and the register cap.
    pub discovered: crate::model::DiscoveredRules,
    /// The call's terminal `changes.seq` (spec 05 SS-10), recorded by the store from inside
    /// the write transaction right before its commit (the log head at that moment: the call's
    /// last change row, or the head it saw when it wrote none) and read by the service for
    /// the receipt. The store writes it through a shared reference, so the context stays a
    /// borrowed parameter of every write.
    terminal_seq: TerminalSeq,
}

/// The slot a write transaction records its terminal sequence in: unset until the store sets
/// it. Shared-reference writes (the context is borrowed by the store) make it an atomic.
#[derive(Debug)]
pub(crate) struct TerminalSeq(AtomicU64);

impl TerminalSeq {
    /// No position recorded yet (`seq` starts at 1; the head of an empty log is 0).
    const UNSET: u64 = u64::MAX;

    const fn unset() -> Self {
        Self(AtomicU64::new(Self::UNSET))
    }

    fn get(&self) -> Option<u64> {
        Some(self.0.load(Ordering::Acquire)).filter(|seq| *seq != Self::UNSET)
    }
}

impl Clone for TerminalSeq {
    fn clone(&self) -> Self {
        Self(AtomicU64::new(self.0.load(Ordering::Acquire)))
    }
}

impl WriteCtx {
    /// A context with no pending audit entries, no repository identity and the shipped
    /// governed set.
    pub fn new(policy_version: impl Into<String>, workspace_id: WorkspaceId, actor: Actor) -> Self {
        Self {
            policy_version: policy_version.into(),
            workspace_id,
            audit: Vec::new(),
            actor,
            removal_intent: RemovalIntent::Automatic,
            repo: None,
            governed: GovernedSet::shipped(),
            discovered: crate::model::DiscoveredRules::default(),
            terminal_seq: TerminalSeq::unset(),
        }
    }

    /// The store's record of the call's terminal `changes.seq`, made inside the write
    /// transaction right before its commit (the log head read there: with `BEGIN IMMEDIATE`
    /// no other writer commits in between, so it is the call's last change row, or the head
    /// the call saw when it wrote none).
    pub fn record_terminal_seq(&self, seq: u64) {
        self.terminal_seq.0.store(seq, Ordering::Release);
    }

    /// The terminal `changes.seq` the store recorded for this context's write, `None` until a
    /// store call recorded one (the service then falls back to the head read after the
    /// commit, an upper bound).
    #[must_use]
    pub fn terminal_seq(&self) -> Option<u64> {
        self.terminal_seq.get()
    }

    /// The same context with a repository identity.
    #[must_use]
    pub fn with_repo(mut self, repo: Option<String>) -> Self {
        self.repo = repo;
        self
    }

    /// The same context deciding governance by `governed` (the registry's set).
    #[must_use]
    pub fn with_governed(mut self, governed: Arc<GovernedSet>) -> Self {
        self.governed = governed;
        self
    }

    /// The same context placing undeclared labels by `discovered` (the registry's policy and
    /// the engine's cap).
    #[must_use]
    pub fn with_discovered(mut self, discovered: crate::model::DiscoveredRules) -> Self {
        self.discovered = discovered;
        self
    }
}

impl Default for WriteCtx {
    /// An empty policy version, the default workspace, the agent actor and the shipped set
    /// (never an empty set: see [`GovernedSet`]).
    fn default() -> Self {
        Self::new(String::new(), WorkspaceId::default(), Actor::default())
    }
}

/// Result of recomputing the audit chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditVerification {
    /// Number of rows checked.
    pub entries: u64,
    /// Whether every row's HMAC matched and chained onto its predecessor.
    pub ok: bool,
    /// First sequence number that failed, when `ok` is false.
    pub first_bad_seq: Option<u64>,
}
