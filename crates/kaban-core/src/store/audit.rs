//! [`AuditStore`]: the audit chain role trait.

use crate::audit::{AuditEntry, AuditVerification};
use crate::error::Result;

/// The append-only HMAC audit chain (core-spec S3) outside the gated write transactions:
/// rejections and read-path entries, and the whole-chain verification. Used by the policy
/// gates and `verify-audit`.
pub trait AuditStore: Send + Sync {
    /// Append audit rows in their own transaction (rejections have no write to ride on).
    ///
    /// # Errors
    /// Backend errors; the caller must then fail closed.
    fn audit_append(&self, entries: &[AuditEntry]) -> Result<()>;
    /// Append read-path audit rows, at most one per `(node_id, policy_version)`; entries without
    /// a node id are always appended.
    ///
    /// # Errors
    /// Backend errors; the caller returns masked content and logs, never hides.
    fn audit_append_read(&self, entries: &[AuditEntry]) -> Result<()>;
    /// Recompute the whole audit chain.
    ///
    /// # Errors
    /// Backend errors.
    fn audit_verify(&self) -> Result<AuditVerification>;
}
