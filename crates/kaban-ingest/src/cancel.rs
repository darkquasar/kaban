//! Cooperative cancellation for extractors (docs-ingest-plan.md DI0.3; PR-8; D-23): every
//! extractor, chunker and gate stage receives an [`ExtractCtx`] and calls [`ExtractCtx::check`]
//! between units of work (pdf per page, markdown per 64 KiB of events, sheets per sheet, docx
//! per paragraph block, the gate per window), so a cooperative parser exits within one stage of
//! its deadline or of a cancellation.

use crate::model::SourceId;
use std::sync::Arc;
use thiserror::Error;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Why an extraction stage stopped early. Messages never carry document content: the
/// `Display` of every variant is a fixed text, and it is what the ingestor records as the
/// failed Document's `error` (D8).
#[derive(Debug, Clone, Error)]
#[non_exhaustive] // extractor failure variants are added per format (DI2.1 `Format`)
pub enum ExtractError {
    /// The run (or this document) was cancelled.
    #[error("extraction cancelled")]
    Cancelled,
    /// The per-document deadline passed.
    #[error("extraction deadline exceeded")]
    DeadlineExceeded,
    /// The parser rejected the bytes (a corrupted or unsupported file, D8). `class` is the
    /// extractor's fixed message; the parser's own error stays behind `source()` for a
    /// debugging chain (`err-source-chain`) and is never persisted.
    #[error("{class}")]
    Format {
        /// Fixed failure class (no document content).
        class: &'static str,
        /// The parser's error.
        #[source]
        source: Arc<dyn std::error::Error + Send + Sync + 'static>,
    },
}

/// Equality by variant and, for `Format`, by class: the wrapped parser error is debugging
/// context, not identity.
impl PartialEq for ExtractError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Cancelled, Self::Cancelled)
            | (Self::DeadlineExceeded, Self::DeadlineExceeded) => true,
            (Self::Format { class: a, .. }, Self::Format { class: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl Eq for ExtractError {}

/// Cancellation handle passed to every stage of one document: the document's child token, its
/// deadline and its source identity.
#[derive(Debug, Clone)]
pub struct ExtractCtx {
    cancel: CancellationToken,
    deadline: Instant,
    /// The document being extracted.
    pub source: SourceId,
}

impl ExtractCtx {
    /// A context for one document.
    pub fn new(cancel: CancellationToken, deadline: Instant, source: SourceId) -> Self {
        Self {
            cancel,
            deadline,
            source,
        }
    }

    /// Stop here if the document was cancelled or its deadline passed. Cancellation is
    /// reported first: a cancelled run does not pretend a document timed out.
    ///
    /// # Errors
    /// `Cancelled` or `DeadlineExceeded`.
    pub fn check(&self) -> Result<(), ExtractError> {
        if self.cancel.is_cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(ExtractError::DeadlineExceeded);
        }
        Ok(())
    }

    /// The deadline this document must finish by.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}
