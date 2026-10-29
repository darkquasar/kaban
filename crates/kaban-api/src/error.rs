//! Service-layer error type. Messages are safe to echo to callers: a rejection names the
//! field and the rule ids, never the offending value.

use kaban_core::StoreError;
use kaban_policy::PolicyError;
use thiserror::Error;

/// Why a service call failed.
#[derive(Debug, Error)]
pub enum ApiError {
    /// The denylist refused the write (or the audit log could not record it: fail-closed).
    #[error(
        "REJECTED by denylist policy; nothing was stored. field={field} rules={}. Remove the sensitive value and retry.",
        rules.join(",")
    )]
    Rejected {
        /// Field that triggered the rejection (`text`, `props`, `all` for the joined view, ...).
        field: String,
        /// Reject-action rule ids (or `audit-unavailable`).
        rules: Vec<String>,
    },
    /// No node with that id.
    #[error("node not found: {0}")]
    NotFound(String),
    /// The request violates a bound or shape constraint.
    #[error("{0}")]
    Invalid(String),
    /// Policy load/compile error.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Store error (other than `NotFound`, which maps to [`ApiError::NotFound`]).
    #[error(transparent)]
    Store(StoreError),
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(id) => Self::NotFound(id),
            other => Self::Store(other),
        }
    }
}
