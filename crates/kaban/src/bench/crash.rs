//! `kaban bench crash` (spec 11 T6 Step 6, KER-02): kill a child at a named crash point of the
//! catalog `--kills` times, recover and verify after each kill, one JSONL line per kill.
//!
//! ```text
//!   parent: fresh store => arrange the catalog for the point
//!     | spawn `kaban bench crash --child <dir>` with KABAN_FAILPOINT=<point>
//!     v
//!   child: open the store => run the operation => SIGKILL at the point
//!     |
//!     v
//!   parent: recover_catalog => verify_catalog => one JSONL line
//! ```
//!
//! The first kill whose child was not killed at its point, or whose verify is not clean, stops
//! the campaign (exit 1) and keeps its store directory; every other store directory is removed.
//! Needs a build with the `bench` feature (the store's test class and crash points).

use std::path::PathBuf;
use std::process::ExitCode;

/// The catalog phase a kill lands in.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Phase {
    /// A publication (`publish.after_append`, `publish.before_commit`)
    Publish,
    /// A seal (`seal.mid`)
    Seal,
    /// A fold (`fold.mid`)
    Fold,
    /// A reclamation (`reclaim.after_floor`, `reclaim.mid_punch`)
    Reclaim,
}

/// `bench crash` arguments.
#[derive(clap::Args)]
pub(crate) struct Args {
    /// How many children to kill
    #[arg(long, default_value_t = 250)]
    kills: u32,
    /// Phases to kill in, comma-separated (default: every crash point, in turn)
    #[arg(long, value_enum, value_delimiter = ',')]
    phase: Vec<Phase>,
    /// The JSONL file one line per kill is appended to
    #[arg(long, default_value = "crash.jsonl")]
    out: PathBuf,
    /// The crashing child's store directory (internal)
    #[arg(long, hide = true)]
    child: Option<PathBuf>,
}

/// Run the campaign, or the crashing child.
#[cfg(feature = "bench")]
pub(crate) fn run(args: &Args) -> ExitCode {
    let result = match &args.child {
        Some(dir) => campaign::child(dir),
        None => campaign::run(args),
    };
    result.unwrap_or_else(|e| {
        crate::cli::output::print_failure(&e);
        ExitCode::FAILURE
    })
}

/// Without the `bench` feature: refused with exit 4.
#[cfg(not(feature = "bench"))]
pub(crate) fn run(args: &Args) -> ExitCode {
    let _ = (args.kills, &args.phase, &args.out, &args.child);
    crate::cli::stubs::not_available("bench crash", "feature bench")
}

#[cfg(feature = "bench")]
mod campaign {
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use kaban_core::config::{CatalogCfg, SealOps};
    use kaban_core::query_budget::QueryBudget;
    use kaban_core::{Actor, MemoryStore, NewNode, NodeId, RememberOutcome, WorkspaceId, WriteCtx};
    use kaban_store_sqlite::{CompactBudget, SqliteStore, catalog_testing};
    use serde_json::json;

    use super::{Args, Phase};

    /// The bound of one child's run.
    const CHILD_WAIT: Duration = Duration::from_secs(60);

    /// The crash points of `phases`; every point (`recover.mid` included) without one.
    fn points(phases: &[Phase]) -> Vec<&'static str> {
        if phases.is_empty() {
            return vec![
                "publish.after_append",
                "publish.before_commit",
                "seal.mid",
                "fold.mid",
                "reclaim.after_floor",
                "reclaim.mid_punch",
                "recover.mid",
            ];
        }
        phases
            .iter()
            .flat_map(|phase| match phase {
                Phase::Publish => &["publish.after_append", "publish.before_commit"][..],
                Phase::Seal => &["seal.mid"][..],
                Phase::Fold => &["fold.mid"][..],
                Phase::Reclaim => &["reclaim.after_floor", "reclaim.mid_punch"][..],
            })
            .copied()
            .collect()
    }

    /// The store of `dir`, sealing at 2 pending rows and folding past 2 layers, with the test
    /// class installed.
    fn open(dir: &Path) -> Result<SqliteStore> {
        let policy = CatalogCfg {
            seal_ops: SealOps {
                graph: 2,
                ..SealOps::default()
            },
            fold_depth: 2,
            ..CatalogCfg::default()
        };
        let store = SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys"))?
            .with_catalog_config(policy)?;
        let _observed = catalog_testing::install_edge_sediment_class(&store);
        Ok(store)
    }

    fn ctx() -> WriteCtx {
        WriteCtx::new(WorkspaceId::default(), Actor::Agent)
    }

    /// A fresh node of `text`; its id.
    fn node(store: &SqliteStore, text: &str) -> Result<NodeId> {
        let new = NewNode {
            kind: "Fact".into(),
            text: text.into(),
            ..Default::default()
        };
        match store.remember(new, &[], &ctx())? {
            RememberOutcome::Created(n) | RememberOutcome::Existing { node: n, .. } => {
                Ok(NodeId::parse(&n.id)?)
            }
        }
    }

    /// Arrange the catalog for a crash at `point`: pending sediment for a seal, a retired
    /// extent for a reclamation, a chain past the fold depth otherwise.
    fn arrange(store: &SqliteStore, point: &str) -> Result<()> {
        match point {
            "seal.mid" => {
                let a = node(store, "the lamp is lit at dusk")?;
                let b = node(store, "the keeper climbs the stair")?;
                store.link(&a, &b, "RELATES_TO", None, None, &ctx())?;
                store.link(&b, &a, "RELATES_TO", None, None, &ctx())?;
            }
            "reclaim.after_floor" | "reclaim.mid_punch" => {
                catalog_testing::append_and_publish(store, &[1; 4096])?;
                let doomed = catalog_testing::append_and_publish_into(store, &[2; 4096], &[])?;
                catalog_testing::retire(store, &[doomed])?;
            }
            _ => {
                catalog_testing::append_and_publish_all(
                    store,
                    &[vec![1; 4096], vec![2; 4096], vec![3; 4096]],
                )?;
            }
        }
        Ok(())
    }

    /// The crashing child: run the operation that reaches the point `KABAN_FAILPOINT` arms,
    /// where the process kills itself. Surviving it is exit 3.
    pub(super) fn child(dir: &Path) -> Result<ExitCode> {
        let store = open(dir)?;
        let point = std::env::var(catalog_testing::FAILPOINT_ARM).unwrap_or_default();
        match point.as_str() {
            "reclaim.after_floor" | "reclaim.mid_punch" => {
                catalog_testing::reclaim_retired(&store)?;
            }
            "recover.mid" => {
                // what a dead compaction leaves: a stale claim and an unpublished range
                let budget = QueryBudget::new(u64::MAX, Some(CHILD_WAIT));
                let live = catalog_testing::pin(&store, &budget)?.view().extents();
                drop(catalog_testing::claim(&store, &live[..1])?);
                catalog_testing::append_unpublished(&store, &[9; 512])?;
                store.recover_catalog()?;
            }
            _ => {
                let budget = CompactBudget {
                    memory_bytes: u64::MAX,
                    deadline: CHILD_WAIT,
                };
                catalog_testing::compact(&store, budget)?;
            }
        }
        Ok(ExitCode::from(3))
    }

    /// One kill at `point` over a fresh store in `dir`: its JSONL line and whether the child
    /// died at the point and the recovered catalog verifies clean.
    fn kill(dir: &Path, n: u32, point: &str) -> Result<(serde_json::Value, bool)> {
        let started = Instant::now();
        let store = open(dir)?;
        arrange(&store, point)?;
        let args = [
            "bench".as_ref(),
            "crash".as_ref(),
            "--child".as_ref(),
            dir.as_os_str(),
        ];
        let child = crate::bench::run_self(
            &args,
            &[(catalog_testing::FAILPOINT_ARM, point)],
            CHILD_WAIT,
        )?;
        std::fs::write(dir.join("child.stderr"), &child.stderr)?;
        let status = child.status;
        let recovered = store.recover_catalog()?;
        let verified = store.verify_catalog()?;
        let signal = status.and_then(|s| s.signal());
        // killed by its own crash point, not by the runner's deadline
        let killed = signal == Some(9) && child.killed.is_none();
        let inconsistencies = verified.inconsistencies();
        let line = json!({
            "kill": n,
            "point": point,
            "killed": killed,
            "exit": status.and_then(|s| s.code()),
            "signal": signal,
            "recover": {
                "maintenance_live": recovered.maintenance_live,
                "claims_cleared": recovered.claims_cleared,
                "ranges_reclaimed": recovered.ranges_reclaimed.len(),
            },
            "inconsistencies": inconsistencies,
            "elapsed_ms": started.elapsed().as_secs_f64() * 1e3,
        });
        Ok((line, killed && inconsistencies == 0))
    }

    /// `--kills` kills rotating through the points of `--phase`.
    pub(super) fn run(args: &Args) -> Result<ExitCode> {
        let points = points(&args.phase);
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&args.out)
            .with_context(|| format!("opening {}", args.out.display()))?;
        let root = std::env::temp_dir().join(format!("kaban-crash-{}", std::process::id()));
        for n in 0..args.kills {
            let point = points[n as usize % points.len()];
            let dir = root.join(format!("{n}-{}", point.replace('.', "-")));
            std::fs::create_dir_all(&dir)?;
            let (line, clean) = kill(&dir, n, point)?;
            writeln!(out, "{line}")?;
            if !clean {
                bail!(
                    "kill {n} at {point} is not clean; its store is kept in {}",
                    dir.display()
                );
            }
            std::fs::remove_dir_all(&dir)?;
        }
        let _ = std::fs::remove_dir(&root);
        crate::cli::output::print_json(
            &json!({ "kills": args.kills, "points": points, "verify_failures": 0 }),
            crate::cli::output::Format::Compact,
        )?;
        Ok(ExitCode::SUCCESS)
    }
}
