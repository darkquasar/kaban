//! The one content of a repository or ancestor `.cargo/config[.toml]` the producer refuses
//! (hd1-1), mirroring the consumer's `rust_cargo_config_redirects_backend`: a redirect of the
//! compiler backend kaban pins for the active target, which the fixed child environment does
//! not beat (a forced `[env]` entry, higher-priority linker search flags, the target's
//! `linker`). Everything else in the file stays the consumer's accepted content (DF-2).

use std::path::Path;

use anyhow::Result;
use toml::Value;

/// Refuse when a `.cargo/config[.toml]` of `workspace` or an ancestor redirects the compiler
/// backend for `target`; the message names the file, the table and the key in plain words.
pub(super) fn admit(workspace: &Path, target: &str) -> Result<()> {
    for dir in workspace.ancestors() {
        for name in [".cargo/config.toml", ".cargo/config"] {
            let path = dir.join(name);
            if !path.is_file() {
                continue;
            }
            let shown = path
                .strip_prefix(workspace)
                .unwrap_or(&path)
                .display()
                .to_string();
            // A file cargo cannot read either (unreadable, invalid TOML) hides no redirect
            // cargo would honour: it stays accepted content (DF-2) and fails in cargo's words.
            let Ok(table) = std::fs::read_to_string(&path)
                .map_err(|_| ())
                .and_then(|text| text.parse::<toml::Table>().map_err(|_| ()))
            else {
                continue;
            };
            if let Some(reason) = redirect(&table, target) {
                anyhow::bail!("{shown}: {reason}");
            }
        }
    }
    Ok(())
}

/// The first way `config` redirects the compiler backend for the active `target`, or `None`:
/// - `[env]` entries with `force = true` on a variable of the pin (`COMPILER_PATH`, `CC`,
///   `PATH`, `CARGO_TARGET_<TARGET>_LINKER`), and any entry on `LD_*` or `GCC_EXEC_PREFIX`
///   (unset by kaban, so an unforced entry takes effect too);
/// - `build.rustflags` or the active target's `rustflags` carrying `-C linker=`,
///   `-C link-self-contained` or a link argument `-B...`/`-specs...`;
/// - the active target's `linker`.
///
/// A `[target.'cfg(...)']` table may apply to the active target and counts as active; a table
/// of another triple is inert.
fn redirect(config: &toml::Table, target: &str) -> Option<String> {
    let darwin = target.ends_with("-apple-darwin");
    let linker_variable = format!(
        "CARGO_TARGET_{}_LINKER",
        target.to_ascii_uppercase().replace('-', "_")
    );
    let env = config.get("env").and_then(Value::as_table);
    for (name, entry) in env.into_iter().flatten() {
        let forced = entry
            .as_table()
            .and_then(|t| t.get("force"))
            .and_then(Value::as_bool)
            == Some(true);
        let pin = matches!(name.as_str(), "COMPILER_PATH" | "CC" | "PATH")
            || *name == linker_variable
            || (darwin
                && matches!(
                    name.as_str(),
                    "SDKROOT" | "DEVELOPER_DIR" | "CARGO" | "RUSTC" | "RUSTDOC"
                ));
        if pin && forced {
            return Some(format!(
                "[env] {name} with force = true redirects the compiler backend; remove it to index"
            ));
        }
        if name.starts_with("LD_")
            || name == "GCC_EXEC_PREFIX"
            || (darwin
                && (name.starts_with("DYLD_")
                    || matches!(
                        name.as_str(),
                        "TOOLCHAINS"
                            | "CCC_OVERRIDE_OPTIONS"
                            | "CPATH"
                            | "C_INCLUDE_PATH"
                            | "CPLUS_INCLUDE_PATH"
                            | "OBJC_INCLUDE_PATH"
                            | "LIBRARY_PATH"
                            | "CXX"
                            | "AR"
                            | "AS"
                            | "LD"
                            | "RUSTC_WRAPPER"
                            | "RUSTC_WORKSPACE_WRAPPER"
                            | "RUSTUP_TOOLCHAIN"
                    )))
        {
            return Some(format!(
                "[env] {name} redirects the compiler backend; remove it to index"
            ));
        }
    }
    if darwin
        && config
            .get("build")
            .and_then(Value::as_table)
            .is_some_and(|table| {
                ["rustc", "rustc-wrapper", "rustc-workspace-wrapper"]
                    .iter()
                    .any(|key| table.contains_key(*key))
            })
    {
        return Some(
            "[build] compiler proxy redirects the compiler backend; remove it to index".to_owned(),
        );
    }
    if let Some(flag) = config
        .get("build")
        .and_then(Value::as_table)
        .and_then(|b| b.get("rustflags"))
        .and_then(|value| backend_flag(value, darwin))
    {
        return Some(format!(
            "[build] rustflags carries {flag}, which redirects the compiler backend; remove it to index"
        ));
    }
    let targets = config.get("target").and_then(Value::as_table);
    for (name, table) in targets.into_iter().flatten() {
        if !(name == target || name.starts_with("cfg(")) {
            continue;
        }
        let Some(table) = table.as_table() else {
            continue;
        };
        if let Some(flag) = table
            .get("rustflags")
            .and_then(|value| backend_flag(value, darwin))
        {
            return Some(format!(
                "[target.{name}] rustflags carries {flag}, which redirects the compiler backend; remove it to index"
            ));
        }
        if table.contains_key("linker") {
            return Some(format!(
                "[target.{name}] linker redirects the compiler backend; remove it to index"
            ));
        }
    }
    None
}

/// The flag of a `rustflags` value (a string or an array of strings) that redirects the
/// compiler backend, spelled as cargo would pass it, or `None`.
fn backend_flag(value: &Value, darwin: bool) -> Option<String> {
    let tokens: Vec<String> = match value {
        Value::String(s) => s.split_whitespace().map(str::to_owned).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => return None,
    };
    let mut tokens = tokens.into_iter();
    while let Some(token) = tokens.next() {
        // `-C link-arg=...` arrives as two array elements or one string token
        let (prefix, option) = if matches!(token.as_str(), "-C" | "-Z") {
            (format!("{token} "), tokens.next()?)
        } else if let Some(rest) = token
            .strip_prefix("-C")
            .or_else(|| token.strip_prefix("-Z"))
        {
            (token[..2].to_owned(), rest.to_owned())
        } else {
            continue;
        };
        let redirects = option.starts_with("linker=")
            || option.starts_with("link-self-contained")
            || ["link-arg=", "link-args=", "pre-link-arg=", "pre-link-args="]
                .iter()
                .filter_map(|key| option.strip_prefix(key))
                .any(|args| {
                    args.split_whitespace().any(|arg| {
                        arg.starts_with("-B")
                            || arg.starts_with("-specs")
                            || arg.starts_with("--specs")
                            || (darwin
                                && [
                                    "-isysroot",
                                    "--sysroot",
                                    "-syslibroot",
                                    "-Wl,-syslibroot",
                                    "-fuse-ld=",
                                    "--ld-path=",
                                    "-resource-dir",
                                    "--config",
                                    "-Xclang",
                                    "-Xlinker",
                                    "@",
                                    "-Wl,@",
                                ]
                                .iter()
                                .any(|prefix| arg.starts_with(prefix)))
                    })
                });
        if redirects {
            return Some(format!("{prefix}{option}"));
        }
    }
    None
}
