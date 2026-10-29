//! Language detection and indexer resolution (stage1-spec G1-F20, G1-F7 `--indexer`,
//! G1-S5.3 "resolved indexer binary, absolute, verified at run start"; stage1-plan CG4.2).
//!
//! ```text
//!   languages(repo_root, &overrides) ==> Vec<Detected { lang, markers, indexer }>
//!     markers: Go `go.mod`; TypeScript `package.json` | `tsconfig.json`
//!     neither ==> Err(DetectError::UnsupportedInStage1)
//!   resolve_indexer(lang, override, path_env, forbidden) ==> ResolvedIndexer (canonical,
//!     admitted by `tools::admit_executable`: regular, owned by the user or root, not
//!     group/other writable, else `indexer_untrusted_mode`; not under the checkout or a
//!     forbidden root, else `indexer_untrusted_location`; the identity is kept for the spawn)
//!     `--indexer <lang>=<path>` must be absolute; else one PATH lookup at run start
//!   nested_workspaces(repo_root) ==> Vec<relative dir>: Cargo roots below the root workspace
//!     that its run never covers (backlog 17); named so the snapshot can say so
//! ```
//!
//! Errors are fixed classes; no path is ever part of an error text.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::tools::{self, ToolError, ToolIdentity};

/// The stage 1 languages (G1-F20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    /// `go.mod` at the root; indexer `scip-go`.
    Go,
    /// `package.json` or `tsconfig.json` at the root; indexer `scip-typescript`.
    TypeScript,
    /// Cargo package or workspace, indexed by the separate semantic producer.
    Rust,
}

impl Lang {
    /// Every stage 1 language in detection order.
    pub const ALL: [Lang; 3] = [Lang::Go, Lang::TypeScript, Lang::Rust];

    /// The fixed language name (`go`, `typescript`) used in `--indexer <lang>=<path>`, the
    /// log name `<commit40>.<lang>.log` and `meta.languages`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Go => "go",
            Self::TypeScript => "typescript",
            Self::Rust => "rust",
        }
    }

    /// The indexer executable name looked up on `PATH` when no override is given.
    #[must_use]
    pub fn indexer_name(self) -> &'static str {
        match self {
            Self::Go => "scip-go",
            Self::TypeScript => "scip-typescript",
            Self::Rust => "kaban-rust-scip",
        }
    }

    /// The root markers of G1-F20.
    #[must_use]
    pub fn markers(self) -> &'static [&'static str] {
        match self {
            Self::Go => &["go.mod"],
            Self::TypeScript => &["package.json", "tsconfig.json"],
            Self::Rust => &["Cargo.toml"],
        }
    }

    /// Parse the fixed name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.name() == name)
    }
}

/// Why detection or resolution refused. Classes only.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DetectError {
    /// No stage 1 marker at the repository root (`unsupported_in_stage_1`).
    #[error("unsupported_in_stage_1")]
    UnsupportedInStage1,
    /// `--indexer <lang>=<path>` is not `lang=path` with a known language.
    #[error("indexer override malformed")]
    OverrideMalformed,
    /// An override path is not absolute.
    #[error("indexer path not absolute")]
    NotAbsolute,
    /// The indexer was not found (override absent on disk, or no `PATH` entry holds it).
    #[error("indexer not found")]
    NotFound,
    /// Found, but not a regular file, owned by neither the user nor root, or writable by
    /// group or other (`indexer_untrusted_mode`): not used.
    #[error("indexer_untrusted_mode")]
    UntrustedMode,
    /// Found under the checkout or the kaban data directory (`indexer_untrusted_location`, U2
    /// RU2-3): a repository-controlled candidate never runs.
    #[error("indexer_untrusted_location")]
    UntrustedLocation,
}

/// A resolved indexer: the canonical path of the admitted object and its identity, which the
/// runner re-checks immediately before the spawn (U2 RU2-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIndexer {
    /// Canonical absolute path of the executable.
    pub path: PathBuf,
    /// Whether it came from `--indexer` (`true`) or the `PATH` lookup.
    pub from_override: bool,
    /// The identity at admission (device, inode, size, change time).
    pub identity: ToolIdentity,
}

/// One detected language with the markers that fired and the indexer, if one resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The language.
    pub lang: Lang,
    /// The marker files present at the root, in marker order.
    pub markers: Vec<&'static str>,
    /// The indexer, when found and trusted.
    pub indexer: Result<ResolvedIndexer, DetectErrorClass>,
}

/// A serialisable view of [`DetectError`] for `doctor` and the JSON report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectErrorClass {
    /// Not found.
    IndexerNotFound,
    /// Found but writable by group or other, not a regular file, or not the user's or root's.
    IndexerUntrustedMode,
    /// Found under the checkout or the data directory.
    IndexerUntrustedLocation,
    /// The override was malformed or relative.
    IndexerOverrideInvalid,
}

impl DetectErrorClass {
    /// The fixed class string (`doctor` text and the JSON report).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IndexerNotFound => "indexer_not_found",
            Self::IndexerUntrustedMode => "indexer_untrusted_mode",
            Self::IndexerUntrustedLocation => "indexer_untrusted_location",
            Self::IndexerOverrideInvalid => "indexer_override_invalid",
        }
    }
}

impl From<&DetectError> for DetectErrorClass {
    fn from(e: &DetectError) -> Self {
        match e {
            DetectError::NotFound | DetectError::UnsupportedInStage1 => Self::IndexerNotFound,
            DetectError::UntrustedMode => Self::IndexerUntrustedMode,
            DetectError::UntrustedLocation => Self::IndexerUntrustedLocation,
            DetectError::OverrideMalformed | DetectError::NotAbsolute => {
                Self::IndexerOverrideInvalid
            }
        }
    }
}

/// `--indexer <lang>=<path>` parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerOverride {
    /// The language.
    pub lang: Lang,
    /// The absolute path given.
    pub path: PathBuf,
}

impl IndexerOverride {
    /// Parse one `lang=path` argument.
    ///
    /// # Errors
    /// [`DetectError::OverrideMalformed`], [`DetectError::NotAbsolute`].
    pub fn parse(arg: &str) -> Result<Self, DetectError> {
        let (lang, path) = arg.split_once('=').ok_or(DetectError::OverrideMalformed)?;
        let lang = Lang::parse(lang).ok_or(DetectError::OverrideMalformed)?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(DetectError::NotAbsolute);
        }
        Ok(Self { lang, path })
    }
}

/// Detect the stage 1 languages at `repo_root` (G1-F20) and resolve each indexer: the override
/// for the language when given, else one `PATH` lookup over `path_env`. An indexer under
/// `repo_root` or under any of `forbidden` (the kaban data directory) is refused.
///
/// # Errors
/// [`DetectError::UnsupportedInStage1`] when no marker is present.
pub fn languages(
    repo_root: &Path,
    overrides: &[IndexerOverride],
    path_env: Option<&OsString>,
    forbidden: &[&Path],
) -> Result<Vec<Detected>, DetectError> {
    let mut roots: Vec<&Path> = vec![repo_root];
    roots.extend_from_slice(forbidden);
    let mut out = Vec::new();
    for lang in Lang::ALL {
        let markers: Vec<&'static str> = lang
            .markers()
            .iter()
            .copied()
            .filter(|m| repo_root.join(m).is_file())
            .collect();
        if markers.is_empty() {
            continue;
        }
        let configured = overrides
            .iter()
            .find(|o| o.lang == lang)
            .map(|o| o.path.as_path());
        let indexer = resolve_indexer(lang, configured, path_env, &roots).map_err(|e| (&e).into());
        out.push(Detected {
            lang,
            markers,
            indexer,
        });
    }
    if out.is_empty() {
        return Err(DetectError::UnsupportedInStage1);
    }
    Ok(out)
}

/// Resolve the indexer of `lang`: the configured absolute path, else the first `PATH` entry
/// holding `<indexer_name>` (absolute entries only; a relative `PATH` entry is skipped),
/// admitted through `tools::admit_executable`: a regular file owned by the user or root,
/// writable by neither group nor other, not under any of `forbidden`.
///
/// # Errors
/// [`DetectError::NotAbsolute`], [`DetectError::NotFound`], [`DetectError::UntrustedMode`],
/// [`DetectError::UntrustedLocation`].
pub fn resolve_indexer(
    lang: Lang,
    configured: Option<&Path>,
    path_env: Option<&OsString>,
    forbidden: &[&Path],
) -> Result<ResolvedIndexer, DetectError> {
    if let Some(path) = configured {
        if !path.is_absolute() {
            return Err(DetectError::NotAbsolute);
        }
        let admitted = admit(path, forbidden)?;
        return Ok(ResolvedIndexer {
            path: admitted.path,
            from_override: true,
            identity: admitted.identity,
        });
    }
    let path_env = path_env.ok_or(DetectError::NotFound)?;
    for entry in std::env::split_paths(path_env) {
        if !entry.is_absolute() {
            continue;
        }
        let candidate = entry.join(lang.indexer_name());
        if candidate.symlink_metadata().is_err() {
            continue;
        }
        // the first PATH hit decides: a hit in untrusted mode or location is reported, not
        // skipped over
        let admitted = admit(&candidate, forbidden)?;
        return Ok(ResolvedIndexer {
            path: admitted.path,
            from_override: false,
            identity: admitted.identity,
        });
    }
    Err(DetectError::NotFound)
}

/// The admission of G1-F7 through the verified-tool rule (U2 RU2-3).
fn admit(path: &Path, forbidden: &[&Path]) -> Result<tools::AdmittedTool, DetectError> {
    tools::admit_executable(path, forbidden).map_err(|e| match e {
        ToolError::NotFound => DetectError::NotFound,
        ToolError::UntrustedLocation => DetectError::UntrustedLocation,
        ToolError::UntrustedMode | ToolError::Changed => DetectError::UntrustedMode,
    })
}

/// The nested-workspace walk stops this many directories below the root.
const NESTED_MAX_DEPTH: usize = 6;
/// At most this many nested roots are named (a checkout of more is a monorepo of another kind).
const NESTED_MAX_ROOTS: usize = 32;
/// A manifest over this size is not read (no real `Cargo.toml` comes near it).
const MANIFEST_MAX_BYTES: u64 = 1 << 20;

/// The Cargo roots below `repo_root` that the root's own run never indexes (backlog 17): a
/// `Cargo.toml` that declares a `[workspace]` table (or a `[workspace.*]` sub-table) is a
/// workspace of its own, never a member of the root's; under a root that is a plain package
/// (no `[workspace]`) every nested manifest is its own root too. A nested package manifest
/// without a `[workspace]` table under a root workspace is taken to be one of its members
/// (Cargo refuses the layout otherwise; a package the root `exclude`s is the one shape this
/// misses). Bounded: nothing under a root already found, under `target/`, under a hidden
/// directory or behind a symlink is walked, at most [`NESTED_MAX_DEPTH`] directories deep, at
/// most [`NESTED_MAX_ROOTS`] roots. Relative directory paths, sorted; an unreadable directory
/// or manifest is skipped (it cannot be named). Never an error: the walk only informs.
#[must_use]
pub fn nested_workspaces(repo_root: &Path) -> Vec<PathBuf> {
    let root_is_workspace = declares_workspace(&repo_root.join("Cargo.toml"));
    let mut found = Vec::new();
    walk_nested(repo_root, Path::new(""), 1, root_is_workspace, &mut found);
    found.sort();
    found
}

fn walk_nested(
    repo_root: &Path,
    rel: &Path,
    depth: usize,
    root_is_workspace: bool,
    found: &mut Vec<PathBuf>,
) {
    if depth > NESTED_MAX_DEPTH || found.len() >= NESTED_MAX_ROOTS {
        return;
    }
    let Ok(entries) = fs::read_dir(repo_root.join(rel)) else {
        return;
    };
    // `DirEntry::file_type` does not follow symlinks: a linked directory is never walked
    let mut names: Vec<OsString> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name())
        .collect();
    names.sort();
    for name in names {
        if found.len() >= NESTED_MAX_ROOTS {
            return;
        }
        let text = name.to_string_lossy();
        if text.starts_with('.') || text == "target" {
            continue;
        }
        let child = rel.join(&name);
        let manifest = repo_root.join(&child).join("Cargo.toml");
        let is_manifest = manifest.symlink_metadata().is_ok_and(|m| m.is_file());
        if is_manifest && (!root_is_workspace || declares_workspace(&manifest)) {
            found.push(child);
            continue;
        }
        walk_nested(repo_root, &child, depth + 1, root_is_workspace, found);
    }
}

/// Whether the manifest at `path` has a top-level `[workspace]` table, by its header lines
/// (`[workspace]`, `[workspace.package]`, ...); a missing, oversized or unreadable file does not.
fn declares_workspace(path: &Path) -> bool {
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    if !meta.is_file() || meta.len() > MANIFEST_MAX_BYTES {
        return false;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().map(str::trim).any(|line| {
        line.strip_prefix("[workspace")
            .is_some_and(|rest| rest.starts_with(']') || rest.starts_with('.'))
    })
}
