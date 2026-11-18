//! `kaban bench catalog` (spec 11 T6 Step 6, KER-04, KER-06, OD-5): writers and a merge thread
//! for `--secs`, measuring the publish lock hold, the merge/seal switch hold and the WAL
//! high-water mark (informational), written as JSONL.
//!
//! ```text
//!   writer x W  => append_and_publish(256 B test extent)   timed per call: publish_ms
//!   merge       => SqliteStore::compact (seal, fold, reclaim, checkpoint)
//!                                       timed per call that published: switch_ms
//!   main        => size of kaban.db-wal every 5 ms          wal_high_water_bytes
//! ```
//!
//! Each writer and the merge thread own a store of the same database, as separate processes
//! would. Both timings are of the whole call, so they bound the lock holds from above: a
//! publication includes its container append and sync, a compaction its merge work and
//! reclamation. One run per writer count of `--writers`, one JSONL line per run.
//!
//! The hidden `--memory seal|fold|build-base --edges N` arm (spec 11 7.6) instead runs one
//! graph writer over a synthetic layer of N edges in a child process and prints the child's
//! peak resident memory (`VmHWM`, Linux). Needs a build with the `bench` feature.

use std::path::PathBuf;
use std::process::ExitCode;

/// A graph writer whose peak memory `--memory` measures.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Op {
    /// `seal`: the edges as one layer over an empty chain
    Seal,
    /// `fold`: a base of three quarters of the edges and a layer of the rest, folded
    Fold,
    /// `build_base`: the edges as a base
    BuildBase,
}

#[cfg(feature = "bench")]
impl Op {
    /// The operation's `--memory` value.
    fn name(self) -> &'static str {
        match self {
            Self::Seal => "seal",
            Self::Fold => "fold",
            Self::BuildBase => "build-base",
        }
    }
}

/// `bench catalog` arguments.
#[derive(clap::Args)]
pub(crate) struct Args {
    /// How long to run, in seconds
    #[arg(long, default_value_t = 60)]
    secs: u64,
    /// Concurrent writer counts to run, as a range `MIN..MAX` (each count is one run)
    #[arg(long, default_value = "1..4")]
    writers: String,
    /// Run the merge thread beside the writers (`on` or `off`)
    #[arg(long, default_value = "on")]
    merge_thread: String,
    /// The JSONL output file
    #[arg(long, default_value = "catalog.jsonl")]
    out: PathBuf,
    /// Measure the peak memory of one graph writer instead (spec 11 7.6)
    #[arg(long, hide = true, value_enum)]
    memory: Option<Op>,
    /// Edges of the synthetic layer `--memory` runs over
    #[arg(long, hide = true, default_value_t = 1_000_000)]
    edges: u64,
    /// Run the `--memory` writer in this process (the child)
    #[arg(long, hide = true)]
    memory_child: bool,
}

/// Run the bench, or the memory measurement.
#[cfg(feature = "bench")]
pub(crate) fn run(args: &Args) -> ExitCode {
    let result = match args.memory {
        Some(op) if args.memory_child => memory::child(op, args.edges),
        Some(op) => memory::run(op, args.edges),
        None => writers::run(args),
    };
    result.unwrap_or_else(|e| {
        crate::cli::output::print_failure(&e);
        ExitCode::FAILURE
    })
}

/// Without the `bench` feature: refused with exit 4.
#[cfg(not(feature = "bench"))]
pub(crate) fn run(args: &Args) -> ExitCode {
    let _ = (args.secs, &args.writers, &args.merge_thread, &args.out);
    let _ = (args.memory, args.edges, args.memory_child);
    crate::cli::stubs::not_available("bench catalog", "feature bench")
}

#[cfg(feature = "bench")]
mod writers {
    use std::io::Write;
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use kaban_store_sqlite::{SqliteStore, WorkKind, catalog_testing};
    use serde_json::{Value, json};

    use super::Args;

    /// Bytes of one writer publication.
    const EXTENT: [u8; 256] = [0x5A; 256];

    /// Timings of one thread, in ms, with its error count and first error.
    #[derive(Default)]
    struct Timed {
        ms: Vec<f64>,
        seals: u64,
        folds: u64,
        errors: u64,
        first_error: Option<String>,
    }

    impl Timed {
        fn failed(&mut self, e: &dyn std::fmt::Display) {
            self.errors += 1;
            self.first_error.get_or_insert_with(|| e.to_string());
        }
    }

    /// p50, p99 and max of `ms` (nearest rank).
    fn summary(mut ms: Vec<f64>) -> Value {
        ms.sort_by(f64::total_cmp);
        let rank = |p: f64| {
            ms.get(((p * ms.len() as f64).ceil() as usize).saturating_sub(1))
                .copied()
        };
        json!({ "n": ms.len(), "p50": rank(0.50), "p99": rank(0.99), "max": ms.last() })
    }

    fn open(db: &Path) -> Result<SqliteStore> {
        let store = SqliteStore::open(db, &db.with_file_name("keys"))?;
        let _observed = catalog_testing::install_edge_sediment_class(&store);
        Ok(store)
    }

    fn writer(store: &SqliteStore, until: Instant) -> Timed {
        let mut t = Timed::default();
        while Instant::now() < until {
            let started = Instant::now();
            match catalog_testing::append_and_publish(store, &EXTENT) {
                Ok(_) => t.ms.push(started.elapsed().as_secs_f64() * 1e3),
                Err(e) => t.failed(&e),
            }
        }
        t
    }

    fn merger(store: &SqliteStore, until: Instant) -> Timed {
        let mut t = Timed::default();
        while Instant::now() < until {
            let started = Instant::now();
            match store.compact() {
                Ok(r) if !r.published.is_empty() => {
                    t.ms.push(started.elapsed().as_secs_f64() * 1e3);
                    for unit in &r.published {
                        match unit.kind {
                            WorkKind::Seal => t.seals += 1,
                            WorkKind::Fold => t.folds += 1,
                            WorkKind::Purge => {}
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => t.failed(&e),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        t
    }

    /// One run of `writers` writers (and the merge thread when `merge`) over a fresh store.
    fn one(writers: usize, merge: bool, secs: u64) -> Result<Value> {
        let dir =
            std::env::temp_dir().join(format!("kaban-catalog-{}-{writers}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let db = dir.join("kaban.db");
        let wal = dir.join("kaban.db-wal");
        let stores = (0..=writers)
            .map(|_| open(&db))
            .collect::<Result<Vec<_>>>()?;
        let until = Instant::now() + Duration::from_secs(secs);
        let (published, merged, wal_high_water) = std::thread::scope(|s| {
            let (merge_store, writer_stores) = stores.split_first().context("no store")?;
            let handles: Vec<_> = writer_stores
                .iter()
                .map(|store| s.spawn(move || writer(store, until)))
                .collect();
            let merging = merge.then(|| s.spawn(move || merger(merge_store, until)));
            let mut high = 0;
            while Instant::now() < until {
                high = high.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut published = Timed::default();
            for handle in handles {
                let Ok(t) = handle.join() else {
                    bail!("a writer panicked")
                };
                published.ms.extend(t.ms);
                published.errors += t.errors;
                published.first_error = published.first_error.or(t.first_error);
            }
            let merged = match merging.map(std::thread::ScopedJoinHandle::join) {
                Some(Ok(t)) => t,
                Some(Err(_)) => bail!("the merge thread panicked"),
                None => Timed::default(),
            };
            high = high.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
            Ok((published, merged, high))
        })?;
        drop(stores);
        std::fs::remove_dir_all(&dir)?;
        Ok(json!({
            "writers": writers,
            "merge_thread": merge,
            "secs": secs,
            "publish_ms": summary(published.ms),
            "publish_errors": published.errors,
            "publish_first_error": published.first_error,
            "switch_ms": summary(merged.ms),
            "seals": merged.seals,
            "folds": merged.folds,
            "compact_errors": merged.errors,
            "compact_first_error": merged.first_error,
            "wal_high_water_bytes": wal_high_water,
        }))
    }

    /// `MIN..MAX` (or one count) as an inclusive range of at least one writer.
    fn counts(range: &str) -> Result<std::ops::RangeInclusive<usize>> {
        let (min, max) = range.split_once("..").unwrap_or((range, range));
        let (min, max): (usize, usize) = (min.trim().parse()?, max.trim().parse()?);
        if min == 0 || min > max {
            bail!("--writers {range}: expected MIN..MAX with 1 <= MIN <= MAX");
        }
        Ok(min..=max)
    }

    pub(super) fn run(args: &Args) -> Result<ExitCode> {
        let merge = match args.merge_thread.as_str() {
            "on" => true,
            "off" => false,
            other => bail!("--merge-thread {other}: expected on or off"),
        };
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&args.out)
            .with_context(|| format!("opening {}", args.out.display()))?;
        for writers in counts(&args.writers)? {
            let line = one(writers, merge, args.secs)?;
            writeln!(out, "{line}")?;
            crate::cli::output::print_json(&line, crate::cli::output::Format::Compact)?;
        }
        Ok(ExitCode::SUCCESS)
    }
}

#[cfg(feature = "bench")]
mod memory {
    use std::io::Write;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use kaban_core::query_budget::QueryBudget;
    use kaban_index_graph::{Base, Layer, PreparedSediment, Records, View, build_base, fold, seal};
    use kaban_store_sqlite::{SqliteStore, catalog_testing};
    use serde_json::json;

    use super::Op;
    use crate::bench::synthetic;

    /// The generator seed of every measurement.
    const SEED: u64 = 0x7_6;
    /// The bound of one measured child (a 5M-edge fold ran 10 s in a debug build).
    const CHILD_DEADLINE: Duration = Duration::from_mins(15);

    /// `key` of `/proc/self/status`, in KiB.
    fn status_kib(key: &str) -> Result<u64> {
        let status = std::fs::read_to_string("/proc/self/status")?;
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse().ok())
            .with_context(|| format!("{key} in /proc/self/status"))
    }

    /// Reset the peak resident memory to the current one (`clear_refs` 5): the setup's peak
    /// is not the operation's.
    fn reset_peak() -> Result<u64> {
        std::fs::write("/proc/self/clear_refs", "5").context("resetting VmHWM")?;
        status_kib("VmRSS:")
    }

    /// What one measured operation left: the resident memory just before it (KiB), its
    /// output length and how long it ran.
    type Ran = (u64, usize, f64);

    /// `op`'s run from just after a peak reset.
    fn timed(op: impl FnOnce() -> Vec<u8>) -> Result<Ran> {
        let before = reset_peak()?;
        let started = Instant::now();
        let output = op().len();
        Ok((before, output, started.elapsed().as_secs_f64() * 1e3))
    }

    /// A base of `base` and one layer of `layer` over it, appended to a fresh catalog in
    /// `dir`, folded as the catalog maps them.
    fn fold_mapped(dir: &std::path::Path, base: Records, layer: Records, n: u32) -> Result<Ran> {
        let budget = QueryBudget::new(u64::MAX, None);
        let empty = PreparedSediment::empty();
        let store = SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys"))?;
        let _observed = catalog_testing::install_edge_sediment_class(&store);
        let base_id = catalog_testing::append_and_publish(&store, &build_base(&base, n))?;
        drop(base);
        let layer_bytes = {
            let pin = catalog_testing::pin(&store, &budget)?;
            let bytes = pin.view().bytes(base_id)?.context("the base extent")?;
            let base = Base::open(bytes, &budget)?;
            seal(&View::new(Some(&base), &[], &empty, &budget)?, &layer)
        };
        drop(layer);
        let layer_id = catalog_testing::append_and_publish(&store, &layer_bytes)?;
        drop(layer_bytes);
        let pin = catalog_testing::pin(&store, &budget)?;
        let view = pin.view();
        let base = Base::open(view.bytes(base_id)?.context("the base extent")?, &budget)?;
        let layers = [Layer::open(
            view.bytes(layer_id)?.context("the layer extent")?,
            &budget,
        )?];
        let chain = View::new(Some(&base), &layers, &empty, &budget)?;
        timed(|| fold(&chain, &[]))
    }

    /// The child: generate the layer, run `op` once, print one JSON line with the peak.
    pub(super) fn child(op: Op, edges: u64) -> Result<ExitCode> {
        let mut records = synthetic::generate(SEED, edges);
        let n = u32::try_from(records.nodes.len())?;
        let budget = QueryBudget::new(u64::MAX, None);
        let empty = PreparedSediment::empty();
        let (rss_before, output, elapsed_ms) = match op {
            Op::BuildBase => timed(|| build_base(&records, n))?,
            Op::Seal => {
                let parent = View::new(None, &[], &empty, &budget)?;
                timed(|| seal(&parent, &records))?
            }
            Op::Fold => {
                let dir = std::env::temp_dir().join(format!("kaban-memory-{}", std::process::id()));
                std::fs::create_dir_all(&dir)?;
                let layer = Records {
                    edges: records.edges.split_off(records.edges.len() * 3 / 4),
                    ..Records::default()
                };
                let folded = fold_mapped(&dir, records, layer, n);
                let _ = std::fs::remove_dir_all(&dir);
                folded?
            }
        };
        let line = json!({
            "op": op.name(),
            "edges": edges,
            "nodes": n,
            "rss_before_op_kib": rss_before,
            "vmhwm_kib": status_kib("VmHWM:")?,
            "output_bytes": output,
            "elapsed_ms": elapsed_ms,
        });
        crate::cli::output::print_json(&line, crate::cli::output::Format::Compact)?;
        Ok(ExitCode::SUCCESS)
    }

    /// Run `op` over `edges` edges in a child process; its line goes to stdout.
    pub(super) fn run(op: Op, edges: u64) -> Result<ExitCode> {
        let edges = edges.to_string();
        let args = [
            "bench",
            "catalog",
            "--memory",
            op.name(),
            "--memory-child",
            "--edges",
            &edges,
        ];
        let args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        let child = crate::bench::run_self(&args, &[], CHILD_DEADLINE)?;
        std::io::stdout().write_all(&child.stdout)?;
        std::io::stderr().write_all(&child.stderr)?;
        let ok = child.killed.is_none() && child.status.is_some_and(|s| s.success());
        Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }
}
