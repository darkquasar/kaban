//! Denylist gating before identity (docs-ingest-plan.md DI1.4; docs-ingest-spec D6; core-spec
//! S1; PR-2). Order per document: (0) the path was gated in DI0.2 (`GatedPath` -> `SourceId`);
//! (1a) [`admit_frontmatter`]: the YAML block's source text, before any interpretation (RD2-1:
//! the parser only ever sees an [`AdmittedFrontmatter`]); (1b) [`gate_metadata`]: the source
//! redact hits mapped onto the parsed scalars (or the block withheld), then the title and every
//! frontmatter leaf through `scan` and its `key=value` view through `scan_kv`; (2)
//! [`gate_document`]: the pre-chunk extracted stream with overlapping windows (a secret split
//! across chunks, or one whose rule needs the blank line between two paragraphs, is found),
//! projected onto chunk text through each chunk's exact [`ChunkMap`] (RD2-2), then every
//! chunk's fence language, text and `context_prefix`; (2b) [`gate_links_and_tags`]: a link or
//! tag whose source span a hit covered is removed (RD2-3); (3) only after `Proceed` may the
//! caller compute ids and the content hash, and only from the sanitized texts this module
//! returns.
//!
//! ```text
//!   frontmatter source ==> admit_frontmatter ==> Admitted(source, redact hits) | SkipFile
//!   title/frontmatter  ==> gate_metadata     ==> Proceed(SanitizedMeta) | SkipFile{rule_ids}
//!   stream + chunks    ==> gate_document     ==> Proceed(Gated{chunks, coverage}) | SkipFile
//!                           |  windows: any hit of an `on_document: skip_file` rule => SkipFile
//!                           |  stream hit over a chunk's map: reject => stub, redact => masked
//!                           +- chunk: redact => masked text; reject => Stub + HMAC(audit_key, text)
//!   links/tags + coverage ==> gate_links_and_tags ==> kept (masked) | removed with a receipt
//! ```
//!
//! Nothing here logs document content: audit rows carry rule ids and keyed hashes only
//! (`obs-no-sensitive-data`).

use crate::cancel::ExtractCtx;
use crate::chunk::{ChunkConfig, ChunkDraft, ChunkMap, Stream};
use crate::extract::{LinkDraft, TagDraft};
use crate::model::{ChunkKind, IngestError};
use kaban_core::{AuditEntry, AuditOp, WorkspaceId};
use kaban_policy::{Action, AuditSigner, Hit, OnDocument, Policy, Verdict};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;

/// Metadata after stage (1): masked where redact hits occurred.
#[derive(Debug, Clone, PartialEq)]
pub struct SanitizedMeta {
    /// The title, masked where redact hits occurred.
    pub title: Option<String>,
    /// The frontmatter with every string leaf masked where redact hits occurred.
    pub frontmatter: Value,
    /// Rule ids of the redactions applied (one entry per hit; counts only, never text).
    pub redactions: Vec<String>,
    /// Keyed audit receipts of the redactions (`title`, `frontmatter`; RD1-10).
    pub audit: Vec<AuditEntry>,
}

/// Outcome of the metadata gate.
#[derive(Debug, Clone, PartialEq)]
pub enum MetaDecision {
    /// Metadata is safe to use (masked where needed).
    Proceed(SanitizedMeta),
    /// A reject-class or `skip_file` hit: the document is recorded `blocked`, no title, empty
    /// frontmatter, no `content_hash`.
    SkipFile {
        /// Rule ids that fired.
        rule_ids: Vec<String>,
    },
}

/// Window size for the frontmatter source text and its sanitized rendering (`scan_windows`):
/// the per-field cap does not apply to a joined view, so the block is scanned in windows.
const SOURCE_WINDOW_CHARS: usize = 8192;

/// Rule ids of a verdict's hits and reject rules, sorted and deduplicated.
fn rule_ids_of(v: &Verdict) -> Vec<String> {
    let mut ids: Vec<String> = v.hits.iter().map(|h| h.rule.clone()).collect();
    ids.extend(v.reject_rules.iter().cloned());
    ids.sort();
    ids.dedup();
    ids
}

/// Whether any hit of `v` belongs to a rule with `on_document: skip_file`.
fn skips_file(policy: &Policy, v: &Verdict) -> bool {
    v.hits.iter().any(|h| {
        policy
            .file
            .rules
            .iter()
            .any(|r| r.id == h.rule && r.on_document == Some(OnDocument::SkipFile))
    })
}

/// Scan one free-text field: `Err(rule_ids)` when it rejects (or a `skip_file` rule fired),
/// otherwise the text masked where redact hits occurred (borrowed when clean,
/// `own-cow-conditional`) and the redaction rule ids.
pub(crate) fn gate_text<'a>(
    policy: &Policy,
    text: &'a str,
) -> Result<(Cow<'a, str>, Vec<String>), Vec<String>> {
    let v = policy.scan(text);
    if v.rejected || skips_file(policy, &v) {
        return Err(rule_ids_of(&v));
    }
    if v.hits.is_empty() {
        return Ok((Cow::Borrowed(text), Vec::new()));
    }
    let ids = v.hits.iter().map(|h| h.rule.clone()).collect();
    Ok((Cow::Owned(v.redacted), ids))
}

/// What the gate found in one short persisted string (a fence language, a link target, a tag).
enum FieldVerdict {
    /// No hit: keep the string.
    Clean,
    /// Redact hits: the masked string and the rule ids.
    Masked(String, Vec<String>),
    /// A reject-class hit without `skip_file`: drop or stub the string, rule ids.
    Rejected(Vec<String>),
    /// A `skip_file` rule fired: the whole document is blocked.
    SkipsFile(Vec<String>),
}

fn gate_field(policy: &Policy, text: &str) -> FieldVerdict {
    let v = policy.scan(text);
    if skips_file(policy, &v) {
        FieldVerdict::SkipsFile(rule_ids_of(&v))
    } else if v.rejected {
        FieldVerdict::Rejected(rule_ids_of(&v))
    } else if v.hits.is_empty() {
        FieldVerdict::Clean
    } else {
        let ids = v.hits.iter().map(|h| h.rule.clone()).collect();
        FieldVerdict::Masked(v.redacted, ids)
    }
}

/// Gate one structured leaf: its `key=value` view through `scan_kv` (credential-named keys,
/// rules anchored on the view) and the bare value through `scan`. `Err(rule_ids)` when either
/// rejects or a `skip_file` rule fired; otherwise the redact hits of the view are projected
/// onto the value (`start - (key.len() + 1)`), unioned with the direct hits and masked
/// together (RD1-4), so a rule that matches only the view still redacts and is counted.
fn gate_leaf<'a>(
    policy: &Policy,
    key: &str,
    value: &'a str,
) -> Result<(Cow<'a, str>, Vec<String>), Vec<String>> {
    let kv = policy.scan_kv(key, value);
    if kv.rejected || skips_file(policy, &kv) {
        return Err(rule_ids_of(&kv));
    }
    let direct = policy.scan(value);
    if direct.rejected || skips_file(policy, &direct) {
        return Err(rule_ids_of(&direct));
    }
    let offset = key.len() + 1; // the `=`
    let mut hits = direct.hits;
    hits.extend(
        kv.hits
            .into_iter()
            .filter(|h| h.end > offset && h.action == Action::Redact)
            .map(|h| Hit {
                start: h.start.saturating_sub(offset),
                end: (h.end - offset).min(value.len()),
                ..h
            }),
    );
    if hits.is_empty() {
        return Ok((Cow::Borrowed(value), Vec::new()));
    }
    hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
    hits.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.rule == b.rule);
    let ids = hits.iter().map(|h| h.rule.clone()).collect();
    let masked = mask_spans(value, &hits, &policy.file.defaults.mask);
    Ok((Cow::Owned(masked), ids))
}

/// Walk a JSON value, masking string leaves and judging their `key=value` view; `Err` on the
/// first rejecting leaf. `path` is the dotted key path used as the `key` of the kv view.
fn gate_value(
    policy: &Policy,
    auditor: &Auditor<'_>,
    path: &str,
    value: &Value,
    meta: &mut SanitizedMeta,
) -> Result<Value, Vec<String>> {
    match value {
        Value::String(s) => {
            let (masked, ids) = gate_leaf(policy, path, s)?;
            meta.redacted(auditor, "frontmatter", s, ids);
            Ok(Value::String(masked.into_owned()))
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| gate_value(policy, auditor, &format!("{path}.{i}"), v, meta))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                // the key itself is content too (S1 props leaves)
                let (key, ids) = gate_text(policy, k)?;
                meta.redacted(auditor, "frontmatter", k, ids);
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                out.insert(
                    key.into_owned(),
                    gate_value(policy, auditor, &child, v, meta)?,
                );
            }
            Ok(Value::Object(out))
        }
        // RD1-3: a number, boolean or null is judged in its `key=value` view like a string, so
        // YAML typing cannot hide `password: 12345678`; a redact hit replaces it by the mask
        other => {
            let rendered = other.to_string();
            let (masked, ids) = gate_leaf(policy, path, &rendered)?;
            match masked {
                Cow::Borrowed(_) => Ok(other.clone()),
                Cow::Owned(masked) => {
                    meta.redacted(auditor, "frontmatter", &rendered, ids);
                    Ok(Value::String(masked))
                }
            }
        }
    }
}

impl SanitizedMeta {
    /// Record one field's redaction: the rule ids counted and one keyed receipt (no text).
    fn redacted(&mut self, auditor: &Auditor<'_>, field: &str, original: &str, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        self.redactions.extend(ids.iter().cloned());
        self.audit
            .push(auditor.entry(AuditOp::Redact, field, original, ids));
    }
}

/// A frontmatter source block the policy admitted for interpretation (RD2-1; `type-newtype-
/// validated`): the only value [`crate::Extracted::interpret_frontmatter`] accepts, so no YAML
/// is ever parsed from source text the denylist has not scanned. Carries the redact hits the
/// source scan found, for [`gate_metadata`] to map onto the parsed scalars.
#[derive(Debug, Clone)]
pub struct AdmittedFrontmatter<'a> {
    source: &'a str,
    redact_hits: Vec<Hit>,
}

impl AdmittedFrontmatter<'_> {
    /// The admitted YAML text.
    pub fn source(&self) -> &str {
        self.source
    }
}

/// Outcome of [`admit_frontmatter`].
#[derive(Debug, Clone)]
pub enum Admission<'a> {
    /// No reject-class or `skip_file` hit: the block may be interpreted.
    Admitted(AdmittedFrontmatter<'a>),
    /// A reject-class or `skip_file` hit in the source: the document is blocked before any
    /// interpretation.
    SkipFile {
        /// Rule ids that fired.
        rule_ids: Vec<String>,
    },
}

/// Stage (1a): scan the frontmatter block as written, in windows, before anything interprets
/// it (RD1-3, RD2-1): a credential the parser would turn into a number, or one inside a block
/// that does not parse, blocks the document here; redact hits travel with the admission.
pub fn admit_frontmatter<'a>(policy: &Policy, source: &'a str) -> Admission<'a> {
    let v = policy.scan_windows(source, SOURCE_WINDOW_CHARS);
    if v.rejected || skips_file(policy, &v) {
        return Admission::SkipFile {
            rule_ids: rule_ids_of(&v),
        };
    }
    let mut redact_hits: Vec<Hit> = v
        .hits
        .into_iter()
        .filter(|h| h.action == Action::Redact && h.end > h.start)
        .collect();
    redact_hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
    redact_hits.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.rule == b.rule);
    Admission::Admitted(AdmittedFrontmatter {
        source,
        redact_hits,
    })
}

/// Apply the source redact hits to the parsed value through a source-to-scalar mapping: the
/// matched source text of every hit must occur verbatim inside one or more string (or
/// rendered scalar) leaves, where it is masked; a hit found in no leaf, or inside a key (the
/// match crossed `key: value`, or YAML quoting changed the text), makes the mapping
/// unavailable and the caller withholds the whole block. `None` = unmappable.
fn apply_source_hits(
    value: &Value,
    admitted: &AdmittedFrontmatter<'_>,
    mask: &str,
) -> Option<Value> {
    let mut out = value.clone();
    for hit in &admitted.redact_hits {
        let matched = admitted.source.get(hit.start..hit.end)?;
        if matched.trim().is_empty() {
            return None;
        }
        let label = mask.replace("{rule}", &hit.rule);
        if !mask_leaves(&mut out, matched, &label)? {
            return None;
        }
    }
    Some(out)
}

/// Mask `matched` inside every scalar leaf of `value`; `Some(true)` when at least one leaf
/// carried it, `Some(false)` when none did, `None` when a key carried it.
fn mask_leaves(value: &mut Value, matched: &str, label: &str) -> Option<bool> {
    match value {
        Value::String(s) => {
            if s.contains(matched) {
                *s = s.replace(matched, label);
                return Some(true);
            }
            Some(false)
        }
        Value::Array(items) => {
            let mut any = false;
            for v in items {
                any |= mask_leaves(v, matched, label)?;
            }
            Some(any)
        }
        Value::Object(map) => {
            if map.keys().any(|k| k.contains(matched)) {
                return None;
            }
            let mut any = false;
            for v in map.values_mut() {
                any |= mask_leaves(v, matched, label)?;
            }
            Some(any)
        }
        other => {
            let rendered = other.to_string();
            if rendered.contains(matched) {
                *other = Value::String(rendered.replace(matched, label));
                return Some(true);
            }
            Some(false)
        }
    }
}

/// Stage (1b): gate the title and the frontmatter before anything derived from them exists.
/// The path was gated in DI0.2 (`GatedPath`); `admitted` is the block's source admission
/// (stage 1a): its redact hits are applied first through the source-to-scalar mapping, or the
/// whole block is withheld as `{}` with one keyed `frontmatter` receipt (RD2-1). A title taken
/// from the frontmatter follows the block's sanitization. Then every scalar is judged in its
/// `key=value` view (RD1-3, RD1-4), and the sanitized value is rendered and rescanned: a redact
/// hit that survived leaf masking withholds the whole frontmatter.
pub fn gate_metadata(
    policy: &Policy,
    signer: &dyn AuditSigner,
    workspace: &WorkspaceId,
    title: Option<&str>,
    frontmatter: &Value,
    admitted: Option<&AdmittedFrontmatter<'_>>,
) -> MetaDecision {
    let auditor = Auditor {
        signer,
        workspace,
        policy_version: policy.effective_hash(),
    };
    let mut meta = SanitizedMeta {
        title: None,
        frontmatter: Value::Null,
        redactions: Vec::new(),
        audit: Vec::new(),
    };
    // (1b.1) the source redact hits, mapped or withholding
    let mut title = title.map(Cow::Borrowed);
    let mapped: Cow<'_, Value> = match admitted {
        Some(a) if !a.redact_hits.is_empty() => {
            let ids: Vec<String> = a.redact_hits.iter().map(|h| h.rule.clone()).collect();
            meta.redacted(&auditor, "frontmatter", a.source, ids);
            let sanitized = apply_source_hits(frontmatter, a, &policy.file.defaults.mask)
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            // a title that came from the block follows the block (`None` when withheld)
            if let (Some(t), Some(from_block)) = (
                title.as_deref(),
                frontmatter.get("title").and_then(Value::as_str),
            ) && t == from_block
            {
                title = sanitized
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| Cow::Owned(t.to_owned()));
            }
            Cow::Owned(sanitized)
        }
        _ => Cow::Borrowed(frontmatter),
    };
    meta.title = match title.as_deref().map(|t| gate_text(policy, t)) {
        None => None,
        Some(Err(rule_ids)) => return MetaDecision::SkipFile { rule_ids },
        Some(Ok((masked, ids))) => {
            meta.redacted(&auditor, "title", title.as_deref().unwrap_or_default(), ids);
            Some(masked.into_owned())
        }
    };
    meta.frontmatter = match gate_value(policy, &auditor, "", &mapped, &mut meta) {
        Ok(v) => v,
        Err(rule_ids) => return MetaDecision::SkipFile { rule_ids },
    };
    if admitted.is_some() {
        let rendered = meta.frontmatter.to_string();
        let v = policy.scan_windows(&rendered, SOURCE_WINDOW_CHARS);
        if v.rejected || skips_file(policy, &v) {
            return MetaDecision::SkipFile {
                rule_ids: rule_ids_of(&v),
            };
        }
        if !v.hits.is_empty() {
            let ids = v.hits.iter().map(|h| h.rule.clone()).collect();
            meta.redacted(&auditor, "frontmatter", &rendered, ids);
            meta.frontmatter = Value::Object(serde_json::Map::new());
        }
    }
    MetaDecision::Proceed(meta)
}

/// A document after stage (2): sanitized metadata and chunks, counts and audit rows.
#[derive(Debug, Clone)]
pub struct Gated {
    /// Stage (1) output, passed through.
    pub meta: SanitizedMeta,
    /// Chunks with text masked or replaced by a `Stub` (`text` empty, `stub_hmac` set).
    pub chunks: Vec<GatedChunk>,
    /// Chunks turned into stubs.
    pub blocked_chunks: u32,
    /// Rule id -> hits across the document (stream and chunk scans; counts only).
    pub rule_ids: BTreeMap<String, u32>,
    /// Audit rows (`reject`/`redact`, tool `ingest`, keyed hashes, rule ids; never text).
    pub audit: Vec<AuditEntry>,
    /// The extractor error, gated (masked, or replaced when it rejects).
    pub error: Option<String>,
    /// Every stream range a hit covered (stream hits, chunk hits mapped back, stubbed chunks
    /// whole), for the derived links and tags (RD2-3).
    pub coverage: StreamCoverage,
}

/// The stream ranges the document gate acted on (RD2-3), in stream coordinates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamCoverage {
    ranges: Vec<Range<usize>>,
}

impl StreamCoverage {
    fn add(&mut self, r: Range<usize>) {
        if r.end > r.start {
            self.ranges.push(r);
        }
    }

    /// Whether any covered range overlaps `r`.
    pub fn overlaps(&self, r: &Range<usize>) -> bool {
        self.ranges
            .iter()
            .any(|c| c.start < r.end && c.end > r.start)
    }

    /// Whether anything was covered.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// A chunk after gating: the draft with sanitized text plus the stub hash when stubbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedChunk {
    /// The chunk; `text` is the masked text, or empty for a stub (`kind: Stub`).
    pub draft: ChunkDraft,
    /// `HMAC(audit_key, original_text)` when the chunk was stubbed (core-spec S1).
    pub stub_hmac: Option<String>,
}

/// Outcome of the document gate.
#[derive(Debug, Clone)]
pub enum GateDecision {
    /// Safe to hash, id and commit from the sanitized values.
    Proceed(Gated),
    /// A `skip_file` rule (or a rejecting metadata leaf) fired: `Document{status: blocked}`.
    SkipFile {
        /// Rule ids that fired.
        rule_ids: Vec<String>,
        /// The audit rows to append with the blocked write.
        audit: Vec<AuditEntry>,
    },
}

/// A contiguous [`Stream`] over already-cut chunks: their texts concatenated with no separator
/// and one span each. This is the layout of pieces cut from one paragraph group inside a token
/// (`password = Sup3r` | `S3cretValue!`), for callers that hold chunks but no extractor output.
/// The real pipeline passes the chunker's stream ([`crate::chunk::chunk_with_stream`]), which
/// keeps the paragraph boundaries the extractor produced (RD1-5): a separator-less join of two
/// paragraphs would hide a hit whose rule needs the whitespace between them.
pub fn stream_text(chunks: &[ChunkDraft]) -> Stream {
    Stream::contiguous(chunks.iter().map(|c| c.text.as_str()))
}

/// Window size in chars for the stream scan: `2 * hard_max_chunk_chars` (core-spec S1).
fn window_chars(cfg: &ChunkConfig) -> usize {
    (cfg.hard_max_tokens as usize)
        .saturating_mul(4)
        .saturating_mul(2)
        .max(2)
}

/// `Policy::scan_windows` with `ctx.check()` between windows (the plan's "calls `ctx.check()`
/// per window"): the same stride (half a window) and the same hit offset mapping.
fn scan_windows_checked(
    policy: &Policy,
    text: &str,
    window_chars: usize,
    ctx: &ExtractCtx,
) -> Result<Verdict, IngestError> {
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    if starts.len() <= window_chars {
        ctx.check()?;
        return Ok(policy.scan(text));
    }
    let stride = (window_chars / 2).max(1);
    let mut hits = Vec::new();
    let mut reject_rules: Vec<String> = Vec::new();
    let mut from = 0usize;
    loop {
        ctx.check()?;
        let to = (from + window_chars).min(starts.len());
        let b0 = starts[from];
        let b1 = if to == starts.len() {
            text.len()
        } else {
            starts[to]
        };
        let v = policy.scan(&text[b0..b1]);
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
    Ok(Verdict {
        rejected: !reject_rules.is_empty(),
        hits,
        redacted: String::new(),
        reject_rules,
    })
}

/// Everything the gate needs to write an audit row.
struct Auditor<'a> {
    signer: &'a dyn AuditSigner,
    workspace: &'a WorkspaceId,
    policy_version: &'a str,
}

impl Auditor<'_> {
    fn entry(&self, op: AuditOp, field: &str, original: &str, rules: Vec<String>) -> AuditEntry {
        AuditEntry {
            op,
            tool: "ingest".into(),
            field: field.into(),
            workspace_id: self.workspace.clone(),
            policy_version: self.policy_version.to_owned(),
            input_hmac: self.signer.hmac_hex(original.as_bytes()),
            rules,
            node_id: None,
        }
    }
}

fn count(rule_ids: &mut BTreeMap<String, u32>, ids: &[String]) {
    for r in ids {
        *rule_ids.entry(r.clone()).or_default() += 1;
    }
}

/// Stage (2): the pre-chunk stream with overlapping windows, then every chunk (fence language,
/// text and `context_prefix`), then the extractor error string. `stream` carries one span per
/// chunk (the chunker's [`crate::chunk::Chunked::stream`], or [`stream_text`] over hand-cut
/// chunks) so stream hits map back onto chunks; `workspace` stamps the audit rows (additive to
/// the plan's signature: `AuditEntry` requires it).
///
/// # Errors
/// `Cancelled`/`DeadlineExceeded` from `ctx` between windows (as `IngestError::Extract`).
#[allow(clippy::too_many_arguments)] // the plan's signature plus the workspace
pub fn gate_document(
    policy: &Policy,
    signer: &dyn AuditSigner,
    ctx: &ExtractCtx,
    workspace: &WorkspaceId,
    meta: SanitizedMeta,
    stream: &Stream,
    chunks: Vec<ChunkDraft>,
    extractor_error: Option<&str>,
    cfg: &ChunkConfig,
) -> Result<GateDecision, IngestError> {
    debug_assert_eq!(stream.maps.len(), chunks.len(), "one map per chunk");
    let full_text = stream.text.as_str();
    let mut coverage = StreamCoverage::default();
    let auditor = Auditor {
        signer,
        workspace,
        policy_version: policy.effective_hash(),
    };
    let mut rule_ids: BTreeMap<String, u32> = BTreeMap::new();
    let mut meta = meta;
    let mut audit = std::mem::take(&mut meta.audit);
    for r in &meta.redactions {
        *rule_ids.entry(r.clone()).or_default() += 1;
    }

    // (2) the whole stream: a `skip_file` rule anywhere blocks the document; other hits are
    // remembered by span so a chunk the per-chunk scan cannot judge (the hit spans a boundary)
    // is stubbed rather than stored
    let whole = scan_windows_checked(policy, full_text, window_chars(cfg), ctx)?;
    if skips_file(policy, &whole) {
        let ids = rule_ids_of(&whole);
        audit.push(auditor.entry(AuditOp::Reject, "stream", full_text, ids.clone()));
        return Ok(GateDecision::SkipFile {
            rule_ids: ids,
            audit,
        });
    }
    for h in &whole.hits {
        coverage.add(h.start..h.end);
    }
    // (3) per chunk. A stream hit that overlaps a chunk's map is a finding about that chunk
    // even when the chunk alone shows nothing (the rule needed context in a neighbour, or the
    // match crosses the boundary): reject-class => stub, redact-class => its projection
    // through the exact map onto the chunk text is masked together with the chunk's own hits.
    // A chunk whose map is inexact is stubbed under any overlapping hit (fail closed).
    let empty_map = ChunkMap::default();
    let mut out = Vec::with_capacity(chunks.len());
    let mut blocked_chunks = 0u32;
    for (i, mut draft) in chunks.into_iter().enumerate() {
        ctx.check()?;
        // the fence-info token is a persisted string too (RD1-2, S1): masked, dropped or,
        // under a `skip_file` rule, the document is blocked
        if let ChunkKind::Code { lang: Some(lang) } = &draft.kind {
            match gate_field(policy, lang) {
                FieldVerdict::Clean => {}
                FieldVerdict::Masked(masked, ids) => {
                    count(&mut rule_ids, &ids);
                    audit.push(auditor.entry(AuditOp::Redact, "lang", lang, ids));
                    draft.kind = ChunkKind::Code { lang: Some(masked) };
                }
                FieldVerdict::Rejected(ids) => {
                    count(&mut rule_ids, &ids);
                    audit.push(auditor.entry(AuditOp::Reject, "lang", lang, ids));
                    draft.kind = ChunkKind::Code { lang: None };
                }
                FieldVerdict::SkipsFile(ids) => {
                    audit.push(auditor.entry(AuditOp::Reject, "lang", lang, ids.clone()));
                    return Ok(GateDecision::SkipFile {
                        rule_ids: ids,
                        audit,
                    });
                }
            }
        }
        let map = stream.maps.get(i).unwrap_or(&empty_map);
        let hull = map.source_span();
        let overlapping = whole
            .hits
            .iter()
            .filter(|h| h.start < hull.end && h.end > hull.start);
        let inexact_overlap = !map.is_exact() && overlapping.clone().next().is_some();
        let mut from_stream: Vec<Hit> = Vec::new();
        for h in overlapping {
            let hit = h.start..h.end;
            from_stream.extend(map.project(&hit).map(|r| Hit {
                start: r.start,
                end: r.end,
                ..h.clone()
            }));
        }

        let text_verdict = if draft.text.chars().count() > window_chars(cfg) {
            scan_windows_checked(policy, &draft.text, window_chars(cfg), ctx)?
        } else {
            policy.scan(&draft.text)
        };
        let prefix_verdict = policy.scan(&draft.context_prefix);
        let mut ids = rule_ids_of(&text_verdict);
        ids.extend(rule_ids_of(&prefix_verdict));
        ids.extend(from_stream.iter().map(|h| h.rule.clone()));
        ids.sort();
        ids.dedup();
        count(&mut rule_ids, &ids);

        for h in &text_verdict.hits {
            for r in map.to_source(&(h.start..h.end)) {
                coverage.add(r);
            }
        }
        let reject = text_verdict.rejected
            || prefix_verdict.rejected
            || inexact_overlap
            || from_stream.iter().any(|h| h.action == Action::Reject);
        if reject {
            coverage.add(hull);
            let original = std::mem::take(&mut draft.text);
            let stub_hmac = signer.hmac_hex(original.as_bytes());
            audit.push(auditor.entry(AuditOp::Reject, "chunk", &original, ids));
            draft.kind = ChunkKind::Stub;
            if prefix_verdict.rejected {
                draft.context_prefix = String::new();
            } else if !prefix_verdict.hits.is_empty() {
                draft.context_prefix = prefix_verdict.redacted;
            }
            blocked_chunks += 1;
            out.push(GatedChunk {
                draft,
                stub_hmac: Some(stub_hmac),
            });
            continue;
        }
        if !text_verdict.hits.is_empty() || !from_stream.is_empty() {
            let masked = if from_stream.is_empty() {
                masked_text(&draft.text, &text_verdict, policy)
            } else {
                let mut hits = text_verdict.hits;
                hits.extend(from_stream);
                hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
                hits.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.rule == b.rule);
                mask_spans(&draft.text, &hits, &policy.file.defaults.mask)
            };
            let original = std::mem::replace(&mut draft.text, masked);
            audit.push(auditor.entry(AuditOp::Redact, "chunk", &original, ids.clone()));
        }
        if !prefix_verdict.hits.is_empty() {
            let original = std::mem::replace(&mut draft.context_prefix, prefix_verdict.redacted);
            audit.push(auditor.entry(AuditOp::Redact, "context_prefix", &original, ids));
        }
        out.push(GatedChunk {
            draft,
            stub_hmac: None,
        });
    }

    // extractor error strings are content too (S1): masked, or withheld when they reject
    let error = extractor_error.map(|e| match gate_text(policy, e) {
        Ok((masked, ids)) => {
            if !ids.is_empty() {
                count(&mut rule_ids, &ids);
                audit.push(auditor.entry(AuditOp::Redact, "error", e, ids));
            }
            masked.into_owned()
        }
        Err(ids) => {
            count(&mut rule_ids, &ids);
            audit.push(auditor.entry(AuditOp::Reject, "error", e, ids));
            "extractor error withheld by the denylist".to_owned()
        }
    });

    Ok(GateDecision::Proceed(Gated {
        meta,
        chunks: out,
        blocked_chunks,
        rule_ids,
        audit,
        error,
        coverage,
    }))
}

/// Where the document's derived links and tags came from (RD2-3): the extracted source text
/// (spans index into it), the stream with its blocks, what the document gate covered and the
/// source ranges of the sections whose heading carried a hit (a span outside every block sits
/// in a heading).
#[derive(Debug, Clone, Copy)]
pub struct DerivedContext<'a> {
    /// The decoded source text the extractor walked.
    pub source: &'a str,
    /// The chunker's stream.
    pub stream: &'a Stream,
    /// What [`gate_document`] covered.
    pub coverage: &'a StreamCoverage,
    /// Source ranges of sections whose heading path had a hit.
    pub redacted_sections: &'a [Range<u64>],
}

impl DerivedContext<'_> {
    /// Whether a hit covered the source span `span` (RD2-3). Inside a stream block the span's
    /// source form (`#tag`, `[[Note|alias]]`) is located verbatim in the block's text and
    /// compared with the coverage exactly; a form that does not survive inline rendering (a
    /// standard link's `[label](url)`) falls back to the block as a whole. Outside every
    /// block the span sits in a heading: covered when that section's heading had a hit. A
    /// span the source cannot slice is treated as covered when anything was covered.
    fn covers(&self, span: &Range<u64>) -> bool {
        let Some(block) = self
            .stream
            .blocks
            .iter()
            .find(|b| b.source.start <= span.start && span.end <= b.source.end)
        else {
            return self
                .redacted_sections
                .iter()
                .any(|s| s.start <= span.start && span.start < s.end);
        };
        let form = usize::try_from(span.start)
            .ok()
            .zip(usize::try_from(span.end).ok())
            .and_then(|(a, b)| self.source.get(a..b))
            .filter(|f| !f.is_empty());
        let Some(form) = form else {
            return !self.coverage.is_empty();
        };
        let text = &self.stream.text[block.stream.clone()];
        let mut found = false;
        let mut covered = false;
        for (at, _) in text.match_indices(form) {
            found = true;
            let r = block.stream.start + at..block.stream.start + at + form.len();
            covered |= self.coverage.overlaps(&r);
        }
        if found {
            covered
        } else {
            self.coverage.overlaps(&block.stream)
        }
    }
}

/// Gate link targets, aliases and tag names (S1: persisted strings; RD1-10 receipts): a
/// rejecting one is dropped and counted, a redactable one is masked; every hit leaves a keyed
/// audit row under `link`, `link_alias` or `tag`. With a [`DerivedContext`], a link or tag
/// whose source span the document gate covered is removed first and audited under `link` or
/// `tag` with the covering rules' ids, counted once already by the chunk hit (RD2-3: the chunk
/// hit decided against that text, so no edge, Tag node or `tag_id` preimage may restore it).
/// Appends to `rule_ids` and `audit`.
#[allow(clippy::too_many_arguments)] // the DI1.4 signature plus the RD2-3 context
pub fn gate_links_and_tags(
    policy: &Policy,
    signer: &dyn AuditSigner,
    workspace: &WorkspaceId,
    links: &[LinkDraft],
    tags: &[TagDraft],
    context: Option<DerivedContext<'_>>,
    rule_ids: &mut BTreeMap<String, u32>,
    audit: &mut Vec<AuditEntry>,
) -> (Vec<LinkDraft>, Vec<TagDraft>) {
    let auditor = Auditor {
        signer,
        workspace,
        policy_version: policy.effective_hash(),
    };
    // RD2-3: a covered span is removed with a receipt before the field-level gate
    let removed =
        |name: &str, text: &str, span: &Range<u64>, audit: &mut Vec<AuditEntry>| -> bool {
            let Some(ctx) = context else { return false };
            if !ctx.covers(span) {
                return false;
            }
            // the covering hit was counted when it masked or stubbed the chunk: receipt only
            let ids = covering_rules(policy, ctx, span, text);
            audit.push(auditor.entry(AuditOp::Reject, name, text, ids));
            true
        };
    // one string: `Some(masked)` to keep (receipt written when it was masked), `None` to drop
    let field = |name: &str,
                 text: &str,
                 rule_ids: &mut BTreeMap<String, u32>,
                 audit: &mut Vec<AuditEntry>|
     -> Option<String> {
        match gate_text(policy, text) {
            Ok((masked, ids)) => {
                if !ids.is_empty() {
                    count(rule_ids, &ids);
                    audit.push(auditor.entry(AuditOp::Redact, name, text, ids));
                }
                Some(masked.into_owned())
            }
            Err(ids) => {
                count(rule_ids, &ids);
                audit.push(auditor.entry(AuditOp::Reject, name, text, ids));
                None
            }
        }
    };
    let mut out_links = Vec::with_capacity(links.len());
    for l in links {
        if removed("link", &l.raw_target, &l.span(), audit) {
            continue;
        }
        let Some(raw_target) = field("link", &l.raw_target, rule_ids, audit) else {
            continue;
        };
        let alias = match l.alias.as_deref() {
            None => None,
            Some(a) => match field("link_alias", a, rule_ids, audit) {
                Some(a) => Some(a),
                None => continue,
            },
        };
        out_links.push(LinkDraft {
            raw_target,
            alias,
            ..l.clone()
        });
    }
    let mut out_tags = Vec::with_capacity(tags.len());
    for t in tags {
        if removed("tag", &t.tag, &t.span(), audit) {
            continue;
        }
        if let Some(tag) = field("tag", &t.tag, rule_ids, audit) {
            out_tags.push(TagDraft {
                tag,
                byte_offset: t.byte_offset,
                byte_end: t.byte_end,
            });
        }
    }
    (out_links, out_tags)
}

/// The rule ids to put on a removal receipt: the rules whose stream hits cover the span's
/// block (re-derived from a scan of that block's text, counts only), else `derived` when no
/// stream text is at hand (a heading hit).
fn covering_rules(
    policy: &Policy,
    ctx: DerivedContext<'_>,
    span: &Range<u64>,
    text: &str,
) -> Vec<String> {
    let block = ctx
        .stream
        .blocks
        .iter()
        .find(|b| b.source.start <= span.start && span.end <= b.source.end);
    let mut ids: Vec<String> = match block {
        Some(b) => policy
            .scan(&ctx.stream.text[b.stream.clone()])
            .hits
            .into_iter()
            .map(|h| h.rule)
            .collect(),
        None => Vec::new(),
    };
    ids.extend(policy.scan(text).hits.into_iter().map(|h| h.rule));
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        ids.push("derived".to_owned());
    }
    ids
}

/// Gate one section's heading path (S1: heading texts are persisted and are inputs of
/// `section_id`/`chunk_id`): redact hits are masked; a rejecting heading is replaced by the
/// policy mask carrying the rule id (a heading is structure, so it is kept as a placeholder
/// rather than dropped) and audited with its keyed hash. Returns the sanitized path and the rule
/// ids that fired.
pub fn gate_heading_path(
    policy: &Policy,
    signer: &dyn AuditSigner,
    workspace: &WorkspaceId,
    heading_path: &[String],
    audit: &mut Vec<AuditEntry>,
) -> (Vec<String>, Vec<String>) {
    let auditor = Auditor {
        signer,
        workspace,
        policy_version: policy.effective_hash(),
    };
    let mut ids_all = Vec::new();
    let path = heading_path
        .iter()
        .map(|h| match gate_text(policy, h) {
            Ok((masked, ids)) => {
                if !ids.is_empty() {
                    audit.push(auditor.entry(AuditOp::Redact, "heading", h, ids.clone()));
                }
                ids_all.extend(ids);
                masked.into_owned()
            }
            Err(ids) => {
                audit.push(auditor.entry(AuditOp::Reject, "heading", h, ids.clone()));
                let rule = ids.first().cloned().unwrap_or_default();
                ids_all.extend(ids);
                policy.file.defaults.mask.replace("{rule}", &rule)
            }
        })
        .collect();
    (path, ids_all)
}

/// The masked text of a redact-only verdict. A windowed verdict carries no `redacted`
/// (`scan_windows` leaves it empty when windows were used), so the mask is applied through
/// [`mask_spans`] over the union of its hits.
fn masked_text(text: &str, v: &Verdict, policy: &Policy) -> String {
    if !v.redacted.is_empty() {
        return v.redacted.clone();
    }
    mask_spans(text, &v.hits, &policy.file.defaults.mask)
}

/// Replace every redact hit of `text` by the mask. Overlapping or touching hits (two windows
/// matched the same run, a `key=value` projection beside a direct hit) are unioned into one
/// covered span first, so no matched character survives between them (RD1-6); the span keeps
/// the rule id of its first hit for the mask label. Hits must be sorted by start.
pub(crate) fn mask_spans(text: &str, hits: &[Hit], mask: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    let mut covered: Option<(usize, usize, &str)> = None;
    let flush = |span: (usize, usize, &str), out: &mut String, pos: &mut usize| {
        let (start, end, rule) = span;
        let start = start.max(*pos).min(text.len());
        let end = end.min(text.len());
        if end > start {
            out.push_str(&text[*pos..start]);
            out.push_str(&mask.replace("{rule}", rule));
            *pos = end;
        }
    };
    for h in hits.iter().filter(|h| h.action == Action::Redact) {
        match covered {
            Some((s, e, r)) if h.start <= e => covered = Some((s, e.max(h.end), r)),
            Some(span) => {
                flush(span, &mut out, &mut pos);
                covered = Some((h.start, h.end, h.rule.as_str()));
            }
            None => covered = Some((h.start, h.end, h.rule.as_str())),
        }
    }
    if let Some(span) = covered {
        flush(span, &mut out, &mut pos);
    }
    out.push_str(&text[pos..]);
    out
}
