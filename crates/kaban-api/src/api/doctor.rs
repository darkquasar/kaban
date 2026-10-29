//! `KabanApi::doctor`: the health report behind `kaban doctor` (core-spec §7; core-plan C4.2
//! as amended by M4-19, M4-25). Every check is reported, never raised: a check that cannot run
//! is a failed check with the reason as its detail, so one damaged part of the database does
//! not hide the state of the others. The report is a CLI-only result with no control registry
//! entry (M4-3): the adapter gates it through `gate_response("doctor", ..)`, where every string
//! is content.

use super::KabanApi;
use crate::layers::{BudgetRow, LayerOutcome, LayerRegistry, RunFlags, budgets};
use crate::snapshot::{self, SnapshotOutcome};
use kaban_codegraph::{CodeGraphReader, ReadError};
use kaban_core::text_index::Visibility;
use kaban_core::{DatabaseFileModes, MaintenanceOutcome, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
    /// Also print the evaluation counters beside the configured priors (`--priors`;
    /// engine-spec EN-F16).
    pub priors: bool,
}

/// One line of the report: the check's fixed name, its verdict and a short detail (modes,
/// counts, the engine's findings or the reason the check could not run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// Fixed name: `db_modes`, `integrity`, `fts_integrity`, `schema_version`, `changes_lag`
    /// (and its deprecated alias row `outbox_lag`, EN-E10b, removed in 3.2), `conflicts`,
    /// `consumer_lag` (spec 05 SS-21: the retention lag of each registered consumer),
    /// `text_index` (SS-21: `in-database FTS5`, or `tantivy` followed by
    /// `text_index_watermark`, the payload watermark, and `text_index_lag`, the canonical
    /// head minus it), `ident_terms_version`, `key_file`, `audit`, `discovered`, `unstamped` (OWN-65: `unstamped` events per reason),
    /// `unresolved` (EN-F12b: the groups only a human action resolves), `stale_anchors`
    /// (E3.11: `CodeRef`s whose symbol the current snapshot lacks), `priors` (EN-F16, under
    /// `--priors`), `vacuum`.
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

/// What a `stat` of the key file found.
enum KeyFile {
    Absent,
    Present { mode: u32, len: u64 },
    Unreadable(std::io::ErrorKind),
}

fn stat_key_file(path: &Path) -> KeyFile {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => KeyFile::Present {
            mode: meta.permissions().mode() & 0o777,
            len: meta.len(),
        },
        Ok(_) => KeyFile::Unreadable(std::io::ErrorKind::InvalidInput),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => KeyFile::Absent,
        Err(e) => KeyFile::Unreadable(e.kind()),
    }
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
    /// Run every doctor check and report (core-spec §7): the pre-open file modes, the engine's
    /// structural check, the FTS index check, the schema version against the binary's, the
    /// `ChangeLog` lag and open conflicts from `stats`, the audit key file's presence and mode, the
    /// audit chain, and `VACUUM` when asked. Infallible by design: a failing check is a line of
    /// the report. A check is not ok on damage or a security finding (loose modes, a broken
    /// index or chain, a check that could not run); the schema version, lag and conflict
    /// counts are reported as facts.
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
        if let Some(authority) = kaban_core::CurrentProducerAuthority::from_config(
            self.engine(),
            kaban_core::Layer::Gazetteer,
        ) {
            match self.store.artifact_diagnostics(self.workspace(), authority) {
                Ok(findings) => {
                    report.push(
                        "stale_artifacts",
                        findings.stale_artifacts == 0,
                        findings.stale_artifacts.to_string(),
                    );
                    report.push(
                        "retired_producers",
                        findings.retired_producers == 0,
                        findings.retired_producers.to_string(),
                    );
                    report.push(
                        "reconcile_cursor_lag",
                        true,
                        findings.cursors_behind.to_string(),
                    );
                }
                Err(_) => {
                    report.push(
                        "artifact_diagnostics",
                        false,
                        "not checked: backend read failure",
                    );
                }
            }
        } else {
            report.push(
                "artifact_diagnostics",
                false,
                "not checked: unavailable producer",
            );
        }
        // informational: the one schema version this kaban serves (OWN-P: a database at any
        // other version was refused at the open, with the way out, before this report)
        report.push("schema_version", true, ctx.schema_version.to_string());
        match self.store.stats(None) {
            Ok(stats) => {
                let lag = stats["changes_lag"].as_i64().unwrap_or_default();
                let conflicts = stats["conflicts"].as_i64().unwrap_or_default();
                // informational: lag and open conflicts are not damage; `outbox_lag` is the
                // EN-E10b deprecated alias row of `changes_lag` (same detail, removed in 3.2)
                report.push("changes_lag", true, format!("{lag}"));
                report.push("outbox_lag", true, format!("{lag}"));
                report.push("conflicts", true, format!("{conflicts}"));
            }
            Err(e) => {
                let detail = format!("not checked: {e}");
                report.push("changes_lag", false, detail.clone());
                report.push("outbox_lag", false, detail.clone());
                report.push("conflicts", false, detail);
            }
        }
        // informational: the retention lag of each registered consumer by name (spec 05
        // SS-21; `changes_lag` above is the slowest one's); a consumer's name is a lease name,
        // never content
        match self.store.changes_lag_by_consumer() {
            Ok(lags) if lags.is_empty() => {
                report.push("consumer_lag", true, "no registered consumer");
            }
            Ok(lags) => {
                let detail: Vec<String> = lags
                    .iter()
                    .map(|(consumer, lag)| format!("{consumer}: {lag}"))
                    .collect();
                report.push("consumer_lag", true, detail.join("; "));
            }
            Err(e) => report.push("consumer_lag", false, format!("not checked: {e}")),
        }
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
                for row in LayerRegistry.with_runs(self.engine(), meta) {
                    let failed = matches!(
                        row.last_outcome,
                        Some(LayerOutcome::Failed | LayerOutcome::SkippedBudget)
                    );
                    let detail = if failed {
                        row.last_outcome.map_or("failed", LayerOutcome::as_str)
                    } else if row.on {
                        "on"
                    } else {
                        row.reason_off.as_deref().unwrap_or("off")
                    };
                    report.push(&format!("layer_{}", row.layer.as_str()), !failed, detail);
                }
            }
            Err(error) => report.push("layers", false, format!("not checked: {error}")),
        }
        let db_id = metadata
            .ok()
            .and_then(|meta| meta.get(kaban_core::IndexMeta::DB_ID).map(str::to_owned));
        // the chain is verified whenever the file holds a key; its mode is a separate finding
        let key_file = db_id.map(|id| stat_key_file(&kaban_policy::key_path(&ctx.keys_dir, &id)));
        let (key_ok, key_usable, key_detail) = match key_file {
            None => (false, false, "the database has no db_id".to_owned()),
            Some(KeyFile::Absent) => (false, false, "absent".to_owned()),
            Some(KeyFile::Unreadable(kind)) => {
                (false, false, format!("not a readable file: {kind}"))
            }
            Some(KeyFile::Present { mode, len }) => {
                let usable = usize::try_from(len).is_ok_and(|len| len == kaban_policy::KEY_LEN);
                (
                    usable && mode == DatabaseFileModes::OWNER_ONLY,
                    usable,
                    format!("present, mode {}, {len} bytes", mode_text(Some(mode))),
                )
            }
        };
        report.push("key_file", key_ok, key_detail);
        if key_usable {
            match self.store.audit_verify() {
                Ok(v) => report.push(
                    "audit",
                    v.ok,
                    match v.first_bad_seq {
                        Some(seq) => format!("{} entries, first bad seq {seq}", v.entries),
                        None => format!("{} entries", v.entries),
                    },
                ),
                Err(e) => report.push("audit", false, format!("not checked: {e}")),
            }
        } else {
            report.push("audit", false, "not checked: the key file is unusable");
        }
        // engine-plan E1.8 (EN-F5): the discovered kinds of the bound workspace, as facts:
        // the prunable ones, the promotion-eligible ones and the labels refused at the cap
        match self.registry.prune(self, true) {
            Ok(prune) => {
                let knobs = self.registry.discovered(self.engine());
                let eligible: Vec<String> = self
                    .store
                    .kind_stats(&self.config.workspace)
                    .unwrap_or_default()
                    .iter()
                    .filter(|s| {
                        s.ns == "x"
                            && s.n_nodes >= u64::from(knobs.promote_when.min_count)
                            && s.n_sources >= u64::from(knobs.promote_when.min_sources)
                    })
                    .map(kaban_core::KindStat::stored)
                    .collect();
                report.push(
                    "discovered",
                    true,
                    // eligibility is partial until E2 supplies `mean_score`: `min_count` and
                    // `min_sources` are applied, `min_mean_score` is not (E1.9a; E2 bridge)
                    format!(
                        "prunable: {}; promote_when eligible (partial: min_count and min_sources only until E2 supplies mean_score): {}; n_rejected: {}",
                        if prune.kinds.is_empty() {
                            "none".to_owned()
                        } else {
                            prune.kinds.join(", ")
                        },
                        if eligible.is_empty() {
                            "none".to_owned()
                        } else {
                            eligible.join(", ")
                        },
                        prune.n_rejected
                    ),
                );
            }
            Err(e) => report.push("discovered", false, format!("not checked: {e}")),
        }
        // OWN-65: historical write-time facts stay historical. HEAD availability is a
        // current observation only, never a backfilled reason for earlier writes.
        let stamp_kinds = self.registry.stamp_kinds();
        let kinds: Vec<&str> = stamp_kinds.iter().map(String::as_str).collect();
        match self.store.unstamped_counts(self.config.workspace.as_str()) {
            Ok(counts) => {
                let historical = format!(
                    "historical no_repository: {}, no_history: {}",
                    counts.no_repository, counts.no_history
                );
                match self.store.unstamped_current_count(
                    self.workspace().as_str(),
                    self.config.repo.as_deref(),
                    &kinds,
                ) {
                    Ok(eligible) => {
                        let status = match self.config.repo_root.as_deref() {
                            None => "no_repository",
                            Some(root) if self.git.head(root).is_some() => "pending",
                            Some(_) => "unknown",
                        };
                        report.push(
                            "unstamped",
                            true,
                            format!("{historical}; current eligible: {eligible}; status: {status}"),
                        );
                    }
                    Err(e) => report.push(
                        "unstamped",
                        false,
                        format!("{historical}; current not checked: {e}"),
                    ),
                }
            }
            Err(e) => report.push("unstamped", false, format!("not checked: {e}")),
        }
        // engine-plan E2.4 (EN-F12b "the group is listed by `doctor`"): the conflict groups of
        // the bound workspace that only a human action resolves; facts, not damage
        match self.store.unresolved_groups(self.config.workspace.as_str()) {
            Ok(groups) => report.push(
                "unresolved",
                true,
                format!(
                    "n: {}; groups: {}",
                    groups.len(),
                    if groups.is_empty() {
                        "none".to_owned()
                    } else {
                        groups.join(", ")
                    }
                ),
            ),
            Err(e) => report.push("unresolved", false, format!("not checked: {e}")),
        }
        // engine-plan E3.11 Step 6 (EN-F17): the anchors `explain` calls `stale`; facts, not damage
        let (ok, detail) = self.stale_anchors();
        report.push("stale_anchors", ok, detail);
        // EN-F16: every configured prior beside the Beta posterior mean of its counters; v1
        // prints and never applies them
        if ctx.priors {
            match self.store.priors_obs() {
                Ok(rows) => report.push(
                    "priors",
                    true,
                    rows.iter()
                        .map(|r| {
                            format!(
                                "{}.{}.{}: configured {}, observed {} (accepted {}, rejected {})",
                                r.layer.as_str(),
                                r.method.as_str(),
                                r.claim_class.as_str(),
                                r.prior,
                                r.observed(),
                                r.accepted,
                                r.rejected
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                Err(e) => report.push("priors", false, format!("not checked: {e}")),
            }
        }
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
    /// snapshot does not hold (the `snapshot_mismatch` leg of `why`, `explain`'s `stale`),
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
            stale.push(format!("+{} more (kaban review)", n - ANCHORS_NAMED));
        }
        let detail = format!("n: {n}; snapshot: {commit}; anchors: {}", stale.join(", "));
        (true, detail)
    }
}
