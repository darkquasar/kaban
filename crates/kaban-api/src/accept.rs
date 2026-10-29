//! `accept` (core-spec F4; core-plan C3.2): the tiered gate and the confirmation handshake.
//! The tier is the kind's `governed.acceptance` in the loaded ontology (engine-spec EN-F4;
//! shipped `adr`: Decision `human`, Convention and Failure `agent`).
//!
//! ```text
//!   caller (adapter-supplied, M3-17)      acceptance    result
//!   ----------------------------------    -----------   --------------------------------------
//!   agent                                 agent         accepted, signer_kind agent
//!   human-cli                             any governed  accepted, signer_kind human-cli
//!   agent                                 human         confirmation_required + one-time token
//!   any, token + confirmed_by_user        human         token redeemed once, human-via-chat
//! ```
//!
//! The token is a kaban-issued secret (OWN-10, review RC-5): 32 random bytes as 64 lowercase hex
//! digits, held in zeroizing memory, returned once in the issuing response, compared through a
//! domain-separated keyed hash (`HMAC(audit_key, "kaban-confirm/v1\0" + token)`) in constant
//! time, and persisted as that hash only. It never reaches tracing, error text, the human
//! prompt, audit `input_hmac` or any other response. A value under `confirmation_token` that is
//! not 64 lowercase hex digits is content: the pre-parse scan rejects it (`credential-key`).

use crate::api::KabanApi;

/// `props.proposed_by` of the record the reconciler proposes for a span winner without a
/// resolved target (engine-spec EN-F13; the store's `materialise::PROPOSED_BY`).
pub(crate) const RECONCILE_PROPOSED_BY: &str = "kaban-reconcile";
use crate::control::{ControlPath, Shape};
use crate::error::ApiError;
use crate::types::{AcceptRequest, AcceptResponse, Card, LinkRequest, LinkResponse};
use kaban_core::ontology::meta::Acceptance as Tier;
use kaban_core::{
    Actor, AuditOp, Governance, NewConfirmation, NewEvent, Node, NodeId, SignerKind, WriteCtx,
};
use kaban_policy::AuditSigner;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

/// Token lifetime (core-spec F4: 10 min).
const TOKEN_TTL_S: u32 = 600;

/// Domain separator of the confirmation hash (M3-17): distinct from audit `input_hmac`.
const HASH_DOMAIN: &[u8] = b"kaban-confirm/v1\0";

/// Longest title quoted in the confirmation prompt.
const PROMPT_TITLE_CHARS: usize = 120;

/// Who calls `accept` (M3-17): supplied by the adapter, never read from the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    /// `Agent` (MCP) or `HumanCli` (the `kaban accept` command).
    pub kind: Actor,
    /// Host the call comes from (MCP: the gated client name), if known.
    pub host: Option<String>,
    /// Session the call comes from, if known.
    pub session: Option<String>,
}

/// A value under `confirmation_token` that is not a kaban-issued token.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("confirmation_token: expected 64 lowercase hex digits")]
pub struct InvalidToken;

/// A kaban-issued one-time confirmation token: 64 lowercase hex digits in zeroizing memory.
/// `Debug` never shows it; `Serialize` writes the digits (the issuing response is the one place
/// it is serialized; requests skip the field).
#[derive(Clone)]
pub struct ConfirmationToken(Zeroizing<String>);

impl ConfirmationToken {
    /// A fresh token: 32 random bytes from the OS.
    ///
    /// # Errors
    /// `ApiError::Invalid` when the OS random source fails (nothing is issued).
    pub fn generate() -> Result<Self, ApiError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut())
            .map_err(|_| ApiError::Invalid("confirmation token: random source failed".into()))?;
        Ok(Self(Zeroizing::new(hex::encode(bytes.as_ref()))))
    }

    /// Validate a pasted or deserialized value by shape (`Shape::Token64`).
    ///
    /// # Errors
    /// [`InvalidToken`]; the value is not echoed.
    pub fn parse(s: &str) -> Result<Self, InvalidToken> {
        if Shape::Token64.accepts(s) {
            Ok(Self(Zeroizing::new(s.to_owned())))
        } else {
            Err(InvalidToken)
        }
    }

    /// The domain-separated keyed hash that is persisted and compared.
    pub(crate) fn hash(&self, signer: &dyn AuditSigner) -> String {
        let mut data = Zeroizing::new(Vec::with_capacity(HASH_DOMAIN.len() + self.0.len()));
        data.extend_from_slice(HASH_DOMAIN);
        data.extend_from_slice(self.0.as_bytes());
        signer.hmac_hex(&data)
    }
}

impl std::fmt::Debug for ConfirmationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfirmationToken([redacted])")
    }
}

impl Serialize for ConfirmationToken {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ConfirmationToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Zeroizing::new(String::deserialize(deserializer)?);
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ConfirmationToken {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ConfirmationToken".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{64}$",
            "description": "kaban-issued one-time confirmation token (64 lowercase hex digits)"
        })
    }
}

/// `path_scope` entries as git pathspecs (M3-21): the canonical set (`\` becomes `/`, an
/// entry named twice is kept once, R3-6); empty entries, `..` components, absolute paths and
/// pathspec magic (a leading `:`) are refused.
fn validate_path_scope(entries: &[String]) -> Result<Vec<String>, ApiError> {
    crate::currency::canonical_path_scope(entries)
        .into_iter()
        .map(|p| {
            let bad = p.trim().is_empty()
                || p.starts_with('/')
                || p.starts_with(':')
                || p.split('/').any(|c| c == "..");
            if bad {
                Err(ApiError::Invalid(
                    "path_scope entries must be relative paths or globs without `..`".into(),
                ))
            } else {
                Ok(p)
            }
        })
        .collect()
}

/// First line of a title, bounded, for the prompt (`label`: the kind's display label).
fn title_of(card: Option<Card>, label: &str) -> String {
    card.and_then(|c| c.summary.or(c.text))
        .and_then(|s| s.lines().next().map(str::to_owned))
        .map_or_else(
            || format!("this {label}"),
            |line| {
                if line.chars().count() > PROMPT_TITLE_CHARS {
                    let cut: String = line.chars().take(PROMPT_TITLE_CHARS).collect();
                    format!("\"{cut}\u{2026}\"")
                } else {
                    format!("\"{line}\"")
                }
            },
        )
}

/// The gated strings of one acceptance, assembled before the store is touched.
struct Acceptance {
    reason: String,
    path_scope: Vec<String>,
    signer: Option<String>,
    host: Option<String>,
    session: Option<String>,
}

impl KabanApi {
    /// Accept a governed record (core-spec F4): a kind with `acceptance: agent` (Conventions,
    /// Failures) by anyone, a kind with `acceptance: human` (Decisions) by a human on the CLI,
    /// or by an agent only through the confirmation handshake. See the module docs for the
    /// table.
    ///
    /// # Errors
    /// `Invalid` (empty `reason`/`path_scope`, a bad path, a token that is unknown, expired,
    /// consumed or issued for another node: `confirmation_token`), `Rejected` (denylist),
    /// `NotFound` (unknown id or another workspace, S6), `Store(InvalidTransition)` (kind,
    /// governance or a non-active record).
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn accept(&self, p: AcceptRequest, caller: &Caller) -> Result<AcceptResponse, ApiError> {
        if p.path_scope.len() > crate::api::MAX_PATH_SCOPE {
            return Err(ApiError::Invalid("too many path_scope entries".into()));
        }
        let t = "accept";
        let mut ctx = self.write_ctx(None).with_repo(self.config.repo.clone());
        ctx.actor = caller.kind.clone();
        let mut red = Vec::new();
        // the id is control (validated by shape); every other string is content and gated
        // before any lookup (review R1-3); the token is never gated: it is a secret control
        // value the pre-parse scan already validated by shape
        let id = self.gate_control(
            &mut ctx,
            t,
            ControlPath::request(t, "id"),
            "id",
            &p.id,
            &mut red,
        )?;
        let reason = self.gate_opt(&mut ctx, t, "reason", p.reason.as_deref(), &mut red)?;
        let path_scope = validate_path_scope(&p.path_scope)?;
        let path_scope = self.gate_all(&mut ctx, t, "path_scope", &path_scope, &mut red)?;
        let signer = self.gate_opt(&mut ctx, t, "signer", p.signer.as_deref(), &mut red)?;
        let host = self.gate_opt(&mut ctx, t, "host", caller.host.as_deref(), &mut red)?;
        let session = self.gate_opt(
            &mut ctx,
            t,
            "session_id",
            caller.session.as_deref(),
            &mut red,
        )?;
        self.flush_audit(&mut ctx)?;
        let node_id = Self::node_id(&id)?;
        let node = self
            .store
            .get(&node_id, true)?
            .filter(|n| n.workspace_id == self.config.workspace.as_str())
            .ok_or_else(|| ApiError::NotFound(id.clone()))?;

        // redeem: the confirmed request's strings win (M3-17), re-gated before they are
        // persisted again (M3-6)
        if let (Some(token), true) = (&p.confirmation_token, p.confirmed_by_user) {
            let row = self
                .store
                .confirmation_consume(&token.hash(self.signer.as_ref()), &node_id)?
                .ok_or_else(|| ApiError::Invalid("confirmation_token".into()))?;
            let row_reason = self.gate(&mut ctx, t, "reason", &row.reason, &mut red)?;
            let row_scope = self.gate_all(&mut ctx, t, "path_scope", &row.path_scope, &mut red)?;
            let row_host = self.gate_opt(&mut ctx, t, "host", row.host.as_deref(), &mut red)?;
            let row_session = self.gate_opt(
                &mut ctx,
                t,
                "session_id",
                row.session_id.as_deref(),
                &mut red,
            )?;
            if reason.as_ref().is_some_and(|r| *r != row_reason)
                || (!path_scope.is_empty() && path_scope != row_scope)
            {
                self.flush_audit(&mut ctx)?;
                return Err(ApiError::Invalid(
                    "reason and path_scope must be absent or equal to the confirmed request".into(),
                ));
            }
            let a = Acceptance {
                reason: row_reason,
                path_scope: row_scope,
                signer,
                host: host.or(row_host),
                session: session.or(row_session),
            };
            return self.accept_now(ctx, &node, a, SignerKind::HumanViaChat);
        }

        let reason = reason
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| ApiError::Invalid("reason is required".into()))?;
        if path_scope.is_empty() {
            return Err(ApiError::Invalid("path_scope must not be empty".into()));
        }
        let not_active = || -> ApiError {
            kaban_core::StoreError::from(kaban_core::InvalidTransition::NotActive(node.lifecycle))
                .into()
        };
        // EN-F5a (E1.9a RE3-1): a review candidate of an undeclared kind is accepted through
        // the store's candidate branch (the row is released in the acceptance transaction, the
        // governance column stays `none`); the §4 table is consulted for governed kinds only,
        // since an `x:` kind is never in it. A non-active record is refused as for governed
        // kinds; a released `x:` node is no candidate and falls through to the table, which
        // refuses it as before.
        // EN-F13 (E2.5b): the `proposed` record the reconciler materialised for an ungoverned
        // declared kind (`props.proposed_by = kaban-reconcile`) is promoted through the same
        // branch: the store moves it to `accepted` with its event and the EN-F12c claim.
        // OWN-84 (1) (E2.5g): the F4 tier applies here too: `GovernedSet::tier` (the kind's
        // `acceptance`, else the `discovered` block's, else `agent`); at `human` an agent is
        // refused `human_only` and never offered a token (the handshake is the governed
        // kinds', OWN-84 (3)); at `agent` it is accepted with `signer_kind: agent`, the
        // request's `signer` and the client name as the event's record of who (default (b))
        let reconcile_proposed = node.governance == Governance::Proposed
            && node
                .props
                .get("proposed_by")
                .and_then(serde_json::Value::as_str)
                == Some(RECONCILE_PROPOSED_BY);
        if !ctx.governed.contains(&node.kind)
            && (reconcile_proposed || self.store.is_candidate(&node_id)?)
        {
            let signer_kind = match (ctx.governed.tier(&node.kind), &caller.kind) {
                (Tier::Human, Actor::Agent) => {
                    return Err(kaban_core::StoreError::HumanOnly("accept_candidate").into());
                }
                (_, Actor::Agent) => SignerKind::Agent,
                (_, Actor::HumanCli) => SignerKind::HumanCli,
            };
            if node.lifecycle != kaban_core::Lifecycle::Active {
                return Err(not_active());
            }
            let a = Acceptance {
                reason,
                path_scope,
                signer,
                host,
                session,
            };
            return self.accept_now(ctx, &node, a, signer_kind);
        }
        // the §4 table (under the loaded governed set, E1.4/E1.5) and the lifecycle rule are
        // checked before a token is issued, so a record that cannot be accepted gets the
        // transition error, not a token
        ctx.governed
            .governance_transition(&node.kind, node.governance, Governance::Accepted)
            .map_err(kaban_core::StoreError::from)?;
        if node.lifecycle != kaban_core::Lifecycle::Active {
            return Err(not_active());
        }
        let a = Acceptance {
            reason,
            path_scope,
            signer,
            host,
            session,
        };
        // the tier is the mixin's (`NotGoverned` above rules out a kind without one)
        match (ctx.governed.acceptance(&node.kind), &caller.kind) {
            (Some(Tier::Human), Actor::Agent) => self.issue_confirmation(&ctx, &node, a),
            (_, Actor::Agent) => self.accept_now(ctx, &node, a, SignerKind::Agent),
            (_, Actor::HumanCli) => self.accept_now(ctx, &node, a, SignerKind::HumanCli),
        }
    }

    /// Append the `accepted` event (with `accepted_commit` from `git rev-parse HEAD` when the
    /// process runs in a repository) and project the record.
    fn accept_now(
        &self,
        mut ctx: WriteCtx,
        node: &Node,
        a: Acceptance,
        signer_kind: SignerKind,
    ) -> Result<AcceptResponse, ApiError> {
        let commit_sha = self
            .config
            .repo_root
            .as_deref()
            .and_then(|root| self.git.head(root));
        let ev = NewEvent {
            reason: a.reason,
            path_scope: a.path_scope,
            signer: a.signer,
            signer_kind,
            host: a.host,
            session_id: a.session,
            commit_sha,
        };
        let mut entry = self.entry_for(&ctx, AuditOp::Accept, "accept", "node", &node.id, vec![]);
        entry.node_id = Some(node.id.clone());
        ctx.audit.push(entry);
        let node_id = NodeId::parse(&node.id).map_err(|_| ApiError::NotFound(node.id.clone()))?;
        let (accepted, event_seq) =
            self.store
                .transition(&node_id, Governance::Accepted, ev, &ctx)?;
        self.export_audit(&ctx.audit);
        // the barrier after the acceptance's transaction (SS-10), once per public call
        let receipt = self.await_searchable(self.committed_receipt(ctx.terminal_seq()));
        // EN-F16: the acceptance is evidence for every reconciled winner or candidate claim
        // of the record's groups (its own human claim counts nothing); the acceptance stands
        // whatever the counters do, so a failed count is logged, never raised
        match self
            .store
            .priors_obs_bump(&node_id, kaban_core::PriorsOutcome::Accepted)
        {
            Ok(counted) => tracing::debug!(counted, "priors_obs: accepted"),
            Err(e) => tracing::warn!(error = %e, "priors_obs: the accepted count failed"),
        }
        let card = self.project(&accepted, false, None).ok_or_else(|| {
            // the write happened; the card is hidden by a reject rule like every read of it
            ApiError::Rejected {
                field: "node".into(),
                rules: vec!["read-hidden".into()],
            }
        })?;
        Ok(AcceptResponse::Accepted {
            node: card,
            event_seq,
            signer_kind,
            receipt,
        })
    }

    /// Issue a one-time token for a human-tier record (a Decision) an agent wants accepted: the
    /// row holds the hash and the gated request strings; the prompt names the record (by its
    /// kind's display label) and the reason, never the token.
    fn issue_confirmation(
        &self,
        ctx: &WriteCtx,
        node: &Node,
        a: Acceptance,
    ) -> Result<AcceptResponse, ApiError> {
        let _ = self.store.confirmation_prune()?;
        let token = ConfirmationToken::generate()?;
        self.store.confirmation_create(NewConfirmation {
            token_hash: token.hash(self.signer.as_ref()),
            node_id: node.id.clone(),
            reason: a.reason.clone(),
            path_scope: a.path_scope.clone(),
            requested_by: SignerKind::Agent.as_str().to_owned(),
            host: a.host,
            session_id: a.session,
            ttl_s: TOKEN_TTL_S,
        })?;
        let label = ctx
            .governed
            .display(&node.kind)
            .map_or(node.kind.as_str(), |d| d.label.as_str());
        let title = title_of(self.project(node, false, None), label);
        let prompt = format!(
            "Accept the {label} {title} with reason \"{}\" for path_scope {}? If you agree, say so: the agent then calls accept again with the confirmation_token and confirmed_by_user: true. You can also run `kaban accept <id> --confirmation-token <token> --confirmed-by-user` yourself. The confirmation expires in {TOKEN_TTL_S} s.",
            a.reason,
            serde_json::to_string(&a.path_scope).unwrap_or_default()
        );
        Ok(AcceptResponse::ConfirmationRequired {
            prompt,
            confirmation_token: token,
            expires_in_s: TOKEN_TTL_S,
        })
    }

    /// Supersede `old` by `new` (core-spec F4): a `SUPERSEDES` edge whose store side effect
    /// moves the old record to `superseded` (archived) with its event. Over MCP this is the
    /// `link` tool with `rel: SUPERSEDES` (M3-24); the actor names the event's signer kind.
    ///
    /// # Errors
    /// As [`KabanApi::link`]: `NotFound`, `Rejected`, `Store(InvalidTransition)` (an old record
    /// that is still `proposed`, already superseded or invalidated).
    pub fn supersede(
        &self,
        new: &str,
        old: &str,
        caller: &Caller,
    ) -> Result<LinkResponse, ApiError> {
        self.link_as(
            LinkRequest {
                from: new.to_owned(),
                rel: "SUPERSEDES".into(),
                to: old.to_owned(),
                confidence: None,
                props: None,
            },
            caller.kind.clone(),
        )
    }
}
