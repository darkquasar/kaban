//! Private loading boundary. Execution isolation/deadlines belong to the caller.
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::admission::Prepared;
use anyhow::{Context, Result, ensure};
use ra_ap_paths::AbsPathBuf;
use ra_ap_project_model::{
    CargoConfig, ProjectManifest, ProjectWorkspace, ProjectWorkspaceKind, RustLibSource,
};

mod declarations;
mod targets;

pub(super) fn run(args: &[OsString], prepared: &Prepared) -> Result<()> {
    let scratch = Scratch::read().context(
        "clean scratch environment is required; semantic exporter qualification is incomplete",
    )?;
    let environment = scratch.child_environment(prepared)?;
    if args[0] == "--load-prepared" {
        ensure!(
            std::env::vars_os().collect::<BTreeMap<_, _>>() == environment,
            "unclean semantic child environment; use the prepared index dispatch"
        );
        qualify_resources(prepared)?;
        let (bytes, coverage, notes) = load(prepared)?;
        eprint!("{}", report(&coverage, &notes));
        publish(&prepared.output, &bytes)?;
        // beside index.scip, for R-1 (coverage visible in the snapshot); ignored until then
        return fs::write(
            prepared.output.with_file_name("coverage.json"),
            serde_json::to_vec_pretty(&coverage).context("cannot encode coverage")?,
        )
        .context("cannot write coverage.json");
    }
    // RA resolves Cargo/PATH from process environment before extra_env. Re-exec,
    // never mutate the parent process environment. Admission runs again in child. The
    // child's stderr (at most 4000 bytes) is its report: forwarded on success, the
    // diagnostic on failure.
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--load-prepared")
        .args(&args[1..])
        .env_clear()
        .envs(environment)
        .current_dir(&prepared.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start clean semantic child")?;
    let mut pipe = child
        .stderr
        .take()
        .context("missing child diagnostic pipe")?;
    let mut diagnostic = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let n = match pipe.read(&mut buffer) {
            Ok(n) => n,
            Err(error) => {
                // Settle our direct child even when the pipe fails. The existing
                // runner still owns descendant/group deadlines and teardown.
                let _ = child.kill();
                child
                    .wait()
                    .context("cannot settle failed semantic child")?;
                return Err(error).context("cannot read semantic child diagnostics");
            }
        };
        if n == 0 {
            break;
        }
        let keep = n.min(4000usize.saturating_sub(diagnostic.len()));
        diagnostic.extend_from_slice(&buffer[..keep]);
    }
    let status = child.wait()?;
    if status.success() {
        // the child's report lines (contexts, declarations, skipped targets) reach the caller
        std::io::stderr().write_all(&diagnostic)?;
        return Ok(());
    }
    anyhow::bail!(
        "semantic child {status}: {}",
        String::from_utf8_lossy(&diagnostic)
    );
}

struct Scratch {
    home: PathBuf,
    cargo: PathBuf,
    target: PathBuf,
    tmp: PathBuf,
}
impl Scratch {
    fn child_environment(&self, p: &Prepared) -> Result<BTreeMap<OsString, OsString>> {
        let mut values: BTreeMap<OsString, OsString> = [
            ("CARGO", p.cargo.as_os_str()),
            ("RUSTC", p.rustc.as_os_str()),
            ("HOME", self.home.as_os_str()),
            ("CARGO_HOME", self.cargo.as_os_str()),
            ("CARGO_TARGET_DIR", self.target.as_os_str()),
            ("TMPDIR", self.tmp.as_os_str()),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        values.insert(
            "PATH".into(),
            format!("{}:/tools:/usr/bin:/bin", p.sysroot.join("bin").display()).into(),
        );
        values.insert(
            "RUSTDOC".into(),
            p.sysroot.join("bin/rustdoc").into_os_string(),
        );
        // Contract v2: cargo links with the admitted linker, named for the prepared target;
        // build scripts compiling C (`cc-rs`) take the same compiler as `CC` (DF-2: inside an
        // isolating view `/usr/bin/cc` is a symlink through an unbound `/etc/alternatives`).
        values.insert(
            format!(
                "CARGO_TARGET_{}_LINKER",
                p.target.to_ascii_uppercase().replace('-', "_")
            )
            .into(),
            p.linker.clone().into_os_string(),
        );
        values.insert("CC".into(), p.linker.clone().into_os_string());
        // The backend pin (HD-1, astra ri4-3bc-1): gcc resolves `ld` and `as` through its own
        // program directories, then `COMPILER_PATH`, and only then `PATH`, where the toolchain
        // directories precede `/usr/bin`. Naming the admitted linker's own directory closes
        // that fallback for every gcc the child runs (cargo's link step, `cc-rs` compiles);
        // the linker is canonical and absolute, so its directory exists.
        if let Some(directory) = p.linker.parent() {
            values.insert("COMPILER_PATH".into(), directory.as_os_str().to_owned());
        }
        #[cfg(target_os = "macos")]
        if p.target.ends_with("-apple-darwin") {
            let apple = crate::apple::resources(&p.linker, &p.workspace)?;
            values.insert("SDKROOT".into(), apple.sdk.into_os_string());
            values.insert("DEVELOPER_DIR".into(), apple.developer.into_os_string());
            // CoreFoundation otherwise adds this key during process initialization.
            // Pin the real UID and encoding instead of accepting an ambient extra key.
            values.insert(
                "__CF_USER_TEXT_ENCODING".into(),
                format!("0x{:X}:0x0:0x0", rustix::process::getuid().as_raw()).into(),
            );
            // Clang's own backend directory precedes both the Rust toolchain and /tools.
            // A shadow ld/as there must never displace the admitted Apple tools.
            values.insert(
                "PATH".into(),
                format!(
                    "{}:{}:/tools:/usr/bin:/bin",
                    apple.bin.display(),
                    p.sysroot.join("bin").display()
                )
                .into(),
            );
        }
        for (key, value) in [
            ("CARGO_NET_OFFLINE", "true"),
            ("CARGO_BUILD_JOBS", "2"),
            ("CARGO_INCREMENTAL", "0"),
            ("CARGO_PROFILE_DEV_DEBUG", "0"),
            ("LANG", "C.UTF-8"),
        ] {
            values.insert(key.into(), value.into());
        }
        Ok(values)
    }
    fn read() -> Result<Self> {
        fn directory(name: &str) -> Result<PathBuf> {
            let path =
                PathBuf::from(std::env::var_os(name).with_context(|| format!("missing {name}"))?);
            ensure!(
                path.is_absolute() && path.is_dir(),
                "invalid scratch directory {name}"
            );
            Ok(path)
        }
        let result = Self {
            home: directory("HOME")?,
            cargo: directory("CARGO_HOME")?,
            target: directory("CARGO_TARGET_DIR")?,
            tmp: directory("TMPDIR")?,
        };
        ensure!(
            fs::read(result.cargo.join("config.toml"))? == b"[net]\noffline = true\n",
            "clean offline Cargo home config required"
        );
        ensure!(
            !result.cargo.join("config").exists()
                && !result.cargo.join("credentials").exists()
                && !result.cargo.join("credentials.toml").exists(),
            "Cargo home contains unsupported configuration or credentials"
        );
        Ok(result)
    }
}

fn qualify_resources(p: &Prepared) -> Result<()> {
    for (resource, expected) in [
        (&p.cargo, p.sysroot.join("bin/cargo")),
        (&p.rustc, p.sysroot.join("bin/rustc")),
        (
            &p.proc_macro_server,
            p.sysroot.join("libexec/rust-analyzer-proc-macro-srv"),
        ),
    ] {
        ensure!(
            !fs::symlink_metadata(resource)?.file_type().is_symlink() && *resource == expected,
            "real pinned toolchain resources required (no proxies)"
        );
    }
    for (tool, prefix) in [(&p.cargo, "cargo 1.99.0 "), (&p.rustc, "rustc 1.99.0 ")] {
        let output = Command::new(tool)
            .arg("--version")
            .output()
            .context("cannot query pinned toolchain")?;
        ensure!(
            output.status.success() && output.stdout.starts_with(prefix.as_bytes()),
            "pinned Rust 1.99.0 toolchain required"
        );
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<AbsPathBuf> {
    ensure!(
        path.is_absolute() && path.to_str().is_some(),
        "semantic paths must be absolute UTF-8"
    );
    Ok(AbsPathBuf::assert_utf8(path.to_path_buf()))
}

fn load(p: &Prepared) -> Result<(Vec<u8>, Coverage, Vec<String>)> {
    let mut notes = Vec::new();
    // RA can silently replace failed full sysroot metadata with either no-deps
    // metadata or a stitched crate graph. Neither is complete contract-v1 input.
    // Prove full metadata first, using the pinned Cargo in our clean child and
    // the same fixed channel override RA needs for rust-src's public-dependency.
    let library = p.sysroot.join("lib/rustlib/src/rust/library");
    let sysroot_metadata = Command::new(&p.cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--filter-platform",
        ])
        .arg(&p.target)
        .arg("--manifest-path")
        .arg(library.join("Cargo.toml"))
        .env("__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS", "nightly")
        .current_dir(&library)
        .output()
        .context("semantic sysroot metadata loading failed")?;
    ensure!(
        sysroot_metadata.status.success(),
        "semantic sysroot metadata loading failed: {}",
        String::from_utf8_lossy(&sysroot_metadata.stderr)
    );
    let config = CargoConfig {
        all_targets: true,
        target: Some(p.target.clone()),
        sysroot: Some(RustLibSource::Path(absolute(&p.sysroot)?)),
        sysroot_src: Some(absolute(&p.sysroot.join("lib/rustlib/src/rust/library"))?),
        extra_args: vec!["--locked".into(), "--offline".into()],
        ..CargoConfig::default()
    };
    let manifest = ProjectManifest::discover_single(&absolute(&p.workspace)?)?;
    let mut workspace = ProjectWorkspace::load(manifest, &config, &|_| {})
        .context("semantic metadata loading failed")?;
    ensure!(
        matches!(
            &workspace.kind,
            ProjectWorkspaceKind::Cargo { error: None, .. }
        ),
        "semantic metadata loading failed or unsupported configuration"
    );
    ensure!(
        workspace.sysroot.error().is_none() && !workspace.sysroot.is_rust_lib_src_empty(),
        "semantic sysroot loading failed"
    );
    ensure!(
        workspace.sysroot.metadata_error().is_none(),
        "semantic sysroot metadata loading failed: {}",
        workspace.sysroot.metadata_error().unwrap_or("unknown")
    );
    let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else {
        anyhow::bail!("unsupported non-Cargo workspace");
    };
    let metadata = Command::new(&p.cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--locked",
            "--offline",
        ])
        .current_dir(&p.workspace)
        .output()
        .context("Cargo test-context metadata failed")?;
    ensure!(
        metadata.status.success(),
        "Cargo test-context metadata failed"
    );
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let targets = targets::Catalog::read(cargo, &p.workspace, &metadata)?;
    if !targets.skipped().is_empty() {
        // the consumer reads this line by its prefix (kaban-index `skipped_targets`)
        notes.push(format!(
            "semantic skipped targets={}",
            targets.skipped().join(",")
        ));
    }
    let builds = workspace
        .run_build_scripts(&config, &|_| {})
        .context("semantic build loading failed")?;
    ensure!(
        builds.error().is_none(),
        "semantic build loading failed: {}",
        builds.error().unwrap_or("unknown")
    );
    workspace.set_build_scripts(builds);
    let load_config = ra_ap_load_cargo::LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ra_ap_load_cargo::ProcMacroServerChoice::Explicit(absolute(
            &p.proc_macro_server,
        )?),
        prefill_caches: false,
        num_worker_threads: 4,
        proc_macro_processes: 1,
    };
    let (mut db, vfs, server) =
        ra_ap_load_cargo::load_workspace(workspace, &config.extra_env, &load_config)
            .context("semantic database loading failed")?;
    ensure!(
        server.is_some(),
        "semantic proc-macro server loading failed"
    );
    ensure!(vfs.iter().next().is_some(), "semantic workspace is empty");
    // Never install RA's global set_test overlay: dependencies of a selected
    // test root must remain production crates. Change one safe Salsa input at a
    // time; cached queries are invalidated and the root is restored afterward.
    let contexts = targets.bind(&db, &vfs, targets::Mode::Production)?;
    let mut inventory = declarations::Inventory::default();
    let mut context_count = 0usize;
    let mut proc_macros_not_built = 0usize;
    for krate in ra_ap_hir::Crate::all(&db) {
        if let Some(macros) = ra_ap_hir::ProcMacros::get_for_crate(&db, krate.base())
            && let Some(error) = macros.get_error()
        {
            // rust-analyzer's own classification (DF-2): a proc-macro crate the compile-time
            // build never produced (`NotYetBuilt`: in the resolve but behind a feature no
            // selected target enables, kaban's own `time-macros`) is reported and tolerated,
            // like a disabled one; a crate that failed to build or load refuses.
            let name = krate
                .display_name(&db)
                .map_or_else(|| "?".to_owned(), |n| n.to_string());
            ensure!(
                !error.is_hard_error(),
                "semantic proc-macro library loading failed for crate {name}: {error}"
            );
            notes.push(format!(
                "semantic proc-macro crate {name} not loaded: {error}"
            ));
            proc_macros_not_built += 1;
        }
    }
    // declarations by Cargo package (R-1 `per_crate`): a declaration counts for the package of
    // the context that inventoried it first (the identities of one declaration seen again in
    // another context of the same target merge; `Inventory::len` counts identities)
    let mut declarations_by_package: BTreeMap<String, usize> = BTreeMap::new();
    for mut context in contexts {
        let modes = context.modes();
        for &mode in modes {
            // Per-target traversal intentionally does not visit a file just once.
            // Distinct targets may share modules; the semantic owner is retained.
            let before = inventory.len();
            context.in_mode(&mut db, mode, |db, context| {
                ra_ap_hir::attach_db(db, || {
                    inventory.collect(db, &vfs, context, &p.workspace, &targets)
                })
            })?;
            *declarations_by_package
                .entry(context.identity.package_name().to_owned())
                .or_default() += inventory.len() - before;
            context_count += 1;
        }
    }
    ensure!(
        inventory.len() > 0,
        "semantic workspace has no located declarations"
    );
    let bytes = inventory.encode(&p.workspace)?;
    let coverage = Coverage {
        targets_skipped: targets.skipped().len(),
        proc_macros_not_built,
        contexts: context_count,
        declarations: inventory.len(),
        block_items_skipped: inventory.skipped_block_items(),
        block_item_references: inventory.block_item_references(),
        shared_file_contexts_skipped: inventory.skipped_shared_file_contexts(),
        incomplete_modules: inventory.incomplete_modules(),
        divergent_bodies: inventory.divergent_bodies(),
        cfg_divergent_declarations: inventory.divergent_declarations(),
        tokens_without_symbol: inventory.skipped_tokens(),
        positions_skipped: inventory.skipped_positions(),
        scip_bytes: bytes.len(),
        derived_impls_generic_skipped: inventory.derived_impls_generic_skipped(),
        ..Coverage::default()
    }
    .with_block_items(inventory.block_items(), &declarations_by_package);
    notes.extend(inventory.skipped_token_note());
    notes.extend(inventory.notes().iter().cloned());
    Ok((bytes, coverage, notes))
}

/// The aggregate coverage counters of one run (DF-2 limitations, counted not refused). They
/// are reported as one machine-readable line and written as `coverage.json` beside
/// `index.scip` (df23-7; the consumer ignores the file until R-1 reads it into the snapshot).
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Coverage {
    targets_skipped: usize,
    proc_macros_not_built: usize,
    contexts: usize,
    declarations: usize,
    block_items_skipped: usize,
    block_item_references: usize,
    shared_file_contexts_skipped: usize,
    incomplete_modules: usize,
    divergent_bodies: usize,
    cfg_divergent_declarations: usize,
    tokens_without_symbol: usize,
    positions_skipped: usize,
    scip_bytes: usize,
    /// Distinct skipped block items, by (file, source span) across every context (R-1): the
    /// measurement `block_items_skipped` is not, since that counter adds every visit of an
    /// item (production and test mode of a target, every target sharing the module). Items a
    /// macro generates inside one call share the call's span and count as one.
    block_items_unique: usize,
    /// Lines of committed source inside those items, each line of a file counted once.
    block_item_lines: usize,
    /// Impls inside anonymous consts (the derive shape, R-3) declared without an `IMPLEMENTS`
    /// edge because contract v1 rejects their endpoints (type or const parameters, predicates,
    /// negative): a gap counter like the block-item ones (FIX-G Task 3; the consumer's
    /// `writer::Coverage::KEYS` names it, so both sides accept each other).
    derived_impls_generic_skipped: usize,
    /// The same figures and the published declarations by Cargo package, sorted by name.
    per_crate: Vec<CrateCoverage>,
}

/// One Cargo package's row of `Coverage::per_crate`.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CrateCoverage {
    #[serde(rename = "crate")]
    name: String,
    declarations: usize,
    block_items_unique: usize,
    block_item_lines: usize,
}

/// The skipped block items of a run, each physical item once: its identity is the committed
/// file and the item's byte range in it (the item's own span, or the span of the macro call
/// that generated it; r1-2: two items on one line are two items), with the item's 0-based line
/// span and the Cargo packages whose contexts visited it. One item visited in several target
/// contexts is one entry.
#[derive(Debug, Default)]
pub(super) struct BlockItems {
    items: BTreeMap<String, BTreeMap<(u32, u32), BlockItem>>,
}

#[derive(Debug)]
struct BlockItem {
    lines: (u32, u32),
    packages: std::collections::BTreeSet<String>,
}

impl BlockItems {
    pub(super) fn record(
        &mut self,
        package: &str,
        path: &str,
        range: (u32, u32),
        lines: (u32, u32),
    ) {
        self.items
            .entry(path.to_owned())
            .or_default()
            .entry(range)
            .or_insert_with(|| BlockItem {
                lines,
                packages: std::collections::BTreeSet::new(),
            })
            .packages
            .insert(package.to_owned());
    }

    /// Every package that visited an item.
    fn packages(&self) -> std::collections::BTreeSet<&str> {
        self.items
            .values()
            .flat_map(BTreeMap::values)
            .flat_map(|item| item.packages.iter().map(String::as_str))
            .collect()
    }

    /// `(distinct items, lines covered)` over the items `keep` selects, lines as the union of
    /// their line spans per file: a nested item adds no line.
    fn measure(&self, keep: impl Fn(&BlockItem) -> bool) -> (usize, usize) {
        let mut unique = 0;
        let mut lines = 0;
        for items in self.items.values() {
            // items are sorted by start offset, so by start line: fold the union of the
            // inclusive line ranges
            let mut covered_to: Option<u32> = None;
            for item in items.values().filter(|item| keep(item)) {
                unique += 1;
                let (start, end) = item.lines;
                let from = match covered_to {
                    Some(to) if to >= start => to + 1,
                    _ => start,
                };
                if end >= from {
                    lines += (end - from) as usize + 1;
                    covered_to = Some(end);
                }
            }
        }
        (unique, lines)
    }
}

impl Coverage {
    /// Fill the unique block-item figures (the global union over every item) and the
    /// per-package table (the items each package's contexts visited; a file two packages share
    /// counts for both) from the ledger and the published declarations by package (every
    /// package of either side gets a row).
    fn with_block_items(
        mut self,
        items: &BlockItems,
        declarations: &BTreeMap<String, usize>,
    ) -> Self {
        let mut rows: BTreeMap<&str, CrateCoverage> = BTreeMap::new();
        for (package, count) in declarations {
            rows.entry(package).or_default().declarations = *count;
        }
        (self.block_items_unique, self.block_item_lines) = items.measure(|_| true);
        for package in items.packages() {
            let (unique, lines) = items.measure(|item| item.packages.contains(package));
            let row = rows.entry(package).or_default();
            row.block_items_unique = unique;
            row.block_item_lines = lines;
        }
        self.per_crate = rows
            .into_iter()
            .map(|(name, row)| CrateCoverage {
                name: name.to_owned(),
                ..row
            })
            .collect();
        self
    }

    /// The one-line summary: `semantic coverage: key=value ...`, every counter always present
    /// (`per_crate` is in `coverage.json` only).
    fn line(&self) -> String {
        format!(
            "semantic coverage: targets_skipped={} proc_macros_not_built={} contexts={} declarations={} block_items_skipped={} block_item_references={} shared_file_contexts_skipped={} incomplete_modules={} divergent_bodies={} cfg_divergent_declarations={} tokens_without_symbol={} positions_skipped={} scip_bytes={} block_items_unique={} block_item_lines={} derived_impls_generic_skipped={}",
            self.targets_skipped,
            self.proc_macros_not_built,
            self.contexts,
            self.declarations,
            self.block_items_skipped,
            self.block_item_references,
            self.shared_file_contexts_skipped,
            self.incomplete_modules,
            self.divergent_bodies,
            self.cfg_divergent_declarations,
            self.tokens_without_symbol,
            self.positions_skipped,
            self.scip_bytes,
            self.block_items_unique,
            self.block_item_lines,
            self.derived_impls_generic_skipped
        )
    }
}

/// The child's stderr report: the coverage summary first, on one line, then the detailed
/// diagnostics. The parent keeps the first 4,000 bytes (`run`), so however much detail is cut
/// the counters reach the gated log intact (df23-7).
fn report(coverage: &Coverage, notes: &[String]) -> String {
    let mut out = coverage.line();
    out.push('\n');
    for note in notes {
        out.push_str(note);
        out.push('\n');
    }
    out
}

fn publish(output: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(!output.exists(), "SCIP output already exists");
    let pending = output.with_extension("scip.pending");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)
        .context("cannot create atomic SCIP staging file")?;
    file.write_all(bytes)
        .context("cannot stage complete SCIP")?;
    file.sync_all().context("cannot sync complete SCIP")?;
    // hard_link is atomic and refuses an existing destination. Keep the
    // scratch-only staging link as retained evidence, never replace user data.
    fs::hard_link(&pending, output).context("cannot publish complete SCIP")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn directory() -> PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
            PathBuf::from,
        );
        let root = target.join(format!(
            "a7-publish-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("test scratch");
        root
    }

    /// The parent keeps only the first 4,000 bytes of the child's stderr (`run`): every counter
    /// must be in them however many detailed diagnostics the run produced (df23-7).
    #[test]
    fn every_coverage_counter_survives_the_parent_bound_whatever_the_diagnostics() {
        let coverage = Coverage {
            targets_skipped: 3,
            proc_macros_not_built: 1,
            contexts: 277,
            declarations: 68292,
            block_items_skipped: 28202,
            block_item_references: 41,
            shared_file_contexts_skipped: 352,
            incomplete_modules: 9,
            divergent_bodies: 1,
            cfg_divergent_declarations: 4,
            tokens_without_symbol: 405,
            positions_skipped: 2,
            scip_bytes: 95_184_043,
            derived_impls_generic_skipped: 764,
            ..Coverage::default()
        };
        let notes: Vec<String> = (0..200)
            .map(|i| format!("semantic incomplete resolution in module m{i}: unresolved import"))
            .collect();
        let text = report(&coverage, &notes);
        assert!(
            text.len() > 4000,
            "the fixture must exceed the parent bound"
        );
        let mut end = 4000;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let retained = &text[..end];
        assert_eq!(
            retained.lines().next(),
            Some(coverage.line().as_str()),
            "the summary is the first line"
        );
        for counter in [
            "targets_skipped=3",
            "proc_macros_not_built=1",
            "contexts=277",
            "declarations=68292",
            "block_items_skipped=28202",
            "block_item_references=41",
            "shared_file_contexts_skipped=352",
            "incomplete_modules=9",
            "divergent_bodies=1",
            "cfg_divergent_declarations=4",
            "tokens_without_symbol=405",
            "positions_skipped=2",
            "scip_bytes=95184043",
            "derived_impls_generic_skipped=764",
        ] {
            assert!(
                retained.contains(counter),
                "{counter} lost in the retained log"
            );
        }
        let json = serde_json::to_vec(&coverage).expect("coverage.json");
        assert_eq!(
            serde_json::from_slice::<Coverage>(&json).expect("round trip"),
            coverage
        );
    }

    /// Task 3 (consumer request 1): the derived-impl gap counter is a coverage key like the
    /// others, in the one-line summary and in `coverage.json`, never a stderr note only.
    #[test]
    fn derived_impls_generic_skipped_is_a_coverage_key_in_the_line_and_the_sidecar() {
        let coverage = Coverage::default();
        let json = serde_json::to_value(&coverage).expect("coverage.json");
        assert!(
            json["derived_impls_generic_skipped"].is_u64(),
            "the key is absent from coverage.json: {json}"
        );
        assert!(
            coverage.line().contains(" derived_impls_generic_skipped=0"),
            "{}",
            coverage.line()
        );
    }

    /// `block_items_skipped` counts every visit of a block item (one per selected context of
    /// every target that owns the module); the unique figure, its lines and the per-crate table
    /// count the physical item once (astra-block-items.md, first correction).
    #[test]
    fn a_block_item_seen_in_two_contexts_is_one_unique_item_with_its_lines_counted_once() {
        let mut items = BlockItems::default();
        // the same item in the production and in the test context of one target
        items.record("kaban-index", "src/lib.rs", (100, 180), (10, 14));
        items.record("kaban-index", "src/lib.rs", (100, 180), (10, 14));
        // an item nested inside it: distinct, but adds no line
        items.record("kaban-index", "src/lib.rs", (130, 140), (12, 12));
        items.record("kaban-api", "src/code/mod.rs", (40, 70), (3, 4));
        let declarations = BTreeMap::from([
            ("kaban-index".to_owned(), 7usize),
            ("kaban-core".to_owned(), 2),
        ]);
        let coverage = Coverage {
            block_items_skipped: 4,
            ..Coverage::default()
        }
        .with_block_items(&items, &declarations);
        assert_eq!(
            coverage.block_items_skipped, 4,
            "the raw visit count is kept"
        );
        assert_eq!(coverage.block_items_unique, 3);
        assert_eq!(coverage.block_item_lines, 5 + 2);
        assert_eq!(
            coverage.per_crate,
            vec![
                CrateCoverage {
                    name: "kaban-api".to_owned(),
                    declarations: 0,
                    block_items_unique: 1,
                    block_item_lines: 2,
                },
                CrateCoverage {
                    name: "kaban-core".to_owned(),
                    declarations: 2,
                    block_items_unique: 0,
                    block_item_lines: 0,
                },
                CrateCoverage {
                    name: "kaban-index".to_owned(),
                    declarations: 7,
                    block_items_unique: 2,
                    block_item_lines: 5,
                },
            ]
        );
        let json = serde_json::to_value(&coverage).expect("coverage.json");
        assert_eq!(json["block_items_unique"], 3);
        assert_eq!(json["per_crate"][2]["crate"], "kaban-index");
        assert!(
            coverage
                .line()
                .contains("block_items_unique=3 block_item_lines=7"),
            "{}",
            coverage.line()
        );
    }

    /// r1-2: identity is the item's source range, not its line span. `fn outer() { fn a() {}
    /// fn b() {} }` holds two items on one line; and the global figures are the union over
    /// every package, not the sum of the package buckets (a file two packages visit).
    #[test]
    fn two_block_items_on_one_line_are_two_items_and_the_global_union_is_not_a_sum() {
        let mut items = BlockItems::default();
        items.record("kaban-core", "src/lib.rs", (12, 22), (0, 0));
        items.record("kaban-core", "src/lib.rs", (23, 33), (0, 0));
        // a `#[path]`-included file visited by a lib and a bin package: one item, two buckets
        items.record("kaban-core", "src/shared.rs", (0, 30), (0, 2));
        items.record("kaban-bin", "src/shared.rs", (0, 30), (0, 2));
        let coverage = Coverage::default().with_block_items(&items, &BTreeMap::new());
        assert_eq!(coverage.block_items_unique, 3, "{coverage:?}");
        assert_eq!(coverage.block_item_lines, 1 + 3);
        let bucket = |name: &str| {
            coverage
                .per_crate
                .iter()
                .find(|row| row.name == name)
                .map(|row| (row.block_items_unique, row.block_item_lines))
                .expect("row")
        };
        assert_eq!(bucket("kaban-core"), (3, 4));
        assert_eq!(bucket("kaban-bin"), (1, 3));
    }

    #[test]
    fn child_environment_is_the_fixed_set_plus_the_linker_as_target_linker_cc_and_backend_pin() {
        let scratch = Scratch {
            home: PathBuf::from("/s/home"),
            cargo: PathBuf::from("/s/cargo-home"),
            target: PathBuf::from("/s/target"),
            tmp: PathBuf::from("/s/tmp"),
        };
        let prepared = Prepared {
            workspace: PathBuf::from("/work"),
            output: PathBuf::from("/s/out/index.scip"),
            target: "aarch64-unknown-linux-gnu".to_owned(),
            cargo: PathBuf::from("/toolchain/bin/cargo"),
            rustc: PathBuf::from("/toolchain/bin/rustc"),
            sysroot: PathBuf::from("/toolchain"),
            proc_macro_server: PathBuf::from("/toolchain/libexec/rust-analyzer-proc-macro-srv"),
            linker: PathBuf::from("/usr/bin/aarch64-linux-gnu-gcc-15"),
        };
        let expected: BTreeMap<OsString, OsString> = [
            ("CARGO", "/toolchain/bin/cargo"),
            ("RUSTC", "/toolchain/bin/rustc"),
            ("HOME", "/s/home"),
            ("CARGO_HOME", "/s/cargo-home"),
            ("CARGO_TARGET_DIR", "/s/target"),
            ("TMPDIR", "/s/tmp"),
            ("PATH", "/toolchain/bin:/tools:/usr/bin:/bin"),
            ("RUSTDOC", "/toolchain/bin/rustdoc"),
            ("CARGO_NET_OFFLINE", "true"),
            ("CARGO_BUILD_JOBS", "2"),
            ("CARGO_INCREMENTAL", "0"),
            ("CARGO_PROFILE_DEV_DEBUG", "0"),
            ("LANG", "C.UTF-8"),
            (
                "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
                "/usr/bin/aarch64-linux-gnu-gcc-15",
            ),
            ("CC", "/usr/bin/aarch64-linux-gnu-gcc-15"),
            // the backend pin (HD-1): gcc searches the admitted linker's directory before PATH
            ("COMPILER_PATH", "/usr/bin"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        assert_eq!(
            scratch
                .child_environment(&prepared)
                .expect("child environment"),
            expected
        );
    }

    #[test]
    fn output_is_published_complete_and_existing_output_is_never_replaced() {
        let root = directory();
        let output = root.join("index.scip");
        publish(&output, b"complete-serialized-index").expect("atomic output");
        assert_eq!(
            fs::read(&output).expect("output"),
            b"complete-serialized-index"
        );
        assert!(publish(&output, b"replacement").is_err());
        assert_eq!(
            fs::read(&output).expect("original"),
            b"complete-serialized-index"
        );
    }

    #[test]
    fn staging_collision_fails_without_partial_output_or_overwrite() {
        let root = directory();
        let output = root.join("index.scip");
        let pending = output.with_extension("scip.pending");
        fs::write(&pending, b"existing-staging-data").expect("existing staging");
        assert!(publish(&output, b"new-index").is_err());
        assert!(!output.exists());
        assert_eq!(
            fs::read(&pending).expect("retained staging"),
            b"existing-staging-data"
        );
    }
}
