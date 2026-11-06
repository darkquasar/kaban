//! Rust admission negatives through the `kaban-index` binary (RI-3b): the system linker is a
//! toolchain resource admitted by the Rust driver arm, never taken from `PATH` or the
//! repository, and the checkout boundary of `doctor` cannot be shrunk by an inner `.git`
//! entry (astra ri3-1). Fake producers and fake toolchains are written per test under `/tmp`
//! (`test-integration-dir`, `test-fixture-raii`); the binary is started through `/bin/bash`
//! closing the descriptors this process inherited without `FD_CLOEXEC`, so the production
//! descriptor policy (`Refuse`) holds in the child. The tier is whatever `doctor` selects on
//! this host; the `none` tier is forced with `index.isolation: off`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible `NOT RUN` markers

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kaban_index::doctor::{self, ProbeSite};
use protobuf::Message;
use serde_json::Value;

// ---------------------------------------------------------------------------------------------
// harness

/// A scratch directory deleted on drop, test failure included.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-ri3b-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("scratch");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn home(&self) -> PathBuf {
        let home = self.0.join("home");
        fs::create_dir_all(&home).unwrap();
        home
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_COMMITTER_DATE", "2026-10-26T08:16:02Z")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

/// The numbers of the descriptors this process holds without `FD_CLOEXEC` (above 2).
fn non_cloexec_fds() -> Vec<u32> {
    kaban_core::process::inheritable_fds().expect("descriptor observation is required")
}

/// Native probes enforce `Refuse` on their caller, so re-execute these tests cleanly when
/// the runner inherited descriptors. The clean child runs the same enabled assertions.
fn rerun_native_test_with_clean_descriptors(name: &str) -> bool {
    let inherited = non_cloexec_fds();
    if inherited.is_empty() {
        return false;
    }
    let closes = inherited.iter().fold(String::new(), |mut acc, n| {
        let _ = write!(acc, " {n}>&-");
        acc
    });
    let script = format!("exec{closes}\nexec \"$0\" --exact \"$1\" --nocapture\n");
    let out = Command::new("/bin/bash")
        .arg("-c")
        .arg(script)
        .arg(std::env::current_exe().expect("test executable"))
        .arg(name)
        .output()
        .expect("clean native test starts");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "clean native test {name} failed:\n{stdout}\n{}",
        stderr(&out)
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "clean native test {name} ran: {stdout}"
    );
    // Keep unavailable-host markers visible to CI when the assertions run in a child.
    eprint!("{stdout}{}", stderr(&out));
    true
}

/// Run `kaban-index <args>` with `KABAN_HOME=<home>` and the extra environment, in a child
/// holding only the standard descriptors.
fn kaban_index_env(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let closes = non_cloexec_fds().iter().fold(String::new(), |mut acc, n| {
        let _ = write!(acc, " {n}>&-");
        acc
    });
    let script = format!("exec{closes}\nexec \"$0\" \"$@\"\n");
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-c")
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_kaban-index"))
        .args(args)
        .env("KABAN_HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("kaban-index runs")
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "one JSON report on stdout ({e}): {}\n--- stderr\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

fn trust(home: &Path, repo: &Path) -> String {
    let out = kaban_index_env(home, &["trust", "--repo", repo.to_str().unwrap()], &[]);
    assert_eq!(code(&out), 0, "trust: {}", stderr(&out));
    json(&out)["repo_id"].as_str().unwrap().to_owned()
}

fn head(repo: &Path) -> String {
    git(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// A fake executable `dir/name` (0755) with `body` after the `#!/bin/sh` line.
fn fake_tool(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake toolchain root with the narrow Rust layout; `rustc` answers `-vV` for the pinned
/// release and this host.
fn fake_rust_chain(root: &Path) -> PathBuf {
    for file in [
        "lib/rustlib/src/rust/library/core/src/lib.rs",
        "lib/rustlib/src/rust/library/std/src/lib.rs",
        "lib/rustlib/components",
    ] {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }
    fake_tool(&root.join("bin"), "cargo", "exit 0");
    fake_tool(&root.join("bin"), "rustdoc", "exit 0");
    fake_tool(
        &root.join("libexec"),
        "rust-analyzer-proc-macro-srv",
        "exit 0",
    );
    fake_tool(
        &root.join("bin"),
        "rustc",
        &format!(
            "echo 'release: 1.99.0'\necho 'host: {}'",
            doctor::rust_host()
        ),
    );
    root.to_path_buf()
}

/// The qualified capability answer of the fake producer.
fn capabilities_json() -> String {
    format!(
        r#"{{"protocol_version":2,"semantic_profile":"kaban-rust-v1","producer_version":"0.1.0-dev","ra_ap_version":"0.0.358","supported_host_triples":["{}"]}}"#,
        doctor::rust_host()
    )
}

/// A one-commit Rust package at `dir` (`Cargo.toml`, a clean `src/lib.rs`); the canonical path.
fn rust_repo(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("src")).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(dir.join("src/lib.rs"), "pub fn fine() {}\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "fixture"]);
    fs::canonicalize(dir).unwrap()
}

/// A committed SCIP index for `repo` (one Rust document, the producer's tool info) a fake
/// producer copies to `--output`; its path under `dir`.
fn committed_index(dir: &Path, repo: &Path) -> PathBuf {
    let document = scip::types::Document {
        relative_path: "src/lib.rs".to_owned(),
        language: "rust".to_owned(),
        ..Default::default()
    };
    let mut index = scip::types::Index {
        documents: vec![document],
        ..Default::default()
    };
    index.metadata = protobuf::MessageField::some(scip::types::Metadata {
        project_root: format!("file://{}", repo.display()),
        tool_info: protobuf::MessageField::some(scip::types::ToolInfo {
            name: "kaban-rust-scip".to_owned(),
            version: "0.1.0-dev".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    });
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("index.scip");
    fs::write(&path, index.write_to_bytes().unwrap()).unwrap();
    path
}

/// The tier `doctor` selects on this host under the default configuration.
fn doctor_selected_tier(home: &Path) -> String {
    let out = kaban_index_env(home, &["doctor"], &[]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    json(&out)["doctor"]["isolation"]["selected"]["Ok"]["tier"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// `CARGO_TARGET_<TRIPLE>_LINKER` for this host, spelled independently of the production code.
fn linker_variable() -> String {
    format!(
        "CARGO_TARGET_{}_LINKER",
        doctor::rust_host().to_ascii_uppercase().replace('-', "_")
    )
}

// ---------------------------------------------------------------------------------------------
// DF-2: the host registry is an input of every Rust run; skipped targets reach the report

/// The caller's Cargo registry is what a workspace with registry dependencies needs to load
/// offline: on both tiers the run's scratch `CARGO_HOME/registry` resolves to the admitted host
/// registry with no flag (`none`: the caller's own directory; the selected isolating tier: the
/// same path bound read-only inside the view). The producer's `semantic skipped targets=` line
/// lands in the report as `skipped_targets_per_indexer.rust` (DF-2 blocker 4, nothing to act on).
fn host_registry_is_an_input_of_the_rust_run(isolation_off: bool) {
    let s = Scratch::new("registry");
    let home = s.home();
    if isolation_off {
        fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    } else if doctor_selected_tier(&home) == "none" {
        eprintln!("NOT RUN: host_registry_is_an_input_of_the_rust_run: no isolating tier here");
        return;
    }
    let repo = rust_repo(&s.path().join("repo"));
    let id = trust(&home, &repo);
    let chain = fake_rust_chain(&s.path().join("chain"));
    let cargo_home = s.path().join("host-cargo-home");
    fs::create_dir_all(cargo_home.join("registry/index")).unwrap();
    let registry = fs::canonicalize(cargo_home.join("registry")).unwrap();
    let fakes = s.path().join("fakes");
    let index = committed_index(&fakes, &repo);
    let producer = fake_tool(
        &fakes,
        "kaban-rust-scip",
        &format!(
            "case \"$1\" in\n  --version) echo qualified; exit 0;;\n  capabilities) echo '{}'; exit 0;;\nesac\nfor a in \"$@\"; do out=\"$a\"; done\nif [ -e \"$CARGO_HOME/registry/index\" ]; then echo \"registry=$(readlink -f \"$CARGO_HOME/registry\")\" >&2; else echo registry=none >&2; fi\necho 'semantic skipped targets=gated,bench_only' >&2\ncp '{}' \"$out\"",
            capabilities_json(),
            index.display()
        ),
    );
    let override_arg = format!("rust={}", producer.display());
    let path = format!("{}:/usr/bin:/bin", chain.join("bin").display());
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--lang",
            "rust",
            "--indexer",
            &override_arg,
        ],
        &[
            ("PATH", path.as_str()),
            ("CARGO_HOME", cargo_home.to_str().unwrap()),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(
        report["isolation"],
        if isolation_off { "none" } else { "namespaces" },
        "{report}"
    );
    assert_eq!(
        report["skipped_targets_per_indexer"]["rust"],
        serde_json::json!(["gated", "bench_only"]),
        "{report}"
    );
    let log = fs::read_to_string(
        home.join("codegraph")
            .join(&id)
            .join(format!("{}.rust.a.log", head(&repo))),
    )
    .expect("the gated run log");
    assert!(
        log.lines()
            .any(|l| l == format!("registry={}", registry.display())),
        "CARGO_HOME/registry resolves to the host registry: {log}"
    );
    assert!(
        registry.join("index").is_dir(),
        "the host registry is untouched by the run's cleanup"
    );
}

#[test]
fn host_registry_is_an_input_of_the_rust_run_on_the_none_tier() {
    host_registry_is_an_input_of_the_rust_run(true);
}

#[test]
fn host_registry_is_an_input_of_the_rust_run_on_the_selected_tier() {
    host_registry_is_an_input_of_the_rust_run(false);
}

// ---------------------------------------------------------------------------------------------
// Commit 1: the system linker is admitted by the driver, never taken from PATH

/// A fake `cc` under the checkout, first on `PATH`, is never the linker: the run is told the
/// canonical system compiler (or refuses `rust_linker_refused` when the host has none), the
/// fake never executes, and `doctor` reports the linker outcome as a class only.
fn checkout_cc_is_never_the_linker(isolation_off: bool) {
    let s = Scratch::new("linker-checkout-cc");
    let home = s.home();
    if isolation_off {
        fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    }
    let repo = rust_repo(&s.path().join("repo"));
    let marker = s.path().join("checkout-cc-ran");
    fake_tool(
        &repo.join("bin"),
        "cc",
        &format!("touch '{}'; exit 99", marker.display()),
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "a checkout cc"]);
    let id = trust(&home, &repo);
    let chain = fake_rust_chain(&s.path().join("chain"));
    // the fake producer: qualified probes; `index` records the linker it was given and copies
    // a committed index for the repository to `--output` (the last argument)
    let fakes = s.path().join("fakes");
    let index = committed_index(&fakes, &repo);
    let producer = fake_tool(
        &fakes,
        "kaban-rust-scip",
        &format!(
            "case \"$1\" in\n  --version) echo qualified; exit 0;;\n  capabilities) echo '{}'; exit 0;;\nesac\nfor a in \"$@\"; do out=\"$a\"; done\necho \"linker=${{{}-unset}}\" >&2\ncp '{}' \"$out\"",
            capabilities_json(),
            linker_variable(),
            index.display()
        ),
    );
    let override_arg = format!("rust={}", producer.display());
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        repo.join("bin").display(),
        chain.join("bin").display()
    );
    let env = [("PATH", path.as_str())];

    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &override_arg,
        ],
        &env,
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let check = json(&out)["doctor"]["indexers"][0].clone();
    assert!(
        !marker.exists(),
        "doctor: the checkout cc must never execute"
    );
    let refused = check["version"] == serde_json::json!({"Err": "rust_linker_refused"});
    if refused {
        assert_eq!(check["linker"], "refused", "{check}");
        assert!(stderr(&out).contains("indexer rust linker: rust_linker_refused"));
    } else {
        assert_eq!(
            check["version"],
            serde_json::json!({"Ok": "qualified"}),
            "{check}"
        );
        assert_eq!(check["linker"], "admitted", "{check}");
        assert!(stderr(&out).contains("indexer rust linker: admitted"));
    }
    let system_cc = fs::canonicalize("/usr/bin/cc").unwrap_or_default();
    let doctor_text = stderr(&out);
    assert!(
        !doctor_text.contains(repo.to_str().unwrap())
            && !doctor_text.contains("libexec")
            && (system_cc.as_os_str().is_empty()
                || !doctor_text.contains(system_cc.to_str().unwrap())),
        "doctor names the linker's class, never its path: {}",
        stderr(&out)
    );

    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--lang",
            "rust",
            "--indexer",
            &override_arg,
        ],
        &env,
    );
    assert!(
        !marker.exists(),
        "index: the checkout cc must never execute"
    );
    let snapshot = home
        .join("codegraph")
        .join(&id)
        .join(format!("{}.cg.db", head(&repo)));
    if refused {
        assert_eq!(code(&out), 4, "{}", stderr(&out));
        assert_eq!(json(&out)["class"], "rust_linker_refused");
        assert!(!snapshot.exists(), "nothing ran, nothing published");
        return;
    }
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        snapshot.is_file(),
        "the run proceeded with the system linker"
    );
    let log = fs::read_to_string(
        home.join("codegraph")
            .join(&id)
            .join(format!("{}.rust.a.log", head(&repo))),
    )
    .expect("the gated run log");
    let system_cc = fs::canonicalize("/usr/bin/cc").unwrap();
    assert!(
        log.lines()
            .any(|l| l == format!("linker={}", system_cc.display())),
        "the run is told the canonical system compiler: {log}"
    );
    assert!(
        !log.contains(repo.to_str().unwrap()),
        "the checkout never names the linker: {log}"
    );
}

#[test]
fn checkout_cc_on_path_is_never_the_linker_on_the_none_tier() {
    checkout_cc_is_never_the_linker(true);
}

#[test]
fn checkout_cc_on_path_is_never_the_linker_on_the_selected_tier() {
    let s = Scratch::new("linker-tier");
    if doctor_selected_tier(&s.home()) != "namespaces" {
        eprintln!("NOT RUN: checkout cc on the selected tier: namespaces unavailable");
        return;
    }
    checkout_cc_is_never_the_linker(false);
}

/// The resolver refuses a `cc` whose canonical target is not under `/usr/bin`, or that lies
/// under a forbidden root, before anything is probed: with an unavailable site the class is
/// the resolution's, not `isolation_unavailable`; a resolvable system compiler reaches the
/// probe, which the unavailable site refuses.
/// A selector requirement that fails after the linker was admitted (`profile = "default"` on a
/// chain whose `components` lists no `rustfmt`): `doctor` reports
/// `rust_toolchain_component_missing` and the linker row keeps the outcome of the three probes
/// that completed (astra ri4-3bc-3), never "not checked".
#[test]
fn doctor_linker_row_keeps_its_outcome_when_a_later_selector_requirement_fails() {
    // Arrange
    let s = Scratch::new("linker-row-selector");
    let home = s.home();
    let repo = rust_repo(&s.path().join("repo"));
    fs::write(
        repo.join("rust-toolchain.toml"),
        "[toolchain]\nprofile = \"default\"\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "default profile"]);
    trust(&home, &repo);
    let chain = fake_rust_chain(&s.path().join("chain"));
    let producer = fake_tool(
        &s.path().join("fakes"),
        "kaban-rust-scip",
        &format!(
            "case \"$1\" in\n  --version) echo qualified; exit 0;;\n  capabilities) echo '{}'; exit 0;;\nesac\nexit 99",
            capabilities_json()
        ),
    );
    let override_arg = format!("rust={}", producer.display());
    let path = format!("{}:/usr/bin:/bin", chain.join("bin").display());

    // Act
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &override_arg,
        ],
        &[("PATH", path.as_str())],
    );

    // Assert
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let check = json(&out)["doctor"]["indexers"][0].clone();
    assert_eq!(
        check["version"],
        serde_json::json!({"Err": "rust_toolchain_component_missing"}),
        "{check}"
    );
    let host_has_cc = fs::canonicalize("/usr/bin/cc").is_ok_and(|cc| cc.starts_with("/usr/bin"));
    let (row, text) = if host_has_cc {
        ("admitted", "indexer rust linker: admitted")
    } else {
        ("refused", "indexer rust linker: rust_linker_refused")
    };
    assert_eq!(
        check["linker"], row,
        "the linker probes completed before the selector refused: {check}"
    );
    assert!(stderr(&out).contains(text), "{}", stderr(&out));
}

#[test]
fn linker_outside_usr_bin_is_refused_before_any_probe() {
    let s = Scratch::new("linker-resolve");
    let marker = s.path().join("cc-ran");
    let elsewhere = fake_tool(
        s.path(),
        "gcc",
        &format!("touch '{}'; exit 0", marker.display()),
    );
    let link = s.path().join("cc");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    for cc in [link, elsewhere, s.path().join("missing-cc")] {
        assert_eq!(
            doctor::rust_linker(&cc, &[], ProbeSite::Unavailable).map(|_| ()),
            Err("rust_linker_refused"),
            "{}",
            cc.display()
        );
        assert_eq!(
            doctor::rust_linker(&cc, &[], ProbeSite::Native).map(|_| ()),
            Err("rust_linker_refused"),
            "{}",
            cc.display()
        );
    }
    assert!(!marker.exists(), "an unadmitted cc never executes");
    let system = Path::new("/usr/bin/cc");
    if fs::canonicalize(system).is_err() {
        eprintln!("NOT RUN: resolvable system compiler: /usr/bin/cc absent");
        return;
    }
    assert_eq!(
        doctor::rust_linker(system, &[Path::new("/usr")], ProbeSite::Unavailable).map(|_| ()),
        Err("rust_linker_refused"),
        "a forbidden root over the compiler refuses it"
    );
    assert_eq!(
        doctor::rust_linker(system, &[], ProbeSite::Unavailable).map(|_| ()),
        Err("isolation_unavailable"),
        "resolution passed; the probe is next and the unavailable site refuses it"
    );
}

// ---------------------------------------------------------------------------------------------
// Commit 4 (RI-3d): the consumer speaks contract v2

/// A producer still answering protocol 1 cannot carry the linker, so it is refused
/// `rust_capabilities_refused` through `doctor` (the indexer row) and `index` (exit 4, no
/// snapshot); the fake never reaches `index`.
#[test]
fn producer_answering_protocol_1_is_refused_through_doctor_and_index() {
    let s = Scratch::new("protocol-1");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    let repo = rust_repo(&s.path().join("repo"));
    let id = trust(&home, &repo);
    let chain = fake_rust_chain(&s.path().join("chain"));
    let marker = s.path().join("index-ran");
    let producer = fake_tool(
        &s.path().join("fakes"),
        "kaban-rust-scip",
        &format!(
            "case \"$1\" in\n  --version) echo qualified; exit 0;;\n  capabilities) echo '{}'; exit 0;;\nesac\ntouch '{}'; exit 99",
            capabilities_json().replace("\"protocol_version\":2", "\"protocol_version\":1"),
            marker.display()
        ),
    );
    let override_arg = format!("rust={}", producer.display());
    let path = format!("{}:/usr/bin:/bin", chain.join("bin").display());
    let env = [("PATH", path.as_str())];
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &override_arg,
        ],
        &env,
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        json(&out)["doctor"]["indexers"][0]["version"],
        serde_json::json!({"Err": "rust_capabilities_refused"})
    );
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--lang",
            "rust",
            "--indexer",
            &override_arg,
        ],
        &env,
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "rust_capabilities_refused");
    assert!(!marker.exists(), "a protocol-1 producer never indexes");
    assert!(
        !home
            .join("codegraph")
            .join(&id)
            .join(format!("{}.cg.db", head(&repo)))
            .exists()
    );
}

// ---------------------------------------------------------------------------------------------
// Commit 3 (astra ri4-3bc-2): the linker's helpers are admitted as narrowly as the linker

/// The compiler's helper answers (`cc1`, `collect2`, `crt1.o`) cross the same admission
/// boundary as `cc`: canonical regular files whose parent is under `/usr/libexec` or
/// `/usr/lib`, outside every forbidden root; only that canonical parent is bound. On the real
/// host the admitted helper directory is exactly the canonical parent of `cc1`, and a
/// forbidden root over it refuses the linker as a whole.
#[test]
fn linker_helpers_are_admitted_narrowly_and_under_the_same_forbidden_roots() {
    if rerun_native_test_with_clean_descriptors(
        "linker_helpers_are_admitted_narrowly_and_under_the_same_forbidden_roots",
    ) {
        return;
    }
    let system = Path::new("/usr/bin/cc");
    let Ok(cc) = fs::canonicalize(system) else {
        eprintln!("NOT RUN: linker helpers: /usr/bin/cc absent");
        return;
    };
    let answer = |arg: &str| {
        let out = Command::new(&cc).arg(arg).output().expect("cc answers");
        assert!(out.status.success(), "{arg}");
        PathBuf::from(String::from_utf8(out.stdout).expect("utf8").trim())
    };
    let cc1 = fs::canonicalize(answer("-print-prog-name=cc1")).expect("a real cc1");
    let helper_dir = cc1.parent().expect("cc1 has a directory").to_path_buf();
    assert!(
        helper_dir.starts_with("/usr/libexec") || helper_dir.starts_with("/usr/lib"),
        "this host keeps cc1 at {}",
        cc1.display()
    );
    // the admission seam itself: an answer under /usr, /usr/bin or /usr/local, a regular file
    // outside /usr, a relative answer and an answer under a forbidden root are refused; the
    // real cc1 is admitted at its canonical path
    let s = Scratch::new("linker-helper");
    let elsewhere = fake_tool(s.path(), "cc1", "exit 0");
    for answer in [
        PathBuf::from("/usr/cc1"),
        cc.clone(),
        PathBuf::from("/usr/local/cc1"),
        elsewhere,
        PathBuf::from("libexec/cc1"),
    ] {
        assert_eq!(
            doctor::linker_helper(&answer, &[]),
            Err("rust_linker_refused"),
            "{}",
            answer.display()
        );
    }
    assert_eq!(
        doctor::linker_helper(&cc1, &[&helper_dir]),
        Err("rust_linker_refused"),
        "a forbidden root over the helper refuses it"
    );
    assert_eq!(doctor::linker_helper(&cc1, &[]), Ok(cc1.clone()));
    let linker = doctor::rust_linker(system, &[], ProbeSite::Native).expect("the host linker");
    assert_eq!(
        linker.helper_dir, helper_dir,
        "exactly the canonical parent of cc1 is the bound resource"
    );
    assert_eq!(
        doctor::rust_linker(system, &[&helper_dir], ProbeSite::Native).map(|_| ()),
        Err("rust_linker_refused"),
        "a forbidden root over the helper directory refuses the linker"
    );
}

/// The directory a program helper admits is the one bound into the view, so it is itself
/// validated as one GCC helper leaf (astra ri3d-1): strictly below `/usr/libexec` or
/// `/usr/lib` by at least two components, overlapping no forbidden root in either direction.
/// A regular file directly under `/usr/lib` (`os-release`) and one a single level below (the
/// host's `crt1.o`, a linker object, not a program helper) are refused; a forbidden root that
/// is a descendant of the real helper directory refuses the helper and the linker as a whole;
/// the real `cc1` is admitted with exactly its canonical directory.
#[test]
fn linker_helper_directory_is_one_gcc_leaf() {
    if rerun_native_test_with_clean_descriptors("linker_helper_directory_is_one_gcc_leaf") {
        return;
    }
    let system = Path::new("/usr/bin/cc");
    let Ok(cc) = fs::canonicalize(system) else {
        eprintln!("NOT RUN: linker helper directory: /usr/bin/cc absent");
        return;
    };
    let answer = |arg: &str| {
        let out = Command::new(&cc).arg(arg).output().expect("cc answers");
        assert!(out.status.success(), "{arg}");
        fs::canonicalize(String::from_utf8(out.stdout).expect("utf8").trim()).expect(arg)
    };
    // (1) a helper whose directory is exactly `/usr/lib`
    let at_root = Path::new("/usr/lib/os-release");
    if at_root.is_file() {
        assert_eq!(
            doctor::linker_helper(at_root, &[]),
            Err("rust_linker_refused"),
            "a helper directly under /usr/lib would bind all of /usr/lib"
        );
    } else {
        eprintln!("NOT RUN: helper directly under /usr/lib: /usr/lib/os-release absent");
    }
    // (2) a helper whose directory is one level below `/usr/lib`
    let crt1 = answer("-print-file-name=crt1.o");
    let crt_dir = crt1.parent().expect("crt1.o has a directory");
    if crt_dir.parent() == Some(Path::new("/usr/lib")) {
        assert_eq!(
            doctor::linker_helper(&crt1, &[]),
            Err("rust_linker_refused"),
            "{} is one level below /usr/lib",
            crt1.display()
        );
    } else {
        eprintln!(
            "NOT RUN: helper one level below /usr/lib: crt1.o at {}",
            crt1.display()
        );
    }
    // (3) a forbidden root that is a descendant of the helper directory
    let cc1 = answer("-print-prog-name=cc1");
    let helper_dir = cc1.parent().expect("cc1 has a directory").to_path_buf();
    let private = helper_dir.join("private");
    assert_eq!(
        doctor::linker_helper(&cc1, &[&private]),
        Err("rust_linker_refused"),
        "mounting {} would expose the forbidden {}",
        helper_dir.display(),
        private.display()
    );
    assert_eq!(
        doctor::rust_linker(system, &[&private], ProbeSite::Native).map(|_| ()),
        Err("rust_linker_refused"),
        "a forbidden root below the helper directory refuses the linker"
    );
    // (4) the real helper is admitted with exactly its canonical directory
    assert_eq!(doctor::linker_helper(&cc1, &[]), Ok(cc1.clone()));
    let linker = doctor::rust_linker(system, &[], ProbeSite::Native).expect("the host linker");
    assert_eq!(linker.helper_dir, helper_dir);
}

// ---------------------------------------------------------------------------------------------
// Commit 2 (astra ri3-1): an inner `.git` entry never shrinks the checkout boundary

/// A repository that plants `member/.git` (a plain file, then a dangling symlink) does not
/// make `member` its own checkout: `doctor --repo repo/member`, through a symlink alias too,
/// still refuses `repo/bin/kaban-rust-scip` from `PATH` as `indexer_untrusted_location`, and
/// the fake producer never runs. The boundary is the outermost ancestor holding a `.git`
/// entry; over-exclusion is safe, under-exclusion is the defect.
#[test]
fn inner_git_entry_never_shrinks_the_checkout_boundary() {
    let s = Scratch::new("inner-git");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    let repo = s.path().join("repo");
    fs::create_dir_all(repo.join("member")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = ['member']\n",
    )
    .unwrap();
    fs::write(
        repo.join("member/Cargo.toml"),
        "[package]\nname = 'member'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let marker = s.path().join("producer-ran");
    fake_tool(
        &repo.join("bin"),
        "kaban-rust-scip",
        &format!("touch '{}'; exit 99", marker.display()),
    );
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "nested member and a checkout producer",
        ],
    );
    let repo = fs::canonicalize(&repo).unwrap();
    let link = s.path().join("alias");
    std::os::unix::fs::symlink(repo.join("member"), &link).unwrap();
    let checkout_bin = format!("{}:/usr/bin:/bin", repo.join("bin").display());
    let inner = repo.join("member/.git");
    let plant = |shape: &str| match shape {
        "file" => fs::write(&inner, "gitdir: nowhere\n").unwrap(),
        "dangling symlink" => {
            std::os::unix::fs::symlink(s.path().join("absent"), &inner).unwrap();
        }
        _ => unreachable!(),
    };
    for shape in ["file", "dangling symlink"] {
        let _ = fs::remove_file(&inner);
        plant(shape);
        for nested in [repo.join("member"), link.clone()] {
            let out = kaban_index_env(
                &home,
                &["doctor", "--repo", nested.to_str().unwrap()],
                &[("PATH", &checkout_bin)],
            );
            assert_eq!(code(&out), 0, "{}", stderr(&out));
            assert!(
                stderr(&out).contains(
                    "language: rust (markers: Cargo.toml) indexer: indexer_untrusted_location"
                ),
                "member/.git as a {shape}, --repo {}: {}",
                nested.display(),
                stderr(&out)
            );
            assert!(
                !marker.exists(),
                "member/.git as a {shape}: the checkout producer must never execute"
            );
        }
    }
}
