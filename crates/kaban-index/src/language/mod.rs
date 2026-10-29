//! Fixed language drivers: argv and typed preparation, not execution policy.
//!
//! One [`Driver`] per language owns admission of what the producer needs beyond the indexer
//! (Rust: the pinned toolchain and the repository's declared profile), the cache mounts of
//! the namespaces view and the prepared invocations. Executable identity checks stay in
//! `tools`, execution and security enforcement in `runner`. Rust-only logic lives in
//! [`rust`]; Go's is the module cache admission below; TypeScript has none beyond its arms.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use kaban_core::process::InheritedFds;
use kaban_policy::Policy;

use crate::IndexError;
use crate::derive::Nodes;
use crate::detect::{Lang, ResolvedIndexer};
use crate::doctor;
use crate::isolate::Scratch;
use crate::runner::{GoRun, RunSlot};
use crate::tools::{self, AdmittedTool, Toolchain};
use crate::writer::Commit;

pub(crate) mod go;
pub(crate) mod rust;

pub use go::{GoVersion, ModcacheToolchain};

/// Mark the committed-test facts of the languages present in `nodes.files`: Rust reads the
/// gated committed source (`#[test]`, `#[cfg(test)]` scopes); Go and TypeScript carry SCIP
/// Test roles already and need nothing.
///
/// # Errors
/// [`IndexError::SourceClassification`] from the Rust classifier (a policy hit skips the
/// file's classification and counts, DF-3).
pub(crate) fn classify_committed_tests(
    nodes: &mut Nodes,
    repo: &Path,
    commit: &Commit,
    policy: &Policy,
    inherited_fds: InheritedFds,
) -> Result<(), IndexError> {
    if nodes
        .files
        .iter()
        .any(|file| file.language.eq_ignore_ascii_case("rust"))
    {
        rust::committed_tests::classify(nodes, repo, commit, policy, inherited_fds)?;
    }
    Ok(())
}

/// Why a language was not admitted: the fixed class and the line for the user (plain words:
/// what was found, what the repository asks for, the action).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The fixed class (`rust_*`, `go_modcache_refused`, `go_toolchain_too_old`, ...).
    pub class: &'static str,
    /// The stderr line.
    pub text: String,
}

impl Refusal {
    /// A Rust admission failure: the fixed Rust line under `class`.
    fn rust(class: &'static str) -> Self {
        Self {
            class,
            text: "Rust not admitted: the producer must be the qualified kaban-rust-scip and the toolchain on PATH the real Rust 1.99.0 (no rustup proxy); a rust-toolchain file's channel, components and targets must be installed; run doctor for the row".to_owned(),
        }
    }
}

/// What one language's runs are given beyond the indexer, admitted once per `index` run.
#[derive(Clone)]
pub enum Driver {
    /// scip-go: two runs, the user's module cache as a read-only input on both tiers (G1).
    Go {
        /// The caller's admitted module cache (`GOMODCACHE`, else `$HOME/go/pkg/mod`): the
        /// child's `GOMODCACHE` on both tiers, bound read-only at its host path on an
        /// isolating tier; `None` when the caller has none (a dedicated empty cache under the
        /// scratch then, and only the standard library resolves).
        modcache: Option<PathBuf>,
        /// The repository has `vendor/modules.txt` (`GOFLAGS=-mod=vendor`).
        vendored: bool,
        /// The module-cache toolchain admitted because the installed `go` falls short of
        /// `go.mod` (G2): first on the pinned `PATH`, the child's `GOROOT`; `None` when the
        /// installed one satisfies the directive.
        toolchain: Option<go::ModcacheToolchain>,
    },
    /// scip-typescript: one run, `--infer-tsconfig` without a `tsconfig.json`.
    TypeScript,
    /// kaban-rust-scip: the admitted real toolchain and the caller's Cargo registry.
    Rust {
        /// The sysroot of the pinned toolchain.
        root: PathBuf,
        /// `cargo`, `rustc`, the proc-macro server and `rustdoc`, in that order.
        executables: Box<[AdmittedTool; 4]>,
        /// The caller's admitted `CARGO_HOME/registry`, a read-only input of every Rust run
        /// (DF-2): bound read-only at its host path on an isolating tier, and linked from the
        /// scratch Cargo home on both tiers; `None` when the caller has none.
        registry: Option<PathBuf>,
        /// The admitted system C compiler cargo links with.
        linker: rust::Linker,
    },
}

/// One prepared producer run: its slot, the output file under `out/` and its argv.
pub struct Invocation {
    /// Run A, or Go's run B (`--skip-tests`).
    pub slot: RunSlot,
    /// The output file name under the scratch `out/`.
    pub file: &'static str,
    /// The producer's arguments.
    pub args: Vec<OsString>,
}

impl Driver {
    /// Admit what `lang` needs beyond the indexer; Go admits its module cache and checks the
    /// toolchain against `go.mod` (G1, G2); Rust checks the repository profile, the producer's
    /// capabilities, the pinned toolchain and the registry. Nothing runs before this passes.
    ///
    /// # Errors
    /// The fixed refusal class with its line (`go_modcache_refused`, `go_toolchain_too_old`,
    /// `rust_*`, `indexer_changed`, `isolation_unavailable`, ...).
    pub fn admit(
        lang: Lang,
        repo: &Path,
        indexer: &ResolvedIndexer,
        chain: Option<&Toolchain>,
        data: &Path,
        site: doctor::ProbeSite<'_>,
    ) -> Result<Self, Refusal> {
        match lang {
            Lang::Go => {
                let modcache = go::modcache(repo, data).map_err(|reason| Refusal {
                    class: go::GO_MODCACHE_REFUSED,
                    text: format!(
                        "the Go module cache (GOMODCACHE, else $HOME/go/pkg/mod) {reason}; refused: point GOMODCACHE at your module cache, or unset it"
                    ),
                })?;
                let check =
                    go::check_toolchain(chain, modcache.as_deref(), repo, &[repo, data], site)
                        .map_err(|class| Refusal {
                            class,
                            text: "the go on PATH did not answer `go version` with a release version; run doctor for the row".to_owned(),
                        })?;
                if let Some(class) = check.refusal() {
                    return Err(Refusal {
                        class,
                        text: check.row(),
                    });
                }
                Ok(Self::Go {
                    modcache,
                    vendored: repo.join("vendor").join("modules.txt").is_file(),
                    toolchain: check.into_modcache_toolchain(),
                })
            }
            Lang::TypeScript => Ok(Self::TypeScript),
            Lang::Rust => {
                rust::rust_repository_profile(repo).map_err(Refusal::rust)?;
                rust::rust_capabilities(indexer, site).map_err(Refusal::rust)?;
                let rust::AdmittedToolchain {
                    root,
                    executables,
                    linker,
                } = rust::admit_toolchain(chain, &[repo, data], site).map_err(Refusal::rust)?;
                rust::rust_repository_toolchain(repo, &root).map_err(Refusal::rust)?;
                let registry = rust::registry(repo, data).map_err(Refusal::rust)?;
                Ok(Self::Rust {
                    root,
                    executables,
                    registry,
                    linker,
                })
            }
        }
    }

    /// The executable directories a language puts first on the pinned `PATH`, before the
    /// indexer's and the installed toolchain's: the module-cache Go toolchain's `bin` (G2);
    /// none otherwise.
    #[must_use]
    pub fn tool_dirs(&self) -> Vec<PathBuf> {
        match self {
            Self::Go {
                toolchain: Some(toolchain),
                ..
            } => vec![toolchain.root.join("bin")],
            Self::Go { .. } | Self::TypeScript | Self::Rust { .. } => Vec::new(),
        }
    }

    /// The system compiler a language's runs link with: Rust's admitted `cc`, by its canonical
    /// path (the runner names it to cargo); `None` for Go and TypeScript.
    #[must_use]
    pub fn linker(&self) -> Option<&Path> {
        match self {
            Self::Rust { linker, .. } => Some(&linker.cc.path),
            Self::Go { .. } | Self::TypeScript => None,
        }
    }

    /// The Go inputs of a run as the runner spells them in the environment (G1, G2): the
    /// admitted module cache, the vendor mode, the module-cache toolchain's root and its
    /// admitted `bin/go` (the runner rechecks it at every spawn); the default for the other
    /// languages.
    #[must_use]
    pub fn go_run(&self) -> GoRun<'_> {
        match self {
            Self::Go {
                modcache,
                vendored,
                toolchain,
            } => GoRun {
                modcache: modcache.as_deref(),
                vendored: *vendored,
                goroot: toolchain.as_ref().map(|t| t.root.as_path()),
                toolchain: toolchain.as_ref().map(|t| &t.go),
            },
            Self::TypeScript | Self::Rust { .. } => GoRun::default(),
        }
    }

    /// The read-only resources of the view beyond the toolchain's: Go's admitted module cache
    /// (an input of every run, whatever the cache policy: G1, like the Rust registry of DF-2);
    /// the directory of the Rust linker's helpers (`cc1`, `collect2`), narrow like a toolchain
    /// symlink target, and the caller's Cargo registry; none for TypeScript.
    #[must_use]
    pub fn resource_dirs(&self) -> Vec<PathBuf> {
        match self {
            Self::Go { modcache, .. } => modcache.iter().cloned().collect(),
            Self::Rust {
                linker, registry, ..
            } => std::iter::once(linker.helper_dir.clone())
                .chain(registry.clone())
                .collect(),
            Self::TypeScript => Vec::new(),
        }
    }

    /// The kinds of read-only resources this language's runs were given, for the snapshot's
    /// provenance (`meta.resources`, gofix1-7): `modcache` and `toolchain` for Go (the
    /// admitted module cache, the module-cache toolchain), `registry` and `toolchain` for
    /// Rust (the caller's registry, the pinned toolchain root); none for TypeScript.
    #[must_use]
    pub fn resource_kinds(&self) -> Vec<&'static str> {
        match self {
            Self::Go {
                modcache,
                toolchain,
                ..
            } => modcache
                .iter()
                .map(|_| "modcache")
                .chain(toolchain.iter().map(|_| "toolchain"))
                .collect(),
            Self::Rust { registry, .. } => registry
                .iter()
                .map(|_| "registry")
                .chain(std::iter::once("toolchain"))
                .collect(),
            Self::TypeScript => Vec::new(),
        }
    }

    /// The runs of this language over `repo` at `commit`, their output under the scratch; Go's
    /// module-cache toolchain is rechecked by the runner at every spawn ([`GoRun::toolchain`]);
    /// Rust rechecks its admitted toolchain and linker and writes the prepared invocation
    /// first, with the scratch Cargo home's `registry` linked to the admitted host registry
    /// (bound read-only at that path inside an isolating view; the caller's own directory on
    /// `none`; DF-2), so a workspace with registry dependencies loads offline on both tiers.
    ///
    /// # Errors
    /// `rust_toolchain_refused`, `rust_linker_refused`, `rust_scratch_failed` or a profile
    /// refusal class.
    pub fn prepare(
        &self,
        repo: &Path,
        scratch: &Scratch,
        isolated: bool,
        commit: &str,
    ) -> Result<Vec<Invocation>, &'static str> {
        let child = if isolated {
            PathBuf::from("/scratch")
        } else {
            scratch.root().to_path_buf()
        };
        let mut base: Vec<OsString> = vec!["index".into()];
        match self {
            Self::Go { .. } => {
                base.extend(["-q".into(), "--module-version".into(), commit.into()]);
            }
            Self::TypeScript => {
                if !repo.join("tsconfig.json").is_file() {
                    base.push("--infer-tsconfig".into());
                }
            }
            Self::Rust {
                root,
                executables,
                registry,
                linker,
            } => {
                rust::rust_repository_profile(repo)?;
                for exe in executables {
                    tools::recheck(exe).map_err(|_| "rust_toolchain_refused")?;
                }
                tools::recheck(&linker.cc).map_err(|_| "rust_linker_refused")?;
                rust::rust_repository_toolchain(repo, root)?;
                let bytes = rust::invocation_config(root, executables, &linker.cc.path)?;
                scratch
                    .write_rust_invocation(&bytes, registry.as_deref())
                    .map_err(|_| "rust_scratch_failed")?;
                base.extend([
                    "--workspace".into(),
                    repo.as_os_str().to_owned(),
                    "--config".into(),
                    child.join("rust-prepared.json").into_os_string(),
                    "--threads".into(),
                    "4".into(),
                ]);
            }
        }
        let files = if matches!(self, Self::Go { .. }) {
            vec![
                (RunSlot::A, "index.scip"),
                (RunSlot::B, "index-notest.scip"),
            ]
        } else {
            vec![(RunSlot::A, "index.scip")]
        };
        Ok(files
            .into_iter()
            .map(|(slot, file)| {
                let mut args = base.clone();
                if slot == RunSlot::B {
                    args.push("--skip-tests".into());
                }
                args.extend([
                    "--output".into(),
                    child.join("out").join(file).into_os_string(),
                ]);
                Invocation { slot, file, args }
            })
            .collect())
    }
}
