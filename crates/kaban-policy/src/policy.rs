//! Forbidden-content gate. Runs BEFORE anything is hashed, logged, indexed or persisted.
//! Lexical and deterministic only: Aho-Corasick literals/phrases over casefolded views (NFKD,
//! confusables skeleton, zero-width/combining marks stripped, separators collapsed or removed),
//! keyword-gated regex rules with optional entropy threshold on the NFKC/ignorable-stripped
//! view (core-spec S2), and per-rule allowlists. Hits carry raw byte offsets on char boundaries.
//! Rule model borrowed from gitleaks (`id`/`regex`/`secret_group`/`keywords`/`entropy`/`allowlists`)
//! and detect-secrets (entropy thresholds). Verdicts never contain the matched value.

// Facade over the policy children (`proj-mod-by-feature`, review RB-2 / MT-1): the YAML model
// in `config`, the compiled scanner in `engine`, the unit and property tests in `tests`. The
// supported surface is the re-export list below, consumed unchanged by `lib.rs`.
mod config;
mod engine;
// test code: failures are the assertion
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

pub use config::{
    Action, Allow, BUILTIN_RULES_YAML, Defaults, Extend, OnDocument, PolicyError, PolicyFile, Rule,
    RuleClass, Term,
};
pub use engine::{Hit, Policy, Verdict, is_credential_key, sha256_hex, shannon_entropy};
