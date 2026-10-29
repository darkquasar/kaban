//! The default layer matrix and effective resource budgets (EN-F22/F52, OWN-54).

use kaban_core::config::EngineConfig;
use kaban_core::{IndexMeta, Layer, Owner};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Run-local switches that affect a resource limit, not the saved configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunFlags {
    /// Force the symbol dictionary even when it exceeds the configured automaton cap.
    pub symbols: bool,
}

/// Where an effective resource policy came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetSource {
    /// The register default.
    Default,
    /// A saved configuration value that differs from the default.
    Config,
    /// An explicit run-local flag; its spelling is fixed, never caller text.
    Flag(&'static str),
}

/// One effective resource policy. Values retain each key's documented units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetView {
    /// Dotted configuration key.
    pub key: &'static str,
    /// Register value.
    pub default: u64,
    /// Configured value (retained even when a flag bypasses the cap).
    pub effective: u64,
    /// Provenance of the policy.
    pub source: BudgetSource,
    /// An explicit bypass, not an invented numeric infinity.
    pub cap_lifted: bool,
}

/// Report all nine resource budgets, including the v4 deadline and byte quotas.
#[must_use]
pub fn budgets(cfg: &EngineConfig, flags: &RunFlags) -> Vec<BudgetView> {
    let defaults = EngineConfig::default();
    let values = |c: &EngineConfig| {
        [
            ("budget.l0_rss_mb", u64::from(c.budget.l0_rss_mb)),
            (
                "gazetteer.max_automaton_mb",
                u64::from(c.gazetteer.max_automaton_mb),
            ),
            ("embed.max_scan_rss_mb", u64::from(c.embed.max_scan_rss_mb)),
            ("ingest.lru_mib", u64::from(c.ingest.lru_mib)),
            ("index.deadline_secs", c.index.deadline_secs),
            ("index.quota.file_bytes", c.index.quota.file_bytes),
            ("index.quota.run_dir_bytes", c.index.quota.run_dir_bytes),
            ("index.quota.tmp_bytes", c.index.quota.tmp_bytes),
        ]
    };
    values(cfg)
        .into_iter()
        .zip(values(&defaults))
        .map(|((key, effective), (_, default))| {
            let cap_lifted = flags.symbols && key == "gazetteer.max_automaton_mb";
            let source = if cap_lifted {
                BudgetSource::Flag("--symbols")
            } else if effective == default {
                BudgetSource::Default
            } else {
                BudgetSource::Config
            };
            BudgetView {
                key,
                default,
                effective,
                source,
                cap_lifted,
            }
        })
        .collect()
}

/// Owned wire provenance: flag spelling is reported separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetOrigin {
    /// Register default.
    Default,
    /// Saved override.
    Config,
    /// Run-local flag.
    Flag,
}

/// An owned resource-policy row, suitable for report round trips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetRow {
    /// Dotted configuration key.
    pub key: String,
    /// Register value, in the key's documented units.
    pub default: u64,
    /// Configured value, not a sentinel for a lifted cap.
    pub effective: u64,
    /// Policy provenance.
    pub source: BudgetOrigin,
    /// Fixed flag spelling when a run lifts the cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// Whether enforcement is explicitly bypassed for this run.
    pub cap_lifted: bool,
}

impl From<BudgetView> for BudgetRow {
    fn from(view: BudgetView) -> Self {
        let (source, flag) = match view.source {
            BudgetSource::Default => (BudgetOrigin::Default, None),
            BudgetSource::Config => (BudgetOrigin::Config, None),
            BudgetSource::Flag(flag) => (BudgetOrigin::Flag, Some(flag.to_owned())),
        };
        Self {
            key: view.key.to_owned(),
            default: view.default,
            effective: view.effective,
            source,
            flag,
            cap_lifted: view.cap_lifted,
        }
    }
}

/// Display categories, not new assertion-source vocabulary values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatrixLayer {
    /// L0 lexical index and chunks.
    Fts,
    /// L1 optional mention dictionary.
    Gazetteer,
    /// Reserved L2 embeddings, unavailable in this release (OWN-94).
    Embed,
    /// L3 explicit extractors (llm, gliner and script).
    Extractors,
}

impl MatrixLayer {
    /// Fixed diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fts => "fts",
            Self::Gazetteer => "gazetteer",
            Self::Embed => "embed",
            Self::Extractors => "extractors",
        }
    }
}

/// A fixed, content-free producer result stored under `layer_run:<layer>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayerOutcome {
    /// Completed.
    Ok,
    /// Producer failed; other layers remain usable.
    Failed,
    /// Resource cap prevented this layer from running.
    SkippedBudget,
}

impl LayerOutcome {
    /// Fixed diagnostic reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::SkippedBudget => "skipped_budget",
        }
    }
}

/// Last run bookkeeping; only fixed outcomes are admitted, never arbitrary messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayerRun {
    /// Unix timestamp in seconds.
    pub at: i64,
    /// Content-free outcome.
    pub outcome: LayerOutcome,
}

/// One of the five default-matrix rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LayerRow {
    /// Matrix display category (not a producer vocabulary extension).
    pub layer: MatrixLayer,
    /// Actual closed-vocabulary sources represented by the row.
    pub producer_layers: Vec<Layer>,
    /// Whether configuration enables the layer by default.
    pub on: bool,
    /// Edge producer, when the layer has one.
    #[schemars(with = "Option<String>")]
    pub owner: Option<Owner>,
    /// Latest valid run timestamp, including explicitly selected runs.
    pub last_run: Option<i64>,
    /// Last fixed run outcome, absent when no valid bookkeeping exists.
    pub last_outcome: Option<LayerOutcome>,
    /// Fixed reason when the default is disabled.
    pub reason_off: Option<String>,
}

/// The five-row display registry. Producer vocabulary remains `Layer::ALL`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayerRegistry;

impl LayerRegistry {
    /// Configured default state with no run bookkeeping.
    #[must_use]
    pub fn enabled(&self, cfg: &EngineConfig) -> Vec<LayerRow> {
        self.with_runs(cfg, &IndexMeta::default())
    }

    /// Configured default state and last valid producer runs.
    #[must_use]
    pub fn with_runs(&self, cfg: &EngineConfig, meta: &IndexMeta) -> Vec<LayerRow> {
        let rows = [
            (
                MatrixLayer::Fts,
                vec![Layer::Fts],
                true,
                Some(Owner::KabanIngest),
            ),
            (
                MatrixLayer::Gazetteer,
                vec![Layer::Gazetteer],
                cfg.link.default == "gazetteer",
                Some(Owner::KabanIngest),
            ),
            (
                MatrixLayer::Embed,
                vec![Layer::Embed],
                false,
                Some(Owner::KabanEmbed),
            ),
            // Default enrollment is unavailable in 3.0.0, even for a manually constructed config.
            (
                MatrixLayer::Extractors,
                vec![Layer::Llm, Layer::Gliner, Layer::Script],
                false,
                Some(Owner::KabanParse),
            ),
        ];
        rows.into_iter()
            .map(|(layer, producer_layers, on, owner)| {
                let last = producer_layers
                    .iter()
                    .filter_map(|producer| {
                        let value = meta.get(&format!("layer_run:{}", producer.as_str()))?;
                        let run = serde_json::from_str::<LayerRun>(value).ok().or_else(|| {
                            value.parse::<i64>().ok().map(|at| LayerRun {
                                at,
                                outcome: LayerOutcome::Ok,
                            })
                        })?;
                        (run.at >= 0).then_some(run)
                    })
                    .max_by_key(|run| run.at);
                LayerRow {
                    layer,
                    producer_layers,
                    on,
                    owner,
                    last_run: last.map(|run| run.at),
                    last_outcome: last.map(|run| run.outcome),
                    reason_off: (!on).then(|| {
                        if layer == MatrixLayer::Embed {
                            "not_available_in_this_release".to_owned()
                        } else if layer == MatrixLayer::Extractors {
                            "explicit_selection_only".to_owned()
                        } else {
                            "not_enabled".to_owned()
                        }
                    }),
                }
            })
            .collect()
    }
}
