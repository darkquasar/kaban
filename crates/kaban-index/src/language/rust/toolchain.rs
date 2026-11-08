//! Bounded declarative selector and Cargo configuration admission against an already pinned
//! toolchain.
//!
//! A repository `rust-toolchain.toml` is read by [`parse_selector`], a reader of the one shape
//! kaban accepts rather than a TOML implementation (no `toml` dependency: its `winnow` closure
//! is on the NEVER list): a single `[toolchain]` table with `channel` (string), `components`
//! and `targets` (single-line string arrays), `profile` (`minimal` or `default`), blank lines
//! and `#` comments. Everything else (other tables or keys, dotted or quoted keys, multi-line
//! arrays, string escapes, `path`, `profile = "complete"`) refuses with
//! `rust_toolchain_selector_unsupported`; the selector is never installed from.
//!
//! A repository or ancestor `.cargo/config[.toml]` is accepted (DF-2, owner decision): the
//! repository's build scripts already run arbitrary code inside the isolation, and every
//! setting kaban relies on (`CARGO_HOME`, `CARGO_TARGET_DIR`, the linker, offline, jobs,
//! incremental) reaches cargo as an environment variable, which beats the configuration file,
//! with one exception (hd1-1): the keys that redirect the pinned compiler backend for the
//! active target outrank that environment and refuse with
//! `rust_cargo_config_redirects_backend` ([`cargo_config_redirect`]); the producer mirrors the
//! check and names the file, the table and the key.

use std::io::Read;
use std::path::Path;

#[derive(Debug, Default, PartialEq, Eq)]
struct Requirements {
    channel: Option<String>,
    components: Vec<String>,
    targets: Vec<String>,
    profile: Profile,
}

/// rustup's install-set hint: not a toolchain requirement of its own, but `default` names
/// components the repository expects installed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// `minimal`, or no `profile` key: nothing beyond `components` and `targets`.
    #[default]
    Minimal,
    /// `default`: the host's `rustfmt-preview`, `clippy-preview` and `rust-docs` as well.
    Default,
}

impl Profile {
    fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "minimal" => Ok(Self::Minimal),
            "default" => Ok(Self::Default),
            _ => Err(UNSUPPORTED),
        }
    }

    /// The component names the profile implies, checked exactly like explicit `components`.
    const fn implied_components(self) -> &'static [&'static str] {
        match self {
            Self::Minimal => &[],
            Self::Default => &["rustfmt", "clippy", "rust-docs"],
        }
    }
}

/// What one accepted component name asks of the installation: its `lib/rustlib/components`
/// entry and the resource whose presence shows the entry was not left behind by a removal.
struct Component {
    manifest: String,
    resource: &'static str,
    /// `resource` is a directory (`rust-std`'s target library), else a file.
    directory: bool,
}

impl Component {
    /// `rustup`'s component vocabulary the subset accepts, on `host`; `rust-docs` is rustup's
    /// `share/doc/rust/html` tree.
    fn parse(name: &str, host: &str) -> Result<Self, &'static str> {
        let (manifest, resource, directory) = match name {
            "rust-src" => (
                "rust-src".to_owned(),
                "lib/rustlib/src/rust/library/core/src/lib.rs",
                false,
            ),
            "cargo" => (format!("cargo-{host}"), "bin/cargo", false),
            "rustc" => (format!("rustc-{host}"), "bin/rustc", false),
            "rust-std" => (format!("rust-std-{host}"), "", true),
            "rustfmt" => (format!("rustfmt-preview-{host}"), "bin/rustfmt", false),
            "clippy" => (format!("clippy-preview-{host}"), "bin/clippy-driver", false),
            "rust-docs" => (
                format!("rust-docs-{host}"),
                "share/doc/rust/html/index.html",
                false,
            ),
            _ => return Err("rust_toolchain_component_unsupported"),
        };
        Ok(Self {
            manifest,
            resource,
            directory,
        })
    }

    fn installed(&self, manifest: &str, root: &Path, host: &str) -> Result<(), &'static str> {
        let present = manifest.lines().any(|line| line == self.manifest)
            && if self.directory {
                root.join("lib/rustlib").join(host).join("lib").is_dir()
            } else {
                root.join(self.resource).is_file()
            };
        if present {
            Ok(())
        } else {
            Err("rust_toolchain_component_missing")
        }
    }
}

const UNSUPPORTED: &str = "rust_toolchain_selector_unsupported";
/// A target triple's grammar: ASCII letters, digits, `-` and `_` (`x86_64-unknown-linux-musl`),
/// non-empty, at most 128 bytes.
fn is_triple(triple: &str) -> bool {
    !triple.is_empty()
        && triple.len() <= 128
        && triple
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Any control character other than tab and the line terminators (`\n`, `\r\n`), comments
/// included: a bare `\r` is checked over the whole text before lines are split or comments
/// discarded, since `str::lines` would hide what follows it on the same line.
fn hides_text(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
        || text
            .split('\r')
            .skip(1)
            .any(|after| !after.starts_with('\n'))
}

pub(crate) fn check(repo: &Path, sysroot: Option<&Path>) -> Result<(), &'static str> {
    for ancestor in repo.ancestors() {
        let legacy = ancestor.join("rust-toolchain");
        let modern = ancestor.join("rust-toolchain.toml");
        let a = present(&legacy)?;
        let b = present(&modern)?;
        if a && b {
            return Err("rust_toolchain_selector_ambiguous");
        }
        if !a && !b {
            continue;
        }
        let path = if a { &legacy } else { &modern };
        let text = read(path)?;
        let requirements = if a && !text.trim_start().starts_with('[') {
            Requirements {
                channel: Some(text.trim().to_owned()),
                ..Requirements::default()
            }
        } else {
            parse_selector(&text)?
        };
        if let Some(root) = sysroot {
            satisfies(&requirements, root)?;
        }
    }
    for ancestor in repo.ancestors() {
        cargo_config_admits(ancestor, super::rust_host())?;
    }
    Ok(())
}

/// The refusal class of a `.cargo/config[.toml]` that redirects the pinned compiler backend.
const CARGO_CONFIG_REDIRECTS: &str = "rust_cargo_config_redirects_backend";

/// The one thing a repository or ancestor `.cargo/config[.toml]` may not do (hd1-1): redirect
/// the compiler backend kaban pins for the active target, which cargo's environment does not
/// beat. Everything else in the file stays accepted (DF-2).
fn cargo_config_admits(dir: &Path, target: &str) -> Result<(), &'static str> {
    for name in [".cargo/config.toml", ".cargo/config"] {
        let path = dir.join(name);
        if !present(&path)? {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|_| "rust_config_unreadable")?;
        if cargo_config_redirect(&text, target).is_some() {
            return Err(CARGO_CONFIG_REDIRECTS);
        }
    }
    Ok(())
}

/// The first way `text` (a `.cargo/config[.toml]`) redirects the compiler backend for the
/// active `target`, in plain words naming the table and the key, or `None`:
/// - `[env]` entries with `force = true` on a variable of the pin (`COMPILER_PATH`, `CC`,
///   `PATH`, `CARGO_TARGET_<TARGET>_LINKER`), and any entry on `LD_*` or `GCC_EXEC_PREFIX`
///   (unset by kaban, so an unforced entry takes effect too);
/// - `build.rustflags` or the active target's `rustflags` carrying `-C linker=`,
///   `-C link-self-contained` or a link argument `-B...`/`-specs...`;
/// - the active target's `linker`.
///
/// A `[target.'cfg(...)']` table may apply to the active target and counts as active; a table
/// of another triple is inert and accepted. Read without a TOML implementation (module doc):
/// headers, dotted keys, quoted keys, inline tables, strings and arrays over several lines.
fn cargo_config_redirect(text: &str, target: &str) -> Option<String> {
    let darwin = target.ends_with("-apple-darwin");
    let linker_variable = format!(
        "CARGO_TARGET_{}_LINKER",
        target.to_ascii_uppercase().replace('-', "_")
    );
    let mut table: Vec<String> = Vec::new();
    for statement in toml_statements(text) {
        let (key, value) = match statement {
            TomlStatement::Header(path) => {
                table = path;
                continue;
            }
            TomlStatement::Pair(key, value) => (key, value),
        };
        let path: Vec<&str> = table.iter().chain(key.iter()).map(String::as_str).collect();
        match path.as_slice() {
            ["env", name] | ["env", name, "force"] => {
                let forced = if path.len() == 3 {
                    value.trim() == "true"
                } else {
                    inline_table_entry(&value, "force").is_some_and(|v| v == "true")
                };
                let pin = matches!(*name, "COMPILER_PATH" | "CC" | "PATH")
                    || *name == linker_variable
                    || (darwin
                        && matches!(
                            *name,
                            "SDKROOT" | "DEVELOPER_DIR" | "CARGO" | "RUSTC" | "RUSTDOC"
                        ));
                if pin && forced {
                    return Some(format!(
                        "[env] {name} with force = true redirects the compiler backend; remove it to index"
                    ));
                }
                if name.starts_with("LD_")
                    || *name == "GCC_EXEC_PREFIX"
                    || (darwin
                        && (name.starts_with("DYLD_")
                            || matches!(
                                *name,
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
            [
                "build",
                "rustc" | "rustc-wrapper" | "rustc-workspace-wrapper",
            ] if darwin => {
                return Some(
                    "[build] compiler proxy redirects the compiler backend; remove it to index"
                        .to_owned(),
                );
            }
            ["build", "rustflags"] => {
                if let Some(flag) = backend_flag(&value, darwin) {
                    return Some(format!(
                        "[build] rustflags carries {flag}, which redirects the compiler backend; remove it to index"
                    ));
                }
            }
            ["target", name, "rustflags"] if *name == target || name.starts_with("cfg(") => {
                if let Some(flag) = backend_flag(&value, darwin) {
                    return Some(format!(
                        "[target.{name}] rustflags carries {flag}, which redirects the compiler backend; remove it to index"
                    ));
                }
            }
            ["target", name, "linker"] if *name == target || name.starts_with("cfg(") => {
                return Some(format!(
                    "[target.{name}] linker redirects the compiler backend; remove it to index"
                ));
            }
            _ => {}
        }
    }
    None
}

/// The flag of a `rustflags` value (a string or an array of strings) that redirects the
/// compiler backend, spelled as cargo would pass it, or `None`.
fn backend_flag(value: &str, darwin: bool) -> Option<String> {
    let value = value.trim();
    let tokens: Vec<String> = if value.starts_with('[') {
        quoted_strings(value)
    } else {
        string(value)
            .map(|(s, _)| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
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

/// One TOML statement with its comment removed: a table header's path or a key's path and
/// its value text (possibly gathered over several lines).
#[derive(Debug, PartialEq, Eq)]
enum TomlStatement {
    Header(Vec<String>),
    Pair(Vec<String>, String),
}

/// The statements of a TOML text, read line by line: comments stripped outside strings, a
/// value whose brackets are still open continued on the following lines. Not a TOML
/// implementation: what it cannot read it passes over, and the producer's full parser refuses.
fn toml_statements(text: &str) -> Vec<TomlStatement> {
    let mut statements = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        let line = strip_comment(line);
        let line = line.trim_matches(WS);
        if line.is_empty() && pending.is_empty() {
            continue;
        }
        if !pending.is_empty() {
            pending.push(' ');
        }
        pending.push_str(line);
        if bracket_depth(&pending) > 0 {
            continue;
        }
        let statement = std::mem::take(&mut pending);
        if let Some(inner) = statement.strip_prefix('[') {
            let inner = inner.trim_start_matches('[');
            let inner = inner.trim_end_matches(WS).trim_end_matches(']');
            statements.push(TomlStatement::Header(key_path(inner)));
        } else if let Some(at) = split_point(&statement, '=') {
            statements.push(TomlStatement::Pair(
                key_path(&statement[..at]),
                statement[at + 1..].trim_matches(WS).to_owned(),
            ));
        }
    }
    statements
}

/// A dotted key's segments, quotes removed.
fn key_path(key: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut rest = key.trim_matches(WS);
    while let Some(at) = split_point(rest, '.') {
        segments.push(unquote(&rest[..at]));
        rest = &rest[at + 1..];
    }
    segments.push(unquote(rest));
    segments
}

fn unquote(segment: &str) -> String {
    let segment = segment.trim_matches(WS);
    string(segment).map_or_else(|_| segment.to_owned(), |(s, _)| s)
}

/// The value of `key` in an inline table `{ a = 1, key = value }`, as text.
fn inline_table_entry(value: &str, key: &str) -> Option<String> {
    let inner = value
        .trim_matches(WS)
        .strip_prefix('{')?
        .trim_end_matches(WS)
        .strip_suffix('}')?;
    let mut rest = inner;
    loop {
        let (entry, after) = match split_point(rest, ',') {
            Some(at) => (&rest[..at], Some(&rest[at + 1..])),
            None => (rest, None),
        };
        if let Some(at) = split_point(entry, '=')
            && key_path(&entry[..at]) == [key]
        {
            return Some(entry[at + 1..].trim_matches(WS).to_owned());
        }
        rest = after?;
    }
}

/// Every quoted string of `text`, in order (the elements of an array of strings).
fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(['"', '\'']) {
        match string(&rest[start..]) {
            Ok((value, after)) => {
                out.push(value);
                rest = after;
            }
            Err(_) => break,
        }
    }
    out
}

/// The first `needle` of `text` outside quotes, brackets and braces.
fn split_point(text: &str, needle: char) -> Option<usize> {
    let mut quote = None;
    let mut depth = 0i32;
    for (i, c) in text.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ if c == needle && depth == 0 => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// `text` without its `#` comment (a `#` inside a string is content).
fn strip_comment(text: &str) -> &str {
    let mut quote = None;
    for (i, c) in text.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            None if c == '#' => return &text[..i],
            None if matches!(c, '"' | '\'') => quote = Some(c),
            Some(_) | None => {}
        }
    }
    text
}

/// Open brackets and braces of `text` outside strings, net of closed ones.
fn bracket_depth(text: &str) -> i32 {
    let mut quote = None;
    let mut depth = 0i32;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ => {}
            },
        }
    }
    depth
}

/// TOML whitespace: space and tab only (`str::trim` would also strip Unicode whitespace,
/// which is not TOML).
const WS: [char; 2] = [' ', '\t'];

/// The accepted selector subset (module doc), or [`UNSUPPORTED`] ([`hides_text`] refuses
/// first).
fn parse_selector(text: &str) -> Result<Requirements, &'static str> {
    if hides_text(text) {
        return Err(UNSUPPORTED);
    }
    let mut lines = text
        .lines()
        .map(|l| l.trim_matches(WS))
        .filter(|l| !is_blank(l));
    let header = lines.next().ok_or(UNSUPPORTED)?;
    rest_is_blank(header.strip_prefix("[toolchain]").ok_or(UNSUPPORTED)?)?;
    let mut requirements = Requirements::default();
    let mut seen = Vec::new();
    for line in lines {
        let (key, value) = line.split_once('=').ok_or(UNSUPPORTED)?;
        let key = key.trim_end_matches(WS);
        if !key.bytes().all(|b| b.is_ascii_lowercase()) || seen.contains(&key) {
            return Err(UNSUPPORTED);
        }
        seen.push(key);
        let value = value.trim_start_matches(WS);
        match key {
            "channel" => {
                let (channel, rest) = string(value)?;
                requirements.channel = Some(channel);
                rest_is_blank(rest)?;
            }
            "components" => requirements.components = string_array(value)?,
            "targets" => requirements.targets = string_array(value)?,
            "profile" => {
                let (profile, rest) = string(value)?;
                requirements.profile = Profile::parse(&profile)?;
                rest_is_blank(rest)?;
            }
            _ => return Err(UNSUPPORTED),
        }
    }
    Ok(requirements)
}

fn is_blank(line: &str) -> bool {
    line.is_empty() || line.starts_with('#')
}

fn rest_is_blank(rest: &str) -> Result<(), &'static str> {
    if is_blank(rest.trim_start_matches(WS)) {
        Ok(())
    } else {
        Err(UNSUPPORTED)
    }
}

/// One quoted string without escapes at the start of `text`; returns it and what follows.
fn string(text: &str) -> Result<(String, &str), &'static str> {
    let quote = text
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\''))
        .ok_or(UNSUPPORTED)?;
    let body = &text[1..];
    let end = body.find(quote).ok_or(UNSUPPORTED)?;
    let value = &body[..end];
    if quote == '"' && value.contains('\\') {
        return Err(UNSUPPORTED);
    }
    Ok((value.to_owned(), &body[end + 1..]))
}

/// A single-line `[ "a", 'b', ]` of strings, optionally followed by a comment.
fn string_array(text: &str) -> Result<Vec<String>, &'static str> {
    let mut rest = text
        .strip_prefix('[')
        .ok_or(UNSUPPORTED)?
        .trim_start_matches(WS);
    let mut out = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix(']') {
            rest_is_blank(after)?;
            return Ok(out);
        }
        let (value, after) = string(rest)?;
        out.push(value);
        rest = after.trim_start_matches(WS);
        match rest.strip_prefix(',') {
            Some(after) => rest = after.trim_start_matches(WS),
            None if rest.starts_with(']') => {}
            None => return Err(UNSUPPORTED),
        }
    }
}

fn present(path: &Path) -> Result<bool, &'static str> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("rust_config_unreadable"),
    }
}

fn read(path: &Path) -> Result<String, &'static str> {
    let meta = path
        .symlink_metadata()
        .map_err(|_| "rust_config_unreadable")?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 16 * 1024 {
        return Err(UNSUPPORTED);
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .map_err(|_| "rust_config_unreadable")?
        .take(16 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "rust_config_unreadable")?;
    if text.len() > 16 * 1024 {
        return Err(UNSUPPORTED);
    }
    Ok(text)
}

fn satisfies(request: &Requirements, root: &Path) -> Result<(), &'static str> {
    if request
        .channel
        .as_deref()
        .is_some_and(|channel| channel != "1.99.0")
    {
        return Err("rust_toolchain_channel_mismatch");
    }
    if request.components.len() > 64 || request.targets.len() > 64 {
        return Err(UNSUPPORTED);
    }
    let host = super::rust_host();
    let installed = read(&root.join("lib/rustlib/components"))?;
    let implied = request.profile.implied_components().iter().copied();
    for name in implied.chain(request.components.iter().map(String::as_str)) {
        Component::parse(name, host)?.installed(&installed, root, host)?;
    }
    for target in &request.targets {
        if !is_triple(target) {
            return Err("rust_toolchain_target_unsupported");
        }
        if !installed
            .lines()
            .any(|line| line == format!("rust-std-{target}"))
            || !root.join("lib/rustlib").join(target).join("lib").is_dir()
        {
            return Err("rust_toolchain_target_missing");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: failures are the assertion

    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn selector_accepts_the_one_toolchain_table_with_channel_components_and_targets() {
        let parsed = parse_selector(
            "# pinned\n\n[toolchain]\nchannel = \"1.99.0\" # the release\ncomponents = ['rust-src', \"clippy\",]\ntargets = []\n",
        )
        .unwrap();
        assert_eq!(
            parsed,
            Requirements {
                channel: Some("1.99.0".to_owned()),
                components: strings(&["rust-src", "clippy"]),
                targets: Vec::new(),
                profile: Profile::Minimal,
            }
        );
        assert_eq!(
            parse_selector("[toolchain]\n").unwrap(),
            Requirements::default()
        );
        assert_eq!(
            parse_selector("[toolchain]\ntargets=[ 'x' ]").unwrap(),
            Requirements {
                targets: strings(&["x"]),
                ..Requirements::default()
            }
        );
        assert_eq!(
            parse_selector("[toolchain]\nprofile = \"minimal\" # install hint\n").unwrap(),
            Requirements::default()
        );
        assert_eq!(
            parse_selector("[toolchain]\nprofile = 'default'\n").unwrap(),
            Requirements {
                profile: Profile::Default,
                ..Requirements::default()
            }
        );
        // a trailing comment after the header is valid TOML (ri12-2)
        assert_eq!(
            parse_selector("[toolchain] # pinned\nchannel\t=\t'1.99.0'\r\n").unwrap(),
            Requirements {
                channel: Some("1.99.0".to_owned()),
                ..Requirements::default()
            }
        );
    }

    /// TOML whitespace is space and tab only, and control characters other than tab never
    /// belong to a selector, comments included (ri12-2).
    #[test]
    fn selector_refuses_unicode_whitespace_and_control_characters() {
        for text in [
            "[toolchain]\nchannel\u{a0}=\u{a0}\"1.99.0\"\n",
            "[toolchain]\u{a0}\nchannel = '1.99.0'\n",
            "[toolchain]\n# comment with \0 inside\n",
            "[toolchain]\nchannel = '1.99.0' # \u{1}\n",
            "[toolchain]\rchannel = '1.99.0'\n",
            "[toolchain]\u{2028}channel = '1.99.0'\n",
        ] {
            assert_eq!(parse_selector(text), Err(UNSUPPORTED), "{text:?}");
        }
    }

    /// A carriage return is a line terminator only as `\r\n`; bare, it hides text from the
    /// line-based reader, in the header comment, a comment-only line or at the end of input
    /// too (ri3-6).
    #[test]
    fn selector_refuses_bare_carriage_returns_wherever_they_hide() {
        for text in [
            "[toolchain] # comment\rjunk\nprofile = 'minimal'\n",
            "[toolchain]\n# comment\rchannel = 'nightly'\n",
            "[toolchain]\nchannel = '1.99.0'\n\r",
            "[toolchain]\nchannel = '1.99.0'\r",
            "\r[toolchain]\n",
        ] {
            assert_eq!(parse_selector(text), Err(UNSUPPORTED), "{text:?}");
        }
        assert_eq!(
            parse_selector("[toolchain] # pinned\r\n# a comment\r\nprofile = 'minimal'\r\n")
                .unwrap(),
            Requirements::default()
        );
    }

    #[test]
    fn selector_refuses_every_shape_outside_the_subset() {
        for text in [
            "",
            "channel = '1.99.0'\n",
            "[toolchain]\n[other]\n",
            "[toolchain]\n[toolchain]\n",
            "[toolchain]\nprofile = 'complete'\n",
            "[toolchain]\nprofile = 1\n",
            "[toolchain]\nprofile = 'minimal' trailing\n",
            "[toolchain]\npath = '/tmp'\n",
            "[toolchain]\ntoolchain.channel = '1.99.0'\n",
            "[toolchain]\n\"channel\" = '1.99.0'\n",
            "[toolchain]\nchannel = '1.99.0'\nchannel = '1.99.0'\n",
            "[toolchain]\nchannel = 1\n",
            "[toolchain]\nchannel = \"1.99\\n0\"\n",
            "[toolchain]\nchannel = '1.99.0' trailing\n",
            "[toolchain]\nchannel = '1.99.0\n",
            "[toolchain]\ncomponents = [\n'rust-src'\n]\n",
            "[toolchain]\ncomponents = ['rust-src' 'clippy']\n",
            "[toolchain]\ncomponents = ['rust-src'\n",
            "[toolchain]\ncomponents = [1]\n",
            "[toolchain]\ncomponents = 'rust-src'\n",
            "[toolchain]\nchannel\n",
        ] {
            assert_eq!(parse_selector(text), Err(UNSUPPORTED), "{text:?}");
        }
    }

    /// hd1-1: the keys that redirect the pinned backend for the active target refuse, named
    /// in plain words, in every TOML spelling the reader knows; nothing else does.
    #[test]
    fn cargo_config_redirect_names_the_table_and_key_of_the_active_target_only() {
        let target = "aarch64-unknown-linux-gnu";
        for (text, expected) in [
            (
                "[env]\nCOMPILER_PATH = { value = \"x\", force = true }\n",
                "[env] COMPILER_PATH with force = true redirects the compiler backend; remove it to index",
            ),
            (
                "[env.CC]\nvalue = 'x' # comment\nforce = true\n",
                "[env] CC with force = true redirects the compiler backend; remove it to index",
            ),
            (
                "env.CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = { value = \"x\", relative = true, force = true }\n",
                "[env] CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER with force = true redirects the compiler backend; remove it to index",
            ),
            (
                "[env]\nLD_PRELOAD = \"x\"\n",
                "[env] LD_PRELOAD redirects the compiler backend; remove it to index",
            ),
            (
                "[target.aarch64-unknown-linux-gnu]\nrustflags = [\n  \"-C\", # split\n  \"link-arg=-B/tmp/x/\",\n]\n",
                "[target.aarch64-unknown-linux-gnu] rustflags carries -C link-arg=-B/tmp/x/, which redirects the compiler backend; remove it to index",
            ),
            (
                "[target.\"aarch64-unknown-linux-gnu\"]\nrustflags = \"-Clink-arg=-Wl,-O1 -Clinker=/work/cc\"\n",
                "[target.aarch64-unknown-linux-gnu] rustflags carries -Clinker=/work/cc, which redirects the compiler backend; remove it to index",
            ),
            (
                "[build]\nrustflags = ['-C', 'link-self-contained=yes']\n",
                "[build] rustflags carries -C link-self-contained=yes, which redirects the compiler backend; remove it to index",
            ),
            (
                "[build]\nrustflags = ['-C', 'link-args=-O1 -specs=/work/specs']\n",
                "[build] rustflags carries -C link-args=-O1 -specs=/work/specs, which redirects the compiler backend; remove it to index",
            ),
            (
                "[target.'cfg(unix)']\nlinker = \"cc\"\n",
                "[target.cfg(unix)] linker redirects the compiler backend; remove it to index",
            ),
            (
                "target.aarch64-unknown-linux-gnu.linker = \"cc\"\n",
                "[target.aarch64-unknown-linux-gnu] linker redirects the compiler backend; remove it to index",
            ),
        ] {
            assert_eq!(
                cargo_config_redirect(text, target).as_deref(),
                Some(expected),
                "{text:?}"
            );
        }
        for text in [
            "",
            "# kaban's own\n[target.aarch64-unknown-linux-musl]\nlinker = \"musl-gcc\"\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n[target.x86_64-unknown-linux-musl]\nlinker = \"musl-gcc\"\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n",
            "[env]\nCOMPILER_PATH = \"x\"\nCC = { value = \"x\", force = false }\nFOO = { value = \"# not a comment\", force = true }\n",
            "[build]\nrustflags = [\"-C\", \"target-cpu=native\", \"-Clink-arg=-Wl,-O1\"]\nrustc-wrapper = \"./anything\"\n",
            "[target.aarch64-unknown-linux-gnu]\nrustflags = [\"-L\", \"/work/lib\"]\n[target.x86_64-unknown-linux-gnu]\nlinker = \"cc\"\n",
            "[target.aarch64-unknown-linux-gnu.foo]\nrustc-link-search = [\"/work\"]\n",
        ] {
            assert_eq!(cargo_config_redirect(text, target), None, "{text:?}");
        }
    }

    /// DF-2 blocker 2a: a target triple may hold `_` (`x86_64-unknown-linux-musl`).
    #[test]
    fn selector_target_triple_accepts_underscores() {
        let root = std::env::temp_dir().join(format!(
            "kaban-df2-triple-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let target = "x86_64-unknown-linux-musl";
        std::fs::create_dir_all(root.join("lib/rustlib").join(target).join("lib")).unwrap();
        std::fs::write(
            root.join("lib/rustlib/components"),
            format!("rust-std-{target}\n"),
        )
        .unwrap();
        let request = Requirements {
            targets: strings(&[target]),
            ..Requirements::default()
        };
        let result = satisfies(&request, &root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
    }
}
