//! Compiled scanner: `Policy` (literal automata, keyword-gated regex rules, allowlists), the
//! `Hit`/`Verdict` output types and the scanner helpers (word boundaries, masking, entropy,
//! hashing). The YAML model it compiles from lives in `super::config`.

use super::config::{
    Action, BUILTIN_RULES_YAML, PolicyError, PolicyFile, Rule, RuleClass, is_anchored,
};
use crate::normalize::{Views, norm_term};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

/// One denylist hit. Never carries the matched value.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    /// Rule or term id.
    pub rule: String,
    /// Action the hit demands.
    pub action: Action,
    /// Byte offset (start) into the ORIGINAL string.
    pub start: usize,
    /// Byte offset (end, exclusive) into the ORIGINAL string.
    pub end: usize,
}

/// Result of scanning one string.
#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    /// At least one reject-action hit (or the input was too long).
    pub rejected: bool,
    /// All hits, sorted by start offset.
    pub hits: Vec<Hit>,
    /// Input with redact-action spans masked (empty when rejected or clean).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub redacted: String,
    /// Reject-action rule ids (safe to echo to the caller).
    pub reject_rules: Vec<String>,
}

impl Verdict {
    /// No hit, nothing rejected, nothing to mask.
    #[must_use]
    pub fn clean() -> Self {
        Self {
            rejected: false,
            hits: Vec::new(),
            redacted: String::new(),
            reject_rules: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct LiteralEntry {
    id: String,
    action: Action,
    word: bool,
}
#[derive(Debug)]
struct CompiledRule {
    rule: Rule,
    re: Regex,
    action: Action,
}
#[derive(Debug)]
struct CompiledAllow {
    rule_ids: Vec<String>,
    res: Vec<Regex>,
    exact: Vec<String>,
}

/// A compiled denylist policy.
#[derive(Debug)]
pub struct Policy {
    /// The effective policy document (user file + builtin rules).
    pub file: PolicyFile,
    /// sha256 of the policy file bytes (`builtin` for the empty policy).
    pub version_hash: String,
    effective_hash: String,
    lit_sep: Option<AhoCorasick>,
    lit_nosep: Option<AhoCorasick>,
    literal_meta: Vec<LiteralEntry>,
    keyword_ac: Option<AhoCorasick>,
    keyword_to_rules: Vec<Vec<usize>>,
    rules: Vec<CompiledRule>,
    allows: Vec<CompiledAllow>,
    mask: String,
    /// [`Policy::disabled`]: every scan answers clean without looking.
    disabled: bool,
}

impl Policy {
    /// Read, parse and compile a policy file; `version_hash` is the sha256 of its bytes.
    ///
    /// # Errors
    /// `Io` when the file cannot be read, `Yaml` when it does not parse, or any compile error
    /// from [`Policy::from_file`].
    pub fn load(path: &Path) -> Result<Self, PolicyError> {
        let bytes = std::fs::read(path)?;
        let file = Self::parse(&bytes)?;
        let hash = sha256_hex(&bytes);
        Self::from_file(file, hash)
    }

    /// Parse policy YAML bytes. The only place the YAML crate is used.
    ///
    /// # Errors
    /// `Yaml` when the bytes are not a valid policy document.
    pub fn parse(bytes: &[u8]) -> Result<PolicyFile, PolicyError> {
        Ok(yaml_serde::from_slice(bytes)?)
    }

    /// The builtin-only policy (no user terms or rules).
    pub fn empty() -> Self {
        // Invariant: BUILTIN_RULES_YAML is a compile-time constant covered by the unit tests;
        // failing to compile it is a bug, not a runtime condition.
        #[allow(clippy::expect_used)]
        Self::from_file(PolicyFile::default(), "builtin".into()).expect("builtin policy compiles")
    }

    /// The policy that scans nothing: every verdict is clean, `effective_hash` is `off`. For a
    /// caller whose output stays on this machine and who turned content scanning off
    /// (kaban-index's `index.policy_scan`, DF-3).
    #[must_use]
    pub fn disabled() -> Self {
        let mut policy = Self::empty();
        policy.disabled = true;
        policy.effective_hash = "off".into();
        policy
    }

    /// Compile a parsed policy document (merging builtin rules when `extend.use_builtin`).
    ///
    /// # Errors
    /// `EmptyTerm`, `Regex`, `SecretGroup` or `Automaton`.
    pub fn from_file(mut file: PolicyFile, version_hash: String) -> Result<Self, PolicyError> {
        if file.extend.use_builtin {
            let builtin = Self::parse(BUILTIN_RULES_YAML.as_bytes())?;
            let user_ids: Vec<String> = file.rules.iter().map(|r| r.id.clone()).collect();
            for r in builtin.rules {
                if !user_ids.contains(&r.id) && !file.extend.disabled_rules.contains(&r.id) {
                    file.rules.push(r);
                }
            }
            file.allow.extend(builtin.allow);
        }
        // literals (words + phrases), matched on normalized views
        let mut literal_meta = Vec::new();
        let mut pats_sep = Vec::new();
        let mut pats_nosep = Vec::new();
        for (i, t) in file.words.iter().chain(file.phrases.iter()).enumerate() {
            let a = norm_term(&t.term, true);
            let b = norm_term(&t.term, false);
            if b.is_empty() {
                return Err(PolicyError::EmptyTerm(i));
            }
            pats_sep.push(a);
            pats_nosep.push(b);
            literal_meta.push(LiteralEntry {
                id: t.id.clone().unwrap_or_else(|| format!("literal-{i}")),
                action: t.action.unwrap_or(file.defaults.action),
                word: t.word,
            });
        }
        let build = |pats: &[String]| -> Result<Option<AhoCorasick>, PolicyError> {
            if pats.is_empty() {
                return Ok(None);
            }
            Ok(Some(
                AhoCorasickBuilder::new()
                    .match_kind(MatchKind::Standard)
                    .build(pats)?,
            ))
        };
        let lit_sep = build(&pats_sep)?;
        let lit_nosep = build(&pats_nosep)?;

        // regex rules + keyword prefilter
        let mut rules = Vec::new();
        let mut kw_map: HashMap<String, Vec<usize>> = HashMap::new();
        for r in &file.rules {
            if file.extend.disabled_rules.contains(&r.id) {
                continue;
            }
            let re = Regex::new(&r.regex).map_err(|source| PolicyError::Regex {
                rule: r.id.clone(),
                source,
            })?;
            if r.secret_group > re.captures_len().saturating_sub(1) {
                return Err(PolicyError::SecretGroup {
                    rule: r.id.clone(),
                    group: r.secret_group,
                });
            }
            let idx = rules.len();
            for k in &r.keywords {
                kw_map.entry(k.to_lowercase()).or_default().push(idx);
            }
            let action = match r.class {
                RuleClass::Credential => Action::Reject,
                RuleClass::Literal | RuleClass::Generic => r.action.unwrap_or(file.defaults.action),
            };
            rules.push(CompiledRule {
                action,
                re,
                rule: r.clone(),
            });
        }
        let mut kw_pats = Vec::new();
        let mut keyword_to_rules = Vec::new();
        for (k, idxs) in kw_map {
            kw_pats.push(k);
            keyword_to_rules.push(idxs);
        }
        let keyword_ac = if kw_pats.is_empty() {
            None
        } else {
            Some(
                AhoCorasickBuilder::new()
                    .ascii_case_insensitive(true)
                    .build(&kw_pats)?,
            )
        };
        let allows = file
            .allow
            .iter()
            .map(|a| {
                Ok(CompiledAllow {
                    rule_ids: a.rule_ids.clone(),
                    res: a
                        .regexes
                        .iter()
                        .enumerate()
                        .map(|(index, s)| {
                            if !is_anchored(s) {
                                return Err(PolicyError::UnanchoredAllow {
                                    rule_ids: a.rule_ids.clone(),
                                    index,
                                });
                            }
                            Regex::new(s).map_err(|source| PolicyError::Regex {
                                rule: "allow".into(),
                                source,
                            })
                        })
                        .collect::<Result<Vec<_>, PolicyError>>()?,
                    exact: a.exact.iter().map(|s| s.to_lowercase()).collect(),
                })
            })
            .collect::<Result<Vec<_>, PolicyError>>()?;
        let mask = file.defaults.mask.clone();
        // N12: the version that gates writes must change when the *effective* rule set changes
        // (builtin rules shipped with the binary included), not only when the user file does.
        let effective_hash = sha256_hex(&serde_json::to_vec(&file).unwrap_or_default());
        Ok(Self {
            file,
            version_hash,
            effective_hash,
            lit_sep,
            lit_nosep,
            literal_meta,
            keyword_ac,
            keyword_to_rules,
            rules,
            allows,
            mask,
            disabled: false,
        })
    }

    /// sha256 over the canonical serialization of the effective policy (defaults, builtin and
    /// user rules after `extend`), independent of the file's byte formatting (m2 N12).
    pub fn effective_hash(&self) -> &str {
        &self.effective_hash
    }

    /// Scan `text` in overlapping windows of `window_chars` chars with a 50% stride and merge
    /// the verdicts (offsets refer to `text`). Inputs up to `window_chars` are scanned whole.
    /// Used for views joined from several fields and for document streams, so the per-field
    /// cap (`max_field_chars`) is not applied to the joined length (core-plan C1.2, docs-ingest D6).
    pub fn scan_windows(&self, text: &str, window_chars: usize) -> Verdict {
        if self.disabled {
            return Verdict::clean();
        }
        let window_chars = window_chars.max(2);
        let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        if starts.len() <= window_chars {
            return self.scan(text);
        }
        let stride = window_chars / 2;
        let mut hits: Vec<Hit> = Vec::new();
        let mut reject_rules: Vec<String> = Vec::new();
        let mut from = 0usize;
        loop {
            let to = (from + window_chars).min(starts.len());
            let (b0, b1) = (
                starts[from],
                if to == starts.len() {
                    text.len()
                } else {
                    starts[to]
                },
            );
            let v = self.scan(&text[b0..b1]);
            for r in v.reject_rules {
                if !reject_rules.contains(&r) {
                    reject_rules.push(r);
                }
            }
            hits.extend(v.hits.into_iter().map(|h| Hit {
                start: h.start + b0,
                end: h.end + b0,
                ..h
            }));
            if to == starts.len() {
                break;
            }
            from += stride;
        }
        hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
        hits.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.rule == b.rule);
        let rejected = !reject_rules.is_empty();
        let redacted = if rejected || hits.is_empty() {
            String::new()
        } else {
            apply_mask(text, &hits, &self.mask)
        };
        Verdict {
            rejected,
            hits,
            redacted,
            reject_rules,
        }
    }

    /// Scan the `key=value` view of one structured leaf (JSON prop, frontmatter value). Besides
    /// the ordinary rules, a credential-named key (`password`, `secret`, `api_key`, `token`,
    /// `db_password`, ...) with a non-empty, non-placeholder value is a `credential-key` hit
    /// that always rejects, whatever the value looks like (core-spec S1; m2 N2).
    pub fn scan_kv(&self, key: &str, value: &str) -> Verdict {
        if self.disabled {
            return Verdict::clean();
        }
        let joined = format!("{key}={value}");
        let mut v = self.scan(&joined);
        if v.rejected && v.reject_rules.iter().any(|r| r == "input-too-long") {
            return v;
        }
        if is_credential_key(key) && !value.is_empty() && !self.allowed("credential-key", value) {
            v.hits.push(Hit {
                rule: "credential-key".into(),
                action: Action::Reject,
                start: key.len() + 1,
                end: joined.len(),
            });
            v.hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
            v.rejected = true;
            v.reject_rules.push("credential-key".into());
            v.redacted = String::new();
        }
        v
    }

    fn allowed(&self, rule_id: &str, secret: &str) -> bool {
        let lower = secret.to_lowercase();
        self.allows
            .iter()
            .filter(|a| a.rule_ids.is_empty() || a.rule_ids.iter().any(|r| r == rule_id))
            .any(|a| a.exact.contains(&lower) || a.res.iter().any(|re| re.is_match(secret)))
    }

    /// Scan one string. Offsets in hits refer to the original string.
    pub fn scan(&self, text: &str) -> Verdict {
        if self.disabled {
            return Verdict::clean();
        }
        let mut hits: Vec<Hit> = Vec::new();
        if text.chars().count() > self.file.defaults.max_field_chars {
            return Verdict {
                rejected: true,
                hits: vec![],
                redacted: String::new(),
                reject_rules: vec!["input-too-long".into()],
            };
        }
        let views = Views::new(text);
        // literals over both casefolded views
        for (ac, nz) in [(&self.lit_sep, &views.sep), (&self.lit_nosep, &views.nosep)] {
            let Some(ac) = ac else { continue };
            for m in ac.find_overlapping_iter(&nz.text) {
                let meta = &self.literal_meta[m.pattern().as_usize()];
                if meta.word && !at_word_boundaries(&nz.text, m.start(), m.end()) {
                    continue;
                }
                let (start, end) = nz.raw_span(views.raw, m.start(), m.end());
                hits.push(Hit {
                    rule: meta.id.clone(),
                    action: meta.action,
                    start,
                    end,
                });
            }
        }
        // keyword-gated regex rules on the NFKC/ignorable-stripped view (S2); rules without
        // keywords always run
        let nfkc = &views.nfkc;
        let mut candidate: Vec<bool> = self
            .rules
            .iter()
            .map(|r| r.rule.keywords.is_empty())
            .collect();
        if let Some(ac) = &self.keyword_ac {
            for m in ac.find_iter(&nfkc.text) {
                for &ri in &self.keyword_to_rules[m.pattern().as_usize()] {
                    candidate[ri] = true;
                }
            }
        }
        for (i, cr) in self.rules.iter().enumerate() {
            if !candidate[i] {
                continue;
            }
            for caps in cr.re.captures_iter(&nfkc.text) {
                let Some(m) = caps.get(cr.rule.secret_group).or_else(|| caps.get(0)) else {
                    continue;
                };
                let secret = m.as_str();
                if let Some(min) = cr.rule.min_length
                    && secret.chars().count() < min
                {
                    continue;
                }
                if let Some(th) = cr.rule.entropy
                    && shannon_entropy(secret) < th
                {
                    continue;
                }
                if self.allowed(&cr.rule.id, secret) {
                    continue;
                }
                let (start, end) = nfkc.raw_span(views.raw, m.start(), m.end());
                hits.push(Hit {
                    rule: cr.rule.id.clone(),
                    action: cr.action,
                    start,
                    end,
                });
            }
        }
        // snap to char boundaries and sort
        for h in &mut hits {
            while h.start > 0 && !text.is_char_boundary(h.start) {
                h.start -= 1;
            }
            while h.end < text.len() && !text.is_char_boundary(h.end) {
                h.end += 1;
            }
        }
        hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
        hits.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.rule == b.rule);
        let rejected = hits.iter().any(|h| h.action == Action::Reject);
        let mut reject_rules: Vec<String> = hits
            .iter()
            .filter(|h| h.action == Action::Reject)
            .map(|h| h.rule.clone())
            .collect();
        reject_rules.dedup();
        let redacted = if rejected || hits.is_empty() {
            String::new()
        } else {
            apply_mask(text, &hits, &self.mask)
        };
        Verdict {
            rejected,
            hits,
            redacted,
            reject_rules,
        }
    }
}

/// Key names whose value is a credential by construction (exact, or as the last `_`-separated
/// part: `db_password`, `SMTP-Password`, `aws.secret_access_key`).
const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "pwd",
    "secret",
    "secret_key",
    "secretkey",
    "api_key",
    "apikey",
    "api_secret",
    "access_key",
    "secret_access_key",
    "access_token",
    "auth_token",
    "refresh_token",
    "session_token",
    "bearer_token",
    "id_token",
    "token",
    "client_secret",
    "app_secret",
    "private_key",
    "credential",
    "credentials",
];

/// Whether `key` names a credential. The key is classified on the same normalized view as
/// literal terms (core-spec S2; review R1-2): NFKD, casefold, confusables skeleton, ignorable
/// code points dropped and every separator run (`-`, `.`, `/`, spaces, ...) folded to one `_`,
/// so `ｓｅｃｒｅｔ`, `рassword` (Cyrillic er), `SMTP-Password` and `aws.secret_access_key` all
/// classify like their ASCII spellings.
pub fn is_credential_key(key: &str) -> bool {
    let k: String = crate::normalize::norm_term(key, true)
        .chars()
        .map(|c| if c == ' ' { '_' } else { c })
        .collect();
    CREDENTIAL_KEYS
        .iter()
        .any(|t| k == *t || k.strip_suffix(t).is_some_and(|p| p.ends_with('_')))
}

/// Whether `text[start..end]` is delimited by non-alphanumerics (or the ends) on both sides.
fn at_word_boundaries(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
}

/// Replace every hit span with the mask, merging overlapping/adjacent spans.
fn apply_mask(text: &str, hits: &[Hit], mask: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    let mut i = 0;
    while i < hits.len() {
        let mut start = hits[i].start;
        let mut end = hits[i].end;
        let rule = hits[i].rule.clone();
        let mut j = i + 1;
        while j < hits.len() && hits[j].start < end {
            end = end.max(hits[j].end);
            j += 1;
        }
        if start < pos {
            start = pos;
        }
        if end > start {
            out.push_str(&text[pos..start]);
            out.push_str(&mask.replace("{rule}", &rule));
            pos = end;
        }
        i = j;
    }
    out.push_str(&text[pos..]);
    out
}

/// Shannon entropy in bits per byte.
#[allow(clippy::cast_precision_loss)] // byte counts are far below 2^52
pub fn shannon_entropy(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    let bytes = s.as_bytes();
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}
