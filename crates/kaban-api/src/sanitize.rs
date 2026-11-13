//! The denylist gate for strings and JSON values (`kaban-policy`): a redact-rule hit masks the
//! string and is reported as `field:rule` in the caller's redaction list; a reject-rule hit
//! refuses the call with the rule ids. Verdicts are traced by tool, field and rule id only.

use crate::api::KabanApi;
use crate::control::{ControlPath, Direction, Walk};
use crate::error::ApiError;
use kaban_core::WriteCtx;
use kaban_policy::{Verdict, is_credential_key};
use serde_json::Value;

/// What replaces a secret control value at a response entry that is not the issuing one.
const SECRET_MASK: &str = "[REDACTED:secret]";

impl KabanApi {
    /// Finalize under API-derived authority, gating the report inside the graph transaction.
    /// The builder cannot acquire the store or await. A retained report is never remasked:
    /// current-policy changes that would alter it refuse after rolling back.
    /// # Errors
    /// Policy/authority refusal, revoked/stale/expired preparation or backend failure.
    pub fn publish_ingest_finish(
        &self,
        request: kaban_core::JobFinishRequest,
        build: &mut dyn FnMut(
            &kaban_core::IngestJob,
            &kaban_core::JobFinishOutcome,
        ) -> kaban_core::Result<String>,
    ) -> Result<String, ApiError> {
        let transaction_ctx = self.write_ctx(None);
        let mut refusal = None;
        let result = self
            .store
            .job_finish(request, &transaction_ctx, &mut |job, outcome| {
                let bytes = build(job, outcome)?;
                let value: Value = serde_json::from_str(&bytes)?;
                let mut walk = Walk::scoped("ingest_finish", Direction::Response);
                let gated = match self.scan_strings(
                    "mcp",
                    "response",
                    &value,
                    &mut walk,
                    &mut Vec::new(),
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        refusal = Some(error);
                        return Err(kaban_core::StoreError::InvalidName(
                            "job".into(),
                            "response_withheld",
                        ));
                    }
                };
                let report_json = if job.status == kaban_core::JobStatus::Finished {
                    if gated != value {
                        refusal = Some(ApiError::Invalid("stored_report_policy_changed".into()));
                        return Err(kaban_core::StoreError::InvalidName(
                            "job".into(),
                            "response_withheld",
                        ));
                    }
                    bytes
                } else {
                    if serde_json::from_value::<crate::ingest_jobs::IngestFinishReport>(
                        gated.clone(),
                    )
                    .is_err()
                    {
                        refusal = Some(ApiError::Invalid("invalid_report".into()));
                        return Err(kaban_core::StoreError::InvalidName(
                            "job".into(),
                            "response_withheld",
                        ));
                    }
                    serde_json::to_string(&gated)?
                };
                Ok(kaban_core::JobFinishStateUpdate { report_json })
            });
        if let Some(error) = refusal {
            return Err(error);
        }
        result.map_err(ApiError::from)
    }
    /// Begin an opaque gate session for structured publication. The service
    /// derives workspace, actor, policy and ontology; callers cannot replace its context.
    #[must_use]
    pub fn begin_ingest_submit(&self) -> IngestSubmitGate<'_> {
        IngestSubmitGate {
            api: self,
            ctx: self.write_ctx(None),
            policy_version: self.policy_version(),
            governed: self.governed(),
            redactions: Vec::new(),
            nodes: std::collections::BTreeSet::new(),
            edges: std::collections::BTreeSet::new(),
        }
    }
    /// The only way a string gets in or out: returns the (possibly redacted) string or a
    /// rejection.
    ///
    /// # Errors
    /// `ApiError::Rejected` with the field and the reject-rule ids.
    pub fn sanitize(
        &self,
        tool: &str,
        field: &str,
        s: &str,
        redactions: &mut Vec<String>,
    ) -> Result<String, ApiError> {
        self.gate(tool, field, s, redactions)
    }

    /// The request side of the field-class seam (core-spec S1, OWN-10): the pre-parse scan of
    /// `tool`'s raw arguments. Every content string leaf and `key=value` view is checked before
    /// any typed deserialization, so a serde error can never echo a secret; a control leaf
    /// ([`crate::control::REGISTRY`]) whose value passes its shape is validated instead of
    /// scanned, one that fails is scanned as content. The typed handlers still gate every content
    /// field they use, so redactions are reported with the write they belong to
    /// (review R1-3, option A). Rejections refuse here; redactions are not
    /// recorded here. This is the only pre-parse entry an adapter calls. The walk judges the
    /// borrowed tree and materializes nothing, so a secret control leaf (the confirmation
    /// token) is never copied into an ordinary string (review R3-2).
    ///
    /// # Errors
    /// `ApiError::Rejected` with the field `args` and the reject-rule ids.
    pub fn prepare_args(&self, tool: &str, value: &Value) -> Result<(), ApiError> {
        let mut walk = Walk::scoped(tool, Direction::Request);
        self.check_json_under(tool, "args", value, None, &mut walk, &mut Vec::new())
    }

    /// The response side of the field-class seam (core-spec S1 "before returning", S4; OWN-10):
    /// the last gate before `tool`'s serialized result reaches the wire. Every content string
    /// (object keys and string leaves, as plain text: response trees are not property bags, so
    /// a masked key such as `[REDACTED:old-password]` must not read as a credential assignment
    /// of its count) is masked or, on a reject hit, the whole result is refused; a control leaf
    /// whose value passes its shape is validated instead of scanned. Rejections refuse
    /// the result; redactions are not listed here (the read projection already masks them
    /// per item). This is the only response gate an adapter calls.
    ///
    /// # Errors
    /// `ApiError::Rejected` with the field `response` and the reject-rule ids.
    pub fn gate_response(&self, tool: &str, value: &Value) -> Result<Value, ApiError> {
        let mut walk = Walk::scoped(tool, Direction::Response);
        self.scan_strings("mcp", "response", value, &mut walk, &mut Vec::new())
    }

    /// The plain-string form of [`Self::gate_response`]: the last gate before free text that is
    /// not a tool result (an error message) reaches the wire. There are no control entries in
    /// free text, so every character is content. Rejections refuse.
    ///
    /// # Errors
    /// `ApiError::Rejected` with the field `error` and the reject-rule ids; the adapter replaces
    /// the text with a fixed message.
    pub fn gate_response_text(&self, text: &str) -> Result<String, ApiError> {
        self.scan_str("mcp", "error", text, &mut Vec::new())
    }

    /// Scan every content string of a JSON tree as plain text (see [`Self::gate_response`]);
    /// control leaves that pass their shape are kept as they are.
    fn scan_strings(
        &self,
        tool: &str,
        field: &str,
        v: &Value,
        walk: &mut Walk,
        redactions: &mut Vec<String>,
    ) -> Result<Value, ApiError> {
        Ok(match v {
            // a secret at a non-issuing response entry never leaves (review RC-5)
            Value::String(_) if walk.withholds_secret() => Value::String(SECRET_MASK.into()),
            Value::String(s) if walk.passes_control(s) => Value::String(s.clone()),
            Value::String(s) if walk.is_ingest_stream() => {
                let verdict = {
                    let policy = self.pol();
                    policy.scan_windows(s, policy.file.defaults.max_field_chars)
                };
                Value::String(if verdict.hits.is_empty() && !verdict.rejected {
                    s.clone()
                } else {
                    self.apply_verdict(tool, field, verdict, redactions)?
                })
            }
            Value::String(s) => Value::String(self.scan_str(tool, field, s, redactions)?),
            Value::Array(a) => {
                let mark = walk.push_index();
                let items = a
                    .iter()
                    .map(|x| self.scan_strings(tool, field, x, walk, redactions))
                    .collect::<Result<_, _>>();
                walk.pop(mark);
                Value::Array(items?)
            }
            Value::Object(o) => {
                let mut m = serde_json::Map::new();
                for (k, x) in o {
                    let k2 = self.scan_str(tool, field, k, redactions)?;
                    let mark = walk.push_key(k);
                    let leaf = self.scan_strings(tool, field, x, walk, redactions);
                    walk.pop(mark);
                    if m.insert(k2, leaf?).is_some() {
                        return Err(ApiError::Rejected {
                            field: field.into(),
                            rules: vec!["props-key-collision".into()],
                        });
                    }
                }
                Value::Object(m)
            }
            other => other.clone(),
        })
    }

    /// The per-field gate with redaction, public for producers outside the crate (engine-spec
    /// EN-E5; engine-plan E2.11): `s` as `field` of `tool`, masked where a redact rule hits,
    /// with the ids of the rules that masked it. Like [`Self::prepare_args`], a rejection is
    /// reported; redactions are the caller's to report with the write they belong to.
    ///
    /// # Errors
    /// `ApiError::Rejected` with `field` and the reject-rule ids.
    pub fn gate_field(
        &self,
        tool: &str,
        field: &str,
        s: &str,
    ) -> Result<(String, Vec<String>), ApiError> {
        let mut redactions = Vec::new();
        let masked = self.gate(tool, field, s, &mut redactions)?;
        Ok((masked, redactions))
    }

    /// The JSON props gate, public (EN-E5): every string of `v` (keys and leaves) as `field`
    /// of `tool`, masked where a redact rule hits, with the rule ids; the size and key rules of
    /// [`Self::scan_json`] apply. Audited like [`Self::gate_field`].
    ///
    /// # Errors
    /// `ApiError::Rejected` with `field` and the reject-rule ids (`props-too-large`,
    /// `props-key-collision` included).
    pub fn gate_props_json(
        &self,
        tool: &str,
        field: &str,
        v: &Value,
    ) -> Result<(Value, Vec<String>), ApiError> {
        let mut redactions = Vec::new();
        let masked = self.gate_json(tool, field, v, &mut redactions)?;
        Ok((masked, redactions))
    }

    /// The joined-view gate, public (EN-E5): `s` is a view joined from several fields
    /// (so a phrase split across them is caught), scanned in overlapping windows; only reject
    /// hits count, the fields record their own redactions. Audited like [`Self::gate_field`].
    ///
    /// # Errors
    /// `ApiError::Rejected` with `field` and the reject-rule ids.
    pub fn gate_joined_view(&self, tool: &str, field: &str, s: &str) -> Result<(), ApiError> {
        self.gate_joined(tool, field, s)
    }

    /// Gate one string: the masked string, or the rejection.
    pub(crate) fn gate(
        &self,
        tool: &str,
        field: &str,
        s: &str,
        redactions: &mut Vec<String>,
    ) -> Result<String, ApiError> {
        self.scan_str(tool, field, s, redactions)
    }

    /// Gate one string a typed handler takes from a declared control path (review RC-1): a
    /// value the registered shape accepts is validated instead of scanned (the pre-parse scan
    /// already made the same decision); any other value is content and goes through
    /// [`Self::gate`] under `field`.
    pub(crate) fn gate_control(
        &self,
        tool: &str,
        at: ControlPath,
        field: &str,
        s: &str,
        redactions: &mut Vec<String>,
    ) -> Result<String, ApiError> {
        if at.accepts(s) {
            return Ok(s.to_owned());
        }
        self.gate(tool, field, s, redactions)
    }

    /// [`Self::gate_control`] for read projections (their verdicts are
    /// recorded per item by the caller).
    pub(crate) fn scan_control(
        &self,
        tool: &str,
        at: ControlPath,
        field: &str,
        s: &str,
        redactions: &mut Vec<String>,
    ) -> Result<String, ApiError> {
        if at.accepts(s) {
            return Ok(s.to_owned());
        }
        self.scan_str(tool, field, s, redactions)
    }

    /// Gate every string of a list (same field name for all).
    pub(crate) fn gate_all(
        &self,
        tool: &str,
        field: &str,
        values: &[String],
        redactions: &mut Vec<String>,
    ) -> Result<Vec<String>, ApiError> {
        values
            .iter()
            .map(|s| self.gate(tool, field, s, redactions))
            .collect()
    }

    /// Gate an optional string.
    pub(crate) fn gate_opt(
        &self,
        tool: &str,
        field: &str,
        value: Option<&str>,
        redactions: &mut Vec<String>,
    ) -> Result<Option<String>, ApiError> {
        value
            .map(|s| self.gate(tool, field, s, redactions))
            .transpose()
    }

    /// Gate a view joined from several fields (so a phrase split across fields is caught):
    /// scanned in overlapping windows of `max_field_chars`, so the per-field cap is not applied
    /// to the joined length (core-plan C1.2; m2 N7). Only rejections are recorded here; the
    /// individual fields record their own redactions.
    pub(crate) fn gate_joined(&self, tool: &str, field: &str, s: &str) -> Result<(), ApiError> {
        let v = {
            let pol = self.pol();
            pol.scan_windows(s, pol.file.defaults.max_field_chars)
        };
        if !v.rejected {
            return Ok(());
        }
        self.apply_verdict(tool, field, v, &mut Vec::new())
            .map(|_| ())
    }

    /// Gate a JSON value (see [`KabanApi::scan_json`]).
    pub(crate) fn gate_json(
        &self,
        tool: &str,
        field: &str,
        v: &Value,
        redactions: &mut Vec<String>,
    ) -> Result<Value, ApiError> {
        self.scan_json(tool, field, v, redactions)
    }

    /// Scan one string.
    pub(crate) fn scan_str(
        &self,
        tool: &str,
        field: &str,
        s: &str,
        redactions: &mut Vec<String>,
    ) -> Result<String, ApiError> {
        let v = self.pol().scan(s);
        if v.hits.is_empty() && !v.rejected {
            return Ok(s.to_string());
        }
        self.apply_verdict(tool, field, v, redactions)
    }

    /// Turn a verdict into the redacted string or a rejection; a redaction is listed in
    /// `redactions` as `field:rule` per hit.
    fn apply_verdict(
        &self,
        tool: &str,
        field: &str,
        v: Verdict,
        redactions: &mut Vec<String>,
    ) -> Result<String, ApiError> {
        if v.rejected {
            tracing::debug!(tool, field, rules = ?v.reject_rules, "denylist rejection");
            return Err(ApiError::Rejected {
                field: field.into(),
                rules: v.reject_rules,
            });
        }
        tracing::debug!(tool, field, hits = v.hits.len(), "denylist redaction");
        for h in &v.hits {
            redactions.push(format!("{field}:{}", h.rule));
        }
        Ok(v.redacted)
    }

    /// Scan a `key=value` view of one JSON leaf (scalar or array element) so assignment-shaped
    /// rules see structured credentials. Returns the leaf to store: the original when clean, the
    /// redacted value (as a string) when a redact rule hit.
    fn scan_leaf(
        &self,
        tool: &str,
        field: &str,
        key: &str,
        leaf: &Value,
        redactions: &mut Vec<String>,
    ) -> Result<Value, ApiError> {
        let value_text = match leaf {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let joined = format!("{key}={value_text}");
        let v = if leaf.is_string() || leaf.is_number() {
            self.pol().scan_kv(key, &value_text)
        } else {
            self.pol().scan(&joined)
        };
        let prefix = format!("{key}=");
        // a redact hit confined to the key side of the view belongs to the key, which is scanned
        // (and masked) on its own: the value is untouched
        if !v.rejected && v.hits.iter().all(|h| h.end <= prefix.len()) {
            return Ok(leaf.clone());
        }
        let redacted = self.apply_verdict(tool, field, v, redactions)?;
        Ok(match redacted.strip_prefix(&prefix) {
            Some(value) => Value::String(value.to_string()),
            // the key side was masked too, so the value cannot be cut out of the joined view;
            // string values are still scanned alone by `scan_scalar`
            None => leaf.clone(),
        })
    }

    /// A scalar under `key`: the `key=value` view first, then (for strings) the value alone,
    /// since the two views have different word boundaries.
    fn scan_scalar(
        &self,
        tool: &str,
        field: &str,
        key: &str,
        scalar: &Value,
        redactions: &mut Vec<String>,
    ) -> Result<Value, ApiError> {
        let kv = self.scan_leaf(tool, field, key, scalar, redactions)?;
        match kv {
            Value::String(s) if scalar.is_string() => {
                Ok(Value::String(self.scan_str(tool, field, &s, redactions)?))
            }
            other => Ok(other),
        }
    }

    /// Sanitize every string leaf of a JSON value (keys are scanned too, as `key` only) without
    /// flushing. Scalars under a key, at any container depth, are additionally scanned as
    /// `key=value` (core-spec S1); the redacted value replaces the leaf; two keys that redact to
    /// the same name are a rejection (`props-key-collision`). Every string of a props tree is
    /// content: there are no control paths inside `props`.
    pub(crate) fn scan_json(
        &self,
        tool: &str,
        field: &str,
        v: &Value,
        redactions: &mut Vec<String>,
    ) -> Result<Value, ApiError> {
        self.scan_json_under(tool, field, v, None, redactions)
    }

    /// [`Self::scan_json`] with the key context of the enclosing object (see [`inner_key`]).
    fn scan_json_under(
        &self,
        tool: &str,
        field: &str,
        v: &Value,
        key: Option<&str>,
        redactions: &mut Vec<String>,
    ) -> Result<Value, ApiError> {
        Ok(match v {
            Value::Array(a) => Value::Array(
                a.iter()
                    .map(|x| self.scan_json_under(tool, field, x, key, redactions))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(o) => {
                let mut m = serde_json::Map::new();
                for (k, x) in o {
                    let k2 = self.scan_str(tool, field, k, redactions)?;
                    let inner = inner_key(key, k);
                    let leaf = self.scan_json_under(tool, field, x, Some(inner), redactions)?;
                    if m.insert(k2, leaf).is_some() {
                        return Err(key_collision(field));
                    }
                }
                Value::Object(m)
            }
            scalar => match key {
                Some(k) => self.scan_scalar(tool, field, k, scalar, redactions)?,
                None => match scalar {
                    Value::String(s) => Value::String(self.scan_str(tool, field, s, redactions)?),
                    other => other.clone(),
                },
            },
        })
    }

    /// The pre-parse judgement of a raw request tree (see [`Self::prepare_args`]): the same
    /// views as [`Self::scan_json_under`] (keys, `key=value` and plain leaves, the key context
    /// of [`inner_key`], `props-key-collision`), but nothing is built or returned. A string
    /// leaf at a control path of `walk` that passes its shape is validated and not touched
    /// (no `key=value` view either, and no copy: review R3-2).
    #[allow(clippy::too_many_arguments)] // one recursive walker; the walk state is a single cursor
    fn check_json_under(
        &self,
        tool: &str,
        field: &str,
        v: &Value,
        key: Option<&str>,
        walk: &mut Walk,
        redactions: &mut Vec<String>,
    ) -> Result<(), ApiError> {
        match v {
            Value::Array(a) => {
                let mark = walk.push_index();
                let r = a
                    .iter()
                    .try_for_each(|x| self.check_json_under(tool, field, x, key, walk, redactions));
                walk.pop(mark);
                r
            }
            Value::Object(o) => {
                let mut seen: Vec<String> = Vec::with_capacity(o.len());
                for (k, x) in o {
                    let k2 = self.scan_str(tool, field, k, redactions)?;
                    if seen.contains(&k2) {
                        return Err(key_collision(field));
                    }
                    seen.push(k2);
                    let inner = inner_key(key, k);
                    let mark = walk.push_key(k);
                    let r = self.check_json_under(tool, field, x, Some(inner), walk, redactions);
                    walk.pop(mark);
                    r?;
                }
                Ok(())
            }
            Value::String(s) if walk.passes_control(s) => Ok(()),
            scalar => match key {
                Some(k) => self
                    .scan_scalar(tool, field, k, scalar, redactions)
                    .map(|_| ()),
                None => match scalar {
                    Value::String(s) => self.scan_str(tool, field, s, redactions).map(|_| ()),
                    _ => Ok(()),
                },
            },
        }
    }
}

/// Opaque transaction-bound gate session: it binds the gated batch to the policy and ontology
/// it was gated under and counts the redactions it made. Abandoning it publishes nothing.
pub struct IngestSubmitGate<'a> {
    api: &'a KabanApi,
    ctx: WriteCtx,
    /// The policy version the session gates under.
    policy_version: String,
    /// The governed set of the ontology the session gates under.
    governed: std::sync::Arc<kaban_core::GovernedSet>,
    /// Every `field:rule` redaction the session made.
    redactions: Vec<String>,
    nodes: std::collections::BTreeSet<String>,
    edges: std::collections::BTreeSet<String>,
}

impl IngestSubmitGate<'_> {
    /// Apply the exact remember per-field gate, counting its redactions in this session.
    ///
    /// # Errors
    /// Existing policy refusal.
    pub fn field(&mut self, field: &str, text: &str) -> Result<String, ApiError> {
        self.api
            .gate("ingest_submit", field, text, &mut self.redactions)
    }

    /// Apply the remember JSON property gate, counting its redactions in this session.
    ///
    /// # Errors
    /// Existing props-size, key-collision and policy refusals.
    pub fn props(&mut self, value: &Value) -> Result<Value, ApiError> {
        self.api
            .gate_json("ingest_submit", "props", value, &mut self.redactions)
    }

    /// Gate and bind a complete node before identity derivation; no caller can publish a
    /// subsequently changed or ungated node through this adapter.
    ///
    /// # Errors
    /// Existing remember field/joined/property gates and fixed numeric bounds.
    pub fn node(&mut self, mut node: kaban_core::NewNode) -> Result<kaban_core::NewNode, ApiError> {
        if node.tags.len() > 32 || node.path_scope.len() > crate::api::MAX_PATH_SCOPE {
            return Err(ApiError::Invalid(
                "too many tags or path_scope entries".into(),
            ));
        }
        let joined = std::iter::once(node.kind.as_str())
            .chain(std::iter::once(node.text.as_str()))
            .chain(node.summary.as_deref())
            .chain(node.tags.iter().map(String::as_str))
            .chain(node.path_scope.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        self.api.gate_joined("ingest_submit", "all", &joined)?;
        node.kind =
            kaban_core::ontology::resolve::normalise_stored(&self.field("kind", &node.kind)?);
        node.text = self.field("text", &node.text)?;
        node.summary = node
            .summary
            .as_deref()
            .map(|s| self.field("summary", s))
            .transpose()?;
        node.tags = node
            .tags
            .iter()
            .map(|s| self.field("tag", s))
            .collect::<Result<_, _>>()?;
        node.path_scope = crate::currency::canonical_path_scope(
            &node
                .path_scope
                .iter()
                .map(|s| self.field("path_scope", s))
                .collect::<Result<Vec<_>, _>>()?,
        );
        node.props = node.props.as_ref().map(|p| self.props(p)).transpose()?;
        if node
            .confidence
            .is_some_and(|n| !n.is_finite() || !(0.0..=1.0).contains(&n))
        {
            return Err(ApiError::Invalid("confidence".into()));
        }
        node.store_scope = kaban_core::StoreScope::Project;
        node.repo.clone_from(&self.api.config.repo);
        node.workspace_id = None;
        node.dup_key = self
            .api
            .registry
            .with_kind(&node.kind, |kind| {
                kind.governed
                    .as_ref()
                    .is_some_and(|g| g.dedup == kaban_core::ontology::meta::Dedup::TitleScope)
            })
            .filter(|dedup| *dedup)
            .map(|_| {
                crate::coderef::dup_key(node.summary.as_deref(), &node.text, &node.path_scope)
            });
        self.nodes.insert(
            serde_json::to_string(&node).map_err(|_| ApiError::Invalid("invalid_body".into()))?,
        );
        Ok(node)
    }

    /// Gate and bind an owned structural edge; governed semantics are never extractor input.
    ///
    /// # Errors
    /// Policy refusal, forbidden governed relation or malformed operation.
    pub fn edge(&mut self, mut edge: kaban_core::Op) -> Result<kaban_core::Op, ApiError> {
        let kaban_core::Op::UpsertEdge { rel, props, .. } = &mut edge else {
            return Err(ApiError::Invalid("invalid_batch".into()));
        };
        *rel = self.field("rel", rel)?;
        if matches!(
            rel.as_str(),
            "SUPERSEDES" | "INVALIDATES" | "CONTRADICTS" | "GOVERNS"
        ) {
            return Err(ApiError::Invalid("edge_not_allowed".into()));
        }
        *props = props.as_ref().map(|p| self.props(p)).transpose()?;
        self.edges.insert(
            serde_json::to_string(&edge).map_err(|_| ApiError::Invalid("invalid_body".into()))?,
        );
        Ok(edge)
    }

    /// Consume only this session's bound gated batch into atomic graph/job publication, with
    /// the session's redaction count. No actor or mutable context is accepted from the caller.
    ///
    /// # Errors
    /// Ungated/mutated batch, changed policy/authority or transactional publication failure.
    pub fn publish(
        self,
        mut request: kaban_core::JobSubmitRequest,
        accumulate: &mut dyn FnMut(
            &kaban_core::IngestJob,
            &kaban_core::BatchReport,
        ) -> kaban_core::Result<kaban_core::JobSubmitStateUpdate>,
    ) -> Result<kaban_core::JobSubmitReceipt, ApiError> {
        if self.policy_version != self.api.policy_version() || self.governed != self.api.governed()
        {
            return Err(ApiError::Invalid("stale_job_contract".into()));
        }
        for op in &request.batch.ops {
            let admitted = match op {
                kaban_core::Op::UpsertNode { node, .. } => self.nodes.contains(
                    &serde_json::to_string(node)
                        .map_err(|_| ApiError::Invalid("invalid_body".into()))?,
                ),
                kaban_core::Op::UpsertEdge { .. } => self.edges.contains(
                    &serde_json::to_string(op)
                        .map_err(|_| ApiError::Invalid("invalid_body".into()))?,
                ),
                _ => false,
            };
            if !admitted {
                return Err(ApiError::Invalid("ungated_batch".into()));
            }
        }
        request.redactions = u64::try_from(self.redactions.len()).unwrap_or(u64::MAX);
        self.api
            .store
            .job_submit(request, &self.ctx, accumulate)
            .map_err(ApiError::from)
    }
}

/// The key context of a leaf under `k` inside an object whose own context is `outer`: the
/// nearest credential-named ancestor key when there is one (so `{"password": [["a"]]}` and
/// `{"password": {"value": "a"}}` are still credential assignments, review R1-2), otherwise
/// the nearest key.
fn inner_key<'a>(outer: Option<&'a str>, k: &'a str) -> &'a str {
    match outer {
        Some(cred) if is_credential_key(cred) && !is_credential_key(k) => cred,
        _ => k,
    }
}

/// Two keys of one object that scan to the same name.
fn key_collision(field: &str) -> ApiError {
    ApiError::Rejected {
        field: field.into(),
        rules: vec!["props-key-collision".into()],
    }
}
