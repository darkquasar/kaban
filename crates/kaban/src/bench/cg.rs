//! `kaban bench cg gen|run` (stage1-plan §18, CG7.3, CG10.1; feature `bench`): the synthetic
//! code-graph snapshot through `kaban_codegraph::testing::corpus` (or any admitted snapshot, the
//! go-drivers fixture's included) and the API-level ops over it, timed one call per run after a
//! warm-up. The tool ops call the `kaban-api` handlers over a `FixedSnapshot` with the head
//! pinned (`KabanApi::with_pinned_head`), so no git runs and no governance join is involved (the
//! G1-N2 definition); `open_ro` is one `FixedSnapshot::resolve` (`open_ro`, `quick_check` once
//! per process, `validate_for_attach`), `rss_open` the `VmHWM` after the opens with its delta
//! against the start (N6). `code_context_split` (N7) times the three parts of one
//! `code_context` call through the handler's hidden `code_context_timed` seam (the reader part
//! gated at p50 < 5 ms; `why(symbol)` and the git `last_change` informational) beside the whole
//! call. `denylist_fp_rate` (P9) scans every symbol name, doc string and snippet of the snapshot
//! through the loaded policy and reports hits by rule id as counts only.
//!
//! ```text
//!   gen  --symbols N --seed S <dir>   ==> <dir>/<commit40>.cg.db (0600) + the targets in meta
//!   run  <snapshot> --warmup 3 --runs 20 [--ops a,b] [--out file]
//!        targets <== meta bench:*  (hub, interface, tested; every one by `ref`: a file node
//!                                   owns no CONTAINS edge, so `tool_members` reads the top
//!                                   interface's methods, not the hub's file; CG7.3a)
//!                <== KABAN_BENCH_TARGETS=hub=<ref>,interface=<ref>,tested=<ref> when the
//!                    snapshot is not a bench corpus (CG10.1; `../specs/kaban/archive/benchmarks/kaban2/scripts/bench/cg_bench.sh` derives
//!                    the go-drivers refs from the fixture's committed edge golden)
//!        denylist_fp_rate <== KABAN_BENCH_FILES=<file>, one repository-relative path per line
//!                    (the fixture's `git ls-files`): every node defined in those files through
//!                    the reader's `nodes_in_file`; without it the op reports nothing scanned
//!        per op: warmup calls, then `runs` timed calls ==> us_runs[], p50, p95
//!        code_context_split ==> four series from the same calls (reader, governance,
//!                               last_change, total)
//!        denylist_fp_rate   ==> informational.denylist_fp_rate (counts, never text)
//!        ==> the bench_schema 3 document (`codegraph` section; the §18 gates with pass
//!            recorded: N1, N2, N6 and the N7 reader part binding, the rest informational)
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use kaban_api::{
    ApiConfig, CodeContextRequest, CodeDefinitionRequest, CodeImplementersRequest,
    CodeMembersRequest, CodeNeighboursRequest, CodeReferencesRequest, CodeTestsForRequest,
    KabanApi, SymbolSelector,
};
use kaban_codegraph::testing::{FixedSnapshot, corpus};
use kaban_codegraph::{CodeGraphReader, HeadInput, Meta, NodeKind, SnapshotProvider};
use kaban_core::ActorId;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Map, Value, json};

use super::report::{machine_profile, pct, self_status_kb};
use crate::cli::output::{self, EXIT_INVALID, Format};
use crate::{BenchCg, BenchCgCmd};

/// `kaban bench cg <gen|run>`: parse the seed, run, print the one JSON document (and write
/// `--out`); failures print their outer message and map to the CLI exit codes (RM-1).
pub(crate) fn dispatch(
    db: &Path,
    keys: &Path,
    actor: &ActorId,
    policy: Policy,
    cg: BenchCg,
    format: Format,
) -> ExitCode {
    // `why-paired` is dispatched before this bench (`cli/mod.rs`); it never arrives here
    let BenchCg::Cg { cmd } = cg else {
        output::print_error("error: bench why-paired is not a code-graph bench");
        return ExitCode::from(EXIT_INVALID);
    };
    let outcome = match cmd {
        BenchCgCmd::Gen { symbols, seed, out } => {
            let Some(seed) = parse_seed(&seed) else {
                output::print_error("error: --seed is not a hexadecimal number");
                return ExitCode::from(EXIT_INVALID);
            };
            generate(&GenArgs { symbols, seed, out })
        }
        BenchCgCmd::Run {
            snapshot,
            ops,
            warmup,
            runs,
            out,
        } => run(
            db,
            keys,
            actor,
            policy,
            &RunArgs {
                snapshot,
                ops,
                warmup,
                runs,
            },
        )
        .and_then(|v| {
            if let Some(out) = &out {
                let text = format!("{}\n", serde_json::to_string_pretty(&v)?);
                std::fs::write(out, text).context("writing the result file")?;
            }
            Ok(v)
        }),
    };
    match outcome.and_then(|v| output::print_json(&v, format).map_err(Into::into)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            output::print_failure(&e);
            output::exit_code_of(&e)
        }
    }
}

/// `0x`-optional hexadecimal.
fn parse_seed(s: &str) -> Option<u64> {
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(digits, 16).ok()
}

/// Every op, in the §18 order; `code_context_split` (N7) and `denylist_fp_rate` (P9) joined
/// at CG10.1.
pub(crate) const OPS: [&str; 10] = [
    "open_ro",
    "tool_implementers",
    "tool_members",
    "tool_references",
    "tool_definition",
    "tool_tests_for",
    "tool_neighbours_d2",
    "code_context_split",
    "rss_open",
    "denylist_fp_rate",
];

/// The `ops.<name>` series of `code_context_split`, one per part plus the whole call, from
/// the same measured calls.
const SPLIT_SERIES: [&str; 4] = [
    "code_context_split_reader",
    "code_context_split_governance",
    "code_context_split_last_change",
    "code_context_split_total",
];

/// The `why(symbol)` baseline the governance part is read against (19.6 ms p50 at 100k
/// notes, OWN-13; informational).
const WHY_BASELINE_MS: f64 = 19.6;

/// The `GitRunner` bound of `last_change` (stage1-spec G1-N7; informational).
const LAST_CHANGE_BOUND_MS: f64 = 200.0;

/// The six tool ops (N2: p50 < 5 ms, p95 < 20 ms each).
const TOOL_OPS: [&str; 6] = [
    "tool_implementers",
    "tool_members",
    "tool_references",
    "tool_definition",
    "tool_tests_for",
    "tool_neighbours_d2",
];

/// `kaban bench cg gen`.
pub(crate) struct GenArgs {
    /// Symbols (rounded down to whole 20-symbol files).
    pub(crate) symbols: usize,
    /// The xorshift seed.
    pub(crate) seed: u64,
    /// The output directory (created 0700 when missing).
    pub(crate) out: PathBuf,
}

/// `kaban bench cg run`.
pub(crate) struct RunArgs {
    /// `<dir>/<commit40>.cg.db`.
    pub(crate) snapshot: PathBuf,
    /// The ops to run (empty = every op), in [`OPS`] order.
    pub(crate) ops: Vec<String>,
    /// Untimed calls before the measured ones.
    pub(crate) warmup: usize,
    /// Timed calls per op.
    pub(crate) runs: usize,
}

/// `bench cg gen`: generate the corpus; the report is the stats as one JSON object (`path` is
/// what `run` takes).
pub(crate) fn generate(a: &GenArgs) -> anyhow::Result<Value> {
    if !a.out.exists() {
        std::fs::create_dir_all(&a.out).context("creating the output directory")?;
        std::fs::set_permissions(&a.out, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .context("setting the output directory mode")?;
    }
    let s = corpus::generate(&a.out, a.symbols, a.seed).context("generating the corpus")?;
    Ok(json!({
        "path": s.path,
        "commit": s.commit,
        "seed": format!("{:#x}", a.seed),
        "files": s.files, "symbols": s.symbols, "nodes": s.nodes, "edges": s.edges,
        "occurrences": s.occurrences,
        "hub": s.hub, "hub_in_degree": s.hub_in_degree,
        "interface": s.interface, "interface_implementers": s.interface_implementers,
        "tested": s.tested, "tested_count": s.tested_count,
        "members_path": s.members_path,
    }))
}

/// The bench targets a corpus publishes in `meta` (`bench:*`; `bench:members_path`, the hub
/// file's path, is published too but no op reads it: `code_members` of a file node is empty
/// by the CONTAINS rule, so it would time nothing), or the three refs of `KABAN_BENCH_TARGETS`
/// for any other admitted snapshot (CG10.1, the go-drivers fixture; the reader trait
/// enumerates no rows, so the bench cannot derive them itself). The hub is the
/// `code_context_split` target too: a defined symbol with references.
struct Targets {
    hub: String,
    interface: String,
    tested: String,
    /// The corpus seed, `None` for an env-given set.
    seed: Option<String>,
    /// `meta` or `env`.
    source: &'static str,
    symbols: u64,
    edges: u64,
    hub_in_degree: u64,
}

/// The env var naming the targets of a snapshot that is not a bench corpus.
const TARGETS_ENV: &str = "KABAN_BENCH_TARGETS";

/// The env var naming the file list the P9 scan walks.
const FILES_ENV: &str = "KABAN_BENCH_FILES";

impl Targets {
    /// The `bench:*` rows when the snapshot is a corpus, else [`TARGETS_ENV`].
    fn load(meta: &Meta) -> anyhow::Result<Self> {
        if meta.contains_key("bench:hub") {
            Self::from_meta(meta)
        } else {
            Self::from_env(meta)
        }
    }

    fn from_meta(meta: &Meta) -> anyhow::Result<Self> {
        let text = |k: &str| {
            meta.get(k)
                .cloned()
                .ok_or_else(|| anyhow!("the snapshot's meta has no `{k}` row; not a bench corpus"))
        };
        Ok(Self {
            hub: text("bench:hub")?,
            interface: text("bench:interface")?,
            tested: text("bench:tested")?,
            seed: Some(text("bench:seed")?),
            source: "meta",
            symbols: count(meta, "counts:symbols"),
            edges: count(meta, "counts:edges"),
            hub_in_degree: count(meta, "bench:hub_in_degree"),
        })
    }

    /// `KABAN_BENCH_TARGETS=hub=<ref>,interface=<ref>,tested=<ref>` (any order; the three keys
    /// required, the values opaque refs the ops resolve).
    fn from_env(meta: &Meta) -> anyhow::Result<Self> {
        let raw = std::env::var(TARGETS_ENV).map_err(|_| {
            anyhow!("the snapshot is not a bench corpus and {TARGETS_ENV} is not set")
        })?;
        let mut given: BTreeMap<&str, &str> = BTreeMap::new();
        for pair in raw.split(',').filter(|p| !p.is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("{TARGETS_ENV}: expected key=ref pairs"))?;
            given.insert(k.trim(), v.trim());
        }
        let take = |k: &str| -> anyhow::Result<String> {
            given
                .get(k)
                .filter(|v| !v.is_empty())
                .map(|v| (*v).to_owned())
                .ok_or_else(|| anyhow!("{TARGETS_ENV}: `{k}` is missing"))
        };
        Ok(Self {
            hub: take("hub")?,
            interface: take("interface")?,
            tested: take("tested")?,
            seed: None,
            source: "env",
            symbols: count(meta, "counts:symbols"),
            edges: count(meta, "counts:edges"),
            hub_in_degree: 0,
        })
    }
}

/// A `counts:*` row as a number (0 when absent).
fn count(meta: &Meta, k: &str) -> u64 {
    meta.get(k).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The P9 file list: the lines of the file `KABAN_BENCH_FILES` names (empty lines dropped),
/// or none when the variable is unset.
fn files_from_env() -> anyhow::Result<Vec<String>> {
    let Ok(path) = std::env::var(FILES_ENV) else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {FILES_ENV}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The four series of one `code_context_split` run (microseconds): the three parts from the
/// handler's own clocks, the whole call from ours.
#[derive(Debug, Default)]
struct Split {
    reader: Vec<f64>,
    governance: Vec<f64>,
    last_change: Vec<f64>,
    total: Vec<f64>,
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// `warmup` untimed `code_context` calls on `target` (by `ref`), then `runs` measured ones;
/// the error of a call (a target without a definition is the handler's fixed
/// `unknown_symbol`) ends the op with no partial series.
fn context_split(
    api: &KabanApi,
    target: &str,
    warmup: usize,
    runs: usize,
) -> anyhow::Result<Split> {
    let call = || -> anyhow::Result<(Duration, Duration, Duration, Duration)> {
        let t = Instant::now();
        let (_, parts) = api.code_context_timed(CodeContextRequest {
            target: by_ref(target),
            current_path: None,
        })?;
        Ok((
            parts.reader,
            parts.governance,
            parts.last_change,
            t.elapsed(),
        ))
    };
    for _ in 0..warmup {
        call()?;
    }
    let mut split = Split::default();
    for _ in 0..runs {
        let (reader, governance, last_change, total) = call()?;
        split.reader.push(us(reader));
        split.governance.push(us(governance));
        split.last_change.push(us(last_change));
        split.total.push(us(total));
    }
    Ok(split)
}

/// One content class of the P9 scan.
#[derive(Debug, Default, Clone, Copy)]
struct Class {
    scanned: u64,
    hit: u64,
}

/// P9: the name, doc string and snippet of every symbol or local node defined in `files`
/// (the reader's `nodes_in_file`, the rows the tools project) through `Policy::scan`; the
/// report carries counts only (files walked, strings scanned and hit per class, hits by rule
/// id, strings over the policy's field cap, the hit rate) and never a matched text or an
/// offset.
fn denylist_fp_rate(
    policy: &Policy,
    reader: &dyn CodeGraphReader,
    files: &[String],
) -> anyhow::Result<Value> {
    let mut by_rule: BTreeMap<String, u64> = BTreeMap::new();
    let mut too_long = 0u64;
    let mut scan = |class: &mut Class, text: Option<&str>| {
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            return;
        };
        class.scanned += 1;
        let verdict = policy.scan(text);
        if verdict.hits.is_empty() && verdict.reject_rules.is_empty() {
            return;
        }
        class.hit += 1;
        for hit in &verdict.hits {
            *by_rule.entry(hit.rule.clone()).or_default() += 1;
        }
        if verdict.hits.is_empty() {
            // the one hit-less rejection is the field cap (`input-too-long`)
            too_long += 1;
        }
    };
    let (mut names, mut docs, mut snippets) =
        (Class::default(), Class::default(), Class::default());
    let mut nodes = 0u64;
    for path in files {
        for row in reader
            .nodes_in_file(path)
            .map_err(|e| anyhow!("reading a file's nodes failed ({e})"))?
        {
            if row.kind == NodeKind::File {
                continue;
            }
            nodes += 1;
            scan(&mut names, Some(row.name.as_str()));
            scan(&mut docs, row.doc.as_deref());
            scan(&mut snippets, row.snippet.as_deref());
        }
    }
    let scanned = names.scanned + docs.scanned + snippets.scanned;
    let hit = names.hit + docs.hit + snippets.hit;
    let rate = if scanned == 0 {
        0.0
    } else {
        hit as f64 / scanned as f64
    };
    Ok(json!({
        "policy_hash": policy.effective_hash(),
        "files": files.len(),
        "nodes": nodes,
        "scanned": {
            "names": names.scanned, "docs": docs.scanned, "snippets": snippets.scanned,
            "total": scanned,
        },
        "strings_hit": {
            "names": names.hit, "docs": docs.hit, "snippets": snippets.hit, "total": hit,
        },
        "hits_by_rule": by_rule,
        "too_long": too_long,
        "rate": rate,
    }))
}

/// The 40-hex commit of `<commit40>.cg.db`.
fn commit_of(snapshot: &Path) -> anyhow::Result<String> {
    snapshot
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".cg.db"))
        .filter(|c| c.len() == 40)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the snapshot is not a `<commit40>.cg.db` file"))
}

fn by_ref(r#ref: &str) -> SymbolSelector {
    SymbolSelector {
        r#ref: Some(r#ref.to_owned()),
        ..Default::default()
    }
}

/// `warmup` untimed calls, then `runs` timed ones (microseconds).
fn measure(
    warmup: usize,
    runs: usize,
    mut call: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Vec<f64>> {
    for _ in 0..warmup {
        call()?;
    }
    let mut us = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        call()?;
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    Ok(us)
}

/// `YYYY-MM-DD` (UTC) from the system clock (civil-from-days, no calendar crate).
fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let z = i64::try_from(secs / 86_400).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// One op's series as the result's `ops.<name>` entry.
fn series(us: &[f64]) -> Value {
    let mut sorted = us.to_vec();
    json!({
        "n": us.len(),
        "p50_us": pct(&mut sorted, 0.5),
        "p95_us": pct(&mut sorted, 0.95),
        "max_us": pct(&mut sorted, 1.0),
        "us_runs": us,
    })
}

/// A §18 gate row (`pass` recorded; `informational` rows are read against a baseline or a
/// bound and never block, the others are binding from CG10.1 on).
fn gate(value: Option<f64>, limit: f64, informational: bool) -> Value {
    json!({
        "value": value,
        "limit": limit,
        "pass": value.map(|v| v < limit),
        "informational": informational,
    })
}

/// The opened bench: the provider, the pinned-head service and the targets, plus the
/// readings taken on the way (the first open with `quick_check`, the file size, `VmHWM`).
struct Session {
    provider: FixedSnapshot,
    head: HeadInput,
    api: KabanApi,
    targets: Targets,
    commit: String,
    bytes: u64,
    open_ro_first_us: f64,
    vm_hwm_before: Option<u64>,
    /// The P9 report when the op was asked for.
    fp_rate: Option<Value>,
}

impl Session {
    /// Open `snapshot` (the first open of the process pays `quick_check` once, N1
    /// "incl. `quick_check`"), read the targets, build the service as the adapters do but
    /// without git (the pinned head) and with the snapshot's directory as the repository root
    /// (`last_change` runs its one `git log` there).
    fn open(
        db: &Path,
        keys: &Path,
        actor: &ActorId,
        policy: Policy,
        snapshot: &Path,
        p9_files: Option<&[String]>,
    ) -> anyhow::Result<Self> {
        let commit = commit_of(snapshot)?;
        let dir = snapshot
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("the snapshot has no parent directory"))?;
        let provider = FixedSnapshot::new(snapshot.to_path_buf());
        let head = HeadInput {
            commit: commit.clone(),
            ancestors: Vec::new(),
        };
        let vm_hwm_before = self_status_kb("VmHWM");
        let t = Instant::now();
        let first = provider
            .resolve("bench", &head)
            .map_err(|r| anyhow!("the snapshot did not resolve ({r:?})"))?;
        let open_ro_first_us = t.elapsed().as_secs_f64() * 1e6;
        let reader: &dyn CodeGraphReader = first.snapshot.as_ref();
        let targets = Targets::load(&reader.meta().context("reading meta")?)?;
        let bytes = std::fs::metadata(snapshot).context("snapshot size")?.len();
        // P9 is a reading over the rows, not a timing: taken on the first open, before the
        // service owns the policy
        let fp_rate = match p9_files {
            Some(files) => Some(denylist_fp_rate(&policy, reader, files)?),
            None => None,
        };
        drop(first);

        let store = Arc::new(SqliteStore::open_as(db, keys, actor)?);
        let signer = store.signer();
        let api = KabanApi::with_config(
            store,
            policy,
            signer,
            None,
            ApiConfig {
                repo_root: Some(dir.clone()),
                repo: Some("bench".to_owned()),
                ..Default::default()
            },
        )
        .with_snapshot_provider(dir, Arc::new(provider.clone()))
        .with_pinned_head(&commit);
        Ok(Self {
            provider,
            head,
            api,
            targets,
            commit,
            bytes,
            open_ro_first_us,
            vm_hwm_before,
            fp_rate,
        })
    }
}

/// The ops asked for, in [`OPS`] order (empty = every op).
fn wanted(ops: &[String]) -> anyhow::Result<Vec<&'static str>> {
    if ops.is_empty() {
        return Ok(OPS.to_vec());
    }
    for op in ops {
        if !OPS.contains(&op.as_str()) {
            return Err(anyhow!("unknown op (the ops are: {})", OPS.join(",")));
        }
    }
    Ok(OPS
        .iter()
        .copied()
        .filter(|op| ops.iter().any(|w| w == op))
        .collect())
}

/// Run the ops over `snapshot` and build the `bench_schema: 3` document.
pub(crate) fn run(
    db: &Path,
    keys: &Path,
    actor: &ActorId,
    policy: Policy,
    a: &RunArgs,
) -> anyhow::Result<Value> {
    let wanted = wanted(&a.ops)?;
    let p9_files = if wanted.contains(&"denylist_fp_rate") {
        Some(files_from_env()?)
    } else {
        None
    };
    let mut s = Session::open(db, keys, actor, policy, &a.snapshot, p9_files.as_deref())?;
    let fp_rate = s.fp_rate.take();
    let (api, targets) = (&s.api, &s.targets);

    let mut ops = Map::new();
    let mut vm_hwm_after = None;
    for op in &wanted {
        let us = match *op {
            "open_ro" => measure(a.warmup, a.runs, || {
                s.provider
                    .resolve("bench", &s.head)
                    .map(drop)
                    .map_err(|r| anyhow!("open_ro failed ({r:?})"))
            })?,
            "tool_implementers" => measure(a.warmup, a.runs, || {
                api.code_implementers(CodeImplementersRequest {
                    target: by_ref(&targets.interface),
                    ..Default::default()
                })
                .map(drop)
                .map_err(Into::into)
            })?,
            "tool_members" => measure(a.warmup, a.runs, || {
                // the top interface's two methods (the real `code_members` shape); the hub
                // file's node resolves too but has no CONTAINS children
                api.code_members(CodeMembersRequest {
                    target: by_ref(&targets.interface),
                    ..Default::default()
                })
                .map(drop)
                .map_err(Into::into)
            })?,
            "tool_references" => measure(a.warmup, a.runs, || {
                api.code_references(CodeReferencesRequest {
                    target: by_ref(&targets.hub),
                    include_tests: true,
                    ..Default::default()
                })
                .map(drop)
                .map_err(Into::into)
            })?,
            "tool_definition" => measure(a.warmup, a.runs, || {
                api.code_definition(CodeDefinitionRequest {
                    target: by_ref(&targets.hub),
                    current_path: None,
                })
                .map(drop)
                .map_err(Into::into)
            })?,
            "tool_tests_for" => measure(a.warmup, a.runs, || {
                api.code_tests_for(CodeTestsForRequest {
                    target: by_ref(&targets.tested),
                    ..Default::default()
                })
                .map(drop)
                .map_err(Into::into)
            })?,
            "tool_neighbours_d2" => measure(a.warmup, a.runs, || {
                api.code_neighbours(CodeNeighboursRequest {
                    target: by_ref(&targets.hub),
                    depth: 2,
                    ..Default::default()
                })
                .map(drop)
                .map_err(Into::into)
            })?,
            "code_context_split" => {
                let split = context_split(api, &targets.hub, a.warmup, a.runs)?;
                for (name, series_us) in SPLIT_SERIES.iter().zip([
                    &split.reader,
                    &split.governance,
                    &split.last_change,
                    &split.total,
                ]) {
                    ops.insert((*name).to_owned(), series(series_us));
                }
                continue;
            }
            _ => {
                // rss_open: a reading, not a timing; denylist_fp_rate: taken at the open
                if *op == "rss_open" {
                    vm_hwm_after = self_status_kb("VmHWM");
                }
                continue;
            }
        };
        ops.insert((*op).to_owned(), series(&us));
    }
    if vm_hwm_after.is_none() {
        vm_hwm_after = self_status_kb("VmHWM");
    }

    // the §18 gates: N1 open_ro p50 < 1 ms; N2 p50 < 5 ms and p95 < 20 ms per tool; N6 VmHWM
    // delta < 10 MB; N7 the code_context reader part p50 < 5 ms, its governance part against
    // the `why` baseline and `last_change` against the runner bound (both informational)
    let ms = |op: &str, key: &str| ops.get(op).and_then(|v| v[key].as_f64()).map(|us| us / 1e3);
    let mut gates = Map::new();
    gates.insert(
        "open_ro_p50_ms".to_owned(),
        gate(ms("open_ro", "p50_us"), 1.0, false),
    );
    for op in TOOL_OPS {
        gates.insert(format!("{op}_p50_ms"), gate(ms(op, "p50_us"), 5.0, false));
        gates.insert(format!("{op}_p95_ms"), gate(ms(op, "p95_us"), 20.0, false));
    }
    let hwm_delta_mb = match (s.vm_hwm_before, vm_hwm_after) {
        (Some(b), Some(after)) => Some((after.saturating_sub(b)) as f64 / 1024.0),
        _ => None,
    };
    gates.insert(
        "rss_open_delta_mb".to_owned(),
        gate(hwm_delta_mb, 10.0, false),
    );
    gates.insert(
        "code_context_reader_p50_ms".to_owned(),
        gate(ms(SPLIT_SERIES[0], "p50_us"), 5.0, false),
    );
    gates.insert(
        "code_context_governance_p50_ms".to_owned(),
        gate(ms(SPLIT_SERIES[1], "p50_us"), WHY_BASELINE_MS, true),
    );
    gates.insert(
        "code_context_last_change_p50_ms".to_owned(),
        gate(ms(SPLIT_SERIES[2], "p50_us"), LAST_CHANGE_BOUND_MS, true),
    );

    let mut informational = json!({
        "open_ro_first_us": s.open_ro_first_us,
        "hub_in_degree": targets.hub_in_degree,
        "targets_source": targets.source,
        "vm_hwm_kb_before": s.vm_hwm_before,
        "vm_hwm_kb_after": vm_hwm_after,
        "rss_open_kb": vm_hwm_after,
    });
    if let Some(fp) = fp_rate {
        informational["denylist_fp_rate"] = fp;
    }

    Ok(json!({
        "bench_schema": 3,
        "date": today(),
        "record": "stage1-plan.md §18 code-graph bench (CG10.1 campaign: N1 open_ro, N2 six tools, N6 VmHWM delta, N7 code_context split, P9 denylist_fp_rate); N1, N2, N6 and the N7 reader part binding, the rest informational",
        "n_nodes": targets.symbols,
        "runs": a.runs,
        "seed": targets.seed,
        "machine": machine_profile(),
        "gate_values": {},
        "max_p50_rel_spread": 0,
        "per_op": {},
        "raw_runs": [],
        "codegraph": {
            "snapshot": {
                "symbols": targets.symbols,
                "edges": targets.edges,
                "bytes": s.bytes,
                "seed": targets.seed,
                "commit": s.commit,
            },
            "warmup": a.warmup,
            "ops": ops,
            "gates": gates,
            "informational": informational,
        },
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A private scratch directory, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("cg101-bench-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A 200-symbol corpus under `scratch` with its scratch store: `(snapshot, db, keys)`.
    fn corpus(scratch: &Scratch) -> (PathBuf, PathBuf, PathBuf) {
        let out = scratch.0.join("cg");
        let stats = generate(&GenArgs {
            symbols: 200,
            seed: 0x9e37_79b9_7f4a_7c15,
            out: out.clone(),
        })
        .unwrap();
        let snapshot = PathBuf::from(stats["path"].as_str().unwrap());
        (snapshot, scratch.0.join("kaban.db"), scratch.0.join("keys"))
    }

    fn examples_policy() -> Policy {
        Policy::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy.yaml"))
            .expect("examples/policy.yaml loads")
    }

    fn session(db: &Path, keys: &Path, snapshot: &Path) -> Session {
        Session::open(
            db,
            keys,
            &ActorId::default(),
            Policy::empty(),
            snapshot,
            None,
        )
        .unwrap()
    }

    /// The hub's `ref` and defining path over a resolved snapshot.
    fn hub_and_path(session: &Session) -> (String, String) {
        let resolved = session.provider.resolve("bench", &session.head).unwrap();
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let hub = reader
            .node(&session.targets.hub)
            .unwrap()
            .expect("the hub resolves");
        (
            hub.r#ref.clone(),
            hub.path.clone().expect("the hub is defined in a file"),
        )
    }

    /// Positive (N7): over the corpus the op yields the four series of `runs` values each,
    /// every run's three parts sum to no more than the whole call, and the reader gate row is
    /// binding with `pass` recorded.
    #[test]
    fn code_context_split_parts_sum_within_the_whole() {
        let scratch = Scratch::new("split");
        let (snapshot, db, keys) = corpus(&scratch);
        let doc = run(
            &db,
            &keys,
            &ActorId::default(),
            Policy::empty(),
            &RunArgs {
                snapshot,
                ops: vec!["code_context_split".to_owned()],
                warmup: 1,
                runs: 4,
            },
        )
        .unwrap();
        let ops = doc["codegraph"]["ops"].as_object().unwrap();
        assert_eq!(ops.len(), 4, "the four series and nothing else");
        let runs = |name: &str| -> Vec<f64> {
            ops[name]["us_runs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap())
                .collect()
        };
        let reader = runs(SPLIT_SERIES[0]);
        let governance = runs(SPLIT_SERIES[1]);
        let last_change = runs(SPLIT_SERIES[2]);
        let total = runs(SPLIT_SERIES[3]);
        assert_eq!(total.len(), 4);
        for i in 0..4 {
            let parts = reader[i] + governance[i] + last_change[i];
            assert!(
                parts <= total[i],
                "run {i}: the parts ({parts} us) exceed the whole ({} us)",
                total[i]
            );
            assert!(reader[i] > 0.0, "the reader part did work");
        }
        let gate = &doc["codegraph"]["gates"]["code_context_reader_p50_ms"];
        assert_eq!(gate["informational"], false);
        assert_eq!(gate["limit"], 5.0);
        assert!(gate["pass"].is_boolean(), "pass recorded");
        assert_eq!(
            doc["codegraph"]["gates"]["code_context_governance_p50_ms"]["informational"],
            true
        );
        assert_eq!(doc["codegraph"]["informational"]["targets_source"], "meta");
    }

    /// Negative (N7): a target without a definition ends the op with the handler's fixed
    /// `unknown_symbol` error (the one `code_definition` gives the same target) and no series.
    #[test]
    fn code_context_split_without_definition_is_the_fixed_error() {
        let scratch = Scratch::new("split-neg");
        let (snapshot, db, keys) = corpus(&scratch);
        let session = session(&db, &keys, &snapshot);
        let bogus = "scip:no-such-symbol";
        let err = context_split(&session.api, bogus, 1, 2).unwrap_err();
        let fixed = session
            .api
            .code_definition(CodeDefinitionRequest {
                target: by_ref(bogus),
                current_path: None,
            })
            .unwrap_err();
        assert_eq!(err.to_string(), fixed.to_string());
        assert!(
            !err.to_string().contains("no-such"),
            "the target is not echoed"
        );
    }

    /// Positive (P9): a canary planted in a scratch copy (a phrase in the hub's doc string, a
    /// word in its snippet) is counted under its rule id and its bytes appear nowhere in the
    /// report.
    #[test]
    fn denylist_fp_rate_counts_a_planted_canary_by_rule_id() {
        let scratch = Scratch::new("fp-plant");
        let (snapshot, db, keys) = corpus(&scratch);
        let (hub, path) = hub_and_path(&session(&db, &keys, &snapshot));
        let plant = scratch.0.join("plant").join(snapshot.file_name().unwrap());
        std::fs::create_dir_all(plant.parent().unwrap()).unwrap();
        std::fs::copy(&snapshot, &plant).unwrap();
        std::fs::set_permissions(&plant, std::fs::Permissions::from_mode(0o600)).unwrap();
        {
            let conn = rusqlite::Connection::open(&plant).unwrap();
            conn.execute(
                "UPDATE symbols SET doc = 'see Project Nightjar for the plan' WHERE ref = ?1",
                [&hub],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO snippets(symbol_id, text) \
                 SELECT id, 'pw := \"hunter2\"' FROM symbols WHERE ref = ?1",
                [&hub],
            )
            .unwrap();
        }
        let planted = session(&db, &keys, &plant);
        let resolved = planted.provider.resolve("bench", &planted.head).unwrap();
        let report =
            denylist_fp_rate(&examples_policy(), resolved.snapshot.as_ref(), &[path]).unwrap();
        assert_eq!(report["hits_by_rule"]["codename-nightjar"], 1);
        assert_eq!(report["hits_by_rule"]["old-password"], 1);
        assert_eq!(report["strings_hit"]["docs"], 1);
        assert_eq!(report["strings_hit"]["snippets"], 1);
        assert_eq!(report["scanned"]["docs"], 1);
        assert_eq!(report["scanned"]["snippets"], 1);
        assert_eq!(report["files"], 1);
        let text = report.to_string();
        assert!(
            !text.contains("Nightjar") && !text.contains("hunter2"),
            "counts only: {text}"
        );
        assert!(
            !text.contains("start") && !text.contains("offset"),
            "no offsets: {text}"
        );
    }

    /// Negative (P9): the clean corpus yields zero hits and a zero rate over every symbol name
    /// of the hub's file (a corpus has no docs or snippets).
    #[test]
    fn denylist_fp_rate_is_zero_on_the_clean_corpus() {
        let scratch = Scratch::new("fp-clean");
        let (snapshot, db, keys) = corpus(&scratch);
        let session = session(&db, &keys, &snapshot);
        let (_, path) = hub_and_path(&session);
        let resolved = session.provider.resolve("bench", &session.head).unwrap();
        let report =
            denylist_fp_rate(&examples_policy(), resolved.snapshot.as_ref(), &[path]).unwrap();
        assert_eq!(report["hits_by_rule"], json!({}));
        assert_eq!(report["strings_hit"]["total"], 0);
        assert_eq!(report["too_long"], 0);
        assert_eq!(report["rate"], 0.0);
        let names = report["scanned"]["names"].as_u64().unwrap();
        assert!(names >= 1, "the file's symbol names were scanned ({names})");
        assert_eq!(report["scanned"]["total"], names);
        assert_eq!(report["nodes"], names);
    }
}
