//! YAML policy model and defaults: the error type, the on-disk document (`PolicyFile` and its
//! parts) and the curated builtin rules. The compiled scanner lives in `super::engine`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Why a policy file could not be loaded or compiled. Never contains policy term values.
#[derive(Debug, Error)]
pub enum PolicyError {
    /// The policy file could not be read.
    #[error("cannot read policy file: {0}")]
    Io(#[from] std::io::Error),
    /// The policy file is not valid YAML for the policy schema.
    #[error("invalid policy yaml: {0}")]
    Yaml(#[from] yaml_serde::Error),
    /// A denylist word/phrase normalizes to nothing.
    #[error("denylist term #{0} is empty after normalization")]
    EmptyTerm(usize),
    /// A rule regex does not compile.
    #[error("rule {rule}: {source}")]
    Regex {
        /// Rule id.
        rule: String,
        /// Compile error.
        #[source]
        source: regex::Error,
    },
    /// A rule names a capture group the regex does not have.
    #[error("rule {rule}: secret_group {group} exceeds capture groups")]
    SecretGroup {
        /// Rule id.
        rule: String,
        /// Requested group.
        group: usize,
    },
    /// The literal/keyword automaton could not be built.
    #[error("cannot build matcher: {0}")]
    Automaton(#[from] aho_corasick::BuildError),
    /// An allowlist regex is not anchored at both ends (`^...$` or `\A...\z`), so it could
    /// whitelist any secret containing the pattern (core-spec S7).
    #[error(
        "allow regex #{index} for rules [{}] must be anchored at both ends (^...$)",
        rule_ids.join(",")
    )]
    UnanchoredAllow {
        /// Rule ids the allowlist entry applies to (empty = all regex rules).
        rule_ids: Vec<String>,
        /// Index of the offending regex inside the entry (the pattern itself is never echoed).
        index: usize,
    },
}

/// `^...$` or `\A...\z` on both ends (an optional leading `(?flags)` group is allowed).
pub(super) fn is_anchored(pattern: &str) -> bool {
    let body = pattern
        .strip_prefix("(?")
        .and_then(|rest| rest.find(')').map(|i| &rest[i + 1..]))
        .unwrap_or(pattern);
    let start_ok = body.starts_with('^') || body.starts_with("\\A");
    let end_ok = (body.ends_with('$') && !body.ends_with("\\$")) || body.ends_with("\\z");
    start_ok && end_ok
}

/// What to do on a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Refuse the whole write; nothing is stored.
    #[default]
    Reject,
    /// Replace the span with the mask and continue.
    Redact,
}

/// Policy-wide defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Action for terms/rules that do not set one.
    #[serde(default)]
    pub action: Action,
    /// Mask template; `{rule}` is replaced with the rule id.
    #[serde(default = "default_mask")]
    pub mask: String,
    /// Re-scan stored text on the way out (policy may have grown).
    #[serde(default = "default_true")]
    pub scan_on_read: bool,
    /// Maximum accepted input length per string field (chars). Longer inputs are rejected.
    #[serde(default = "default_max_len")]
    pub max_field_chars: usize,
}
fn default_mask() -> String {
    "[REDACTED:{rule}]".into()
}
fn default_true() -> bool {
    true
}
fn default_max_len() -> usize {
    20_000
}
impl Default for Defaults {
    fn default() -> Self {
        Self {
            action: Action::Reject,
            mask: default_mask(),
            scan_on_read: true,
            max_field_chars: default_max_len(),
        }
    }
}

/// A denylist word or phrase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Term {
    /// The literal (matched on normalized views).
    pub term: String,
    /// Per-term action override.
    #[serde(default)]
    pub action: Option<Action>,
    /// Stable id echoed in verdicts; defaults to `literal-<n>`.
    #[serde(default)]
    pub id: Option<String>,
    /// Match only at word boundaries of the normalized view (`ace` does not match `place`).
    #[serde(default)]
    pub word: bool,
}

/// What a rule detects; decides whether `defaults.action` / per-rule `action` apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuleClass {
    /// A credential shape (key id, token, private key, assignment of a secret). Hits always
    /// reject, regardless of `defaults.action` or the rule's own `action` (core-spec S1).
    Credential,
    /// A denylist word or phrase (user terms); follows the term's action.
    Literal,
    /// A heuristic or project-specific pattern; follows `action` / `defaults.action`.
    #[default]
    Generic,
}

/// What a hit of this rule does to the whole document being ingested (docs-ingest-spec D6):
/// `skip_file` records the file as `Document{status: blocked}` with no chunks instead of
/// stubbing or masking the chunk that matched. Absent means per-chunk handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDocument {
    /// Skip the whole file.
    SkipFile,
}

/// A regex rule (gitleaks-style).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Stable id echoed in verdicts.
    pub id: String,
    /// Rule class; `credential` rules always reject.
    #[serde(default)]
    pub class: RuleClass,
    /// Human description.
    #[serde(default)]
    pub description: String,
    /// The regex, run on the NFKC/ignorable-stripped view of the text (offsets map back to raw).
    pub regex: String,
    /// Capture group holding the secret (0 = whole match).
    #[serde(default)]
    pub secret_group: usize,
    /// Case-insensitive prefilter keywords; empty = rule always runs.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Minimum Shannon entropy (bits/char) of the secret group, if set.
    #[serde(default)]
    pub entropy: Option<f64>,
    /// Minimum secret length (chars), if set.
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Per-rule action override.
    #[serde(default)]
    pub action: Option<Action>,
    /// Document-level consequence of a hit during ingestion (D6); policies written before this
    /// field load unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_document: Option<OnDocument>,
}

/// An allowlist entry (applies to regex rules only, never to literals).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Allow {
    /// Human description.
    #[serde(default)]
    pub description: String,
    /// Rule ids this allowlist applies to. Empty = all regex rules (never literals).
    #[serde(default)]
    pub rule_ids: Vec<String>,
    /// Anchored regexes tested against the extracted secret.
    #[serde(default)]
    pub regexes: Vec<String>,
    /// Exact (case-insensitive) secret values to allow.
    #[serde(default)]
    pub exact: Vec<String>,
}

/// How the user policy composes with the builtin rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extend {
    /// Include the builtin rules (default true).
    #[serde(default = "default_true")]
    pub use_builtin: bool,
    /// Builtin or user rule ids to disable.
    #[serde(default)]
    pub disabled_rules: Vec<String>,
}
impl Default for Extend {
    fn default() -> Self {
        Self {
            use_builtin: true,
            disabled_rules: vec![],
        }
    }
}

/// The on-disk policy document.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    /// Document version.
    #[serde(default)]
    pub version: u32,
    /// Policy-wide defaults.
    #[serde(default)]
    pub defaults: Defaults,
    /// Builtin composition.
    #[serde(default)]
    pub extend: Extend,
    /// Denylist words.
    #[serde(default)]
    pub words: Vec<Term>,
    /// Denylist phrases.
    #[serde(default)]
    pub phrases: Vec<Term>,
    /// Regex rules.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Allowlists.
    #[serde(default)]
    pub allow: Vec<Allow>,
}

/// Curated builtin rules (gitleaks-style shapes). Keyword prefilters are case-insensitive.
pub const BUILTIN_RULES_YAML: &str = r#"
rules:
  - id: aws-access-key-id
    class: credential
    description: AWS access key id
    regex: '\b((?:AKIA|ASIA)[0-9A-Z]{16})\b'
    secret_group: 1
    keywords: [AKIA, ASIA]
  - id: private-key-block
    class: credential
    description: PEM private key header
    regex: '-----BEGIN (?:[A-Z]+ )*PRIVATE KEY(?: BLOCK)?-----'
    keywords: ["PRIVATE KEY"]
    on_document: skip_file
  - id: github-token
    class: credential
    description: GitHub token
    regex: '\b((?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b'
    secret_group: 1
    keywords: [ghp_, gho_, ghu_, ghs_, ghr_, github_pat_]
  - id: gitlab-token
    class: credential
    regex: '\b(glpat-[A-Za-z0-9_-]{20,})\b'
    secret_group: 1
    keywords: [glpat-]
  - id: openai-api-key
    class: credential
    description: OpenAI style secret key
    regex: '\b(sk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,})\b'
    secret_group: 1
    keywords: [sk-]
  - id: stripe-key
    class: credential
    regex: '\b((?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,})\b'
    secret_group: 1
    keywords: [sk_live_, sk_test_, rk_live_, rk_test_]
  - id: google-api-key
    class: credential
    regex: '\b(AIza[0-9A-Za-z_-]{35})\b'
    secret_group: 1
    keywords: [AIza]
  - id: jwt
    class: credential
    description: JSON Web Token
    regex: '\b(eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})\b'
    secret_group: 1
    keywords: [eyJ]
  - id: slack-token
    class: credential
    regex: '\b(xox[baprs]-[A-Za-z0-9-]{10,})\b'
    secret_group: 1
    keywords: [xoxb-, xoxa-, xoxp-, xoxr-, xoxs-]
  - id: bearer-token
    class: credential
    regex: '(?i)\bbearer\s+([A-Za-z0-9._~+/=-]{20,})'
    secret_group: 1
    keywords: [bearer]
  - id: http-basic-auth
    class: credential
    description: HTTP Basic authorization value (base64 user:password)
    regex: '(?i)\bbasic\s+([A-Za-z0-9+/]{16,}={0,2})'
    secret_group: 1
    entropy: 3.5
    min_length: 16
    keywords: [basic]
  - id: url-with-credentials
    class: credential
    description: URL with embedded user:password
    regex: '(?i)\b[a-z][a-z0-9+.-]*://[^/\s:@]+:([^@\s/]{3,})@'
    secret_group: 1
    keywords: ["://"]
  - id: generic-credential-assignment
    class: credential
    description: password/secret/token/api key (any prefix/suffix, JSON, env, YAML or markdown style) assigned a literal value
    regex: '(?i)[a-z0-9_.-]*(?:password|passwd|passphrase|pwd|pw|secret|api[_ -]?key|apikey|api[_ -]?secret|access[_-]?token|auth[_-]?token|refresh[_-]?token|session[_-]?token|client[_-]?secret|private[_-]?key|credentials?)[a-z0-9_]*["''`*]*\s*(?::=|=>|[:=])\s*["''`*]*([^\s"''`,;|]{6,})'
    secret_group: 1
    keywords: [password, passwd, passphrase, pwd, pw, secret, api_key, apikey, api-key, 'api key', apisecret, api_secret, api-secret, 'api secret', token, private_key, private-key, privatekey, credential]
    on_document: skip_file
  - id: credential-prose-assignment
    class: credential
    description: credential key followed by is/was or a table cell, with a secret-looking value (digit or symbol); pw/pwd excluded (shell prose)
    regex: '(?i)[a-z0-9_.-]*(?:password|passwd|passphrase|secret|api[_ -]?key|apikey|api[_ -]?secret|access[_-]?token|auth[_-]?token|refresh[_-]?token|session[_-]?token|client[_-]?secret|private[_-]?key|credentials?)[a-z0-9_]*["''`*]*(?:\s+(?:is|was)\s+|\s*\|\s*)["''`*]*([^\s"''`,;|]*[0-9!#$%&*+/=?@^_~\\-][^\s"''`,;|]*)'
    secret_group: 1
    min_length: 6
    keywords: [password, passwd, passphrase, secret, api_key, apikey, api-key, 'api key', apisecret, api_secret, api-secret, 'api secret', token, private_key, private-key, privatekey, credential]
  - id: weak-credential-assignment
    class: credential
    description: bare token/auth key assigned a secret-looking value (digit or symbol)
    regex: '(?i)[a-z0-9_.-]*(?:token|auth)s?["''`*]*(?:\s*(?::=|=>|[:=])\s*|\s+(?:is|was)\s+|\s*\|\s*)["''`*]*([^\s"''`,;|]*[0-9!#$%&*+/=?@^_~\\-][^\s"''`,;|]*)'
    secret_group: 1
    min_length: 6
    keywords: [token, auth]
  - id: high-entropy-quoted
    description: quoted high-entropy base64/hex-like token (>=24 chars)
    regex: '["''`]([A-Za-z0-9+/_=-]{24,})["''`]'
    secret_group: 1
    entropy: 4.3
    min_length: 24
allow:
  - description: documented example / test keys (exact or anchored only)
    exact: [AKIAIOSFODNN7EXAMPLE, "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"]
    rule_ids: [aws-access-key-id, github-token]
  - description: obvious placeholders for the assignment rules (angle brackets must wrap a plain name)
    regexes: ['^(?i)(?:<[a-z][a-z0-9 _-]{0,40}>|\$\{[^}]+\}|\$[A-Z_]+|changeme|placeholder|example|xxx+|\*\*\*+|redacted|null|none|true|false)$']
    rule_ids: [generic-credential-assignment, credential-prose-assignment, weak-credential-assignment, credential-key]
"#;
