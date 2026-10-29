//! The synchronous adapter boundary of an ingestion run (DI2.2a, review RD3-3; docs-ingest-spec
//! D8, PR-8): the CLI verb and the MCP tool call [`run_blocking`] from a plain thread or a
//! blocking-pool thread. It builds a private current-thread runtime, runs [`Ingestor::run`] on
//! it and shuts the runtime down in the background on every exit path.
//!
//! Why the shutdown mode matters: after a document timeout the supervisor leaves the
//! non-cooperative extractor running on the blocking pool on purpose (PR-8), but dropping a
//! Tokio runtime waits for its blocking tasks, so the adapter would block past the reported
//! deadline, indefinitely if the parser never returns. `Runtime::shutdown_background` abandons
//! the thread instead, and the run reports how many it abandoned
//! (`IngestReport::timed_out_outstanding`). Those threads keep their worker permits inside the
//! `Ingestor` that started them; a long-lived service runtime that carries the permits across
//! adapter calls is the E3 ingest-lifecycle residual.

use crate::ingestor::Ingestor;
use crate::model::{IngestError, IngestReport};
use crate::walk::WalkConfig;
use std::future::Future;
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

/// Run one ingestion to completion from synchronous code and return its report.
///
/// The private runtime is shut down with `Runtime::shutdown_background` whether the run returns
/// `Ok`, `Err` or unwinds (a drop guard), so a parser abandoned at its deadline never holds the
/// caller. Call it from a plain thread or a blocking-pool thread, never from inside an async
/// task (`block_on` panics there).
///
/// # Errors
/// `Internal` when the runtime cannot start; otherwise [`Ingestor::run`]'s errors unchanged.
pub fn run_blocking(
    ingestor: &Ingestor,
    walk: &WalkConfig,
    cancel: CancellationToken,
) -> Result<IngestReport, IngestError> {
    let runtime = BackgroundShutdown::new()?;
    let outcome = runtime.block_on(ingestor.run(walk, cancel));
    outcome.ok_or(IngestError::Internal("the ingestion runtime is gone"))?
}

/// A current-thread runtime whose drop is `shutdown_background`: no wait for blocking tasks.
struct BackgroundShutdown(Option<Runtime>);

impl BackgroundShutdown {
    fn new() -> Result<Self, IngestError> {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| Self(Some(rt)))
            .map_err(|_| IngestError::Internal("the ingestion runtime could not start"))
    }

    /// `None` only once the runtime was taken for shutdown, which `Drop` alone does.
    fn block_on<F: Future>(&self, fut: F) -> Option<F::Output> {
        self.0.as_ref().map(|rt| rt.block_on(fut))
    }
}

impl Drop for BackgroundShutdown {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}
