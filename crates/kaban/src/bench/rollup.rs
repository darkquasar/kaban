//! Synthetic rollup lookup, hot-group edit and independent recount measurements (spec 11).

use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub(crate) struct Args {
    /// Active rows to generate in a fresh project
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..))]
    rows: u32,
    /// Where to write the measurement as JSONL
    #[arg(long, default_value = "rollup.jsonl")]
    out: PathBuf,
}

/// Without the benchmark feature, refuse with the same exit as the other catalog benchmarks.
#[cfg(not(feature = "bench"))]
pub(crate) fn run(args: &Args) -> ExitCode {
    let _ = (args.rows, &args.out);
    crate::cli::stubs::not_available("bench rollup", "feature bench")
}

#[cfg(feature = "bench")]
pub(crate) fn run(args: &Args) -> ExitCode {
    imp::run(args).unwrap_or_else(|err| {
        crate::cli::output::print_failure(&format!("{err:#}"));
        ExitCode::FAILURE
    })
}

#[cfg(feature = "bench")]
mod imp {
    use super::Args;
    use crate::bench::report::{machine_profile, pct, raw_series};
    use anyhow::{Result, ensure};
    use kaban_core::{
        Actor, IngestStore, MemoryStore, NewNode, NodeId, Op, SchemaStore, SqlValue, WorkspaceId,
        WriteBatch, WriteCtx,
    };
    use kaban_store_sqlite::{SqliteStore, catalog_testing::recount_rollup_bench};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::Instant;

    const BATCH: u32 = 1024;
    const READS: usize = 101;
    const EDITS: u32 = 31;

    pub(super) fn run(args: &Args) -> Result<ExitCode> {
        let dir = std::env::temp_dir().join(format!("kaban-rollup-{}", NodeId::new()));
        fs::create_dir(&dir)?;
        let result = measure(&dir, args.rows);
        fs::remove_dir_all(&dir)?;
        let (line, valid) = result?;
        writeln!(fs::File::create(&args.out)?, "{line}")?;
        ensure!(
            valid,
            "rollup results disagree with the generated corpus or independent recount"
        );
        Ok(ExitCode::SUCCESS)
    }

    fn bucket(row: u32) -> u32 {
        if row.is_multiple_of(2) {
            0
        } else {
            1 + (row / 2) % 32
        }
    }

    fn upsert(row: u32, cost: f64) -> Result<Op> {
        Ok(Op::UpsertNode {
            id: NodeId::parse(&format!("00000000-0000-7000-8000-{row:012x}"))?,
            node: NewNode {
                kind: "RollupRecord".into(),
                text: format!("Rollup row {row}"),
                props: Some(json!({"bucket":bucket(row),"cost":cost})),
                ..NewNode::default()
            },
        })
    }

    fn write(store: &SqliteStore, ctx: &WriteCtx, ops: Vec<Op>) -> Result<()> {
        store.apply_batch(WriteBatch::new(WorkspaceId::default(), ops), ctx)?;
        Ok(())
    }

    fn summary(samples: &[f64]) -> Value {
        let mut series = raw_series(samples);
        series["p99_us"] = json!(pct(&mut samples.to_vec(), 0.99));
        series
    }

    /// Apparent file lengths and allocated filesystem blocks while both database handles are live.
    fn footprint(root: &Path) -> Result<(u64, u64, BTreeMap<String, Value>)> {
        let mut dirs = vec![root.to_owned()];
        let mut files = BTreeMap::new();
        let (mut logical, mut allocated) = (0, 0);
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    dirs.push(path);
                } else if meta.is_file() {
                    let bytes = meta.len();
                    let blocks = meta.blocks() * 512;
                    logical += bytes;
                    allocated += blocks;
                    files.insert(
                        path.strip_prefix(root)?.display().to_string(),
                        json!({"logical_bytes":bytes,"allocated_bytes":blocks}),
                    );
                }
            }
        }
        Ok((logical, allocated, files))
    }

    /// A full recount reads the underlying properties independently from the saved rollups.
    fn recount(
        reader: &SqliteStore,
        writer: &SqliteStore,
        rows: u32,
    ) -> Result<(Value, bool, f64)> {
        let start = Instant::now();
        let totals = recount_rollup_bench(reader)?;
        let micros = start.elapsed().as_secs_f64() * 1e6;
        let hot_rows = i64::from(rows.div_ceil(2));
        let mut valid = totals.iter().map(|(_, n, _)| *n).sum::<i64>() == i64::from(rows)
            && totals.len() == usize::try_from(1 + (rows / 2).min(32))?
            && totals
                .first()
                .is_some_and(|(bucket, n, _)| *bucket == 0 && *n == hot_rows);
        for &(group, count, cost) in &totals {
            let key = [SqlValue::Integer(group)];
            valid &= writer.rollup("row_count", &key)? == SqlValue::Integer(count);
            valid &= writer.rollup("total_cost", &key)? == SqlValue::Real(cost);
        }
        let hot_cost = totals.first().map_or(0.0, |(_, _, v)| *v);
        Ok((
            json!({"elapsed_us":micros,"columns":["bucket","count","sum_cost"],"groups":totals}),
            valid,
            hot_cost,
        ))
    }

    fn measure(dir: &Path, rows: u32) -> Result<(Value, bool)> {
        let db = dir.join("kaban.db");
        let keys = dir.join("keys");
        let writer = SqliteStore::open(&db, &keys)?;
        let reader = SqliteStore::open(&db, &keys)?;
        writer.schema_extend(serde_json::from_value(json!({
            "kind":"label","name":"RollupRecord","description":"Synthetic rollup record",
            "columns":[{"name":"bucket","ty":"integer"},{"name":"cost","ty":"real"}],
            "rollups":[{"name":"row_count","label":"RollupRecord","group_by":["bucket"],"op":"count"},
                {"name":"total_cost","label":"RollupRecord","group_by":["bucket"],"op":{"sum":"cost"}}]
        }))?)?;
        let ctx = WriteCtx::new(WorkspaceId::default(), Actor::Agent);
        let setup = Instant::now();
        let (mut peak_logical, mut peak_allocated) = (0, 0);
        let mut setup_wal_high_water = 0;
        let mut setup_checkpoints = 0u32;
        for start in (0..rows).step_by(BATCH as usize) {
            let batch = (start..start.saturating_add(BATCH).min(rows))
                .map(|i| upsert(i, 1.25 + f64::from(i % 8) / 4.0))
                .collect::<Result<Vec<_>>>()?;
            write(&writer, &ctx, batch)?;
            let (logical, allocated, _) = footprint(dir)?;
            peak_logical = peak_logical.max(logical);
            peak_allocated = peak_allocated.max(allocated);
            setup_wal_high_water =
                setup_wal_high_water.max(fs::metadata(dir.join("kaban.db-wal"))?.len());
            if writer.checkpoint_if_due()?.is_some() {
                setup_checkpoints += 1;
            }
        }
        let setup_us = setup.elapsed().as_secs_f64() * 1e6;
        let (before, mut valid, initial_cost) = recount(&reader, &writer, rows)?;
        let hot = [SqlValue::Integer(0)];
        let hot_rows = rows.div_ceil(2);
        let mut count_us = Vec::new();
        let mut sum_us = Vec::new();
        for _ in 0..READS {
            let now = Instant::now();
            let count = writer.rollup("row_count", &hot)?;
            count_us.push(now.elapsed().as_secs_f64() * 1e6);
            let now = Instant::now();
            let sum = writer.rollup("total_cost", &hot)?;
            sum_us.push(now.elapsed().as_secs_f64() * 1e6);
            valid &= count == SqlValue::Integer(i64::from(hot_rows))
                && sum == SqlValue::Real(initial_cost);
        }
        let mut edit_us = Vec::new();
        for i in 0..EDITS {
            let cost = if i.is_multiple_of(2) { 2.5 } else { 1.25 };
            let batch = WriteBatch::new(WorkspaceId::default(), vec![upsert(0, cost)?]);
            let now = Instant::now();
            writer.apply_batch(batch, &ctx)?;
            edit_us.push(now.elapsed().as_secs_f64() * 1e6);
            valid &=
                reader.rollup("total_cost", &hot)? == SqlValue::Real(initial_cost - 1.25 + cost);
        }
        let (after, agreed, _) = recount(&reader, &writer, rows)?;
        valid &= agreed;
        let (logical, allocated, files) = footprint(dir)?;
        let target_met = count_us.iter().chain(&sum_us).all(|us| *us <= 1000.0);
        Ok((
            json!({"op":"rollup","rows":rows,"groups":1+(rows/2).min(32),"hot_bucket":0,
            "hot_group_rows":hot_rows,"batch_rows":BATCH,"setup_us":setup_us,
            "setup_checkpoint":"checkpoint_if_due after every setup batch; WAL sampled before the tick",
            "checkpoint_interval_ms":kaban_core::config::CatalogCfg::default().checkpoint_interval_ms,
            "setup_checkpoints":setup_checkpoints,"setup_wal_high_water_bytes":setup_wal_high_water,
            "index_state":"open file store, committed inbox, no compaction",
            "read_path":"warm MemoryStore::rollup on bucket 0","count_reads":summary(&count_us),
            "real_sum_reads":summary(&sum_us),"read_target_us":1000,"all_reads_within_target":target_met,
            "edit_path":"one apply_batch changing row 0 cost, including index staging and commit",
            "hot_group_edits":summary(&edit_us),"recount_before":before,"recount_after":after,
            "footprint":{"logical_bytes":logical,"allocated_bytes":allocated,"files":files,
                "max_observed_logical_bytes":peak_logical.max(logical),
                "max_observed_allocated_bytes":peak_allocated.max(allocated)},
            "machine":machine_profile(),"results_valid":valid}),
            valid,
        ))
    }
}
