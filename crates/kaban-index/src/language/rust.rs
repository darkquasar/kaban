//! The Rust-only logic behind the language driver: the producer capability probe, the
//! repository profile and selector checks ([`toolchain`]), the one toolchain admission
//! sequence `doctor` and `index` share ([`admit_toolchain`]), the host Cargo registry check,
//! the prepared invocation the producer reads and the committed-test classifier
//! ([`committed_tests`]). Nothing here decides execution policy: the runner keeps `env_for`,
//! `RUST_INTERNAL`, quotas and deadlines; identity checks stay in `tools`.

use std::path::{Path, PathBuf};

use crate::detect::ResolvedIndexer;
use crate::doctor::{ProbeSite, probe};
use crate::tools::{self, AdmittedTool, Toolchain};

pub(super) mod committed_tests;
mod toolchain;

/// The system C compiler cargo links with; a Rust toolchain resource, admitted like the others.
pub const SYSTEM_CC: &str = "/usr/bin/cc";
/// The fixed class of every linker admission failure.
const LINKER_REFUSED: &str = "rust_linker_refused";

/// The pinned toolchain once admitted: its sysroot and `cargo`, `rustc`, the proc-macro
/// server and `rustdoc`, in that order, and the system linker.
pub(crate) struct AdmittedToolchain {
    pub(crate) root: PathBuf,
    pub(crate) executables: Box<[AdmittedTool; 4]>,
    pub(crate) linker: Linker,
}

/// The system C compiler the pinned toolchain links with, admitted as one more toolchain
/// resource (never taken from `PATH` or the repository): cargo is told its canonical path,
/// the directory of its helpers (`cc1`, `collect2`) is bound read-only in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linker {
    /// The admitted canonical compiler (`CARGO_TARGET_<TRIPLE>_LINKER`).
    pub cc: AdmittedTool,
    /// The directory holding `cc1`, a read-only resource of the run; nothing under `/etc`.
    pub helper_dir: PathBuf,
}

/// Admit the system C compiler at `cc` (production: [`SYSTEM_CC`]): canonical under
/// `/usr/bin` and admitted by [`tools::admit_executable`] outside every `forbidden` root,
/// then probed at `site`: `-print-search-dirs` names its program prefixes and the first
/// holding a `cc1` admitted by [`linker_helper`] is the helper directory; with exactly that
/// canonical directory bound as a resource, `-print-prog-name=cc1` must resolve there and
/// `-print-file-name=crt1.o` to an admitted start object. One behaviour on both tiers.
///
/// # Errors
/// `rust_linker_refused` for every resolution and answer failure; the probe's own classes
/// (`isolation_unavailable`, `indexer_failed`, `indexer_timeout`) when it could not run.
pub fn rust_linker(
    cc: &Path,
    forbidden: &[&Path],
    site: ProbeSite<'_>,
) -> Result<Linker, &'static str> {
    let canonical = std::fs::canonicalize(cc).map_err(|_| LINKER_REFUSED)?;
    if !canonical.starts_with("/usr/bin") {
        return Err(LINKER_REFUSED);
    }
    let cc = tools::admit_executable(&canonical, forbidden).map_err(|_| LINKER_REFUSED)?;
    let search_dirs = probe(site, &cc.path, &["-print-search-dirs"])?;
    let helper_dir = search_dirs
        .lines()
        .find_map(|line| line.strip_prefix("programs: "))
        .ok_or(LINKER_REFUSED)?
        .trim_start_matches('=')
        .split(':')
        .find_map(|dir| linker_helper(&Path::new(dir).join("cc1"), forbidden).ok())
        .and_then(|cc1| cc1.parent().map(Path::to_path_buf))
        .ok_or(LINKER_REFUSED)?;
    let resources = [helper_dir.clone()];
    let site = site.with_resources(&resources);
    let cc1 = linker_helper(
        Path::new(probe(site, &cc.path, &["-print-prog-name=cc1"])?.trim()),
        forbidden,
    )?;
    if cc1 != helper_dir.join("cc1") {
        return Err(LINKER_REFUSED);
    }
    linker_object(
        Path::new(probe(site, &cc.path, &["-print-file-name=crt1.o"])?.trim()),
        forbidden,
    )?;
    Ok(Linker { cc, helper_dir })
}

/// The roots GCC installs its helpers under; a helper directory is a leaf strictly below one
/// (`/usr/libexec/gcc/<triple>/<version>`), never a root itself or its direct child.
const HELPER_ROOTS: [&str; 2] = ["/usr/libexec", "/usr/lib"];

/// Admit a linker program helper (`cc1`, `collect2`) named by a compiler `answer` at the same
/// boundary as `cc` (astra ri4-3bc-2): the canonical path of an existing regular file. Its
/// directory is what gets bound into the view, so the directory is validated as one GCC
/// helper leaf (astra ri3d-1): at least two components below `/usr/libexec` or `/usr/lib`
/// (never a root itself, which would bind all of it, nor `/usr`, `/usr/bin` or `/usr/local`),
/// and overlapping no `forbidden` root in either direction, since mounting the directory
/// exposes every descendant.
///
/// # Errors
/// `rust_linker_refused` for a relative, unresolvable, non-regular, misplaced or forbidden
/// answer.
pub fn linker_helper(answer: &Path, forbidden: &[&Path]) -> Result<PathBuf, &'static str> {
    let canonical = linker_file(answer, &HELPER_ROOTS, forbidden)?;
    let dir = canonical.parent().ok_or(LINKER_REFUSED)?;
    let below_root = |root: &&str| {
        dir.strip_prefix(root)
            .is_ok_and(|rest| rest.components().count() >= 2)
    };
    if !HELPER_ROOTS.iter().any(below_root) || forbidden.iter().any(|root| root.starts_with(dir)) {
        return Err(LINKER_REFUSED);
    }
    Ok(canonical)
}

/// Admit the linker's start object (`crt1.o`) named by a compiler `answer`: a canonical regular
/// file under `/usr/lib` outside every `forbidden` root. It is read through the `/usr/lib`
/// resource every view carries, never bound on its own, so no directory rule applies.
fn linker_object(answer: &Path, forbidden: &[&Path]) -> Result<PathBuf, &'static str> {
    linker_file(answer, &["/usr/lib"], forbidden)
}

/// The file half of helper admission: an absolute `answer` resolving to a regular file under
/// one of `roots` and under no `forbidden` root.
fn linker_file(
    answer: &Path,
    roots: &[&str],
    forbidden: &[&Path],
) -> Result<PathBuf, &'static str> {
    if !answer.is_absolute() {
        return Err(LINKER_REFUSED);
    }
    let canonical = std::fs::canonicalize(answer).map_err(|_| LINKER_REFUSED)?;
    if !canonical.is_file()
        || !roots.iter().any(|root| canonical.starts_with(root))
        || forbidden.iter().any(|root| canonical.starts_with(root))
    {
        return Err(LINKER_REFUSED);
    }
    Ok(canonical)
}

/// Admit the real toolchain `chain` found on `PATH`: its root, the four executables (never
/// under a `forbidden` root), `rustc -vV` at `site` with the chain's read-only resources, then
/// the system linker ([`rust_linker`]). `doctor` and `index` run this same sequence after
/// [`rust_repository_profile`] and check the repository's selector against the admitted root
/// themselves ([`rust_repository_toolchain`]), so that `doctor` can report the linker's
/// outcome when a selector requirement fails afterwards (astra ri4-3bc-3).
pub(crate) fn admit_toolchain(
    chain: Option<&Toolchain>,
    forbidden: &[&Path],
    site: ProbeSite<'_>,
) -> Result<AdmittedToolchain, &'static str> {
    let root = chain
        .and_then(|c| c.root.as_ref())
        .ok_or("rust_toolchain_refused")?;
    let admit = |name| {
        tools::admit_executable(&root.join(name), forbidden).map_err(|_| "rust_toolchain_refused")
    };
    let executables = [
        admit("bin/cargo")?,
        admit("bin/rustc")?,
        admit("libexec/rust-analyzer-proc-macro-srv")?,
        admit("bin/rustdoc")?,
    ];
    let resources = chain.map(Toolchain::resource_dirs).unwrap_or_default();
    rust_toolchain(&executables[1], site.with_resources(&resources))?;
    let linker = rust_linker(Path::new(SYSTEM_CC), forbidden, site)?;
    Ok(AdmittedToolchain {
        root: root.clone(),
        executables: Box::new(executables),
        linker,
    })
}

/// Probe the semantic Rust producer without loading a repository.
///
/// # Errors
/// Refuses unsupported protocol, semantic profile, versions or host capability.
pub fn rust_capabilities(
    resolved: &ResolvedIndexer,
    site: ProbeSite<'_>,
) -> Result<(), &'static str> {
    tools::recheck(&AdmittedTool {
        path: resolved.path.clone(),
        identity: resolved.identity,
    })
    .map_err(|_| "indexer_changed")?;
    check_rust_capabilities(&probe(site, &resolved.path, &["capabilities", "--json"])?)
}

/// Check the deliberately narrow prepared Cargo configuration profile without building.
///
/// # Errors
/// Refuses Cargo config and malformed, linked or ambiguous selectors. Toolchain
/// requirements must additionally pass [`rust_repository_toolchain`] before execution.
pub fn rust_repository_profile(repo: &Path) -> Result<(), &'static str> {
    toolchain::check(repo, None)
}

/// Check selector requirements against an already admitted real toolchain, without installing.
///
/// # Errors
/// Refuses unsupported selectors, version mismatch or unavailable components/targets.
pub fn rust_repository_toolchain(repo: &Path, sysroot: &Path) -> Result<(), &'static str> {
    toolchain::check(repo, Some(sysroot))
}

pub(crate) fn check_rust_capabilities(output: &str) -> Result<(), &'static str> {
    let value: serde_json::Value =
        serde_json::from_str(output).map_err(|_| "rust_capabilities_refused")?;
    let host = rust_host();
    if value["protocol_version"] != 2
        || value["semantic_profile"] != "kaban-rust-v1"
        || value["producer_version"] != "0.1.0-dev"
        || value["ra_ap_version"] != "0.0.358"
        || !value["supported_host_triples"]
            .as_array()
            .is_some_and(|hosts| hosts.iter().any(|v| v.as_str() == Some(host)))
    {
        return Err("rust_capabilities_refused");
    }
    Ok(())
}

/// Probe an admitted real Rust compiler for the pinned release and native host.
///
/// # Errors
/// Returns a fixed refusal if the compiler changed, failed, or is not the pinned toolchain.
pub fn rust_toolchain(compiler: &AdmittedTool, site: ProbeSite<'_>) -> Result<(), &'static str> {
    tools::recheck(compiler).map_err(|_| "toolchain_refused")?;
    let text = probe(site, &compiler.path, &["-vV"])?;
    if !text.lines().any(|l| l == "release: 1.99.0")
        || !text
            .lines()
            .any(|l| l.strip_prefix("host: ") == Some(rust_host()))
    {
        return Err("rust_toolchain_refused");
    }
    Ok(())
}

/// Native target supported by the prepared Rust contract, or an unsupported sentinel.
#[must_use]
pub const fn rust_host() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else {
        "unsupported"
    }
}

/// The cargo variable naming the host target's linker: `CARGO_TARGET_<TRIPLE>_LINKER` with
/// [`rust_host`] upper-cased and `-` as `_`.
pub(crate) fn linker_variable() -> String {
    format!(
        "CARGO_TARGET_{}_LINKER",
        rust_host().to_ascii_uppercase().replace('-', "_")
    )
}

/// The bytes of `rust-prepared.json` for the producer's `--config`: protocol v2, the native
/// host, the admitted `cargo`, `rustc` and proc-macro server paths, the sysroot and the
/// admitted canonical `linker` the producer's clean child passes to cargo.
pub(crate) fn invocation_config(
    root: &Path,
    executables: &[AdmittedTool; 4],
    linker: &Path,
) -> Result<Vec<u8>, &'static str> {
    let config = serde_json::json!({"protocol_version": 2, "target": rust_host(),
        "cargo": executables[0].path, "rustc": executables[1].path,
        "sysroot": root, "proc_macro_server": executables[2].path, "linker": linker});
    serde_json::to_vec(&config).map_err(|_| "rust_scratch_failed")
}

/// The caller's `CARGO_HOME/registry` (or `~/.cargo/registry`) when it is a real, absolute
/// directory outside the repository and the data directory; `None` when there is none.
pub(crate) fn registry(repo: &Path, data: &Path) -> Result<Option<PathBuf>, &'static str> {
    let Some(home) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cargo")))
    else {
        return Ok(None);
    };
    let path = home.join("registry");
    match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() && path.is_absolute() => {
            let path = path.canonicalize().map_err(|_| "rust_registry_refused")?;
            if path.starts_with(repo) || path.starts_with(data) {
                return Err("rust_registry_refused");
            }
            Ok(Some(path))
        }
        _ => Err("rust_registry_refused"),
    }
}
