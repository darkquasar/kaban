//! The producers (`stats.producers`, PC-16) and the effective resource budgets grouped under
//! the producer or index each bounds (EN-F22/F52, OWN-54).

use kaban_core::config::EngineConfig;
use kaban_core::{ExtractorKind, IndexMeta, Producer};
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
    /// The producer (`stats.producers` row name) or index (`text`) the budget bounds.
    pub group: &'static str,
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

/// Report all seven resource budgets, each under the producer or index it bounds, including
/// the SCIP import deadline and byte quotas.
#[must_use]
pub fn budgets(cfg: &EngineConfig, flags: &RunFlags) -> Vec<BudgetView> {
    let defaults = EngineConfig::default();
    let values = |c: &EngineConfig| {
        [
            ("text", "text.build_rss_mb", u64::from(c.text.build_rss_mb)),
            (
                "gazetteer",
                "gazetteer.max_automaton_mb",
                u64::from(c.gazetteer.max_automaton_mb),
            ),
            (
                "document_ingest",
                "ingest.lru_mib",
                u64::from(c.ingest.lru_mib),
            ),
            ("scip_import", "index.deadline_secs", c.index.deadline_secs),
            (
                "scip_import",
                "index.quota.file_bytes",
                c.index.quota.file_bytes,
            ),
            (
                "scip_import",
                "index.quota.run_dir_bytes",
                c.index.quota.run_dir_bytes,
            ),
            (
                "scip_import",
                "index.quota.tmp_bytes",
                c.index.quota.tmp_bytes,
            ),
        ]
    };
    values(cfg)
        .into_iter()
        .zip(values(&defaults))
        .map(|((group, key, effective), (_, _, default))| {
            let cap_lifted = flags.symbols && key == "gazetteer.max_automaton_mb";
            let source = if cap_lifted {
                BudgetSource::Flag("--symbols")
            } else if effective == default {
                BudgetSource::Default
            } else {
                BudgetSource::Config
            };
            BudgetView {
                group,
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
    /// The producer or index the budget bounds (`text`, `gazetteer`, `document_ingest`,
    /// `scip_import`).
    pub group: String,
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
            group: view.group.to_owned(),
            key: view.key.to_owned(),
            default: view.default,
            effective: view.effective,
            source,
            flag,
            cap_lifted: view.cap_lifted,
        }
    }
}

/// A fixed, content-free producer result stored under `producer_run:<producer>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProducerOutcome {
    /// Completed.
    Ok,
    /// Producer failed; the other producers remain usable.
    Failed,
    /// Resource cap prevented this producer from running.
    SkippedBudget,
}

impl ProducerOutcome {
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
pub struct ProducerRun {
    /// Unix timestamp in seconds.
    pub at: i64,
    /// Content-free outcome.
    pub outcome: ProducerOutcome,
}

/// One `stats.producers` row (PC-16): a producer, whether it runs by default, its last run
/// and the budgets that bound it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProducerRow {
    /// The producer (`extractor` stands for its three kinds).
    pub producer: String,
    /// Whether the producer runs by default.
    pub on: bool,
    /// Latest valid run timestamp, including explicitly selected runs.
    pub last_run: Option<i64>,
    /// Last fixed run outcome, absent when no valid bookkeeping exists.
    pub last_outcome: Option<ProducerOutcome>,
    /// Fixed reason when the producer is off by default.
    pub reason_off: Option<String>,
    /// The resource budgets that bound the producer (empty when none does).
    pub budgets: Vec<BudgetRow>,
}

/// The seven `stats.producers` rows, in [`Producer::ALL`] order with the three extractor
/// kinds under one row.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProducerRegistry;

impl ProducerRegistry {
    /// Configured default state with no run bookkeeping.
    #[must_use]
    pub fn enabled(&self, cfg: &EngineConfig) -> Vec<ProducerRow> {
        self.with_runs(cfg, &IndexMeta::default())
    }

    /// Configured default state and last valid producer runs.
    #[must_use]
    pub fn with_runs(&self, cfg: &EngineConfig, meta: &IndexMeta) -> Vec<ProducerRow> {
        let extractors = [
            Producer::Extractor(ExtractorKind::Llm),
            Producer::Extractor(ExtractorKind::Gliner),
            Producer::Extractor(ExtractorKind::Script),
        ];
        let rows: [(&[Producer], bool, Option<&str>); 7] = [
            (&[Producer::Authored], true, None),
            (&[Producer::HookEvents], true, None),
            (&[Producer::DocumentIngest], true, None),
            (
                &[Producer::Gazetteer],
                cfg.link.default == "gazetteer",
                Some("not_enabled"),
            ),
            // explicit selection only, even for a manually constructed config
            (&extractors, false, Some("explicit_selection_only")),
            (
                &[Producer::ScipImport],
                false,
                Some("explicit_selection_only"),
            ),
            (
                &[Producer::GitHistory],
                false,
                Some("not_available_in_this_release"),
            ),
        ];
        let budgets = budgets(cfg, &RunFlags::default());
        rows.into_iter()
            .map(|(producers, on, reason)| {
                let family = producers[0].family();
                let last = producers
                    .iter()
                    .filter_map(|producer| {
                        let value = meta.get(&format!("producer_run:{}", producer.as_str()))?;
                        let run = serde_json::from_str::<ProducerRun>(value).ok()?;
                        (run.at >= 0).then_some(run)
                    })
                    .max_by_key(|run| run.at);
                ProducerRow {
                    producer: family.to_owned(),
                    on,
                    last_run: last.map(|run| run.at),
                    last_outcome: last.map(|run| run.outcome),
                    reason_off: (!on).then(|| reason.unwrap_or("not_enabled").to_owned()),
                    budgets: budgets
                        .iter()
                        .filter(|view| view.group == family)
                        .copied()
                        .map(BudgetRow::from)
                        .collect(),
                }
            })
            .collect()
    }
}
