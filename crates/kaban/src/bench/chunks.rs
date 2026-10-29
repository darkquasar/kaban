//! `kaban bench --chunks N` (docs-ingest-plan.md DI1.7; docs-ingest-spec D9 "recall latency
//! unchanged within N2 at +50k chunks"): after the §16 note corpus, a synthetic Markdown
//! corpus of `N` chunks is written beside the database and ingested through the real pipeline
//! (`Ingestor` + `DocumentPreparer`: walker, extractor, chunker, gate, graph assembly, one
//! committer transaction per document), so the Document/Section/Chunk nodes and their
//! `PART_OF`/`NEXT` edges reach the store exactly the way `kaban ingest` writes them.
//!
//! ```text
//!   Rng::seeded(SEED ^ CHUNK_SALT)  ==>  ceil(N / 250) files  d0000.md .. dNNNN.md
//!     each: "# Document i" then 250 x ("## Section j" + one 40-word paragraph)
//!     words: the notes' Zipf(1.07) vocabulary (Corpus::term_from), so `recall_*` candidate
//!     sets mix notes and chunks
//!   ==> Ingestor::run(WalkConfig::new(dir)) in the API's workspace (ws1)  ==> report
//! ```
//!
//! One heading section per paragraph keeps the chunker from merging neighbours (D4 merges only
//! inside one `section_path`), so every paragraph is one chunk and the count is exact. Only the
//! `ingest` feature compiles this module; without it `--chunks` above 0 is refused.

use super::corpus::{Corpus, Rng, SEED};
use kaban_api::KabanApi;
use kaban_ingest::{ChunkConfig, DocumentPreparer, IngestConfig, Ingestor, WalkConfig};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Chunks per synthetic document (50,000 chunks = 200 documents).
const CHUNKS_PER_DOCUMENT: usize = 250;
/// Words per paragraph: about 250 characters, well under the 512-token target, so one chunk.
const WORDS_PER_CHUNK: usize = 40;
/// Salt for the chunk corpus stream (distinct from the notes' seed).
const CHUNK_SALT: u64 = 0x5EED_C4A1_1C5E_0000;

/// Write the synthetic corpus under `dir`: `ceil(n / 250)` files, the last one carrying the
/// remainder. Returns the number of files written.
fn write_corpus(dir: &Path, corpus: &Corpus, n: usize) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    let mut rng = Rng::seeded(SEED ^ CHUNK_SALT);
    let documents = n.div_ceil(CHUNKS_PER_DOCUMENT);
    for d in 0..documents {
        let chunks = (n - d * CHUNKS_PER_DOCUMENT).min(CHUNKS_PER_DOCUMENT);
        let mut text = String::with_capacity(chunks * 300);
        let _ = writeln!(text, "# Document {d}\n");
        for c in 0..chunks {
            let _ = writeln!(text, "## Section {c}\n");
            for w in 0..WORDS_PER_CHUNK {
                text.push_str(corpus.term_from(&mut rng));
                text.push(if w + 1 == WORDS_PER_CHUNK { '.' } else { ' ' });
            }
            text.push_str("\n\n");
        }
        std::fs::write(dir.join(format!("d{d:04}.md")), text)?;
    }
    Ok(documents)
}

/// Build and ingest `n` synthetic chunks through the ingestor's commit path; the corpus files
/// are removed afterwards. Returns the `chunks` section of the bench result.
///
/// # Errors
/// File-system, runtime or ingestion errors.
pub(super) fn ingest(
    api: &Arc<KabanApi>,
    corpus: &Corpus,
    dir: &Path,
    n: usize,
) -> anyhow::Result<Value> {
    let t_write = Instant::now();
    let documents = write_corpus(dir, corpus, n)?;
    let write_s = t_write.elapsed().as_secs_f64();

    let cfg = IngestConfig::default();
    let preparer = Arc::new(DocumentPreparer::new(
        api,
        Arc::clone(api.store()),
        Arc::clone(api.signer()),
        ChunkConfig::default(),
        cfg.clone(),
    ));
    let ingestor = Ingestor::new(
        Arc::clone(api),
        Arc::clone(api.store()),
        Arc::clone(api.signer()),
        preparer,
        cfg,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let t_ingest = Instant::now();
    let report = runtime.block_on(ingestor.run(
        &WalkConfig::new(dir.to_path_buf()),
        CancellationToken::new(),
    ))?;
    let ingest_s = t_ingest.elapsed().as_secs_f64();
    let _ = std::fs::remove_dir_all(dir);
    anyhow::ensure!(
        report.failed == 0 && report.blocked == 0 && report.blocked_chunks == 0,
        "the synthetic corpus must ingest clean: {report:?}"
    );
    Ok(json!({
        "requested": n,
        "documents": documents,
        "documents_committed": report.documents,
        "chunks_inserted": report.chunks_upserted,
        "chunks_per_document": CHUNKS_PER_DOCUMENT,
        "words_per_chunk": WORDS_PER_CHUNK,
        "write_s": write_s,
        "ingest_s": ingest_s,
        "chunks_per_s": if ingest_s > 0.0 { (f64::from(report.chunks_upserted) / ingest_s).round() } else { 0.0 },
        "outbox_rows": report.outbox_rows,
        "workers": IngestConfig::default().workers,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test code: failures are the assertion
mod tests {
    use super::*;

    /// The corpus is a pure function of `n`: the same files twice, `ceil(n / 250)` of them,
    /// with the remainder in the last.
    #[test]
    fn the_chunk_corpus_is_deterministic_and_sized_by_n() {
        let base = std::env::temp_dir().join(format!("kaban-bench-chunks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let corpus = Corpus::new(SEED);
        let a = base.join("a");
        let b = base.join("b");
        assert_eq!(write_corpus(&a, &corpus, 501).unwrap(), 3);
        assert_eq!(write_corpus(&b, &corpus, 501).unwrap(), 3);
        for name in ["d0000.md", "d0001.md", "d0002.md"] {
            assert_eq!(
                std::fs::read(a.join(name)).unwrap(),
                std::fs::read(b.join(name)).unwrap(),
                "{name}"
            );
        }
        let last = std::fs::read_to_string(a.join("d0002.md")).unwrap();
        assert_eq!(last.matches("\n## Section ").count(), 1, "the remainder");
        let first = std::fs::read_to_string(a.join("d0000.md")).unwrap();
        assert_eq!(first.matches("\n## Section ").count(), CHUNKS_PER_DOCUMENT);
        let _ = std::fs::remove_dir_all(&base);
    }
}
