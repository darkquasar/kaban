//! Read-only contract-v2 admission. This does not establish toolchain or loader qualification.

use std::{
    ffi::OsString,
    fs::{self, File},
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

const MAX_CONFIG_BYTES: u64 = 16 * 1024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    protocol_version: u32,
    target: String,
    cargo: PathBuf,
    rustc: PathBuf,
    sysroot: PathBuf,
    proc_macro_server: PathBuf,
    linker: PathBuf,
}

pub(super) struct Prepared {
    pub(super) workspace: PathBuf,
    pub(super) output: PathBuf,
    pub(super) target: String,
    pub(super) cargo: PathBuf,
    pub(super) rustc: PathBuf,
    pub(super) sysroot: PathBuf,
    pub(super) proc_macro_server: PathBuf,
    /// The canonical system C compiler cargo links with (`CARGO_TARGET_<TRIPLE>_LINKER`).
    pub(super) linker: PathBuf,
}

/// Check the bounded prepared invocation without executing tools or creating output.
pub(super) fn check(args: &[OsString]) -> Result<Prepared> {
    ensure!(
        args.len() == 9,
        "invalid prepared invocation: expected four option/value pairs"
    );
    let (mut workspace, mut output, mut config, mut threads) = (None, None, None, None);
    for pair in args[1..].as_chunks::<2>().0 {
        let slot = match pair[0].to_str() {
            Some("--workspace") => &mut workspace,
            Some("--output") => &mut output,
            Some("--config") => &mut config,
            Some("--threads") => &mut threads,
            _ => anyhow::bail!("invalid prepared invocation: unknown option"),
        };
        ensure!(
            slot.replace(&pair[1]).is_none(),
            "invalid prepared invocation: duplicate option"
        );
    }
    ensure!(
        threads.is_some_and(|value| value == "4"),
        "invalid prepared invocation: threads must be 4"
    );
    let workspace = Path::new(workspace.context("invalid prepared invocation: missing workspace")?);
    let output = Path::new(output.context("invalid prepared invocation: missing output")?);
    let config = Path::new(config.context("invalid prepared invocation: missing config")?);
    for path in [workspace, output, config] {
        absolute_normal_path(path)?;
    }

    // The repository's `.cargo/config[.toml]` and `rust-toolchain[.toml]` are the consumer's
    // concern (it admits their content and passes the resolved sysroot, whose own `cargo` runs
    // here: no rustup proxy, `RUSTUP_TOOLCHAIN` never set), except the one content the fixed
    // child environment cannot beat: a redirect of the compiler backend (`cargo_config`, hd1-1).
    // No child process is started here.
    let canonical_workspace = fs::canonicalize(workspace).context("workspace is unavailable")?;
    ensure!(canonical_workspace.is_dir(), "workspace is not a directory");
    ensure!(
        canonical_workspace.join("Cargo.toml").is_file(),
        "workspace Cargo.toml is unavailable"
    );

    let config = read_config(config)?;
    ensure!(
        config.protocol_version == 2,
        "unsupported prepared protocol version; expected 2"
    );
    ensure!(
        config.target == native_target()?,
        "unsupported target: v2 requires the native host target"
    );
    super::cargo_config::admit(&canonical_workspace, &config.target)?;
    let sysroot = directory(&config.sysroot).context("sysroot is unavailable")?;
    resource(&config.cargo).context("cargo resource is unavailable")?;
    resource(&config.rustc).context("rustc resource is unavailable")?;
    resource(&config.proc_macro_server).context("proc-macro server resource is unavailable")?;
    let linker =
        linker(&config.linker, &canonical_workspace).context("linker resource is unavailable")?;
    // Real-toolchain identity/version checks belong to the future loader. File
    // presence is deliberately not represented as proof of an executable or pin.
    for relative in [
        "lib/rustlib/src/rust/library/core/src/lib.rs",
        "lib/rustlib/src/rust/library/std/src/lib.rs",
    ] {
        resource(&sysroot.join(relative)).context("matching rust-src resources are unavailable")?;
    }
    Ok(Prepared {
        workspace: canonical_workspace,
        output: output.to_path_buf(),
        target: config.target,
        cargo: config.cargo,
        rustc: config.rustc,
        sysroot,
        proc_macro_server: config.proc_macro_server,
        linker,
    })
}

fn absolute_normal_path(path: &Path) -> Result<()> {
    let normalized: PathBuf = path.components().collect();
    ensure!(
        path.is_absolute()
            && normalized.as_os_str() == path.as_os_str()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
        "prepared paths must be absolute and normalized"
    );
    Ok(())
}

fn directory(path: &Path) -> Result<PathBuf> {
    absolute_normal_path(path)?;
    let path = fs::canonicalize(path)?;
    ensure!(path.is_dir(), "expected a directory");
    Ok(path)
}

fn resource(path: &Path) -> Result<()> {
    absolute_normal_path(path)?;
    let metadata = fs::metadata(path)?;
    ensure!(metadata.is_file(), "expected a regular resource file");
    Ok(())
}

/// The linker cargo is told: an existing regular executable named by its canonical path
/// (so no symlink such as `/usr/bin/cc` through `/etc/alternatives`) outside `workspace`,
/// the canonical checkout being indexed. Like the other resources it is not executed here.
fn linker(path: &Path, workspace: &Path) -> Result<PathBuf> {
    absolute_normal_path(path)?;
    let canonical = fs::canonicalize(path)?;
    ensure!(canonical == path, "linker must be a canonical path");
    let metadata = fs::metadata(&canonical)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "linker must be a regular executable"
    );
    ensure!(
        !canonical.starts_with(workspace),
        "linker must not be under the workspace"
    );
    Ok(canonical)
}

fn read_config(path: &Path) -> Result<Config> {
    resource(path).context("prepared configuration is unavailable")?;
    let mut bytes = Vec::new();
    File::open(path)
        .context("cannot open prepared configuration")?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("cannot read prepared configuration")?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "prepared configuration exceeds 16 KiB"
    );
    serde_json::from_slice(&bytes).context("invalid prepared configuration")
}

fn native_target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("aarch64", "linux") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("x86_64", "linux") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        _ => anyhow::bail!("unsupported producer host for prepared invocation v2"),
    }
}
