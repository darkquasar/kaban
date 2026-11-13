//! `KabanApi::link` and `KabanApi::forget`: the typed-edge and lifecycle writes, plus the
//! producer-layer maintenance writes.

use super::{KabanApi, MAX_PROPS_BYTES, check_unit};
use crate::control::ControlPath;
use crate::error::ApiError;
use crate::types::{ForgetRequest, ForgetResponse, LinkRequest, LinkResponse};
use kaban_core::{Actor, AuditOp, ForgetMode};

// The `GOVERNS` source rule (core-spec §4; engine-spec EN-F4: `rels.GOVERNS.from` = the kinds
// with `governs_paths: true`) is the store's, in its one edge-write path, since E1.7a RE2-8.

impl KabanApi {
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
