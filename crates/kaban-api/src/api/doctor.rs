//! `KabanApi::doctor`: the health report behind `kaban doctor` (core-spec §7; core-plan C4.2
//! as amended by M4-19, M4-25). Every check is reported, never raised: a check that cannot run
//! is a failed check with the reason as its detail, so one damaged part of the database does
//! not hide the state of the others. The report is a CLI-only result with no control registry
//! entry (M4-3): the adapter gates it through `gate_response("doctor", ..)`, where every string
//! is content.

use super::KabanApi;
use crate::producers::{BudgetRow, ProducerOutcome, ProducerRegistry, RunFlags, budgets};
use crate::snapshot::{self, SnapshotOutcome};
use kaban_codegraph::{CodeGraphReader, ReadError};
use kaban_core::text_index::Visibility;
use kaban_core::{DatabaseFileModes, MaintenanceOutcome, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// What the adapter observed around its diagnostic open and what the service cannot see
/// itself: the file modes as found on disk, the schema version found against the newest one
/// the binary knows, where the audit key lives, and whether to `VACUUM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorContext {
    /// Modes of the database file and its sidecars, `stat`ed before the open (never tightened).
    pub files: DatabaseFileModes,
    /// Schema version recorded in the database.
    pub schema_version: u32,
    /// Directory holding `<db-id>.key` (core-spec S3); the key is only `stat`ed here.
    pub keys_dir: PathBuf,
    /// Run `VACUUM` after the checks (`--vacuum`; core-spec S5 residual).
    pub vacuum: bool,
}

/// One line of the report: the check's fixed name, its verdict and a short detail (modes,
/// counts, the engine's findings or the reason the check could not run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// Fixed name: `db_modes`, `integrity`, `fts_integrity`, `claim_exhausted`,
    /// `stale_artifacts`, `schema_version`, `text_index` (SS-21: `tantivy` followed by
    /// `text_index_watermark`, the payload watermark, and `text_index_lag`, the canonical
    /// head minus it), `ident_terms_version`, one `producer_<producer>` row per producer
    /// (PC-16), `stale_anchors` (E3.11: `CodeRef`s whose symbol the current snapshot lacks),
    /// `catalog_verify` and `catalog_budget` (spec 11 T6, when a catalog is wired), `vacuum`, `optimize` and
    /// `catalog_recover` when asked.
    pub name: String,
    /// Whether the check passed.
    pub ok: bool,
    /// Human-readable detail.
    pub detail: String,
}

/// The whole report: `ok` is the conjunction of every check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Every check passed.
    pub ok: bool,
    /// The checks in the order they ran.
    pub checks: Vec<DoctorCheck>,
    /// All effective resource policies, including the v4 quotas (OWN-54).
    #[serde(default)]
    pub budgets: Vec<BudgetRow>,
}

impl DoctorReport {
    /// The report on a database whose format the diagnostic open refused (spec 11 CMP-01:
    /// another schema version, or no kaban project): nothing else can be checked, so its one
    /// row is a failed `schema_version` check whose detail is the refusal's content-free
    /// `Display` (both versions and the way out).
    #[must_use]
    pub fn refused_format(refusal: &StoreError) -> Self {
        let mut report = Self {
            ok: true,
            checks: Vec::new(),
            budgets: Vec::new(),
        };
        report.push("schema_version", false, refusal.to_string());
        report
    }

    fn push(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.ok &= ok;
        self.checks.push(DoctorCheck {
            name: name.to_owned(),
            ok,
            detail: detail.into(),
        });
    }

    /// A check whose outcome came from the store: busy and failed are not ok; a store error
    /// is reported as the check's detail (its content-free `Display`).
    fn push_outcome(&mut self, name: &str, outcome: Result<MaintenanceOutcome, StoreError>) {
        match outcome {
            Ok(MaintenanceOutcome::Ok) => self.push(name, true, "ok"),
            Ok(MaintenanceOutcome::Busy) => self.push(name, false, "busy"),
            Ok(MaintenanceOutcome::Failed(findings)) => {
                self.push(name, false, findings.join("; "));
            }
            Err(e) => self.push(name, false, format!("not checked: {e}")),
        }
    }
}

/// One page of the `CodeRef` scan of `stale_anchors`.
const ANCHOR_PAGE: usize = 500;
/// At most this many stale anchors are named in the detail; `n` counts them all.
const ANCHORS_NAMED: usize = 50;

/// Octal rendering of a mode, or `absent`.
fn mode_text(mode: Option<u32>) -> String {
    mode.map_or_else(|| "absent".to_owned(), |m| format!("{m:04o}"))
}

impl KabanApi {
    /// Run the usual diagnostics and explicitly optimize FTS once. The CLI gates the
    /// report before rendering; this separate method preserves `DoctorContext` callers.
    ///
    /// # Errors
    /// Request gate refusal. Store failures remain named failed checks in the report.
    pub fn doctor_optimize(&self, ctx: &DoctorContext) -> Result<DoctorReport, crate::ApiError> {
        self.prepare_args("doctor", &serde_json::json!({"optimize":true}))?;
        let mut report = self.doctor(ctx);
        report.push_outcome("optimize", self.store.optimize_fts());
        Ok(report)
    }
    /// Recover the catalog after a crash (`doctor --recover`: stale claims and unpublished
    /// ranges a dead process left), then run every doctor check; the recovery is the
    /// `catalog_recover` row.
    ///
    /// # Errors
    /// Request gate refusal. Store failures remain named failed checks in the report.
    pub fn doctor_recover(&self, ctx: &DoctorContext) -> Result<DoctorReport, crate::ApiError> {
        self.prepare_args("doctor", &serde_json::json!({"recover":true}))?;
        let recovered = self.catalog.as_ref().map(|catalog| catalog.recover());
        let mut report = self.doctor(ctx);
        match recovered {
            Some(Ok(detail)) => report.push("catalog_recover", true, detail),
            Some(Err(e)) => report.push("catalog_recover", false, format!("not run: {e}")),
            None => report.push("catalog_recover", false, "not run: no catalog"),
        }
        Ok(report)
    }
    /// Run every doctor check and report (core-spec §7): the pre-open file modes, the engine's
    /// structural check, the FTS index check, exhausted ingest claims, stale derived artifacts,
    /// the schema version against the binary's, the text index and its lag, the producer rows,
    /// stale code anchors, and `VACUUM` when asked. Infallible by design: a failing check is a
    /// line of the report. A check is not ok on damage or a security finding (loose modes, a
    /// broken index, a check that could not run); the schema version and lag are reported as
    /// facts.
    pub fn doctor(&self, ctx: &DoctorContext) -> DoctorReport {
        let mut report = DoctorReport {
            ok: true,
            checks: Vec::with_capacity(12),
            budgets: budgets(self.engine(), &RunFlags::default())
                .into_iter()
                .map(BudgetRow::from)
                .collect(),
        };
        let files = &ctx.files;
        report.push(
            "db_modes",
            files.all_owner_only(),
            format!(
                "db {}, wal {}, shm {}",
                mode_text(Some(files.db)),
                mode_text(files.wal),
                mode_text(files.shm)
            ),
        );
        report.push_outcome("integrity", self.store.integrity_check());
        report.push_outcome("fts_integrity", self.store.fts_integrity_check());
        // spec 11 T6: the catalog's structural verification, every inconsistency and every
        // view that needs a reimport named; then its effective budget (7.6)
        if let Some(catalog) = &self.catalog {
            match catalog.verify() {
                Ok(found) if found.inconsistencies.is_empty() && found.needs_reimport.is_empty() => {
                    report.push("catalog_verify", true, "ok");
                }
                Ok(found) => {
                    let mut lines = found.inconsistencies;
                    if !found.needs_reimport.is_empty() {
                        lines.push(format!(
                            "views need reimport: {}",
                            found.needs_reimport.join(", ")
                        ));
                    }
                    report.push("catalog_verify", false, lines.join("; "));
                }
                Err(e) => report.push("catalog_verify", false, format!("not checked: {e}")),
            }
            let c = &self.engine().catalog;
            let values = format!(
                "maintenance_memory_mib {}, maintenance_deadline_ms {}, seal_ops text {} graph {} \
                 code {}, fold_depth {}, fold_delta_ratio {}, checkpoint_interval_ms {}, \
                 view_retention {}",
                c.maintenance_memory_mib,
                c.maintenance_deadline_ms,
                c.seal_ops.text,
                c.seal_ops.graph,
                c.seal_ops.code,
                c.fold_depth,
                c.fold_delta_ratio,
                c.checkpoint_interval_ms,
                c.view_retention
            );
            match catalog.status() {
                Ok(status) if status.refused_units == 0 => {
                    report.push("catalog_budget", true, values);
                }
                Ok(status) => report.push(
                    "catalog_budget",
                    false,
                    format!(
                        "{} units refused at the smallest size: raise \
                         catalog.maintenance_memory_mib or the deadline ({values})",
                        status.refused_units
                    ),
                ),
                Err(e) => report.push("catalog_budget", false, format!("not checked: {e}")),
            }
        }
        match self
            .store
            .jobs_exhausted(self.workspace(), self.engine().ingest.claim_max_attempts)
        {
            Ok(count) => report.push("claim_exhausted", count == 0, count.to_string()),
            Err(_) => report.push(
                "claim_exhausted",
                false,
                "not checked: backend read failure",
            ),
        }
        match self.store.artifact_diagnostics(self.workspace()) {
            Ok(findings) => report.push(
                "stale_artifacts",
                findings.stale_artifacts == 0,
                findings.stale_artifacts.to_string(),
            ),
            Err(_) => report.push(
                "artifact_diagnostics",
                false,
                "not checked: backend read failure",
            ),
        }
        // informational: the one schema version this kaban serves (OWN-P: a database at any
        // other version was refused at the open, with the way out, before this report)
        report.push("schema_version", true, ctx.schema_version.to_string());
        let metadata = self.store.index_meta();
        // spec 05 SS-21: the text index's own position, apart from the consumers' retention
        // lag above. The index publishes before it acknowledges, so its payload watermark and
        // the canonical head minus it (the search lag) are facts of their own. Doctor attaches
        // the index only as it exists, never creating one (T4 review t4-9): a database with no
        // index beside it has no watermark to read and is reported not checked; an attached
        // adapter serving no payload (never built, or not of this database) is missing, while
        // a payload at sequence 0 (built over an empty history) is a watermark like any other
        // (T6 review t6-5)
        match self.store.text_visible() {
            Ok(Visibility::Current) => report.push("text_index", true, "current"),
            Ok(Visibility::Unavailable) => report.push(
                "text_index",
                false,
                "tantivy (no payload served: not built, or not of this database)",
            ),
            Ok(Visibility::Through(watermark)) => {
                report.push("text_index", true, "tantivy");
                report.push("text_index_watermark", true, watermark.to_string());
                match self.store.changes_head() {
                    Ok(head) => {
                        let lag = i64::try_from(head)
                            .unwrap_or(i64::MAX)
                            .saturating_sub(watermark);
                        report.push("text_index_lag", true, lag.to_string());
                    }
                    Err(e) => report.push("text_index_lag", false, format!("not checked: {e}")),
                }
            }
            Err(e) => report.push("text_index", false, format!("not checked: {e}")),
        }
        match &metadata {
            Ok(meta) => {
                let current =
                    meta.get("ident_terms_version") == Some(kaban_core::ident::IDENT_TERMS_VERSION);
                report.push(
                    "ident_terms_version",
                    current,
                    if current {
                        kaban_core::ident::IDENT_TERMS_VERSION
                    } else {
                        "not current; run kaban reindex text"
                    },
                );
            }
            Err(_) => report.push(
                "ident_terms_version",
                false,
                "not checked: backend read failed",
            ),
        }
        match &metadata {
            Ok(meta) => {
                for row in ProducerRegistry.with_runs(self.engine(), meta) {
                    let failed = matches!(
                        row.last_outcome,
                        Some(ProducerOutcome::Failed | ProducerOutcome::SkippedBudget)
                    );
                    let detail = if failed {
                        row.last_outcome.map_or("failed", ProducerOutcome::as_str)
                    } else if row.on {
                        "on"
                    } else {
                        row.reason_off.as_deref().unwrap_or("off")
                    };
                    report.push(&format!("producer_{}", row.producer), !failed, detail);
                }
            }
            Err(error) => report.push("producers", false, format!("not checked: {error}")),
        }
        // engine-plan E3.11 Step 6 (EN-F17): anchors whose symbol the snapshot lost; facts, not damage
        let (ok, detail) = self.stale_anchors();
        report.push("stale_anchors", ok, detail);
        if ctx.vacuum {
            report.push_outcome("vacuum", self.store.vacuum());
        }
        tracing::info!(
            ok = report.ok,
            checks = report.checks.len(),
            failed = report.checks.iter().filter(|c| !c.ok).count(),
            "doctor report"
        );
        report
    }

    /// The `CodeRef`s of the bound workspace and repository whose `symbol` the current
    /// snapshot does not hold (`snapshot_mismatch`),
    /// in id order: `n: <count>; snapshot: <commit>[; anchors: <id> code: snapshot_mismatch
    /// symbol: <key>, ..]`. An ambiguous key is not stale. Without a resolved snapshot nothing
    /// can be called renamed: the check cannot inspect and names no anchor. Reads only: the
    /// lease is the provider's, nothing is attached to the store.
    fn stale_anchors(&self) -> (bool, String) {
        let resolved = match snapshot::resolve_for_card(self) {
            SnapshotOutcome::Resolved(r) => r,
            SnapshotOutcome::Absent => return (true, "cannot inspect: no snapshot".to_owned()),
            SnapshotOutcome::TooNew => {
                return (true, "cannot inspect: snapshot too new".to_owned());
            }
        };
        let (ws, repo) = (self.config.workspace.as_str(), self.config.repo.as_deref());
        let mut stale = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = match self.store.list_nodes("CodeRef", ws, offset, ANCHOR_PAGE) {
                Ok(page) => page,
                Err(e) => return (false, format!("not checked: {e}")),
            };
            let n = page.len();
            for node in page.into_iter().filter(|node| node.repo.as_deref() == repo) {
                let Some(key) = node.props.get("symbol").and_then(Value::as_str) else {
                    continue;
                };
                match resolved.snapshot.symbol_unique(key) {
                    Ok(None) if !key.is_empty() => {
                        stale.push(format!("{} code: snapshot_mismatch symbol: {key}", node.id));
                    }
                    Ok(_) | Err(ReadError::Ambiguous) => {}
                    Err(_) => return (false, "not checked: snapshot read failure".to_owned()),
                }
            }
            if n < ANCHOR_PAGE {
                break;
            }
            offset += n;
        }
        let (n, commit) = (stale.len(), &resolved.r#ref.commit);
        if n == 0 {
            return (true, format!("n: 0; snapshot: {commit}"));
        }
        stale.truncate(ANCHORS_NAMED);
        if n > ANCHORS_NAMED {
            stale.push(format!("+{} more", n - ANCHORS_NAMED));
        }
        let detail = format!("n: {n}; snapshot: {commit}; anchors: {}", stale.join(", "));
        (true, detail)
    }
}
