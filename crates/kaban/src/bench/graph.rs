//! `kaban bench graph` (spec 11 T11 Step 7; GRA-02, GRA-04, wave 2 items 3.1, 3.6 and 7.3): the
//! memory graph's read and write cost on a store, written as JSONL to `--out`.
//!
//! ```text
//!   setup    => fresh store; one node row per corpus node (rid = local id, so ids resolve);
//!               main's graph base = build_base(synthetic corpus of --edges)
//!               + --sediment pending graph ops as sediment rows of 100 ops each
//!               (--sealed: those ops sealed into a layer by one compaction first)
//!   expand2  => per query: snapshot + cached load_view + expand(depth 2, 50 rows) + every
//!               row's public id (no tie order: expand consumes none); query_ms (the whole
//!               read) and expand_ms (the expand alone), p50/p99/max
//!   hub-cap1500 => the same from node 0 (the generator's largest hub), 1500 rows
//!   tie-order => a tie-consuming query (tie order + weighted shortest path, depth 2, 50
//!               rows): the process's first (cold), --queries more at that generation (warm),
//!               then rounds of one commit (one pending edge row) and two queries, the first
//!               at the new generation (it prepares the order) and the second; whole query and
//!               order_ms (asking for the order alone); first the store's footprint on
//!               disk (kaban.db after a checkpoint, by table and index, and kaban.kbx)
//!   link-wal => one single-link commit after another: kaban.db-wal growth per commit
//!   oneshot  => C6: a fresh child process opens the store, loads (validates) main's graph
//!               and runs one small expand; each phase timed
//! ```
//!
//! The `real` corpus (GRA-02's 4.9M nodes / 21.4M edges) is an existing store at `--store`,
//! read in place (expand2 only, its own pending rows as they are); it is refused only when the
//! host's physical memory is below its container's size (the graph a load maps). Measurements
//! are run once and reported (wave 2 item 3.7); nothing here is a test.

use std::path::PathBuf;
use std::process::ExitCode;

/// What the bench measures.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Case {
    /// Depth-2 expand from seeded nodes, for each `--sediment` value
    Expand2,
    /// WAL bytes per single-link commit
    LinkWal,
    /// Depth-2 expand from the highest-degree node with a 1500-row cap
    #[value(name = "hub-cap1500")]
    HubCap1500,
    /// A fresh process: open, validate and one small expand (C6)
    Oneshot,
    /// The first tie-consuming query at a generation versus the second
    #[value(name = "tie-order")]
    TieOrder,
}

/// The graph the bench builds.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Corpus {
    /// The seeded generator's Zipf graph (`--edges` edges)
    Synthetic,
    /// GRA-02's corpus: an existing store at `--store`
    Real,
}

/// `bench graph` arguments.
#[derive(clap::Args)]
pub(crate) struct Args {
    /// What to measure
    #[arg(long, value_enum)]
    case: Case,
    /// The graph to measure on
    #[arg(long, value_enum, default_value = "synthetic")]
    corpus: Corpus,
    /// Edges of the synthetic corpus
    #[arg(long, default_value_t = 1_000_000)]
    edges: u64,
    /// Pending graph ops kept in main's sediment, comma separated (one run each)
    #[arg(long, default_value = "0")]
    sediment: String,
    /// Seal the pending graph ops into a layer before measuring
    #[arg(long)]
    sealed: bool,
    /// The store directory of the `real` corpus (its `kaban.db` and `kaban.kbx`)
    #[arg(long)]
    store: Option<PathBuf>,
    /// Timed queries (or link commits) per run
    #[arg(long, default_value_t = 1_000)]
    queries: u32,
    /// The JSONL output file
    #[arg(long, default_value = "graph.jsonl")]
    out: PathBuf,
    /// The one-shot child: the store directory it opens
    #[arg(long, hide = true)]
    oneshot_child: Option<PathBuf>,
}

/// Run the bench.
#[cfg(feature = "bench")]
pub(crate) fn run(args: &Args) -> ExitCode {
    imp::run(args).unwrap_or_else(|e| {
        crate::cli::output::print_failure(&format!("{e:#}"));
        ExitCode::FAILURE
    })
}

/// Without the `bench` feature: refused with exit 4.
#[cfg(not(feature = "bench"))]
pub(crate) fn run(args: &Args) -> ExitCode {
    let _ = (
        args.case,
        args.corpus,
        args.edges,
        &args.sediment,
        args.sealed,
        args.queries,
    );
    let _ = (&args.out, &args.oneshot_child, &args.store);
    crate::cli::stubs::not_available("bench graph", "feature bench")
}

#[cfg(feature = "bench")]
mod imp {
    use std::io::Write;
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use kaban_core::config::{CatalogCfg, SealOps};
    use kaban_core::query_budget::QueryBudget;
    use kaban_core::{
        Actor, BackendSource, IngestStore, MemoryStore, NewNode, NodeId, Op, StoreError,
        WorkspaceId, WriteBatch, WriteCtx,
    };
    use kaban_index_graph::{
        Caps, Direction, GraphError, LocalId, Records, RelSelection, build_base, expand,
        weighted_shortest_path,
    };
    use kaban_store_sqlite::{SqliteStore, catalog_testing};
    use serde_json::{Value, json};

    use super::{Args, Case, Corpus};
    use crate::bench::synthetic;

    /// The corpus seed and the pending sediment's.
    const SEED: u64 = 0x7_6;
    const SEDIMENT_SEED: u64 = 0x7_7;
    /// Graph ops per pending sediment row (one small commit each).
    const ROW_OPS: usize = 100;
    /// The per-call default row cap and the ceiling (wave 2 item 3.1).
    const EXPAND: Caps = Caps { depth: 2, rows: 50 };
    const HUB: Caps = Caps {
        depth: 2,
        rows: 1_500,
    };
    /// The bound of the one-shot child.
    const CHILD_DEADLINE: Duration = Duration::from_mins(15);
    /// Commits of the tie-order case, each followed by two queries.
    const ROUNDS: usize = 20;

    pub(super) fn run(args: &Args) -> Result<ExitCode> {
        if let Some(dir) = &args.oneshot_child {
            return oneshot_child(dir);
        }
        let lines = match (args.corpus, args.case) {
            (Corpus::Real, Case::Expand2) => vec![real(args)?],
            (Corpus::Real, _) => bail!("corpus real measures expand2 only"),
            (Corpus::Synthetic, Case::Expand2) => {
                let mut lines = Vec::new();
                for ops in args.sediment.split(',') {
                    let ops: usize = ops.trim().parse().context("--sediment")?;
                    lines.push(reads("expand2", args, ops, EXPAND, None)?);
                }
                lines
            }
            (Corpus::Synthetic, Case::HubCap1500) => {
                vec![reads("hub-cap1500", args, 0, HUB, Some(LocalId(0)))?]
            }
            (Corpus::Synthetic, Case::LinkWal) => vec![link_wal(args.queries)?],
            (Corpus::Synthetic, Case::Oneshot) => vec![oneshot(args.edges)?],
            (Corpus::Synthetic, Case::TieOrder) => vec![tie_order(args)?],
        };
        let mut out = std::fs::File::create(&args.out)
            .with_context(|| format!("creating {}", args.out.display()))?;
        for line in &lines {
            writeln!(out, "{line}")?;
            crate::cli::output::print_json(line, crate::cli::output::Format::Compact)?;
        }
        Ok(ExitCode::SUCCESS)
    }

    /// A fresh scratch directory for one run.
    fn scratch(name: &str) -> Result<std::path::PathBuf> {
        let dir =
            std::env::temp_dir().join(format!("kaban-graph-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// A store in `dir` whose catalog never checkpoints during a run and seals any pending
    /// graph op when it compacts (only `--sealed` compacts).
    fn open(dir: &Path) -> Result<SqliteStore> {
        Ok(
            SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys"))?.with_catalog_config(
                CatalogCfg {
                    seal_ops: SealOps {
                        graph: 1,
                        ..SealOps::default()
                    },
                    checkpoint_interval_ms: 3_600_000,
                    ..CatalogCfg::default()
                },
            )?,
        )
    }

    /// One committed node row per corpus node, `rid` = its local id and a `NodeId` that sorts
    /// as the rid, so public ids resolve and the stable order is the corpus's; the graph ops
    /// that commit stages are consumed at once (the corpus base already holds the nodes).
    fn node_rows(store: &SqliteStore, nodes: u32) -> Result<()> {
        catalog_testing::seed_nodes(store, nodes)?;
        catalog_testing::consume_graph_sediment(store)?;
        Ok(())
    }

    /// A store in `dir` whose main graph is the synthetic corpus of `edges` edges as its base,
    /// with a node row per corpus node, plus `ops` pending graph ops (sealed into one layer
    /// when `sealed`); the corpus's node count.
    fn setup(dir: &Path, edges: u64, ops: usize, sealed: bool) -> Result<(SqliteStore, u32)> {
        let store = open(dir)?;
        let corpus = synthetic::generate(SEED, edges);
        let nodes = u32::try_from(corpus.nodes.len())?;
        node_rows(&store, nodes)?;
        let base = build_base(&corpus, nodes);
        drop(corpus);
        catalog_testing::publish_graph_base(&store, &base)?;
        drop(base);
        let pending = synthetic::generate(SEDIMENT_SEED, ops as u64).edges;
        for chunk in pending.chunks(ROW_OPS) {
            let row = Records {
                edges: chunk.to_vec(),
                ..Records::default()
            };
            catalog_testing::stage_graph_sediment(&store, &row.encode())?;
        }
        if sealed && ops > 0 {
            let report = store.compact()?;
            let rows: usize = report
                .published
                .iter()
                .filter(|c| c.class == "graph_layer")
                .map(|c| c.sediment_rows)
                .sum();
            if rows != ops.div_ceil(ROW_OPS) {
                bail!("--sealed: the compaction sealed {rows} graph rows: {report:?}");
            }
        }
        Ok((store, nodes))
    }

    fn budget() -> QueryBudget {
        QueryBudget::new(u64::MAX, Some(Duration::from_secs(60)))
    }

    fn graph_error(e: GraphError) -> StoreError {
        StoreError::Backend {
            source: BackendSource::new(e),
        }
    }

    /// One read through the store: a snapshot, main's graph loaded (cached after the first
    /// load), one expand from `start` and every row's public id (no tie order). Returns
    /// (whole read ms, expand ms, rows, truncated).
    fn read(store: &SqliteStore, start: LocalId, caps: Caps) -> Result<(f64, f64, usize, bool)> {
        let b = budget();
        let began = Instant::now();
        let (expand_ms, rows, truncated) = catalog_testing::with_main_graph(store, &b, |g| {
            let t = Instant::now();
            let r = expand(
                g.view(),
                &[start],
                &RelSelection::All,
                Direction::Out,
                caps,
                &b,
            )
            .map_err(graph_error)?;
            let expand_ms = ms(t.elapsed());
            for &(n, _) in &r.nodes {
                g.node_id(n)?;
            }
            Ok((expand_ms, r.nodes.len(), r.truncated))
        })?;
        Ok((ms(began.elapsed()), expand_ms, rows, truncated))
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    /// p50, p99 and max of `xs`.
    fn quantiles(mut xs: Vec<f64>) -> Value {
        xs.sort_by(f64::total_cmp);
        let at = |q: f64| xs[((xs.len() - 1) as f64 * q).round() as usize];
        json!({ "p50": at(0.5), "p99": at(0.99), "max": at(1.0) })
    }

    /// `args.queries` reads (after one untimed first load) from seeded start nodes, or all
    /// from `from`, over a store with `ops` pending graph ops.
    fn reads(
        case: &str,
        args: &Args,
        ops: usize,
        caps: Caps,
        from: Option<LocalId>,
    ) -> Result<Value> {
        let dir = scratch(case)?;
        let built = Instant::now();
        let (store, nodes) = setup(&dir, args.edges, ops, args.sealed)?;
        let setup_ms = ms(built.elapsed());
        let starts: Vec<LocalId> = (0..nodes).map(LocalId).collect();
        let mut line = timed(&store, &starts, from, args.queries, caps)?;
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
        line["case"] = json!(case);
        line["corpus"] = json!("synthetic");
        line["edges"] = json!(args.edges);
        line["nodes"] = json!(nodes);
        line["sediment_ops"] = json!(ops);
        line["sealed"] = json!(args.sealed && ops > 0);
        line["setup_ms"] = json!(setup_ms);
        Ok(line)
    }

    /// `queries` reads (after one untimed first load) from seeded picks of `starts`, or all
    /// from `from`.
    fn timed(
        store: &SqliteStore,
        starts: &[LocalId],
        from: Option<LocalId>,
        queries: u32,
        caps: Caps,
    ) -> Result<Value> {
        let Some(&first) = starts.first() else {
            bail!("the graph has no node");
        };
        let mut seed = SEED;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let at = usize::try_from((seed >> 33) % starts.len() as u64).unwrap_or(0);
            starts.get(at).copied().unwrap_or(first)
        };
        let (first_ms, ..) = read(store, from.unwrap_or(first), caps)?;
        let (mut query, mut expand_ms, mut rows) = (Vec::new(), Vec::new(), Vec::new());
        let mut truncated = 0_u32;
        for _ in 0..queries {
            let (q, e, n, t) = read(store, from.unwrap_or_else(&mut next), caps)?;
            query.push(q);
            expand_ms.push(e);
            rows.push(n as f64);
            truncated += u32::from(t);
        }
        Ok(json!({
            "caps": { "depth": caps.depth, "rows": caps.rows },
            "queries": queries,
            "first_read_ms": first_ms,
            "query_ms": quantiles(query),
            "expand_ms": quantiles(expand_ms),
            "rows": quantiles(rows),
            "truncated": truncated,
        }))
    }

    /// One tie-consuming query: a snapshot, main's graph loaded, its tie order, one weighted
    /// shortest path from `src` to `dst` (or its refusal at the caps). Returns (whole query ms,
    /// order ms).
    fn tie_read(store: &SqliteStore, src: LocalId, dst: LocalId) -> Result<(f64, f64)> {
        let b = budget();
        let began = Instant::now();
        let order_ms = catalog_testing::with_main_graph(store, &b, |g| {
            let t = Instant::now();
            let order = g.order()?;
            let order_ms = ms(t.elapsed());
            // a search refused at its caps has still consumed the order
            let path =
                weighted_shortest_path(g.view(), src, dst, &RelSelection::All, EXPAND, &*order, &b);
            match path {
                Ok(_) | Err(GraphError::CapExceeded { .. }) => Ok(order_ms),
                Err(e) => Err(graph_error(e)),
            }
        })?;
        Ok((ms(began.elapsed()), order_ms))
    }

    /// The store on disk: `kaban.db`'s bytes after a checkpoint, each table's and index's
    /// bytes (`dbstat`, or why it is not there) and the container's bytes.
    fn footprint(store: &SqliteStore, dir: &Path) -> Result<Value> {
        let catalog_testing::DbFootprint { bytes: db, objects } =
            catalog_testing::db_footprint(store)?;
        let objects =
            objects.map_or_else(|e| json!(format!("dbstat unavailable: {e}")), |o| json!(o));
        let kbx = std::fs::metadata(dir.join("kaban.kbx")).map_or(0, |m| m.len());
        Ok(json!({ "db_bytes": db, "objects": objects, "kbx_bytes": kbx }))
    }

    /// The tie-order case (see the module documentation) on the synthetic corpus.
    fn tie_order(args: &Args) -> Result<Value> {
        let dir = scratch("tie-order")?;
        let built = Instant::now();
        let (store, nodes) = setup(&dir, args.edges, 0, false)?;
        let setup_ms = ms(built.elapsed());
        let footprint = footprint(&store, &dir)?;
        let mut seed = SEED;
        let mut pick = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            LocalId(u32::try_from((seed >> 33) % u64::from(nodes)).unwrap_or(0))
        };
        let (cold_ms, cold_order_ms) = tie_read(&store, pick(), pick())?;
        let (mut warm, mut warm_order) = (Vec::new(), Vec::new());
        for _ in 0..args.queries {
            let (q, o) = tie_read(&store, pick(), pick())?;
            warm.push(q);
            warm_order.push(o);
        }
        let (mut first, mut first_order, mut second, mut second_order) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for edge in synthetic::generate(SEDIMENT_SEED, ROUNDS as u64).edges {
            let row = Records {
                edges: vec![edge],
                ..Records::default()
            };
            catalog_testing::stage_graph_sediment(&store, &row.encode())?;
            let (q, o) = tie_read(&store, pick(), pick())?;
            first.push(q);
            first_order.push(o);
            let (q, o) = tie_read(&store, pick(), pick())?;
            second.push(q);
            second_order.push(o);
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(json!({
            "case": "tie-order",
            "corpus": "synthetic",
            "edges": args.edges,
            "nodes": nodes,
            "queries": args.queries,
            "rounds": ROUNDS,
            "setup_ms": setup_ms,
            "footprint": footprint,
            "cold_ms": cold_ms,
            "cold_order_ms": cold_order_ms,
            "warm_ms": quantiles(warm),
            "warm_order_ms": quantiles(warm_order),
            "after_commit_first_ms": quantiles(first),
            "after_commit_first_order_ms": quantiles(first_order),
            "after_commit_second_ms": quantiles(second),
            "after_commit_second_order_ms": quantiles(second_order),
        }))
    }

    /// The host's physical memory in bytes (`MemTotal` of `/proc/meminfo`, as the catalog
    /// configuration reads it), `None` where it is not known.
    fn physical_memory() -> Option<u64> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kib: u64 = meminfo
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()?;
        Some(kib * 1024)
    }

    /// expand2 on the `real` corpus: the store at `--store`, read in place, from seeded picks
    /// of its present nodes. Refused when the host's physical memory is below the size of its
    /// container (what a load maps).
    fn real(args: &Args) -> Result<Value> {
        let Some(dir) = &args.store else {
            bail!("corpus real needs --store <the corpus store directory>");
        };
        let kbx = std::fs::metadata(dir.join("kaban.kbx"))
            .with_context(|| format!("the corpus container in {}", dir.display()))?
            .len();
        if let Some(memory) = physical_memory().filter(|&m| m < kbx) {
            bail!("corpus real needs {kbx} bytes of memory for its graph; this host has {memory}");
        }
        let store = open(dir)?;
        let starts = catalog_testing::present_local_ids(&store)?;
        let mut line = timed(&store, &starts, None, args.queries, EXPAND)?;
        line["case"] = json!("expand2");
        line["corpus"] = json!("real");
        line["nodes"] = json!(starts.len());
        line["kbx_bytes"] = json!(kbx);
        Ok(line)
    }

    /// `links` single-link commits between fresh nodes: the WAL growth of each.
    fn link_wal(links: u32) -> Result<Value> {
        let dir = scratch("link-wal")?;
        let store = open(&dir)?;
        let ctx = WriteCtx::new(WorkspaceId::default(), Actor::Agent);
        let ids: Vec<NodeId> = (0..links * 2).map(|_| NodeId::new()).collect();
        let ops = ids
            .iter()
            .enumerate()
            .map(|(i, id)| Op::UpsertNode {
                id: id.clone(),
                node: NewNode {
                    kind: "Fact".into(),
                    text: format!("node {i}"),
                    ..Default::default()
                },
            })
            .collect();
        store.apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx)?;
        let wal = dir.join("kaban.db-wal");
        let size = || std::fs::metadata(&wal).map_or(0, |m| m.len());
        let mut grown = Vec::new();
        for pair in ids.chunks(2) {
            let before = size();
            store.link(&pair[0], &pair[1], "RELATES_TO", None, None, &ctx)?;
            grown.push((size() - before) as f64);
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
        let total: f64 = grown.iter().sum();
        Ok(json!({
            "case": "link-wal",
            "links": links,
            "wal_bytes_per_link_mean": total / f64::from(links),
            "wal_bytes_per_link": quantiles(grown),
        }))
    }

    /// C6 (wave 2 item 7.3): build the store here, then time a fresh child process's open,
    /// validating load and one small expand.
    fn oneshot(edges: u64) -> Result<Value> {
        let dir = scratch("oneshot")?;
        let built = Instant::now();
        let (store, nodes) = setup(&dir, edges, 0, false)?;
        let setup_ms = ms(built.elapsed());
        drop(store);
        let kbx = std::fs::metadata(dir.join("kaban.kbx")).map_or(0, |m| m.len());
        let dir_arg = dir.as_os_str().to_owned();
        let args = [
            "bench".as_ref(),
            "graph".as_ref(),
            "--case".as_ref(),
            "oneshot".as_ref(),
            "--oneshot-child".as_ref(),
            dir_arg.as_os_str(),
        ];
        let child = crate::bench::run_self(&args, &[], CHILD_DEADLINE)?;
        let _ = std::fs::remove_dir_all(&dir);
        std::io::stderr().write_all(&child.stderr)?;
        if child.killed.is_some() || !child.status.is_some_and(|s| s.success()) {
            bail!("the one-shot child failed");
        }
        let mut line: Value = serde_json::from_slice(&child.stdout).context("the child's line")?;
        line["edges"] = json!(edges);
        line["nodes"] = json!(nodes);
        line["kbx_bytes"] = json!(kbx);
        line["setup_ms"] = json!(setup_ms);
        Ok(line)
    }

    /// The one-shot child: open, load (validate) and expand once, each timed; one JSON line.
    fn oneshot_child(dir: &Path) -> Result<ExitCode> {
        let began = Instant::now();
        let store = open(dir)?;
        let open_ms = ms(began.elapsed());
        let b = budget();
        let loading = Instant::now();
        let (load_ms, expand_ms, rows) = catalog_testing::with_main_graph(&store, &b, |g| {
            let load_ms = ms(loading.elapsed());
            let t = Instant::now();
            let r = expand(
                g.view(),
                &[LocalId(1)],
                &RelSelection::All,
                Direction::Out,
                EXPAND,
                &b,
            )
            .map_err(graph_error)?;
            Ok((load_ms, ms(t.elapsed()), r.nodes.len()))
        })?;
        let line = json!({
            "case": "oneshot",
            "corpus": "synthetic",
            "open_ms": open_ms,
            "load_validate_ms": load_ms,
            "expand_ms": expand_ms,
            "rows": rows,
            "total_ms": ms(began.elapsed()),
        });
        crate::cli::output::print_json(&line, crate::cli::output::Format::Compact)?;
        Ok(ExitCode::SUCCESS)
    }
}
