//! kaban document ingestion (docs-ingest stream): index local documents into
//! `Document -> Section -> Chunk` nodes of the project store, deterministically, incrementally
//! and denylist-gated, with no network and no LLM.
//!
//! The pipeline (docs-ingest-plan.md §2) is a gitignore-aware walker, per-format extractors
//! behind additive cargo features, a section/chunker, the denylist gate and a single committer
//! that writes one `GraphStore::apply_batch` transaction per document in sorted path order
//! (core-spec F16, N4, N5). Every path, title, frontmatter value, chunk text and error message
//! is scanned before it is hashed for identity, logged, indexed or persisted (core-spec S1).
//!
//! DI0 shipped the crate scaffold, the walker, identities and the worker pool; DI1 adds the
//! document model, the Markdown and text extractors ([`extract`]), the chunker, the gate and
//! the graph assembly with its per-document batch ([`graph`], [`DocumentPreparer`]). DI2.0
//! shares one run lock per `(store, workspace)` across every `Ingestor` of a process
//! (`service`), prunes `.git` at the directory level and pages the document listing through
//! the store role. DI2.2a adds the synchronous adapter boundary ([`run_blocking`]: a private
//! runtime shut down without waiting for a parser abandoned at its deadline) and the
//! content-free process panic hook ([`panic_hook`]).

// Modules are private; the supported surface is the flat re-export list below
// (`proj-pub-use-reexport`, as in the sibling crates).
mod cancel;
pub mod chunk;
mod commit;
pub mod extract;
pub mod gate;
pub mod graph;
mod hash;
mod ids;
mod ingestor;
pub mod jobs;
mod mentions;
mod model;
pub mod panic_hook;
pub mod prepare;
mod runtime;
mod service;
pub mod status;
mod walk;

pub use cancel::{ExtractCtx, ExtractError};
pub use chunk::{
    ChunkConfig, ChunkConfigBuilder, ChunkConfigError, ChunkDraft, ChunkMap, Chunked,
    DEFAULT_SLICE_BYTES, Segment, Stream, StreamBlock, chunk, chunk_with_stream, estimate_tokens,
    slices_for,
};
pub use commit::Prepared;
#[cfg(feature = "md")]
pub use extract::MarkdownExtractor;
pub use extract::{
    AnchorKind, Block, Extracted, Extractor, LinkDraft, SectionDraft, TagDraft, TextExtractor,
    detect_lang, extractor_for,
};
pub use gate::{
    Admission, AdmittedFrontmatter, DerivedContext, GateDecision, Gated, GatedChunk, MetaDecision,
    SanitizedMeta, StreamCoverage, admit_frontmatter, gate_document, gate_heading_path,
    gate_links_and_tags, gate_metadata, stream_text,
};
pub use graph::{
    AssembleInput, Assembled, EDGE_OWNER, ExistingDocument, FORGOTTEN, ForgottenDocument,
    LinkTarget, ResolvedLink, assemble, build_batch, document_only_batch, forget_document,
    forget_document_batch, ingest_batch, reconcile_failed_batch, resolve_links,
};
pub use hash::{
    BlockedDocument, GatedPath, Identity, SkipKey, blocked_path_document, config_hash,
    content_hash, gate_path, keyed_id, keyed_path_id, position_fingerprint, should_skip,
    should_skip_blocked,
};
pub use ids::{
    PIPELINE_VERSION, blocked_document_id, chunk_id, document_id, document_version_id, section_id,
    derived_id, tag_id,
};
pub use ingestor::{IngestConfig, Ingestor, PoolMetrics, PrepareInput, Preparer};
pub use mentions::{
    DictSource, Dictionary, Mention, SnapshotSource, build_dictionary, dictionary_from_terms,
    find_mentions, mention_edge_ops,
};
pub use model::{
    ChunkKind, ChunkNode, ChunkProps, DocStatus, DocumentNode, DocumentProps, Format, IngestError,
    IngestReport, PreparedGraph, SectionKind, SectionNode, SectionProps, SourceId, TOKEN_ESTIMATOR,
    TokenEstimator,
};
pub use prepare::{DocumentPreparer, PrepareRequest, PreparedDocument, prepare_document};
pub use runtime::run_blocking;
pub use status::{documents_resource, index_status, record_last_run};
pub use walk::{
    Candidate, CheckedFile, DEFAULT_DENY_GLOBS, DEFAULT_MAX_FILE_BYTES, Skip, WalkConfig, walk,
};
