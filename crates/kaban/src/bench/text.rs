//! Reproduce the S4 names-only workload on a freshly populated store.
//! `kaban bench text --cases s4 --corpus symbols.jsonl --out text.jsonl`
//! takes the non-local S4 export in (path, ref) order. The checked query fixture
//! supplies the semantic oracle. Warm calls time the store. Fresh processes time
//! the pinned text index and the whole writable store independently.

use std::path::PathBuf;
use std::process::ExitCode;

/// S4 inputs and output.
#[derive(clap::Args)]
pub(crate) struct Args {
    /// Benchmark workload
    #[arg(long, default_value = "s4", value_parser = ["s4"])]
    cases: String,
    /// S4 non-local symbol export (JSONL)
    #[arg(long, default_value = "symbols.jsonl")]
    corpus: PathBuf,
    /// Write parity, per-query results, latency samples and gates here
    #[arg(long, default_value = "text.jsonl")]
    out: PathBuf,
    /// Directory of the prepared store for a fresh-process observation
    #[arg(long, hide = true)]
    first_child: Option<PathBuf>,
    /// Pinned tier manifest for a fresh index-only observation
    #[arg(long, hide = true, conflicts_with = "first_child")]
    first_index_child: Option<PathBuf>,
}

/// Run the S4 workload, retaining measured misses of its targets.
#[cfg(feature = "bench")]
pub(crate) fn run(args: &Args) -> ExitCode {
    imp::run(args).unwrap_or_else(|error| {
        crate::cli::output::print_failure(&format!("{error:#}"));
        ExitCode::FAILURE
    })
}

/// Without the bench feature, retain the normal feature refusal.
#[cfg(not(feature = "bench"))]
pub(crate) fn run(args: &Args) -> ExitCode {
    let _ = (
        &args.cases,
        &args.corpus,
        &args.out,
        &args.first_child,
        &args.first_index_child,
    );
    crate::cli::stubs::not_available("bench text", "feature bench")
}

#[cfg(feature = "bench")]
mod imp {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::io::{BufWriter, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail, ensure};
    use kaban_core::config::Recall;
    use kaban_core::query_budget::QueryBudget;
    use kaban_core::text_index::{
        ExtentSpan, MatchMode, Searched, SeedFilter, TextHit, TextIndex, TextQuery, TextSnapshot,
    };
    use kaban_core::{
        Actor, IngestStore, NamespaceSet, NewNode, NodeId, OntologyFile, Op, SchemaStore, TypeDef,
        WorkspaceId, WriteBatch, WriteCtx,
    };
    use kaban_index_tantivy::TantivyIndex;
    use kaban_store_sqlite::{SqliteStore, catalog_testing};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    use super::Args;
    use crate::bench::report::{machine_profile, pct};

    const CORPUS_SHA: &str = "75e9a6e5840439c27bba7c1e081384cae0b617e21382b301337f30f59fc2badb";
    const CASE_SHA: &str = "6bc2a7310f56cec25f13fbeeb5a9aabc8b63e231013d3a0f9d46a01f73f64ec0";
    const CASES: &str =
        include_str!("../../../kaban-index-tantivy/tests/fixtures/s4-parity-queries.jsonl");
    const WARM_RUNS: usize = 1000;
    const FRESH_RUNS: usize = 20;

    #[derive(Deserialize)]
    struct Symbol {
        rid: i64,
        kind: String,
        name: String,
        tail: String,
        label: String,
        path: String,
        line: Option<i64>,
        #[serde(rename = "ref")]
        reference: String,
        external: u8,
    }

    fn lookup(rows: &[Symbol], rid: i64) -> Result<&Symbol> {
        let idx = usize::try_from(rid)?.checked_sub(1).context("zero rid")?;
        rows.get(idx).context("hit outside S4 store")
    }

    fn load(path: &Path) -> Result<Vec<Symbol>> {
        let input = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        ensure!(
            kaban_policy::sha256_hex(input.as_bytes()) == CORPUS_SHA,
            "S4 corpus SHA-256 mismatch"
        );
        ensure!(
            kaban_policy::sha256_hex(CASES.as_bytes()) == CASE_SHA,
            "S4 query fixture SHA-256 mismatch"
        );
        let rows = input
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Symbol>, _>>()?;
        for (i, row) in rows.iter().enumerate() {
            ensure!(usize::try_from(row.rid)? == i + 1, "nonsequential S4 rid");
        }
        Ok(rows)
    }

    fn open(dir: &Path) -> Result<SqliteStore> {
        Ok(SqliteStore::open(&dir.join("kaban.db"), &dir.join("keys"))?)
    }

    /// Project the two S4 code fields through the store's ordinary ontology,
    /// stage its nodes in source order and compact their pending packs.
    fn prepare(dir: &Path, rows: &[Symbol]) -> Result<SqliteStore> {
        let store = open(dir)?;
        let mut kinds = BTreeMap::new();
        for row in rows {
            kinds.entry(&row.label).or_insert_with(|| {
                json!({
                    "description":"S4 code symbol", "ident_fields":["props.name","props.tail"],
                    "props_schema":{"type":"object","properties":{
                        "name":{"type":"string"},"tail":{"type":"string"}
                    }}
                })
            });
        }
        let mut files = kaban_core::shipped::files()?;
        files.push(serde_json::from_value::<OntologyFile>(json!({
            "ontology":"s4", "version":1, "kinds":kinds, "rels":{}
        }))?);
        let ontology = kaban_core::resolve(&files, &NamespaceSet::new())?;
        for kind in ontology
            .kinds
            .values()
            .filter(|kind| kind.id.ns.as_str() == "s4")
        {
            store.schema_extend(TypeDef {
                kind: "label".into(),
                name: kaban_core::stored_name(&kind.id),
                ns: None,
                description: kind.description.clone(),
                from_kinds: vec![],
                to_kinds: vec![],
                directed: true,
                props_schema: Some(kind.props_schema.as_value().clone()),
                builtin: false,
                declaration: kaban_core::TypeDeclaration::of(kind),
            })?;
        }
        let store = store.with_ontology(Arc::new(ontology))?;
        for group in rows.chunk_by(|a, b| a.path == b.path) {
            let path = &group[0].path;
            let ws = WorkspaceId::new(if path.is_empty() { "-" } else { path.as_str() });
            let ctx = WriteCtx::new(ws.clone(), Actor::Agent);
            for chunk in group.chunks(1024) {
                let mut ops = Vec::with_capacity(chunk.len());
                for row in chunk {
                    ops.push(Op::UpsertNode {
                        id: NodeId::parse(&format!("00000000-0000-7000-8000-{:012x}", row.rid))?,
                        node: NewNode {
                            kind: format!("s4:{}", row.label),
                            props: Some(json!({"name":row.name, "tail":row.tail})),
                            ..Default::default()
                        },
                    });
                }
                let report = store.apply_batch(WriteBatch::new(ws.clone(), ops), &ctx)?;
                ensure!(
                    report.inserted as usize == chunk.len(),
                    "incomplete S4 import"
                );
            }
        }
        let _maintenance = store.compact()?;
        // A new store inserted in export order must preserve the source rid lookup.
        let ids = kaban_store_sqlite::catalog_testing::present_local_ids(&store)?;
        ensure!(
            ids.len() == rows.len() && ids.iter().zip(rows).all(|(id, r)| i64::from(id.0) == r.rid),
            "store/source rid mismatch"
        );
        Ok(store)
    }

    fn query<'a>(
        text: &'a str,
        mode: MatchMode,
        kinds: &'a [String],
        path: Option<&'a str>,
        limit: usize,
    ) -> TextQuery<'a> {
        TextQuery {
            text,
            mode,
            filter: SeedFilter {
                kinds,
                tags: &[],
                workspace_id: path,
                code_ref: None,
            },
            limit,
            sufficient: 0,
            skip: 0,
        }
    }

    fn search(store: &SqliteStore, q: &TextQuery<'_>) -> Result<Vec<TextHit>> {
        match store.search_text(q)? {
            Searched::Hits { hits, .. } => Ok(hits),
            Searched::Unavailable => bail!("text search unavailable for {}", q.text),
        }
    }

    fn exact_parity(
        store: &SqliteStore,
        rows: &[Symbol],
        name: &str,
        sel: &Value,
        oracle: &Value,
    ) -> Result<(bool, Value)> {
        // The source export is in selector order (path, ref). Preserve the order of
        // Exact hits so an engine ordering regression is visible for an ambiguous name.
        let q = query(name, MatchMode::Exact, &[], sel["path"].as_str(), 5000);
        let hits = search(store, &q)?;
        ensure!(
            hits.len() < q.limit,
            "incomplete exact candidate page for {name}"
        );
        let mut candidates = hits
            .iter()
            .map(|h| lookup(rows, h.rid))
            .collect::<Result<Vec<_>>>()?;
        candidates.retain(|r| r.name == name);
        let same_label = candidates.iter().any(|r| sel["label"] == r.label);
        candidates.retain(|r| {
            if same_label {
                sel["label"] == r.label
            } else {
                r.external == 0
            }
        });
        let first: Vec<_> = candidates
            .iter()
            .take(3)
            .map(|r| (&r.path, r.line))
            .collect();
        let reference = candidates.first().map(|r| &r.reference);
        let rids: Vec<_> = candidates.iter().map(|r| r.rid).collect();
        let passed = match oracle["status"].as_str() {
            Some("ok") => candidates.len() == 1 && json!(reference) == oracle["ref"],
            Some("error") => {
                oracle["candidates_total"]
                    .as_u64()
                    .or_else(|| oracle["candidates_shown"].as_u64())
                    == Some(candidates.len() as u64)
                    && json!(first) == oracle["first_candidates"]
            }
            _ => bail!("unknown exact oracle for {name}"),
        };
        Ok((
            passed,
            json!({"count":candidates.len(),"first_candidates":first,"first_ref":reference,"candidate_rids":rids}),
        ))
    }

    fn top(store: &SqliteStore, rows: &[Symbol], q: &TextQuery<'_>) -> Result<Vec<Value>> {
        search(store, q)?
            .iter()
            .map(|h| {
                let r = lookup(rows, h.rid)?;
                Ok(json!({"label":r.label,"name":r.name,"path":r.path,"rid":r.rid}))
            })
            .collect()
    }

    fn required_fragment(q: usize, ranked: &[Value]) -> Option<(Value, bool)> {
        match q {
            40 => Some((
                json!({"includes_rid":27_294}),
                ranked.iter().any(|r| r["rid"] == 27_294),
            )),
            // The second Method supplies both parts via its Snapshot descriptor.
            43 => Some((
                json!({"first_rid":27_501, "rids":[16_402,27_501]}),
                matches!(ranked, [first, second] if first["rid"] == 27_501 && second["rid"] == 16_402),
            )),
            _ => None,
        }
    }

    fn parity(
        store: &SqliteStore,
        rows: &[Symbol],
        phase: &str,
        out: &mut impl Write,
    ) -> Result<(bool, Vec<Value>)> {
        let mut answers = Vec::new();
        let (mut exact, mut fragments, mut records, mut exact_pass) = (0, 0, 0, 0);
        let mut required_pass = true;
        for (i, line) in CASES.lines().enumerate() {
            let case: Value = serde_json::from_str(line)?;
            let sel = &case["selector"];
            let oracle = &case["kq_output_summary"];
            let mut result = json!({"op":"parity","q":i,"selector":sel});
            if let Some(name) = sel["name"].as_str() {
                if case["category"] == "fragment" && i != 38 {
                    fragments += 1;
                    let kinds = [format!(
                        "s4:{}",
                        sel["label"].as_str().context("fragment label")?
                    )];
                    let ranked = top(
                        store,
                        rows,
                        &query(name, MatchMode::Lexical, &kinds, None, 5),
                    )?;
                    let fixed = required_fragment(i, &ranked).map(|(expected, passed)| {
                        result["expected"] = expected;
                        passed
                    });
                    required_pass &= fixed.unwrap_or(true);
                    result["class"] = json!("fragment");
                    result["label_lexical"] = json!(ranked);
                    result["any_lexical"] = json!(top(
                        store,
                        rows,
                        &query(name, MatchMode::Lexical, &[], None, 5)
                    )?);
                    result["label_fuzzy"] = json!(top(
                        store,
                        rows,
                        &query(name, MatchMode::Fuzzy, &kinds, None, 5)
                    )?);
                    result["fts5_oracle"] = case["fts5_oracle"].clone();
                    result["s4_judgment"] = json!(match i {
                        40 => "weak: lexical prefix missed configure_connection",
                        42 =>
                            "partial: fuzzy found snapshot_symbols; its Method descriptor lacks CodeGraphReader",
                        43 => "weak: partial names outranked snapshot_symbols",
                        45 => "partial: irrelevant names in the tail",
                        _ => "good",
                    });
                    result["required_fix_pass"] = json!(fixed);
                } else {
                    exact += 1;
                    let (passed, actual) = exact_parity(store, rows, name, sel, oracle)?;
                    exact_pass += usize::from(passed);
                    result["class"] = json!("exact");
                    result["expected"] = oracle.clone();
                    result["actual"] = actual;
                    result["pass"] = json!(passed);
                }
            } else {
                records += 1;
                result["class"] = json!("record-key");
                result["status"] = json!("outside text workload");
            }
            writeln!(out, "{}", json!({"phase":phase, "case":result}))?;
            answers.push(result);
        }
        ensure!(
            (exact, fragments, records) == (36, 10, 4),
            "wrong S4 query classes"
        );
        writeln!(
            out,
            "{}",
            json!({"op":"parity_summary","phase":phase,"exact_pass":exact_pass,"exact_total":exact,
            "hand_judged_fragments":fragments,"record_lookups":records,"required_fixes_pass":required_pass})
        )?;
        Ok((exact_pass == exact && required_pass, answers))
    }

    // Rotations, limit and sample count are those of the original S4 names-only run.
    const EXACT_NAMES: &[&str] = &[
        "SqliteStore",
        "StoreDrain",
        "FsIdentity",
        "configure_connection",
        "CodeGraphReader",
        "GraphStore",
        "PublishError",
        "call_tool",
        "tool_router",
        "MAX_PROPS_BYTES",
        "snapshot_symbols",
        "by_name",
    ];
    const PREFIXES: &[&str] = &[
        "sqlite*", "snap*", "config*", "code*", "graph*", "tool*", "store*", "ident*",
    ];
    const FRAGMENTS: &[&str] = &[
        "snapshot symbols",
        "code graph",
        "configure connection",
        "tool router",
        "by name",
        "graph store",
        "publish error",
        "max props",
    ];
    const FUZZY: &[&str] = &[
        "sqlitestoer",
        "configur_connection",
        "snapshotsymbls",
        "codegraphreder",
        "graphstor",
        "toolrouter",
    ];

    /// Case-insensitive name term, in export (rid) order, capped like the timed call.
    fn exact_rids(rows: &[Symbol], text: &str) -> Vec<i64> {
        rows.iter()
            .filter(|r| r.name.eq_ignore_ascii_case(text))
            .take(10)
            .map(|r| r.rid)
            .collect()
    }

    /// Validate the six frozen S4 probes from their independently identified
    /// source rows. Both capped probes have a single eligible whole surface token.
    fn fuzzy_oracle(rows: &[Symbol], text: &str, rids: &[i64]) -> Result<(Value, bool)> {
        let capped = match text {
            "sqlitestoer" => Some(("sqlitestore", 239)),
            "codegraphreder" => Some(("codegraphreader", 37)),
            _ => None,
        };
        if let Some((token, population)) = capped {
            let eligible: BTreeSet<_> = rows
                .iter()
                .filter(|row| {
                    [&row.name, &row.tail].into_iter().any(|field| {
                        field
                            .split(|c: char| !c.is_ascii_alphanumeric())
                            .any(|word| word.eq_ignore_ascii_case(token))
                    })
                })
                .map(|row| row.rid)
                .collect();
            let returned: BTreeSet<_> = rids.iter().copied().collect();
            let valid = eligible.len() == population
                && rids.len() == 10
                && returned.len() == 10
                && returned.is_subset(&eligible);
            return Ok((
                json!({"whole_token":token,"population":population,
                "eligible_rids":eligible,"distinct_hits":10}),
                valid,
            ));
        }
        let expected: &[i64] = match text {
            "configur_connection" => &[27_294],
            "snapshotsymbls" => &[],
            "graphstor" => &[19_937],
            "toolrouter" => &[7_845, 7_846, 7_847, 7_848],
            _ => bail!("missing fuzzy oracle for {text}"),
        };
        let valid = rids.len() == expected.len() && expected.iter().all(|rid| rids.contains(rid));
        Ok((json!({"rids":expected}), valid))
    }

    fn warm(store: &SqliteStore, rows: &[Symbol], out: &mut impl Write) -> Result<bool> {
        let mut all_pass = true;
        for (class, mode, texts) in [
            ("exact", MatchMode::Exact, EXACT_NAMES),
            ("fragment", MatchMode::Lexical, FRAGMENTS),
            ("prefix", MatchMode::Lexical, PREFIXES),
            ("fuzzy", MatchMode::Fuzzy, FUZZY),
        ] {
            let queries: Vec<_> = texts
                .iter()
                .map(|text| query(text, mode, &[], None, 10))
                .collect();
            let mut warmup = Vec::new();
            let mut results_valid = true;
            for q in &queries {
                let hits = search(store, q)?;
                let rids: Vec<_> = hits.iter().map(|h| h.rid).collect();
                let names: Vec<_> = rids
                    .iter()
                    .map(|r| Ok(&lookup(rows, *r)?.name))
                    .collect::<Result<_>>()?;
                let (expected, valid) = match class {
                    "exact" => {
                        let expected = exact_rids(rows, q.text);
                        (
                            json!({"rids":expected}),
                            !expected.is_empty() && expected == rids,
                        )
                    }
                    "prefix" => (json!({"hits":10}), hits.len() == 10),
                    "fuzzy" => fuzzy_oracle(rows, q.text, &rids)?,
                    _ => (json!({"min_hits":1}), !hits.is_empty()),
                };
                results_valid &= valid;
                writeln!(
                    out,
                    "{}",
                    json!({"op":"warmup","class":class,"query":q.text,"limit":10,
                    "expected":expected,"actual_rids":rids,"actual_names":names,"pass":valid})
                )?;
                warmup.push(rids);
            }
            let mut samples = Vec::with_capacity(WARM_RUNS);
            for index in (0..WARM_RUNS).map(|i| i % queries.len()) {
                let began = Instant::now();
                let hits = search(store, &queries[index])?;
                samples.push(began.elapsed().as_secs_f64() * 1e6);
                results_valid &= hits.iter().map(|h| h.rid).eq(warmup[index].iter().copied());
            }
            let mut sorted = samples.clone();
            let p50 = pct(&mut sorted, 0.50);
            let p99 = pct(&mut sorted, 0.99);
            let informational = class == "exact";
            let (observed, target_us, original_target_us) = if informational {
                (p50, 10, 10)
            } else {
                (p99, 170, 150)
            };
            let target_met = observed <= f64::from(target_us);
            all_pass &= results_valid && (informational || target_met);
            writeln!(
                out,
                "{}",
                json!({"op":"warm","class":class,"queries":texts,"order":"round robin",
                "limit":10,"clock":"whole SqliteStore::search_text, including catalog pin and TextIndex::search",
                "unit":"us","n":samples.len(),"samples_us":samples,"p50_us":p50,"p99_us":p99,
                "results_valid":results_valid,"target_us":target_us,"original_target_us":original_target_us,
                "original_target_met":observed <= f64::from(original_target_us),
                "metric":if informational {"p50"} else {"p99"},"informational":informational,"target_met":target_met})
            )?;
        }
        Ok(all_pass)
    }

    #[derive(Deserialize, Serialize)]
    struct First {
        #[serde(skip_serializing_if = "Option::is_none")]
        open_us: Option<f64>,
        open_plus_first_us: f64,
        rids: Vec<i64>,
    }

    fn first_child(dir: &Path) -> Result<ExitCode> {
        ensure!(
            dir.join("kaban.db").is_file(),
            "fresh-process S4 store is missing"
        );
        let q = query("SqliteStore", MatchMode::Exact, &[], None, 10);
        let began = Instant::now();
        let store = open(dir)?;
        let open_us = began.elapsed().as_secs_f64() * 1e6;
        let hits = search(&store, &q)?;
        let open_plus_first_us = began.elapsed().as_secs_f64() * 1e6;
        print_first(Some(open_us), open_plus_first_us, &hits)
    }

    fn print_first(
        open_us: Option<f64>,
        open_plus_first_us: f64,
        hits: &[TextHit],
    ) -> Result<ExitCode> {
        let result = First {
            open_us,
            open_plus_first_us,
            rids: hits.iter().map(|h| h.rid).collect(),
        };
        crate::cli::output::print_json(&result, crate::cli::output::Format::Compact)?;
        Ok(ExitCode::SUCCESS)
    }

    /// Only sealed KBX ranges cross the process boundary. The parent's catalog
    /// pin protects them until the last child has exited.
    #[derive(Deserialize, Serialize)]
    struct PinnedIndex {
        kbx: PathBuf,
        generation: u64,
        tiers: Vec<(u64, u64)>,
    }

    fn first_index_child(manifest: &Path) -> Result<ExitCode> {
        let pinned: PinnedIndex = serde_json::from_slice(&fs::read(manifest)?)?;
        let tiers: Vec<_> = pinned
            .tiers
            .iter()
            .map(|&(offset, len)| ExtentSpan { offset, len })
            .collect();
        let snapshot = TextSnapshot {
            generation: pinned.generation,
            kbx: &pinned.kbx,
            tiers: &tiers,
            sediment: &[],
        };
        let q = query("SqliteStore", MatchMode::Exact, &[], None, 10);
        let began = Instant::now();
        let ontology = kaban_core::shipped::resolved()?;
        let index = TantivyIndex::new(&ontology, &Recall::default());
        let budget = QueryBudget::new(256 << 20, Some(Duration::from_secs(30)));
        let found = index.search(&snapshot, &q, &budget)?;
        let elapsed = began.elapsed().as_secs_f64() * 1e6;
        let Searched::Hits { hits, .. } = found else {
            bail!("fresh text index unavailable")
        };
        print_first(None, elapsed, &hits)
    }

    fn fresh_index(
        at: &TextSnapshot<'_>,
        dir: &Path,
        expected: &[i64],
        out: &mut impl Write,
    ) -> Result<bool> {
        ensure!(
            at.sediment.is_empty() && !at.tiers.is_empty(),
            "expected sealed S4 text index"
        );
        let pinned = PinnedIndex {
            kbx: at.kbx.to_path_buf(),
            generation: at.generation,
            tiers: at
                .tiers
                .iter()
                .map(|span| (span.offset, span.len))
                .collect(),
        };
        let manifest = dir.join("pinned-text.json");
        fs::write(&manifest, serde_json::to_vec(&pinned)?)?;
        fresh(&manifest, Some(&pinned), expected, out)
    }

    fn fresh(
        input: &Path,
        index: Option<&PinnedIndex>,
        expected: &[i64],
        out: &mut impl Write,
    ) -> Result<bool> {
        let mut valid = expected.len() == 8;
        let mut samples = Vec::with_capacity(FRESH_RUNS);
        let args = [
            "bench".as_ref(),
            "text".as_ref(),
            if index.is_some() {
                "--first-index-child"
            } else {
                "--first-child"
            }
            .as_ref(),
            input.as_os_str(),
        ];
        for _ in 0..FRESH_RUNS {
            let child = crate::bench::run_self(&args, &[], Duration::from_secs(60))?;
            ensure!(
                child.killed.is_none() && child.status.is_some_and(|s| s.success()),
                "fresh child failed: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            let result: First =
                serde_json::from_slice(&child.stdout).context("fresh child result")?;
            valid &= result.rids == expected;
            samples.push(result);
        }
        let mut times: Vec<_> = samples.iter().map(|r| r.open_plus_first_us).collect();
        times.sort_by(f64::total_cmp);
        let median = f64::midpoint(times[(times.len() - 1) / 2], times[times.len() / 2]);
        let p99 = pct(&mut times, 0.99);
        let mut record = json!({"query":"SqliteStore","mode":"exact","limit":10,
            "unit":"us","n":samples.len(),"samples":samples,"expected_rids":expected,"results_valid":valid,
            "min_us":times.first(),"median_us":median,"p99_us":p99,"max_us":times.last(),"metric":"median",
            "environment":"new process per sample; process launch, CLI dispatch and output excluded; OS page cache retained"});
        let passed = if let Some(pinned) = index {
            record["op"] = json!("fresh_index");
            record["clock"] = json!(
                "from resolving the shipped ontology through production TantivyIndex::search return, including adapter and budget setup, KBX mapping, reader open and first exact query; parent catalog pin and child manifest read excluded"
            );
            record["generation"] = json!(pinned.generation);
            record["tiers"] = json!(pinned.tiers);
            record["informational"] = json!(false);
            record["target_us"] = json!(1000);
            record["target_met"] = json!(median <= 1000.0);
            valid && median <= 1000.0
        } else {
            record["op"] = json!("fresh");
            record["clock"] =
                json!("before writable SqliteStore::open through first search_text return");
            record["informational"] = json!(true);
            valid
        };
        writeln!(out, "{record}")?;
        Ok(passed)
    }

    fn measure(dir: &Path, rows: &[Symbol], out: &mut impl Write) -> Result<bool> {
        writeln!(
            out,
            "{}",
            json!({"op":"corpus","cases":"s4","sha256":CORPUS_SHA,"queries_sha256":CASE_SHA,
            "source_commit":"c15cd8805eff75509575deb8da8e7229f79efd99","rows":rows.len(),
            "files":rows.iter().filter(|r| r.kind == "file").count(),"symbols":rows.iter().filter(|r| r.kind == "symbol").count(),
            "external":rows.iter().filter(|r| r.external != 0).count(),"profile":"names and descriptor tails","machine":machine_profile()})
        )?;
        let store = prepare(dir, rows)?;
        let (before_ok, before) = parity(&store, rows, "before_reindex", out)?;
        ensure!(
            store.reindex_text()? == rows.len() as u64,
            "incomplete S4 rebuild"
        );
        let (after_ok, after) = parity(&store, rows, "after_reindex", out)?;
        let changed: Vec<_> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .collect();
        let parity_ok = before_ok && after_ok && changed.is_empty();
        writeln!(
            out,
            "{}",
            json!({"op":"reindex_parity","cases":before.len(),"changed_queries":changed,
            "pass":parity_ok})
        )?;
        let warm_ok = warm(&store, rows, out)?;
        let expected = exact_rids(rows, "SqliteStore");
        let budget = QueryBudget::new(256 << 20, Some(Duration::from_secs(30)));
        let fresh_ok = catalog_testing::with_main_text(&store, &budget, |at| {
            Ok(fresh_index(at, dir, &expected, out))
        })??;
        drop(store);
        let store_valid = fresh(dir, None, &expected, out)?;
        let pass = parity_ok && warm_ok && fresh_ok && store_valid;
        writeln!(
            out,
            "{}",
            json!({"op":"summary","parity_pass":parity_ok,"warm_pass":warm_ok,"fresh_pass":fresh_ok,"fresh_store_results_valid":store_valid,
            "pass":pass})
        )?;
        Ok(pass)
    }

    pub(super) fn run(args: &Args) -> Result<ExitCode> {
        if let Some(manifest) = &args.first_index_child {
            return first_index_child(manifest);
        }
        if let Some(dir) = &args.first_child {
            return first_child(dir);
        }
        let rows = load(&args.corpus)?;
        if let Ok(output) = fs::metadata(&args.out) {
            let input = fs::metadata(&args.corpus)?;
            ensure!(
                (output.dev(), output.ino()) != (input.dev(), input.ino()),
                "output aliases corpus"
            );
        }
        let mut out = BufWriter::new(fs::File::create(&args.out)?);
        let dir = std::env::temp_dir().join(format!("kaban-text-{}", NodeId::new()));
        fs::create_dir(&dir)?;
        let result = measure(&dir, &rows, &mut out);
        fs::remove_dir_all(&dir)?;
        out.flush()?;
        ensure!(
            result?,
            "S4 correctness or latency target missed; see {}",
            args.out.display()
        );
        Ok(ExitCode::SUCCESS)
    }
}
