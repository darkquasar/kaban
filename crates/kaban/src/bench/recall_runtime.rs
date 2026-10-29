//! E3.5: stable fixture identities, actual service ranking and paired API evidence.
//! This harness never changes judgments or prefilters the service's candidate pool.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use kaban_api::{
    ApiConfig, EdgeFilter, ForgetRequest, KabanApi, LinkRequest, RecallRequest, RememberRequest,
    RememberStatus, TraverseRequest, WhyRequest, WhyTarget,
};
use kaban_core::{
    Actor, ActorId, ForgetMode, Governance, GraphStore, NewEvent, NodeId, TraverseDirection,
    WorkspaceId, WriteCtx, config::EngineConfig,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::corpus::{Corpus, Kind, Life, RELS, SEED, WORKSPACES, fixture_repo};
use super::recall_classes::{Fixtures, JudgedQuery, Pair, QueryClass, edge_precision, ndcg_at_10};

/// Recall runs require a new database; `--n` is small only for harness checks.
#[derive(Debug, Args)]
pub(crate) struct RecallArgs {
    #[arg(
        long,
        default_value = "exact_id,error_string,natural_language,identifier"
    )]
    pub classes: String,
    /// Semicolon-separated layer sets (`fts`, `fuzzy`); sim is unavailable.
    #[arg(long, default_value = "fts;fts,fuzzy")]
    pub layers: String,
    /// Verify the effective configured RRF selection; never override it per request.
    #[arg(long)]
    pub fusion: Option<String>,
    #[arg(long, default_value = "fixtures/judged/queries.jsonl")]
    pub judged: PathBuf,
    #[arg(long, default_value = "fixtures/judged/walkthrough.jsonl")]
    pub sources: PathBuf,
    #[arg(long, default_value = "fixtures/judged/edges.jsonl")]
    pub edge_judged: PathBuf,
    #[arg(long, default_value_t = 100_000)]
    pub n: usize,
    #[arg(long, default_value_t = 3)]
    pub edges_per_node: usize,
    #[arg(long, default_value_t = 30)]
    pub samples: usize,
    #[arg(long, default_value_t = 3)]
    pub warmup: usize,
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Measure one EN-N3 layer set in this fresh process instead of the recall evaluation:
    /// l0, l0+trigram, l0+gazetteer-run, l0+l3-brief, or l0+l2-query (deferred row)
    #[arg(long, value_name = "LAYER_SET")]
    pub measure: Option<String>,
}

/// Edge evaluation reads an existing corpus with the exact retained identity map.
#[derive(Debug, Args)]
pub(crate) struct EdgesArgs {
    #[arg(long, default_value = "gazetteer")]
    pub layer: String,
    #[arg(long, default_value = "fixtures/judged/edges.jsonl")]
    pub judged: PathBuf,
    #[arg(long, default_value = "fixtures/judged/queries.jsonl")]
    pub queries: PathBuf,
    #[arg(long, default_value = "fixtures/judged/walkthrough.jsonl")]
    pub sources: PathBuf,
    /// The recall result that materialized this corpus, not a generated UUID lookup.
    #[arg(long)]
    pub map: PathBuf,
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceIdentity {
    pub source: String,
    pub workspace: String,
    pub node_id: String,
    pub outcome: String,
}

/// A many-to-one dedup outcome is retained. Ambiguous reverse identities stay unjudged.
#[derive(Debug, Default)]
pub(crate) struct SourceMap {
    pub entries: Vec<SourceIdentity>,
}

impl SourceMap {
    pub(super) fn resolve(&self, workspace: &str, source: &str) -> Result<&str> {
        self.entries
            .iter()
            .find(|entry| entry.source == source && entry.workspace == workspace)
            .map(|entry| entry.node_id.as_str())
            .context("unmapped or foreign stable source")
    }

    pub(super) fn reverse(&self) -> BTreeMap<(&str, &str), Option<&str>> {
        let mut reverse = BTreeMap::new();
        for entry in &self.entries {
            reverse
                .entry((entry.workspace.as_str(), entry.node_id.as_str()))
                .and_modify(|source| *source = None)
                .or_insert(Some(entry.source.as_str()));
        }
        reverse
    }

    fn verify(&self, store: &dyn GraphStore, fixtures: &Fixtures) -> Result<()> {
        let mut identities = BTreeSet::new();
        for entry in &self.entries {
            ensure!(
                identities.insert((&entry.workspace, &entry.source)),
                "duplicate source mapping"
            );
            let node = store
                .get(&NodeId::parse(&entry.node_id)?, true)?
                .context("mapped node missing")?;
            ensure!(
                node.workspace_id.as_str() == entry.workspace,
                "foreign mapped node"
            );
        }
        for source in &fixtures.sources {
            let id = self.resolve(&source.workspace, &source.id)?;
            let node = store
                .get(&NodeId::parse(id)?, true)?
                .context("catalog node missing")?;
            ensure!(node.text == source.text, "catalog text mismatch");
        }
        Ok(())
    }
}

pub(super) fn load(
    sources: &Path,
    queries: &Path,
    edges: &Path,
    minimum: usize,
) -> Result<Fixtures> {
    Fixtures::load(
        &std::fs::read_to_string(sources).context("reading sources")?,
        &std::fs::read_to_string(queries).context("reading judged queries")?,
        &std::fs::read_to_string(edges).context("reading judged edges")?,
        minimum,
    )
}

fn class_name(class: QueryClass) -> &'static str {
    match class {
        QueryClass::ExactId => "exact_id",
        QueryClass::ErrorString => "error_string",
        QueryClass::NaturalLanguage => "natural_language",
        QueryClass::Identifier => "identifier",
    }
}

fn classes(input: &str) -> Result<BTreeSet<QueryClass>> {
    let mut result = BTreeSet::new();
    for name in input.split(',') {
        let class = match name {
            "exact_id" => QueryClass::ExactId,
            "error_string" => QueryClass::ErrorString,
            "natural_language" => QueryClass::NaturalLanguage,
            "identifier" => QueryClass::Identifier,
            _ => bail!("unknown query class"),
        };
        ensure!(result.insert(class), "duplicate query class");
    }
    Ok(result)
}

fn layer_sets(input: &str) -> Result<Vec<Vec<kaban_core::RecallLayer>>> {
    use kaban_core::RecallLayer;
    let mut result = Vec::new();
    let mut names = BTreeSet::new();
    for set in input.split(';') {
        let mut layers = Vec::new();
        for name in set.split(',') {
            let layer = match name {
                "fts" => RecallLayer::Fts,
                "fuzzy" => RecallLayer::Fuzzy,
                "sim" | "embed" => bail!("not_available_in_this_release: sim/embed"),
                _ => bail!("unknown recall layer"),
            };
            ensure!(!layers.contains(&layer), "duplicate recall layer");
            layers.push(layer);
        }
        ensure!(
            layers.contains(&RecallLayer::Fts),
            "evaluation requires fts"
        );
        layers.sort_by_key(|layer| match layer {
            RecallLayer::Fts => 0,
            RecallLayer::Fuzzy => 1,
            RecallLayer::Sim => 2,
        });
        ensure!(
            names.insert(serde_json::to_string(&layers)?),
            "duplicate layer set"
        );
        result.push(layers);
    }
    Ok(result)
}

fn check_fusion(selection: Option<&str>, engine: &EngineConfig) -> Result<u32> {
    let k = engine.recall.effective_rrf_k()?;
    if let Some(selection) = selection {
        ensure!(
            !selection.starts_with("cc"),
            "not_available_in_this_release: cc"
        );
        let selected = selection
            .strip_prefix("rrf:")
            .and_then(|value| value.parse::<u32>().ok())
            .or_else(|| {
                selection
                    .strip_prefix("rrf(")
                    .and_then(|value| value.strip_suffix(')'))
                    .and_then(|value| value.parse::<u32>().ok())
            })
            .context("invalid fusion selection")?;
        ensure!(
            selected == k,
            "fusion selection differs from effective recall configuration"
        );
    }
    Ok(k)
}

/// Replace every exact-id placeholder before calling recall; foreign/unmapped refs refuse.
pub(crate) fn query_text(query: &JudgedQuery, map: &SourceMap) -> Result<String> {
    let mut input = query.query.as_str();
    let mut output = String::new();
    while let Some((before, rest)) = input.split_once("{node_id:") {
        output.push_str(before);
        let (source, after) = rest
            .split_once('}')
            .context("unterminated node-id template")?;
        output.push_str(map.resolve(&query.workspace, source)?);
        input = after;
    }
    output.push_str(input);
    Ok(output)
}

/// Build the same note/request RNG stream as the legacy corpus; never call its deleting runner.
pub(super) fn materialize(
    api: &KabanApi,
    store: &dyn GraphStore,
    fixtures: &Fixtures,
    n: usize,
    edges_per_node: usize,
    corpus: &mut Corpus,
    commit: &str,
) -> Result<SourceMap> {
    let catalog: BTreeMap<_, _> = fixtures
        .sources
        .iter()
        .map(|source| (source.id.as_str(), source))
        .collect();
    let mut map = SourceMap::default();
    let mut by_ws: Vec<Vec<String>> = vec![Vec::new(); WORKSPACES.len()];
    let mut lifecycle = Vec::new();
    let mut decisions = Vec::new();
    for ordinal in 0..n {
        let (spec, request) = super::runner::corpus_request(corpus, ordinal);
        let stable = format!("corpus:{ordinal}");
        if let Some(source) = catalog.get(stable.as_str()) {
            ensure!(
                source.text == spec.text && source.workspace == spec.workspace,
                "synthetic source does not match seed/ordinal corpus"
            );
        }
        let out = api.remember(request)?;
        map.entries.push(SourceIdentity {
            source: stable,
            workspace: spec.workspace.into(),
            node_id: out.id.clone(),
            outcome: format!("{:?}", out.status).to_lowercase(),
        });
        if out.status != RememberStatus::Created {
            continue;
        }
        let ws = WORKSPACES
            .iter()
            .position(|(name, _)| *name == spec.workspace)
            .context("unknown corpus workspace")?;
        by_ws[ws].push(out.id.clone());
        if spec.kind == Kind::Decision && spec.accepted {
            decisions.push((out.id.clone(), spec.workspace));
        }
        match spec.life {
            Life::Active => {}
            Life::Archived => lifecycle.push((out.id, ForgetMode::Archive)),
            Life::Tombstoned => lifecycle.push((out.id, ForgetMode::Tombstone)),
        }
    }
    for (id, workspace) in decisions {
        store.transition(
            &NodeId::parse(&id)?,
            Governance::Accepted,
            NewEvent {
                reason: "bench-accept".into(),
                commit_sha: Some(commit.into()),
                ..Default::default()
            },
            &WriteCtx::new(
                api.policy_version(),
                WorkspaceId::new(workspace),
                Actor::Agent,
            ),
        )?;
    }
    for ordinal in 0..n
        .checked_mul(edges_per_node)
        .context("corpus edge count overflow")?
    {
        let workspace = corpus.note_workspace_for_link();
        let ws = &by_ws[WORKSPACES
            .iter()
            .position(|(name, _)| *name == workspace)
            .context("unknown corpus workspace")?];
        if ws.len() < 2 {
            continue;
        }
        let from = ws[corpus.rng.below(ws.len())].clone();
        let to = ws[corpus.rng.below(ws.len())].clone();
        match api.link(LinkRequest {
            from,
            to,
            rel: RELS[ordinal % RELS.len()].into(),
            confidence: Some(0.9),
            props: None,
        }) {
            Ok(_) | Err(kaban_api::ApiError::NotFound(_) | kaban_api::ApiError::Store(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    for (id, mode) in lifecycle {
        api.forget(ForgetRequest {
            id,
            mode: Some(mode),
        })?;
    }
    for source in &fixtures.sources {
        if source.id.starts_with("corpus:") {
            map.resolve(&source.workspace, &source.id)?;
            continue;
        }
        let out = api.remember(RememberRequest {
            kind: "Fact".into(),
            text: source.text.clone(),
            workspace_id: Some(source.workspace.clone()),
            ..Default::default()
        })?;
        map.entries.push(SourceIdentity {
            source: source.id.clone(),
            workspace: source.workspace.clone(),
            node_id: out.id,
            outcome: format!("{:?}", out.status).to_lowercase(),
        });
    }
    map.verify(store, fixtures)?;
    Ok(map)
}

/// Hit order is the service order, including ties; unknown identities stay unjudged.
pub(crate) fn evaluate(
    api: &KabanApi,
    fixtures: &Fixtures,
    map: &SourceMap,
    selected: &BTreeSet<QueryClass>,
    layers: &[kaban_core::RecallLayer],
) -> Result<Value> {
    let reverse = map.reverse();
    let mut rows = Vec::new();
    let mut scores: BTreeMap<QueryClass, Vec<Option<f64>>> = BTreeMap::new();
    for query in fixtures
        .queries
        .iter()
        .filter(|query| selected.contains(&query.class))
    {
        for judgment in &query.judgments {
            map.resolve(&query.workspace, &judgment.target)?;
        }
        let text = query_text(query, map)?;
        let response = api.recall(RecallRequest {
            query: text.clone(),
            layers: Some(layers.to_vec()),
            fuzzy: layers.contains(&kaban_core::RecallLayer::Fuzzy),
            workspace_id: Some(query.workspace.clone()),
            limit: Some(10),
            ..Default::default()
        })?;
        let stable_hits: Vec<String> = response
            .results
            .iter()
            .map(|card| {
                reverse
                    .get(&(query.workspace.as_str(), card.id.as_str()))
                    .copied()
                    .flatten()
                    .map_or_else(|| format!("unmapped-node:{}", card.id), str::to_owned)
            })
            .collect();
        let score = ndcg_at_10(&query.judgments, &stable_hits)?;
        scores.entry(query.class).or_default().push(score.score);
        rows.push(json!({ "id": query.id, "class": class_name(query.class), "workspace": query.workspace,
            "query": text, "node_ids": response.results.iter().map(|card| &card.id).collect::<Vec<_>>(),
            "hits": stable_hits, "ndcg_at_10": score.score, "zero_ideal": score.zero_ideal,
            "judged_hits": score.judged_hits, "unjudged_hits": score.unjudged_hits, "duplicates": score.duplicate_hits }));
    }
    let classes: Vec<_> = scores.into_iter().map(|(class, scores)| {
        let available: Vec<_> = scores.iter().flatten().copied().collect();
        json!({ "class": class_name(class), "evaluated": scores.len(), "available": available.len(),
            "unavailable": scores.len() - available.len(), "mean_ndcg_at_10":
                (!available.is_empty()).then(|| available.iter().sum::<f64>() / available.len() as f64) })
    }).collect();
    Ok(json!({ "layers": layers, "status": "evaluated", "queries": rows, "classes": classes }))
}

/// Actual gazetteer-projected MENTIONS only; no candidate judgment is emitted as a prediction.
pub(crate) fn evaluate_edges(
    api: &KabanApi,
    store: &dyn GraphStore,
    fixtures: &Fixtures,
    map: &SourceMap,
) -> Result<Value> {
    map.verify(store, fixtures)?;
    let reverse = map.reverse();
    let mut predictions = Vec::new();
    let mut edge_ids = BTreeSet::new();
    let mut truncated = false;
    for entry in &map.entries {
        let node = store
            .get(&NodeId::parse(&entry.node_id)?, true)?
            .context("mapped source missing")?;
        if node.lifecycle != kaban_core::Lifecycle::Active {
            continue;
        }
        let result = api.traverse(TraverseRequest {
            start_ids: vec![entry.node_id.clone()],
            workspace_id: Some(entry.workspace.clone()),
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
        for edge in result.edges {
            if !edge_ids.insert(edge.id) {
                continue;
            }
            let stable = |id: &str| {
                reverse
                    .get(&(entry.workspace.as_str(), id))
                    .copied()
                    .flatten()
                    .map_or_else(|| format!("unmapped-node:{id}"), str::to_owned)
            };
            predictions.push(Pair {
                workspace: entry.workspace.clone(),
                from: stable(&edge.from),
                to: stable(&edge.to),
            });
        }
    }
    let precision = edge_precision(&fixtures.edges, &predictions)?;
    Ok(
        json!({ "layer": "gazetteer", "rel": "MENTIONS", "precision": precision.precision,
        "coverage": precision.coverage, "judged": precision.judged, "predicted": precision.predicted,
        "judged_predicted": precision.judged_predicted, "correct": precision.correct,
        "unjudged": precision.unjudged, "duplicates": precision.duplicates,
        "truncated": truncated, "scope": "retained mapped sources; active API projection",
        "qualification": "informational; producer precision qualification belongs to E3.12" }),
    )
}

fn timed<T>(operation: impl FnOnce() -> Result<T>) -> Result<f64> {
    let start = Instant::now();
    operation()?;
    Ok(start.elapsed().as_secs_f64() * 1e6)
}

/// N2 is only qualified at 100k; the same arithmetic is exercised by small harness tests.
/// `fuzzy` is whether the fused request's fuzzy layer was answered by an index (the Tantivy
/// adapter; the in-database FTS5 index has none since the trigram index was retired, spec 05
/// SS-16): the key keeps its historical name `required_trigram_available` for the validator.
pub(crate) fn n2_gates(recall: f64, traverse: f64, notes: usize, fuzzy: bool) -> Value {
    json!({ "recall_fused": { "value_us": recall, "limit_us": 10000, "pass": fuzzy && recall < 10000.0 },
        "traverse_filtered_depth2": { "value_us": traverse, "limit_us": 1000, "pass": traverse < 1000.0 },
        "required_trigram_available": fuzzy, "qualified_corpus": notes == 100_000,
        "pass": notes == 100_000 && fuzzy && recall < 10000.0 && traverse < 1000.0,
        "status": if notes == 100_000 { "campaign" } else { "harness_only" } })
}

fn paired(
    api: &KabanApi,
    fixtures: &Fixtures,
    map: &SourceMap,
    samples: usize,
    warmup: usize,
    notes: usize,
    fuzzy: bool,
) -> Result<Value> {
    let query = fixtures
        .queries
        .iter()
        .find(|query| query.class == QueryClass::Identifier)
        .context("identifier timing query missing")?;
    let request = RecallRequest {
        query: query_text(query, map)?,
        workspace_id: Some(query.workspace.clone()),
        limit: Some(10),
        fuzzy: true,
        ..Default::default()
    };
    let mut fts = request.clone();
    fts.layers = Some(vec![kaban_core::RecallLayer::Fts]);
    fts.fuzzy = false;
    let mut fused = request;
    fused.layers = Some(vec![
        kaban_core::RecallLayer::Fts,
        kaban_core::RecallLayer::Fuzzy,
    ]);
    let traverse = TraverseRequest {
        start_ids: vec![map.resolve("ws1", "corpus:0")?.into()],
        workspace_id: Some("ws1".into()),
        max_depth: Some(2),
        limit: Some(100),
        node_kinds: vec!["Fact".into(), "Failure".into(), "Idea".into()],
        rels: vec!["RELATES_TO".into()],
        ..Default::default()
    };
    let why = WhyRequest {
        targets: vec![WhyTarget::Path("src/checkout/cart.rs".into())],
        limit: Some(10),
        ..Default::default()
    };
    for _ in 0..warmup {
        api.recall(fts.clone())?;
        api.recall(fused.clone())?;
        api.traverse(traverse.clone())?;
        api.why(why.clone())?;
    }
    let mut fts_us = Vec::new();
    let mut fused_us = Vec::new();
    let mut traverse_us = Vec::new();
    let mut why_us = Vec::new();
    let mut rounds = Vec::new();
    for round in 0..samples {
        if round % 2 == 0 {
            fts_us.push(timed(|| Ok(api.recall(fts.clone())?))?);
            fused_us.push(timed(|| Ok(api.recall(fused.clone())?))?);
            rounds.push("fts_first");
        } else {
            fused_us.push(timed(|| Ok(api.recall(fused.clone())?))?);
            fts_us.push(timed(|| Ok(api.recall(fts.clone())?))?);
            rounds.push("fused_first");
        }
        traverse_us.push(timed(|| Ok(api.traverse(traverse.clone())?))?);
        why_us.push(timed(|| Ok(api.why(why.clone())?))?);
    }
    let requests = json!([fts, fused]);
    let fts = super::report::raw_series(&fts_us);
    let fused = super::report::raw_series(&fused_us);
    let traversal = super::report::raw_series(&traverse_us);
    let gates = n2_gates(
        fused["p50_us"].as_f64().context("missing p50")?,
        traversal["p50_us"].as_f64().context("missing p50")?,
        notes,
        fuzzy,
    );
    Ok(
        json!({ "samples": samples, "warmup": warmup, "round_order": rounds,
        "recall_fts": fts, "recall_fused": fused, "traverse": traversal,
        "traverse_request": traverse, "why": super::report::raw_series(&why_us),
        "why_informational": true, "n2": gates, "recall_requests": requests }),
    )
}

fn command(program: &str, args: &[&str]) -> Result<String> {
    use kaban_core::process::{EnvAllowlist, Exec, InheritedFds, Spec};
    let mut env = EnvAllowlist::new();
    for name in [
        "PATH",
        "HOME",
        "RUSTUP_HOME",
        "CARGO_HOME",
        "RUSTUP_TOOLCHAIN",
    ] {
        env.carry(name)?;
    }
    let output = kaban_core::process::run(Spec {
        exec: Exec::Path(program.into()),
        args: args.iter().map(std::ffi::OsString::from).collect(),
        cwd: Path::new("."),
        env,
        deadline: Duration::from_secs(5),
        stdout_cap: 4096,
        stderr_cap: 4096,
        monitors: Vec::new(),
        join_bound: Duration::from_secs(1),
        // Like GitRunner, this internal identity command can run under a flock launcher.
        // Its environment is still the allowlist above; the runner bounds the entire group.
        inherited_fds: InheritedFds::Tolerate,
    })
    .with_context(|| format!("running {program}"))?;
    ensure!(
        output.status.is_some_and(|status| status.success())
            && output.killed.is_none()
            && !output.descendants_survived,
        "benchmark identity command failed"
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub(super) fn hash(path: &Path) -> Result<String> {
    Ok(kaban_policy::sha256_hex(
        &std::fs::read(path).context("hashing benchmark input")?,
    ))
}

pub(super) fn envelope(notes: usize, section: Value, kind: &str) -> Result<Value> {
    let binary = std::env::current_exe()?;
    let mut machine = super::report::machine_profile();
    machine["rustc"] = json!(command("rustc", &["--version"])?);
    machine["cargo"] = json!(command("cargo", &["--version"])?);
    machine["git_head"] = json!(command("git", &["rev-parse", "HEAD"])?);
    machine["binary_sha256"] = json!(hash(&binary)?);
    machine["binary_bytes"] = json!(std::fs::metadata(binary)?.len());
    let mut result = json!({ "bench_schema": 3, "evaluation_kind": kind,
        "date": command("date", &["+%F"])? , "n_nodes": notes, "runs": 1,
        "machine": machine, "gate_values": {}, "max_p50_rel_spread": 0.0,
        "per_op": {}, "raw_runs": [] });
    result["recall_evaluation"] = section;
    Ok(result)
}

fn recall(
    db: &Path,
    keys: &Path,
    actor: &ActorId,
    policy: Policy,
    engine: &EngineConfig,
    args: &RecallArgs,
) -> Result<Value> {
    let selected = classes(&args.classes)?;
    let sets = layer_sets(&args.layers)?;
    let k = check_fusion(args.fusion.as_deref(), engine)?;
    let fixtures = load(
        &args.sources,
        &args.judged,
        &args.edge_judged,
        engine.recall.judged_min_queries as usize,
    )?;
    ensure!(
        args.n >= 5 && args.warmup > 0,
        "corpus requires at least five notes and warmup"
    );
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db.as_os_str().to_os_string();
        path.push(suffix);
        ensure!(
            !Path::new(&path).exists(),
            "recall bench requires a fresh database"
        );
    }
    let fixture = db.with_file_name("e3-recall-fixture-repo");
    ensure!(
        !fixture.exists(),
        "recall bench requires a fresh fixture repository"
    );
    let store = SqliteStore::open_as(db, keys, actor)?.with_recall_config(engine.recall.clone())?;
    let sqlite = Arc::new(store);
    let mut corpus = Corpus::new(SEED);
    let commit = fixture_repo(&fixture, &corpus)?;
    let config = ApiConfig {
        workspace: WorkspaceId::new("ws1"),
        repo_root: Some(fixture),
        repo: Some("bench-fixture-repo".into()),
        ..Default::default()
    };
    let api = KabanApi::with_config(
        sqlite.clone(),
        policy,
        sqlite.signer(),
        None,
        config.clone(),
    );
    let map = materialize(
        &api,
        sqlite.as_ref(),
        &fixtures,
        args.n,
        args.edges_per_node,
        &mut corpus,
        &commit,
    )?;
    let rows = sets
        .iter()
        .map(|layers| evaluate(&api, &fixtures, &map, &selected, layers))
        .collect::<Result<Vec<_>>>()?;
    let samples = args.samples.max(30).max(engine.bench.min_samples as usize);
    // the fuzzy layer of the fused request is the store's own index's
    let fuzzy = true;
    let timing = paired(&api, &fixtures, &map, samples, args.warmup, args.n, fuzzy)?;
    let mut query_counts = BTreeMap::new();
    for query in &fixtures.queries {
        *query_counts
            .entry(class_name(query.class))
            .or_insert(0usize) += 1;
    }
    envelope(
        args.n,
        json!({ "mode": "recall", "text_index": "tantivy",
        "selected_classes": selected.iter().map(|class| class_name(*class)).collect::<Vec<_>>(),
        "selected_layer_sets": sets, "effective_config": engine, "effective_fusion": format!("rrf({k})"),
        "minimum_queries": (engine.recall.judged_min_queries as usize).max(100),
        "minimum_samples": (engine.bench.min_samples as usize).max(30),
        "corpus": { "seed": format!("{SEED:#x}"), "notes": args.n, "edges_per_node": args.edges_per_node,
            "mapped": map.entries.len(), "source_map": map.entries },
        "fixtures": { "sources_sha256": hash(&args.sources)?, "queries_sha256": hash(&args.judged)?,
            "edges_sha256": hash(&args.edge_judged)?, "query_count": fixtures.queries.len(),
            "query_counts_by_class": query_counts, "edge_count": fixtures.edges.len() },
        "layer_sets": rows, "paired": timing,
        "availability": { "fuzzy": "text_index", "sim": "not_available_in_this_release", "embed": "not_available_in_this_release", "cc": "not_available_in_this_release" },
        "quality_informational": true }),
        "recall",
    )
}

fn edges(
    db: &Path,
    keys: &Path,
    actor: &ActorId,
    policy: Policy,
    engine: &EngineConfig,
    args: &EdgesArgs,
) -> Result<Value> {
    ensure!(
        args.layer == "gazetteer",
        "not_available_in_this_release: edge layer"
    );
    ensure!(
        db.is_file(),
        "edge evaluation requires an existing database"
    );
    let fixtures = load(
        &args.sources,
        &args.queries,
        &args.judged,
        engine.recall.judged_min_queries as usize,
    )?;
    let input: Value = serde_json::from_slice(&std::fs::read(&args.map)?)?;
    let entries: Vec<SourceIdentity> =
        serde_json::from_value(input["recall_evaluation"]["corpus"]["source_map"].clone())?;
    let map = SourceMap { entries };
    let sqlite = Arc::new(SqliteStore::open_as(db, keys, actor)?);
    let api = KabanApi::with_config(
        sqlite.clone(),
        policy,
        sqlite.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new("ws1"),
            ..Default::default()
        },
    );
    let precision = evaluate_edges(&api, sqlite.as_ref(), &fixtures, &map)?;
    envelope(
        map.entries.len(),
        json!({ "mode": "edges", "edge_precision": precision,
        "fixtures": { "sources_sha256": hash(&args.sources)?, "queries_sha256": hash(&args.queries)?,
            "edges_sha256": hash(&args.judged)? }, "source_map_sha256": hash(&args.map)? }),
        "edges",
    )
}

pub(crate) fn dispatch(
    db: &Path,
    keys: &Path,
    actor: &ActorId,
    policy: Policy,
    engine: &EngineConfig,
    command: Option<&crate::BenchCg>,
) -> ExitCode {
    let requested_out = match command {
        Some(crate::BenchCg::Recall(args)) => args.out.as_deref(),
        Some(crate::BenchCg::Edges(args)) => args.out.as_deref(),
        _ => None,
    };
    if requested_out.is_some_and(Path::exists) {
        crate::cli::output::print_failure(&"benchmark output already exists");
        return ExitCode::from(4);
    }
    let (result, out) = match command {
        Some(crate::BenchCg::Recall(args)) => (
            match args.measure.as_deref() {
                None => recall(db, keys, actor, policy, engine, args),
                #[cfg(feature = "ingest")]
                Some(set) => super::layers::run(db, keys, actor, policy, engine, args, set),
                #[cfg(not(feature = "ingest"))]
                Some(_) => Err(anyhow::anyhow!(
                    "layer measurement needs a kaban built with the `ingest` feature"
                )),
            },
            args.out.as_deref(),
        ),
        Some(crate::BenchCg::Edges(args)) => (
            edges(db, keys, actor, policy, engine, args),
            args.out.as_deref(),
        ),
        _ => return ExitCode::from(4),
    };
    match result.and_then(|result| publish(&result, out)) {
        Ok(code) => code,
        Err(error) => {
            crate::cli::output::print_failure(&error);
            ExitCode::from(4)
        }
    }
}

/// Retain complete output before returning a binding N2 failure.
fn publish(result: &Value, out: Option<&Path>) -> Result<ExitCode> {
    if let Some(out) = out {
        ensure!(!out.exists(), "benchmark output already exists");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(out)?;
        serde_json::to_writer_pretty(file, result)?;
    }
    crate::cli::output::print_json_pretty(result)?;
    let n2 = &result["recall_evaluation"]["paired"]["n2"];
    // A layer row fails only when asserted (informational and deferred rows carry `null`).
    let layers = &result["recall_evaluation"]["layer_gate"];
    Ok(
        if (n2["status"] == "campaign" && n2["pass"] != true)
            || (layers["status"] == "campaign" && layers["pass"] == false)
        {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::recall_classes::{EdgeJudgment, Source};
    use super::*;
    use kaban_core::{
        BatchAssertion, BatchSubject, ClaimClass, Fixed, IngestStore, Layer, Method, Rank,
        WriteBatch,
    };

    fn fixtures() -> Fixtures {
        Fixtures::load(
            include_str!("../../../../fixtures/judged/walkthrough.jsonl"),
            include_str!("../../../../fixtures/judged/queries.jsonl"),
            include_str!("../../../../fixtures/judged/edges.jsonl"),
            100,
        )
        .unwrap()
    }

    fn service() -> (Arc<SqliteStore>, KabanApi) {
        let store = Arc::new(SqliteStore::open_memory().unwrap());
        let api = KabanApi::with_config(
            store.clone(),
            Policy::empty(),
            store.signer(),
            None,
            ApiConfig {
                workspace: WorkspaceId::new("ws1"),
                ..Default::default()
            },
        );
        (store, api)
    }

    #[test]
    fn stable_mapping_templates_real_service_and_foreign_unmapped_negatives() {
        let (store, api) = service();
        let mut fixtures = fixtures();
        let map = materialize(
            &api,
            store.as_ref(),
            &fixtures,
            5,
            0,
            &mut Corpus::new(SEED),
            "0123456789abcdef0123456789abcdef0123456789",
        )
        .unwrap();
        assert_eq!(
            map.resolve("ws1", "corpus:1").unwrap(),
            map.entries[1].node_id
        );
        assert!(map.resolve("ws2", "corpus:1").is_err());
        let query = &fixtures.queries[0];
        assert_eq!(
            query_text(query, &map).unwrap(),
            map.resolve("ws1", "w:architecture").unwrap()
        );
        let mut broken = query.clone();
        broken.query = "{node_id:missing}".into();
        assert!(query_text(&broken, &map).is_err());
        broken = query.clone();
        broken.workspace = "ws2".into();
        assert!(query_text(&broken, &map).is_err());
        let result = evaluate(
            &api,
            &fixtures,
            &map,
            &classes("identifier").unwrap(),
            &[kaban_core::RecallLayer::Fts],
        )
        .unwrap();
        assert_eq!(result["queries"].as_array().unwrap().len(), 25);
        assert!(
            result["queries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| !row["hits"].as_array().unwrap().is_empty())
        );
        // Unknown real hits are counted, not inserted into the judged population.
        assert!(
            result["queries"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |row| row["unjudged_hits"].as_u64().unwrap() > 0 && row["ndcg_at_10"].is_null()
                )
        );
        fixtures.queries[0].judgments[0].target = "missing".into();
        assert!(
            evaluate(
                &api,
                &fixtures,
                &map,
                &classes("exact_id").unwrap(),
                &[kaban_core::RecallLayer::Fts]
            )
            .is_err()
        );
    }

    #[test]
    fn corpus_catalog_mismatch_and_missing_ordinal_refuse_instead_of_remapping() {
        let (store, api) = service();
        let mut bad = fixtures();
        bad.sources
            .iter_mut()
            .find(|source| source.id == "corpus:0")
            .unwrap()
            .text = "different".into();
        assert!(
            materialize(
                &api,
                store.as_ref(),
                &bad,
                5,
                0,
                &mut Corpus::new(SEED),
                "unused"
            )
            .is_err()
        );
        let (store, api) = service();
        assert!(
            materialize(
                &api,
                store.as_ref(),
                &fixtures(),
                2,
                0,
                &mut Corpus::new(SEED),
                "unused"
            )
            .is_err()
        );
    }

    #[test]
    fn dedup_mapping_retains_actual_id_and_ambiguous_reverse_stays_unjudged() {
        let (store, api) = service();
        let mut fixtures = fixtures();
        let original = fixtures
            .sources
            .iter()
            .find(|source| source.id == "w:architecture")
            .unwrap()
            .clone();
        fixtures.sources.push(Source {
            id: "alias:architecture".into(),
            ..original
        });
        let map = materialize(
            &api,
            store.as_ref(),
            &fixtures,
            5,
            0,
            &mut Corpus::new(SEED),
            "unused",
        )
        .unwrap();
        let id = map.resolve("ws1", "w:architecture").unwrap();
        assert_eq!(map.resolve("ws1", "alias:architecture").unwrap(), id);
        assert_eq!(map.reverse().get(&("ws1", id)), Some(&None));
        assert_ne!(map.entries.last().unwrap().outcome, "created");
    }

    #[test]
    fn synthetic_store_edges_count_correct_wrong_unjudged_and_exclude_human() {
        let (store, api) = service();
        let mut map = SourceMap::default();
        let mut sources = Vec::new();
        for source in ["from", "correct", "wrong", "unknown", "human"] {
            let out = api
                .remember(RememberRequest {
                    kind: "Fact".into(),
                    text: source.into(),
                    workspace_id: Some("ws1".into()),
                    ..Default::default()
                })
                .unwrap();
            map.entries.push(SourceIdentity {
                source: source.into(),
                workspace: "ws1".into(),
                node_id: out.id,
                outcome: "created".into(),
            });
            sources.push(Source {
                id: source.into(),
                workspace: "ws1".into(),
                origin: "synthetic test".into(),
                text: source.into(),
            });
        }
        let src = NodeId::parse(map.resolve("ws1", "from").unwrap()).unwrap();
        let mut batch = WriteBatch::new(WorkspaceId::new("ws1"), vec![]);
        for target in ["correct", "wrong", "unknown"] {
            batch.assertions.push(BatchAssertion {
                subject: BatchSubject::Edge {
                    src: src.clone(),
                    rel: "MENTIONS".into(),
                    dst: NodeId::parse(map.resolve("ws1", target).unwrap()).unwrap(),
                },
                claim_class: ClaimClass::Edge,
                predicate: None,
                object: None,
                value: json!({"method":"exact", "offset":0, "len":3, "score":9000}),
                layer: Layer::Gazetteer,
                method: Method::Exact,
                extractor_id: None,
                extractor_version: Some("1".into()),
                source_id: Some("synthetic".into()),
                lineage_id: format!("synthetic:{target}"),
                raw_score: Fixed(9000),
                rank: Rank::Normal,
                rank_reason: None,
                calibration_version: 0,
                supersedes: None,
            });
        }
        store
            .apply_batch(
                batch,
                &WriteCtx::new(api.policy_version(), WorkspaceId::new("ws1"), Actor::Agent),
            )
            .unwrap();
        api.link(LinkRequest {
            from: src.as_str().into(),
            to: map.resolve("ws1", "human").unwrap().into(),
            rel: "MENTIONS".into(),
            confidence: None,
            props: None,
        })
        .unwrap();
        let edges = [("correct", true), ("wrong", false)]
            .into_iter()
            .map(|(to, correct)| EdgeJudgment {
                id: to.into(),
                from: "from".into(),
                to: to.into(),
                workspace: "ws1".into(),
                rel: "MENTIONS".into(),
                correct,
                evidence: "synthetic assigned judgment".into(),
                provenance: "test".into(),
            })
            .collect();
        let fixtures = Fixtures {
            sources,
            queries: vec![],
            edges,
        };
        let result = evaluate_edges(&api, store.as_ref(), &fixtures, &map).unwrap();
        assert_eq!(result["predicted"], 3);
        assert_eq!(result["judged_predicted"], 2);
        assert_eq!(result["correct"], 1);
        assert_eq!(result["unjudged"], 1);
        assert_eq!(result["precision"], 0.5);
        assert_eq!(result["coverage"], 2.0 / 3.0);
        let mut foreign = map;
        foreign.entries[0].workspace = "ws2".into();
        assert!(evaluate_edges(&api, store.as_ref(), &fixtures, &foreign).is_err());
    }

    #[test]
    fn paired_service_rounds_retain_raw_samples_filters_and_deliberate_n2_failures() {
        let (store, api) = service();
        let fixtures = fixtures();
        let map = materialize(
            &api,
            store.as_ref(),
            &fixtures,
            5,
            1,
            &mut Corpus::new(SEED),
            "unused",
        )
        .unwrap();
        let result = paired(&api, &fixtures, &map, 30, 1, 5, false).unwrap();
        assert_eq!(result["round_order"][0], "fts_first");
        assert_eq!(result["round_order"][1], "fused_first");
        for key in ["recall_fts", "recall_fused", "traverse", "why"] {
            assert_eq!(result[key]["n"], 30);
            assert_eq!(result[key]["us_runs"].as_array().unwrap().len(), 30);
        }
        assert_eq!(result["traverse_request"]["workspace_id"], "ws1");
        assert_ne!(
            result["traverse_request"]["rels"]
                .as_array()
                .unwrap()
                .as_slice(),
            &[] as &[Value]
        );
        assert_ne!(
            result["traverse_request"]["node_kinds"]
                .as_array()
                .unwrap()
                .as_slice(),
            &[] as &[Value]
        );
        assert_eq!(result["n2"]["status"], "harness_only");
        assert_eq!(result["n2"]["required_trigram_available"], false);
        assert_eq!(n2_gates(9_999.0, 999.0, 100_000, true)["pass"], true);
        assert_eq!(n2_gates(10_000.0, 999.0, 100_000, true)["pass"], false);
        assert_eq!(n2_gates(9_999.0, 1000.0, 100_000, true)["pass"], false);
        assert_eq!(n2_gates(1.0, 1.0, 100_000, false)["pass"], false);
    }

    #[test]
    fn binding_n2_failure_is_written_before_nonzero_and_never_overwrites_output() {
        let output = std::env::temp_dir().join(format!(
            "kaban-e35-n2-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let result = json!({"recall_evaluation":{"paired":{"n2":n2_gates(10_000.0, 1000.0, 100_000, true)}}});
        assert_eq!(publish(&result, Some(&output)).unwrap(), ExitCode::from(1));
        let retained: Value = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(retained, result);
        assert!(publish(&json!({"different":true}), Some(&output)).is_err());
        // A failed asserted layer row is binding at 100k; an informational row never is.
        let layer = |status: &str, pass: Value| json!({"recall_evaluation":{"layer_gate":{"status":status,"pass":pass}}});
        assert_eq!(
            publish(&layer("campaign", json!(false)), None).unwrap(),
            ExitCode::from(1)
        );
        for (status, pass) in [
            ("campaign", json!(true)),
            ("campaign", Value::Null),
            ("harness_only", json!(false)),
        ] {
            assert_eq!(
                publish(&layer(status, pass), None).unwrap(),
                ExitCode::SUCCESS
            );
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(output).unwrap()).unwrap(),
            result
        );
    }
}
