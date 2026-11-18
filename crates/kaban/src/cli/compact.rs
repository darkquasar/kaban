//! `kaban compact` (spec 11 PC-7): seal, fold and reclaim the catalog by its policy, then
//! vacuum and checkpoint, printing the report as the one JSON document. [`StoreCatalog`] is the
//! SQLite store's catalog behind the service's [`CatalogMaintenance`] port, wired by every
//! open of the binary, and [`after_ack`] is the bounded checkpoint a one-shot write runs once
//! its acknowledgement is printed (KER-09).

use crate::cli::{output::Format, seam};
use kaban_api::{
    CatalogIndexStatus, CatalogMaintenance, CatalogStatus, CatalogVerify, CompactReport, CompactUnit, IndexName,
    KabanApi,
};
use kaban_core::StoreError;
use kaban_core::query_budget::Limit;
use kaban_store_sqlite::{ClassExtents, SkipReason, SqliteStore, WorkKind};
use serde_json::{Value, json};
use std::process::ExitCode;
use std::sync::Arc;

/// The SQLite store's catalog as the service's maintenance port.
pub(crate) struct StoreCatalog(pub(crate) Arc<SqliteStore>);

impl CatalogMaintenance for StoreCatalog {
    fn compact(&self) -> Result<CompactReport, StoreError> {
        let r = self.0.compact()?;
        let kind = |k: WorkKind| match k {
            WorkKind::Seal => "seal",
            WorkKind::Fold => "fold",
            WorkKind::Purge => "purge",
        };
        let published = r
            .published
            .iter()
            .map(|u| CompactUnit {
                class: u.class.to_owned(),
                kind: kind(u.kind).to_owned(),
                extents: u.extents.clone(),
                sediment_rows: u64::try_from(u.sediment_rows).unwrap_or(u64::MAX),
                generation: Some(u.generation),
                skipped: None,
            })
            .collect();
        let skipped = r
            .skipped
            .iter()
            .map(|u| CompactUnit {
                class: u.class.to_owned(),
                kind: kind(u.kind).to_owned(),
                extents: u.extents.clone(),
                sediment_rows: u64::try_from(u.sediment_rows).unwrap_or(u64::MAX),
                generation: None,
                skipped: Some(match &u.reason {
                    SkipReason::Claimed => "claimed".to_owned(),
                    SkipReason::Budget(_) => "maintenance_budget_refused".to_owned(),
                    SkipReason::Refused(why) => format!("refused: {why}"),
                }),
            })
            .collect();
        Ok(CompactReport {
            pass_skipped: r.pass_skipped.map(|e| {
                match e.limit {
                    Limit::Memory => "inputs_exceed_maintenance_memory",
                    _ => "maintenance_deadline_exceeded",
                }
                .to_owned()
            }),
            published,
            skipped,
            reclaimed: r.reclaim.reclaimed,
            pending: r.reclaim.pending,
            released_bytes: r.reclaim.released_bytes,
            zero_filled_bytes: r.reclaim.zero_filled_bytes,
            barrier_pending: r.reclaim.barrier_pending,
            degraded_durability: r.reclaim.degraded_durability,
            space_reclaim_unsupported: r.reclaim.space_reclaim_unsupported,
            vacuumed_pages: r.vacuumed_pages,
            checkpointed: r.checkpoint.is_some(),
            views_retired: r.views_retired,
        })
    }

    fn recover(&self) -> Result<String, StoreError> {
        let r = self.0.recover_catalog()?;
        Ok(if r.maintenance_live {
            "maintenance live: nothing cleared".to_owned()
        } else {
            format!(
                "{} stale claim(s) cleared, {} unpublished range(s) reclaimed{}",
                r.claims_cleared,
                r.ranges_reclaimed.len(),
                if r.hole_detection {
                    ""
                } else {
                    " (hole_detection: false, only nonzero ranges found)"
                }
            )
        })
    }

    fn verify(&self) -> Result<CatalogVerify, StoreError> {
        let r = self.0.verify_catalog()?;
        let mut inconsistencies = Vec::new();
        if !r.maintenance_live {
            for (offset, len) in &r.unreferenced_ranges {
                inconsistencies.push(format!("unreferenced range {len} bytes at {offset}"));
            }
            for extent in &r.stale_claims {
                inconsistencies.push(format!("stale claim on extent {extent}"));
            }
        }
        for file in &r.missing_files {
            inconsistencies.push(format!("missing file {file}"));
        }
        inconsistencies.extend(r.chain_inconsistencies.iter().cloned());
        for extent in &r.hash_mismatches {
            inconsistencies.push(format!("extent {extent}: content hash mismatch"));
        }
        Ok(CatalogVerify {
            indexes: indexes(&r.classes),
            inconsistencies,
            needs_reimport: r.needs_reimport,
        })
    }

    fn status(&self) -> Result<CatalogStatus, StoreError> {
        let s = self.0.catalog_status()?;
        Ok(CatalogStatus {
            indexes: indexes(&s.classes),
            refused_units: s.refused_units,
        })
    }

    fn checkpoint_if_due(&self) -> Result<bool, StoreError> {
        Ok(self.0.checkpoint_if_due()?.is_some())
    }
}

/// The text, graph and code index states from the catalog's per-class counts.
fn indexes(classes: &[ClassExtents]) -> Vec<CatalogIndexStatus> {
    let class = |name: &str| classes.iter().find(|c| c.class == name);
    let index = |index: IndexName, base: &str, layer: &str| {
        let (base, layer) = (class(base), class(layer));
        let sediment = |c: Option<&ClassExtents>| c.map_or(0, |c| c.sediment_rows);
        let newest = |c: Option<&ClassExtents>| c.and_then(|c| c.newest_generation);
        CatalogIndexStatus {
            index,
            sediment_rows: sediment(base) + sediment(layer),
            layers: layer.map_or(0, |c| u32::try_from(c.live).unwrap_or(u32::MAX)),
            base_generation: newest(base),
            last_compaction: newest(base)
                .max(newest(layer))
                .map(|g| i64::try_from(g).unwrap_or(i64::MAX)),
        }
    };
    vec![
        // text has tiers and no base
        index(IndexName::Text, "", "text_tier"),
        index(IndexName::Graph, "graph_base", "graph_layer"),
        index(IndexName::Code, "code_base", "code_layer"),
    ]
}

/// Run one compaction through the seam and print its report.
pub(crate) fn run(api: &KabanApi, format: Format) -> ExitCode {
    seam::run_tool::<Value, CompactReport, _>(api, "compact", json!({}), format, |api, _| {
        api.compact()
    })
}

/// The bounded checkpoint after a one-shot write's acknowledgement (KER-09), when `code` says
/// the write succeeded. The write is already acknowledged, so a failure is logged and never
/// changes the exit code, which is returned as given.
pub(crate) fn after_ack(api: &KabanApi, code: ExitCode) -> ExitCode {
    let Some(catalog) = api.catalog().filter(|_| code == ExitCode::SUCCESS) else {
        return code;
    };
    match catalog.checkpoint_if_due() {
        Ok(ran) => tracing::debug!(ran, "post-ack checkpoint"),
        Err(e) => tracing::warn!(error = %e, "post-ack checkpoint failed"),
    }
    code
}
