//! Policy-gated run-local gazetteer. Never reads SQL or writes ABOUT edges.
use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use kaban_core::config::Gazetteer;
use kaban_core::{
    BatchAssertion, BatchSubject, ClaimClass, Fixed, GraphStore, Layer, Method, Node, NodeId, Op,
    Rank, SNAPSHOT_SYMBOL_PAGE, SymbolPage, WorkspaceId, WriteBatch,
};
use kaban_policy::{Policy, norm_term, norm_with_offsets};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// Sources contributing dictionary patterns (counts only in diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictSource {
    /// Record titles.
    Titles,
    /// Explicit aliases.
    Aliases,
    /// Record tags.
    Tags,
    /// Declared path scopes.
    PathPrefixes,
    /// Code reference paths.
    CodeRefPaths,
    /// Symbols paged from an admitted snapshot.
    V4Symbols,
    /// Explicit record identifiers.
    Ids,
}

/// An attached admitted snapshot. No URI or connection crosses into ingest.
#[derive(Debug, Clone)]
pub struct SnapshotSource {
    /// Canonical snapshot commit.
    pub commit: String,
}

#[derive(Debug, Clone)]
struct Pattern {
    normalized: String,
    targets: Vec<(NodeId, Method)>,
    ambiguous: bool,
    surfaces: Vec<String>,
}

/// Run-local, content-gated matcher. Debug never reveals pattern contents.
pub struct Dictionary {
    patterns: Vec<Pattern>,
    automaton: AhoCorasick,
    /// Retained automaton bytes (not process RSS).
    pub memory_usage: usize,
    /// Same-process sampled build RSS increase on Linux, in bytes; unavailable (zero) elsewhere.
    pub build_peak_delta: u64,
    /// Linux same-process high-water increase; unavailable (zero) elsewhere.
    pub build_hwm_delta: u64,
    /// Sources used by this dictionary.
    pub sources: Vec<DictSource>,
    fingerprint: String,
}

impl std::fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dictionary")
            .field("patterns", &self.patterns.len())
            .field("memory_usage", &self.memory_usage)
            .finish_non_exhaustive()
    }
}

/// One byte-bounded candidate on sanitized chunk text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// Existing chunk id, assigned after the gate.
    pub chunk: NodeId,
    /// Existing gated dictionary target.
    pub target: NodeId,
    /// Closed core method spelling.
    pub method: Method,
    /// Start in original sanitized UTF-8 chunk bytes.
    pub offset: u64,
    /// Original byte length.
    pub len: u64,
    /// Fixed confidence, divided across the candidate targets.
    pub score: Fixed,
    /// Candidate only, never a plain edge.
    pub ambiguous: bool,
}

fn admitted(policy: &Policy, text: &str) -> Option<String> {
    let verdict = policy.scan(text);
    if verdict.rejected {
        None
    } else if verdict.hits.is_empty() {
        Some(text.to_owned())
    } else {
        Some(verdict.redacted)
    }
}

fn rejected_value(policy: &Policy, value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => policy.scan(text).rejected,
        serde_json::Value::Array(values) => {
            values.iter().any(|value| rejected_value(policy, value))
        }
        serde_json::Value::Object(values) => {
            values.values().any(|value| rejected_value(policy, value))
        }
        _ => false,
    }
}

fn fields(node: &Node) -> Vec<(&str, Method, DictSource)> {
    let mut result = Vec::new();
    for title in [
        node.props.get("title").and_then(serde_json::Value::as_str),
        node.summary.as_deref(),
        node.text.lines().next(),
    ]
    .into_iter()
    .flatten()
    {
        result.push((title, Method::Exact, DictSource::Titles));
    }
    for (key, method, source) in [
        ("aliases", Method::Alias, DictSource::Aliases),
        ("tags", Method::Tag, DictSource::Tags),
    ] {
        if let Some(values) = node.props.get(key).and_then(serde_json::Value::as_array) {
            result.extend(
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(|s| (s, method, source)),
            );
        }
    }
    result.extend(
        node.tags
            .iter()
            .map(|tag| (tag.as_str(), Method::Tag, DictSource::Tags)),
    );
    result.extend(node.path_scope.iter().map(|path| {
        (
            path.trim_end_matches('*'),
            Method::Path,
            DictSource::PathPrefixes,
        )
    }));
    if node.kind == "CodeRef" {
        for key in ["relative_path", "name_path"] {
            if let Some(path) = node.props.get(key).and_then(serde_json::Value::as_str) {
                result.push((path, Method::Path, DictSource::CodeRefPaths));
            }
        }
    }
    result.push((&node.id, Method::Id, DictSource::Ids));
    result
}

/// Build dictionary from workspace-scoped role methods only, gating every source before normalization.
///
/// # Errors
/// Store read errors or an automaton construction failure (fixed content-free text).
pub fn build_dictionary(
    store: &dyn GraphStore,
    workspace: &WorkspaceId,
    snapshot: Option<&SnapshotSource>,
    policy: &Policy,
    cfg: &Gazetteer,
) -> Result<Dictionary, crate::IngestError> {
    let mut patterns: BTreeMap<String, BTreeMap<String, (NodeId, Method, String)>> =
        BTreeMap::new();
    let mut sources = BTreeSet::new();
    let mut symbols: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
    let mut document_count = 0_u32;
    let mut document_frequency = BTreeMap::<String, u32>::new();
    let mut insert = |raw: &str, target: &NodeId, method: Method, source: DictSource| {
        let Some(safe) = admitted(policy, raw) else {
            return;
        };
        let normalized = norm_term(&safe, true);
        if normalized.len() < cfg.min_pattern_len as usize {
            return;
        }
        // Short stopwords are not entities, even when a record has that title.
        if normalized.chars().count() <= cfg.common_word_max_len as usize
            && ["the", "and", "for", "with", "from", "this", "that", "into"]
                .contains(&normalized.as_str())
        {
            return;
        }
        sources.insert(source);
        patterns
            .entry(normalized)
            .or_default()
            .entry(target.to_string())
            .or_insert_with(|| (target.clone(), method, safe));
    };
    for kind in store
        .schema()?
        .into_iter()
        .filter(|kind| kind.kind == "label")
    {
        let mut offset = 0;
        loop {
            let page = store.list_nodes(&kind.name, workspace.as_str(), offset, 500)?;
            let count = page.len();
            for node in page {
                // Reject-hidden records cannot contribute even a clean alias/id.
                if policy.scan(&node.text).rejected
                    || rejected_value(policy, &node.props)
                    || node
                        .tags
                        .iter()
                        .chain(&node.path_scope)
                        .any(|text| policy.scan(text).rejected)
                    || node
                        .summary
                        .as_ref()
                        .is_some_and(|s| policy.scan(s).rejected)
                {
                    continue;
                }
                let Ok(id) = NodeId::parse(&node.id) else {
                    continue;
                };
                // L0 derived rows are evidence, not title targets (avoids unstable self dictionaries).
                if matches!(node.kind.as_str(), "Chunk" | "Section") {
                    continue;
                }
                if node.kind == "Document"
                    && node
                        .props
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|status| matches!(status, "ok" | "partial"))
                {
                    let mut terms = BTreeSet::new();
                    if let Some(chunks) = node
                        .props
                        .get("chunk_ids")
                        .and_then(serde_json::Value::as_array)
                    {
                        for chunk in chunks
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .filter_map(|id| NodeId::parse(id).ok())
                        {
                            if let Some(chunk) = store.get(&chunk, false)?
                                && chunk.workspace_id == workspace.as_str()
                                && chunk.kind == "Chunk"
                                && let Some(safe) = admitted(policy, &chunk.text)
                            {
                                terms.extend(
                                    norm_term(&safe, true)
                                        .split_whitespace()
                                        .filter(|term| term.chars().all(char::is_alphabetic))
                                        .map(str::to_owned),
                                );
                            }
                        }
                    }
                    document_count = document_count.saturating_add(1);
                    for term in terms {
                        let frequency = document_frequency.entry(term).or_default();
                        *frequency = frequency.saturating_add(1);
                    }
                }
                for (raw, method, source) in fields(&node) {
                    insert(raw, &id, method, source);
                }
                if node.kind == "CodeRef"
                    && let Some(key) = node.props.get("symbol").and_then(serde_json::Value::as_str)
                {
                    symbols.entry(key.to_owned()).or_default().push(id);
                }
            }
            if count < 500 {
                break;
            }
            offset += count;
        }
    }
    if cfg.v4_symbols
        && let Some(snapshot) = snapshot
    {
        let mut after_id = 0;
        loop {
            let (page, next) = store.snapshot_symbols(
                &snapshot.commit,
                SymbolPage {
                    after_id,
                    limit: SNAPSHOT_SYMBOL_PAGE,
                },
            )?;
            for symbol in page {
                if let Some(targets) = symbols.get(&symbol.key) {
                    for target in targets {
                        for raw in [
                            &symbol.name,
                            &symbol.label,
                            &symbol.path,
                            &symbol.ident_terms,
                        ] {
                            insert(raw, target, Method::Symbol, DictSource::V4Symbols);
                        }
                    }
                }
            }
            let Some(next) = next else {
                break;
            };
            after_id = next;
        }
    }
    finish_dictionary(patterns, sources, document_count, document_frequency, cfg)
}

type PatternMap = BTreeMap<String, BTreeMap<String, (NodeId, Method, String)>>;

/// SPIKE S5 (extracted from `build_dictionary`, no behaviour change): the gate, automaton and
/// fingerprint half, shared with [`dictionary_from_terms`].
fn finish_dictionary(
    patterns: PatternMap,
    sources: BTreeSet<DictSource>,
    document_count: u32,
    document_frequency: BTreeMap<String, u32>,
    cfg: &Gazetteer,
) -> Result<Dictionary, crate::IngestError> {
    let patterns: Vec<Pattern> = patterns
        .into_iter()
        .filter(|(normalized, targets)| {
            let lowercase_word = targets
                .values()
                .all(|(_, _, surface)| surface.chars().all(char::is_lowercase));
            !(lowercase_word
                && document_count > 0
                && f64::from(document_frequency.get(normalized).copied().unwrap_or(0))
                    / f64::from(document_count)
                    > cfg.corpus_freq_cutoff)
        })
        .map(|(normalized, targets)| {
            let ambiguous = targets.len() > cfg.max_targets as usize;
            let (targets, surfaces) = targets
                .into_values()
                .map(|(id, method, surface)| ((id, method), surface))
                .unzip();
            Pattern {
                normalized,
                targets,
                ambiguous,
                surfaces,
            }
        })
        .collect();
    #[cfg(target_os = "linux")]
    let sampler = BuildSampler::start();
    let automaton = AhoCorasick::builder()
        .match_kind(MatchKind::LeftmostLongest)
        .kind(Some(AhoCorasickKind::ContiguousNFA))
        .prefilter(true)
        .build(patterns.iter().map(|p| &p.normalized));
    #[cfg(target_os = "linux")]
    let (build_peak_delta, build_hwm_delta) = sampler.finish();
    // Existing integer telemetry uses zero for unavailable observations. Retained
    // automaton bytes remain measured and enforce the configured budget everywhere.
    #[cfg(not(target_os = "linux"))]
    let (build_peak_delta, build_hwm_delta) = (0, 0);
    let automaton =
        automaton.map_err(|_| crate::IngestError::Config("gazetteer build failed".into()))?;
    let memory_usage = automaton.memory_usage();
    let mut digest = blake3::Hasher::new();
    for pattern in &patterns {
        digest.update(pattern.normalized.as_bytes());
        digest.update(&[0]);
        for (id, method) in &pattern.targets {
            digest.update(id.as_str().as_bytes());
            digest.update(method.as_str().as_bytes());
        }
    }
    Ok(Dictionary {
        patterns,
        automaton,
        memory_usage,
        build_peak_delta,
        build_hwm_delta,
        sources: sources.into_iter().collect(),
        fingerprint: digest.finalize().to_hex().to_string(),
    })
}

/// SPIKE S5: a dictionary straight from `(surface, target)` pairs (symbol names -> symbol node
/// ids), no store read. Same admission, stopword, min-length and ambiguity rules as
/// [`build_dictionary`] because it shares [`finish_dictionary`].
///
/// # Errors
/// An automaton construction failure.
pub fn dictionary_from_terms(
    policy: &Policy,
    cfg: &Gazetteer,
    terms: impl IntoIterator<Item = (String, NodeId)>,
) -> Result<Dictionary, crate::IngestError> {
    let mut patterns = PatternMap::new();
    for (raw, target) in terms {
        let Some(safe) = admitted(policy, &raw) else { continue };
        let normalized = norm_term(&safe, true);
        if normalized.len() < cfg.min_pattern_len as usize { continue; }
        patterns.entry(normalized).or_default().entry(target.to_string()).or_insert_with(|| (target.clone(), Method::Symbol, safe));
    }
    finish_dictionary(patterns, BTreeSet::from([DictSource::V4Symbols]), 0, BTreeMap::new(), cfg)
}

#[cfg(target_os = "linux")]
fn process_memory() -> (u64, u64) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0);
    };
    let bytes = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1024)
    };
    (bytes("VmRSS:"), bytes("VmHWM:"))
}

#[cfg(target_os = "linux")]
struct BuildSampler {
    before: (u64, u64),
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<u64>>,
}

#[cfg(target_os = "linux")]
impl BuildSampler {
    fn start() -> Self {
        let before = process_memory();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("gazetteer-rss".into())
            .spawn(move || {
                let mut peak = before.0;
                while !flag.load(std::sync::atomic::Ordering::Acquire) {
                    peak = peak.max(process_memory().0);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                peak.max(process_memory().0)
            })
            .ok();
        Self {
            before,
            stop,
            worker,
        }
    }

    fn finish(mut self) -> (u64, u64) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let after = process_memory();
        let peak = self
            .worker
            .take()
            .and_then(|worker| worker.join().ok())
            .unwrap_or(after.0);
        (
            peak.max(after.0).saturating_sub(self.before.0),
            after.1.saturating_sub(self.before.1),
        )
    }
}

impl Dictionary {
    /// Gated dictionary identity used by the document skip key.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub(crate) fn external_fingerprint(&self, document: &NodeId) -> String {
        let mut digest = blake3::Hasher::new();
        for pattern in &self.patterns {
            for (id, method) in &pattern.targets {
                // Only this document's targets are self-induced. Other documents of the
                // same walk are real dependencies, including ones created on the first pass.
                if id != document {
                    digest.update(&[u8::from(pattern.ambiguous)]);
                    for field in [pattern.normalized.as_str(), id.as_str(), method.as_str()] {
                        digest.update(&(field.len() as u64).to_le_bytes());
                        digest.update(field.as_bytes());
                    }
                }
            }
        }
        digest.finalize().to_hex().to_string()
    }
}

/// Match already-gated text whose chunk id was assigned after gating.
#[must_use]
pub fn find_mentions(dict: &Dictionary, chunk: &NodeId, text: &str) -> Vec<Mention> {
    let view = norm_with_offsets(text, true);
    let normalized = view.text();
    let mut mentions = Vec::new();
    let mut repeats = BTreeMap::<usize, u32>::new();
    for hit in dict.automaton.find_iter(normalized) {
        let pattern = &dict.patterns[hit.pattern().as_usize()];
        let boundary = |ch: Option<char>| ch.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if !boundary(normalized[..hit.start()].chars().next_back())
            || !boundary(normalized[hit.end()..].chars().next())
        {
            continue;
        }
        let Some(span) = view.raw_span(hit.start()..hit.end()) else {
            continue;
        };
        let repeat = repeats.entry(hit.pattern().as_usize()).or_default();
        *repeat += 1;
        for ((target, method), surface) in pattern.targets.iter().zip(&pattern.surfaces) {
            if target == chunk {
                continue;
            }
            let base = match method {
                Method::Id => 10000,
                Method::Symbol => 6000,
                Method::Alias => 8000,
                _ => 9000,
            };
            let specificity =
                if !surface.chars().any(char::is_uppercase) && !pattern.normalized.contains(' ') {
                    1
                } else {
                    2
                };
            let score = (base * specificity / 2 + repeat.saturating_sub(1).min(4) * 100).min(10000)
                / u32::try_from(pattern.targets.len())
                    .unwrap_or(u32::MAX)
                    .max(1);
            let candidate = Mention {
                chunk: chunk.clone(),
                target: target.clone(),
                method: *method,
                offset: span.start as u64,
                len: span.len() as u64,
                score: Fixed::new(score).unwrap_or(Fixed::ZERO),
                ambiguous: pattern.ambiguous,
            };
            // Similarity only orders already-exact candidates; it never generates a fuzzy hit.
            let similarity = strsim::normalized_levenshtein(&text[span.clone()], surface);
            mentions.push((candidate, similarity));
        }
    }
    mentions.sort_by(|(left, ls), (right, rs)| {
        left.offset
            .cmp(&right.offset)
            .then_with(|| right.score.cmp(&left.score))
            .then_with(|| rs.total_cmp(ls))
            .then_with(|| left.target.cmp(&right.target))
    });
    mentions.into_iter().map(|(mention, _)| mention).collect()
}

/// One claim per `(chunk, target)`: the strongest occurrence, ties broken by the first original
/// byte position; ambiguous candidates never become edges.
fn strongest(mentions: &[Mention]) -> Vec<&Mention> {
    let mut selected = BTreeMap::new();
    for mention in mentions.iter().filter(|m| !m.ambiguous) {
        selected
            .entry((&mention.chunk, &mention.target))
            .and_modify(|current: &mut &Mention| {
                if mention.score > current.score
                    || (mention.score == current.score && mention.offset < current.offset)
                {
                    *current = mention;
                }
            })
            .or_insert(mention);
    }
    selected.into_values().collect()
}

/// SPIKE S5: the edge half of [`append_mentions`] alone (what survives dropping governance,
/// 07 section 7): one `MENTIONS` `Op::UpsertEdge` per selected mention, no assertion.
#[must_use]
pub fn mention_edge_ops(mentions: &[Mention]) -> Vec<Op> {
    strongest(mentions)
        .into_iter()
        .map(|mention| Op::UpsertEdge { src: mention.chunk.clone(), dst: mention.target.clone(), rel: "MENTIONS".into(), props: Some(json!({"owner": "kaban-ingest", "method": mention.method.as_str(), "offset": mention.offset, "len": mention.len, "score": mention.score.0})) })
        .collect()
}

pub(crate) fn append_mentions(batch: &mut WriteBatch, mentions: &[Mention], source_id: &str) {
    for mention in strongest(mentions) {
        let value = json!({"method": mention.method.as_str(), "offset": mention.offset, "len": mention.len, "score": mention.score.0});
        batch.assertions.push(BatchAssertion {
            subject: BatchSubject::Edge {
                src: mention.chunk.clone(),
                rel: "MENTIONS".into(),
                dst: mention.target.clone(),
            },
            claim_class: ClaimClass::Edge,
            predicate: None,
            object: None,
            value: value.clone(),
            layer: Layer::Gazetteer,
            method: mention.method,
            extractor_id: None,
            extractor_version: Some("1".into()),
            source_id: Some(source_id.to_owned()),
            lineage_id: format!("gazetteer:{source_id}"),
            raw_score: mention.score,
            rank: Rank::Normal,
            rank_reason: None,
            calibration_version: 0,
            supersedes: None,
        });
        // Name the edge in Owned diff; immutable claim reconciliation remains the authority.
        batch.ops.push(Op::UpsertEdge { src: mention.chunk.clone(), dst: mention.target.clone(), rel: "MENTIONS".into(), props: Some(json!({"owner": "kaban-ingest", "method": mention.method.as_str(), "offset": mention.offset, "len": mention.len, "score": mention.score.0})) });
    }
}
