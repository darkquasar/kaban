//! Gated structured publication: graph, claims, successful audit and accumulation are atomic.

use super::types::{JobEnvelope, JobError, StructuredReturn};
use kaban_api::KabanApi;
use kaban_core::{
    BatchAssertion, BatchReport, BatchSubject, ClaimClass, Fixed, Governance, IngestJob, JobId,
    JobStatus, JobSubmitContract, JobSubmitRequest, JobSubmitStateUpdate, Layer, Lifecycle, Method,
    NewNode, Node, NodeId, Op, OutcomeKind, Rank, SourceRev, WriteBatch,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Metadata-only, bounded accumulation consumed by finish. An overflow never grants deletion
/// authority: previously admitted names are retained, and submit publication may continue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubmitState {
    /// Union of successful record identities, never local names or record contents.
    pub named_records: BTreeSet<NodeId>,
    /// At least one complete union exceeded the configured finish ceiling.
    pub overflow: bool,
    /// Number of proposed revision candidates across successful submits.
    pub revision_candidates: u64,
    /// Revisions of superseded or invalidated predecessors.
    pub predecessor_terminal: u64,
    /// Actual newly inserted assertions.
    pub assertions: u64,
    /// Number of successfully named edges across submits.
    pub edges: u64,
    /// Fixed skip-reason counts; no extractor-controlled keys.
    pub skipped: BTreeMap<String, u64>,
    /// Fixed evidence-drop reason counts.
    pub evidence_dropped: BTreeMap<String, u64>,
    /// First graph sequence across successful submits.
    pub first_seq: Option<u64>,
    /// Last graph sequence across successful submits.
    pub last_seq: Option<u64>,
}

#[derive(Clone)]
struct Placed {
    local_id: String,
    id: NodeId,
    kind: String,
    confidence: f64,
    predecessor: Option<NodeId>,
    terminal: bool,
    props: Value,
}

fn invalid() -> JobError {
    kaban_api::ApiError::Invalid("invalid_body".into()).into()
}

/// Submit one canonical raw body after pre-parse screening and checked-source reopening.
/// The returned JSON has passed the API response gate. No extractor is executed.
///
/// # Errors
/// Fixed parse/size refusals, scoped job/source/ontology refusals and atomic storage failures.
pub async fn submit(api: &KabanApi, root: &Path, raw: Value) -> Result<Value, JobError> {
    api.prepare_args("ingest_submit", &raw)?;
    if serde_json::to_vec(&raw).map_err(|_| invalid())?.len() > 1024 * 1024 {
        return Err(kaban_api::ApiError::Invalid("too_large".into()).into());
    }
    let ret: StructuredReturn = serde_json::from_value(raw).map_err(|_| invalid())?;
    if ret.records.len() > 256 {
        return Err(kaban_api::ApiError::Invalid("too_large".into()).into());
    }
    let job = JobId::parse(&ret.job).map_err(|_| JobError::Revoked)?;
    // Capture the database source revision before asynchronous source preparation. The
    // transaction checks it again rather than trusting a source read completed earlier.
    let initial = api
        .store()
        .job_get(&job, api.workspace())?
        .ok_or(JobError::Revoked)?;
    let initial_envelope = JobEnvelope::decode(&initial.plan_json).map_err(|_| JobError::Stale)?;
    let document = api
        .store()
        .get(&initial_envelope.document_id, true)?
        .ok_or(JobError::Revoked)?;
    let source_rev = SourceRev::from(&document);
    let contract = JobSubmitContract::from_job(&initial)?;
    let (row, envelope, source) =
        super::next::revalidate_fenced(api, root, &job, ret.fence).await?;
    if row.status != JobStatus::Open || JobSubmitContract::from_job(&row)? != contract {
        return Err(JobError::Stale);
    }
    if ret.chunk.is_some_and(|index| {
        usize::try_from(index)
            .ok()
            .is_none_or(|index| index >= envelope.chunks.len())
    }) {
        return Err(JobError::Selector);
    }
    let mut gate = api.begin_ingest_submit();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut drafts = Vec::new();
    for mut draft in ret.records {
        gate.field("local_id", &draft.local_id)?
            .trim()
            .clone_into(&mut draft.local_id);
        *counts.entry(draft.local_id.clone()).or_default() += 1;
        drafts.push(draft);
    }
    let mut skipped = Vec::<Value>::new();
    let mut evidence_dropped = Vec::<Value>::new();
    let mut placed = BTreeMap::<String, Placed>::new();
    let mut ops = Vec::new();
    let mut pending_edges = Vec::new();
    let chunks: BTreeMap<_, _> = envelope
        .chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            Ok((
                chunk.id.as_str().to_owned(),
                super::next::chunk_view(&envelope, &source, index)?.text,
            ))
        })
        .collect::<Result<_, JobError>>()?;
    for draft in drafts {
        let local = draft.local_id.clone();
        if counts.get(&local).copied().unwrap_or_default() > 1 {
            skip(&mut skipped, &local, "duplicate_local_id", None);
            continue;
        }
        if local.is_empty() || local.chars().count() > 128 {
            skip(&mut skipped, &local, "invalid_props", Some("local_id"));
            continue;
        }
        let kind =
            kaban_core::ontology::resolve::normalise_stored(&gate.field("kind", &draft.kind)?);
        if !row.kinds.contains(&kind) {
            skip(&mut skipped, &local, "kind_not_allowed", None);
            continue;
        }
        let mut props = gate.props(&draft.props.unwrap_or_else(|| json!({})))?;
        let internal = crate::extract::structured_prep::schema::INTERNAL_PROPERTIES
            .iter()
            .find(|key| props.get(**key).is_some());
        if let Some(key) = internal {
            skip(
                &mut skipped,
                &local,
                "invalid_props",
                Some(&format!("props.{key}")),
            );
            continue;
        }
        remove_empty(&mut props);
        if kind == "Decision"
            && props
                .get("rationale")
                .and_then(Value::as_str)
                .is_none_or(|value| value.trim().is_empty())
        {
            skip(
                &mut skipped,
                &local,
                "invalid_props",
                Some("props.rationale"),
            );
            continue;
        }
        if let Err(error) = kaban_api::validate_kind_props(api.registry(), &kind, &props) {
            let field = match error {
                kaban_api::ApiError::Invalid(field) if field.starts_with("props") => field,
                _ => "props".into(),
            };
            skip(&mut skipped, &local, "invalid_props", Some(&field));
            continue;
        }
        // Symbols are content, even where no symbol resolver is enabled. Never derive an
        // authority-bearing edge from an ungated or unresolved symbol string.
        for symbol in &draft.symbols {
            let _ = gate.field("symbols", symbol)?;
        }
        let mut evidence = Vec::new();
        let mut evidence_ids = Vec::new();
        for reference in draft.evidence {
            let id = if NodeId::parse(&reference.chunk_id).is_ok() {
                reference.chunk_id
            } else {
                gate.field("chunk_id", &reference.chunk_id)?
            };
            let quote = gate.field("quote", &reference.quote)?;
            let reason = match chunks.get(&id) {
                None => Some("chunk_not_in_job"),
                Some(text)
                    if quote.trim().is_empty()
                        || !whitespace(text).contains(&whitespace(&quote)) =>
                {
                    Some("quote_not_found")
                }
                Some(_) => None,
            };
            if let Some(reason) = reason {
                evidence_dropped.push(json!({"local_id":local,"reason":reason}));
            } else {
                let id = NodeId::parse(&id).map_err(|_| JobError::Stale)?;
                evidence.push(json!({"id":id,"quote":quote}));
                evidence_ids.push(id);
            }
        }
        if !evidence.is_empty() {
            props
                .as_object_mut()
                .ok_or_else(invalid)?
                .insert("evidence".into(), json!(evidence));
        }
        let mut node = gate.node(NewNode {
            kind: kind.clone(),
            text: draft.text,
            summary: draft.summary,
            tags: draft.tags,
            confidence: draft.confidence,
            props: Some(props),
            path_scope: draft.path_scope,
            ..NewNode::default()
        })?;
        if node.text.trim().is_empty() {
            skip(&mut skipped, &local, "invalid_props", Some("text"));
            continue;
        }
        let base = derived_id(
            b"kaban-parse/v1/record",
            &[api.workspace().as_str(), &row.source_id, &kind, &local],
        )?;
        let mut id = base.clone();
        let mut predecessor = None;
        let mut terminal = false;
        if let Some(existing) = api.store().get(&base, true)? {
            if existing.workspace_id != api.workspace().as_str() {
                skip(&mut skipped, &local, "unknown_target", None);
                continue;
            }
            if existing.kind != kind {
                skip(&mut skipped, &local, "kind_mismatch", None);
                continue;
            }
            terminal = matches!(
                existing.governance,
                Governance::Superseded | Governance::Invalidated
            );
            if existing.lifecycle == Lifecycle::Purged
                || (!terminal && existing.lifecycle != Lifecycle::Active)
            {
                skip(&mut skipped, &local, "hidden_exists", None);
                continue;
            }
            if matches!(
                existing.governance,
                Governance::Accepted | Governance::Superseded | Governance::Invalidated
            ) {
                if same_content(&existing, &node) {
                    // Acceptance metadata is derived internally, never extractor input. Keep
                    // exact original props so the store's unchanged fast path preserves it.
                    node.props = Some(existing.props.clone());
                    node = gate.node(node)?;
                } else {
                    let hash = content_hash(api.workspace().as_str(), &base, &node)?;
                    id = derived_id(b"kaban-parse/v1/revision", &[base.as_str(), &hash])?;
                    predecessor = Some(base.clone());
                }
            }
        }
        if id != base
            && let Some(candidate) = api.store().get(&id, true)?
        {
            if candidate.workspace_id != api.workspace().as_str()
                || candidate.lifecycle != Lifecycle::Active
                || candidate.kind != kind
                || !same_content(&candidate, &node)
            {
                skip(&mut skipped, &local, "hidden_exists", None);
                continue;
            }
            // A human may have accepted an unchanged candidate since the previous submit.
            node.props = Some(candidate.props);
            node = gate.node(node)?;
        }
        let confidence = node.confidence.unwrap_or(1.0);
        let props = node.props.clone().unwrap_or_else(|| json!({}));
        if let Some(original) = &predecessor {
            pending_edges.push(owned_edge(
                &id,
                "RELATES_TO",
                original,
                json!({"owner":"kaban-parse","role":"revision_candidate"}),
            ));
        }
        ops.push(Op::UpsertNode {
            id: id.clone(),
            node,
        });
        pending_edges.push(owned_edge(
            &id,
            "DERIVED_FROM",
            &envelope.document_id,
            json!({
                "owner":"kaban-parse","mode":row.mode,"with":row.dialect,
                "pipeline_version":row.pipeline_version,"content_hash":row.content_hash,
                "chunk_ids":evidence_ids
            }),
        ));
        for chunk in evidence_ids {
            pending_edges.push(owned_edge(
                &chunk,
                "EVIDENCE_FOR",
                &id,
                json!({"owner":"kaban-parse"}),
            ));
        }
        placed.insert(
            local.clone(),
            Placed {
                local_id: local,
                id,
                kind,
                confidence,
                predecessor,
                terminal,
                props,
            },
        );
    }
    let mut returned_edges = ret.edges;
    returned_edges.extend(
        ret.links
            .into_iter()
            .map(|link| kaban_api::ingest_jobs::EdgeDraft {
                from_local_id: link.from_local_id,
                rel: link.rel,
                to_local_id: None,
                to_id: Some(link.to_id),
            }),
    );
    for edge in returned_edges {
        let from = gate
            .field("local_id", &edge.from_local_id)?
            .trim()
            .to_owned();
        let rel = gate.field("rel", &edge.rel)?;
        let local_target = edge
            .to_local_id
            .as_deref()
            .map(|s| gate.field("local_id", s))
            .transpose()?;
        let target_id = edge
            .to_id
            .as_deref()
            .map(|s| {
                if NodeId::parse(s).is_ok() {
                    Ok(s.to_owned())
                } else {
                    gate.field("to_id", s)
                }
            })
            .transpose()?;
        let Some(src) = placed.get(&from) else {
            skip(&mut skipped, &from, "unknown_ref", None);
            continue;
        };
        if local_target.is_some() == target_id.is_some() {
            skip(&mut skipped, &from, "unknown_ref", None);
            continue;
        }
        let target = if let Some(local) = local_target {
            placed
                .get(local.trim())
                .map(|target| (target.id.clone(), target.kind.clone()))
        } else if let Some(id) = target_id {
            resolve_target(api, &id)?
        } else {
            None
        };
        let Some((dst, target_kind)) = target else {
            skip(&mut skipped, &from, "unknown_target", None);
            continue;
        };
        if forbidden(&rel) || !allowed_edge(&envelope, &src.kind, &rel, &target_kind) {
            skip(&mut skipped, &from, "edge_not_allowed", None);
            continue;
        }
        pending_edges.push(owned_edge(
            &src.id,
            &rel,
            &dst,
            json!({"owner":"kaban-parse"}),
        ));
    }
    for target in ret.about {
        let target = if NodeId::parse(&target).is_ok() {
            target
        } else {
            gate.field("to_id", &target)?
        };
        if let Some((id, _)) = resolve_target(api, &target)? {
            pending_edges.push(owned_edge(
                &envelope.document_id,
                "ABOUT",
                &id,
                json!({"owner":"kaban-parse"}),
            ));
        } else {
            skip(&mut skipped, "", "unknown_target", None);
        }
    }
    // E38's authoritative brief currently requests no uncertain pairs. The core filter
    // therefore rejects every unsolicited judgement; a return cannot create its own request.
    let returned: Vec<kaban_core::PairJudgement> =
        ret.pair_judgements.into_iter().map(Into::into).collect();
    for (_, reason) in kaban_core::er::judge_filter(&[], &returned).skipped {
        skip(&mut skipped, "", reason, None);
    }
    let layer = match row.dialect.as_str() {
        "llm" => Layer::Llm,
        "gliner" => Layer::Gliner,
        _ => Layer::Script,
    };
    let lineage = kaban_core::lineage_id(
        layer,
        &kaban_core::LineageParts::Machine {
            content_hash_or_source: &row.content_hash,
            extractor_id: &row.dialect,
            version: &row.pipeline_version,
        },
    );
    let mut assertions = Vec::new();
    for record in placed.values() {
        assertions.push(assertion(
            BatchSubject::Record(record.id.clone()),
            ClaimClass::KindOfRecord,
            json!({"kind":record.kind,"props":record.props}),
            layer,
            &row,
            &lineage,
            record.confidence,
        )?);
    }
    let mut named_edges = BTreeSet::new();
    for edge in pending_edges {
        let edge = gate.edge(edge)?;
        if let Op::UpsertEdge {
            src,
            rel,
            dst,
            props,
        } = &edge
        {
            if !named_edges.insert((
                src.as_str().to_owned(),
                dst.as_str().to_owned(),
                rel.clone(),
            )) {
                continue;
            }
            // Outgoing record/revision/provenance edges use the source record's score;
            // evidence citations use their destination record's score. Document ABOUT
            // has no record confidence and uses the neutral raw input score of one,
            // still multiplied by the configured layer prior by reconciliation.
            let confidence = placed
                .values()
                .find(|record| record.id == *src)
                .or_else(|| placed.values().find(|record| record.id == *dst))
                .map_or(1.0, |record| record.confidence);
            assertions.push(assertion(
                BatchSubject::Edge {
                    src: src.clone(),
                    rel: rel.clone(),
                    dst: dst.clone(),
                },
                ClaimClass::Edge,
                props.clone().unwrap_or_else(|| json!({})),
                layer,
                &row,
                &lineage,
                confidence,
            )?);
        }
        ops.push(edge);
    }
    let mut batch = WriteBatch::new(api.workspace().clone(), ops);
    batch.assertions = assertions;
    let request = JobSubmitRequest {
        job: job.clone(),
        contract: contract.with_fence(ret.fence),
        document: envelope.document_id,
        source_rev,
        chunks: envelope
            .chunks
            .iter()
            .map(|chunk| chunk.id.clone())
            .collect(),
        batch,
    };
    let cap = usize::try_from(api.engine().ingest.finish_cap).map_err(|_| invalid())?;
    let mut previous_redactions = row.redactions;
    let mut accumulate = |current: &IngestJob, report: &BatchReport| {
        previous_redactions = current.redactions;
        accumulate_state(
            current,
            report,
            &placed,
            &named_edges,
            &skipped,
            &evidence_dropped,
            cap,
        )
    };
    let receipt = gate
        .publish(request, &mut accumulate)
        .map_err(|error| match error {
            kaban_api::ApiError::Store(kaban_core::StoreError::LeaseLost { .. }) => {
                JobError::LeaseLost
            }
            kaban_api::ApiError::Store(kaban_core::StoreError::NotFound(_)) => JobError::Revoked,
            other => JobError::Api(other),
        })?;
    let batch = &receipt.batch;
    let mut records = Vec::new();
    let mut revisions = Vec::new();
    for outcome in &batch.outcomes {
        let Some(record) = placed.values().find(|record| record.id == outcome.id) else {
            continue;
        };
        let status = match outcome.outcome {
            OutcomeKind::Inserted => "created",
            OutcomeKind::Updated => "updated",
            OutcomeKind::Unchanged => "unchanged",
            _ => {
                skip(&mut skipped, &record.local_id, "kind_not_allowed", None);
                continue;
            }
        };
        records.push(json!({"local_id":record.local_id,"id":record.id,"outcome":if record.predecessor.is_some() { "revision_candidate" } else { status }}));
        if let Some(predecessor) = &record.predecessor {
            revisions.push(json!({"local_id":record.local_id,"id":record.id,"of":predecessor}));
        }
    }
    let output = json!({"job":job,"status":"open","chunk":ret.chunk,"records":records,
        "revision_candidates":revisions,"predecessor_terminal":placed.values().filter(|r| r.predecessor.is_some() && r.terminal).count(),
        "skipped":skipped,"evidence_dropped":evidence_dropped,
        "redactions":receipt.job.redactions.checked_sub(previous_redactions).ok_or_else(invalid)?,
        "created":batch.inserted,"updated":batch.updated,"unchanged":batch.unchanged,
        "edges":named_edges.len(),"assertions":batch.assertions_inserted,
        "changes":{"first_seq":batch.outbox_first_seq,"last_seq":batch.outbox_last_seq}});
    let _: kaban_api::IngestSubmitReport =
        serde_json::from_value(output.clone()).map_err(|_| invalid())?;
    let clean = api.gate_response("ingest_submit", &output)?;
    let _: kaban_api::IngestSubmitReport =
        serde_json::from_value(clean.clone()).map_err(|_| invalid())?;
    Ok(clean)
}

fn skip(skipped: &mut Vec<Value>, local: &str, reason: &str, field: Option<&str>) {
    let mut entry = json!({"local_id":local,"reason":reason});
    if let Some(field) = field {
        entry["field"] = json!(field);
    }
    skipped.push(entry);
}

fn whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn remove_empty(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                remove_empty(child);
            }
            object.retain(|_, child| {
                !matches!(child, Value::String(s) if s.trim().is_empty())
                    && !matches!(child, Value::Array(a) if a.is_empty())
            });
        }
        Value::Array(array) => {
            for child in array.iter_mut() {
                remove_empty(child);
            }
            array.retain(|child| {
                !matches!(child, Value::String(s) if s.trim().is_empty())
                    && !matches!(child, Value::Array(a) if a.is_empty())
            });
        }
        _ => {}
    }
}

fn derived_id(domain: &[u8], fields: &[&str]) -> Result<NodeId, JobError> {
    let mut hash = blake3::Hasher::new();
    hash.update(domain);
    for field in fields {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    NodeId::try_from(uuid::Uuid::from_bytes(bytes)).map_err(|_| invalid())
}

fn content_hash(workspace: &str, id: &NodeId, node: &NewNode) -> Result<String, JobError> {
    let mut bytes = Vec::new();
    let mut put = |field: &str| {
        bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
        bytes.extend_from_slice(field.as_bytes());
    };
    let props = serde_json::to_string(node.props.as_ref().unwrap_or(&Value::Null))
        .map_err(|_| invalid())?;
    for field in [
        "kaban-batch/v1",
        workspace,
        &node.kind,
        id.as_str(),
        node.text.trim(),
        node.summary.as_deref().unwrap_or(""),
        if node.summary.is_some() { "1" } else { "0" },
    ] {
        put(field);
    }
    let mut tags = node.tags.clone();
    tags.sort_unstable();
    put(&tags.len().to_string());
    for tag in &tags {
        put(tag);
    }
    put(&props);
    for scope in &node.path_scope {
        put(scope);
    }
    Ok(kaban_policy::sha256_hex(&bytes))
}

fn same_content(existing: &Node, node: &NewNode) -> bool {
    let mut props = existing.props.clone();
    if let Some(object) = props.as_object_mut() {
        object.remove("accepted_commit");
    }
    let mut old_tags = existing.tags.clone();
    old_tags.sort_unstable();
    let mut tags = node.tags.clone();
    tags.sort_unstable();
    existing.text.trim() == node.text.trim()
        && existing.summary == node.summary
        && old_tags == tags
        && props == node.props.clone().unwrap_or_else(|| json!({}))
        && existing.path_scope == node.path_scope
}

fn owned_edge(src: &NodeId, rel: &str, dst: &NodeId, props: Value) -> Op {
    Op::UpsertEdge {
        src: src.clone(),
        rel: rel.into(),
        dst: dst.clone(),
        props: Some(props),
    }
}

fn forbidden(rel: &str) -> bool {
    matches!(
        rel,
        "SUPERSEDES" | "INVALIDATES" | "CONTRADICTS" | "GOVERNS"
    )
}

fn allowed_edge(envelope: &JobEnvelope, from: &str, rel: &str, to: &str) -> bool {
    envelope
        .target_schema
        .get("allowed_edges")
        .and_then(Value::as_array)
        .is_some_and(|edges| edges.iter().any(|edge| edge == &json!([from, rel, to])))
}

fn resolve_target(api: &KabanApi, raw: &str) -> Result<Option<(NodeId, String)>, JobError> {
    let Ok(id) = NodeId::parse(raw) else {
        return Ok(None);
    };
    Ok(api
        .store()
        .get(&id, false)?
        .filter(|node| {
            node.workspace_id == api.workspace().as_str() && node.lifecycle == Lifecycle::Active
        })
        .map(|node| (id, node.kind)))
}

fn assertion(
    subject: BatchSubject,
    class: ClaimClass,
    value: Value,
    layer: Layer,
    row: &IngestJob,
    lineage: &str,
    confidence: f64,
) -> Result<BatchAssertion, JobError> {
    Ok(BatchAssertion {
        subject,
        claim_class: class,
        predicate: None,
        object: None,
        value,
        layer,
        method: Method::IngestSubmit,
        extractor_id: Some(row.dialect.clone()),
        extractor_version: None,
        source_id: Some(row.source_id.clone()),
        lineage_id: lineage.into(),
        raw_score: Fixed::from_f64(confidence).map_err(|_| invalid())?,
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
    })
}

fn accumulate_state(
    current: &IngestJob,
    report: &BatchReport,
    placed: &BTreeMap<String, Placed>,
    edges: &BTreeSet<(String, String, String)>,
    skipped: &[Value],
    evidence: &[Value],
    cap: usize,
) -> kaban_core::Result<JobSubmitStateUpdate> {
    let bad = || kaban_core::StoreError::InvalidName("submit_state".into(), "invalid");
    let mut envelope = JobEnvelope::decode(&current.plan_json).map_err(|_| bad())?;
    let mut state: SubmitState =
        serde_json::from_value(envelope.submit_state).map_err(|_| bad())?;
    if state.named_records.len() > cap || current.named_edges.len() > cap {
        return Err(bad());
    }
    let mut names: BTreeSet<_> = current.named_edges.iter().cloned().collect();
    for outcome in &report.outcomes {
        if !matches!(
            outcome.outcome,
            OutcomeKind::Inserted | OutcomeKind::Updated | OutcomeKind::Unchanged
        ) {
            continue;
        }
        if state.named_records.len() < cap || state.named_records.contains(&outcome.id) {
            state.named_records.insert(outcome.id.clone());
        } else {
            state.overflow = true;
        }
        if let Some(record) = placed.values().find(|r| r.id == outcome.id)
            && record.predecessor.is_some()
        {
            state.revision_candidates = state.revision_candidates.checked_add(1).ok_or_else(bad)?;
            if record.terminal {
                state.predecessor_terminal =
                    state.predecessor_terminal.checked_add(1).ok_or_else(bad)?;
            }
        }
    }
    for edge in edges {
        if names.len() < cap || names.contains(edge) {
            names.insert(edge.clone());
        } else {
            state.overflow = true;
        }
    }
    state.assertions = state
        .assertions
        .checked_add(u64::from(report.assertions_inserted))
        .ok_or_else(bad)?;
    state.edges = state
        .edges
        .checked_add(u64::try_from(edges.len()).map_err(|_| bad())?)
        .ok_or_else(bad)?;
    for (entries, counts) in [
        (skipped, &mut state.skipped),
        (evidence, &mut state.evidence_dropped),
    ] {
        for entry in entries {
            let reason = entry
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(bad)?;
            let count = counts.entry(reason.into()).or_default();
            *count = count.checked_add(1).ok_or_else(bad)?;
        }
        if counts.len() > 16 {
            return Err(bad());
        }
    }
    state.first_seq = state.first_seq.or(report.outbox_first_seq);
    state.last_seq = report.outbox_last_seq.or(state.last_seq);
    envelope.submit_state = serde_json::to_value(state)?;
    Ok(JobSubmitStateUpdate {
        plan_json: envelope.encode().map_err(|_| bad())?,
        named_edges: names.into_iter().collect(),
    })
}
