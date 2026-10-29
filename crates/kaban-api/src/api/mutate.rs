//! `KabanApi::link`, `KabanApi::forget` and `KabanApi::deprecate`: the typed-edge, lifecycle
//! and claim-rank writes.

use super::{KabanApi, MAX_PROPS_BYTES, check_unit};
use crate::control::ControlPath;
use crate::error::ApiError;
use crate::types::{
    DeprecateRequest, DeprecateResponse, ForgetRequest, ForgetResponse, LinkRequest, LinkResponse,
    WriteReceipt,
};
use kaban_core::{Actor, AuditOp, ForgetMode, NodeId, Rank};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The fixed refusal of `deprecate` for an agent actor (EN-F12b: `deprecated` is set only by
/// a human).
pub const DEPRECATE_HUMAN_ONLY: &str =
    "deprecate is a human action: run `kaban explain <key> --deprecate <reason>`";

/// The fixed refusal of `resolve --winner` for an agent actor (engine-plan E2.5: the winner
/// is a human claim; EN-F12c).
pub const RESOLVE_WINNER_HUMAN_ONLY: &str =
    "resolve --winner is a human action: run `kaban resolve <id> --winner <value>`";

/// The fixed refusal of `unmerge` for an agent actor (engine-plan E2.10: a merge is reversed by
/// a human; EN-F51 (5)).
pub(crate) const UNMERGE_HUMAN_ONLY: &str =
    "unmerge is a human action: run `kaban unmerge <id> [--reason <text>]`";

/// The `rank_reason` `kaban unmerge` writes when the human gives none.
pub(crate) const UNMERGE_DEFAULT_REASON: &str = "unmerged";

/// What `kaban unmerge <id>` prints: the node, the canonical it was redirected to, the nodes
/// whose reads are restored and the identity claims now `deprecated`. The registry's
/// `unmerge` row takes its output schema from this type (E2.11b: the node ids and claim keys
/// are control, `reason` and `redactions[]` are content).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct UnmergeResponse {
    /// The node named.
    pub id: String,
    /// The canonical the node was redirected to, when it was the merged one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
    /// The merged nodes whose reads are restored.
    pub restored: Vec<String>,
    /// The identity claims now `deprecated` under the reason.
    pub deprecated: Vec<String>,
    /// The reason as stored (after the content gate).
    pub reason: String,
    /// Redactions the gate applied to `reason`.
    pub redactions: Vec<String>,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog` position.
    pub receipt: WriteReceipt,
}

/// What `kaban resolve <id> --winner <value>` prints: the node and the gated value as
/// written. The conflict group the claim joined is not echoed: its canonical key names the
/// subject by local row coordinates (`chunk_rid`, node and edge row ids), which SS-27 keeps
/// inside the database; `kaban review` and `explain` still list the open groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct ResolveWinnerResponse {
    /// The node whose candidate group was decided.
    pub id: String,
    /// The winning value as stored (after the content gate).
    pub value: String,
    /// Redactions the gate applied to `value`.
    pub redactions: Vec<String>,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog` position.
    pub receipt: WriteReceipt,
}

/// The fixed refusal of `resolve --symbol` for an agent actor (engine-plan E2.5, SO-6: the
/// re-target is a human claim).
pub const RESOLVE_SYMBOL_HUMAN_ONLY: &str =
    "resolve --symbol is a human action: run `kaban resolve <coderef-id> --symbol <key>`";

/// The fixed refusal of a `--symbol` key that is not a normalised global key (stage1-spec
/// §4.2: a versioned or `local` form; G1-F9 `symbol_invalid`): never the key.
pub const RESOLVE_SYMBOL_INVALID: &str = "symbol: symbol_invalid";

/// What `kaban resolve <coderef-id> --symbol <key>` prints: the node and the key as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct ResolveSymbolResponse {
    /// The `CodeRef` re-targeted.
    pub id: String,
    /// The key as stored (after the content gate; `format` of its parse).
    pub symbol: String,
    /// Redactions the gate applied to `symbol`.
    pub redactions: Vec<String>,
    /// The write's receipt (spec 05 SS-10): outcome, store identities and `ChangeLog` position.
    pub receipt: WriteReceipt,
}

// The `GOVERNS` source rule (core-spec §4; engine-spec EN-F4: `rels.GOVERNS.from` = the kinds
// with `governs_paths: true`) is the store's, in its one edge-write path, since E1.7a RE2-8.

impl KabanApi {
    /// Inspect, set or clear durable automatic-pruning protection. Actor is trusted
    /// adapter context, never a serialized request field.
    ///
    /// # Errors
    /// Policy rejection/redacted identity, malformed target, missing/foreign target,
    /// or a backend failure.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6: typed requests are taken by value
    pub fn pruning_protection(
        &self,
        p: crate::PruningProtectionRequest,
        actor: Actor,
    ) -> Result<crate::PruningProtectionResponse, ApiError> {
        use crate::{PruningOperation, PruningTargetKind};
        self.prepare_args(
            "pruning_protection",
            &serde_json::to_value(&p).map_err(|_| ApiError::Invalid("pruning_target".into()))?,
        )?;
        let mut ctx = self.write_ctx(None);
        ctx.actor = actor;
        let mut red = Vec::new();
        let tool = "pruning_protection";
        let mut control =
            |field: &'static str, value: &Option<String>| -> Result<Option<String>, ApiError> {
                value
                    .as_ref()
                    .map(|v| {
                        self.gate_control(
                            &mut ctx,
                            tool,
                            ControlPath::request(tool, field),
                            field,
                            v,
                            &mut red,
                        )
                    })
                    .transpose()
            };
        let id = control("id", &p.id)?;
        let key = control("key", &p.key)?;
        let src = control("src", &p.src)?;
        let dst = control("dst", &p.dst)?;
        let rel = self.gate_opt(&mut ctx, tool, "rel", p.rel.as_deref(), &mut red)?;
        if id != p.id || key != p.key || src != p.src || dst != p.dst || rel != p.rel {
            self.flush_audit(&mut ctx)?;
            return Err(ApiError::Invalid("pruning_identity_redacted".into()));
        }
        let target = match (p.target_kind, id, key, src, dst, rel) {
            (PruningTargetKind::Node, Some(id), None, None, None, None) => {
                kaban_core::PruningTarget::Node(Self::node_id(&id)?)
            }
            (PruningTargetKind::Claim, None, Some(key), None, None, None)
                if key.len() == 64
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
            {
                kaban_core::PruningTarget::Claim(kaban_core::AssertionKey(key))
            }
            (PruningTargetKind::Relationship, None, None, Some(src), Some(dst), Some(rel)) => {
                kaban_core::PruningTarget::Relationship {
                    src: Self::node_id(&src)?,
                    dst: Self::node_id(&dst)?,
                    rel,
                }
            }
            _ => return Err(ApiError::Invalid("pruning_target".into())),
        };
        if p.operation != PruningOperation::Get {
            ctx.audit.push(self.entry_for(
                &ctx,
                AuditOp::Accept,
                tool,
                "operation",
                match p.operation {
                    PruningOperation::Set => "set",
                    _ => "clear",
                },
                vec![],
            ));
        }
        let result = match p.operation {
            PruningOperation::Get => self.store.pruning_protection_get(&target, &ctx)?,
            PruningOperation::Set => self.store.pruning_protection_set(&target, &ctx)?,
            PruningOperation::Clear => self.store.pruning_protection_clear(&target, &ctx)?,
        };
        self.export_audit(&ctx.audit);
        let receipt = match p.operation {
            PruningOperation::Get => None,
            PruningOperation::Set | PruningOperation::Clear => {
                Some(self.await_searchable(self.committed_receipt(ctx.terminal_seq())))
            }
        };
        Ok(crate::PruningProtectionResponse {
            operation: p.operation,
            target_kind: p.target_kind,
            explicit: result.explicit,
            effective: result.effective,
            receipt,
        })
    }
    /// Retire a tracked obsolete artifact-only producer. The producer string is CONTENT;
    /// the loaded engine configuration supplies current-producer authority.
    ///
    /// # Errors
    /// Content rejection, unavailable layer mapping, or backend failure.
    pub fn retire_producer(
        &self,
        layer: kaban_core::Layer,
        old_producer: &str,
    ) -> Result<crate::LayerMaintenanceResponse, ApiError> {
        self.prepare_args(
            "retire_producer",
            &serde_json::json!({"layer":layer.as_str(),"producer":old_producer}),
        )?;
        let mut ctx = self.write_ctx(None);
        let producer = self.gate(
            &mut ctx,
            "retire_producer",
            "producer",
            old_producer,
            &mut Vec::new(),
        )?;
        if producer != old_producer {
            self.flush_audit(&mut ctx)?;
            return Err(ApiError::Invalid("maintenance_identity_redacted".into()));
        }
        let authority = kaban_core::CurrentProducerAuthority::from_config(self.engine(), layer)
            .ok_or_else(|| ApiError::Invalid("unavailable_producer".into()))?;
        ctx.audit.push(self.entry_for(
            &ctx,
            AuditOp::Forget,
            "retire_producer",
            "layer",
            layer.as_str(),
            vec![],
        ));
        let report = self.store.retire_producer(
            authority,
            &producer,
            self.engine().pipeline.retire_batch_rows,
            &ctx,
        )?;
        Ok(self.settled_layer_report(report, &ctx))
    }

    /// A layer maintenance report after the write barrier (SS-10, once per public call,
    /// after the store's transactions): the receipt holds at the call's terminal position.
    fn settled_layer_report(
        &self,
        report: kaban_core::RetireReport,
        ctx: &kaban_core::WriteCtx,
    ) -> crate::LayerMaintenanceResponse {
        crate::LayerMaintenanceResponse {
            report,
            receipt: self.await_searchable(self.committed_receipt(ctx.terminal_seq())),
        }
    }
    /// CLI-only DB layer replay, with no repository write or canonical-node mutation.
    ///
    /// # Errors
    /// Request gate, unavailable producer mapping, or store failure.
    pub fn reindex_layer(
        &self,
        layer: kaban_core::Layer,
    ) -> Result<crate::LayerMaintenanceResponse, ApiError> {
        if matches!(
            layer,
            kaban_core::Layer::Llm | kaban_core::Layer::Gliner | kaban_core::Layer::Script
        ) {
            self.prepare_args(
                "reindex_layer",
                &serde_json::json!({"layer":layer.as_str()}),
            )?;
            let mut ctx = self.write_ctx(None);
            ctx.audit.push(self.entry_for(
                &ctx,
                AuditOp::Forget,
                "reindex_layer",
                "layer",
                layer.as_str(),
                vec![],
            ));
            let report =
                self.store
                    .jobs_reindex(layer, self.engine().pipeline.retire_batch_rows, &ctx)?;
            return Ok(self.settled_layer_report(report, &ctx));
        }
        self.maintain_machine_layer(layer, kaban_core::LayerMaintenance::Reindex, None)
    }

    /// CLI-only layer retirement. A document path remains CONTENT and is scanned before
    /// storage can look up its identity; this does not change the `ForgetRequest` wire shape.
    ///
    /// # Errors
    /// Request gate, unavailable producer mapping, unknown document, or store failure.
    pub fn forget_layer(
        &self,
        layer: kaban_core::Layer,
        document: Option<&str>,
    ) -> Result<crate::LayerMaintenanceResponse, ApiError> {
        self.maintain_machine_layer(layer, kaban_core::LayerMaintenance::Forget, document)
    }

    fn maintain_machine_layer(
        &self,
        layer: kaban_core::Layer,
        operation: kaban_core::LayerMaintenance,
        document: Option<&str>,
    ) -> Result<crate::LayerMaintenanceResponse, ApiError> {
        let tool = if operation == kaban_core::LayerMaintenance::Forget {
            "forget_layer"
        } else {
            "reindex_layer"
        };
        self.prepare_args(
            tool,
            &serde_json::json!({"layer":layer.as_str(),"document":document}),
        )?;
        let mut ctx = self.write_ctx(None);
        let gated_document =
            self.gate_opt(&mut ctx, tool, "document", document, &mut Vec::new())?;
        if gated_document.as_deref() != document {
            self.flush_audit(&mut ctx)?;
            return Err(ApiError::Invalid("maintenance_identity_redacted".into()));
        }
        let authority = kaban_core::CurrentProducerAuthority::from_config(self.engine(), layer)
            .ok_or_else(|| ApiError::Invalid("unavailable_producer".into()))?;
        ctx.audit.push(self.entry_for(
            &ctx,
            AuditOp::Forget,
            tool,
            "layer",
            layer.as_str(),
            vec![],
        ));
        let report = self.store.maintain_layer(
            authority,
            operation,
            gated_document.as_deref(),
            self.engine().pipeline.retire_batch_rows,
            &ctx,
        )?;
        if report.refused.is_some() {
            return Err(ApiError::Invalid(format!(
                "untracked_producer: {}",
                report.untracked
            )));
        }
        Ok(self.settled_layer_report(report, &ctx))
    }
    /// Create or update a typed relationship between two existing nodes; a `GOVERNS` edge
    /// starts at a governing kind (`governs_paths: true` in the loaded ontology).
    ///
    /// # Errors
    /// `Invalid`, `Rejected`, `NotFound`, `Store(EndpointNotAllowed)` (a `GOVERNS` source that
    /// does not govern paths) or store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn link(&self, p: LinkRequest) -> Result<LinkResponse, ApiError> {
        self.link_as(p, Actor::Agent)
    }

    /// [`Self::link`] on behalf of `actor` (the signer kind of a `SUPERSEDES`/`INVALIDATES`
    /// event on the target).
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub(crate) fn link_as(&self, p: LinkRequest, actor: Actor) -> Result<LinkResponse, ApiError> {
        check_unit("confidence", p.confidence)?;
        let mut red = Vec::new();
        let t = "link";
        let mut ctx = self.write_ctx(None);
        ctx.actor = actor;
        // the endpoint ids are control (validated by shape, scanned only when malformed)
        let from = self.gate_control(
            &mut ctx,
            t,
            ControlPath::request(t, "from"),
            "from",
            &p.from,
            &mut red,
        )?;
        let to = self.gate_control(
            &mut ctx,
            t,
            ControlPath::request(t, "to"),
            "to",
            &p.to,
            &mut red,
        )?;
        let rel = self.gate(&mut ctx, t, "rel", &p.rel, &mut red)?;
        let props = match &p.props {
            Some(v) => {
                if v.to_string().len() > MAX_PROPS_BYTES {
                    return Err(ApiError::Invalid("props too large".into()));
                }
                Some(self.gate_json(&mut ctx, t, "props", v, &mut red)?)
            }
            None => None,
        };
        let (from, to) = (Self::node_id(&from)?, Self::node_id(&to)?);
        // the `GOVERNS` source rule is the store's, in the one edge-write path shared with the
        // inline links of `remember` (E1.7a RE2-8); nothing is pre-judged here
        let e = self
            .store
            .link(&from, &to, &rel, p.confidence, props, &ctx)?;
        self.export_audit(&ctx.audit);
        let receipt = self.await_searchable(self.committed_receipt(ctx.terminal_seq()));
        Ok(LinkResponse {
            id: e.id,
            from: e.src,
            rel: e.rel,
            to: e.dst,
            confidence: e.confidence,
            redactions: red,
            receipt,
        })
    }

    /// A human deprecates one claim (engine-spec EN-F12b; `kaban explain <key> --deprecate`):
    /// the rank becomes `deprecated` with the gated `reason` and the claim's group is
    /// re-decided without its vote in the same store transaction.
    ///
    /// # Errors
    /// `Invalid` with [`DEPRECATE_HUMAN_ONLY`] for an agent actor or a key that is not 64 hex
    /// digits, `Rejected` when the reason hits a reject rule (nothing written), `NotFound` for
    /// an unknown key, or store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn deprecate(
        &self,
        p: DeprecateRequest,
        actor: Actor,
    ) -> Result<DeprecateResponse, ApiError> {
        if actor != Actor::HumanCli {
            return Err(ApiError::Invalid(DEPRECATE_HUMAN_ONLY.into()));
        }
        // the key is control: a claim key is 64 lowercase hex digits (EN-F12a) and names no
        // content; anything else is refused before the reason is judged
        if p.key.len() != 64 || !p.key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiError::Invalid("key must be 64 hex digits".into()));
        }
        let mut red = Vec::new();
        let mut ctx = self.write_ctx(None);
        ctx.actor = actor;
        // `rank_reason` is content (amendments.md: gated like every stored string)
        let reason = self.gate(&mut ctx, "explain", "reason", &p.reason, &mut red)?;
        // the store binds the key to `ctx.workspace_id` (S6) and commits `ctx.audit` with
        // the rank write (RE4-4): no separate audit flush
        let disposition = self.store.deprecate_assertion(&p.key, &reason, &ctx)?;
        self.export_audit(&ctx.audit);
        let receipt = self.await_searchable(self.committed_receipt(ctx.terminal_seq()));
        Ok(DeprecateResponse {
            key: p.key,
            rank: Rank::Deprecated.as_str().to_owned(),
            disposition: disposition.as_str().to_owned(),
            redactions: red,
            receipt,
        })
    }

    /// A human names the winner of one of a node's outstanding candidate groups (engine-plan
    /// E2.5 `kaban resolve <id> --winner <value> [--group <key>]`; EN-F5a, EN-F12c; wave-3
    /// review RW3-3): one human `preferred` claim carrying the gated `value` joins the group
    /// named by `group` (the key `kaban review` and `explain` render on the item), or the
    /// node's only candidate group when none is named; the group is re-decided and projected,
    /// and the node's candidate row is released in the same store transaction.
    ///
    /// # Errors
    /// `Invalid` with [`RESOLVE_WINNER_HUMAN_ONLY`] for an agent actor, `Rejected` when the
    /// value or the group hits a reject rule (nothing written), `NotFound` for an unknown node,
    /// one with no candidate group or a group that is not the node's, `Store(InvalidName)`
    /// with `ambiguous_group` when the node has several candidate groups and none is named,
    /// or store errors.
    pub fn resolve_winner(
        &self,
        id: &NodeId,
        value: &str,
        group: Option<&str>,
        actor: Actor,
    ) -> Result<ResolveWinnerResponse, ApiError> {
        if actor != Actor::HumanCli {
            return Err(ApiError::Invalid(RESOLVE_WINNER_HUMAN_ONLY.into()));
        }
        let mut red = Vec::new();
        let mut ctx = self.write_ctx(None);
        ctx.actor = actor;
        // the value is content (amendments.md: gated like every stored string); the group key
        // travels under the same scope (a masked key names no group: `NotFound`)
        let value = self.gate(&mut ctx, "resolve", "winner", value, &mut red)?;
        let group = group
            .map(|g| self.gate(&mut ctx, "resolve", "group", g, &mut red))
            .transpose()?;
        let ws = self.config.workspace.as_str().to_owned();
        // the decided group's key stays inside the database (SS-27, see the response type)
        let _group_key: String =
            self.store
                .resolve_winner(&ws, id, &value, group.as_deref(), &ctx)?;
        self.export_audit(&ctx.audit);
        let receipt = self.await_searchable(self.committed_receipt(ctx.terminal_seq()));
        Ok(ResolveWinnerResponse {
            id: id.to_string(),
            value,
            redactions: red,
            receipt,
        })
    }

    /// A human re-targets a stale `CodeRef` to the key its symbol has now (engine-plan E2.5
    /// `kaban resolve <coderef-id> --symbol <key>`; SO-6): the key is content under the scope
    /// `resolve` (gated first, G1-S1), then checked as a normalised global key (stage1-spec
    /// §4.2: a versioned or `local` form is [`RESOLVE_SYMBOL_INVALID`], nothing written), and
    /// the store records the human `prop_value` claim, rewrites `props.symbol` and the `symbol`
    /// anchor row in one transaction, so the item leaves `kaban review`.
    ///
    /// # Errors
    /// `Invalid` with [`RESOLVE_SYMBOL_HUMAN_ONLY`] for an agent actor or
    /// [`RESOLVE_SYMBOL_INVALID`] for a key that is not a normalised global key, `Rejected`
    /// when the key hits a reject rule (nothing written), `NotFound` for an unknown node or
    /// one that is not an active `CodeRef`, or store errors.
    pub fn resolve_symbol(
        &self,
        id: &NodeId,
        key: &str,
        actor: Actor,
    ) -> Result<ResolveSymbolResponse, ApiError> {
        if actor != Actor::HumanCli {
            return Err(ApiError::Invalid(RESOLVE_SYMBOL_HUMAN_ONLY.into()));
        }
        let mut red = Vec::new();
        let mut ctx = self.write_ctx(None);
        ctx.actor = actor;
        // content before grammar (G1-S1, SO-13): the parser only ever sees a gated string,
        // and a key the gate masked fails the key-form rule like any other malformed input
        let key = self.gate(&mut ctx, "resolve", "symbol", key, &mut red)?;
        let key = crate::coderef::check_symbol(&key)
            .map_err(|_| ApiError::Invalid(RESOLVE_SYMBOL_INVALID.into()))?;
        let ws = self.config.workspace.as_str().to_owned();
        self.store.resolve_symbol(&ws, id, &key, &ctx)?;
        self.export_audit(&ctx.audit);
        let receipt = self.await_searchable(self.committed_receipt(ctx.terminal_seq()));
        Ok(ResolveSymbolResponse {
            id: id.to_string(),
            symbol: key,
            redactions: red,
            receipt,
        })
    }

    /// A human reverses a merge (engine-plan E2.10 `kaban unmerge <id>`; EN-F51 (5)): the
    /// `node_identity` rows naming the node go, the identity claim behind each becomes
    /// `deprecated` under the gated `reason` and its group is re-decided in one store
    /// transaction; both nodes stay.
    ///
    /// # Errors
    /// `Invalid` with [`UNMERGE_HUMAN_ONLY`] for an agent actor, `Rejected` when the reason hits
    /// a reject rule (nothing written), `NotFound` for an unknown node or one no identity row
    /// names, or store errors.
    pub fn unmerge(
        &self,
        id: &str,
        reason: Option<&str>,
        actor: Actor,
    ) -> Result<UnmergeResponse, ApiError> {
        if actor != Actor::HumanCli {
            return Err(ApiError::Invalid(UNMERGE_HUMAN_ONLY.into()));
        }
        let mut red = Vec::new();
        let mut ctx = self.write_ctx(None);
        ctx.actor = actor;
        // the id is control (validated by shape, scanned only when malformed, review R1-3)
        let id = self.gate_control(
            &mut ctx,
            "unmerge",
            ControlPath::request("unmerge", "id"),
            "id",
            id,
            &mut red,
        )?;
        // `rank_reason` is content (amendments.md: gated like every stored string)
        let reason = self.gate(
            &mut ctx,
            "unmerge",
            "reason",
            reason.unwrap_or(UNMERGE_DEFAULT_REASON),
            &mut red,
        )?;
        let id = Self::node_id(&id)?;
        let ws = self.config.workspace.as_str().to_owned();
        let outcome = self.store.unmerge(&ws, &id, &reason, &ctx)?;
        self.export_audit(&ctx.audit);
        let receipt = self.await_searchable(self.committed_receipt(ctx.terminal_seq()));
        Ok(UnmergeResponse {
            id: outcome.id,
            canonical_id: outcome.canonical_id,
            restored: outcome.restored,
            deprecated: outcome.deprecated,
            reason,
            redactions: red,
            receipt,
        })
    }

    /// Tombstone (default), invalidate, archive or purge a node (core-spec F5).
    ///
    /// # Errors
    /// `NotFound` (unknown, purged or not a node id), an `InvalidTransition` store error, or
    /// other store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn forget(&self, p: ForgetRequest) -> Result<ForgetResponse, ApiError> {
        let mode = p.mode.unwrap_or_default();
        let mut ctx = self.write_ctx(None);
        // the id is control: a node id is validated, anything else is gated before the lookup
        // (review R1-3), so a redact-class value is looked up (and reported) masked, never raw
        let id = self.gate_control(
            &mut ctx,
            "forget",
            ControlPath::request("forget", "id"),
            "id",
            &p.id,
            &mut Vec::new(),
        )?;
        self.flush_audit(&mut ctx)?; // a redaction verdict is recorded even if the lookup fails
        let op = match mode {
            ForgetMode::Purge => AuditOp::Purge,
            ForgetMode::Tombstone | ForgetMode::Invalidate | ForgetMode::Archive => AuditOp::Forget,
        };
        // a string that is not a node id names no node (D2); the gated (masked) form is echoed
        let node_id = Self::node_id(&id)?;
        let mut entry = self.entry_for(&ctx, op, "forget", "node", &id, vec![]);
        entry.node_id = Some(id.clone());
        ctx.audit.push(entry);
        // EN-F16: a retirement is evidence against the node's reconciled winner and candidate
        // claims, read before the store rejects their statuses (an archive keeps them and
        // counts nothing); an unknown id is the store's `NotFound` below, not a count
        if mode != ForgetMode::Archive {
            match self
                .store
                .priors_obs_bump(&node_id, kaban_core::PriorsOutcome::Rejected)
            {
                Ok(counted) => tracing::debug!(counted, "priors_obs: rejected"),
                Err(kaban_core::StoreError::NotFound(_)) => {}
                Err(e) => tracing::warn!(error = %e, "priors_obs: the rejected count failed"),
            }
        }
        let outcome = self.store.forget(&node_id, mode, &ctx)?;
        self.export_audit(&ctx.audit);
        // the store read the terminal `seq` inside the transaction (SS-10)
        let receipt = self.await_searchable(self.committed_receipt(Some(outcome.outbox_seq)));
        Ok(ForgetResponse {
            id,
            mode,
            lifecycle: outcome.lifecycle,
            governance: outcome.governance,
            outbox_seq: outcome.outbox_seq,
            revoked: outcome.revoked,
            physically_cleaned: outcome.physically_cleaned,
            cleanup: outcome.cleanup,
            receipt,
        })
    }
}
