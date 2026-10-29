//! The Go-only logic behind the language driver: the module cache admission (G1) and the
//! `go.mod` toolchain check (G2). The `go` and `toolchain` directives are read by a bounded
//! reader and compared with the version the admitted `go` answers; when the module cache
//! holds a `golang.org/toolchain@v0.0.1-go<ver>.<os>-<arch>` download that satisfies them,
//! its `bin/go` is admitted as the run's toolchain instead (first on the pinned `PATH`, the
//! child's `GOROOT`), so a repository newer than the machine's Go indexes with no user action.
//! Nothing here decides execution policy: the runner keeps `GO_INTERNAL` (`GOTOOLCHAIN=local`:
//! no download inside a run).

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::doctor::{ProbeSite, probe};
use crate::tools::{self, AdmittedTool, Toolchain};

/// The fixed class of a module cache that is not a narrow module cache directory outside the
/// checkout, the data directory and the home directory's ancestry ([`ModcacheError`]).
pub(crate) const GO_MODCACHE_REFUSED: &str = "go_modcache_refused";
/// The fixed class of a `go.mod` directive no admitted toolchain satisfies.
pub(crate) const GO_TOOLCHAIN_TOO_OLD: &str = "go_toolchain_too_old";
/// The fixed class of a `go version` answer that is not a release version.
pub(crate) const GO_VERSION_UNREADABLE: &str = "go_version_unreadable";
/// The fixed class of a `go.mod` whose directives could not be read ([`GoModError`]).
pub(crate) const GO_MOD_UNREADABLE: &str = "go_mod_unreadable";
/// `go.mod` is read whole up to this many bytes; a larger one is refused, not truncated.
const GO_MOD_BOUND: usize = 1 << 20;
/// The name prefix of Go's downloaded toolchains under `<GOMODCACHE>/golang.org/`.
const TOOLCHAIN_PREFIX: &str = "toolchain@v0.0.1-go";

/// Why the module cache was not admitted (the class is [`GO_MODCACHE_REFUSED`]): the reason in
/// plain words, for the refusal line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ModcacheError {
    /// A relative path, a symlink, a file, or an unresolvable path.
    #[error("is not an absolute, real directory")]
    NotDirectory,
    /// Under the checkout or the data directory.
    #[error("lies inside the repository or the kaban data directory")]
    Inside,
    /// Contains the checkout or the data directory (binding it would bind them).
    #[error("contains the repository or the kaban data directory")]
    Contains,
    /// `/`, the caller's home directory or one of its ancestors.
    #[error("is /, your home directory or one of its ancestors")]
    Home,
    /// A non-empty directory without the module cache layout.
    #[error(
        "does not look like a Go module cache (no cache/download directory and no <module>@<version> entry)"
    )]
    Layout,
}

/// The caller's Go module cache as a read-only input of every Go run (G1; the Rust registry's
/// rule, `rust::registry`): `GOMODCACHE`, else `$HOME/go/pkg/mod`; `Ok(None)` when neither is
/// set, the directory does not exist or it is empty (nothing to bind: the run gets a dedicated
/// empty cache). Admitted by [`admit_modcache`].
///
/// # Errors
/// [`ModcacheError`] (the class [`GO_MODCACHE_REFUSED`]).
pub(crate) fn modcache(repo: &Path, data: &Path) -> Result<Option<PathBuf>, ModcacheError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(path) = std::env::var_os("GOMODCACHE")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join("go/pkg/mod")))
    else {
        return Ok(None);
    };
    admit_modcache(&path, repo, data, home.as_deref())
}

/// Admit `path` as the module cache: an absolute, real (not a symlink) directory whose
/// canonical path is neither inside nor containing the checkout `repo` and the data directory
/// `data` (both directions, like a linker helper), is not `/`, `home` or an ancestor of `home`
/// (the view would see the home directory through it), and has the module cache layout
/// (`cache/download`, or a `<module>@<version>` entry within [`MODCACHE_SCAN_CAP`] entries).
/// `Ok(None)` when it does not exist or is empty.
///
/// # Errors
/// [`ModcacheError`].
fn admit_modcache(
    path: &Path,
    repo: &Path,
    data: &Path,
    home: Option<&Path>,
) -> Result<Option<PathBuf>, ModcacheError> {
    let meta = match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ModcacheError::NotDirectory),
        Ok(meta) => meta,
    };
    if !meta.is_dir() || meta.file_type().is_symlink() || !path.is_absolute() {
        return Err(ModcacheError::NotDirectory);
    }
    let path = path
        .canonicalize()
        .map_err(|_| ModcacheError::NotDirectory)?;
    if path == Path::new("/") {
        return Err(ModcacheError::Home);
    }
    if let Some(home) = home {
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        if home.starts_with(&path) {
            return Err(ModcacheError::Home);
        }
    }
    if path.starts_with(repo) || path.starts_with(data) {
        return Err(ModcacheError::Inside);
    }
    if repo.starts_with(&path) || data.starts_with(&path) {
        return Err(ModcacheError::Contains);
    }
    match modcache_layout(&path) {
        Layout::Empty => Ok(None),
        Layout::ModuleCache => Ok(Some(path)),
        Layout::Other => Err(ModcacheError::Layout),
    }
}

/// At most this many directory entries are read while looking for the module cache layout.
const MODCACHE_SCAN_CAP: usize = 4096;
/// `<cache>/<host>/<path...>/<module>@<version>`: the versioned entry is at most this deep.
const MODCACHE_SCAN_DEPTH: usize = 4;

/// What a directory offered as the module cache looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// No entry at all.
    Empty,
    /// `cache/download` or a `<module>@<version>` directory within the bounded scan.
    ModuleCache,
    /// Entries, none of them a module cache's.
    Other,
}

/// The bounded layout check: `cache/download` first (every cache `go` wrote has it), else a
/// breadth-first scan of at most [`MODCACHE_SCAN_CAP`] entries down to [`MODCACHE_SCAN_DEPTH`]
/// for a directory named `<module>@<version>`.
fn modcache_layout(cache: &Path) -> Layout {
    if cache.join("cache").join("download").is_dir() {
        return Layout::ModuleCache;
    }
    let mut seen = 0usize;
    let mut level: Vec<PathBuf> = vec![cache.to_path_buf()];
    for _ in 0..MODCACHE_SCAN_DEPTH {
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                seen += 1;
                if seen > MODCACHE_SCAN_CAP {
                    return Layout::Other;
                }
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                if is_dir && entry.file_name().to_string_lossy().contains('@') {
                    return Layout::ModuleCache;
                }
                if is_dir {
                    next.push(entry.path());
                }
            }
        }
        level = next;
    }
    if seen == 0 {
        Layout::Empty
    } else {
        Layout::Other
    }
}

/// A Go toolchain version as `go.mod` and `go version` spell it (`1.27`, `1.27rc1`, `1.27.0`,
/// `go1.27.1`), ordered as Go orders them (`internal/gover`): by major, minor, then the
/// language version (`1.27`, a bare minor from 1.21 on) below every pre-release, pre-releases
/// below the releases (`1.27 < 1.27beta1 < 1.27rc1 < 1.27.0 < 1.27.1`); before 1.21 a bare
/// minor is the release itself. The spelling is kept for messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoVersion {
    major: u32,
    minor: u32,
    release: Release,
    text: String,
}

/// What follows `major.minor`: nothing (the language version), a pre-release, or a patch
/// release; declared in Go's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Release {
    /// `1.27`: the language version, below every toolchain of that minor.
    Language,
    /// `1.27beta1`, `1.27rc2`.
    Pre {
        /// Alpha, beta or release candidate, in that order.
        kind: PreKind,
        /// The pre-release number.
        number: u32,
    },
    /// `1.27.0`, `1.27.1`.
    Patch(u32),
}

/// The pre-release kinds Go spells, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PreKind {
    Alpha,
    Beta,
    Rc,
}

impl GoVersion {
    /// Parse `1.27`, `1.27.1`, `go1.27.1`, `1.27rc1` or `1.27beta2` (digits only, no patch
    /// pre-releases, nothing trailing); `None` for anything else (`default`, `local`, a devel
    /// build, a cache directory name with its platform suffix: [`Self::parse_cached`]).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let body = text.strip_prefix("go").unwrap_or(text);
        let (major, rest) = cut_number(body)?;
        let rest = rest.strip_prefix('.')?;
        let (minor, rest) = cut_number(rest)?;
        let release = if rest.is_empty() {
            if minor < 21 {
                Release::Patch(0)
            } else {
                Release::Language
            }
        } else if let Some(rest) = rest.strip_prefix('.') {
            let (patch, rest) = cut_number(rest)?;
            rest.is_empty().then_some(Release::Patch(patch))?
        } else {
            let (kind, rest) = if let Some(rest) = rest.strip_prefix("alpha") {
                (PreKind::Alpha, rest)
            } else if let Some(rest) = rest.strip_prefix("beta") {
                (PreKind::Beta, rest)
            } else {
                (PreKind::Rc, rest.strip_prefix("rc")?)
            };
            let (number, rest) = cut_number(rest)?;
            rest.is_empty().then_some(Release::Pre { kind, number })?
        };
        Some(Self {
            major,
            minor,
            release,
            text: body.to_owned(),
        })
    }

    /// Parse the version part of a downloaded toolchain's directory name after
    /// [`TOOLCHAIN_PREFIX`]: `<version>.<goos>-<goarch>` (`1.27rc1.linux-arm64`); `None`
    /// without the platform suffix or with a version [`Self::parse`] refuses.
    #[must_use]
    pub fn parse_cached(name: &str) -> Option<Self> {
        let (version, platform) = name.rsplit_once('.')?;
        platform.contains('-').then(|| Self::parse(version))?
    }

    const fn key(&self) -> (u32, u32, Release) {
        (self.major, self.minor, self.release)
    }

    /// Whether this toolchain handles a file that says `go <required>`: `required` or newer
    /// in Go's order (a release candidate handles `go 1.27`, not `go 1.27.0`).
    #[must_use]
    pub fn satisfies(&self, required: &Self) -> bool {
        self.key() >= required.key()
    }
}

/// The leading decimal digits of `text` as a number, and the rest; `None` without digits.
fn cut_number(text: &str) -> Option<(u32, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, rest) = text.split_at(end);
    Some((digits.parse().ok()?, rest))
}

impl fmt::Display for GoVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Why the `go.mod` directives could not be read (the class [`GO_MOD_UNREADABLE`]): the
/// reason in plain words, for the doctor row and the refusal line.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum GoModError {
    /// The file exists but could not be opened or read.
    #[error("could not be read ({0})")]
    Unreadable(std::io::ErrorKind),
    /// Larger than [`GO_MOD_BOUND`] bytes: a directive after the bound would be missed.
    #[error("larger than 1 MiB")]
    Oversized,
    /// Not UTF-8.
    #[error("not UTF-8")]
    NotUtf8,
    /// A `go` or `toolchain` line whose argument is not what Go accepts.
    #[error("the directive '{0}' has no valid version")]
    Directive(String),
}

/// The toolchain `go.mod` asks for: the higher of its `go` and `toolchain` directives
/// (`//` comments dropped, space or tab separated, read like Go reads them: `go <version>`,
/// `toolchain go<version>` or `toolchain default`, which asks nothing). `Ok(None)` without
/// `go.mod` or without a directive.
///
/// # Errors
/// [`GoModError`]: the file could not be read whole, or a directive is malformed; absence is
/// never an error.
pub(crate) fn required_version(repo: &Path) -> Result<Option<GoVersion>, GoModError> {
    let file = match std::fs::File::open(repo.join("go.mod")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(GoModError::Unreadable(e.kind())),
        Ok(file) => file,
    };
    // one byte beyond the bound tells a bounded file from a cut one
    let mut bytes = Vec::new();
    file.take(GO_MOD_BOUND as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| GoModError::Unreadable(e.kind()))?;
    if bytes.len() > GO_MOD_BOUND {
        return Err(GoModError::Oversized);
    }
    let text = String::from_utf8(bytes).map_err(|_| GoModError::NotUtf8)?;
    let mut required: Option<GoVersion> = None;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        let mut words = line.split_whitespace();
        let version = match (words.next(), words.next(), words.next()) {
            (Some("go"), Some(arg), None) if !arg.starts_with("go") => GoVersion::parse(arg),
            (Some("toolchain"), Some("default"), None) => continue,
            (Some("toolchain"), Some(arg), None) if arg.starts_with("go") => GoVersion::parse(arg),
            (Some("go" | "toolchain"), _, _) => None,
            _ => continue,
        };
        let version = version.ok_or_else(|| GoModError::Directive(line.to_owned()))?;
        if required.as_ref().is_none_or(|r| version.key() > r.key()) {
            required = Some(version);
        }
    }
    Ok(required)
}

/// A Go toolchain admitted from the module cache: `golang.org/toolchain@v0.0.1-go<ver>.<os>-
/// <arch>` under the admitted cache (canonical, the toolchain layout), its `bin/go` a trusted
/// executable that answered `go version` at the probe site with a satisfying version.
#[derive(Debug, Clone)]
pub struct ModcacheToolchain {
    /// The toolchain root (the child's `GOROOT`; `bin` goes first on the pinned `PATH`).
    pub root: PathBuf,
    /// The admitted `bin/go`, rechecked by the runner immediately before every spawn.
    pub go: AdmittedTool,
    /// The version it answered.
    pub version: GoVersion,
}

/// The Go toolchain of a repository checked against its `go.mod` (G2): what doctor shows and
/// index decides (`type-enum-states`: the admitted module-cache toolchain exists exactly when
/// the installed one fell short and the cache held one that does not).
#[derive(Debug, Clone)]
pub(crate) enum ToolchainCheck {
    /// No `go` on `PATH`: nothing to compare (the indexer will say so).
    Absent,
    /// The `go` on `PATH` satisfies `go.mod`, or `go.mod` asks for nothing.
    Installed {
        /// Its version.
        version: GoVersion,
        /// What `go.mod` asks for.
        required: Option<GoVersion>,
    },
    /// The module cache holds a toolchain that satisfies `go.mod` where the installed one does
    /// not: the run uses it.
    FromModcache {
        /// The admitted toolchain.
        toolchain: ModcacheToolchain,
        /// The installed version that fell short.
        installed: GoVersion,
        /// What `go.mod` asks for.
        required: GoVersion,
    },
    /// Nothing satisfies `go.mod`: the run is refused before any spawn.
    TooOld {
        /// The installed version.
        installed: GoVersion,
        /// What `go.mod` asks for.
        required: GoVersion,
    },
    /// `go.mod` exists but its directives could not be read: nothing can be checked, the run
    /// is refused before any spawn.
    Unreadable {
        /// Why.
        reason: GoModError,
    },
}

impl ToolchainCheck {
    /// The doctor row and, for [`Self::TooOld`], the refusal line: versions only, in plain
    /// words with the action.
    #[must_use]
    pub(crate) fn row(&self) -> String {
        match self {
            Self::Absent => "go toolchain: no go on PATH".to_owned(),
            Self::Installed {
                version,
                required: None,
            } => format!("go toolchain: {version}, go.mod requires nothing"),
            Self::Installed {
                version,
                required: Some(required),
            } => format!("go toolchain: {version}, go.mod requires {required}"),
            Self::FromModcache {
                toolchain,
                installed,
                required,
            } => format!(
                "go toolchain: {} (module cache, installed {installed}), go.mod requires {required}",
                toolchain.version
            ),
            Self::TooOld {
                installed,
                required,
            } => format!(
                "go toolchain: {installed}, go.mod requires {required}: run 'GOTOOLCHAIN=auto go version' in the repository once (it downloads go {required} into the module cache), or install Go {required}"
            ),
            Self::Unreadable { reason } => {
                format!(
                    "go toolchain: go.mod unreadable: {reason}; fix go.mod (go mod tidy reads it the same way)"
                )
            }
        }
    }

    /// The fixed refusal class: [`GO_TOOLCHAIN_TOO_OLD`] for [`Self::TooOld`],
    /// [`GO_MOD_UNREADABLE`] for [`Self::Unreadable`], else `None`.
    #[must_use]
    pub(crate) const fn refusal(&self) -> Option<&'static str> {
        match self {
            Self::TooOld { .. } => Some(GO_TOOLCHAIN_TOO_OLD),
            Self::Unreadable { .. } => Some(GO_MOD_UNREADABLE),
            Self::Absent | Self::Installed { .. } | Self::FromModcache { .. } => None,
        }
    }

    /// The admitted module-cache toolchain, if the check selected one.
    #[must_use]
    pub(crate) fn into_modcache_toolchain(self) -> Option<ModcacheToolchain> {
        match self {
            Self::FromModcache { toolchain, .. } => Some(toolchain),
            Self::Absent
            | Self::Installed { .. }
            | Self::TooOld { .. }
            | Self::Unreadable { .. } => None,
        }
    }
}

/// Check the Go toolchain of `repo` (G2): the version of `chain`'s `go` probed at `site`
/// against [`required_version`]; when it falls short, the lowest satisfying toolchain of
/// `modcache` admitted by [`admit_modcache_toolchain`] under `forbidden`, if any.
///
/// # Errors
/// [`GO_VERSION_UNREADABLE`] when the installed `go` did not answer a release version; the
/// probe's own classes (`isolation_unavailable`, `indexer_failed`, `indexer_timeout`) when it
/// could not run.
pub(crate) fn check_toolchain(
    chain: Option<&Toolchain>,
    modcache: Option<&Path>,
    repo: &Path,
    forbidden: &[&Path],
    site: ProbeSite<'_>,
) -> Result<ToolchainCheck, &'static str> {
    let Some(chain) = chain else {
        return Ok(ToolchainCheck::Absent);
    };
    // go.mod before the probe: an unreadable file is answered without spawning anything
    let required = match required_version(repo) {
        Ok(required) => required,
        Err(reason) => return Ok(ToolchainCheck::Unreadable { reason }),
    };
    let installed = probe_version(site, &chain.bin_dir.join("go"))?;
    let Some(required) = required else {
        return Ok(ToolchainCheck::Installed {
            version: installed,
            required: None,
        });
    };
    if installed.satisfies(&required) {
        return Ok(ToolchainCheck::Installed {
            version: installed,
            required: Some(required),
        });
    }
    Ok(
        match modcache.and_then(|cache| modcache_toolchain(cache, &required, forbidden, site)) {
            Some(toolchain) => ToolchainCheck::FromModcache {
                toolchain,
                installed,
                required,
            },
            None => ToolchainCheck::TooOld {
                installed,
                required,
            },
        },
    )
}

/// The version `<go> version` answers at `site` (`go version go1.26.0 linux/arm64`).
fn probe_version(site: ProbeSite<'_>, go: &Path) -> Result<GoVersion, &'static str> {
    let out = probe(site, go, &["version"])?;
    out.split_whitespace()
        .nth(2)
        .and_then(GoVersion::parse)
        .ok_or(GO_VERSION_UNREADABLE)
}

/// The lowest toolchain under `<cache>/golang.org/` named for a version that satisfies
/// `required` and admitted by [`admit_modcache_toolchain`]; the cache is a resource of the
/// probe site (bound read-only inside the view, where the toolchain's `go` runs).
fn modcache_toolchain(
    cache: &Path,
    required: &GoVersion,
    forbidden: &[&Path],
    site: ProbeSite<'_>,
) -> Option<ModcacheToolchain> {
    let mut candidates: Vec<(GoVersion, PathBuf)> = std::fs::read_dir(cache.join("golang.org"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let version = GoVersion::parse_cached(name.to_str()?.strip_prefix(TOOLCHAIN_PREFIX)?)?;
            version.satisfies(required).then(|| (version, entry.path()))
        })
        .collect();
    candidates.sort_by_key(|(version, _)| version.key());
    let resources = [cache.to_path_buf()];
    let site = site.with_resources(&resources);
    candidates.into_iter().find_map(|(_, root)| {
        admit_modcache_toolchain(&root, cache, required, forbidden, site).ok()
    })
}

/// Admit one downloaded toolchain at `root`: canonical under `cache` with the Go root layout
/// (`VERSION`, `src/`), its executables admitted as a whole by [`admit_toolchain_tree`] under
/// `forbidden`, its `bin/go` answering `go version` at `site` with a version that satisfies
/// `required`.
fn admit_modcache_toolchain(
    root: &Path,
    cache: &Path,
    required: &GoVersion,
    forbidden: &[&Path],
    site: ProbeSite<'_>,
) -> Result<ModcacheToolchain, &'static str> {
    let root = std::fs::canonicalize(root).map_err(|_| GO_TOOLCHAIN_TOO_OLD)?;
    if !root.starts_with(cache) || !root.join("VERSION").is_file() || !root.join("src").is_dir() {
        return Err(GO_TOOLCHAIN_TOO_OLD);
    }
    let go = admit_toolchain_tree(&root, forbidden)?;
    let version = probe_version(site, &go.path)?;
    if !version.satisfies(required) {
        return Err(GO_TOOLCHAIN_TOO_OLD);
    }
    Ok(ModcacheToolchain { root, go, version })
}

/// At most this many entries under `bin/` and `pkg/tool/<host>/` are admitted (a release
/// holds about twenty); more is not a downloaded toolchain.
const TOOLCHAIN_ENTRY_CAP: usize = 256;

/// Admit the executables of the toolchain at the canonical `root` as a whole (gofix1-2): `go`
/// executes `bin/*` (first on the pinned `PATH`) and `pkg/tool/<host>/*` (`compile`, `link`,
/// `asm`, ...) from `GOROOT` by name, so every entry of those directories must be a regular
/// file admitted like `bin/go` itself ([`tools::admit_executable`] under `forbidden`: owner,
/// mode, no forbidden root) whose canonical path stays under `root`: no symlink leaving the
/// root, no subdirectory, no more than [`TOOLCHAIN_ENTRY_CAP`] entries. Returns the admitted
/// `bin/go`.
///
/// # Errors
/// [`GO_TOOLCHAIN_TOO_OLD`]: the cache holds no admissible toolchain for the directive.
fn admit_toolchain_tree(root: &Path, forbidden: &[&Path]) -> Result<AdmittedTool, &'static str> {
    let bin = root.join("bin");
    let mut dirs = vec![bin.clone()];
    if let Ok(hosts) = std::fs::read_dir(root.join("pkg").join("tool")) {
        for host in hosts {
            let host = host.map_err(|_| GO_TOOLCHAIN_TOO_OLD)?;
            if !host.file_type().is_ok_and(|kind| kind.is_dir()) {
                return Err(GO_TOOLCHAIN_TOO_OLD);
            }
            dirs.push(host.path());
        }
    }
    let mut seen = 0usize;
    let mut go = None;
    for dir in dirs {
        for entry in std::fs::read_dir(&dir).map_err(|_| GO_TOOLCHAIN_TOO_OLD)? {
            let entry = entry.map_err(|_| GO_TOOLCHAIN_TOO_OLD)?;
            seen += 1;
            if seen > TOOLCHAIN_ENTRY_CAP || entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                return Err(GO_TOOLCHAIN_TOO_OLD);
            }
            let tool = tools::admit_executable(&entry.path(), forbidden)
                .map_err(|_| GO_TOOLCHAIN_TOO_OLD)?;
            if !tool.path.starts_with(root) {
                return Err(GO_TOOLCHAIN_TOO_OLD);
            }
            if dir == bin && entry.file_name() == "go" {
                go = Some(tool);
            }
        }
    }
    go.ok_or(GO_TOOLCHAIN_TOO_OLD)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    // gofix1-4: Go's own ordering (internal/gover): 1.27 < 1.27rc1 < 1.27rc2 < 1.27.0 <
    // 1.27.1; a release candidate handles a `go 1.27` file but not a `go 1.27.0` one; before
    // 1.21 a missing patch is the release itself
    #[test]
    fn go_versions_order_like_go_does() {
        let v = |t: &str| GoVersion::parse(t).unwrap_or_else(|| panic!("{t}"));
        let order = [
            "1.26.5",
            "1.27",
            "1.27beta1",
            "1.27rc1",
            "1.27rc2",
            "1.27.0",
            "1.27.1",
            "1.28",
        ];
        for pair in order.windows(2) {
            let (lower, higher) = (v(pair[0]), v(pair[1]));
            assert!(higher.satisfies(&lower), "{higher} satisfies {lower}");
            assert!(
                !lower.satisfies(&higher),
                "{lower} does not satisfy {higher}"
            );
        }
        assert!(v("1.27rc1").satisfies(&v("1.27")), "rc handles `go 1.27`");
        assert!(
            !v("1.27rc1").satisfies(&v("1.27.0")),
            "rc does not handle `go 1.27.0`"
        );
        assert!(!v("1.27rc1").satisfies(&v("1.27rc2")));
        assert!(v("1.27.0").satisfies(&v("1.27rc2")));
        assert!(v("go1.27.1").satisfies(&v("1.27")));
        assert!(v("1.27.0").satisfies(&v("1.27.0")), "equal satisfies");
        assert!(!v("1.9").satisfies(&v("1.10")), "numeric, not lexical");
        assert!(v("1.20").satisfies(&v("1.20.0")) && v("1.20.0").satisfies(&v("1.20")));
        assert!(
            v("1.20").satisfies(&v("1.20rc1")),
            "before 1.21 the bare version is the release"
        );
        assert_eq!(v("go1.26.0").to_string(), "1.26.0");
        for bad in [
            "default",
            "local",
            "devel",
            "",
            "x.1",
            "1",
            "1.27.1.linux-arm64",
            "1.27rc",
            "1.27.x",
            "1.27.1rc1",
            "1.27 ",
            "1.27.1\n",
        ] {
            assert!(GoVersion::parse(bad).is_none(), "{bad:?}");
        }
    }

    // gofix1-4: the cache directory name carries the platform; the version is parsed apart
    // from it, pre-releases included
    #[test]
    fn cached_toolchain_names_separate_the_platform_suffix() {
        let cached = |t: &str| GoVersion::parse_cached(t).map(|v| v.to_string());
        assert_eq!(cached("1.27rc1.linux-arm64"), Some("1.27rc1".to_owned()));
        assert_eq!(cached("1.27.1.linux-arm64"), Some("1.27.1".to_owned()));
        assert_eq!(cached("1.27.0.darwin-amd64"), Some("1.27.0".to_owned()));
        assert!(
            GoVersion::parse_cached("1.27rc1.linux-arm64")
                .unwrap()
                .satisfies(&GoVersion::parse("1.27").unwrap())
        );
        for bad in ["1.27.1", "1.27rc1", "linux-arm64", "", "1.27.1.linux"] {
            assert_eq!(cached(bad), None, "{bad:?}");
        }
    }

    /// A scratch tree deleted on drop.
    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "kaban-go-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // gofix1-1: the cache root is narrow in both directions, never the home directory's
    // ancestry, and must look like a module cache before it is bound
    #[test]
    fn module_cache_admission_is_narrow() {
        let t = Tree::new("modcache");
        let base = std::fs::canonicalize(&t.0).unwrap();
        let home = t.dir("home");
        let repo = t.dir("home/src/repo");
        let data = t.dir("home/.local/kaban");
        let admit = |path: &Path| admit_modcache(path, &repo, &data, Some(&home));
        let legit = t.dir("home/go/pkg/mod/cache/download");
        let legit = legit.parent().unwrap().parent().unwrap().to_path_buf();
        assert_eq!(admit(&legit), Ok(Some(legit.clone())), "cache/download");
        let by_entry = t.dir("elsewhere/mod/github.com/org/dep@v1.2.3");
        let by_entry = by_entry.ancestors().nth(3).unwrap().to_path_buf();
        assert_eq!(
            admit(&by_entry),
            Ok(Some(by_entry.clone())),
            "a <module>@<version> entry"
        );
        assert_eq!(admit(&home), Err(ModcacheError::Home), "HOME");
        assert_eq!(
            admit(&base),
            Err(ModcacheError::Home),
            "an ancestor of HOME"
        );
        assert_eq!(admit(Path::new("/")), Err(ModcacheError::Home), "/");
        let other_home = t.dir("other");
        let parent = t.dir("other/src");
        let repo2 = t.dir("other/src/repo");
        assert_eq!(
            admit_modcache(&parent, &repo2, &data, Some(&other_home)),
            Err(ModcacheError::Contains),
            "the parent of the checkout"
        );
        assert_eq!(
            admit_modcache(
                &t.dir("other/src/repo/vendor"),
                &repo2,
                &data,
                Some(&other_home)
            ),
            Err(ModcacheError::Inside),
            "inside the checkout"
        );
        assert_eq!(
            admit(&t.dir("empty")),
            Ok(None),
            "an empty directory binds nothing"
        );
        let plain = t.dir("plain/docs");
        std::fs::write(plain.join("notes.txt"), "x").unwrap();
        assert_eq!(
            admit(plain.parent().unwrap()),
            Err(ModcacheError::Layout),
            "entries, none a module cache's"
        );
        assert_eq!(admit(&t.0.join("missing")), Ok(None), "absent");
        assert_eq!(
            admit(Path::new(".")),
            Err(ModcacheError::NotDirectory),
            "relative"
        );
    }

    // gofix1-2: every executable `go` runs from the toolchain is admitted, not only `bin/go`
    #[test]
    fn a_cached_toolchain_is_admitted_as_a_whole() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tree::new("toolchain");
        let repo = t.dir("repo");
        let root =
            std::fs::canonicalize(t.dir("mod/golang.org/toolchain@v0.0.1-go1.27.0.linux-arm64"))
                .unwrap();
        let exe = |path: &Path| {
            std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let bin = t.dir("mod/golang.org/toolchain@v0.0.1-go1.27.0.linux-arm64/bin");
        let host =
            t.dir("mod/golang.org/toolchain@v0.0.1-go1.27.0.linux-arm64/pkg/tool/linux_arm64");
        exe(&bin.join("go"));
        exe(&bin.join("gofmt"));
        exe(&host.join("compile"));
        exe(&host.join("link"));
        let forbidden: [&Path; 1] = [&repo];
        let admitted = admit_toolchain_tree(&root, &forbidden).unwrap();
        assert_eq!(
            admitted.path,
            root.join("bin").join("go"),
            "benign: admitted"
        );
        exe(&repo.join("evil"));
        std::fs::remove_file(host.join("compile")).unwrap();
        std::os::unix::fs::symlink(repo.join("evil"), host.join("compile")).unwrap();
        assert_eq!(
            admit_toolchain_tree(&root, &forbidden),
            Err(GO_TOOLCHAIN_TOO_OLD),
            "a helper linked into the checkout"
        );
        std::fs::remove_file(host.join("compile")).unwrap();
        std::os::unix::fs::symlink("/bin/sh", host.join("compile")).unwrap();
        assert_eq!(
            admit_toolchain_tree(&root, &forbidden),
            Err(GO_TOOLCHAIN_TOO_OLD),
            "a helper linked out of the root"
        );
        std::fs::remove_file(host.join("compile")).unwrap();
        std::os::unix::fs::symlink(bin.join("gofmt"), host.join("compile")).unwrap();
        assert!(
            admit_toolchain_tree(&root, &forbidden).is_ok(),
            "a link inside the root stays inside"
        );
        std::fs::remove_file(host.join("compile")).unwrap();
        exe(&host.join("compile"));
        std::fs::remove_file(bin.join("gofmt")).unwrap();
        std::os::unix::fs::symlink(repo.join("evil"), bin.join("gofmt")).unwrap();
        assert_eq!(
            admit_toolchain_tree(&root, &forbidden),
            Err(GO_TOOLCHAIN_TOO_OLD),
            "the rest of bin/ is on PATH too"
        );
        std::fs::remove_file(bin.join("gofmt")).unwrap();
        for n in 0..TOOLCHAIN_ENTRY_CAP {
            exe(&host.join(format!("t{n}")));
        }
        assert_eq!(
            admit_toolchain_tree(&root, &forbidden),
            Err(GO_TOOLCHAIN_TOO_OLD),
            "the entry cap"
        );
    }

    #[test]
    fn go_mod_directives_pick_the_higher_of_go_and_toolchain() {
        let t = Tree::new("gomod");
        let dir = t.0.clone();
        let read = |text: &[u8]| {
            std::fs::write(dir.join("go.mod"), text).unwrap();
            required_version(&dir).map(|v| v.map(|v| v.to_string()))
        };
        assert_eq!(
            read(
                b"module m\n\ngo 1.27.0 // comment\n\ntoolchain go1.27.1\n\nrequire (\n\tgolang.org/x/text v0.1.0\n)\n"
            ),
            Ok(Some("1.27.1".to_owned()))
        );
        assert_eq!(read(b"module m\n\ngo 1.22\n"), Ok(Some("1.22".to_owned())));
        assert_eq!(
            read(b"module m\n\ngo\t1.27.1\n"),
            Ok(Some("1.27.1".to_owned())),
            "gofix1-5: a tab separates like a space"
        );
        assert_eq!(
            read(b"module m\ntoolchain\t \tgo1.27rc1\n"),
            Ok(Some("1.27rc1".to_owned()))
        );
        assert_eq!(
            read(b"module m\n\ngo 1.22\ntoolchain default\n"),
            Ok(Some("1.22".to_owned()))
        );
        assert_eq!(read(b"module m\n"), Ok(None));
        assert_eq!(
            read("module m\n// caf\u{e9}\ngo 1.22\n".as_bytes()),
            Ok(Some("1.22".to_owned()))
        );
        for bad in [
            "go go1.22",
            "go 1.22 extra",
            "go",
            "toolchain 1.22",
            "toolchain go1.22.x",
        ] {
            assert_eq!(
                read(format!("module m\n{bad}\n").as_bytes()),
                Err(GoModError::Directive(bad.to_owned())),
                "{bad:?}"
            );
        }
        assert_eq!(read(b"module m\ngo 1.22\n\xff\n"), Err(GoModError::NotUtf8));
        // gofix1-6: one byte beyond the bound is oversized, whatever the bytes at the bound;
        // a directive after the bound is never silently missed
        let mut big = b"module m\n".to_vec();
        big.resize(GO_MOD_BOUND - 3, b' ');
        let mut crossing = big.clone();
        crossing.extend_from_slice("\u{e9}\n".as_bytes()); // the 2-byte char ends inside the bound
        assert_eq!(crossing.len(), GO_MOD_BOUND);
        assert_eq!(
            read(&crossing),
            Ok(None),
            "exactly the bound, a complete char"
        );
        crossing.extend_from_slice(b"go 1.30\n");
        assert_eq!(
            read(&crossing),
            Err(GoModError::Oversized),
            "a directive after the bound"
        );
        let mut straddling = big.clone();
        straddling.extend_from_slice("  \u{e9}\n".as_bytes()); // the char straddles the bound
        assert_eq!(straddling.len(), GO_MOD_BOUND + 2);
        assert_eq!(
            read(&straddling),
            Err(GoModError::Oversized),
            "not NotUtf8, not None"
        );
        std::fs::remove_file(dir.join("go.mod")).unwrap();
        assert_eq!(required_version(&dir), Ok(None), "no go.mod asks nothing");
    }
}
