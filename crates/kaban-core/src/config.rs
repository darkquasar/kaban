//! The one `EngineConfig` (engine-spec §15.1 defaults register; engine-plan E1.1, SO-11): every
//! `[section] key` of the register is a field of the matching section struct, its `Default` is
//! the register's value, and the YAML path is `section.key` in `~/.kaban/config.yaml`
//! (`KABAN_HOME`). Fields are plain serde data (`f64`, `u32`, `bool`, `String`, `BTreeMap`,
//! `Option`); the typed forms (`Fixed`, `Fusion`, `ModelIdentity`, `ErPriors`, `LinkMode`) are
//! built once at the consuming seam by the task that owns them, so this module depends on
//! nothing defined later. Unknown keys fail at every depth with the field path (never the value);
//! an opt-in budget of 0 fails [`ConfigError::Range`].
//!
//! `[index]` carries, since v4 CG4.2, the `kaban-index` keys
//! (`isolation`, `container_image`, `container_cli`, `deadline_secs`, `quota.*`,
//! `policy_scan`);
//! `kaban-index` reads the same file through this loader.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canon::canonical_json;
use crate::validator::ValidatorLimits;

/// `[reconcile]` (EN-F12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Reconcile {
    /// Auto-accept threshold of a `type` winner (becomes `Fixed` at the E2.1 seam).
    pub tau_type: f64,
    /// Auto-accept threshold of a `canonical_id` winner.
    pub tau_id: f64,
    /// A `priors.yaml` replacing the EN-F12 table; unset by default.
    pub priors: Option<PathBuf>,
}

impl Default for Reconcile {
    fn default() -> Self {
        Self {
            tau_type: 0.3,
            tau_id: 0.4,
            priors: None,
        }
    }
}

/// `[discovered] promote_when.*` (EN-F5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PromoteWhen {
    /// Minimum occurrences of a candidate kind.
    pub min_count: u32,
    /// Minimum distinct sources.
    pub min_sources: u32,
    /// Minimum mean extractor score.
    pub min_mean_score: f64,
}

impl Default for PromoteWhen {
    fn default() -> Self {
        Self {
            min_count: 20,
            min_sources: 3,
            min_mean_score: 0.7,
        }
    }
}

/// `[discovered]` (EN-F5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Discovered {
    /// Candidate kinds per workspace.
    pub cap: u32,
    /// Days before an unpromoted candidate is pruned.
    pub prune_after_days: u32,
    /// Promotion suggestion thresholds.
    pub promote_when: PromoteWhen,
}

impl Default for Discovered {
    fn default() -> Self {
        Self {
            cap: 500,
            prune_after_days: 90,
            promote_when: PromoteWhen::default(),
        }
    }
}

/// `[ontology]` (EN-F3a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OntologyCfg {
    /// `RegexBuilder::size_limit` of a compiled `pattern`, in KiB.
    pub pattern_size_limit_kib: u32,
    /// Ship the `maxLength`/`pattern` tightenings of the golden schemas.
    pub golden_strict: bool,
}

impl Default for OntologyCfg {
    fn default() -> Self {
        Self {
            pattern_size_limit_kib: 1024,
            golden_strict: false,
        }
    }
}

impl From<&OntologyCfg> for ValidatorLimits {
    fn from(cfg: &OntologyCfg) -> Self {
        ValidatorLimits {
            pattern_size_limit_kib: cfg.pattern_size_limit_kib,
        }
    }
}

/// `[gazetteer]` (EN-F25, EN-N6, OD-AI-6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Gazetteer {
    /// Shortest dictionary pattern.
    pub min_pattern_len: u32,
    /// Common-word exclusion length.
    pub common_word_max_len: u32,
    /// Corpus frequency above which a term is excluded.
    pub corpus_freq_cutoff: f64,
    /// Maximum targets per surface form before it counts as ambiguous.
    pub max_targets: u32,
    /// Automaton build budget in MiB (opt-in budget; 0 is out of range).
    pub max_automaton_mb: u32,
    /// Per-chunk match time budget in ms.
    pub max_chunk_ms: u32,
    /// Feed v4 symbols into the dictionary when a snapshot is attached.
    pub v4_symbols: bool,
}

impl Default for Gazetteer {
    fn default() -> Self {
        Self {
            min_pattern_len: 3,
            common_word_max_len: 4,
            corpus_freq_cutoff: 0.05,
            max_targets: 5,
            max_automaton_mb: 25,
            max_chunk_ms: 1,
            v4_symbols: true,
        }
    }
}

/// `[embed]` (EN-F26, EN-F26a).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Embed {
    /// Reserved external model identity; nonempty configuration is unavailable in this release.
    pub model: Option<BTreeMap<String, String>>,
    /// Vector storage type (`int8` | `f32`).
    pub dtype: String,
    /// Mutual kNN neighbourhood size.
    pub knn_k: u32,
    /// Per-kind similarity threshold; the `default` entry applies to every kind without one.
    pub tau: BTreeMap<String, f64>,
    /// Rows per scan window.
    pub scan_window_rows: u32,
    /// RSS delta budget of one query scan in MiB (opt-in budget; 0 is out of range).
    pub max_scan_rss_mb: u32,
    /// Minimum int8/f32 top-k overlap before int8 storage is kept.
    pub min_topk_overlap: f64,
}

impl Default for Embed {
    fn default() -> Self {
        Self {
            model: None,
            dtype: "int8".to_owned(),
            knn_k: 5,
            tau: BTreeMap::from([("default".to_owned(), 0.80)]),
            scan_window_rows: 4096,
            max_scan_rss_mb: 32,
            min_topk_overlap: 0.9,
        }
    }
}

/// `[l3]` (EN-F27a).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct L3 {
    /// Holdout F1 an extractor must reach before `--with` is accepted.
    pub holdout_min_f1: f64,
    /// Minimum judged spans in the holdout.
    pub holdout_min_spans: u32,
}

impl Default for L3 {
    fn default() -> Self {
        Self {
            holdout_min_f1: 0.50,
            holdout_min_spans: 200,
        }
    }
}

/// `[recall]` (EN-F24, EN-F28a, EN-F29).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Recall {
    /// Fusion rule (`rrf(60)` | `cc(0.7)`; becomes `Fusion` at the E3.4 seam).
    pub fusion: String,
    /// RRF constant.
    pub rrf_k: u32,
    /// Judged queries needed before `cc` may be chosen.
    pub judged_min_queries: u32,
    /// BM25F column weights by column name.
    pub column_weights: BTreeMap<String, f64>,
}

impl Default for Recall {
    fn default() -> Self {
        Self {
            fusion: "rrf(60)".to_owned(),
            rrf_k: 60,
            judged_min_queries: 100,
            column_weights: BTreeMap::from([
                ("text".to_owned(), 1.0),
                ("summary".to_owned(), 1.5),
                ("tags".to_owned(), 1.0),
                ("ident_terms".to_owned(), 2.0),
            ]),
        }
    }
}

impl Recall {
    /// Resolve the compatible RRF settings; convex combination is reserved.
    ///
    /// # Errors
    /// Malformed/conflicting fusion settings or unavailable convex combination.
    pub fn effective_rrf_k(&self) -> Result<u32, ConfigError> {
        if self.fusion.starts_with("cc(") && self.fusion.ends_with(')') {
            return Err(ConfigError::NotAvailable {
                path: "recall.fusion",
            });
        }
        let embedded = self
            .fusion
            .strip_prefix("rrf(")
            .and_then(|v| v.strip_suffix(')'))
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| ConfigError::Range {
                path: "recall.fusion".into(),
            })?;
        if self.rrf_k == 0 || (embedded != 60 && self.rrf_k != 60 && embedded != self.rrf_k) {
            return Err(ConfigError::Range {
                path: "recall.rrf_k".into(),
            });
        }
        for (column, weight) in &self.column_weights {
            if !["text", "summary", "tags", "ident_terms"].contains(&column.as_str())
                || !weight.is_finite()
                || *weight < 0.0
            {
                return Err(ConfigError::Range {
                    path: "recall.column_weights".into(),
                });
            }
        }
        Ok(if embedded == 60 { self.rrf_k } else { embedded })
    }
}

/// `[ask]` (EN-F47).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Ask {
    /// Default template depth.
    pub template_depth: u8,
    /// Structural cap on the depth.
    pub template_depth_max: u8,
}

impl Default for Ask {
    fn default() -> Self {
        Self {
            template_depth: 1,
            template_depth_max: 3,
        }
    }
}

/// `[er]` (EN-F51; becomes `ErPriors` at the E2.10 seam).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Er {
    /// Fellegi-Sunter `m` probability per field.
    pub m: BTreeMap<String, f64>,
    /// Fellegi-Sunter `u` probability per field.
    pub u: BTreeMap<String, f64>,
    /// Match decision threshold.
    pub match_threshold: f64,
    /// Possible-match threshold.
    pub possible_threshold: f64,
}

impl Default for Er {
    fn default() -> Self {
        Self {
            m: BTreeMap::from([
                ("title".to_owned(), 0.9),
                ("aliases".to_owned(), 0.8),
                ("kind".to_owned(), 0.95),
                ("tags".to_owned(), 0.6),
                ("path_scope".to_owned(), 0.7),
                ("anchors".to_owned(), 0.95),
                ("summary".to_owned(), 0.5),
            ]),
            u: BTreeMap::from([
                ("title".to_owned(), 0.01),
                ("aliases".to_owned(), 0.02),
                ("kind".to_owned(), 0.3),
                ("tags".to_owned(), 0.1),
                ("path_scope".to_owned(), 0.05),
                ("anchors".to_owned(), 0.001),
                ("summary".to_owned(), 0.1),
            ]),
            match_threshold: 8.0,
            possible_threshold: 3.0,
        }
    }
}

/// `[ingest]` (EN-F37, EN-F38, EN-F27a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Ingest {
    /// Records per `finish`.
    pub finish_cap: u32,
    /// Job retention in days.
    pub retention_days: u32,
    /// Fenced worker lease duration in seconds.
    pub claim_ttl_s: u32,
    /// Maximum acquisition attempts per unchanged unmarked source.
    pub claim_max_attempts: u32,
    /// Maximum path-ordered pending Documents considered by one claim.
    pub claim_scan_max: u32,
    /// Serialized metadata allowance per record in KiB.
    pub metadata_allowance_kib: u32,
    /// Page LRU entries.
    pub lru_entries: u32,
    /// Page LRU bytes in MiB (opt-in budget; 0 is out of range).
    pub lru_mib: u32,
    /// Default extractor (`--with`); unset by default.
    pub default_with: Option<String>,
}

impl Default for Ingest {
    fn default() -> Self {
        Self {
            finish_cap: 10_000,
            retention_days: 7,
            claim_ttl_s: 300,
            claim_max_attempts: 3,
            claim_scan_max: 1000,
            metadata_allowance_kib: 4,
            lru_entries: 4,
            lru_mib: 8,
            default_with: None,
        }
    }
}

/// `[pipeline]` (EN-F15a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Pipeline {
    /// Rows retired per transaction.
    pub retire_batch_rows: u32,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            retire_batch_rows: 1000,
        }
    }
}

/// `[index]`: the v4 `kaban-index` keys of
/// stage1-spec §10 / stage1-plan CG4.2 (OWN-29 isolation policy, OWN-54/SD-11 budgets with a
/// user override, RV-8 `quota.tmp_bytes`). Every key spells its register default; `doctor`
/// lists the differing ones through [`EngineConfig::effective_overrides`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Index {
    /// `preferred` (strongest passing tier, else `none` with a warning) | `required` (exit 8
    /// without an isolating tier) | `off` (`none` without probing); `--isolation` overrides.
    pub isolation: Isolation,
    /// `<ref>@sha256:<digest>`, required for the `container` tier (G1-S5.1).
    pub container_image: Option<String>,
    /// Absolute path of the container CLI; else the fixed candidate list (G1-S5.2).
    pub container_cli: Option<PathBuf>,
    /// Hard deadline per indexer run in seconds (G1-S5.4; default 600).
    pub deadline_secs: u64,
    /// The scratch quotas (G1-S5.6).
    pub quota: IndexQuota,
    /// Whether the content policy scans the code-graph output (DF-3, owner decision). Off by
    /// default: the snapshot stays on this machine. When on, a hit is masked and counted,
    /// never a refusal.
    pub policy_scan: bool,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            isolation: Isolation::Preferred,
            container_image: None,
            container_cli: None,
            deadline_secs: 600,
            quota: IndexQuota::default(),
            policy_scan: false,
        }
    }
}

/// `index.isolation` (OWN-29): the closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// The strongest tier whose probe passed, else `none` with the warning `isolation_none`.
    #[default]
    Preferred,
    /// Refuse (exit 8, fixed text) when neither `namespaces` nor `container` is available.
    Required,
    /// `none` without probing.
    Off,
}

impl Isolation {
    /// The closed set as `doctor` and the error text list it.
    pub const ALL: [&'static str; 3] = ["preferred", "required", "off"];

    /// The YAML spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preferred => "preferred",
            Self::Required => "required",
            Self::Off => "off",
        }
    }
}

/// `index.quota.*` (G1-S5.6, RV-8): bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexQuota {
    /// Per file the indexer writes (default 1 GiB).
    pub file_bytes: u64,
    /// The whole scratch directory (default 4 GiB).
    pub run_dir_bytes: u64,
    /// Bound of the container `/tmp` tmpfs, charged against `run_dir_bytes` (default 512 MiB).
    pub tmp_bytes: u64,
}

impl Default for IndexQuota {
    fn default() -> Self {
        Self {
            file_bytes: 1 << 30,
            run_dir_bytes: 4 << 30,
            tmp_bytes: 512 << 20,
        }
    }
}

/// `[link]` (EN-F31, OD-AI-4; becomes `LinkMode` at the E3.6 seam).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Link {
    /// Default linking mode (`none` | `gazetteer`).
    pub default: String,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            default: "none".to_owned(),
        }
    }
}

/// `[briefing]` (EN-F49, OD-E-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Briefing {
    /// Candidate kinds listed in the briefing.
    pub max_candidates: u32,
}

impl Default for Briefing {
    fn default() -> Self {
        Self { max_candidates: 5 }
    }
}

/// `[budget]` (EN-N3; both opt-in budgets, 0 is out of range).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Budget {
    /// L0 RSS budget in MiB.
    pub l0_rss_mb: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self { l0_rss_mb: 64 }
    }
}

/// `[bench]` (EN-N2, EN-N7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Bench {
    /// Samples per paired series.
    pub min_samples: u32,
    /// Regression threshold in percent.
    pub why_regression_pct: u32,
}

impl Default for Bench {
    fn default() -> Self {
        Self {
            min_samples: 30,
            why_regression_pct: 10,
        }
    }
}

/// `[why]` (EN-F10 prefilter, EN-N7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Why {
    /// Anchor prefilter on.
    pub anchor_prefilter: bool,
}

impl Default for Why {
    fn default() -> Self {
        Self {
            anchor_prefilter: true,
        }
    }
}

/// The engine configuration: every §15.1 section with its register defaults.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineConfig {
    /// `[reconcile]`
    pub reconcile: Reconcile,
    /// `[discovered]`
    pub discovered: Discovered,
    /// `[ontology]`
    pub ontology: OntologyCfg,
    /// `[gazetteer]`
    pub gazetteer: Gazetteer,
    /// `[embed]`
    pub embed: Embed,
    /// `[l3]`
    pub l3: L3,
    /// `[recall]`
    pub recall: Recall,
    /// `[ask]`
    pub ask: Ask,
    /// `[er]`
    pub er: Er,
    /// `[ingest]`
    pub ingest: Ingest,
    /// `[pipeline]`
    pub pipeline: Pipeline,
    /// `[index]`
    pub index: Index,
    /// `[link]`
    pub link: Link,
    /// `[briefing]`
    pub briefing: Briefing,
    /// `[budget]`
    pub budget: Budget,
    /// `[bench]`
    pub bench: Bench,
    /// `[why]`
    pub why: Why,
}

/// Why a configuration file failed to load. Content-free: an I/O class, a field path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A configured capability is reserved for a later release.
    #[error("not_available_in_this_release")]
    NotAvailable {
        /// Fixed reserved configuration path, never a supplied value.
        path: &'static str,
    },
    /// The file exists but could not be read.
    #[error("config file unreadable ({class})")]
    Io {
        /// The `io::ErrorKind` as a fixed name.
        class: &'static str,
    },
    /// The file is not valid YAML for the struct: an unknown key, a wrong type or a malformed
    /// document. `path` is the dotted field path (`ontology.pattern_size_limit_kb`), `""` for
    /// the document itself; a value is never included.
    #[error("config key invalid: {path}")]
    Parse {
        /// Dotted field path.
        path: String,
    },
    /// An opt-in budget set to 0.
    #[error("config value out of range: {path}")]
    Range {
        /// Dotted field path.
        path: String,
    },
}

/// The opt-in budgets (engine-plan §4 memory policy, OWN-54/SD-11): a positive integer each.
const BUDGETS: [&str; 11] = [
    "ingest.claim_ttl_s",
    "ingest.claim_max_attempts",
    "ingest.claim_scan_max",
    "budget.l0_rss_mb",
    "gazetteer.max_automaton_mb",
    "embed.max_scan_rss_mb",
    "ingest.lru_mib",
    "index.deadline_secs",
    "index.quota.file_bytes",
    "index.quota.run_dir_bytes",
    "index.quota.tmp_bytes",
];

/// Load `path`: an absent file is the register `Default`; any other read failure is
/// [`ConfigError::Io`]; an unknown key at any depth or a wrong type is [`ConfigError::Parse`]
/// with the field path; an opt-in budget of 0 is [`ConfigError::Range`].
///
/// # Errors
/// As listed above.
pub fn load(path: &Path) -> Result<EngineConfig, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(EngineConfig::default()),
        Err(e) => {
            return Err(ConfigError::Io {
                class: io_class(e.kind()),
            });
        }
    };
    parse(&text)
}

/// Parse the YAML text of a configuration file (the file-less half of [`load`]).
fn parse(text: &str) -> Result<EngineConfig, ConfigError> {
    // one generic parse into JSON data first: the field paths of the errors below are found by
    // narrowing on this tree, never by quoting the YAML
    let data: Value = yaml_serde::from_str(text).map_err(|_| ConfigError::Parse {
        path: String::new(),
    })?;
    if data.is_null() {
        return Ok(EngineConfig::default());
    }
    let cfg: EngineConfig = match serde_json::from_value(data.clone()) {
        Ok(cfg) => cfg,
        Err(_) => {
            return Err(ConfigError::Parse {
                path: locate(&data),
            });
        }
    };
    if cfg
        .ingest
        .default_with
        .as_ref()
        .is_some_and(|value| !value.is_empty())
    {
        return Err(ConfigError::NotAvailable {
            path: "ingest.default_with",
        });
    }
    if cfg
        .embed
        .model
        .as_ref()
        .is_some_and(|model| !model.is_empty())
    {
        return Err(ConfigError::NotAvailable {
            path: "embed.model",
        });
    }
    cfg.recall.effective_rrf_k()?;
    let flat = flatten(&cfg);
    for budget in BUDGETS {
        if flat.get(budget).is_some_and(|v| v.as_u64() == Some(0)) {
            return Err(ConfigError::Range {
                path: budget.to_owned(),
            });
        }
    }
    Ok(cfg)
}

/// The dotted path of the node that makes `data` fail to deserialize: the subtree under each
/// key is wrapped back into a single-path document and tried alone; the first failing key is
/// the path when the key itself is unknown or holds a scalar (an empty mapping in its place
/// fails too), otherwise the descent continues into its mapping. The result names a key path,
/// never a value.
fn locate(data: &Value) -> String {
    let mut prefix: Vec<String> = Vec::new();
    let mut node = data;
    loop {
        let Some(object) = node.as_object() else {
            return prefix.join(".");
        };
        let failing = object
            .iter()
            .find(|(key, child)| !deserializes(&wrap(&prefix, key, child)));
        match failing {
            Some((key, child)) => {
                let empty = Value::Object(serde_json::Map::new());
                let key_is_leaf = !deserializes(&wrap(&prefix, key, &empty));
                prefix.push(key.clone());
                if key_is_leaf || !child.is_object() {
                    return prefix.join(".");
                }
                node = child;
            }
            // every child passes alone but the combination fails: report the enclosing path
            None => return prefix.join("."),
        }
    }
}

fn deserializes(doc: &Value) -> bool {
    serde_json::from_value::<EngineConfig>(doc.clone()).is_ok()
}

/// `{p0: {p1: {key: child}}}` for the prefix `[p0, p1]`.
fn wrap(prefix: &[String], key: &str, child: &Value) -> Value {
    let mut doc = Value::Object(serde_json::Map::from_iter([(
        key.to_owned(),
        child.clone(),
    )]));
    for segment in prefix.iter().rev() {
        doc = Value::Object(serde_json::Map::from_iter([(segment.clone(), doc)]));
    }
    doc
}

/// Every leaf of the config as `section.key[.sub]` -> JSON value (maps and nested sections are
/// descended; arrays are leaves).
fn flatten(cfg: &EngineConfig) -> BTreeMap<String, Value> {
    fn descend(prefix: &str, v: &Value, out: &mut BTreeMap<String, Value>) {
        match v {
            Value::Object(map) if !map.is_empty() => {
                for (k, child) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    descend(&path, child, out);
                }
            }
            other => {
                out.insert(prefix.to_owned(), other.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    if let Ok(v) = serde_json::to_value(cfg) {
        descend("", &v, &mut out);
    }
    out
}

/// A leaf value as `doctor` prints it: strings bare, numbers and booleans as JSON, `null` as
/// `unset`, arrays and maps as canonical JSON.
fn render(v: &Value) -> String {
    match v {
        Value::Null => "unset".to_owned(),
        Value::String(s) => s.clone(),
        other => canonical_json(other),
    }
}

impl EngineConfig {
    /// Every leaf whose value differs from the register default, as `("section.key", value)`
    /// pairs in path order; a default map entry removed by the file shows as `unset`. Consumed
    /// by `kaban doctor` (E3.1) and `kaban-index doctor` (v4 CG4.2).
    #[must_use]
    pub fn effective_overrides(&self) -> Vec<(String, String)> {
        let defaults = flatten(&EngineConfig::default());
        let current = flatten(self);
        // a leaf on one side that is a mapping on the other (`embed.model: null` by default,
        // a map in the file) is reported through the mapping's leaves only
        let has_children = |map: &BTreeMap<String, Value>, path: &str| {
            let prefix = format!("{path}.");
            map.range(prefix.clone()..)
                .next()
                .is_some_and(|(k, _)| k.starts_with(&prefix))
        };
        let mut out = Vec::new();
        for (path, value) in &current {
            if defaults.get(path) != Some(value) && !has_children(&defaults, path) {
                out.push((path.clone(), render(value)));
            }
        }
        for path in defaults.keys() {
            if !current.contains_key(path) && !has_children(&current, path) {
                out.push((path.clone(), "unset".to_owned()));
            }
        }
        out.sort();
        out
    }
}

fn io_class(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::IsADirectory => "is_a_directory",
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::Interrupted => "interrupted",
        _ => "other",
    }
}
