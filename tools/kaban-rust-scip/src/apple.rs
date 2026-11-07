//! Apple resources derived from the admitted canonical compiler, never caller environment.

use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub(super) struct Resources {
    pub(super) developer: PathBuf,
    pub(super) sdk: PathBuf,
    pub(super) bin: PathBuf,
}

pub(super) fn resources(linker: &Path, workspace: &Path) -> Result<Resources> {
    let developer = if linker == Path::new("/Library/Developer/CommandLineTools/usr/bin/clang") {
        PathBuf::from("/Library/Developer/CommandLineTools")
    } else {
        let developer = linker
            .to_str()
            .and_then(|p| p.strip_suffix("/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang"))
            .context("unsupported Apple compiler layout")?;
        let app = Path::new(
            developer
                .strip_suffix("/Contents/Developer")
                .context("unsupported Apple developer layout")?,
        );
        let name = app.strip_prefix("/Applications")?;
        ensure!(
            name.components().count() == 1 && name.extension().is_some_and(|e| e == "app"),
            "unsupported Apple developer location"
        );
        PathBuf::from(developer)
    };
    ensure!(
        !developer.starts_with(workspace) && !workspace.starts_with(&developer),
        "Apple resources overlap the workspace"
    );
    let bin = checked_directory(
        linker
            .parent()
            .context("missing Apple compiler directory")?,
        &developer,
    )?;
    for path in [linker.to_path_buf(), bin.join("ld"), bin.join("as")] {
        let meta = fs::metadata(&path)?;
        ensure!(
            path.canonicalize()?.starts_with(&developer)
                && meta.is_file()
                && meta.uid() == 0
                && meta.mode() & 0o022 == 0
                && meta.mode() & 0o111 != 0,
            "untrusted Apple compiler/backend resource"
        );
    }
    let sdk_base = if developer == Path::new("/Library/Developer/CommandLineTools") {
        developer.join("SDKs")
    } else {
        developer.join("Platforms/MacOSX.platform/Developer/SDKs")
    };
    let sdk = checked_directory(&sdk_base.join("MacOSX.sdk"), &developer)?;
    ensure!(
        sdk.starts_with(&sdk_base) && sdk.join("usr/lib/libSystem.tbd").is_file(),
        "Apple SDK resource is unavailable"
    );
    Ok(Resources {
        developer,
        sdk,
        bin,
    })
}

fn checked_directory(path: &Path, developer: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize()?;
    ensure!(
        canonical.starts_with(developer),
        "Apple resource escapes the developer directory"
    );
    for dir in canonical
        .ancestors()
        .take_while(|p| p.starts_with(developer))
    {
        let meta = dir.metadata()?;
        ensure!(
            meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o022 == 0,
            "untrusted Apple resource directory"
        );
    }
    Ok(canonical)
}
