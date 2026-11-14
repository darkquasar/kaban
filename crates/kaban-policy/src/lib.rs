//! kaban denylist policy and audit log.
//!
//! [`Policy`] is the forbidden-content gate: lexical and deterministic scanning (normalized
//! Aho-Corasick literals, keyword-gated regex rules with optional entropy thresholds, per-rule
//! allowlists) that runs before anything is hashed, logged, indexed or persisted. Verdicts never
//! contain the matched value. [`KeySigner`] owns the per-database HMAC key behind the keyed
//! identities of gated material (rule ids and keyed hashes only); [`AuditExport`] is the
//! optional JSONL writer.

// Modules are private; the supported surface is the flat re-export list below
// (`proj-pub-use-reexport`, review R0-7).
mod audit;
mod confusables;
pub mod contain;
mod normalize;
mod policy;

pub use audit::{
    AuditError, AuditExport, AuditSigner, KEY_LEN, KeySigner, key_path, open_or_create_key,
};
pub use confusables::skeleton;
pub use contain::{ContainError, open_contained, read_contained};
pub use normalize::{NormalizedText, norm_term, norm_with_offsets};
pub use policy::{
    Action, Allow, BUILTIN_RULES_YAML, Defaults, Extend, Hit, OnDocument, Policy, PolicyError,
    PolicyFile, Rule, RuleClass, Term, Verdict, is_credential_key, sha256_hex, shannon_entropy,
};
