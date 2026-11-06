//! CG4.3 (stage1-spec G1-S5.1 selection, S5.2 wrapper handles, S5.3 clean environment, S5.4
//! process control, S5.6 quotas and raw-file deletion, S5.7 output gating; stage1-plan CG4.3;
//! the U1 residuals 1, 3 and 4) through the public surface of `kaban_index::isolate`,
//! `kaban_index::runner`, `kaban_index::quota` and `kaban_index::writer`, with the fake
//! indexers under `tests/fake-indexers/` on the `none` tier. Every scratch is under `/tmp`
//! and removed on drop (`test-integration-dir`, `test-fixture-raii`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kaban_core::process::{InheritedFds, KillReason, MonitorVerdict, ProcessError};
use kaban_index::detect::Lang;
use kaban_index::identity_cli::{self, AdmittedDir};
use kaban_index::isolate::{
    Detail, ISOLATION_NONE, PathView, Probe, ReasonClass, Scratch, SelectError, Tier, env_for,
    select,
};
#[cfg(target_os = "linux")]
use kaban_index::isolate::{WrapperError, resolve_wrapper};
use kaban_index::language;
use kaban_index::runner::{
    GoRun, IndexerSpec, IsolationRun, QuotaCaps, RunClass, RunError, RunReport, classify_outcome,
    run_indexer,
};
use kaban_index::trust::TRUST_TEXT;
use kaban_index::writer::{
    Commit, PublishError, clear_unsynced, dir_fsync, is_unsynced, publish_admitted,
};
use kaban_policy::Policy;

/// Explicit trusted producer qualification, never part of ordinary committed-data tests.
#[test]
#[ignore = "requires explicit trusted actual Rust producer and toolchain"]
fn rust_actual_producer_runner_publication_query() {
    use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
    let producer = PathBuf::from(
        std::env::var_os("KABAN_TEST_RUST_PRODUCER").expect("explicit trusted producer"),
    );
    let root = PathBuf::from(
        std::env::var_os("KABAN_TEST_RUST_SYSROOT").expect("explicit trusted real sysroot"),
    );
    let executable = kaban_index::tools::admit_executable(&producer, &[]).unwrap();
    let executables = [
        "bin/cargo",
        "bin/rustc",
        "libexec/rust-analyzer-proc-macro-srv",
        "bin/rustdoc",
    ]
    .map(|p| kaban_index::tools::admit_executable(&root.join(p), &[]).unwrap());
    kaban_index::doctor::rust_toolchain(&executables[1], kaban_index::doctor::ProbeSite::Native)
        .unwrap();
    // This explicit test qualifies execution before the producer advertises hosts. The
    // production admission path still insists on a supported host capability.
    let driver = language::Driver::Rust {
        root: root.clone(),
        executables: Box::new(executables),
        registry: None,
        linker: kaban_index::doctor::rust_linker(
            Path::new(kaban_index::doctor::SYSTEM_CC),
            &[],
            kaban_index::doctor::ProbeSite::Native,
        )
        .unwrap(),
    };
    for fixture in ["minimal", "workspace"] {
        let home = Home::new(&format!("rust-real-{fixture}"));
        let dir = home.repo_dir();
        let checkout = home.checkout();
        common::copy_trusted_tree(
            &common::fixtures_dir()
                .join("rust-qualification")
                .join(fixture),
            &checkout,
        );
        for args in [
            vec!["init", "-q"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-q",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("/usr/bin/git")
                    .current_dir(&checkout)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let commit_text = identity_cli::git(&checkout, &["rev-parse", "HEAD"]).unwrap();
        let checked_commit = Commit::parse(commit_text.trim()).unwrap();
        let scratch = Scratch::create(&dir).unwrap();
        let policy = Policy::empty();
        let invocation = driver
            .prepare(&checkout, &scratch, false, &common::commit(0x91))
            .unwrap()
            .remove(0);
        let mut run = spec(&producer, &[], &checkout, &scratch, &dir, &policy);
        run.lang = Lang::Rust;
        run.linker = driver.linker();
        run.args = invocation.args;
        run.indexer_identity = Some(executable.identity);
        run.tool_dirs.insert(0, root.join("bin"));
        run.inherited_fds = InheritedFds::Refuse;
        run.quota = QuotaCaps {
            file_bytes: 64 << 20,
            total_bytes: 128 << 20,
        };
        let report = run_indexer(run).unwrap();
        assert!(
            report.class.is_none(),
            "{fixture}: {report:?}; {}",
            fs::read_to_string(&report.log_path).unwrap()
        );
        let bytes = scratch.read_output(invocation.file, 64 << 20).unwrap();
        let gated = kaban_index::decode_and_gate(&bytes, &policy).unwrap();
        let admitted = kaban_index::admit_index(&gated);
        let rows = kaban_index::writer::Rows::derive_committed(
            &admitted,
            None,
            &policy,
            &checkout,
            &checked_commit,
        )
        .unwrap();
        let path = kaban_index::writer::publish(
            &dir.path,
            &rows,
            &common::meta(checked_commit.as_str()),
            &policy,
        )
        .unwrap();
        let snapshot = SqliteSnapshot::open_ro(&path).unwrap();
        assert!(
            snapshot.node("file:src/lib.rs").unwrap().is_some()
                || snapshot.node("file:app/src/lib.rs").unwrap().is_some()
        );
        let source = if fixture == "minimal" {
            "src/lib.rs"
        } else {
            "app/src/lib.rs"
        };
        let nodes = snapshot.nodes_in_file(source).unwrap();
        if fixture == "minimal" {
            let trait_node = nodes
                .iter()
                .find(|node| node.name == "DatabaseDriver")
                .unwrap();
            let implementers = snapshot.implementers(trait_node.id, false).unwrap();
            let names: std::collections::BTreeSet<_> =
                implementers.iter().map(|node| node.name.as_str()).collect();
            assert_eq!(
                names,
                std::collections::BTreeSet::from(["Postgres", "Sqlite"])
            );
            let member = snapshot
                .members(trait_node.id, &[])
                .unwrap()
                .into_iter()
                .find(|node| node.name == "execute")
                .unwrap();
            let graph = snapshot
                .neighbours(
                    member.id,
                    kaban_codegraph::Depth::new(1, &kaban_codegraph::Limits::DEFAULT).unwrap(),
                    &[kaban_codegraph::EdgeType::ImplementsMember],
                )
                .unwrap();
            assert_eq!(graph.edges.len(), 2);
            let production = nodes.iter().find(|node| node.name == "production").unwrap();
            assert_ne!(snapshot.references(production.id, false).unwrap(), []);
        }
        if fixture == "workspace" {
            assert!(nodes.iter().any(|node| node.name == "generated_value"));
            assert!(scratch.tmp().join("proc-macro-ran.txt").is_file());
            assert!(contains_file(
                &scratch.root().join("target"),
                "build-script-ran.txt"
            ));
        }
        assert!(
            !checkout.join("target").exists(),
            "build outputs stay in scratch"
        );
    }
}

fn contains_file(root: &Path, name: &str) -> bool {
    fs::read_dir(root).unwrap().any(|entry| {
        let entry = entry.unwrap();
        entry.file_name() == name
            || (entry.file_type().unwrap().is_dir() && contains_file(&entry.path(), name))
    })
}

/// A scratch home deleted on drop, test failure included.
struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-cg43-{tag}-{}-{}",
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

    /// The admitted `<home>/codegraph/<repo-id>` directory (U1 residual 1).
    fn repo_dir(&self) -> AdmittedDir {
        identity_cli::admit_repo_dir(&self.0, &"ab".repeat(16)).expect("admitted data dir")
    }

    /// A fresh checkout directory (the child's working directory).
    fn checkout(&self) -> PathBuf {
        let dir = self.0.join("checkout");
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fake(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fake-indexers")
        .join(name)
}

fn commit(n: u8) -> Commit {
    Commit::parse(&format!("{n:02x}").repeat(20)).unwrap()
}

/// The tool directories every fake indexer needs (`sh`, `env`, `sort`, `dd`, `sleep`).
fn tool_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
}

/// A spec on the `none` tier for `indexer` with `args`, 30 s deadline, 1 MiB output caps and
/// the default quotas; the test runner may itself hold a descriptor without `FD_CLOEXEC`
/// (nextest under `flock(1)` leaks its lock fd), so the launcher policy is `Tolerate` here
/// and `Refuse` is asserted by its own test.
fn spec<'a>(
    indexer: &'a Path,
    args: &[&str],
    checkout: &'a Path,
    scratch: &'a Scratch,
    dir: &'a AdmittedDir,
    policy: &'a Policy,
) -> IndexerSpec<'a> {
    IndexerSpec {
        lang: Lang::Go,
        indexer,
        args: args.iter().map(OsString::from).collect(),
        repo_root: checkout,
        tool_dirs: tool_dirs(),
        scratch,
        deadline: Duration::from_secs(30),
        output_cap: 1 << 20,
        quota: QuotaCaps {
            file_bytes: 1 << 30,
            total_bytes: 4 << 30,
        },
        data_dir: dir,
        commit: commit(0x11),
        policy,
        inherited_fds: InheritedFds::Tolerate,
        go: GoRun::default(),
        linker: None,
        indexer_identity: None,
        resource_dirs: Vec::new(),
        checkout: None,
        run: kaban_index::runner::RunSlot::A,
        isolation: IsolationRun::None,
    }
}

fn env_lines(report: &RunReport, dir: &AdmittedDir) -> BTreeMap<String, String> {
    let log = fs::read_to_string(&report.log_path).unwrap();
    assert!(
        report.log_path.starts_with(&dir.path),
        "the log lives in the admitted directory: {}",
        report.log_path.display()
    );
    log.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

// slice 1 (S5.3): the fake indexer prints its environment: exactly the S5.3 set for Go, `PATH`
// equal to the pinned tool path, `HOME` and `TMPDIR` inside the scratch, the offline
// constants, nothing of the caller's.
#[test]
fn rust_scratch_preparation_never_follows_child_links() {
    for directory in [false, true] {
        let home = Home::new("rust-link");
        let dir = home.repo_dir();
        let checkout = home.checkout();
        let scratch = Scratch::create(&dir).unwrap();
        let outside = home.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("config.toml");
        fs::write(&sentinel, "do-not-overwrite").unwrap();
        if directory {
            fs::remove_dir(scratch.cargo_home()).unwrap();
            std::os::unix::fs::symlink(&outside, scratch.cargo_home()).unwrap();
        } else {
            std::os::unix::fs::symlink(&sentinel, scratch.cargo_home().join("config.toml"))
                .unwrap();
        }
        let policy = Policy::empty();
        let indexer = fake("env.sh");
        let mut run = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
        run.lang = Lang::Rust;
        let result = run_indexer(run);
        assert_eq!(
            fs::read_to_string(&sentinel).unwrap(),
            "do-not-overwrite",
            "Rust preparation must never follow a prior child's scratch link"
        );
        assert!(result.is_err());
    }
}

#[test]
fn scratch_cargo_home_is_offline_and_private() {
    let home = Home::new("cargo-offline");
    let dir = home.repo_dir();
    let scratch = Scratch::create(&dir).unwrap();
    let checkout = home.checkout();
    let policy = Policy::empty();
    let indexer = fake("env.sh");
    let mut rust = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
    rust.lang = Lang::Rust;
    let report = run_indexer(rust).unwrap();
    assert!(report.class.is_none(), "{report:?}");
    let env = env_lines(&report, &dir);
    assert_eq!(env["CARGO_NET_OFFLINE"], "true");
    assert_eq!(
        env["CARGO_TARGET_DIR"],
        scratch.root().join("target").to_string_lossy()
    );
    assert!(!env.contains_key("RUSTC_WRAPPER"));
    assert!(!env.contains_key("CARGO_REGISTRIES_CRATES_IO_TOKEN"));
    let config = fs::read_to_string(scratch.cargo_home().join("config.toml"));
    assert!(
        config.is_ok(),
        "runner scratch must contain its own offline Cargo config"
    );
    assert_eq!(config.unwrap(), "[net]\noffline = true\n");
    assert!(!scratch.cargo_home().join("credentials.toml").exists());
    assert!(!scratch.cargo_home().join("registry").exists());
}

#[test]
fn runner_env_allowlist_only() {
    let home = Home::new("env");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("env.sh");
    let report = run_indexer(spec(&indexer, &[], &checkout, &scratch, &dir, &policy)).unwrap();
    assert!(report.class.is_none(), "{report:?}");
    let env = env_lines(&report, &dir);
    let mut names: Vec<&str> = env.keys().map(String::as_str).collect();
    names.retain(|n| *n != "cwd" && *n != "PWD" && *n != "_" && *n != "SHLVL" && *n != "OLDPWD");
    let mut expected = vec![
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "CARGO_HOME",
        "GOCACHE",
        "GOMODCACHE",
        "GOFLAGS",
        "GOPROXY",
        "GOSUMDB",
        "GOTOOLCHAIN",
    ];
    if std::env::var_os("CGO_ENABLED").is_some() {
        expected.push("CGO_ENABLED");
    }
    expected.sort_unstable();
    assert_eq!(names, expected, "exactly the S5.3 set for Go: {env:?}");
    assert_eq!(env["PATH"], "/usr/bin:/bin", "the pinned tool path");
    assert_eq!(env["HOME"], scratch.home().to_string_lossy());
    assert_eq!(env["TMPDIR"], scratch.tmp().to_string_lossy());
    assert_eq!(env["LANG"], "C.UTF-8");
    assert_eq!(env["LC_ALL"], "C");
    assert_eq!(env["GOPROXY"], "off");
    assert_eq!(env["GOSUMDB"], "off");
    assert_eq!(env["GOTOOLCHAIN"], "local");
    assert_eq!(env["GOFLAGS"], "-mod=readonly");
    assert!(env["GOCACHE"].starts_with(&*scratch.root().to_string_lossy()));
    assert!(env["GOMODCACHE"].starts_with(&*scratch.root().to_string_lossy()));
    assert_eq!(
        env["cwd"],
        checkout.to_string_lossy(),
        "cwd is the checkout"
    );
    assert_eq!(report.status.and_then(|s| s.code()), Some(0));
    assert_eq!(report.tier, Tier::None);
}

/// Whether `pid` is still a live process (a reaped or zombie process is not).
#[cfg(target_os = "linux")]
fn alive(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            let after = s.rsplit(')').next()?.trim_start();
            after.chars().next()
        })
        .is_some_and(|state| state != 'Z' && state != 'X')
}

#[cfg(target_os = "macos")]
fn alive(pid: &str) -> bool {
    let raw: u32 = pid.parse().expect("positive fixture PID");
    match kaban_core::process::process_liveness(raw) {
        kaban_core::process::Liveness::Dead => false,
        kaban_core::process::Liveness::Unknown => panic!("fixture PID observation unavailable"),
        kaban_core::process::Liveness::Alive => true,
    }
}

/// A descendant whose pid the fake indexer wrote to `pidfile`: `SIGKILL`ed on drop if alive
/// (`test-fixture-raii`).
struct Descendant(PathBuf);

impl Descendant {
    fn pid(&self) -> String {
        fs::read_to_string(&self.0).unwrap().trim().to_owned()
    }

    fn dead_within(&self, grace: Duration) -> bool {
        let pid = self.pid();
        let until = std::time::Instant::now() + grace;
        while alive(&pid) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        !alive(&pid)
    }
}

impl Drop for Descendant {
    fn drop(&mut self) {
        if let Some(pid) = fs::read_to_string(&self.0)
            .ok()
            .and_then(|t| t.trim().parse::<i32>().ok())
            .filter(|p| alive(&p.to_string()))
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

// slice 2 (S5.4): the fake indexer forks a grandchild holding the stdout pipe and hangs; at
// the deadline the whole group dies, the join is bounded, and the group-empty poll of U1
// residual 4 (b) reports no survivor.
#[test]
fn runner_kills_group_on_deadline() {
    let home = Home::new("deadline");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("sleep-fork.sh");
    let pidfile = scratch.tmp().join("grandchild.pid");
    let grandchild = Descendant(pidfile.clone());
    let deadline = Duration::from_millis(300);
    let mut s = spec(
        &indexer,
        &[&pidfile.to_string_lossy()],
        &checkout,
        &scratch,
        &dir,
        &policy,
    );
    s.deadline = deadline;
    let started = std::time::Instant::now();
    let report = run_indexer(s).unwrap();
    let took = started.elapsed();
    assert_eq!(report.class, Some(RunClass::IndexerTimeout), "{report:?}");
    assert_eq!(report.killed, Some(KillReason::Deadline));
    assert!(
        took < deadline + Duration::from_millis(500),
        "bounded join: {took:?}"
    );
    assert!(
        grandchild.dead_within(Duration::from_millis(200)),
        "grandchild {} survived the group kill",
        grandchild.pid()
    );
    assert!(
        !report.descendants_survived,
        "the group emptied within the bound"
    );
    // G1-F7: 7 indexer timed out (CG4.3 pinned the interim 1; CG4.4 applies the table)
    assert_eq!(RunClass::IndexerTimeout.exit_code(), 7);
}

// slice 3 (S5.4): stdout past the cap kills the group (`output_cap`, exit 6).
#[test]
fn runner_output_cap_kills() {
    let home = Home::new("cap");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("flood-stdout.sh");
    let mut s = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
    s.output_cap = 4096;
    let report = run_indexer(s).unwrap();
    assert_eq!(report.class, Some(RunClass::OutputCap), "{report:?}");
    assert_eq!(RunClass::OutputCap.exit_code(), 6);
    assert_eq!(RunClass::OutputCap.as_str(), "output_cap");
    assert!(
        report.wall < Duration::from_secs(5),
        "the cap, not the deadline: {:?}",
        report.wall
    );
    let log = fs::read(&report.log_path).unwrap();
    assert!(log.len() < 4096 + 64, "the log holds at most the cap");
}

/// Bytes available on the filesystem holding this run's temporary scratch.
fn tmp_available() -> u64 {
    let stat =
        rustix::fs::statvfs(std::env::temp_dir()).expect("scratch disk observer is required");
    stat.f_bavail.saturating_mul(stat.f_frsize)
}

// slice 4 (S5.6): the fake indexer writes 1 GiB + 1 byte of zeros into one file under the
// scratch; the per-file cap (1 GiB) kills the group (`quota_exceeded`, exit 6) and the raw
// file is gone with the scratch. Needs 2.5 GB free under `/tmp` (the brief's figure), else
// the test fails its prerequisite with the figure (never an early-return pass).
#[test]
fn runner_quota_file_kills() {
    const GIB: u64 = 1 << 30;
    let avail = tmp_available();
    assert!(
        avail >= 2_500_000_000,
        "runner_quota_file_kills prerequisite unavailable: scratch has {avail} bytes free"
    );
    let home = Home::new("quota-file");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("big-file.sh");
    let root = scratch.root().to_path_buf();
    let mut s = spec(
        &indexer,
        &[&root.to_string_lossy(), &(GIB + 1).to_string()],
        &checkout,
        &scratch,
        &dir,
        &policy,
    );
    s.deadline = Duration::from_secs(20);
    let report = run_indexer(s).unwrap();
    assert_eq!(report.class, Some(RunClass::QuotaExceeded), "{report:?}");
    assert_eq!(
        report.killed,
        Some(KillReason::Monitor {
            class: "quota_exceeded"
        })
    );
    assert_eq!(RunClass::QuotaExceeded.exit_code(), 6);
    assert!(
        report.observed_scratch_bytes > GIB,
        "the monitor saw the excess: {}",
        report.observed_scratch_bytes
    );
    drop(scratch);
    assert!(!root.exists(), "the scratch and the raw file are gone");
}

// slice 5 (S5.6): many small files across home, tmp, out and cache exceed the total cap while
// no single file exceeds the per-file cap: the total over every child-writable path counts.
#[test]
fn runner_quota_total_kills() {
    let home = Home::new("quota-total");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("many-files.sh");
    let root = scratch.root().to_path_buf();
    let mut s = spec(
        &indexer,
        &[&root.to_string_lossy(), "65536", "8"],
        &checkout,
        &scratch,
        &dir,
        &policy,
    );
    s.quota = QuotaCaps {
        file_bytes: 1 << 20,
        total_bytes: 1 << 20,
    };
    s.deadline = Duration::from_secs(20);
    let report = run_indexer(s).unwrap();
    assert_eq!(report.class, Some(RunClass::QuotaExceeded), "{report:?}");
    assert!(
        report.wall < Duration::from_secs(10),
        "the quota, not the deadline: {:?}",
        report.wall
    );
}

// slice 6 (S5.6, RS2-7): success, failure, timeout and quota: the scratch is gone after the
// run in every case; there is no option to keep it.
#[test]
fn runner_raw_files_always_deleted() {
    let home = Home::new("raw");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let policy = Policy::empty();
    let cases: [(&str, Vec<String>, Option<RunClass>, Duration); 4] = [
        (
            "exit-code.sh",
            vec!["0".into(), "4096".into(), String::new()],
            None,
            Duration::from_secs(30),
        ),
        (
            "exit-code.sh",
            vec!["3".into(), "4096".into(), String::new()],
            Some(RunClass::IndexerFailed),
            Duration::from_secs(30),
        ),
        (
            "hang.sh",
            vec![String::new()],
            Some(RunClass::IndexerTimeout),
            Duration::from_millis(300),
        ),
        (
            "many-files.sh",
            vec![String::new(), "65536".into(), "8".into()],
            Some(RunClass::QuotaExceeded),
            Duration::from_secs(20),
        ),
    ];
    for (name, args, expected, deadline) in cases {
        let scratch = Scratch::create(&dir).unwrap();
        let root = scratch.root().to_path_buf();
        let args: Vec<String> = args
            .into_iter()
            .map(|a| {
                if a.is_empty() {
                    root.to_string_lossy().into_owned()
                } else {
                    a
                }
            })
            .collect();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let indexer = fake(name);
        let mut s = spec(&indexer, &refs, &checkout, &scratch, &dir, &policy);
        s.deadline = deadline;
        s.quota = QuotaCaps {
            file_bytes: 1 << 20,
            total_bytes: 1 << 20,
        };
        let report = run_indexer(s).unwrap();
        assert_eq!(report.class, expected, "{name}: {report:?}");
        assert!(
            fs::read_dir(&root).unwrap().count() >= 5,
            "{name}: the scratch exists until the drop"
        );
        drop(scratch);
        assert!(!root.exists(), "{name}: the scratch is gone after the run");
        // the only artifact is the gated log
        let names: Vec<String> = fs::read_dir(&dir.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| n.ends_with(".go.a.log")),
            "{name}: only the log survives: {names:?}"
        );
    }
}

/// A policy with one reject rule for `pattern`.
fn reject_policy(pattern: &str) -> Policy {
    let home = std::env::temp_dir().join(format!("kaban-cg43-policy-{}", std::process::id()));
    fs::create_dir_all(&home).unwrap();
    let path = home.join(format!("{}.yaml", pattern.len()));
    fs::write(
        &path,
        format!(
            "version: 1\ndefaults:\n  action: reject\n  mask: \"[REDACTED:{{rule}}]\"\n  scan_on_read: true\nextend:\n  use_builtin: false\nrules:\n  - id: canary-token\n    description: test\n    regex: '{pattern}'\n    class: generic\n"
        ),
    )
    .unwrap();
    let policy = Policy::load(&path).unwrap();
    let _ = fs::remove_dir_all(&home);
    policy
}

/// Whether any regular file under `root` contains `needle` (byte scan, symlinks not followed).
fn any_file_contains(root: &Path, needle: &[u8]) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            let meta = entry.metadata().ok()?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file()
                && fs::read(&path)
                    .unwrap()
                    .windows(needle.len())
                    .any(|w| w == needle)
            {
                return Some(path);
            }
        }
    }
    None
}

// slice 7 (S5.7): the fake indexer prints a canary on stderr; the gated log holds the fixed
// marker `[removed: <rule id>]`, the canary is on no file under the data directory (byte
// scan), the log is 0600 and lives in the admitted directory.
#[test]
fn runner_log_gated_before_write() {
    let home = Home::new("gate");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let canary = "CANARY7f3a9c1d";
    let policy = reject_policy(canary);
    let indexer = fake("canary-stderr.sh");
    let report = run_indexer(spec(
        &indexer,
        &[canary],
        &checkout,
        &scratch,
        &dir,
        &policy,
    ))
    .unwrap();
    assert!(report.class.is_none(), "{report:?}");
    let log = fs::read_to_string(&report.log_path).unwrap();
    assert!(log.contains("[removed: canary-token]"), "{log}");
    assert!(
        log.contains("indexing started\n") && log.contains("done\n"),
        "{log}"
    );
    assert_eq!(
        fs::metadata(&report.log_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(scratch);
    assert_eq!(
        any_file_contains(home.path(), canary.as_bytes()),
        None,
        "the canary reached a file"
    );
}

fn probe(tier: Tier, available: Result<&str, ReasonClass>) -> Probe {
    Probe {
        tier,
        available: available.map(|d| Detail(d.to_owned())),
    }
}

// slice 8 (S5.1): stubbed probes x `preferred | required | off`: the expected tier, the
// `isolation_none` warning on the fallback, or exit 8 under `required`.
#[test]
fn tier_selection_matrix() {
    use kaban_core::config::Isolation::{Off, Preferred, Required};
    let ns = |ok: bool| {
        probe(
            Tier::Namespaces,
            if ok {
                Ok("unshare=/usr/bin/unshare kernel=6.1")
            } else {
                Err(ReasonClass::UnshareFailed)
            },
        )
    };
    let ct = |ok: bool| {
        probe(
            Tier::Container,
            if ok {
                Ok("docker=/usr/bin/docker")
            } else {
                Err(ReasonClass::ContainerImageNotConfigured)
            },
        )
    };
    let both = [ns(true), ct(true)];
    let container_only = [ns(false), ct(true)];
    let neither = [ns(false), ct(false)];
    // preferred: strongest first, then the fallback with the warning
    let s = select(Preferred, &both).unwrap();
    assert_eq!((s.tier, s.warning), (Tier::Namespaces, None));
    assert_eq!(s.detail.0, "unshare=/usr/bin/unshare kernel=6.1");
    let s = select(Preferred, &container_only).unwrap();
    assert_eq!((s.tier, s.warning), (Tier::Container, None));
    let s = select(Preferred, &neither).unwrap();
    assert_eq!((s.tier, s.warning), (Tier::None, Some(ISOLATION_NONE)));
    assert_eq!(s.detail.0, "none");
    // required: an isolating tier or exit 8 with the fixed text
    assert_eq!(select(Required, &both).unwrap().tier, Tier::Namespaces);
    assert_eq!(
        select(Required, &container_only).unwrap().tier,
        Tier::Container
    );
    let err = select(Required, &neither).unwrap_err();
    assert_eq!(err, SelectError::RequiredUnavailable);
    assert_eq!(err.exit_code(), 8);
    assert_eq!(
        err.to_string(),
        "isolation required but no isolating tier is available; indexing refused"
    );
    // off: none without consulting the probes, no warning
    let s = select(Off, &both).unwrap();
    assert_eq!((s.tier, s.warning), (Tier::None, None));
    // an empty probe list (nothing probed) behaves as neither
    assert_eq!(select(Required, &[]), Err(SelectError::RequiredUnavailable));
    assert_eq!(select(Preferred, &[]).unwrap().tier, Tier::None);
}

// slice 14 (RV-9): on a host without handle execution both wrapper probes are
// `handle_exec_unsupported`: `preferred` is `none` with the warning, `required` exits 8.
#[test]
fn tier_selection_non_linux() {
    use kaban_core::config::Isolation::{Preferred, Required};
    let probes = [
        probe(Tier::Namespaces, Err(ReasonClass::HandleExecUnsupported)),
        probe(Tier::Container, Err(ReasonClass::HandleExecUnsupported)),
    ];
    let s = select(Preferred, &probes).unwrap();
    assert_eq!((s.tier, s.warning), (Tier::None, Some(ISOLATION_NONE)));
    assert_eq!(
        select(Required, &probes),
        Err(SelectError::RequiredUnavailable)
    );
    assert_eq!(
        ReasonClass::HandleExecUnsupported.as_str(),
        "handle_exec_unsupported"
    );
    assert_eq!(
        serde_json::to_string(&probes[0]).unwrap(),
        r#"{"tier":"namespaces","available":{"Err":"handle_exec_unsupported"}}"#
    );
}

/// A single-utility ELF (`/bin/gnu<name>` where the distribution ships it, else `/bin/<name>`).
fn elf_tool(name: &str) -> PathBuf {
    let gnu = PathBuf::from(format!("/bin/gnu{name}"));
    if gnu.is_file() {
        gnu
    } else if Path::new(&format!("/bin/{name}")).is_file() {
        PathBuf::from(format!("/bin/{name}"))
    } else {
        PathBuf::from(format!("/usr/bin/{name}"))
    }
}

// slice 9 (S5.2): `PATH` holds a fake `unshare` and a fake `docker` that would each create a
// canary file; the candidate list names paths that do not exist; neither fake is resolved or
// executed (`PATH` is never consulted), and the canary file is absent.
#[cfg(target_os = "linux")]
#[test]
fn wrapper_from_candidate_list_only() {
    let home = Home::new("wrapper-path");
    let bin = home.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let canary = home.path().join("executed");
    for name in ["unshare", "docker"] {
        let p = bin.join(name);
        fs::write(&p, format!("#!/bin/sh\ntouch '{}'\n", canary.display())).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // `set_var` is unsafe in edition 2024, so the fakes are not placed on this process's
    // `PATH`; the point holds by construction: the resolver takes no `PATH` at all, only the
    // absolute candidate list, so a `PATH` entry can never be consulted
    let absent = home.path().join("absent");
    let candidates: Vec<PathBuf> = vec![absent.join("unshare"), absent.join("docker")];
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    let err = resolve_wrapper(&refs, None).unwrap_err();
    assert!(matches!(err, WrapperError::NotFound), "{err:?}");
    assert_eq!(err.class(), "wrapper_not_found");
    assert!(!canary.exists(), "a PATH wrapper was executed");
    // a real ELF at a candidate path resolves through its handle and reports that path
    let good = home.path().join("unshare");
    fs::copy(elf_tool("true"), &good).unwrap();
    let candidates = [absent.join("unshare"), good.clone()];
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    let resolved = resolve_wrapper(&refs, None).unwrap();
    assert_eq!(resolved.path, good);
    assert!(matches!(
        resolved.exec,
        kaban_core::process::Exec::Handle(_)
    ));
    assert!(!canary.exists());
}

// slice 10 (S5.2): a group-writable candidate is refused (`wrapper_writable`), and a refused
// candidate is a finding: the next candidate is not tried over it.
#[cfg(target_os = "linux")]
#[test]
fn wrapper_rejects_writable_candidate() {
    let home = Home::new("wrapper-writable");
    let loose = home.path().join("unshare");
    fs::copy(elf_tool("true"), &loose).unwrap();
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o775)).unwrap();
    let good = home.path().join("unshare-ok");
    fs::copy(elf_tool("true"), &good).unwrap();
    let candidates = [loose.clone(), good];
    let refs: Vec<&Path> = candidates.iter().map(PathBuf::as_path).collect();
    let err = resolve_wrapper(&refs, None).unwrap_err();
    assert!(
        matches!(err, WrapperError::Refused("wrapper_writable")),
        "{err:?}"
    );
    // the configured path goes through the same rule, and must be absolute
    let err = resolve_wrapper(&[], Some(&loose)).unwrap_err();
    assert!(
        matches!(err, WrapperError::Refused("wrapper_writable")),
        "{err:?}"
    );
    let err = resolve_wrapper(&[], Some(Path::new("docker"))).unwrap_err();
    assert!(matches!(err, WrapperError::NotAbsolute), "{err:?}");
    // a symlink at a candidate path is the link itself under O_NOFOLLOW: not a regular file
    let link = home.path().join("link");
    std::os::unix::fs::symlink(elf_tool("true"), &link).unwrap();
    let err = resolve_wrapper(&[&link], None).unwrap_err();
    assert!(
        matches!(err, WrapperError::Refused("wrapper_not_regular")),
        "{err:?}"
    );
}

// slice 11 (S5.2, RV-9): a `#!` file at a candidate path is `wrapper_not_elf` and never runs.
#[cfg(target_os = "linux")]
#[test]
fn wrapper_rejects_script_candidate() {
    let home = Home::new("wrapper-script");
    let canary = home.path().join("executed");
    let script = home.path().join("unshare");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", canary.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let err = resolve_wrapper(&[&script], None).unwrap_err();
    assert!(
        matches!(err, WrapperError::Refused("wrapper_not_elf")),
        "{err:?}"
    );
    assert_eq!(err.class(), "wrapper_not_elf");
    assert!(!canary.exists());
}

// slice 12 (spec §7): a left-over `run-<pid>` of a dead process is removed by the next run's
// scratch creation; a live process's scratch is left alone.
#[test]
fn leftover_scratch_removed_by_next_run() {
    let home = Home::new("leftover");
    let dir = home.repo_dir();
    // a pid that is not alive: the highest possible pid is never ours
    let dead = dir.path.join("run-4194303");
    fs::create_dir_all(dead.join("out")).unwrap();
    fs::write(dead.join("out/index.scip"), b"stale").unwrap();
    let other_live = dir.path.join("run-1");
    fs::create_dir_all(&other_live).unwrap();
    let unknown = dir.path.join("run-4294967295");
    fs::create_dir_all(&unknown).unwrap();
    let unknown_temp = dir.path.join("unknown.cg.db.tmp-4294967295");
    fs::write(&unknown_temp, b"preserve").unwrap();
    let live_temp = dir.path.join("live.cg.db.tmp-1");
    fs::write(&live_temp, b"preserve").unwrap();
    let scratch = Scratch::create(&dir).unwrap();
    assert!(
        unknown.exists(),
        "unknown PID must never authorize scratch removal"
    );
    let dead_temp = dir.path.join("dead.cg.db.tmp-4194303");
    fs::write(&dead_temp, b"stale").unwrap();
    assert_eq!(kaban_index::gc::sweep_leftovers(&dir), 1);
    assert!(!dead_temp.exists());
    assert!(unknown.exists() && unknown_temp.exists() && live_temp.exists());
    assert!(scratch.root().exists(), "gc preserves the current run too");
    assert!(!dead.exists(), "the dead run's scratch is removed");
    assert!(other_live.exists(), "pid 1 is alive: its scratch stays");
    assert!(scratch.root().exists());
    assert_eq!(
        fs::metadata(scratch.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for sub in ["home", "tmp", "out", "cache", "cargo-home"] {
        assert_eq!(
            fs::metadata(scratch.root().join(sub))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "{sub}"
        );
    }
    let root = scratch.root().to_path_buf();
    drop(scratch);
    assert!(!root.exists());
    let _ = fs::remove_dir_all(other_live);
}

// slice 13 (G1; gofix1-7: no cache policy any more): `GOMODCACHE` is the driver's admitted
// cache on every tier, by its host path, and a dedicated empty directory under the scratch only
// when there is none; the build cache stays scratch-scoped.
#[test]
fn module_cache_is_the_drivers_on_every_tier() {
    let home = Home::new("shared-rw");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("env.sh");
    let modcache = home.path().join("modcache");
    fs::create_dir_all(&modcache).unwrap();
    let mut s = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
    s.go = GoRun {
        modcache: Some(&modcache),
        vendored: false,
        goroot: None,
        toolchain: None,
    };
    let report = run_indexer(s).unwrap();
    assert!(report.class.is_none(), "{report:?}");
    assert!(report.warnings.is_empty(), "{report:?}");
    let env = env_lines(&report, &dir);
    assert_eq!(
        env["GOMODCACHE"],
        modcache.to_string_lossy(),
        "the admitted module cache, by its host path"
    );
    assert!(
        env["GOCACHE"].starts_with(&*scratch.root().to_string_lossy()),
        "the build cache stays scratch-scoped"
    );
    // no admitted module cache: a dedicated empty one under the scratch, whatever the policy
    let dedicated = env_for(
        Lang::Go,
        &tool_dirs(),
        &scratch,
        PathView::Host,
        GoRun {
            modcache: None,
            vendored: true,
            goroot: None,
            toolchain: None,
        },
        None,
    )
    .unwrap();
    assert!(
        dedicated
            .get("GOMODCACHE")
            .unwrap()
            .to_string_lossy()
            .starts_with(&*scratch.root().to_string_lossy())
    );
    assert_eq!(dedicated.get("GOFLAGS").unwrap(), "-mod=vendor");
    // the admitted cache keeps its host path inside an isolating view (bound read-only there)
    let isolated = env_for(
        Lang::Go,
        &tool_dirs(),
        &scratch,
        PathView::Namespaces,
        GoRun {
            modcache: Some(&modcache),
            vendored: false,
            goroot: None,
            toolchain: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(isolated.get("GOMODCACHE").unwrap(), modcache.as_os_str());
    assert_eq!(isolated.get("GOFLAGS").unwrap(), "-mod=readonly");
    // the container view spells the fixed mount points (RV-8)
    let container = env_for(
        Lang::TypeScript,
        &tool_dirs(),
        &scratch,
        PathView::Container,
        GoRun::default(),
        None,
    )
    .unwrap();
    assert_eq!(container.get("HOME").unwrap(), "/scratch/home");
    assert_eq!(container.get("npm_config_offline").unwrap(), "true");
    assert_eq!(
        container.get("npm_config_cache").unwrap(),
        "/scratch/cache/npm"
    );
    assert!(container.get("NODE_OPTIONS").is_none());
    assert!(
        container.get("GOPROXY").is_none(),
        "no Go block for TypeScript"
    );
}

// gofix1-3 (FIX-E): the module-cache `go` the run executes is rechecked by the runner at
// every spawn: the object at its admitted path changed (same path, new content) refuses the
// run before the spawn with `RunError::ToolchainChanged`.
#[test]
fn cached_go_is_rechecked_at_the_spawn() {
    let home = Home::new("go-recheck");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("env.sh");
    let root = home.path().join("toolchain");
    fs::create_dir_all(root.join("bin")).unwrap();
    let go = root.join("bin").join("go");
    fs::write(&go, "#!/bin/sh\necho go version go1.27.0 linux/arm64\n").unwrap();
    fs::set_permissions(&go, fs::Permissions::from_mode(0o755)).unwrap();
    let admitted = kaban_index::tools::admit_executable(&go, &[]).unwrap();
    let run = |admitted: &kaban_index::tools::AdmittedTool| {
        let mut s = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
        s.go = GoRun {
            modcache: None,
            vendored: false,
            goroot: Some(&root),
            toolchain: Some(admitted),
        };
        run_indexer(s)
    };
    let report = run(&admitted).unwrap();
    assert!(report.class.is_none(), "{report:?}");
    let mut file = fs::OpenOptions::new().append(true).open(&go).unwrap();
    std::io::Write::write_all(&mut file, b"# replaced\n").unwrap();
    drop(file);
    assert!(
        matches!(
            run(&admitted),
            Err(kaban_index::runner::RunError::ToolchainChanged)
        ),
        "the replaced go refuses the spawn"
    );
}

// slice 15 (G1-N6, RV-10): the fake indexer becomes a `dd` with a 64 MiB buffer:
// `peak_rss_bytes >= 64 MiB` in the run report.
#[test]
fn runner_reports_peak_rss() {
    let home = Home::new("rss");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    #[cfg(target_os = "linux")]
    let (indexer, args) = (fake("alloc.sh"), vec!["64"]);
    #[cfg(not(target_os = "linux"))]
    let (indexer, args) = (fake("env.sh"), vec![]);
    let report = run_indexer(spec(&indexer, &args, &checkout, &scratch, &dir, &policy)).unwrap();
    assert!(report.class.is_none(), "{report:?}");
    #[cfg(target_os = "linux")]
    {
        let peak = report.peak_rss_bytes.expect("sampled at least once");
        assert!(peak >= 64 << 20, "peak {peak} bytes");
        assert_eq!(report.monitor_verdicts.len(), 2, "quota and RSS");
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(report.peak_rss_bytes, None, "RSS explicitly unavailable");
        assert_eq!(
            report.monitor_verdicts,
            vec![MonitorVerdict::Ok],
            "scratch quotas still enforced"
        );
    }
}

// negative path (S5.6 residual, documented): on the `none` tier the fake indexer can write
// outside the scratch (into the checkout); the write succeeds, the quota does not see it, and
// the trust text names the residual. Pinned, not hidden.
#[test]
fn none_tier_child_can_write_outside_scratch_is_the_documented_residual() {
    let home = Home::new("outside");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("write-checkout.sh");
    let mut s = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
    s.quota = QuotaCaps {
        file_bytes: 1,
        total_bytes: 1,
    };
    let report = run_indexer(s).unwrap();
    assert!(report.class.is_none(), "{report:?}");
    assert!(
        checkout.join("outside-scratch.txt").exists(),
        "the write outside the scratch succeeded (none tier)"
    );
    assert_eq!(
        report.observed_scratch_bytes, 0,
        "the quota covers the scratch only"
    );
    assert!(
        TRUST_TEXT
            .iter()
            .any(|l| l.contains("Under the none tier they see your files and network as you do.")),
        "the trust text names the residual"
    );
    assert_eq!(report.tier.as_str(), "none");
}

// the indexer runner refuses the reserved `container` tier (deferred past 3.0.0, OWN-68
// SC-11; `namespaces` runs since CG4.4) with a fixed class and without a spawn
#[test]
fn runner_refuses_tiers_not_implemented_in_this_stage() {
    let home = Home::new("tier-na");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let canary = home.path().join("ran");
    let indexer = home.path().join("mark.sh");
    fs::write(
        &indexer,
        format!("#!/bin/sh\ntouch '{}'\n", canary.display()),
    )
    .unwrap();
    fs::set_permissions(&indexer, fs::Permissions::from_mode(0o755)).unwrap();
    let mut s = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
    s.isolation = IsolationRun::Container;
    let err = run_indexer(s).unwrap_err();
    assert!(
        matches!(err, RunError::TierNotImplemented(Tier::Container)),
        "{err:?}"
    );
    assert!(!canary.exists());
}

// U1 residual 4, descriptor policy: indexers refuse an inherited non-CLOEXEC descriptor by
// default (`Refuse` is `IndexerSpec`'s documented default policy); under a launcher that leaks
// one the refusal names the inherited descriptor (`InheritedFd { fd }`, G7) before any spawn.
#[test]
fn runner_refuses_inherited_descriptors_before_spawn() {
    let home = Home::new("fds");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let leak_path = home.path().join("inherited.txt");
    fs::write(&leak_path, "leak").unwrap();
    let leak = fs::File::open(&leak_path).unwrap();
    rustix::io::fcntl_setfd(&leak, rustix::io::FdFlags::empty()).unwrap();
    let canary = home.path().join("ran");
    let indexer = home.path().join("mark.sh");
    fs::write(
        &indexer,
        format!("#!/bin/sh\ntouch '{}'\n", canary.display()),
    )
    .unwrap();
    fs::set_permissions(&indexer, fs::Permissions::from_mode(0o755)).unwrap();
    let mut s = spec(&indexer, &[], &checkout, &scratch, &dir, &policy);
    s.inherited_fds = InheritedFds::Refuse;
    let err = run_indexer(s).unwrap_err();
    assert!(
        matches!(
            err,
            RunError::Process(ProcessError::InheritedFd { fd: Some(_) })
        ),
        "{err:?}"
    );
    assert!(!canary.exists(), "nothing spawned");
    drop(leak);
    assert!(matches!(InheritedFds::default(), InheritedFds::Refuse));
}

mod common;

/// `Rows` from the committed `typed-only` synthetic index under an empty policy.
fn sample_rows(policy: &Policy) -> (kaban_index::writer::Rows, Vec<u8>) {
    let bytes = common::fixture_bytes("synthetic/typed-only.scip");
    let gated = common::gated(&bytes, policy);
    let admitted = kaban_index::admit_index(&gated);
    (common::rows(&admitted, None, policy), bytes)
}

// U1 residual 1 (second half): the publication goes through the admitted directory handle:
// the temp file, the rename and the directory fsync; the snapshot lands at the handle's path,
// is 0600, opens as a snapshot, and a path-spelled `publish` through a symlinked directory
// is refused (the walk) with nothing created behind the link.
#[test]
fn publish_goes_through_the_admitted_directory_handle() {
    let home = Home::new("publish-handle");
    let dir = home.repo_dir();
    let policy = Policy::empty();
    let (rows, _) = sample_rows(&policy);
    let meta = common::meta(&"5a".repeat(20));
    let published = publish_admitted(&dir.handle, &rows, &meta, &policy, dir_fsync).unwrap();
    assert_eq!(published.name, format!("{}.cg.db", "5a".repeat(20)));
    let path = dir.path.join(&published.name);
    assert_eq!(published.bytes, fs::metadata(&path).unwrap().len());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(kaban_codegraph::SqliteSnapshot::open_ro(&path).is_ok());
    assert!(!is_unsynced(&dir.handle, &meta.commit));
    // a second publication of the same commit is `Exists`, the temp file is gone
    let err = publish_admitted(&dir.handle, &rows, &meta, &policy, dir_fsync).unwrap_err();
    assert!(matches!(err, PublishError::Exists), "{err:?}");
    assert_eq!(err.class(), "snapshot_exists");
    let names: Vec<String> = fs::read_dir(&dir.path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
    // the path-spelled convenience canonicalises once and re-walks into a handle: through a
    // symlink spelling the snapshot lands at the canonical directory (the walk refuses only a
    // component that is still a symlink after canonicalisation, CG4.1's test); a path that is
    // not a directory is refused with the walk class and nothing is created
    let link = home.path().join("link-to-data");
    std::os::unix::fs::symlink(&dir.path, &link).unwrap();
    let meta2 = common::meta(&"5b".repeat(20));
    let via_link = kaban_index::writer::publish(&link, &rows, &meta2, &policy).unwrap();
    assert_eq!(
        via_link,
        dir.path.join(format!("{}.cg.db", "5b".repeat(20)))
    );
    let file = home.path().join("not-a-dir");
    fs::write(&file, b"x").unwrap();
    let meta3 = common::meta(&"5e".repeat(20));
    let err = kaban_index::writer::publish(&file, &rows, &meta3, &policy).unwrap_err();
    assert!(matches!(err, PublishError::Dir(_)), "{err:?}");
    assert_eq!(err.class(), "data_dir_refused");
    assert!(!dir.path.join(format!("{}.cg.db", "5e".repeat(20))).exists());
}

fn failing_dir_fsync(_: std::os::fd::BorrowedFd<'_>) -> Result<(), rustix::io::Errno> {
    Err(rustix::io::Errno::IO)
}

// U1 residual 3: when the directory fsync after the rename fails, the publication is
// `published_unsynced` (fixed class, exit non-zero): the snapshot is present at its final
// name and readable, the `.unsynced` marker beside it says "re-verify before trusting", and
// clearing the marker after a re-verification makes the next run treat it as ordinary.
#[test]
fn publish_reports_published_unsynced_when_the_directory_fsync_fails() {
    let home = Home::new("unsynced");
    let dir = home.repo_dir();
    let policy = Policy::empty();
    let (rows, _) = sample_rows(&policy);
    let meta = common::meta(&"5c".repeat(20));
    let err = publish_admitted(&dir.handle, &rows, &meta, &policy, failing_dir_fsync).unwrap_err();
    let PublishError::Unsynced { name, source } = &err else {
        panic!("{err:?}");
    };
    let path = &dir.path.join(name);
    assert_eq!(*source, rustix::io::Errno::IO);
    assert_eq!(err.class(), "published_unsynced");
    assert_ne!(err.exit_code(), 0);
    assert!(path.exists(), "the snapshot is present");
    assert!(
        kaban_codegraph::SqliteSnapshot::open_ro(path).is_ok(),
        "present and readable: a fresh open re-verifies it (quick_check on first open)"
    );
    assert!(is_unsynced(&dir.handle, &meta.commit), "the marker is set");
    assert_eq!(
        fs::metadata(dir.path.join(format!("{}.cg.db.unsynced", meta.commit)))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    clear_unsynced(&dir.handle, &meta.commit).unwrap();
    assert!(!is_unsynced(&dir.handle, &meta.commit));
    clear_unsynced(&dir.handle, &meta.commit).unwrap();
    assert!(
        !is_unsynced(&dir.handle, &commit(0x5d)),
        "another commit is not marked"
    );
}

/// A monitor that returns `Err` on every sample.
struct AlwaysFail(Duration);

impl kaban_core::process::Monitor for AlwaysFail {
    fn sample(&mut self) -> Result<(), KillReason> {
        Err(KillReason::Monitor {
            class: "quota_exceeded",
        })
    }

    fn interval(&self) -> Duration {
        self.0
    }
}

/// A monitor whose every sample blocks for `sleep`.
struct Blocking(Duration, Duration);

impl kaban_core::process::Monitor for Blocking {
    fn sample(&mut self) -> Result<(), KillReason> {
        std::thread::sleep(self.0);
        Ok(())
    }

    fn interval(&self) -> Duration {
        self.1
    }
}

fn core_spec(
    exec: kaban_core::process::Exec,
    cwd: &Path,
    monitors: Vec<Box<dyn kaban_core::process::Monitor>>,
    join_bound: Duration,
) -> kaban_core::process::Spec<'_> {
    kaban_core::process::Spec {
        exec,
        args: Vec::new(),
        cwd,
        env: kaban_core::process::EnvAllowlist::new(),
        deadline: Duration::from_secs(5),
        stdout_cap: 1024,
        stderr_cap: 0,
        monitors,
        join_bound,
        inherited_fds: InheritedFds::Tolerate,
    }
}

// U1 residual 4 (c) and (d): a run shorter than the monitor's interval is still judged by one
// final sample at stop; a verdict that arrives only then never becomes `killed` (the
// late-sample drop) but is the monitor's verdict, and the runner maps it to its class; a
// monitor that produces no verdict within the join is `Silent` and maps to
// `failed:monitor_silent`, never `ok`.
#[test]
fn late_and_silent_monitor_verdicts_fail_closed() {
    let home = Home::new("verdicts");
    // (d) final sample: the child exits at once, the interval is far away
    let outcome = kaban_core::process::run(core_spec(
        kaban_core::process::Exec::Path(elf_tool("true")),
        home.path(),
        vec![Box::new(AlwaysFail(Duration::from_secs(60)))],
        Duration::from_secs(2),
    ))
    .unwrap();
    assert_eq!(
        outcome.killed, None,
        "a late verdict is not a kill: {outcome:?}"
    );
    assert!(outcome.status.unwrap().success());
    assert_eq!(
        outcome.monitor_verdicts,
        vec![MonitorVerdict::Failed(KillReason::Monitor {
            class: "quota_exceeded"
        })]
    );
    assert_eq!(classify_outcome(&outcome), Some(RunClass::QuotaExceeded));
    // a clean monitor on the same short run: Ok, and the run is ok
    let outcome = kaban_core::process::run(core_spec(
        kaban_core::process::Exec::Path(elf_tool("true")),
        home.path(),
        vec![Box::new(Blocking(Duration::ZERO, Duration::from_secs(60)))],
        Duration::from_secs(2),
    ))
    .unwrap();
    assert_eq!(outcome.monitor_verdicts, vec![MonitorVerdict::Ok]);
    assert_eq!(classify_outcome(&outcome), None);
    // silent: the final sample blocks past the join bound
    let outcome = kaban_core::process::run(core_spec(
        kaban_core::process::Exec::Path(elf_tool("true")),
        home.path(),
        vec![Box::new(Blocking(
            Duration::from_secs(3),
            Duration::from_secs(60),
        ))],
        Duration::from_millis(100),
    ))
    .unwrap();
    assert_eq!(outcome.killed, None);
    assert_eq!(outcome.monitor_verdicts, vec![MonitorVerdict::Silent]);
    assert_eq!(classify_outcome(&outcome), Some(RunClass::MonitorSilent));
    assert_eq!(RunClass::MonitorSilent.as_str(), "failed:monitor_silent");
    // G1-F7: 6 indexer failed (the monitor classes mean the indexer did not end `ok`)
    assert_eq!(RunClass::MonitorSilent.exit_code(), 6);
    assert_eq!(RunClass::IndexerFailed.exit_code(), 6);
    assert_eq!(RunClass::IsolationSelfcheckFailed.exit_code(), 8);
    // precedence: a kill during the run outranks the verdicts, and a descendants flag outranks
    // a silent monitor (both fixed classes)
    assert_eq!(
        RunClass::DescendantsSurvived.as_str(),
        "descendants_survived"
    );
}

// U1 residual 4 (a): the handle checks are a point check immediately before `exec`: a wrapper
// resolved as an acceptable handle whose same inode becomes group-writable before the run is
// refused at the spawn boundary (`wrapper_writable`) and never executes.
#[cfg(target_os = "linux")]
#[test]
fn wrapper_handle_is_rechecked_immediately_before_exec() {
    let home = Home::new("point-check");
    let wrapper = home.path().join("unshare");
    fs::copy(elf_tool("true"), &wrapper).unwrap();
    let resolved = resolve_wrapper(&[&wrapper], None).unwrap();
    // same inode, mode changed after the open
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o775)).unwrap();
    let err = kaban_core::process::run(core_spec(
        resolved.exec,
        home.path(),
        Vec::new(),
        Duration::from_secs(1),
    ))
    .unwrap_err();
    assert!(
        matches!(err, ProcessError::Env("wrapper_writable")),
        "{err:?}"
    );
    // and the unchanged handle still runs the checked bytes
    let truth = elf_tool("true");
    let resolved = resolve_wrapper(&[truth.as_path()], None).unwrap();
    let outcome = kaban_core::process::run(core_spec(
        resolved.exec,
        home.path(),
        Vec::new(),
        Duration::from_secs(1),
    ))
    .unwrap();
    assert!(outcome.status.unwrap().success());
    assert!(!outcome.descendants_survived);
}

// U2 RU2-10 (GM4c): the publication binds SQLite's writes to the inode created through the
// admitted directory handle. The admitted directory is renamed after admission and a decoy
// directory appears at its old path holding an empty file under the publisher's temp name:
// the snapshot must land in the admitted directory (under its new name), the decoy must stay
// empty, nothing may appear at the old path, and what is published must be the written,
// readable snapshot.
#[test]
#[allow(clippy::print_stderr)] // the observed figures are the evidence of the red and the green
fn publish_binds_sqlite_output_to_the_admitted_inode_under_a_renamed_directory() {
    let home = Home::new("publish-rename");
    let dir = home.repo_dir();
    let policy = Policy::empty();
    let (rows, _) = sample_rows(&policy);
    let meta = common::meta(&"5f".repeat(20));
    let moved = home.path().join("moved");
    fs::rename(&dir.path, &moved).unwrap();
    fs::create_dir(&dir.path).unwrap();
    fs::set_permissions(&dir.path, fs::Permissions::from_mode(0o700)).unwrap();
    let tmp_name = format!("{}.cg.db.tmp-{}", meta.commit, std::process::id());
    fs::write(dir.path.join(&tmp_name), b"").unwrap();
    let result = publish_admitted(&dir.handle, &rows, &meta, &policy, dir_fsync);
    let final_name = format!("{}.cg.db", meta.commit);
    let decoy_bytes = fs::metadata(dir.path.join(&tmp_name)).map_or(0, |m| m.len());
    let decoy_final = dir.path.join(&final_name).exists();
    let published = moved.join(&final_name);
    eprintln!(
        "result={result:?} decoy_tmp_bytes={decoy_bytes} decoy_final={decoy_final} published_in_admitted_dir={}",
        published.exists()
    );
    assert_eq!(
        decoy_bytes, 0,
        "nothing is written outside the admitted directory"
    );
    assert!(!decoy_final, "no snapshot at the old path");
    assert!(result.is_ok(), "{result:?}");
    assert!(
        published.exists(),
        "the snapshot is in the admitted directory"
    );
    assert!(
        kaban_codegraph::SqliteSnapshot::open_ro(&published).is_ok(),
        "the published file is the written snapshot, not an empty inode"
    );
}

// backlog 22 (PERF-1): a run that ends `indexer_failed` says how the child died, through the
// production entry point: the typed failure names the signal (a SIGKILL from the host) or the
// exit code, with the resources the monitors saw and the fact that no quota was breached; a run
// that ended `ok` carries none.
#[test]
fn failed_run_reports_how_it_died() {
    let home = Home::new("died");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let policy = Policy::empty();
    // killed by SIGKILL: the signal, no exit code, no quota breach
    let scratch = Scratch::create(&dir).unwrap();
    let indexer = fake("kill-self.sh");
    let report = run_indexer(spec(&indexer, &[], &checkout, &scratch, &dir, &policy)).unwrap();
    assert_eq!(report.class, Some(RunClass::IndexerFailed), "{report:?}");
    let failure = report.failure().expect("a failed run carries its failure");
    assert_eq!(failure.signal, Some(9), "{failure:?}");
    assert_eq!(failure.exit_code, None, "{failure:?}");
    assert!(!failure.quota_breached, "{failure:?}");
    assert_eq!(failure.wall_ms, report.wall.as_millis());
    assert_eq!(
        failure.observed_scratch_bytes,
        report.observed_scratch_bytes
    );
    drop(scratch);
    // exited 101: the code, no signal
    let scratch = Scratch::create(&dir).unwrap();
    let root = scratch.root().to_string_lossy().into_owned();
    let indexer = fake("exit-code.sh");
    let report = run_indexer(spec(
        &indexer,
        &["101", "16", &root],
        &checkout,
        &scratch,
        &dir,
        &policy,
    ))
    .unwrap();
    assert_eq!(report.class, Some(RunClass::IndexerFailed), "{report:?}");
    let failure = report.failure().expect("a failed run carries its failure");
    assert_eq!(failure.exit_code, Some(101), "{failure:?}");
    assert_eq!(failure.signal, None, "{failure:?}");
    drop(scratch);
    // exited 0: no failure
    let scratch = Scratch::create(&dir).unwrap();
    let root = scratch.root().to_string_lossy().into_owned();
    let report = run_indexer(spec(
        &indexer,
        &["0", "16", &root],
        &checkout,
        &scratch,
        &dir,
        &policy,
    ))
    .unwrap();
    assert_eq!(report.class, None, "{report:?}");
    assert!(report.failure().is_none(), "{report:?}");
}

// M3: the hidden helper receives the admitted directory on fd0. Changing its old
// pathname must not change cwd, and every refusal must precede producer execution.
#[test]
fn cwd_helper_keeps_the_admitted_directory_and_rechecks_the_producer() {
    use std::os::fd::AsFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let home = Home::new("cwd-helper");
    let checkout = home.checkout();
    fs::write(checkout.join("marker"), b"original").unwrap();
    let directory = fs::File::open(&checkout).unwrap();
    let st = rustix::fs::fstat(&directory).unwrap();
    let expected = format!("{},{}", st.st_dev, st.st_ino);
    let producer = home.path().join("producer");
    fs::write(
        &producer,
        "#!/bin/sh\ncat marker\nprintf '\\npid=%s\\n' \"$$\"\ncat\n",
    )
    .unwrap();
    fs::set_permissions(&producer, fs::Permissions::from_mode(0o700)).unwrap();
    let admitted = kaban_index::tools::admit_executable(&producer, &[]).unwrap();
    let id = admitted.identity;
    let identity = format!(
        "{},{},{},{},{}",
        id.dev, id.ino, id.size, id.ctime_sec, id.ctime_nsec
    );
    let moved = home.path().join("moved");
    fs::rename(&checkout, &moved).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("marker"), b"decoy").unwrap();
    let run_helper = |input: fs::File, expected: &str, identity: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kaban-index"));
        command
            .args([
                "cwd-helper",
                "--directory",
                expected,
                "--identity",
                identity,
                "--",
            ])
            .arg(&producer)
            .current_dir("/")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::from(input))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let child = command.spawn().unwrap();
        let pid = child.id();
        drop(command);
        (pid, child.wait_with_output().unwrap())
    };
    let parent = std::env::current_dir().unwrap();
    for symlink in [false, true] {
        if symlink {
            fs::remove_dir_all(&checkout).unwrap();
            std::os::unix::fs::symlink(&moved, &checkout).unwrap();
        }
        let (pid, out) = run_helper(directory.try_clone().unwrap(), &expected, &identity);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("original\npid={pid}\n")
        );
        assert_eq!(std::env::current_dir().unwrap(), parent);
    }
    for (input, expected, identity) in [
        (directory.try_clone().unwrap(), "0,0", identity.as_str()),
        (
            fs::File::open(moved.join("marker")).unwrap(),
            expected.as_str(),
            identity.as_str(),
        ),
        (
            directory.try_clone().unwrap(),
            expected.as_str(),
            "0,0,0,0,0",
        ),
    ] {
        let (_, out) = run_helper(input, expected, identity);
        assert_eq!(out.status.code(), Some(125));
        assert_eq!(out.stdout, [] as [u8; 0]);
        assert_eq!(out.stderr, b"cwd_helper_refused\n");
    }
    // Exercise the additive core handoff entry point with the same production helper.
    let mut handoff = core_spec(
        kaban_core::process::Exec::Path(env!("CARGO_BIN_EXE_kaban-index").into()),
        Path::new("/"),
        vec![],
        Duration::from_secs(1),
    );
    handoff.args = [
        "cwd-helper",
        "--directory",
        &expected,
        "--identity",
        &identity,
        "--",
    ]
    .into_iter()
    .map(OsString::from)
    .chain([producer.clone().into_os_string()])
    .collect();
    handoff.env.insert("PATH", "/usr/bin:/bin").unwrap();
    let outcome =
        kaban_core::process::run_with_directory_handoff(handoff, directory.as_fd()).unwrap();
    assert!(outcome.status.unwrap().success(), "{outcome:?}");
    assert!(outcome.stdout.starts_with(b"original\npid="));
    let ordinary = core_spec(
        kaban_core::process::Exec::Path("/usr/bin/true".into()),
        Path::new("/"),
        vec![],
        Duration::from_secs(1),
    );
    let plain = fs::File::open(moved.join("marker")).unwrap();
    assert!(matches!(
        kaban_core::process::run_with_directory_handoff(ordinary, plain.as_fd()),
        Err(ProcessError::Env("cwd_not_directory"))
    ));
    // A metadata/content change to the same executable inode is also refused.
    fs::write(&producer, "#!/bin/sh\necho changed\n").unwrap();
    let (_, out) = run_helper(directory.try_clone().unwrap(), &expected, &identity);
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(out.stdout, [] as [u8; 0]);
    assert!(
        rustix::io::fcntl_getfd(directory.as_fd())
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
    // exec retains supervision after the helper/producer leader exits: its pipe holder
    // must be killed at the deadline and the return must remain bounded.
    let pidfile = moved.join("descendant.pid");
    let descendant = Descendant(pidfile.clone());
    fs::write(
        &producer,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nexit 0\n",
            pidfile.display()
        ),
    )
    .unwrap();
    let id = kaban_index::tools::admit_executable(&producer, &[])
        .unwrap()
        .identity;
    let identity = format!(
        "{},{},{},{},{}",
        id.dev, id.ino, id.size, id.ctime_sec, id.ctime_nsec
    );
    let mut handoff = core_spec(
        kaban_core::process::Exec::Path(env!("CARGO_BIN_EXE_kaban-index").into()),
        Path::new("/"),
        vec![],
        Duration::from_millis(100),
    );
    handoff.deadline = Duration::from_secs(1);
    handoff.args = [
        "cwd-helper",
        "--directory",
        &expected,
        "--identity",
        &identity,
        "--",
    ]
    .into_iter()
    .map(OsString::from)
    .chain([producer.into_os_string()])
    .collect();
    handoff.env.insert("PATH", "/usr/bin:/bin").unwrap();
    let outcome =
        kaban_core::process::run_with_directory_handoff(handoff, directory.as_fd()).unwrap();
    assert_eq!(outcome.killed, Some(KillReason::Deadline), "{outcome:?}");
    assert!(
        outcome.status.unwrap().success(),
        "leader exited before the deadline"
    );
    assert!(outcome.wall < Duration::from_millis(1300));
    assert!(descendant.dead_within(Duration::from_millis(200)));
}

#[test]
fn concurrent_scratch_and_gc_preserve_a_live_runs_files() {
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = Home::new("concurrent-scratch");
    let mut worker = Worker(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "concurrent_scratch_child",
                "--ignored",
                "--nocapture",
            ])
            .env("KABAN_SCRATCH_TEST_HOME", home.path())
            .spawn()
            .unwrap(),
    );
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !home.path().join("ready").exists() {
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "worker exited before handshake"
        );
        assert!(
            std::time::Instant::now() < until,
            "worker handshake timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let dir = home.repo_dir();
    let other = dir.path.join(format!("run-{}", worker.0.id()));
    let other_temp = dir.path.join(format!("live.cg.db.tmp-{}", worker.0.id()));
    let scratch = Scratch::create(&dir).unwrap();
    assert_eq!(kaban_index::gc::sweep_leftovers(&dir), 0);
    assert_eq!(fs::read(other.join("out/sentinel")).unwrap(), b"owned");
    assert_eq!(fs::read(&other_temp).unwrap(), b"owned");
    fs::write(home.path().join("release"), b"release").unwrap();
    loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < until, "worker did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!other.exists());
    assert!(scratch.root().exists());
}

#[test]
#[ignore = "separate live run launched by concurrent_scratch_and_gc_preserve_a_live_runs_files"]
fn concurrent_scratch_child() {
    let home =
        PathBuf::from(std::env::var_os("KABAN_SCRATCH_TEST_HOME").expect("explicit fixture home"));
    let dir = identity_cli::admit_repo_dir(&home, &"ab".repeat(16)).unwrap();
    let scratch = Scratch::create(&dir).unwrap();
    fs::write(scratch.out().join("sentinel"), b"owned").unwrap();
    let temp = dir
        .path
        .join(format!("live.cg.db.tmp-{}", std::process::id()));
    fs::write(&temp, b"owned").unwrap();
    fs::write(home.join("ready"), b"ready").unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !home.join("release").exists() {
        assert!(
            std::time::Instant::now() < until,
            "parent did not release worker"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    scratch.remove().unwrap();
    fs::remove_file(temp).unwrap();
}
