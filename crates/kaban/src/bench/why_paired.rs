//! `kaban bench why-paired` (engine-spec EN-N7; engine-plan E2.7 item 5): the `node_anchor`
//! path prefilter of `why` against today's `json_each` scan on the 100k corpus, as a paired
//! same-process comparison. `before` is a schema-3 copy of the bench database (the migration
//! backup `kaban bench --schema-version 3` leaves beside it), opened with
//! `SqliteStore::open_at_schema_version(_, _, 3)` so it is never migrated; `after` is the
//! migrated copy, opened with `SqliteStore::open`. Both services are built as the bench's
//! (`runner.rs`: the fixture repository beside the database, the first workspace, the bench
//! repository identity). After `warmup` untimed calls per copy, `samples` rounds each run one
//! path target on both copies, the copy order alternating per round; the two series, both
//! p50s, their ratio and `pass` against `regression_pct` come back in one document. `pass` is
//! informational (OWN-13): a fail is recorded, nothing stops.
//!
//! ```text
//!   before (0003 copy) --open_at_schema_version(3)--> api_b   \   round i: target t_i on
//!   after  (migrated)  --open-----------------------> api_a   /   (b, a) or (a, b), timed
//!   ==> PairedResult { before_ms[], after_ms[], p50_before, p50_after, ratio, pass }
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use kaban_api::{ApiConfig, KabanApi, WhyRequest, WhyTarget};
use kaban_core::{ActorId, GraphStore, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde::Serialize;
use serde_json::{Value, json};

use super::corpus::{Corpus, SEED, WORKSPACES};
use super::report::{machine_profile, pct};
use crate::cli::output;

/// The schema `before` is opened at: the one before `node_anchor` (migration 0004).
const BEFORE_SCHEMA: u32 = 3;
/// The directory name `kaban bench` gives the N2 fixture repository beside its database.
const FIXTURE_REPO: &str = "bench-fixture-repo";

/// The result of one paired run (engine-plan E2.7 interfaces).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PairedResult {
    /// Wall time of each `why` call on the schema-3 copy, in rounds order (ms).
    pub before_ms: Vec<f64>,
    /// Wall time of each `why` call on the migrated copy, in rounds order (ms).
    pub after_ms: Vec<f64>,
    pub p50_before: f64,
    pub p50_after: f64,
    /// `p50_after / p50_before` (`1.0` when `p50_before` is zero).
    pub ratio: f64,
    /// `p50_after <= p50_before * (1 + regression_pct / 100)`; informational.
    pub pass: bool,
}

/// The command's arguments.
#[derive(Debug, Clone)]
pub(crate) struct Args {
    pub before: PathBuf,
    pub after: PathBuf,
    pub samples: usize,
    pub warmup: usize,
    pub regression_pct: u32,
    pub out: Option<PathBuf>,
}

/// One copy as the bench reads it: the store and the service over it.
struct Copy {
    schema: u32,
    api: KabanApi,
}

impl Copy {
    fn open(
        db: &Path,
        keys_dir: &Path,
        actor: &ActorId,
        policy: Policy,
        schema: Option<u32>,
    ) -> anyhow::Result<Self> {
        if let Some(v) = schema {
            // OWN-P: kaban serves one schema; the schema-3 scan side can no longer be opened
            anyhow::bail!(
                "kaban serves schema {} only (OWN-P): the paired bench's schema-{v} copy cannot be opened; the scan side is gone with the migration ladder",
                kaban_store_sqlite::LATEST_SCHEMA_VERSION
            );
        }
        let store = SqliteStore::open_as(db, keys_dir, actor)
            .with_context(|| format!("opening {}", db.display()))?;
        let schema = store.migration().to;
        let signer = store.signer();
        let store: Arc<dyn GraphStore> = Arc::new(store);
        // the fixture repository sits beside the database (`runner.rs`); without it the
        // currencies read `uncheckable`, which costs the same on both copies
        let fixture = db.with_file_name(FIXTURE_REPO);
        let repo_root = fixture.is_dir().then_some(fixture);
        let api = KabanApi::with_config(
            store,
            policy,
            signer,
            None,
            ApiConfig {
                workspace: WorkspaceId::new(WORKSPACES[0].0),
                repo_root,
                repo: Some(FIXTURE_REPO.into()),
                ..Default::default()
            },
        );
        Ok(Self { schema, api })
    }

    /// One timed `why` of `target` (ms).
    fn why_ms(&self, target: &str) -> anyhow::Result<f64> {
        let s = Instant::now();
        let r = self.api.why(WhyRequest {
            targets: vec![WhyTarget::Path(target.to_owned())],
            limit: Some(10),
            ..Default::default()
        })?;
        let elapsed = s.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(r);
        Ok(elapsed)
    }
}

/// The `samples` path targets, drawn like the bench's `why_one_path` targets
/// (`Corpus::vocab_path`: `src/<vocab>/<vocab>.rs`, under the directories the corpus Decisions
/// govern), from the corpus seed so every run asks the same questions.
fn targets(samples: usize) -> Vec<String> {
    let mut corpus = Corpus::new(SEED);
    (0..samples).map(|_| corpus.vocab_path()).collect()
}

/// Run the paired comparison; `policy_path` is loaded once per copy (`Policy` is not `Clone`).
///
/// # Errors
/// A copy that does not open, a `before` copy not at schema 3, or a `why` failure.
pub(crate) fn run(
    args: &Args,
    keys_dir: &Path,
    actor: &ActorId,
    policy_path: Option<&Path>,
) -> anyhow::Result<(PairedResult, Value)> {
    let load = || -> anyhow::Result<Policy> {
        Ok(match policy_path {
            Some(p) => Policy::load(p)?,
            None => Policy::empty(),
        })
    };
    let before = Copy::open(&args.before, keys_dir, actor, load()?, Some(BEFORE_SCHEMA))?;
    if before.schema != BEFORE_SCHEMA {
        anyhow::bail!(
            "--before {} is at schema {}, not {BEFORE_SCHEMA}: the scan side needs the 0003 copy",
            args.before.display(),
            before.schema
        );
    }
    let after = Copy::open(&args.after, keys_dir, actor, load()?, None)?;
    let targets = targets(args.samples.max(1));
    // warm-up: the page cache and the prepared statements of each copy, untimed
    for target in targets
        .iter()
        .take(args.warmup.max(1))
        .cycle()
        .take(args.warmup)
    {
        before.why_ms(target)?;
        after.why_ms(target)?;
    }
    let mut before_ms = Vec::with_capacity(targets.len());
    let mut after_ms = Vec::with_capacity(targets.len());
    for (round, target) in targets.iter().enumerate() {
        if round % 2 == 0 {
            before_ms.push(before.why_ms(target)?);
            after_ms.push(after.why_ms(target)?);
        } else {
            after_ms.push(after.why_ms(target)?);
            before_ms.push(before.why_ms(target)?);
        }
    }
    let p50_before = pct(&mut before_ms.clone(), 0.5);
    let p50_after = pct(&mut after_ms.clone(), 0.5);
    let ratio = if p50_before > 0.0 {
        p50_after / p50_before
    } else {
        1.0
    };
    let pass = p50_after <= p50_before * (1.0 + f64::from(args.regression_pct) / 100.0);
    let result = PairedResult {
        before_ms,
        after_ms,
        p50_before,
        p50_after,
        ratio,
        pass,
    };
    let document = json!({
        "bench_schema": 4,
        "bench": "why_paired",
        "machine": machine_profile(),
        "seed": format!("{SEED:#x}"),
        "before": { "path": args.before.display().to_string(), "schema": before.schema },
        "after": { "path": args.after.display().to_string(), "schema": after.schema },
        "samples": targets.len(),
        "warmup": args.warmup,
        "regression_pct": args.regression_pct,
        "targets": targets,
        "informational": true,
        "result": result,
        // filled by `../specs/kaban/archive/benchmarks/kaban2/scripts/bench/bench-why-paired.sh` from the release binary (PROTOCOL §4)
        "binary_sha256": Value::Null,
    });
    Ok((result, document))
}

/// `kaban bench why-paired ...`: run, print the one JSON document (and write `--out`);
/// failures print their outer message and map to the CLI exit codes.
pub(crate) fn dispatch(
    args: &Args,
    keys_dir: &Path,
    actor: &ActorId,
    policy_path: Option<&Path>,
) -> ExitCode {
    match run(args, keys_dir, actor, policy_path) {
        Ok((_, document)) => {
            if let Some(out) = &args.out
                && let Err(e) = write_out(out, &document)
            {
                output::print_failure(&e);
                return ExitCode::from(output::EXIT_FAILURE);
            }
            match output::print_json_pretty(&document) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    output::print_failure(&e);
                    ExitCode::from(output::EXIT_FAILURE)
                }
            }
        }
        Err(e) => {
            output::print_failure(&e);
            output::exit_code_of(&e)
        }
    }
}

fn write_out(out: &Path, document: &Value) -> anyhow::Result<()> {
    if let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(out, serde_json::to_vec_pretty(document)?)
        .with_context(|| format!("writing {}", out.display()))
}
