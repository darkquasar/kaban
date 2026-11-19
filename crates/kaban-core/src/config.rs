//! The one `EngineConfig` (engine-spec §15.1 defaults register; engine-plan E1.1, SO-11): every
//! `[section] key` of the register is a field of the matching section struct, its `Default` is
//! the register's value, and the YAML path is `section.key` in `~/.kaban/config.yaml`
//! (`KABAN_HOME`). Fields are plain serde data (`f64`, `u32`, `bool`, `String`, `BTreeMap`,
//! `Option`); the typed forms (`Fixed`, `Fusion`, `ModelIdentity`, `LinkMode`) are
//! built once at the consuming seam by the task that owns them, so this module depends on
//! nothing defined later. Unknown keys fail at every depth with the field path (never the value),
//! the sections of removed features (`reconcile`, `er`, `why`, `briefing`) included; an opt-in
//! budget of 0 fails [`ConfigError::Range`].
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

/// `[extractors]` (EN-F27a; the old `[l3]`, PC-16).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Extractors {
    /// Holdout F1 an extractor must reach before `--with` is accepted.
    pub holdout_min_f1: f64,
    /// Minimum judged spans in the holdout.
    pub holdout_min_spans: u32,
}

impl Default for Extractors {
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

/// `[text]` (EN-N3; the text index build budget, the old `budget.l0_rss_mb`, PC-16; an opt-in
/// budget, 0 is out of range).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Text {
    /// Text index build RSS budget in MiB.
    pub build_rss_mb: u32,
}

impl Default for Text {
    fn default() -> Self {
        Self { build_rss_mb: 64 }
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

/// The smallest memory a maintenance unit of work (one seal, fold or base build) needs, in MiB:
/// a `catalog.maintenance_memory_mib` below it is refused.
// 7.6 (2026-11-18): seal of 10,000 ops, VmHWM 21,384 KiB, 3.75 MiB over its start: 4x margin
const MIN_MAINTENANCE_MEMORY_MIB: u64 = 16;

/// The shortest deadline a maintenance unit of work can meet, in ms: a
/// `catalog.maintenance_deadline_ms` below it is refused.
// 7.6 (2026-11-18): seal of 10,000 ops, VmHWM 21,384 KiB, ran 13 ms (debug build)
const MIN_MAINTENANCE_DEADLINE_MS: u64 = 100;

/// `[catalog]` (spec 11 PC-10, Task 6, 7.6): compaction policy, the scheduled checkpoint and
/// the maintenance budget of the catalog. Validated at load ([`CatalogCfg::validate`]): a value
/// out of range is refused naming its key, never clamped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CatalogCfg {
    /// What each class may hold pending before it seals ([`SealOps`]).
    pub seal_ops: SealOps,
    /// Sealed layers over a base before they fold.
    pub fold_depth: u32,
    /// Layer bytes over base bytes past which the layers fold.
    pub fold_delta_ratio: f64,
    /// Interval of the scheduled checkpoint, in ms.
    pub checkpoint_interval_ms: u64,
    /// Memory ceiling of one maintenance unit of work (seal, fold, base build), in MiB.
    pub maintenance_memory_mib: u64,
    /// Deadline of one maintenance unit of work, in ms.
    pub maintenance_deadline_ms: u64,
    /// Live views besides `main` that compaction keeps, newest by last use (amendment 3.2):
    /// the older ones are marked `needs_reimport` and their extents retire.
    pub view_retention: u32,
}

impl Default for CatalogCfg {
    fn default() -> Self {
        Self {
            seal_ops: SealOps::default(),
            fold_depth: 8,
            fold_delta_ratio: 0.25,
            // `bench catalog` (2026-11-18): 60 s, 1 to 4 writers, WAL high-water 1,512,072 B
            checkpoint_interval_ms: 1_000,
            // a value the machine can hold: refused above physical memory only when configured
            maintenance_memory_mib: default_maintenance_memory_mib(physical_memory_mib()),
            // 7.6 (2026-11-18): fold of 5M edges, VmHWM 1,605,864 KiB, ran 9.9 s (debug build)
            maintenance_deadline_ms: 30_000,
            // amendment 3.2: the retention count of views besides `main`
            view_retention: 200,
        }
    }
}

/// `[catalog.seal_ops]`: the seal trigger of each index class, in what the class counts as
/// pending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SealOps {
    /// Text: pending text segments, one per commit, not operations; tombstone rows are not
    /// counted.
    pub text: u32,
    /// Graph: pending operations.
    pub graph: u32,
    /// Code: pending operations.
    pub code: u32,
}

impl Default for SealOps {
    fn default() -> Self {
        // 7.6 (2026-11-18): graph seal of 10,000 ops, VmHWM 21,384 KiB, ran 13 ms (debug build)
        Self {
            // owner decision 2026-11-19: seal past 32 pending text segments
            text: 32,
            graph: 10_000,
            code: 10_000,
        }
    }
}

/// The maintenance memory ceiling when `catalog.maintenance_memory_mib` is not set, in MiB.
// 7.6 (2026-11-18): fold of 1M edges, VmHWM 361,472 KiB (353 MiB): 2.9x headroom
const DEFAULT_MAINTENANCE_MEMORY_MIB: u64 = 1_024;

/// The effective default of `catalog.maintenance_memory_mib`: [`DEFAULT_MAINTENANCE_MEMORY_MIB`],
/// or half of `physical_mib` (the machine's physical memory, where known) when that is less, so
/// an untouched configuration loads on every machine; a configured value is still refused
/// above physical memory ([`CatalogCfg::validate`]).
fn default_maintenance_memory_mib(physical_mib: Option<u64>) -> u64 {
    physical_mib.map_or(DEFAULT_MAINTENANCE_MEMORY_MIB, |physical| {
        DEFAULT_MAINTENANCE_MEMORY_MIB.min(physical / 2)
    })
}

/// The machine's physical memory in MiB (`MemTotal` of `/proc/meminfo`), `None` where it is
/// not known (another platform, an unreadable file): then no value is refused for exceeding it.
fn physical_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    Some(kib / 1024)
}

impl CatalogCfg {
    /// Refuse a value out of range, naming its key (spec 11 7.6: never clamped): a count,
    /// depth, interval or view retention of 0, a ratio that is not a positive number, a memory ceiling below
    /// the smallest unit of work or above physical memory, a deadline below the smallest unit
    /// of work.
    ///
    /// # Errors
    /// [`ConfigError::Range`] with the key's dotted path (the first refused key).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let memory = self.maintenance_memory_mib;
        let refused = [
            ("catalog.seal_ops.text", self.seal_ops.text == 0),
            ("catalog.seal_ops.graph", self.seal_ops.graph == 0),
            ("catalog.seal_ops.code", self.seal_ops.code == 0),
            ("catalog.fold_depth", self.fold_depth == 0),
            (
                "catalog.fold_delta_ratio",
                !(self.fold_delta_ratio.is_finite() && self.fold_delta_ratio > 0.0),
            ),
            (
                "catalog.checkpoint_interval_ms",
                self.checkpoint_interval_ms == 0,
            ),
            (
                "catalog.maintenance_memory_mib",
                memory < MIN_MAINTENANCE_MEMORY_MIB
                    || physical_memory_mib().is_some_and(|physical| memory > physical),
            ),
            (
                "catalog.maintenance_deadline_ms",
                self.maintenance_deadline_ms < MIN_MAINTENANCE_DEADLINE_MS,
            ),
            ("catalog.view_retention", self.view_retention == 0),
        ];
        match refused.into_iter().find(|(_, out_of_range)| *out_of_range) {
            Some((path, _)) => Err(ConfigError::Range { path: path.into() }),
            None => Ok(()),
        }
    }
}

/// The engine configuration: every §15.1 section with its register defaults.
///
/// This is the configuration root of the whole kaban process (`Engine` keeps its spec 03
/// meaning, the program), not the storage engine: the storage kernel is named the catalog,
/// with its own `[catalog]` section (PC-10).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineConfig {
    /// `[ontology]`
    pub ontology: OntologyCfg,
    /// `[gazetteer]`
    pub gazetteer: Gazetteer,
    /// `[extractors]`
    pub extractors: Extractors,
    /// `[recall]`
    pub recall: Recall,
    /// `[ask]`
    pub ask: Ask,
    /// `[ingest]`
    pub ingest: Ingest,
    /// `[index]`
    pub index: Index,
    /// `[link]`
    pub link: Link,
    /// `[text]`
    pub text: Text,
    /// `[bench]`
    pub bench: Bench,
    /// `[catalog]`
    pub catalog: CatalogCfg,
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
const BUDGETS: [&str; 10] = [
    "ingest.claim_ttl_s",
    "ingest.claim_max_attempts",
    "ingest.claim_scan_max",
    "text.build_rss_mb",
    "gazetteer.max_automaton_mb",
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
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(EngineConfig::default());
        }
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
    cfg.recall.effective_rrf_k()?;
    cfg.catalog.validate()?;
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

#[cfg(test)]
mod catalog_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // test code: failures are the assertion

    use super::*;

    #[test]
    fn the_catalog_section_defaults_to_fold_depth_8_and_ratio_a_quarter() {
        let cfg = parse("catalog: {}\n").unwrap();
        assert_eq!(cfg.catalog, CatalogCfg::default());
        assert_eq!(cfg.catalog.fold_depth, 8);
        assert!((cfg.catalog.fold_delta_ratio - 0.25).abs() < f64::EPSILON);
        assert_eq!(
            cfg.catalog.maintenance_memory_mib,
            default_maintenance_memory_mib(physical_memory_mib())
        );
        assert_eq!(cfg.effective_overrides(), Vec::new());
    }

    #[test]
    fn the_default_maintenance_memory_fits_a_small_machine() {
        // half of a 1 GiB machine, the full 1024 MiB on a large one or where it is not known
        assert_eq!(default_maintenance_memory_mib(Some(1_024)), 512);
        assert_eq!(default_maintenance_memory_mib(Some(16_384)), 1_024);
        assert_eq!(default_maintenance_memory_mib(None), 1_024);
    }

    #[test]
    fn a_catalog_value_out_of_range_is_refused_naming_its_key() {
        let mut cases = vec![
            ("seal_ops: {text: 0}", "catalog.seal_ops.text"),
            ("seal_ops: {graph: 0}", "catalog.seal_ops.graph"),
            ("seal_ops: {code: 0}", "catalog.seal_ops.code"),
            ("fold_depth: 0", "catalog.fold_depth"),
            ("fold_delta_ratio: 0.0", "catalog.fold_delta_ratio"),
            ("fold_delta_ratio: -1.0", "catalog.fold_delta_ratio"),
            (
                "checkpoint_interval_ms: 0",
                "catalog.checkpoint_interval_ms",
            ),
            (
                "maintenance_memory_mib: 0",
                "catalog.maintenance_memory_mib",
            ),
            (
                "maintenance_memory_mib: 15",
                "catalog.maintenance_memory_mib",
            ),
            (
                "maintenance_deadline_ms: 0",
                "catalog.maintenance_deadline_ms",
            ),
            (
                "maintenance_deadline_ms: 99",
                "catalog.maintenance_deadline_ms",
            ),
            ("view_retention: 0", "catalog.view_retention"),
        ];
        if physical_memory_mib().is_some() {
            // above any machine's physical memory
            cases.push((
                "maintenance_memory_mib: 1099511627776",
                "catalog.maintenance_memory_mib",
            ));
        }
        for (yaml, key) in cases {
            let refused = parse(&format!("catalog:\n  {yaml}\n"));
            assert_eq!(
                refused,
                Err(ConfigError::Range { path: key.into() }),
                "{yaml}"
            );
        }
    }

    #[test]
    fn catalog_values_at_the_smallest_unit_of_work_load_unchanged() {
        let cfg = parse(
            "catalog:\n  maintenance_memory_mib: 16\n  maintenance_deadline_ms: 100\n  fold_depth: 1\n",
        )
        .unwrap();
        assert_eq!(cfg.catalog.maintenance_memory_mib, 16);
        assert_eq!(cfg.catalog.maintenance_deadline_ms, 100);
        assert_eq!(cfg.catalog.fold_depth, 1);
    }
}
