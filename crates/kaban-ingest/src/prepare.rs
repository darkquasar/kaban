//! The per-document pipeline behind the `Preparer` seam (docs-ingest-plan.md DI1.4 wiring;
//! DI0.3 `PrepareInput`): bytes already read through `Candidate::open_checked` come in, a
//! [`PreparedDocument`] goes out with every string sanitized and every id computed from the
//! sanitized texts only. DI1.5's batch builder turns a [`PreparedGraph`] into `WriteBatch` ops.
//!
//! ```text
//!   bytes ==> extractor_for(format).extract ==> admit_frontmatter ==> interpret_frontmatter
//!         ==> gate_metadata ==> chunk_with_stream ==> gate_document(stream, maps)
//!         ==> assemble: section_id / chunk_id over sanitized text_hash (headings gated)
//!         ==> gate_links_and_tags(coverage) ==> content_hash + position_fingerprint (RD1-1, RD2-4)
//!         ==> PreparedDocument::{Ready(PreparedGraph), Blocked(DocumentProps), Failed(DocumentProps)}
//! ```
//!
//! Identity order (PR-2): no `content_hash`, `section_id` or `chunk_id` exists before
//! `gate_document` returned `Proceed`; a blocked document carries only `keyed_id("bytes")`.

use crate::cancel::{ExtractCtx, ExtractError};
use crate::chunk::{ChunkConfig, Chunked, chunk_with_stream};
use crate::commit::{Prepared, SkipReason};
use crate::extract::{decode, extractor_for};
use crate::gate::{
    Admission, DerivedContext, GateDecision, MetaDecision, admit_frontmatter, gate_document,
    gate_links_and_tags, gate_metadata,
};
use crate::graph::{
    AssembleInput, Assembled, ExistingDocument, assemble, build_batch, document_only_batch,
    ingest_batch, resolve_links,
};
use crate::hash::{
    GatedPath, Identity, SkipKey, content_hash, keyed_id, position_fingerprint, should_skip,
};
use crate::ids::{PIPELINE_VERSION, document_id};
use crate::ingestor::{IngestConfig, PrepareInput, Preparer};
use crate::model::{DocStatus, DocumentProps, Format, IngestError, PreparedGraph, SourceId};
use kaban_api::KabanApi;
use kaban_core::{AuditEntry, AuditOp, GraphStore, NodeId, WorkspaceId, WriteBatch};
use kaban_policy::{AuditSigner, Policy};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, RwLock};

/// One document to prepare: identity-safe inputs (the path already passed its gate) and the
/// bytes read through `Candidate::open_checked`.
#[derive(Debug, Clone, Copy)]
pub struct PrepareRequest<'a> {
    /// The source identity.
    pub source: &'a SourceId,
    /// The gated relative path.
    pub path: &'a GatedPath,
    /// Format chosen by the walker.
    pub format: Format,
    /// File size.
    pub size: u64,
    /// Modification time (Unix seconds).
    pub mtime: i64,
    /// The document bytes.
    pub bytes: &'a [u8],
    /// `config_hash` (DI0.2) of the run.
    pub config_hash: &'a str,
}

/// Outcome of one document's pipeline.
#[derive(Debug, Clone)]
pub enum PreparedDocument {
    /// The document passed the gate: nodes with ids, gated links and tags, audit rows.
    Ready {
        /// The graph to build the batch from.
        graph: Box<PreparedGraph>,
        /// Audit rows for the same transaction.
        audit: Vec<AuditEntry>,
    },
    /// The gate skipped the file: `Document{status: blocked}` with `blocked_hmac`, no title, empty
    /// frontmatter, no `content_hash`, zero chunks.
    Blocked {
        /// The blocked node's props.
        props: DocumentProps,
        /// Audit rows for the same transaction.
        audit: Vec<AuditEntry>,
    },
    /// No extractor for the format, or the parser rejected the bytes (D8): `Document{status:
    /// failed, error}`; the next run retries.
    Failed {
        /// The failed node's props (gated error).
        props: DocumentProps,
    },
}

fn base_props(req: &PrepareRequest<'_>, extractor: &str, status: DocStatus) -> DocumentProps {
    DocumentProps {
        source_id: req.source.as_str().to_owned(),
        path: req.path.relative_path().to_owned(),
        content_hash: None,
        blocked_hmac: None,
        position_fingerprint: None,
        version: 0,
        current: true,
        format: req.format,
        size: req.size,
        mtime: req.mtime,
        title: None,
        lang: None,
        frontmatter: Value::Object(serde_json::Map::new()),
        pipeline_version: PIPELINE_VERSION,
        extractor: extractor.to_owned(),
        extractor_version: extractor.to_owned(),
        config_hash: req.config_hash.to_owned(),
        status,
        error: None,
        needs_ocr: false,
        page_count: None,
        blocked_chunks: 0,
        table_unknown_chunks: 0,
        rule_ids: BTreeMap::new(),
    }
}

fn blocked(
    policy: &Policy,
    signer: &dyn AuditSigner,
    workspace: &WorkspaceId,
    req: &PrepareRequest<'_>,
    extractor: &str,
    rule_ids: Vec<String>,
    mut audit: Vec<AuditEntry>,
) -> PreparedDocument {
    let mut props = base_props(req, extractor, DocStatus::Blocked);
    let keyed = keyed_id(signer, "bytes", req.bytes);
    for r in &rule_ids {
        *props.rule_ids.entry(r.clone()).or_default() += 1;
    }
    audit.push(AuditEntry {
        op: AuditOp::Reject,
        tool: "ingest".into(),
        field: "document".into(),
        workspace_id: workspace.clone(),
        policy_version: policy.effective_hash().to_owned(),
        input_hmac: keyed.clone(),
        rules: rule_ids,
        node_id: Some(document_id(req.source).as_str().to_owned()),
    });
    props.blocked_hmac = Some(keyed);
    PreparedDocument::Blocked { props, audit }
}

/// Run the pipeline for one document (see the module doc). `ctx.check()` is honoured by the
/// extractor, the chunker's caller and the gate.
///
/// # Errors
/// `Cancelled`/`DeadlineExceeded` from `ctx` (as `IngestError::Extract`).
pub fn prepare_document(
    policy: &Policy,
    signer: &dyn AuditSigner,
    ctx: &ExtractCtx,
    workspace: &WorkspaceId,
    req: &PrepareRequest<'_>,
    cfg: &ChunkConfig,
) -> Result<PreparedDocument, IngestError> {
    let Some(extractor) = extractor_for(req.format) else {
        let mut props = base_props(req, "", DocStatus::Failed);
        props.error = Some("no extractor for this format".to_owned());
        return Ok(PreparedDocument::Failed { props });
    };
    let mut extracted = match extractor.extract(req.bytes, ctx) {
        Ok(e) => e,
        // a parser rejection (DI2.1 `Format`, D8): the failed node keeps its path, format and
        // extractor like the no-extractor case, with the fixed class as `error`; the parser's
        // own error is not logged (it may quote the bytes) and the next run retries
        Err(ExtractError::Format { class, .. }) => {
            let mut props = base_props(req, extractor.id(), DocStatus::Failed);
            props.error = Some(class.to_owned());
            return Ok(PreparedDocument::Failed { props });
        }
        Err(e) => return Err(e.into()),
    };
    ctx.check()?;

    // (1a) the frontmatter source is admitted before anything interprets it (RD2-1)
    let frontmatter_source = extracted.frontmatter_source.take();
    let admitted = match frontmatter_source
        .as_deref()
        .map(|src| admit_frontmatter(policy, src))
    {
        None => None,
        Some(Admission::Admitted(a)) => Some(a),
        Some(Admission::SkipFile { rule_ids }) => {
            return Ok(blocked(
                policy,
                signer,
                workspace,
                req,
                extractor.id(),
                rule_ids,
                Vec::new(),
            ));
        }
    };
    if let Some(a) = &admitted {
        extracted.interpret_frontmatter(a);
    }

    // (1b) metadata before any hash
    let meta = match gate_metadata(
        policy,
        signer,
        workspace,
        extracted.title.as_deref(),
        &extracted.frontmatter,
        admitted.as_ref(),
    ) {
        MetaDecision::Proceed(meta) => meta,
        MetaDecision::SkipFile { rule_ids } => {
            return Ok(blocked(
                policy,
                signer,
                workspace,
                req,
                extractor.id(),
                rule_ids,
                Vec::new(),
            ));
        }
    };

    // chunk (keeping the pre-chunk stream), then (2) the stream and (3) every chunk
    let Chunked { sections, stream } = chunk_with_stream(&extracted, cfg);
    ctx.check()?;
    let drafts: Vec<_> = sections
        .iter()
        .flat_map(|(_, c)| c.iter().cloned())
        .collect();
    let gated = match gate_document(
        policy, signer, ctx, workspace, meta, &stream, drafts, None, cfg,
    )? {
        GateDecision::Proceed(g) => g,
        GateDecision::SkipFile { rule_ids, audit } => {
            return Ok(blocked(
                policy,
                signer,
                workspace,
                req,
                extractor.id(),
                rule_ids,
                audit,
            ));
        }
    };

    // (4) only now: the ids over sanitized text
    let mut props = base_props(
        req,
        extractor.id(),
        if extracted.partial {
            DocStatus::Partial
        } else {
            DocStatus::Ok
        },
    );
    props.title = gated.meta.title;
    props.frontmatter = gated.meta.frontmatter;
    props.lang.clone_from(&extracted.lang);
    props.error = gated.error;
    props.needs_ocr = extracted.needs_ocr;
    props.page_count = extracted.page_count;
    props.blocked_chunks = gated.blocked_chunks;
    // counted after gating (RD3-4): a stubbed chunk keeps its draft flag, so the count is the
    // chunks whose source rows looked tabular, whatever the gate did to their text
    props.table_unknown_chunks = u32::try_from(
        gated
            .chunks
            .iter()
            .filter(|c| c.draft.table_unknown)
            .count(),
    )
    .unwrap_or(u32::MAX);
    props.rule_ids = gated.rule_ids;

    let Assembled {
        mut graph,
        mut audit,
        redacted_sections,
    } = assemble(
        policy,
        signer,
        workspace,
        AssembleInput {
            source: req.source,
            props,
            sections: &sections,
            chunks: gated.chunks,
            audit: gated.audit,
            pipeline_version: PIPELINE_VERSION,
            slice_bytes: cfg.slice_bytes,
        },
    );
    // (4b) derived links and tags: removed where a hit covered their source span (RD2-3),
    // then gated as fields
    let (source, _) = decode(req.bytes);
    let (links, tags) = gate_links_and_tags(
        policy,
        signer,
        workspace,
        &extracted.links,
        &extracted.tags,
        Some(DerivedContext {
            source: &source,
            stream: &stream,
            coverage: &gated.coverage,
            redacted_sections: &redacted_sections,
        }),
        &mut graph.document.props.rule_ids,
        &mut audit,
    );
    graph.links = links;
    graph.tags = tags;
    // (5) the content hash over the assembled, sanitized graph (RD1-1): title, frontmatter,
    // heading paths, chunk texts, stub hashes, links and tags, nothing from `req.bytes`; and
    // the keyed positional fingerprint for the freshness decision (RD2-4)
    graph.document.props.content_hash = Some(content_hash(&graph));
    graph.document.props.position_fingerprint = Some(position_fingerprint(signer, &graph));
    Ok(PreparedDocument::Ready {
        graph: Box::new(graph),
        audit,
    })
}

/// The `Preparer` of the real pipeline (docs-ingest-plan.md DI1.5): one per `Ingestor`,
/// sharing the API's policy handle (D-3), the store (skip lookups, `ExistingDocument`, link
/// targets) and the audit signer. Runs on the pool's blocking threads.
pub struct DocumentPreparer {
    api: KabanApi,
    dictionary: RwLock<Option<Arc<crate::Dictionary>>>,
    telemetry: RwLock<Option<kaban_api::GazetteerTelemetry>>,
    store: Arc<dyn GraphStore>,
    policy: Arc<RwLock<Policy>>,
    signer: Arc<dyn AuditSigner>,
    chunk: ChunkConfig,
    cfg: IngestConfig,
    capture: Option<Arc<PreparationCapture>>,
    candidate_only: bool,
    candidate_run_digest: RwLock<Option<String>>,
}

/// Synchronous gated preparation projection, not a publication receipt or file authority.
pub(crate) type PreparationCapture = dyn Fn(&PreparedGraph, &str) + Send + Sync;

impl std::fmt::Debug for DocumentPreparer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentPreparer")
            .field("chunk", &self.chunk)
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl DocumentPreparer {
    /// Assemble the preparer; the policy is `api.policy_handle()` (D-3).
    pub fn new(
        api: &KabanApi,
        store: Arc<dyn GraphStore>,
        signer: Arc<dyn AuditSigner>,
        chunk: ChunkConfig,
        cfg: IngestConfig,
    ) -> Self {
        Self {
            api: api.clone(),
            dictionary: RwLock::new(None),
            telemetry: RwLock::new(None),
            store,
            policy: api.policy_handle(),
            signer,
            chunk,
            cfg,
            capture: None,
            candidate_only: false,
            candidate_run_digest: RwLock::new(None),
        }
    }

    /// Capture one bounded document's already-gated graph without repeating the pipeline.
    /// The consumer must verify normal publication before reporting it as indexed.
    pub(crate) fn with_capture(mut self, capture: Arc<PreparationCapture>) -> Self {
        self.capture = Some(capture);
        self
    }

    /// Structured preparation reuses the budgeted dictionary but never publishes L1 edges.
    pub(crate) fn with_candidates_only(mut self) -> Self {
        self.candidate_only = true;
        self
    }

    pub(crate) fn candidates(&self, graph: &PreparedGraph) -> Vec<crate::Mention> {
        let dictionary = self
            .dictionary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        dictionary.as_ref().map_or_else(Vec::new, |dictionary| {
            graph
                .sections
                .iter()
                .flat_map(|s| &s.chunks)
                .filter(|c| c.props.stub_hmac.is_none())
                .flat_map(|c| crate::find_mentions(dictionary, &c.id, &c.props.text))
                .filter(|m| m.target != graph.document.id)
                .collect()
        })
    }

    /// Title matches share this run's admitted budgeted dictionary; they grant no edges.
    pub(crate) fn title_candidates(&self, graph: &PreparedGraph) -> Vec<crate::Mention> {
        let dictionary = self
            .dictionary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        dictionary
            .as_ref()
            .zip(graph.document.props.title.as_deref())
            .map_or_else(Vec::new, |(dictionary, title)| {
                crate::find_mentions(dictionary, &graph.document.id, title)
                    .into_iter()
                    .filter(|hit| hit.target != graph.document.id)
                    .collect()
            })
    }

    fn policy(&self) -> std::sync::RwLockReadGuard<'_, Policy> {
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The stored current Document under `id`, if any.
    fn existing(&self, id: &NodeId) -> Result<Option<ExistingDocument>, IngestError> {
        Ok(self
            .store
            .get(id, true)?
            .as_ref()
            .map(ExistingDocument::from_node))
    }

    /// Whether a Document row will exist under `id` when this document's batch is applied:
    /// stored already (any status, a placeholder included), or a document of this run whose
    /// path sorts before this one (the committer applies batches in path order, RD-3, so it
    /// is committed first). A store error counts as absent, so a link degrades to a
    /// placeholder instead of failing the document.
    fn target_stored(&self, input: &PrepareInput, id: &NodeId, path: &str) -> bool {
        (input.run_documents.contains(id)
            && path.as_bytes() < input.path.relative_path().as_bytes())
            || self.store.get(id, true).ok().flatten().is_some()
    }

    /// Read the candidate's bytes through `open_checked` (RD-2; bounded by `max_file_bytes`).
    fn read(input: &PrepareInput) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(usize::try_from(input.size).unwrap_or(0));
        input.candidate.open_checked()?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn prepared(
        input: &PrepareInput,
        status: DocStatus,
        batch: WriteBatch,
        audit: Vec<AuditEntry>,
    ) -> Prepared {
        Prepared {
            seq: input.seq,
            source: Some(input.source.clone()),
            document_id: input.document_id.clone(),
            status,
            graph: None,
            batch,
            audit,
            skip: None,
        }
    }

    /// An empty batch: the committer counts the document as skipped and writes nothing.
    fn skipped(input: &PrepareInput, why: SkipReason) -> Prepared {
        let mut p = Self::prepared(
            input,
            DocStatus::Ok,
            ingest_batch(&input.workspace, Vec::new()),
            Vec::new(),
        );
        p.skip = Some(why);
        p
    }

    /// The ids among `candidates` whose stored row is an ingest-owned tombstone (DI1.8,
    /// RD2-5): one lookup per candidate, bounded by the change set of an ordinary edit. A
    /// tombstone without `props.owner` (written before DI1.8) is not a candidate: the store
    /// would refuse its reactivation, so it stays hidden as before instead of failing the batch.
    fn tombstoned_among(&self, candidates: &[&NodeId]) -> Result<Vec<NodeId>, IngestError> {
        let mut out = Vec::new();
        for id in candidates {
            if let Some(n) = self.store.get(id, true)?
                && n.lifecycle == kaban_core::Lifecycle::Tombstoned
                && n.props.get("owner").and_then(Value::as_str) == Some(kaban_core::INGEST_OWNER)
            {
                out.push((*id).clone());
            }
        }
        Ok(out)
    }

    fn skip_key(identity: Identity, props: &DocumentProps, input: &PrepareInput) -> SkipKey {
        SkipKey {
            identity,
            pipeline_version: PIPELINE_VERSION,
            extractor_version: props.extractor_version.clone(),
            config_hash: input.config_hash.clone(),
            position_fingerprint: props.position_fingerprint.clone(),
        }
    }

    fn ready(
        &self,
        input: &PrepareInput,
        graph: &PreparedGraph,
        audit: Vec<AuditEntry>,
    ) -> Result<Prepared, IngestError> {
        let props = &graph.document.props;
        if let Some(hash) = &props.content_hash {
            let key = Self::skip_key(Identity::ContentHash(hash.clone()), props, input);
            if should_skip(self.store.as_ref(), &input.source, &key)?
                && self
                    .store
                    .gazetteer_set_fresh(&input.document_id, &input.workspace)?
            {
                return Ok(Self::skipped(input, SkipReason::Unchanged));
            }
        }
        let previous = self.existing(&input.document_id)?;
        if previous.as_ref().is_some_and(|p| p.forgotten && !p.active) && !input.include_forgotten {
            return Ok(Self::skipped(input, SkipReason::Forgotten));
        }
        // Capture the producer's pre-batch source and generation. The store checks both
        // again in the document transaction before accepting any graph or audit writes.
        let source_rev = self
            .store
            .get(&input.document_id, true)?
            .as_ref()
            .map(kaban_core::SourceRev::from);
        let expected_generation = self
            .store
            .gazetteer_generation(&input.document_id, &input.workspace)?;
        let reactivate = self.tombstoned_among(&ExistingDocument::reactivation_candidates(
            previous.as_ref(),
            graph,
        ))?;
        let links = {
            let policy = self.policy();
            resolve_links(
                &policy,
                &input.workspace,
                &props.path,
                graph.links.clone(),
                |id, path| self.target_stored(input, id, path),
            )
        };
        let mut batch = build_batch(
            graph,
            &links,
            previous.as_ref(),
            &reactivate,
            &self.cfg,
            &input.workspace,
        );
        if !self.candidate_only {
            batch
                .gazetteer_claim_sets
                .push(kaban_core::GazetteerClaimSet {
                    document: input.document_id.clone(),
                    source_id: props.source_id.clone(),
                    source_rev,
                    expected_generation,
                    // The run digest binds the enabled/disabled mode and the gated dictionary.
                    config_hash: input.config_hash.clone(),
                });
            if let Some(dictionary) = self
                .dictionary
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                for chunk in graph
                    .sections
                    .iter()
                    .flat_map(|s| &s.chunks)
                    .filter(|c| c.props.stub_hmac.is_none())
                {
                    let mentions = crate::find_mentions(dictionary, &chunk.id, &chunk.props.text)
                        .into_iter()
                        .filter(|m| m.target != graph.document.id)
                        .collect::<Vec<_>>();
                    crate::mentions::append_mentions(&mut batch, &mentions, &props.source_id);
                }
            }
        }
        Ok(Self::prepared(input, props.status, batch, audit))
    }

    fn without_children(
        &self,
        input: &PrepareInput,
        props: &DocumentProps,
        audit: Vec<AuditEntry>,
    ) -> Result<Prepared, IngestError> {
        if let Some(keyed) = &props.blocked_hmac {
            let key = Self::skip_key(Identity::Keyed(keyed.clone()), props, input);
            if should_skip(self.store.as_ref(), &input.source, &key)? {
                return Ok(Self::skipped(input, SkipReason::Unchanged));
            }
        }
        let previous = self.existing(&input.document_id)?;
        let batch = document_only_batch(
            &input.document_id,
            props,
            previous.as_ref(),
            &input.workspace,
        );
        Ok(Self::prepared(input, props.status, batch, audit))
    }

    fn run(&self, input: &PrepareInput, ctx: &ExtractCtx) -> Result<Prepared, IngestError> {
        let mut input = input.clone();
        if let Some(dictionary) = self
            .dictionary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let digest = dictionary.external_fingerprint(&input.document_id);
            input.config_hash = blake3::hash(format!("{}:{digest}", input.config_hash).as_bytes())
                .to_hex()
                .to_string();
        }
        let input = &input;
        let bytes = Self::read(input).map_err(|source| IngestError::Io {
            path: input.path.relative_path().to_owned(),
            source,
        })?;
        ctx.check()?;
        let req = PrepareRequest {
            source: &input.source,
            path: &input.path,
            format: input.format,
            size: input.size,
            mtime: input.mtime,
            bytes: &bytes,
            config_hash: &input.config_hash,
        };
        let prepared = {
            let policy = self.policy();
            prepare_document(
                &policy,
                self.signer.as_ref(),
                ctx,
                &input.workspace,
                &req,
                &self.chunk,
            )?
        };
        let bytes_hmac = self
            .capture
            .as_ref()
            .map(|_| keyed_id(self.signer.as_ref(), "bytes", &bytes));
        drop(bytes);
        ctx.check()?;
        match prepared {
            PreparedDocument::Ready { graph, audit } => {
                // The policy guard above is gone. Capture conveys preparation only, including
                // unchanged/dry-run graphs; source/generation publication checks stay in ready.
                if let (Some(capture), Some(bytes_hmac)) = (&self.capture, &bytes_hmac) {
                    capture(&graph, bytes_hmac);
                }
                self.ready(input, &graph, audit)
            }
            PreparedDocument::Blocked { props, audit } => {
                self.without_children(input, &props, audit)
            }
            PreparedDocument::Failed { props } => self.without_children(input, &props, Vec::new()),
        }
    }
}

impl Preparer for DocumentPreparer {
    fn gazetteer_telemetry(&self) -> Option<kaban_api::GazetteerTelemetry> {
        self.telemetry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn begin_run(
        &self,
        _documents: &std::collections::BTreeSet<NodeId>,
    ) -> Result<String, IngestError> {
        // Only a brief-owned one-use preparer initializes before its producer starts.
        // Ordinary ingestion always builds anew, and no dictionary survives a brief call.
        if self.candidate_only
            && let Some(digest) = self
                .candidate_run_digest
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
        {
            return Ok(digest.clone());
        }
        let mode = self
            .cfg
            .link
            .as_deref()
            .unwrap_or(&self.api.engine().link.default);
        if !matches!(mode, "none" | "gazetteer") {
            return Err(IngestError::Config("link must be none or gazetteer".into()));
        }
        if mode == "none" {
            *self
                .telemetry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            *self
                .dictionary
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            return Ok(if self.cfg.link.is_some() {
                "link:none".into()
            } else {
                String::new()
            });
        }
        let mut dropped = Vec::new();
        let dictionary = self.api.with_current_snapshot(|commit| {
            let snapshot = commit.map(|commit| crate::SnapshotSource {
                commit: commit.to_owned(),
            });
            let cfg = &self.api.engine().gazetteer;
            let mut dictionary = crate::build_dictionary(
                self.store.as_ref(),
                self.api.workspace(),
                snapshot.as_ref(),
                &self.policy(),
                cfg,
            )?;
            let budget = kaban_api::layers::budgets(
                self.api.engine(),
                &kaban_api::layers::RunFlags {
                    symbols: self.cfg.symbols,
                },
            )
            .into_iter()
            .find(|budget| budget.key == "gazetteer.max_automaton_mb")
            .ok_or_else(|| IngestError::Config("gazetteer budget unavailable".into()))?;
            let cap = budget.effective.saturating_mul(1024 * 1024);
            let over = |dictionary: &crate::Dictionary| {
                dictionary.memory_usage as u64 > cap || dictionary.build_peak_delta > cap
            };
            if !budget.cap_lifted
                && over(&dictionary)
                && dictionary.sources.contains(&crate::DictSource::V4Symbols)
            {
                // Drop the first automaton before rebuilding, so the fallback does not
                // measure two retained automatons as its own peak.
                drop(dictionary);
                dropped.push("v4_symbols".to_owned());
                dictionary = crate::build_dictionary(
                    self.store.as_ref(),
                    self.api.workspace(),
                    snapshot.as_ref(),
                    &self.policy(),
                    &kaban_core::config::Gazetteer {
                        v4_symbols: false,
                        ..cfg.clone()
                    },
                )?;
            }
            let skipped = !budget.cap_lifted && over(&dictionary);
            let telemetry = kaban_api::GazetteerTelemetry {
                memory_usage: dictionary.memory_usage as u64,
                build_peak_delta: dictionary.build_peak_delta,
                build_hwm_delta: dictionary.build_hwm_delta,
                dropped: dropped.clone(),
                outcome: if skipped { "skipped_budget" } else { "ok" }.into(),
                budget_lifted_by: match budget.source {
                    kaban_api::layers::BudgetSource::Flag(flag) => Some(flag.into()),
                    kaban_api::layers::BudgetSource::Default
                    | kaban_api::layers::BudgetSource::Config => None,
                },
            };
            *self
                .telemetry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(telemetry);
            Ok::<_, IngestError>((!skipped).then_some(dictionary))
        })?;
        let digest = if dictionary.is_some() {
            "per-document"
        } else {
            "skipped_budget"
        };
        *self
            .dictionary
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dictionary.map(Arc::new);
        let config = serde_json::to_vec(&self.api.engine().gazetteer)
            .map_err(|_| IngestError::Config("gazetteer configuration failed".into()))?;
        let config = blake3::hash(&config);
        let digest = format!(
            "link:gazetteer:{digest}:{config}:symbols:{}",
            self.cfg.symbols
        );
        if self.candidate_only {
            *self
                .candidate_run_digest
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(digest.clone());
        }
        Ok(digest)
    }
    /// Cancellation and the deadline propagate as `ExtractError`; any other failure (an
    /// unreadable file, a store error on a lookup) is recorded on the Document as `failed`
    /// with one of this crate's own messages (D8; no path, no content), so the run goes on.
    fn prepare(&self, input: PrepareInput, ctx: &ExtractCtx) -> Result<Prepared, ExtractError> {
        match self.run(&input, ctx) {
            Ok(p) => Ok(p),
            Err(IngestError::Extract(e)) => Err(e),
            Err(IngestError::Io { .. }) => Ok(Prepared::failed(
                input.seq,
                &input.workspace,
                Some(&input.source),
                &input.document_id,
                "read error",
            )),
            Err(e) => {
                tracing::debug!(error = %e, "document preparation failed");
                Ok(Prepared::failed(
                    input.seq,
                    &input.workspace,
                    Some(&input.source),
                    &input.document_id,
                    "preparation error",
                ))
            }
        }
    }
}
