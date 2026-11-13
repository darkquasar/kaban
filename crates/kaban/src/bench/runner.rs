//! The benchmark itself: build the corpus through the full service path and time the §16
//! operation set (`why_one_path` is reported not applicable since the `why` tool was removed).

use super::corpus::{Corpus, Kind, Life, RELS, SEED, WORKSPACES, fixture_repo};
use super::report::{machine_profile, not_applicable, self_status_kb, summarize};
use kaban_api::{
    ApiConfig, CodeRefWire, ForgetRequest, KabanApi, LinkRequest, RecallRequest, RememberRequest,
    RememberStatus, TraverseRequest,
};
use kaban_core::{
    Actor, ActorId, ForgetMode, Governance, GraphStore, Location, NewEvent, NodeId, RecallQuery,
    TraverseQuery, WorkspaceId, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Sample every `k`-th latency so about 20,000 samples survive `n` operations.
fn stride(n: usize) -> usize {
    (n / 20_000).max(1)
}

/// Draw a note and its request with the unchanged legacy RNG order, including `CodeRef` draws.
/// The evaluation harness retains the spec before writing, so ordinal identities do not
/// depend on returned ranks or on whether the service creates or deduplicates a node.
pub(super) fn corpus_request(
    corpus: &mut Corpus,
    ordinal: usize,
) -> (super::corpus::NoteSpec, RememberRequest) {
    let spec = corpus.note(ordinal);
    let props = (spec.kind == Kind::Decision).then(|| {
        json!({ "decision": format!("decide {}", spec.tags[0]), "rationale": "benchmark corpus" })
    });
    let refs = if spec.kind == Kind::Decision && spec.coderef {
        vec![CodeRefWire {
            repo: "kaban".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            tool: "bench".into(),
            relative_path: format!("src/{}.rs", corpus.common_term(500)),
            name_path: Some("Store/open".into()),
            kind: Some("Function".into()),
            location: Some(Location {
                line: 42,
                column: 0,
            }),
            ..Default::default()
        }]
    } else {
        Vec::new()
    };
    let req = RememberRequest {
        kind: spec.kind.name().into(),
        text: spec.text.clone(),
        tags: spec.tags.clone(),
        refs,
        path_scope: spec.path_scope.clone(),
        confidence: Some(0.8),
        workspace_id: Some(spec.workspace.into()),
        props,
        ..Default::default()
    };
    (spec, req)
}

/// The corpus shape of one run (the `kaban bench` flags).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    /// Notes to insert.
    pub(crate) notes: usize,
    /// Random edges per note.
    pub(crate) edges_per_node: usize,
    /// Synthetic chunks after the notes (0 = none).
    pub(crate) chunks: usize,
    /// Schema version the corpus is built at.
    pub(crate) schema_version: u32,
}

/// The inputs of the read phase, drawn once from the corpus so the phase can run twice on the
/// same database (before and after migration 0004, E1.7) with identical inputs.
struct ReadPlan {
    gets: Vec<NodeId>,
    traverses: Vec<String>,
    recall_and: Vec<RecallRequest>,
    recall_or_fallback: Vec<RecallRequest>,
    recall_filtered: Vec<RecallRequest>,
    recall_chunks: Option<Vec<RecallRequest>>,
    denylist_short: Vec<String>,
    denylist_long: String,
}

impl ReadPlan {
    const GETS: usize = 5000;
    const TRAVERSES: usize = 1000;
    const RECALLS: usize = 1000;
    const DENYLIST_SHORT: usize = 1000;
    const DENYLIST_LONG: usize = 200;

    fn draw(
        corpus: &mut Corpus,
        visible: &[String],
        visible_ws1: &[String],
        chunks: bool,
    ) -> anyhow::Result<Self> {
        let pick = |c: &mut Corpus, v: &[String]| v[c.rng.below(v.len())].clone();
        let gets = (0..Self::GETS)
            .map(|_| NodeId::parse(&pick(corpus, visible)))
            .collect::<Result<Vec<_>, _>>()?;
        let traverses = (0..Self::TRAVERSES)
            .map(|_| pick(corpus, visible_ws1))
            .collect();
        let draw = |c: &mut Corpus, mk: &dyn Fn(&mut Corpus) -> RecallRequest| {
            (0..Self::RECALLS).map(|_| mk(c)).collect::<Vec<_>>()
        };
        let recall_and = draw(corpus, &|c| RecallRequest {
            query: format!("{} {}", c.common_term(200), c.common_term(200)),
            ..Default::default()
        });
        let recall_or_fallback = draw(corpus, &|c| RecallRequest {
            query: format!("{} {}", c.rare_term(500), c.rare_term(500)),
            ..Default::default()
        });
        let recall_filtered = draw(corpus, &|c| RecallRequest {
            query: format!("{} {}", c.common_term(200), c.common_term(200)),
            kinds: vec!["Fact".into(), "Failure".into()],
            tags: vec![c.common_term(50)],
            workspace_id: Some(WORKSPACES[0].0.into()),
            ..Default::default()
        });
        // the chunk-card path over the mixed corpus (DI1.6 `kinds:["Chunk"]`)
        let recall_chunks = chunks.then(|| {
            draw(corpus, &|c| RecallRequest {
                query: format!("{} {}", c.common_term(200), c.common_term(200)),
                kinds: vec!["Chunk".into()],
                ..Default::default()
            })
        });
        let denylist_short = (0..Self::DENYLIST_SHORT)
            .map(|_| {
                format!(
                    "{} {} {}",
                    corpus.term(),
                    corpus.term(),
                    corpus.identifier()
                )
            })
            .collect();
        // ~20k-char clean input (just under `max_field_chars`, so it is actually scanned)
        let denylist_long = {
            let mut s = String::with_capacity(20_000);
            while s.len() + 24 < 19_990 {
                s.push_str(&corpus.term());
                s.push(' ');
            }
            s
        };
        Ok(Self {
            gets,
            traverses,
            recall_and,
            recall_or_fallback,
            recall_filtered,
            recall_chunks,
            denylist_short,
            denylist_long,
        })
    }

    /// Run the plan: the read ops in §16 order and the N3 high-water mark read right after the
    /// recall phase.
    fn run(
        &self,
        api: &KabanApi,
        store: &dyn GraphStore,
    ) -> anyhow::Result<(Vec<Value>, Option<u64>)> {
        let mut ops = Vec::new();
        let mut lat = Vec::new();
        for id in &self.gets {
            let s = Instant::now();
            let _ = store.get(id, false)?;
            lat.push(s.elapsed().as_secs_f64() * 1e6);
        }
        ops.push(summarize("get_by_id", "store", lat));

        let mut lat = Vec::new();
        let mut lat_store = Vec::new();
        for id in &self.traverses {
            let s = Instant::now();
            let _ = api.traverse(TraverseRequest {
                start_ids: vec![id.clone()],
                max_depth: Some(2),
                limit: Some(100),
                ..Default::default()
            })?;
            lat.push(s.elapsed().as_secs_f64() * 1e6);
            let s = Instant::now();
            let _ = store.traverse(&TraverseQuery {
                start_ids: vec![NodeId::parse(id)?],
                rels: vec![],
                direction: kaban_core::TraverseDirection::Both,
                max_depth: 2,
                node_kinds: vec![],
                governance: vec![],
                limit: 100,
                workspace_id: Some(WORKSPACES[0].0.into()),
                ..Default::default()
            })?;
            lat_store.push(s.elapsed().as_secs_f64() * 1e6);
        }
        ops.push(summarize("traverse_depth2", "api", lat));
        ops.push(summarize("traverse_depth2", "store", lat_store));

        let mut recall =
            |name: &str, store_too: bool, reqs: &[RecallRequest]| -> anyhow::Result<()> {
                let mut lat = Vec::new();
                let mut lat_store = Vec::new();
                let mut returned = 0usize;
                for req in reqs {
                    let q = RecallQuery {
                        layers: req.layers.clone(),
                        fuzzy: req.fuzzy,
                        filters: kaban_core::ReadFilters::default(),
                        query: req.query.clone(),
                        kinds: req.kinds.clone(),
                        tags: req.tags.clone(),
                        workspace_id: req
                            .workspace_id
                            .clone()
                            .or_else(|| Some(WORKSPACES[0].0.into())),
                        limit: 10,
                        hops: 1,
                        include_superseded: false,
                        include_archived: false,
                        r#ref: None,
                    };
                    let s = Instant::now();
                    let r = api.recall(req.clone())?;
                    lat.push(s.elapsed().as_secs_f64() * 1e6);
                    returned += r.count;
                    if store_too {
                        let s = Instant::now();
                        let _ = store.recall(&q)?;
                        lat_store.push(s.elapsed().as_secs_f64() * 1e6);
                    }
                }
                let mut v = summarize(name, "api", lat);
                v["avg_results"] = json!(returned as f64 / reqs.len() as f64);
                ops.push(v);
                if store_too {
                    ops.push(summarize(name, "store", lat_store));
                }
                Ok(())
            };
        recall("recall_and", true, &self.recall_and)?;
        recall("recall_or_fallback", false, &self.recall_or_fallback)?;
        recall("recall_filtered", false, &self.recall_filtered)?;
        if let Some(reqs) = &self.recall_chunks {
            recall("recall_chunks", false, reqs)?;
        }
        // N3: the high-water mark right after the recall phase
        let vm_hwm_kb = self_status_kb("VmHWM");

        // ---- denylist ----
        let mut lat = Vec::new();
        for q in &self.denylist_short {
            let s = Instant::now();
            let _ = api.sanitize("bench", "q", q, &mut Vec::new());
            lat.push(s.elapsed().as_secs_f64() * 1e6);
        }
        ops.push(summarize("denylist_scan_short", "api", lat));
        let mut lat = Vec::new();
        for _ in 0..Self::DENYLIST_LONG {
            let s = Instant::now();
            let _ = api.sanitize("bench", "q", &self.denylist_long, &mut Vec::new());
            lat.push(s.elapsed().as_secs_f64() * 1e6);
        }
        ops.push(summarize("denylist_scan_long", "api", lat));
        Ok((ops, vm_hwm_kb))
    }
}

/// Recreate `db`, build the §16 corpus of `n` notes with `epn` edges each through the full
/// service path and print the §16 operation set as JSON. The audit key lives under `keys_dir`
/// (the same `$KABAN_HOME/keys` a `kaban serve` on the finished database would use, so the N1
/// measurement can open it). `chunks > 0` (DI1.7, feature `ingest`) adds that many synthetic
/// chunks through the ingestor before the reads and a `recall_chunks` op; at 0 the run is
/// today's bench, byte for byte apart from timings. `schema_version` must be
/// [`LATEST_SCHEMA_VERSION`]: kaban serves one schema (spec 05 OWN-P), so the E1.7 migration
/// sections (`migration`, `pre_migration_ops`) are never produced any more and the result's
/// `ops` are always the one schema's.
///
/// `_policy_path` is the file `policy` was loaded from; the post-migration service that
/// loaded it again is gone with the migration ladder, the argument stays for the CLI wiring.
///
/// # Errors
/// Store or service errors; `chunks > 0` without the `ingest` feature; a `schema_version`
/// other than the one kaban serves.
pub(crate) fn run(
    db: &Path,
    keys_dir: &Path,
    actor: &ActorId,
    policy: Policy,
    _policy_path: Option<&Path>,
    shape: &Shape,
) -> anyhow::Result<()> {
    let Shape {
        notes: n,
        edges_per_node: epn,
        chunks,
        schema_version,
    } = *shape;
    let _ = std::fs::remove_file(db);
    let _ = std::fs::remove_file(format!("{}-wal", db.display()));
    let _ = std::fs::remove_file(format!("{}-shm", db.display()));
    let t_open = Instant::now();
    if schema_version != LATEST_SCHEMA_VERSION {
        anyhow::bail!(
            "kaban serves schema {LATEST_SCHEMA_VERSION} only (OWN-P): a corpus cannot be built at schema {schema_version}"
        );
    }
    let sqlite = SqliteStore::open_as(db, keys_dir, actor)?;
    let signer = sqlite.signer();
    let store: Arc<dyn GraphStore> = Arc::new(sqlite);
    let open_ms = t_open.elapsed().as_secs_f64() * 1e3;
    let mut corpus = Corpus::new(SEED);
    // the fixture repository: a 50-commit repository beside the database; every accepted
    // Decision is stamped with its commit 25 and the fixture's root name (M3-22, M3-23)
    let fixture = db.with_file_name("bench-fixture-repo");
    let fixture_commit = fixture_repo(&fixture, &corpus)?;
    let api = Arc::new(KabanApi::with_config(
        store.clone(),
        policy,
        signer,
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WORKSPACES[0].0),
            repo_root: Some(fixture.clone()),
            repo: Some("bench-fixture-repo".into()),
            ..Default::default()
        },
    ));
    let policy_version = api.policy_version();
    let mut ops = Vec::new();
    let mut counts = serde_json::Map::new();
    let bump = |counts: &mut serde_json::Map<String, Value>, key: &str| {
        let e = counts.entry(key).or_insert(json!(0));
        *e = json!(e.as_u64().unwrap_or(0) + 1);
    };

    // ---- corpus: remember (api), accept (store), CodeRefs (api), lifecycle (api) ----
    let phase = Instant::now();
    let mut lat = Vec::with_capacity(20_000);
    let mut by_ws: Vec<Vec<String>> = vec![Vec::new(); WORKSPACES.len()];
    let mut all_ids: Vec<String> = Vec::with_capacity(n);
    let mut decisions: Vec<(String, bool)> = Vec::new();
    let mut to_forget: Vec<(String, ForgetMode)> = Vec::new();
    let mut rejected_writes = 0usize;
    for i in 0..n {
        let (spec, req) = corpus_request(&mut corpus, i);
        let has_ref = !req.refs.is_empty();
        let s = Instant::now();
        let out = api.remember(req);
        if i % stride(n) == 0 {
            lat.push(s.elapsed().as_secs_f64() * 1e6);
        }
        let out = match out {
            Ok(o) => o,
            Err(kaban_api::ApiError::Rejected { .. }) => {
                rejected_writes += 1;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if out.status != RememberStatus::Created {
            bump(&mut counts, "dedup_existing");
            continue;
        }
        bump(&mut counts, spec.kind.name());
        if has_ref {
            bump(&mut counts, "coderefs");
        }
        let ws = WORKSPACES
            .iter()
            .position(|(w, _)| *w == spec.workspace)
            .unwrap_or(0);
        by_ws[ws].push(out.id.clone());
        all_ids.push(out.id.clone());
        if spec.kind == Kind::Decision {
            decisions.push((out.id.clone(), spec.accepted));
        }
        match spec.life {
            Life::Active => bump(&mut counts, "lifecycle_active"),
            Life::Archived => to_forget.push((out.id, ForgetMode::Archive)),
            Life::Tombstoned => to_forget.push((out.id, ForgetMode::Tombstone)),
        }
    }
    let remember_total_s = phase.elapsed().as_secs_f64();
    ops.push(summarize("remember", "api", lat));

    // governance: acceptance is a store call until C3.2 (`accept` tool)
    let phase = Instant::now();
    let ctx = WriteCtx::new(policy_version.clone(), WorkspaceId::default(), Actor::Agent);
    let mut accept_lat = Vec::new();
    for (id, accepted) in &decisions {
        if *accepted {
            let s = Instant::now();
            store.transition(
                &NodeId::parse(id)?,
                Governance::Accepted,
                NewEvent {
                    reason: "bench-accept".into(),
                    commit_sha: Some(fixture_commit.clone()),
                    ..Default::default()
                },
                &ctx,
            )?;
            accept_lat.push(s.elapsed().as_secs_f64() * 1e6);
            bump(&mut counts, "decisions_accepted");
        }
    }
    ops.push(summarize("accept_transition", "store", accept_lat));
    let governance_total_s = phase.elapsed().as_secs_f64();

    // ---- edges: `epn` per node inside its workspace, through the full path ----
    let phase = Instant::now();
    let mut lat = Vec::new();
    let mut links_created = 0usize;
    let mut links_skipped = 0usize;
    let total_links = n * epn;
    for i in 0..total_links {
        let ws = &by_ws[WORKSPACES
            .iter()
            .position(|(w, _)| *w == corpus.note_workspace_for_link())
            .unwrap_or(0)];
        if ws.len() < 2 {
            continue;
        }
        let src = &ws[corpus.rng.below(ws.len())];
        let dst = &ws[corpus.rng.below(ws.len())];
        let s = Instant::now();
        let r = api.link(LinkRequest {
            from: src.clone(),
            rel: RELS[i % RELS.len()].into(),
            to: dst.clone(),
            confidence: Some(0.9),
            props: None,
        });
        if i % stride(total_links) == 0 {
            lat.push(s.elapsed().as_secs_f64() * 1e6);
        }
        match r {
            Ok(_) => links_created += 1,
            // self links and hidden endpoints are part of the random mix
            Err(kaban_api::ApiError::NotFound(_) | kaban_api::ApiError::Store(_)) => {
                links_skipped += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let link_total_s = phase.elapsed().as_secs_f64();
    ops.push(summarize("link", "api", lat));

    // ---- lifecycle moves (api): after the edges, so hidden nodes keep theirs ----
    let phase = Instant::now();
    let mut forget_lat = Vec::new();
    for (id, mode) in &to_forget {
        let s = Instant::now();
        api.forget(ForgetRequest {
            id: id.clone(),
            mode: Some(*mode),
        })?;
        forget_lat.push(s.elapsed().as_secs_f64() * 1e6);
        bump(
            &mut counts,
            if *mode == ForgetMode::Archive {
                "lifecycle_archived"
            } else {
                "lifecycle_tombstoned"
            },
        );
    }
    ops.push(summarize("forget", "api", forget_lat));
    let lifecycle_total_s = phase.elapsed().as_secs_f64();

    // ---- DI1.7: the synthetic chunk corpus through the ingestor (feature `ingest`) ----
    #[cfg(feature = "ingest")]
    let chunk_report = (chunks > 0)
        .then(|| {
            super::chunks::ingest(
                &api,
                &corpus,
                &db.with_file_name("bench-chunk-corpus"),
                chunks,
            )
        })
        .transpose()?;
    #[cfg(not(feature = "ingest"))]
    let chunk_report: Option<Value> = {
        anyhow::ensure!(
            chunks == 0,
            "`--chunks` needs a kaban built with the `ingest` feature"
        );
        None
    };

    // the database is now checked against the loaded policy, so a server started on it for N1
    // has no rescan to run (C2.7)
    let rescan = api.rescan_if_policy_changed(&CancellationToken::new())?;

    // ---- reads: one plan of inputs, run against the built schema and (E1.7) after 0004 ----
    let hidden: std::collections::HashSet<&str> =
        to_forget.iter().map(|(id, _)| id.as_str()).collect();
    let visible: Vec<String> = all_ids
        .iter()
        .filter(|id| !hidden.contains(id.as_str()))
        .cloned()
        .collect();
    let visible_ws1: Vec<String> = by_ws[0]
        .iter()
        .filter(|id| !hidden.contains(id.as_str()))
        .cloned()
        .collect();
    let plan = ReadPlan::draw(&mut corpus, &visible, &visible_ws1, chunks > 0)?;

    // OWN-P: one schema, no migrations; the migration sections of a run built below the
    // latest schema (E1.7) are gone with the ladder
    let migration: Option<Value> = None;
    let pre_migration_ops: Option<Vec<Value>> = None;
    let (read_ops, vm_hwm_kb) = plan.run(&api, store.as_ref())?;
    ops.extend(read_ops);
    ops.push(not_applicable(
        "why_one_path",
        "spec 11 DEL-01",
        "the why tool is removed with governance",
    ));
    ops.push(not_applicable(
        "recall_ref",
        "C3.5",
        "recall by CodeRef reference lands with recall v3",
    ));
    ops.push(not_applicable(
        "recall_merged",
        "C6.2",
        "two stores land in M6",
    ));

    let stats = store.stats(None)?;
    let mut result = json!({
        "bench_schema": 2, "machine": machine_profile(), "seed": format!("{SEED:#x}"),
        "n_nodes": n, "edges_per_node": epn, "open_ms": open_ms,
        "corpus": {
            "counts": counts, "rejected_writes": rejected_writes,
            "links_created": links_created, "links_skipped": links_skipped,
            "remember_total_s": remember_total_s, "governance_total_s": governance_total_s,
            "link_total_s": link_total_s, "lifecycle_total_s": lifecycle_total_s,
            "inserts_per_s": (n as f64 / remember_total_s).round(),
            "links_per_s": (total_links as f64 / link_total_s).round(),
            "rescan": rescan,
        },
        "db_mb": stats["db_bytes"].as_f64().unwrap_or(0.0) / 1e6,
        "vm_hwm_kb": vm_hwm_kb, "rss_kb": self_status_kb("VmRSS"),
        "ops": ops,
    });
    // only a run with chunks carries the section, so `--chunks 0` output is unchanged
    if let Some(report) = chunk_report {
        result["n_chunks"] = json!(chunks);
        result["chunks"] = report;
    }
    // only a run built below the latest schema carries the migration sections (E1.7)
    result["schema_version_built"] = json!(schema_version);
    if let Some(m) = migration {
        result["migration"] = m;
        result["pre_migration_ops"] = json!(pre_migration_ops);
    }
    crate::cli::output::print_json_pretty(&result)
}
