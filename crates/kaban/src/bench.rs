//! `kaban bench`: the core-plan §16 benchmark (resolves SR-9; binding for N1-N3). One run builds
//! the §16 corpus on a fresh database through the full service path (`KabanApi`: denylist gate,
//! audit, dedup) and times the §16 operation set; `../specs/kaban/archive/benchmarks/kaban2/scripts/bench/bench_runs.py` repeats it on fresh
//! databases, measures N1 (`initialize` cold/warm through the stdio client) and aggregates.
//!
//! Every operation is labelled with the `path` it measures: `api` (the full service path) or
//! `store` (the `GraphStore` call alone, reported for comparison). Operations the M2 surface
//! cannot run yet are emitted with `status: not_applicable` and the task that owns them (MD-21).
//!
//! ```text
//!   corpus (xorshift seed 0x9E3779B97F4A7C15, deterministic)
//!   +-----------------------------------------------------------------------------+
//!   | 5,000-term Zipf(1.07) vocabulary; 20% of tokens are identifiers             |
//!   |   (Err%04x, src/<a>/<b>.rs); kinds Fact 40 / Failure 20 / Decision 15       |
//!   |   (1-3 path_scope globs, 70% accepted with an event, 10% a CodeRef through  |
//!   |   `refs`, ABOUT) / Convention 5 / Idea+Insight 20; lifecycle 85/10/5;       |
//!   |   workspaces ws1..ws5 80/5/5/5/5; 3 edges per node inside its workspace     |
//!   +-----------------------------------------------------------------------------+
//!        | remember/link/forget (api)         | transition (store: accept is C3.2)
//!        v                                    v
//!   ops: remember, link, get_by_id (store), traverse_depth2, recall_and,
//!        recall_or_fallback, recall_filtered, denylist_scan_short/long (api);
//!        recall_ref, recall_merged, why_one_path -> not_applicable (C3.5, C6.2, C3.4)
//!   --chunks N (feature `ingest`, DI1.7): N synthetic Document/Section/Chunk nodes ingested
//!        through the real pipeline after the notes, plus `recall_chunks`; 0 = today's bench
//!   bench cg gen|run (feature `bench`, stage1-plan §18, CG7.3): the code-graph snapshot bench
//!        in `cg.rs`, a separate corpus and result document (`bench_schema: 3`)
//!   bench why-paired --before <0003 copy> --after <migrated copy> (EN-N7, engine-plan E2.7):
//!        the `why` path prefilter against the scan, paired rounds in one process
//!        (`why_paired.rs`, `bench_schema: 4`, informational)
//!   bench recall --measure <layer set> (EN-N3, engine-plan E3.12 RP-13; feature `ingest`):
//!        one `LayerMeasurement` per fresh process (`layers.rs`)
//! ```

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // benchmark arithmetic on small counts/durations

#[cfg(feature = "bench")]
pub(crate) mod cg;
#[cfg(feature = "ingest")]
mod chunks;
mod corpus;
#[cfg(feature = "ingest")]
mod layers;
#[allow(dead_code)] // E3.5 early pure metric seam; CLI consumer is a separately admitted slice.
pub(crate) mod recall_classes;
pub(crate) mod recall_runtime;
mod report;
mod runner;
pub(crate) mod why_paired;

pub(crate) use runner::{Shape, run};
