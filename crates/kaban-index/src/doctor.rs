//! `kaban-index doctor` (stage1-spec §7 CLI, G1-F3 `--module-version` check, G1-S5.1 "doctor
//! shows every probe result"; stage1-plan CG4.2, CG4.6; U2 RU2-3, RU2-8): the configuration in
//! effect and its effective overrides (OWN-54/SD-11), the detected languages of the repository
//! with their indexers named as `found` or by the refusal class (never a path), each admitted
//! indexer's version and the literal `--module-version` flag check against `scip-go index
//! --help` (the spec says `scip-go --help`; scip-go 0.2.7 lists the flag under its `index`
//! subcommand only, observed in this sandbox: deviation recorded in the CG4.2 report), the
//! isolation tier probes with the tier `index` would select under the configured policy (CG4.3,
//! G1-S5.1), the trust state and identity validation result of the repository (`revalidate`
//! runs: a mismatch revokes the mark here as on `status`), the platform notes of G1-S5.1 and
//! the fixed trust text.
//!
//! Only an admitted tool is ever probed (`tools::admit_executable` at detection, `recheck`
//! immediately before the probe), inside the namespaces view when that is the tier `index`
//! would select (the helper is pid 1 of the view and `exec`s the tool after the privilege
//! drop), natively otherwise, always with the production descriptor policy
//! (`InheritedFds::Refuse`). Nothing from the repository or a tool's output reaches the report
//! unscanned: a captured version line is scanned whole before it is shortened
//! (`gate_output`), and the binary's final gating pass covers every string of the report.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kaban_core::config::{ConfigError, EngineConfig};
use kaban_core::process::{EnvAllowlist, Exec, InheritedFds, Spec};
use kaban_policy::Policy;
use serde::Serialize;

use crate::detect::{self, IndexerOverride, Lang, ResolvedIndexer};
use crate::identity_cli::{self, IdentityCliError};
use crate::isolate::namespaces::{
    self, HELPER_EXIT, SELFCHECK_FAILED_TEXT, USERNS_SYSCTLS, run_tool_in_view,
};
use crate::isolate::{self, Probe, SelectError, Selection, Tier};
use crate::language::{go, rust};
use crate::status::{TrustState, trust_state};
use crate::tools::{self, AdmittedTool};
use crate::trust::TRUST_TEXT;

pub use crate::language::rust::{
    Linker, SYSTEM_CC, linker_helper, rust_capabilities, rust_host, rust_linker,
    rust_repository_profile, rust_repository_toolchain, rust_toolchain,
};

/// Deadline of one `--version` or `--help` call.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(5);
/// Bytes kept of a probe's output.
const PROBE_CAP: usize = 64 * 1024;
/// Characters kept of a gated version line.
const LINE_CAP: usize = 200;
/// The flag G1-F3 spells for the second scip-go run; *unverified* in the spec, confirmed here.
pub const MODULE_VERSION_FLAG: &str = "--module-version";
/// The word a resolved, admitted indexer is reported as (its path is never part of the report).
pub const INDEXER_FOUND: &str = "found";

/// What `doctor` found.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    /// The configuration file consulted.
    pub config_path: PathBuf,
    /// `ok`, or the fixed class and field path of the load error.
    pub config: ConfigStatus,
    /// Every `index.*` key whose value differs from the default, as `(key, value)`.
    pub overrides: Vec<(String, String)>,
    /// The `index.*` keys in effect.
    pub index: kaban_core::config::Index,
    /// Detected languages (`None` when no repository was given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub languages: Option<Result<Vec<LanguageCheck>, &'static str>>,
    /// One entry per detected indexer that was admitted.
    pub indexers: Vec<IndexerCheck>,
    /// The tier probes and the tier `index` would select under `index.isolation`.
    pub isolation: IsolationCheck,
    /// The trust state and identity validation of the repository (`None` without one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<TrustCheck>,
    /// The platform notes of G1-S5.1.
    pub platform: PlatformNotes,
}

/// One detected language as `doctor` reports it (U2 RU2-8): the markers that fired and the
/// indexer as [`INDEXER_FOUND`] or the fixed refusal class; never a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LanguageCheck {
    /// The language.
    pub lang: Lang,
    /// The marker files present at the root.
    pub markers: Vec<&'static str>,
    /// `found`, or the class (`indexer_not_found`, `indexer_untrusted_mode`,
    /// `indexer_untrusted_location`, `indexer_override_invalid`).
    pub indexer: &'static str,
}

/// The trust and identity findings (G1-S6; CG4.6). `doctor` never prints the repository
/// path (CG4.2's pin: the report carries markers and classes only); `status` and `trust` show
/// the live path, scanned.
#[derive(Debug, Serialize)]
pub struct TrustCheck {
    /// `<repo-id>` once the path passed the gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// The mark's state after revalidation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<TrustState>,
    /// Whether the live identity could be resolved and matched.
    pub identity: IdentityCheck,
    /// The fixed class when the repository could not be resolved or admitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<&'static str>,
}

/// The identity validation result as `doctor` prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityCheck {
    /// Resolved; it matches the mark when one exists.
    Resolved,
    /// Resolved, but the mark disagreed: revoked now.
    Mismatch,
    /// Could not be resolved (not a repository, gate hit, key or data directory refused).
    Unavailable,
}

impl IdentityCheck {
    /// The fixed text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Mismatch => "mismatch (mark revoked)",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The platform notes of G1-S5.1, read without a child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformNotes {
    /// `/proc/sys/kernel/osrelease`.
    pub kernel: String,
    /// `/proc/sys/kernel/unprivileged_userns_clone` (`None` when the kernel has no such file:
    /// not restricted).
    pub unprivileged_userns_clone: Option<String>,
    /// `/proc/sys/user/max_user_namespaces` (`None` when absent).
    pub max_user_namespaces: Option<String>,
    /// The `unshare` candidate that resolved under the handle rule, if any.
    pub unshare: Option<PathBuf>,
}

/// Read the platform notes.
#[must_use]
pub fn platform_notes() -> PlatformNotes {
    let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned());
    PlatformNotes {
        kernel: namespaces::kernel_release(),
        unprivileged_userns_clone: read(USERNS_SYSCTLS[0]),
        max_user_namespaces: read(USERNS_SYSCTLS[1]),
        unshare: namespaces::resolve_unshare().ok().map(|w| w.path),
    }
}

/// Resolve the repository, derive `<repo-id>`, admit its directory and revalidate the mark
/// (G1-S6 on `doctor`). Every failure is a fixed class; no path is part of the result.
#[must_use]
pub fn check_trust(home: &Path, policy: &Policy, repo: &Path) -> TrustCheck {
    let mut check = TrustCheck {
        repo_id: None,
        state: None,
        identity: IdentityCheck::Unavailable,
        class: None,
    };
    let class_of = |e: &IdentityCliError| match e {
        IdentityCliError::RepoPathRefused => "repo_path_refused",
        IdentityCliError::NotARepository | IdentityCliError::Identity(_) => "not_a_repository",
        IdentityCliError::Key => "key_unavailable",
        IdentityCliError::DataDir(_) => "data_dir_refused",
        IdentityCliError::GitUntrusted => "git_untrusted",
        _ => "env",
    };
    let resolved = match identity_cli::resolve(repo) {
        Ok(r) => r,
        Err(e) => {
            check.class = Some(class_of(&e));
            return check;
        }
    };
    let admitted = identity_cli::open_key(home)
        .and_then(|key| identity_cli::repo_id_gated(policy, &key, &resolved.canonical_common_dir))
        .and_then(|id| identity_cli::admit_repo_dir(home, &id).map(|dir| (id, dir)));
    match admitted {
        Ok((id, dir)) => {
            let state = trust_state(&dir.handle, &resolved.identity);
            check.identity = match state {
                TrustState::Revoked | TrustState::Malformed => IdentityCheck::Mismatch,
                TrustState::Trusted | TrustState::NotTrusted | TrustState::Io => {
                    IdentityCheck::Resolved
                }
            };
            check.repo_id = Some(id);
            check.state = Some(state);
        }
        Err(e) => check.class = Some(class_of(&e)),
    }
    check
}

/// The isolation findings (G1-S5.1).
#[derive(Debug, Serialize)]
pub struct IsolationCheck {
    /// Every probe result (`namespaces`, `container`), fixed reasons when unavailable.
    pub probes: Vec<Probe>,
    /// The tier `index` would select: `(tier, detail, warning)`, or the exit 8 refusal under
    /// `required`.
    pub selected: Result<SelectedTier, &'static str>,
}

/// The selected tier as the report prints it.
#[derive(Debug, Serialize)]
pub struct SelectedTier {
    /// `namespaces`, `container` or `none`.
    pub tier: &'static str,
    /// `meta.isolation_detail`.
    pub detail: String,
    /// `isolation_none` on the fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<&'static str>,
}

impl From<Selection> for SelectedTier {
    fn from(s: Selection) -> Self {
        Self {
            tier: s.tier.as_str(),
            detail: s.detail.0,
            warning: s.warning,
        }
    }
}

/// Probe the tiers under `cfg` and select as `index` would; `helper` is the `kaban-index`
/// binary the namespaces probe runs.
#[must_use]
pub fn check_isolation(cfg: &kaban_core::config::Index, helper: &Path) -> IsolationCheck {
    let probes = if cfg.isolation == kaban_core::config::Isolation::Off {
        Vec::new()
    } else {
        isolate::probe_all(cfg, helper)
    };
    let selected = isolate::select(cfg.isolation, &probes)
        .map(SelectedTier::from)
        .map_err(|e| match e {
            SelectError::RequiredUnavailable => "isolation_required_unavailable",
        });
    IsolationCheck { probes, selected }
}

/// Whether the configuration loaded.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConfigStatus {
    /// Loaded (or absent: defaults).
    Ok,
    /// Did not load; `class` and `path` from [`ConfigError`], never a value.
    Invalid {
        /// `parse`, `range` or `io`.
        class: &'static str,
        /// The dotted field path, or the io class.
        path: String,
    },
}

/// One indexer's probe results.
#[derive(Debug, Serialize)]
pub struct IndexerCheck {
    /// The language.
    pub lang: Lang,
    /// The gated first line of `<indexer> --version`, or the fixed class of the failure
    /// (`indexer_failed`, `indexer_timeout`, `indexer_changed`, `isolation_selfcheck_failed`).
    pub version: Result<String, &'static str>,
    /// Go only: whether `scip-go index --help` mentions `--module-version` literally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_version_flag: Option<FlagCheck>,
    /// Rust only: the system linker's admission (RI-3b), a class only, never a path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linker: Option<LinkerCheck>,
    /// Go only (G2): the toolchain row, `go toolchain: <installed>, go.mod requires <v>` with
    /// the action when nothing satisfies it, or `go toolchain: (<class>)` when the check could
    /// not run; versions only, never a path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub go_toolchain: Option<String>,
}

/// The system linker's admission as `doctor` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkerCheck {
    /// `/usr/bin/cc` resolved under `/usr/bin`, was admitted and found its helpers at the
    /// probe site.
    Admitted,
    /// `rust_linker_refused`: it did not resolve, was not admitted, or answered a path outside
    /// `/usr`.
    Refused,
    /// The toolchain was refused before the linker was reached, or the producer's probe failed.
    NotChecked,
}

impl LinkerCheck {
    fn of(admission: Result<(), &'static str>) -> Self {
        match admission {
            Ok(()) => Self::Admitted,
            Err("rust_linker_refused") => Self::Refused,
            Err(_) => Self::NotChecked,
        }
    }

    /// The fixed text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Refused => "rust_linker_refused",
            Self::NotChecked => "not checked",
        }
    }
}

/// The result of the literal flag check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagCheck {
    /// The help text names the flag.
    Present,
    /// The help text does not name it: the second run cannot be pinned (G1-F3).
    Absent,
    /// `--help` failed or timed out.
    NotChecked,
}

/// Where a probe runs: inside the namespaces view through `helper`, or natively.
#[derive(Debug, Clone, Copy)]
pub enum ProbeSite<'a> {
    /// `index` would select the namespaces tier: the probe runs inside its view.
    View {
        /// Admitted helper binary.
        helper: &'a Path,
        /// Admitted narrow toolchain resources, bound read-only.
        resources: &'a [PathBuf],
    },
    /// No isolating tier: the native child, the production descriptor policy.
    Native,
    /// Required isolation was unavailable; no tool may run.
    Unavailable,
}

impl<'a> ProbeSite<'a> {
    /// Preserve the selected tier while adding admitted read-only resources.
    #[must_use]
    pub fn with_resources(self, resources: &'a [PathBuf]) -> Self {
        match self {
            Self::View { helper, .. } => Self::View { helper, resources },
            other => other,
        }
    }
}

/// Load the configuration at `<home>/config.yaml` and build the `doctor` report over `repo`
/// (when given), resolving indexers through `overrides` and the caller's `PATH`; an indexer
/// under the repository or under `home` is refused (U2 RU2-3) and never probed.
pub fn run(
    home: &Path,
    policy: &Policy,
    repo: Option<&Path>,
    overrides: &[IndexerOverride],
) -> DoctorReport {
    let config_path = &home.join("config.yaml");
    let (cfg, config) = match kaban_core::config::load(config_path) {
        Ok(cfg) => (cfg, ConfigStatus::Ok),
        Err(e) => (
            EngineConfig::default(),
            ConfigStatus::Invalid {
                class: match e {
                    ConfigError::Io { .. } => "io",
                    ConfigError::Parse { .. } => "parse",
                    ConfigError::Range { .. } => "range",
                    ConfigError::NotAvailable { .. } => "not_available_in_this_release",
                },
                path: match e {
                    ConfigError::Io { class } => class.to_owned(),
                    ConfigError::Parse { path } | ConfigError::Range { path } => path,
                    ConfigError::NotAvailable { path } => path.to_owned(),
                },
            },
        ),
    };
    let overrides_in_effect = cfg
        .effective_overrides()
        .into_iter()
        .filter(|(k, _)| k.starts_with("index."))
        .collect();
    // the tier first: it decides where the indexer probes run (U2 RU2-3)
    let helper =
        isolate::namespaces::self_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let isolation = check_isolation(&cfg.index, &helper);
    let site = match &isolation.selected {
        Ok(sel) if sel.tier == Tier::Namespaces.as_str() => ProbeSite::View {
            helper: &helper,
            resources: &[],
        },
        Ok(_) => ProbeSite::Native,
        Err(_) => ProbeSite::Unavailable,
    };
    let data_dir = std::fs::canonicalize(home);
    // Detection works on any directory (identity is the trust check's concern, below) but over
    // the canonical path: the checkout root that a canonical executable is compared against
    // (U2 RU2-3) is never a raw alias. A path that does not resolve holds no stage 1 marker.
    let canonical_repo =
        repo.map(|r| std::fs::canonicalize(r).map_err(|_| "unsupported_in_stage_1"));
    let caller_home = std::env::var_os("HOME")
        .map(|h| std::fs::canonicalize(PathBuf::from(h)))
        .transpose();
    let path_env = std::env::var_os("PATH");
    let detected = canonical_repo.as_ref().map(|root| {
        let root = root.as_ref().map_err(|class| *class)?;
        let data_dir = data_dir.as_ref().map_err(|_| "data_dir_refused")?;
        caller_home.as_ref().map_err(|_| "env")?;
        // the forbidden root is the enclosing checkout, not only the given directory: a
        // nested `--repo <checkout>/member` never admits `<checkout>/bin/<indexer>` (ri12-1)
        detect::languages(
            root,
            overrides,
            path_env.as_ref(),
            &[enclosing_checkout(root), data_dir],
        )
        .map_err(|_| "unsupported_in_stage_1")
    });
    let mut indexers = Vec::new();
    let languages = detected.map(|result| {
        result.map(|detected| {
            detected
                .iter()
                .map(|d| {
                    let indexer = match &d.indexer {
                        Ok(resolved) => {
                            // Admit Go's resources before even a version/help probe can
                            // execute the indexer. Keep the same diagnostic row as `index`.
                            let go_check = (d.lang == Lang::Go).then(
                                || -> Result<go::ToolchainCheck, &'static str> {
                                    let root = canonical_repo
                                        .as_ref()
                                        .and_then(|result| result.as_ref().ok())
                                        .ok_or("unsupported_in_stage_1")?;
                                    let data_dir =
                                        data_dir.as_ref().map_err(|_| "data_dir_refused")?;
                                    let caller_home = caller_home.as_ref().map_err(|_| "env")?;
                                    let checkout = enclosing_checkout(root);
                                    let rule = tools::RootRule {
                                        checkout,
                                        home: caller_home.as_deref(),
                                        data_dir,
                                    };
                                    let chain =
                                        tools::toolchain(Lang::Go, path_env.as_ref(), &rule)
                                            .map_err(|_| "toolchain_refused")?;
                                    let modcache = go::modcache(root, data_dir)
                                        .map_err(|_| go::GO_MODCACHE_REFUSED)?;
                                    go::check_toolchain(
                                        chain.as_ref(),
                                        modcache.as_deref(),
                                        root,
                                        &[root, checkout, data_dir],
                                        site,
                                    )
                                },
                            );
                            let go_refusal = go_check.as_ref().and_then(|result| match result {
                                Ok(admitted) => admitted.refusal(),
                                Err(class) => Some(*class),
                            });
                            let mut check = if let Some(class) = go_refusal {
                                IndexerCheck {
                                    lang: d.lang,
                                    version: Err(class),
                                    module_version_flag: Some(FlagCheck::NotChecked),
                                    linker: None,
                                    go_toolchain: None,
                                }
                            } else {
                                check_indexer(policy, d.lang, resolved, site)
                            };
                            if let Some(result) = go_check {
                                check.go_toolchain = Some(result.map_or_else(
                                    |class| format!("go toolchain: ({class})"),
                                    |admitted| admitted.row(),
                                ));
                            }
                            if d.lang == Lang::Rust
                                && check.version.is_ok()
                                && let Some(Ok(root)) = canonical_repo.as_ref()
                            {
                                let admission = rust_repository_profile(root).and_then(|()| {
                                    let data_dir =
                                        data_dir.as_ref().map_err(|_| "data_dir_refused")?;
                                    let caller_home = caller_home.as_ref().map_err(|_| "env")?;
                                    let checkout = enclosing_checkout(root);
                                    let rule = tools::RootRule {
                                        checkout,
                                        home: caller_home.as_deref(),
                                        data_dir,
                                    };
                                    let chain =
                                        tools::toolchain(Lang::Rust, path_env.as_ref(), &rule)
                                            .map_err(|_| "rust_toolchain_refused")?;
                                    rust::admit_toolchain(
                                        chain.as_ref(),
                                        &[root, checkout, data_dir],
                                        site,
                                    )
                                });
                                // the linker row keeps the probes' outcome even when the
                                // selector refuses afterwards (astra ri4-3bc-3)
                                check.linker = Some(LinkerCheck::of(
                                    admission.as_ref().map(|_| ()).map_err(|class| *class),
                                ));
                                let admission = admission.and_then(|admitted| {
                                    rust_repository_toolchain(root, &admitted.root)
                                });
                                if let Err(class) = admission {
                                    check.version = Err(class);
                                }
                            }
                            indexers.push(check);
                            INDEXER_FOUND
                        }
                        Err(class) => class.as_str(),
                    };
                    LanguageCheck {
                        lang: d.lang,
                        markers: d.markers.clone(),
                        indexer,
                    }
                })
                .collect()
        })
    });
    let trust = repo.map(|r| check_trust(home, policy, r));
    DoctorReport {
        config_path: config_path.clone(),
        config,
        overrides: overrides_in_effect,
        index: cfg.index,
        languages,
        indexers,
        isolation,
        trust,
        platform: platform_notes(),
    }
}

/// Probe one admitted indexer: the identity is re-checked first (U2 RU2-3: a candidate
/// replaced since admission is `indexer_changed`, nothing runs), then `--version` and, for
/// Go, `index --help`.
fn check_indexer(
    policy: &Policy,
    lang: Lang,
    resolved: &ResolvedIndexer,
    site: ProbeSite<'_>,
) -> IndexerCheck {
    let admitted = AdmittedTool {
        path: resolved.path.clone(),
        identity: resolved.identity,
    };
    if tools::recheck(&admitted).is_err() {
        return IndexerCheck {
            lang,
            version: Err("indexer_changed"),
            module_version_flag: (lang == Lang::Go).then_some(FlagCheck::NotChecked),
            linker: (lang == Lang::Rust).then_some(LinkerCheck::NotChecked),
            go_toolchain: None,
        };
    }
    let path = admitted.path.as_path();
    let version = probe(site, path, &["--version"]).map(|out| gate_output(policy, &out));
    let version = if lang == Lang::Rust {
        version.and_then(|v| {
            let output = probe(site, path, &["capabilities", "--json"])?;
            rust::check_rust_capabilities(&output)?;
            Ok(v)
        })
    } else {
        version
    };
    let module_version_flag =
        (lang == Lang::Go).then(|| match probe(site, path, &["index", "--help"]) {
            Ok(help) => {
                if help.contains(MODULE_VERSION_FLAG) {
                    FlagCheck::Present
                } else {
                    FlagCheck::Absent
                }
            }
            Err(_) => FlagCheck::NotChecked,
        });
    IndexerCheck {
        lang,
        version,
        module_version_flag,
        linker: (lang == Lang::Rust).then_some(LinkerCheck::NotChecked),
        go_toolchain: None,
    }
}

/// The checkout enclosing the canonical `dir`: the OUTERMOST ancestor (`dir` included) holding
/// a `.git` entry (a directory, the file of a linked worktree, or anything a repository
/// planted to pose as one: a nearer entry never shrinks the forbidden root, astra ri3-1);
/// `dir` itself when none does, so a plain directory stays its own boundary (ri12-1).
/// Over-exclusion is safe, under-exclusion is the defect.
fn enclosing_checkout(dir: &Path) -> &Path {
    dir.ancestors()
        .filter(|ancestor| ancestor.join(".git").symlink_metadata().is_ok())
        .last()
        .unwrap_or(dir)
}

/// Run `<path> <args>` at `site` with the probe deadline and the production descriptor policy;
/// stdout and stderr joined (tools print `--version` on either).
pub(crate) fn probe(
    site: ProbeSite<'_>,
    path: &Path,
    args: &[&str],
) -> Result<String, &'static str> {
    let outcome = match site {
        ProbeSite::Unavailable => return Err("isolation_unavailable"),
        ProbeSite::View { helper, resources } => {
            run_tool_in_view(helper, path, args, PROBE_DEADLINE, resources)
                .map_err(|_| "indexer_failed")?
        }
        ProbeSite::Native => kaban_core::process::run(Spec {
            exec: Exec::Path(path.to_path_buf()),
            args: args.iter().map(Into::into).collect(),
            cwd: Path::new("/"),
            env: EnvAllowlist::new(),
            deadline: PROBE_DEADLINE,
            stdout_cap: PROBE_CAP,
            stderr_cap: PROBE_CAP,
            monitors: Vec::new(),
            join_bound: Duration::from_secs(1),
            inherited_fds: InheritedFds::Refuse,
        })
        .map_err(|_| "indexer_failed")?,
    };
    if outcome.killed.is_some() {
        return Err("indexer_timeout");
    }
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    if matches!(site, ProbeSite::View { .. })
        && outcome.status.and_then(|s| s.code()) == Some(i32::from(HELPER_EXIT))
        && stderr.lines().any(|l| l == SELFCHECK_FAILED_TEXT)
    {
        return Err("isolation_selfcheck_failed");
    }
    if !outcome.status.is_some_and(|status| status.success()) {
        return Err("indexer_failed");
    }
    let mut text = String::from_utf8_lossy(&outcome.stdout).into_owned();
    text.push_str(&stderr);
    Ok(text)
}

/// The first non-empty line of a captured output, scanned whole (U2 RU2-8: a hit past the
/// display bound still decides): a redact hit is masked, a reject hit becomes
/// `[removed: <rule id>]` (S5.7); the result is bounded to 200 characters.
#[must_use]
pub fn gate_output(policy: &Policy, text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    crate::runner::gate_line(policy, line)
        .chars()
        .take(LINE_CAP)
        .collect()
}

impl DoctorReport {
    /// The human text for stderr: config in effect, `overrides:`, languages and indexers, the
    /// tiers, the platform notes, then the trust state and identity result (CG4.6) and the
    /// fixed trust text. The binary passes every line through the policy before printing.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = vec![format!("config: {}", self.config_path.display())];
        lines.push(match &self.config {
            ConfigStatus::Ok => "config status: ok".to_owned(),
            ConfigStatus::Invalid { class, path } => {
                format!("config status: invalid ({class}: {path})")
            }
        });
        lines.push(format!(
            "index.isolation: {}",
            self.index.isolation.as_str()
        ));
        lines.push(format!("index.deadline_secs: {}", self.index.deadline_secs));
        lines.push(format!(
            "index.quota.file_bytes: {}",
            self.index.quota.file_bytes
        ));
        lines.push(format!(
            "index.quota.run_dir_bytes: {}",
            self.index.quota.run_dir_bytes
        ));
        lines.push(format!(
            "index.quota.tmp_bytes: {}",
            self.index.quota.tmp_bytes
        ));
        lines.push("overrides:".to_owned());
        lines.extend(self.overrides.iter().map(|(k, v)| format!("  {k}: {v}")));
        match &self.languages {
            None => {}
            Some(Err(class)) => lines.push(format!("languages: {class}")),
            Some(Ok(detected)) => {
                for d in detected {
                    lines.push(format!(
                        "language: {} (markers: {}) indexer: {}",
                        d.lang.name(),
                        d.markers.join(", "),
                        d.indexer
                    ));
                }
            }
        }
        for check in &self.indexers {
            let version = match &check.version {
                Ok(v) => v.clone(),
                Err(class) => format!("({class})"),
            };
            lines.push(format!("indexer {} version: {version}", check.lang.name()));
            if let Some(flag) = check.module_version_flag {
                let word = match flag {
                    FlagCheck::Present => "present",
                    FlagCheck::Absent => "absent",
                    FlagCheck::NotChecked => "not checked",
                };
                lines.push(format!("indexer go {MODULE_VERSION_FLAG}: {word}"));
            }
            if let Some(linker) = check.linker {
                lines.push(format!(
                    "indexer {} linker: {}",
                    check.lang.name(),
                    linker.as_str()
                ));
            }
            if let Some(row) = &check.go_toolchain {
                lines.push(row.clone());
            }
        }
        for probe in &self.isolation.probes {
            let state = match &probe.available {
                Ok(detail) => format!("available ({})", detail.0),
                Err(isolate::ReasonClass::NotInThisRelease) => {
                    "deferred (not_in_this_release)".to_owned()
                }
                Err(reason) => format!("unavailable ({})", reason.as_str()),
            };
            lines.push(format!("tier {}: {state}", probe.tier.as_str()));
        }
        match &self.isolation.selected {
            Ok(sel) => {
                lines.push(format!("tier selected: {} ({})", sel.tier, sel.detail));
                if sel.warning.is_some() {
                    lines.push(isolate::ISOLATION_NONE_TEXT.to_owned());
                }
            }
            Err(_) => lines.push(isolate::REQUIRED_REFUSED_TEXT.to_owned()),
        }
        let absent = |v: &Option<String>| v.clone().unwrap_or_else(|| "absent".to_owned());
        lines.push(format!("platform kernel: {}", self.platform.kernel));
        lines.push(format!(
            "platform unprivileged_userns_clone: {}",
            absent(&self.platform.unprivileged_userns_clone)
        ));
        lines.push(format!(
            "platform max_user_namespaces: {}",
            absent(&self.platform.max_user_namespaces)
        ));
        lines.push(format!(
            "platform unshare: {}",
            self.platform
                .unshare
                .as_ref()
                .map_or_else(|| "absent".to_owned(), |p| p.display().to_string())
        ));
        if let Some(t) = &self.trust {
            if let Some(id) = &t.repo_id {
                lines.push(format!("repo_id: {id}"));
            }
            lines.push(format!(
                "trust: {}",
                t.state.map_or_else(
                    || t.class.unwrap_or("unavailable").to_owned(),
                    |s| s.as_str().to_owned()
                )
            ));
            lines.push(format!("identity: {}", t.identity.as_str()));
            lines.push("trust text:".to_owned());
            lines.extend(TRUST_TEXT.iter().map(|l| format!("  {l}")));
        }
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}
