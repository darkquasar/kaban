//! `KabanApi::link` and `KabanApi::forget`: the typed-edge and lifecycle writes.

use super::{KabanApi, MAX_PROPS_BYTES, check_unit};
use crate::control::ControlPath;
use crate::error::ApiError;
use crate::types::{ForgetRequest, ForgetResponse, LinkRequest, LinkResponse};
use kaban_core::Actor;

// The declared endpoint rule is the store's, in its one edge-write path, since E1.7a RE2-8.

impl KabanApi {
    /// Create or update a typed relationship between two existing nodes, within the rel's
    /// declared endpoints.
    ///
    /// # Errors
    /// `Invalid`, `Rejected`, `NotFound`, `Store(EndpointNotAllowed)` (an endpoint outside the
    /// rel's declared kinds), `Store(UnknownRel)` or store errors.
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
            t,
            ControlPath::request(t, "from"),
            "from",
            &p.from,
            &mut red,
        )?;
        let to = self.gate_control(t, ControlPath::request(t, "to"), "to", &p.to, &mut red)?;
        let rel = self.gate(t, "rel", &p.rel, &mut red)?;
        let props = match &p.props {
            Some(v) => {
                if v.to_string().len() > MAX_PROPS_BYTES {
                    return Err(ApiError::Invalid("props too large".into()));
                }
                Some(self.gate_json(t, "props", v, &mut red)?)
            }
            None => None,
        };
        let (from, to) = (Self::node_id(&from)?, Self::node_id(&to)?);
        // the endpoint rule is the store's, in the one edge-write path shared with the inline
        // links of `remember` (E1.7a RE2-8); nothing is pre-judged here
        let e = self
            .store
            .link(&from, &to, &rel, p.confidence, props, &ctx)?;
        let receipt = self.committed_receipt(ctx.terminal_seq());
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
        let ctx = self.write_ctx(None);
        // the id is control: a node id is validated, anything else is gated before the lookup
        // (review R1-3), so a redact-class value is looked up (and reported) masked, never raw
        let id = self.gate_control(
            "forget",
            ControlPath::request("forget", "id"),
            "id",
            &p.id,
            &mut Vec::new(),
        )?;
        // a string that is not a node id names no node (D2); the gated (masked) form is echoed
        let node_id = Self::node_id(&id)?;
        let outcome = self.store.forget(&node_id, mode, &ctx)?;
        // the store read the terminal `seq` inside the transaction (SS-10)
        let receipt = self.committed_receipt(Some(outcome.outbox_seq));
        Ok(ForgetResponse {
            id,
            mode,
            lifecycle: outcome.lifecycle,
            revoked: outcome.revoked,
            physically_cleaned: outcome.physically_cleaned,
            cleanup: outcome.cleanup,
            receipt,
        })
    }
}
