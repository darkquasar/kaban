//! The crate's closed error enum (stage1-plan CG3.1; G1-S8 "fixed error classes never echo a
//! value").
//!
//! `Display` prints the class only: no gated value, no decoded byte and no file content ever
//! appears in an error text (`obs-no-sensitive-data`).

use thiserror::Error;

/// Why `kaban-index` could not turn an `index.scip` into gated values, or refused the run.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum IndexError {
    /// The bytes are not a protobuf `scip.Index` (truncated, not SCIP, a wrong message). The
    /// protobuf error text is dropped: it can quote the offending bytes.
    #[error("index decode failed")]
    Decode,
    /// Committed Rust source could not be bounded, read, parsed or classified. No source
    /// or parser diagnostic is retained; a partial test classification must not publish.
    #[error("committed Rust source classification refused")]
    SourceClassification,
}

impl IndexError {
    /// The process exit code of G1-F7/§7 for this class: a failed indexer output, 1. (A policy
    /// hit is no longer an error class: a meta hit is masked and counted, DF-3.)
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Decode | Self::SourceClassification => 1,
        }
    }
}
