//! E3.12 (RP-13; EN-N3, EN-N5, EN-N6): one `LayerMeasurement` per fresh process.
//!
//! ```text
//!   kaban bench recall --measure <set> --n N --out <set>.json      (one process per set)
//!   +----------------------------+     +-----------------------------------------------+
//!   | materialize (recall corpus |  => | L0 workload: 100 judged queries, layers [fts] |
//!   | + judged fixture sources)  |     +-----------------------------------------------+
//!   +----------------------------+          | l0: total = process VmHWM (build included)
//!                                           | others: VmHWM reset, L0 workload = baseline
//!                                           v
//!   l0+gazetteer-run ingest the judged source passages with --link gazetteer: telemetry
//!                    (build peak, retained automaton), producer-origin MENTIONS precision,
//!                    then (after the row is read) per-chunk match time, informational
//!   l0+l3-brief      ingest_brief + ingest_next on a 100 KiB target, warm; cache N/A
//!   l0+l2-query      deferred row (OWN-93/94), no process work
//! ```
//!
//! Bounds are the effective values of `kaban_api::layers::budgets` for the loaded engine
//! configuration; overrides are recorded. A missing measurement is an error, never a pass.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use kaban_api::layers::{BudgetRow, RunFlags, budgets};
use kaban_api::{ApiConfig, EdgeFilter, KabanApi, TraverseRequest};
use kaban_core::{
    ActorId, NodeId, RecallLayer, TraverseDirection, WorkspaceId, config::EngineConfig,
};
use kaban_ingest::{ChunkConfig, DocumentPreparer, IngestConfig, Ingestor, WalkConfig};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::corpus::{Corpus, SEED, fixture_repo};
use super::recall_classes::{Fixtures, Pair, QueryClass, edge_precision};
use super::recall_runtime::{RecallArgs, SourceMap, envelope, evaluate, hash, load, materialize};

const MIB: u64 = 1024 * 1024;
/// EN-N3: the default matrix stays under 100 MiB at 100k notes.
const N3_PROMISE_BYTES: u64 = 100 * MIB;
/// Only a 100k run qualifies a bound; smaller runs exercise the same arithmetic.
const CAMPAIGN_NOTES: usize = 100_000;
/// EN-N5 targets (informational, OWN-3).
const BRIEF_TARGET_MS: u64 = 500;
const NEXT_TARGET_MS: u64 = 100;
/// The L3 brief target size (EN-N5: 100 KiB).
const BRIEF_TARGET_BYTES: usize = 100 * 1024;
/// EN-N6: about 512 tokens under the shipped `chars4` estimator.
const MATCH_CHUNK_CHARS: usize = 512 * 4;
const RW_E3_W2_1: &str = "RW-E3-W2-1 (Medium, deferred): the evaluator reads MENTIONS \
    through the public traverse and does not model the 200-edge hub cutoff; precision covers \
    the mapped producer-origin chunks only, never exhaustive producer recall";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerSet {
    L0,
    Gazetteer,
    L2Query,
    L3Brief,
}

impl LayerSet {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "l0" => Self::L0,
            "l0+gazetteer-run" => Self::Gazetteer,
            "l0+l2-query" => Self::L2Query,
            "l0+l3-brief" => Self::L3Brief,
            _ => bail!("unknown layer set"),
        })
    }

    const fn name(self) -> &'static str {
        match self {
            Self::L0 => "l0",
            Self::Gazetteer => "l0+gazetteer-run",
            Self::L2Query => "l0+l2-query",
            Self::L3Brief => "l0+l3-brief",
        }
    }

    /// The `budgets()` key that bounds the set; L3 has no memory bound (cache N/A, OWN-93).
    const fn bound_key(self) -> Option<&'static str> {
        match self {
            Self::L0 => Some("budget.l0_rss_mb"),
            Self::Gazetteer => Some("gazetteer.max_automaton_mb"),
            Self::L2Query => Some("embed.max_scan_rss_mb"),
            Self::L3Brief => None,
        }
    }
}

/// The RP-13 row. `pass` is `None` for an informational (L3) or deferred (L2) row, so no
/// cache or embed pass is ever fabricated; the measured fields are `None` only when deferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LayerMeasurement {
    layer_set: &'static str,
    status: &'static str,
    total_vm_hwm_bytes: Option<u64>,
    baseline_vm_hwm_bytes: Option<u64>,
    delta_bytes: Option<i64>,
    build_peak_delta_bytes: Option<u64>,
    retained_bytes: Option<u64>,
    cache_bytes: Option<u64>,
    bound_key: Option<&'static str>,
    bound_bytes: Option<u64>,
    pass: Option<bool>,
}

/// What one fresh process observed for its set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Observed {
    total: u64,
    baseline: u64,
    build_peak: Option<u64>,
    retained: Option<u64>,
    /// Gazetteer only: telemetry outcome `ok` (a `skipped_budget` run is not a pass, EN-N6).
    outcome_ok: bool,
}

/// RP-13 assertions. l0: total within the L0 budget and the 100 MiB promise; gazetteer:
/// build peak and retained automaton within the cap, from a run that kept its dictionary.
fn judge(set: LayerSet, seen: &Observed, bound_bytes: u64) -> Option<bool> {
    match set {
        LayerSet::L0 => Some(seen.total <= bound_bytes && seen.total <= N3_PROMISE_BYTES),
        LayerSet::Gazetteer => Some(
            seen.outcome_ok
                && seen.build_peak.is_some_and(|peak| peak <= bound_bytes)
                && seen
                    .retained
                    .is_some_and(|retained| retained <= bound_bytes),
        ),
        LayerSet::L2Query | LayerSet::L3Brief => None,
    }
}

fn measurement(set: LayerSet, seen: &Observed, budgets: &[BudgetRow]) -> Result<LayerMeasurement> {
    let bound_bytes = set
        .bound_key()
        .map(|key| {
            budgets
                .iter()
                .find(|row| row.key == key)
                .map(|row| row.effective.saturating_mul(MIB))
                .context("effective budget missing")
        })
        .transpose()?;
    let delta = i64::try_from(seen.total)? - i64::try_from(seen.baseline)?;
    Ok(LayerMeasurement {
        layer_set: set.name(),
        status: if set == LayerSet::L3Brief {
            "informational"
        } else {
            "measured"
        },
        total_vm_hwm_bytes: Some(seen.total),
        baseline_vm_hwm_bytes: Some(seen.baseline),
        delta_bytes: Some(delta),
        build_peak_delta_bytes: seen.build_peak,
        retained_bytes: seen.retained,
        cache_bytes: None,
        bound_key: set.bound_key(),
        bound_bytes,
        pass: bound_bytes.and_then(|bound| judge(set, seen, bound)),
    })
}

/// Producer-origin pairs: every edge starts at a materialized chunk of a judged source;
/// targets map through the retained corpus identities, and unknown or ambiguous targets stay
/// unjudged (`unmapped-node:`). The first copy of a duplicate edge id wins.
fn producer_pairs(
    workspace: &str,
    origin: &BTreeMap<String, String>,
    reverse: &BTreeMap<(&str, &str), Option<&str>>,
    edges: &[(String, String, String)],
) -> Result<Vec<Pair>> {
    let mut seen = BTreeSet::new();
    let mut pairs = Vec::new();
    for (id, from, to) in edges {
        if !seen.insert(id) {
            continue;
        }
        let from = origin
            .get(from)
            .context("MENTIONS edge outside the producer-origin chunks")?;
        let to = reverse
            .get(&(workspace, to.as_str()))
            .copied()
            .flatten()
            .map_or_else(|| format!("unmapped-node:{to}"), str::to_owned);
        pairs.push(Pair {
            workspace: workspace.to_owned(),
            from: from.clone(),
            to,
        });
    }
    Ok(pairs)
}

fn vm_hwm() -> Result<u64> {
    super::report::self_status_kb("VmHWM")
        .map(|kb| kb.saturating_mul(1024))
        .context("VmHWM unavailable (Linux /proc required)")
}

/// Reset `VmHWM` to the current RSS (Linux `clear_refs` 5), so the next read is a phase peak.
fn reset_hwm() -> Result<()> {
    std::fs::write("/proc/self/clear_refs", "5").context("resetting VmHWM (Linux /proc required)")
}

fn fresh_dir(path: &Path) -> Result<()> {
    ensure!(
        !path.exists(),
        "layer measurement requires fresh work paths"
    );
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

struct Rig<'a> {
    api: &'a Arc<KabanApi>,
    fixtures: &'a Fixtures,
    map: &'a SourceMap,
    classes: &'a BTreeSet<QueryClass>,
}

impl Rig<'_> {
    fn queries(&self, layers: &[RecallLayer]) -> Result<Value> {
        evaluate(self.api, self.fixtures, self.map, self.classes, layers)
    }
}

/// Ingest each judged `from` passage as its own document with `--link gazetteer`, map the
/// producer's chunks back to the stable source through the document identity, and judge the
/// actual gazetteer MENTIONS edges.
fn gazetteer(rig: &Rig<'_>, db: &Path, samples: usize) -> Result<(Observed, Value)> {
    let api = rig.api;
    let workspace = api.workspace().clone();
    let dir = db.with_file_name("e3-layers-gazetteer-docs");
    fresh_dir(&dir)?;
    let mut stable: Vec<&str> = Vec::new();
    for edge in &rig.fixtures.edges {
        if !stable.contains(&edge.from.as_str()) {
            stable.push(&edge.from);
        }
    }
    let mut origins = Vec::new();
    for (index, id) in stable.iter().enumerate() {
        let source = rig
            .fixtures
            .sources
            .iter()
            .find(|source| source.id == *id)
            .context("judged edge source missing from the catalog")?;
        ensure!(
            source.workspace == workspace.as_str(),
            "judged source outside the bound workspace"
        );
        let rel = format!("{index:02}.md");
        std::fs::write(dir.join(&rel), format!("{}\n", source.text))?;
        origins.push((rel, source));
    }
    // A passage's remembered copy (a catalog Fact, or corpus note 1, 2 or 4) has the whole
    // passage as its title, which leftmost-longest matching would prefer over every mention
    // inside it. The passages are materialized as producer chunks only: archive the copies.
    for (_, source) in &origins {
        let id = rig.map.resolve(&source.workspace, &source.id)?;
        api.forget(kaban_api::ForgetRequest {
            id: id.to_owned(),
            mode: Some(kaban_core::ForgetMode::Archive),
        })?;
    }
    let cfg = IngestConfig {
        link: Some("gazetteer".into()),
        ..IngestConfig::default()
    };
    let preparer = Arc::new(DocumentPreparer::new(
        api,
        Arc::clone(api.store()),
        Arc::clone(api.signer()),
        ChunkConfig::default(),
        cfg.clone(),
    ));
    let ingestor = Ingestor::new(
        Arc::clone(api),
        Arc::clone(api.store()),
        Arc::clone(api.signer()),
        preparer,
        cfg,
    );
    let report = runtime()?
        .block_on(ingestor.run(&WalkConfig::new(dir.clone()), CancellationToken::new()))?;
    // The run's dictionary dies with its preparer, as in `index_docs` (one per call).
    drop(ingestor);
    let total = vm_hwm()?;
    ensure!(
        report.failed == 0 && report.blocked == 0 && report.documents as usize == origins.len(),
        "the judged passages must ingest clean"
    );
    let telemetry = report
        .gazetteer
        .clone()
        .context("gazetteer telemetry missing")?;
    let policy = api.policy_handle();
    let store = api.store().as_ref();
    let mut origin = BTreeMap::new();
    for (rel, source) in &origins {
        let gated = kaban_ingest::gate_path(
            &policy
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            rel,
        )
        .map_err(|_| anyhow::anyhow!("judged document path refused"))?;
        let document = kaban_ingest::document_id(&kaban_ingest::SourceId::new(&workspace, &gated));
        let document = store
            .get(&document, false)?
            .context("producer document missing")?;
        ensure!(
            document.kind == "Document" && document.workspace_id == workspace.as_str(),
            "foreign producer document"
        );
        let chunks = document.props["chunk_ids"]
            .as_array()
            .filter(|chunks| !chunks.is_empty())
            .context("producer document has no chunks")?;
        for chunk in chunks {
            let id = chunk.as_str().context("invalid chunk id")?;
            let node = store
                .get(&NodeId::parse(id)?, false)?
                .context("producer chunk missing")?;
            ensure!(
                node.kind == "Chunk"
                    && node.workspace_id == workspace.as_str()
                    && node.text.trim() == source.text.trim(),
                "producer chunk does not carry its judged source"
            );
            origin.insert(id.to_owned(), source.id.clone());
        }
    }
    let mut edges = Vec::new();
    let mut truncated = false;
    for chunk in origin.keys() {
        let result = api.traverse(TraverseRequest {
            start_ids: vec![chunk.clone()],
            workspace_id: Some(workspace.to_string()),
            rels: vec!["MENTIONS".into()],
            direction: Some(TraverseDirection::Out),
            max_depth: Some(1),
            limit: Some(500),
            edge_filter: Some(EdgeFilter {
                layers: vec!["gazetteer".into()],
                ..Default::default()
            }),
            ..Default::default()
        })?;
        truncated |= result.nodes.len() == 500;
        edges.extend(result.edges.into_iter().map(|e| (e.id, e.from, e.to)));
    }
    let pairs = producer_pairs(workspace.as_str(), &origin, &rig.map.reverse(), &edges)?;
    let precision = edge_precision(&rig.fixtures.edges, &pairs)?;
    // EN-N6 match cost, after the row's HWM was read: one more build, then timed matching.
    let policy = policy
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dictionary =
        kaban_ingest::build_dictionary(store, &workspace, None, &policy, &api.engine().gazetteer)?;
    let mut text = String::with_capacity(MATCH_CHUNK_CHARS + 256);
    for source in rig.fixtures.sources.iter().cycle() {
        if text.len() >= MATCH_CHUNK_CHARS {
            break;
        }
        let _ = write!(text, "{} ", source.text);
    }
    let verdict = policy.scan(&text);
    ensure!(!verdict.rejected, "match-time chunk rejected by policy");
    let text = if verdict.hits.is_empty() {
        text
    } else {
        verdict.redacted
    };
    let chunk = NodeId::parse(origin.keys().next().context("no producer chunk")?)?;
    let mut matched = 0;
    for _ in 0..3 {
        matched = kaban_ingest::find_mentions(&dictionary, &chunk, &text).len();
    }
    let mut match_us = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let _ = kaban_ingest::find_mentions(&dictionary, &chunk, &text);
        match_us.push(start.elapsed().as_secs_f64() * 1e6);
    }
    drop(dictionary);
    let seen = Observed {
        total,
        build_peak: Some(telemetry.build_peak_delta),
        retained: Some(telemetry.memory_usage),
        outcome_ok: telemetry.outcome == "ok",
        ..Observed::default()
    };
    Ok((
        seen,
        json!({ "telemetry": telemetry, "documents": report.documents,
        "chunks_upserted": report.chunks_upserted,
        "serve_retained": "index_docs builds one preparer per call and drops it after the run; \
            not separately sampled here",
        "edge_precision": { "layer": "gazetteer", "rel": "MENTIONS",
            "precision": precision.precision, "coverage": precision.coverage,
            "judged": precision.judged, "predicted": precision.predicted,
            "judged_predicted": precision.judged_predicted, "correct": precision.correct,
            "unjudged": precision.unjudged, "duplicates": precision.duplicates,
            "truncated": truncated, "origin_sources": origins.len(), "origin_chunks": origin.len(),
            "predictions": pairs.iter().map(|pair| [&pair.from, &pair.to]).collect::<Vec<_>>(),
            "scope": "producer-origin chunks of the judged source passages; targets through the retained corpus map",
            "limitation": RW_E3_W2_1, "informational": true },
        "match_time": { "chunk_chars": text.len(), "token_estimator": "chars4",
            "mentions": matched, "series": super::report::raw_series(&match_us),
            "bound_key": "gazetteer.max_chunk_ms", "bound_ms": api.engine().gazetteer.max_chunk_ms,
            "excludes_build": true, "informational": true } }),
    ))
}

/// EN-N5: brief and next on a 100 KiB Markdown target, warm, without a cache (OWN-93).
fn brief(rig: &Rig<'_>, db: &Path, samples: usize, warmup: usize) -> Result<(u64, Value)> {
    use kaban_ingest::jobs::{brief::brief, next::PageBudget, next::next, types::IngestOpts};
    let root = db.with_file_name("e3-layers-brief-root");
    fresh_dir(&root)?;
    let mut corpus = Corpus::new(SEED);
    let mut body = String::with_capacity(BRIEF_TARGET_BYTES + 512);
    body.push_str("# Brief target\n\n");
    let mut section = 0;
    while body.len() < BRIEF_TARGET_BYTES {
        let _ = write!(body, "## Section {section}\n\n");
        for _ in 0..40 {
            body.push_str(&corpus.term());
            body.push(' ');
        }
        body.push_str("\n\n");
        section += 1;
    }
    std::fs::write(root.join("brief.md"), &body)?;
    let opts = IngestOpts::default();
    let rt = runtime()?;
    let api = rig.api.as_ref();
    let start = Instant::now();
    let first = rt
        .block_on(brief(api, &root, "brief.md", &opts))
        .map_err(|error| anyhow::anyhow!("ingest_brief failed: {error}"))?;
    let cold_us = start.elapsed().as_secs_f64() * 1e6;
    let job = kaban_core::JobId::parse(&first.job)?;
    let call_next = || {
        rt.block_on(next(api, &root, &job, 0, 0, PageBudget::default()))
            .map_err(|error| anyhow::anyhow!("ingest_next failed: {error}"))
    };
    let call_brief = || {
        rt.block_on(brief(api, &root, "brief.md", &opts))
            .map_err(|error| anyhow::anyhow!("ingest_brief failed: {error}"))
    };
    for _ in 0..warmup {
        call_brief()?;
        call_next()?;
    }
    let (mut brief_us, mut next_us) = (Vec::new(), Vec::new());
    for _ in 0..samples {
        let start = Instant::now();
        ensure!(
            call_brief()?.job == first.job,
            "repeat brief changed its job"
        );
        brief_us.push(start.elapsed().as_secs_f64() * 1e6);
        let start = Instant::now();
        call_next()?;
        next_us.push(start.elapsed().as_secs_f64() * 1e6);
    }
    let total = vm_hwm()?;
    Ok((
        total,
        json!({ "target_bytes": body.len(), "plan_pages": first.chunk_plan.pages,
        "brief_cold_us": cold_us, "brief_warm": super::report::raw_series(&brief_us),
        "next_warm": super::report::raw_series(&next_us),
        "targets_ms": { "brief": BRIEF_TARGET_MS, "next": NEXT_TARGET_MS },
        "cache": "not_applicable: LRU deferred (OWN-93)", "informational": true }),
    ))
}

/// Run one layer set in this process and return the bench envelope.
pub(super) fn run(
    db: &Path,
    keys: &Path,
    actor: &ActorId,
    policy: Policy,
    engine: &EngineConfig,
    args: &RecallArgs,
    set: &str,
) -> Result<Value> {
    let set = LayerSet::parse(set)?;
    let rows: Vec<BudgetRow> = budgets(engine, &RunFlags::default())
        .into_iter()
        .map(BudgetRow::from)
        .collect();
    let overrides: Vec<&BudgetRow> = rows
        .iter()
        .filter(|row| row.source != kaban_api::layers::BudgetOrigin::Default)
        .collect();
    let deferred = json!({ "l0+l2-query": "OWN-93/94: embed vectors and L2 not in this release",
        "cache": "OWN-93", "cc_fusion": "OWN-68 SC-06", "holdout_f1": "OWN-68 SC-03",
        "500k": "OWN-68 SC-13" });
    if set == LayerSet::L2Query {
        let row = LayerMeasurement {
            layer_set: set.name(),
            status: "deferred",
            total_vm_hwm_bytes: None,
            baseline_vm_hwm_bytes: None,
            delta_bytes: None,
            build_peak_delta_bytes: None,
            retained_bytes: None,
            cache_bytes: None,
            bound_key: set.bound_key(),
            bound_bytes: None,
            pass: None,
        };
        return envelope(
            0,
            json!({ "mode": "layers", "layer_measurements": [row], "effective_budgets": rows,
            "overrides": overrides, "deferred": deferred,
            "layer_gate": { "status": "deferred", "pass": null } }),
            "layers",
        );
    }
    ensure!(
        args.n >= 5 && args.warmup > 0,
        "corpus requires at least five notes and warmup"
    );
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db.as_os_str().to_os_string();
        path.push(suffix);
        ensure!(
            !Path::new(&path).exists(),
            "layer measurement requires a fresh database"
        );
    }
    let fixture = db.with_file_name("e3-layers-fixture-repo");
    ensure!(
        !fixture.exists(),
        "layer measurement requires a fresh fixture repository"
    );
    let fixtures = load(
        &args.sources,
        &args.judged,
        &args.edge_judged,
        engine.recall.judged_min_queries as usize,
    )?;
    let sqlite =
        Arc::new(SqliteStore::open_as(db, keys, actor)?.with_recall_config(engine.recall.clone())?);
    let mut corpus = Corpus::new(SEED);
    let commit = fixture_repo(&fixture, &corpus)?;
    let api = Arc::new(KabanApi::with_config(
        sqlite.clone(),
        policy,
        sqlite.signer(),
        None,
        ApiConfig {
            engine: Arc::new(engine.clone()),
            workspace: WorkspaceId::new("ws1"),
            repo_root: Some(fixture),
            repo: Some("bench-fixture-repo".into()),
            ..Default::default()
        },
    ));
    let map = materialize(
        &api,
        sqlite.as_ref(),
        &fixtures,
        args.n,
        args.edges_per_node,
        &mut corpus,
        &commit,
    )?;
    let classes: BTreeSet<QueryClass> = fixtures.queries.iter().map(|q| q.class).collect();
    let rig = Rig {
        api: &api,
        fixtures: &fixtures,
        map: &map,
        classes: &classes,
    };
    rig.queries(&[RecallLayer::Fts])?;
    let process_hwm = vm_hwm()?;
    let samples = args.samples.max(30).max(engine.bench.min_samples as usize);
    let (seen, layer) = if set == LayerSet::L0 {
        let seen = Observed {
            total: process_hwm,
            baseline: process_hwm,
            ..Observed::default()
        };
        (
            seen,
            json!({ "basis": "whole-process VmHWM after build and the L0 workload" }),
        )
    } else {
        reset_hwm()?;
        rig.queries(&[RecallLayer::Fts])?;
        let baseline = vm_hwm()?;
        let (mut seen, layer) = match set {
            LayerSet::Gazetteer => gazetteer(&rig, db, samples)?,
            LayerSet::L3Brief => {
                let (total, layer) = brief(&rig, db, samples, args.warmup)?;
                (
                    Observed {
                        total,
                        ..Observed::default()
                    },
                    layer,
                )
            }
            LayerSet::L0 | LayerSet::L2Query => unreachable!("handled above"),
        };
        seen.baseline = baseline;
        (seen, layer)
    };
    let row = measurement(set, &seen, &rows)?;
    let status = if args.n == CAMPAIGN_NOTES {
        "campaign"
    } else {
        "harness_only"
    };
    envelope(
        args.n,
        json!({ "mode": "layers", "layer_measurements": [row], "layer": layer,
        "process_vm_hwm_bytes": process_hwm, "effective_budgets": rows, "overrides": overrides,
        "hwm_basis": "baseline: VmHWM reset (clear_refs 5) after the build, then the L0 workload; \
            total: VmHWM after the layer workload in the same process (l0: whole process)",
        "corpus": { "seed": format!("{SEED:#x}"), "notes": args.n,
            "edges_per_node": args.edges_per_node, "mapped": map.entries.len() },
        "fixtures": { "sources_sha256": hash(&args.sources)?, "queries_sha256": hash(&args.judged)?,
            "edges_sha256": hash(&args.edge_judged)? },
        "deferred": deferred,
        "layer_gate": { "status": status, "pass": row.pass } }),
        "layers",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::recall_classes::EdgeJudgment;
    use super::*;

    fn budget_rows(engine: &EngineConfig) -> Vec<BudgetRow> {
        budgets(engine, &RunFlags::default())
            .into_iter()
            .map(BudgetRow::from)
            .collect()
    }

    #[test]
    fn bounds_pass_within_and_fail_over_the_effective_budget() {
        let rows = budget_rows(&EngineConfig::default());
        // l0: the previously observed 56 MiB passes; over 64 MiB (still under 100 MiB) fails.
        let l0 = |total| Observed {
            total,
            baseline: total,
            ..Observed::default()
        };
        assert_eq!(
            measurement(LayerSet::L0, &l0(56 * MIB), &rows)
                .unwrap()
                .pass,
            Some(true)
        );
        assert_eq!(
            measurement(LayerSet::L0, &l0(65 * MIB), &rows)
                .unwrap()
                .pass,
            Some(false)
        );
        // An override raises the bound but never the 100 MiB promise.
        let mut engine = EngineConfig::default();
        engine.budget.l0_rss_mb = 200;
        let raised = budget_rows(&engine);
        assert_eq!(
            measurement(LayerSet::L0, &l0(101 * MIB), &raised)
                .unwrap()
                .pass,
            Some(false)
        );
        // gazetteer: both figures within 25 MiB from an `ok` run; skipped_budget fails.
        let gazetteer = Observed {
            build_peak: Some(20 * MIB),
            retained: Some(21 * MIB),
            outcome_ok: true,
            ..Observed::default()
        };
        assert_eq!(
            measurement(LayerSet::Gazetteer, &gazetteer, &rows)
                .unwrap()
                .pass,
            Some(true)
        );
        for bad in [
            Observed {
                build_peak: Some(26 * MIB),
                ..gazetteer
            },
            Observed {
                outcome_ok: false,
                ..gazetteer
            },
            Observed {
                retained: None,
                ..gazetteer
            },
        ] {
            assert_eq!(
                measurement(LayerSet::Gazetteer, &bad, &rows).unwrap().pass,
                Some(false)
            );
        }
        // L3 is recorded, never a fabricated cache pass.
        let l3 = measurement(LayerSet::L3Brief, &l0(10 * MIB), &rows).unwrap();
        assert_eq!((l3.pass, l3.cache_bytes, l3.bound_key), (None, None, None));
        assert!(LayerSet::parse("l0+cache").is_err());
    }

    #[test]
    fn producer_pairs_judge_mapped_targets_and_refuse_foreign_origins() {
        let judged = |to: &str, correct| EdgeJudgment {
            id: to.into(),
            from: "w:a".into(),
            to: to.into(),
            workspace: "ws1".into(),
            rel: "MENTIONS".into(),
            correct,
            evidence: "synthetic".into(),
            provenance: "test".into(),
        };
        let judgments = [judged("t:right", true), judged("t:wrong", false)];
        let origin = BTreeMap::from([("chunk-a".to_owned(), "w:a".to_owned())]);
        let reverse = BTreeMap::from([
            (("ws1", "node-right"), Some("t:right")),
            (("ws1", "node-wrong"), Some("t:wrong")),
            (("ws1", "node-ambiguous"), None),
        ]);
        let edge = |id: &str, from: &str, to: &str| (id.to_owned(), from.to_owned(), to.to_owned());
        let edges = [
            edge("e1", "chunk-a", "node-right"),
            edge("e1", "chunk-a", "node-right"),
            edge("e2", "chunk-a", "node-wrong"),
            edge("e3", "chunk-a", "node-ambiguous"),
            edge("e4", "chunk-a", "node-unknown"),
        ];
        let pairs = producer_pairs("ws1", &origin, &reverse, &edges).unwrap();
        let precision = edge_precision(&judgments, &pairs).unwrap();
        assert_eq!(
            (
                precision.predicted,
                precision.judged_predicted,
                precision.correct,
                precision.unjudged
            ),
            (4, 2, 1, 2)
        );
        assert_eq!(precision.precision, Some(0.5));
        assert_eq!(precision.coverage, Some(0.5));
        // No producer edge: unavailable precision, never a perfect score.
        let empty = edge_precision(&judgments, &[]).unwrap();
        assert_eq!((empty.precision, empty.coverage), (None, None));
        // An edge from a chunk outside the materialized origin is refused, not relabeled.
        assert!(
            producer_pairs(
                "ws1",
                &origin,
                &reverse,
                &[edge("e5", "chunk-foreign", "node-right")]
            )
            .is_err()
        );
    }
}
