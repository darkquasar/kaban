//! `kaban bench graph` (spec 11 T11 Step 7; GRA-02, GRA-04, wave 2 items 3.1, 3.6 and 7.3): the
//! memory graph's read and write cost on a store, written as JSONL to `--out`.
//!
//! ```text
//!   setup    => commit canonical node batches, then edge batches, and rebuild the graph.
//!               Append the requested sediment through edge-only commits; optionally seal it.
//!   expand2  => seeded present nodes, depth 2, 50 rows; graph-only and whole read times.
//!   hub-cap1500 => the public ordinal-zero hub, depth 2 and a 1500-row cap.
//!   tie-order => order preparation and whole weighted query, cold, warm, first/second
//!               after single-edge and single-node commits, then after cache expiry.
//!               Includes expand2 and hub-cap1500 reads on the same base and its footprint.
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
    /// Synthetic node count (defaults to one tenth of edge attempts)
    #[arg(long)]
    nodes: Option<u32>,
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
        args.nodes,
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
        Caps, Direction, GraphError, LocalId, RelSelection, expand, weighted_shortest_path,
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
        anyhow::ensure!(args.queries > 0, "--queries must be positive");
        let lines = match (args.corpus, args.case) {
            (Corpus::Real, Case::Expand2) => vec![real(args)?],
            (Corpus::Real, _) => bail!("corpus real measures expand2 only"),
            (Corpus::Synthetic, Case::Expand2) => {
                let mut lines = Vec::new();
                for ops in args.sediment.split(',') {
                    let ops: usize = ops.trim().parse().context("--sediment")?;
                    lines.push(reads("expand2", args, ops, EXPAND, false)?);
                }
                lines
            }
            (Corpus::Synthetic, Case::HubCap1500) => {
                vec![reads("hub-cap1500", args, 0, HUB, true)?]
            }
            (Corpus::Synthetic, Case::LinkWal) => vec![link_wal(args.queries)?],
            (Corpus::Synthetic, Case::Oneshot) => vec![oneshot(args)?],
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

    /// A store using the normal setup checkpoint interval. Only `--sealed` compacts.
    fn open(dir: &Path) -> Result<SqliteStore> {
        Ok(
            SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys"))?.with_catalog_config(
                CatalogCfg {
                    seal_ops: SealOps {
                        graph: 1,
                        ..SealOps::default()
                    },
                    ..CatalogCfg::default()
                },
            )?,
        )
    }

    const HUB_UUID: &str = "00000000-0000-7000-8000-000000000000";

    struct CorpusStore {
        store: SqliteStore,
        starts: Vec<LocalId>,
        edges: i64,
        staged_rows: u64,
        staged_ops: u64,
    }

    fn setup(dir: &Path, args: &Args, ops: usize, sealed: bool) -> Result<CorpusStore> {
        let min = if args.edges > 0 || ops > 0 { 2 } else { 1 };
        let nodes = args
            .nodes
            .unwrap_or((args.edges / 10).clamp(min, u64::from(u32::MAX)) as u32);
        anyhow::ensure!(nodes > 0, "--nodes must be positive");
        let store = open(dir)?;
        synthetic::populate(&store, SEED, nodes, args.edges)?;
        catalog_testing::rebuild_graph(&store)?;
        synthetic::append_edges(&store, SEDIMENT_SEED, nodes, ops as u64, ROW_OPS)?;
        let (edges, staged_rows, staged_ops) = catalog_testing::graph_write_counts(&store)?;
        if sealed && staged_rows > 0 {
            let report = store.compact()?;
            let sealed_rows: usize = report
                .published
                .iter()
                .filter(|c| c.class == "graph_layer")
                .map(|c| c.sediment_rows)
                .sum();
            anyhow::ensure!(
                sealed_rows as u64 == staged_rows,
                "compaction sealed {sealed_rows} of {staged_rows} graph rows: {report:?}"
            );
        }
        let starts = catalog_testing::present_local_ids(&store)?;
        anyhow::ensure!(
            starts.len() == nodes as usize,
            "incomplete canonical node corpus"
        );
        Ok(CorpusStore {
            store,
            starts,
            edges,
            staged_rows,
            staged_ops,
        })
    }

    fn hub(store: &SqliteStore) -> Result<LocalId> {
        let id = NodeId::parse(HUB_UUID)?;
        catalog_testing::with_main_graph(store, &budget(), |g| g.local(&id))?
            .context("canonical hub is missing")
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
    fn reads(case: &str, args: &Args, ops: usize, caps: Caps, use_hub: bool) -> Result<Value> {
        let dir = scratch(case)?;
        let built = Instant::now();
        let corpus = setup(&dir, args, ops, args.sealed)?;
        let setup_ms = ms(built.elapsed());
        let from = if use_hub {
            Some(hub(&corpus.store)?)
        } else {
            None
        };
        let mut line = timed(&corpus.store, &corpus.starts, from, args.queries, caps)?;
        line["case"] = json!(case);
        line["corpus"] = json!("synthetic");
        line["edge_attempts"] = json!(args.edges);
        line["edges"] = json!(corpus.edges);
        line["nodes"] = json!(corpus.starts.len());
        line["sediment_attempts"] = json!(ops);
        line["sediment_ops"] = json!(corpus.staged_ops);
        line["sediment_rows_before_seal"] = json!(corpus.staged_rows);
        drop(corpus);
        let _ = std::fs::remove_dir_all(&dir);
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
        let catalog_testing::DbFootprint {
            bytes: db,
            pages: (pages, page_size, freelist),
            objects,
        } = catalog_testing::db_footprint(store)?;
        let objects =
            objects.map_or_else(|e| json!(format!("dbstat unavailable: {e}")), |o| json!(o));
        let kbx = std::fs::metadata(dir.join("kaban.kbx")).map_or(0, |m| m.len());
        Ok(
            json!({ "db_bytes": db, "page_count": pages, "page_size": page_size, "freelist_count": freelist, "objects": objects, "kbx_bytes": kbx }),
        )
    }

    /// Query twice just after each canonical commit.
    fn after_commits(
        mut commit: impl FnMut(usize) -> Result<()>,
        query: &mut impl FnMut() -> Result<(f64, f64)>,
    ) -> Result<Value> {
        let (mut first, mut first_order, mut second, mut second_order) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for round in 0..ROUNDS {
            commit(round)?;
            let (q, o) = query()?;
            first.push(q);
            first_order.push(o);
            let (q, o) = query()?;
            second.push(q);
            second_order.push(o);
        }
        Ok(
            json!({ "first_ms": quantiles(first), "first_order_ms": quantiles(first_order),
            "second_ms": quantiles(second), "second_order_ms": quantiles(second_order) }),
        )
    }

    fn tie_order(args: &Args) -> Result<Value> {
        let dir = scratch("tie-order")?;
        let built = Instant::now();
        let corpus = setup(&dir, args, 0, false)?;
        let setup_ms = ms(built.elapsed());
        let store = &corpus.store;
        let nodes = u32::try_from(corpus.starts.len())?;
        let footprint = footprint(store, &dir)?;
        let mut seed = SEED;
        let mut pick = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            corpus.starts[((seed >> 33) % u64::from(nodes)) as usize]
        };
        let mut query = || tie_read(store, pick(), pick());
        let (cold_ms, cold_order_ms) = query()?;
        let (mut warm, mut warm_order) = (Vec::new(), Vec::new());
        for _ in 0..args.queries {
            let (q, o) = query()?;
            warm.push(q);
            warm_order.push(o);
        }
        let expand2 = timed(store, &corpus.starts, None, args.queries, EXPAND)?;
        let hub_cap1500 = timed(store, &corpus.starts, Some(hub(store)?), args.queries, HUB)?;
        let edge_commits = after_commits(
            |round| synthetic::append_edges(store, SEDIMENT_SEED + round as u64, nodes, 1, 1),
            &mut query,
        )?;
        let (edges_after, edge_rows, edge_ops) = catalog_testing::graph_write_counts(store)?;
        let ctx = WriteCtx::new(WorkspaceId::default(), Actor::Agent);
        let node_commits = after_commits(
            |round| {
                let op = Op::UpsertNode {
                    id: NodeId::new(),
                    node: NewNode {
                        kind: "Fact".into(),
                        text: format!("added {round}"),
                        ..Default::default()
                    },
                };
                store.apply_batch(WriteBatch::new(WorkspaceId::default(), vec![op]), &ctx)?;
                Ok(())
            },
            &mut query,
        )?;
        let (_, final_rows, final_ops) = catalog_testing::graph_write_counts(store)?;
        catalog_testing::advance_graph_cache_clock(Duration::from_secs(61));
        let (expiry_first_ms, expiry_first_order_ms) = query()?;
        let (expiry_second_ms, expiry_second_order_ms) = query()?;
        let edges = corpus.edges;
        drop(corpus);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(json!({
            "case": "tie-order", "corpus": "synthetic", "edge_attempts": args.edges,
            "edges": edges, "nodes": nodes, "queries": args.queries, "rounds": ROUNDS,
            "setup_ms": setup_ms, "footprint": footprint, "expand2": expand2, "hub_cap1500": hub_cap1500,
            "cold_ms": cold_ms, "cold_order_ms": cold_order_ms,
            "warm_ms": quantiles(warm), "warm_order_ms": quantiles(warm_order),
            "edge_commits": edge_commits, "node_commits": node_commits, "edges_after": edges_after,
            "edge_sediment_rows": edge_rows, "edge_sediment_ops": edge_ops,
            "node_sediment_rows": final_rows - edge_rows, "node_sediment_ops": final_ops - edge_ops,
            "expiry_first_ms": expiry_first_ms, "expiry_first_order_ms": expiry_first_order_ms,
            "expiry_second_ms": expiry_second_ms, "expiry_second_order_ms": expiry_second_order_ms,
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
    fn oneshot(args: &Args) -> Result<Value> {
        let dir = scratch("oneshot")?;
        let built = Instant::now();
        let corpus = setup(&dir, args, 0, false)?;
        let nodes = corpus.starts.len();
        let edges = corpus.edges;
        let attempts = args.edges;
        let setup_ms = ms(built.elapsed());
        drop(corpus);
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
        line["edge_attempts"] = json!(attempts);
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
        let id = NodeId::parse(HUB_UUID)?;
        let loading = Instant::now();
        let (load_ms, expand_ms, rows) = catalog_testing::with_main_graph(&store, &b, |g| {
            let start = g
                .local(&id)?
                .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
            let load_ms = ms(loading.elapsed());
            let t = Instant::now();
            let r = expand(
                g.view(),
                &[start],
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
