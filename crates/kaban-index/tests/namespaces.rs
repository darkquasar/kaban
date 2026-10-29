//! CG4.4 (stage1-spec G1-S5.1 `namespaces`, S5.2 helper and in-run self-check, S5.5 shared
//! read-only caches; stage1-plan CG4.4) through the public surface of `kaban_index::isolate`
//! and `kaban_index::runner` with the fake indexers under `tests/fake-indexers/`, on the real
//! kernel: every test first runs the availability probe (`ns-helper --probe-only` through the
//! `kaban-index` binary) and prints a visible `NOT RUN` marker when unprivileged user
//! namespaces are unavailable on the host (Linux CI runs them; elsewhere the marker is the
//! evidence). Every scratch is under `/tmp` and removed on drop (`test-integration-dir`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible `NOT RUN` marker and the platform evidence lines
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kaban_core::config::Isolation;
use kaban_core::process::InheritedFds;
use kaban_index::detect::Lang;
use kaban_index::identity_cli::{self, AdmittedDir};
use kaban_index::isolate::namespaces::{self, Overlay, probe_namespaces};
use kaban_index::isolate::{
    ISOLATION_NONE, Probe, ReasonClass, Scratch, SelectError, Tier, select,
};
use kaban_index::runner::{
    GoRun, IndexerSpec, IsolationRun, NsRun, QuotaCaps, RunClass, RunReport, run_indexer,
};
use kaban_index::writer::Commit;
use kaban_policy::Policy;

/// A scratch home deleted on drop, test failure included.
struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-cg44-{tag}-{}-{}",
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

    fn repo_dir(&self) -> AdmittedDir {
        identity_cli::admit_repo_dir(&self.0, &"cd".repeat(16)).expect("admitted data dir")
    }

    fn checkout(&self) -> PathBuf {
        let dir = self.0.join("checkout");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("go.mod"), "module example.com/x\n").unwrap();
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

fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kaban-index"))
}

fn commit(n: u8) -> Commit {
    Commit::parse(&format!("{n:02x}").repeat(20)).unwrap()
}

fn tool_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
}

/// The availability probe; `None` with the visible marker when the tier is unavailable here.
fn available(test: &str) -> Option<Probe> {
    let probe = probe_namespaces(&helper());
    match &probe.available {
        Ok(_) => Some(probe),
        Err(reason) => {
            eprintln!(
                "NOT RUN: {test}: the namespaces tier is unavailable on this host ({})",
                reason.as_str()
            );
            None
        }
    }
}

/// A run on the `namespaces` tier with the fake `indexer`: 30 s deadline, 1 MiB caps.
fn spec<'a>(
    indexer: &'a Path,
    args: &[&str],
    checkout: &'a Path,
    scratch: &'a Scratch,
    dir: &'a AdmittedDir,
    policy: &'a Policy,
) -> IndexerSpec<'a> {
    spec_with(
        indexer,
        args,
        checkout,
        scratch,
        dir,
        policy,
        NsRun::prepare(&helper(), None).expect("unshare resolves"),
    )
}

#[allow(clippy::too_many_arguments)] // the test's one spelling of a run
fn spec_with<'a>(
    indexer: &'a Path,
    args: &[&str],
    checkout: &'a Path,
    scratch: &'a Scratch,
    dir: &'a AdmittedDir,
    policy: &'a Policy,
    ns: NsRun<'a>,
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
        commit: commit(0x22),
        policy,
        // the test runner may hold a launcher's descriptor without CLOEXEC (`flock(1)`)
        inherited_fds: InheritedFds::Tolerate,
        go: GoRun::default(),
        linker: None,
        indexer_identity: None,
        resource_dirs: Vec::new(),
        checkout: None,
        run: kaban_index::runner::RunSlot::A,
        isolation: IsolationRun::Namespaces(ns),
    }
}

fn log_text(report: &RunReport) -> String {
    fs::read_to_string(&report.log_path).unwrap()
}

// slice `ns_checkout_is_readonly` (S5.1 read-only checkout, S5.2 EROFS): the fake indexer
// writes into its working directory (the checkout): the write fails with `EROFS` inside the
// view (the script's redirection fails, so the fake exits 2 and the run is `indexer_failed`),
// and nothing appears in the checkout on the host.
#[test]
fn ns_checkout_is_readonly() {
    let Some(probe) = available("ns_checkout_is_readonly") else {
        return;
    };
    let home = Home::new("ro");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("write-checkout.sh");
    let report = run_indexer(spec(&indexer, &[], &checkout, &scratch, &dir, &policy)).unwrap();
    assert_eq!(report.tier, Tier::Namespaces, "{report:?}");
    let log = log_text(&report);
    assert!(
        log.contains("Read-only file system"),
        "the write failed with EROFS inside the view:\n{log}"
    );
    assert_eq!(
        report.class,
        Some(RunClass::IndexerFailed),
        "{report:?}\n{log}"
    );
    assert_eq!(report.status.and_then(|s| s.code()), Some(2), "{log}");
    assert!(
        !log.contains("wrote outside"),
        "the write did not succeed:\n{log}"
    );
    assert!(
        !checkout.join("outside-scratch.txt").exists(),
        "nothing reached the checkout on the host"
    );
    assert!(
        matches!(probe.available, Ok(ref d) if d.0.starts_with("unshare=/")),
        "{probe:?}"
    );
}

/// One run of `mount-list.sh` on the namespaces tier: the view's mount list, the pid inside
/// the pid namespace, the open descriptors and the TCP attempt, from inside.
fn mount_list_run(home: &Home) -> (RunReport, String, namespaces::View) {
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("mount-list.sh");
    let view = namespaces::View::new(
        &checkout,
        None,
        &indexer,
        &tool_dirs(),
        scratch.root(),
        Vec::new(),
    );
    let report = run_indexer(spec(&indexer, &[], &checkout, &scratch, &dir, &policy)).unwrap();
    let log = log_text(&report);
    (report, log, view)
}

fn section<'a>(log: &'a str, name: &str) -> &'a str {
    let start = log
        .find(&format!("--- {name}"))
        .unwrap_or_else(|| panic!("section {name} in\n{log}"));
    let rest = &log[start..];
    let body_start = rest.find('\n').map_or(rest.len(), |i| i + 1);
    let body = &rest[body_start..];
    body.find("\n--- ").map_or(body, |end| &body[..end])
}

// slice `ns_view_only_listed_paths` (S5.1 "the exact mount list is settled by this test"):
// the fake indexer dumps `/proc/self/mountinfo`: the set of mount points equals exactly
// `View::mount_points()` of the view the parent built (the root, `/proc`, the three `/dev`
// nodes, `/scratch`, the checkout, the fake indexer's directory, the two tool directories and
// the system paths that exist on this host). The list is printed for the report.
#[test]
fn ns_view_only_listed_paths() {
    if available("ns_view_only_listed_paths").is_none() {
        return;
    }
    let home = Home::new("view");
    let (report, log, view) = mount_list_run(&home);
    assert!(report.class.is_none(), "{report:?}\n{log}");
    let seen: std::collections::BTreeSet<PathBuf> = section(&log, "mounts")
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    let expected = view.mount_points();
    assert_eq!(
        seen, expected,
        "the view holds exactly the listed mounts:\n{log}"
    );
    eprintln!("ns_view_only_listed_paths: mount list on this host:");
    for p in &seen {
        eprintln!("  {}", p.display());
    }
    // nothing of the host beyond the list: the user's home, `/etc`, `/tmp`, `/sys`, `/run`
    // and `/var` are not mounted (only the listed paths below them can be)
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    for forbidden in [
        "/home", "/etc", "/tmp", "/sys", "/run", "/var", "/root", "/dev",
    ] {
        assert!(
            !seen.contains(Path::new(forbidden)),
            "{forbidden} mounted:\n{log}"
        );
    }
    assert!(
        !seen.contains(&user_home),
        "the user's home is not in the view:\n{log}"
    );
    assert!(
        seen.contains(&home.checkout()) && seen.contains(Path::new("/scratch")),
        "{log}"
    );
}

// slice `ns_usr_include_is_readonly` (DF-2 blocker 6): a build script compiling C inside the
// view (`cc-rs`: the bundled SQLite of kaban itself) needs the system headers: `/usr/include`
// (the multiarch libc headers live under it) is a system path of the view, bound read-only
// like the others, when the host has it.
#[test]
fn ns_usr_include_is_readonly() {
    if available("ns_usr_include_is_readonly").is_none() {
        return;
    }
    let include = PathBuf::from("/usr/include");
    if !include.is_dir() {
        eprintln!("NOT RUN: ns_usr_include_is_readonly: no /usr/include on this host");
        return;
    }
    let home = Home::new("include");
    let (report, log, view) = mount_list_run(&home);
    assert!(report.class.is_none(), "{report:?}\n{log}");
    assert!(
        view.mount_points().contains(&include),
        "/usr/include is a system path of the view"
    );
    let listed = |name: &str| -> std::collections::BTreeSet<PathBuf> {
        section(&log, name)
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect()
    };
    assert!(
        listed("mounts").contains(&include),
        "/usr/include is mounted inside the view:\n{log}"
    );
    assert!(
        listed("ro").contains(&include),
        "/usr/include is read-only inside the view:\n{log}"
    );
}

// slice `ns_helper_single_invocation` (S5.2 "one unshare invocation; the helper execs into
// the indexer without leaving the namespaces"): inside the pid namespace the fake indexer is
// pid 1, the process the helper was (the helper did not fork before its `exec`); the run's
// RSS monitor reads that process through `unshare --fork`'s one child (the `alloc.sh` fake
// becomes a 64 MiB `dd`: `peak_rss_bytes >= 64 MiB`); and the command line of one run names
// `unshare` exactly once (the helper is an argument of it, never a second wrapper).
#[test]
fn ns_helper_single_invocation() {
    if available("ns_helper_single_invocation").is_none() {
        return;
    }
    let home = Home::new("single");
    let (report, log, _) = mount_list_run(&home);
    assert!(report.class.is_none(), "{report:?}\n{log}");
    assert!(
        log.contains("--- pid=1\n"),
        "the indexer runs as pid 1 of the pid namespace (the helper's own pid):\n{log}"
    );
    // the RSS of the indexer process (one hop below `unshare`), not of `unshare`
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("alloc.sh");
    let report = run_indexer(spec(&indexer, &["64"], &checkout, &scratch, &dir, &policy)).unwrap();
    assert!(report.class.is_none(), "{report:?}\n{}", log_text(&report));
    let peak = report
        .peak_rss_bytes
        .expect("the indexer process was sampled");
    assert!(
        peak >= 64 << 20,
        "peak {peak} bytes is the indexer's, not unshare's"
    );
    // the argv the parent builds: one `unshare`, the flags, the helper, then `--` and the indexer
    let view = namespaces::View::new(
        &checkout,
        None,
        &indexer,
        &tool_dirs(),
        scratch.root(),
        Vec::new(),
    );
    let args = view.helper_args(
        namespaces::NsInodes {
            user: 1,
            net: 2,
            mnt: 3,
        },
        namespaces::Overlay::None,
    );
    let text = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !text.contains("unshare"),
        "the helper args never name a second wrapper: {text}"
    );
    assert!(
        namespaces::UNSHARE_FLAGS.starts_with(&["--user", "--map-root-user", "--net", "--mount"]),
        "{:?}",
        namespaces::UNSHARE_FLAGS
    );
}

// slice `ns_helper_closes_wrapper_handle` (RV-9): inside the view the indexer's open
// descriptors never point at the `unshare` wrapper: the parent's `O_PATH|O_CLOEXEC` handle is
// closed at the `unshare` exec and the helper opens no handle of its own.
#[test]
fn ns_helper_closes_wrapper_handle() {
    if available("ns_helper_closes_wrapper_handle").is_none() {
        return;
    }
    let home = Home::new("fds");
    let (report, log, _) = mount_list_run(&home);
    assert!(report.class.is_none(), "{report:?}\n{log}");
    let fds = section(&log, "fds");
    assert!(
        fds.contains("fd 0 -> "),
        "the descriptor list was read:\n{log}"
    );
    for line in fds.lines() {
        assert!(
            !line.ends_with("/unshare") && !line.contains("unshare"),
            "a descriptor points at the wrapper: {line}\n{log}"
        );
        assert!(
            !line.contains("/proc/self/fd"),
            "no handle path survived the exec: {line}"
        );
    }
}

// slice `ns_tcp_connect_unreachable` (S5.1 "a down loopback only"): a TCP connect to
// 127.0.0.1 from inside fails with `ENETUNREACH` (errno 101); `/proc/net/dev` listed only `lo`
// (the helper's self-check, or the run would have been refused).
#[test]
fn ns_tcp_connect_unreachable() {
    if available("ns_tcp_connect_unreachable").is_none() {
        return;
    }
    let home = Home::new("tcp");
    let (report, log, _) = mount_list_run(&home);
    assert!(report.class.is_none(), "{report:?}\n{log}");
    let tcp = section(&log, "tcp");
    assert!(
        tcp.contains("tcp errno 101 "),
        "ENETUNREACH from inside the network namespace:\n{log}"
    );
    assert!(!tcp.contains("tcp connected"), "{log}");
}

/// Start the helper by hand through the host's `unshare` with `flags` (the parent's exact
/// argument list otherwise), so a missing namespace flag can be observed from the outside.
/// The indexer marks its start by creating `/scratch/started` inside the view (the one
/// writable place): [`canary`] is that file's host path.
fn helper_by_hand(home: &Home, flags: &[&str]) -> std::process::Output {
    let checkout = home.checkout();
    let scratch = home.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let indexer = home.path().join("mark.sh");
    fs::write(
        &indexer,
        "#!/bin/sh\nset -e\ntouch /scratch/started\necho started\n",
    )
    .unwrap();
    fs::set_permissions(&indexer, fs::Permissions::from_mode(0o755)).unwrap();
    let view = namespaces::View::new(
        &checkout,
        None,
        &indexer,
        &tool_dirs(),
        &scratch,
        Vec::new(),
    );
    let parent = namespaces::NsInodes::current().unwrap();
    let mut cmd = std::process::Command::new("unshare");
    cmd.args(flags)
        .arg(helper())
        .arg("ns-helper")
        .args(view.helper_args(parent, namespaces::Overlay::None))
        .arg("--")
        .arg(&indexer);
    cmd.output().expect("unshare runs")
}

/// The host path of the start mark `helper_by_hand`'s indexer creates.
fn canary(home: &Home) -> PathBuf {
    home.path().join("scratch").join("started")
}

// slice `ns_selfcheck_mismatch_refuses` (S5.2 "an anomaly, not an unavailability"): the
// helper started without `--net` sees the host's interfaces, prints the fixed text and exits 8
// without starting the indexer; without `--mount` the view cannot be built: the same refusal.
// The helper has no policy argument at all, so the refusal is the same under `preferred`,
// `required` and `off` by construction; the runner maps exit 8 plus the fixed text to the class
// `isolation_selfcheck_failed`, whose exit code is 8.
#[test]
fn ns_selfcheck_mismatch_refuses() {
    if available("ns_selfcheck_mismatch_refuses").is_none() {
        return;
    }
    let home = Home::new("selfcheck");
    let canary = canary(&home);
    let without_net = ["--user", "--map-root-user", "--mount", "--pid", "--fork"];
    let out = helper_by_hand(&home, &without_net);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(8), "{stderr}");
    assert!(
        stderr
            .lines()
            .any(|l| l == namespaces::SELFCHECK_FAILED_TEXT),
        "{stderr}"
    );
    // every failing item is named: the equal net inode and what it lets through
    assert!(
        stderr.contains("self-check namespace_inodes,network_interfaces"),
        "{stderr}"
    );
    assert!(!canary.exists(), "the indexer never started");
    assert!(out.stdout.is_empty(), "nothing on stdout");

    let without_mount = ["--user", "--map-root-user", "--net", "--pid", "--fork"];
    let out = helper_by_hand(&home, &without_mount);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(8), "{stderr}");
    assert!(
        stderr
            .lines()
            .any(|l| l == namespaces::SELFCHECK_FAILED_TEXT),
        "{stderr}"
    );
    assert!(stderr.contains("isolation_setup_failed"), "{stderr}");
    assert!(!canary.exists());

    // the full flag set starts the indexer (the control)
    let out = helper_by_hand(&home, &namespaces::UNSHARE_FLAGS);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "started\n");
    assert!(canary.exists(), "the indexer ran and wrote into /scratch");

    assert_eq!(RunClass::IsolationSelfcheckFailed.exit_code(), 8);
    assert_eq!(
        RunClass::IsolationSelfcheckFailed.as_str(),
        "isolation_selfcheck_failed"
    );
}

/// The user's Go module cache of a cache slice: a seeded directory under the scratch home
/// (never the real `$HOME/go/pkg/mod`), and its byte-level fingerprint.
fn seeded_user_cache(home: &Home) -> PathBuf {
    let lower = home.path().join("user-gomod");
    fs::create_dir_all(lower.join("cache/download")).unwrap();
    fs::write(lower.join("seed.txt"), "seed-7f3a").unwrap();
    fs::write(lower.join("cache/download/list"), "example.com/x v1.0.0\n").unwrap();
    lower
}

fn fingerprint(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push((p.clone(), fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

/// A run of `cache-write.sh` on the namespaces tier with `modcache` as the driver's admitted
/// module cache (G1: a read-only resource of the view at its host path, `GOMODCACHE` inside),
/// or none (a dedicated empty directory under the scratch).
fn cache_run(home: &Home, overlay: Overlay, modcache: Option<&Path>) -> (RunReport, String) {
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("cache-write.sh");
    let ns = NsRun::prepare(&helper(), None)
        .expect("unshare resolves")
        .with_overlay(overlay);
    let mut spec = spec_with(&indexer, &[], &checkout, &scratch, &dir, &policy, ns);
    spec.go = GoRun {
        modcache,
        vendored: false,
        goroot: None,
        toolchain: None,
    };
    spec.resource_dirs = modcache.map(Path::to_path_buf).into_iter().collect();
    let report = run_indexer(spec).unwrap();
    let log = log_text(&report);
    (report, log)
}

// slice `caches_shared_ro_in_namespaces` (S5.5; G1): the tier's cache policy stays
// `shared-ro` and the admitted module cache is a read-only resource of the view at its host
// path: readable inside, `GOMODCACHE` names it, no write lands whatever the overlay finding
// (a resource bind carries no upper), the user's directory is byte-identical afterwards.
#[test]
fn caches_shared_ro_in_namespaces() {
    let Some(probe) = available("caches_shared_ro_in_namespaces") else {
        return;
    };
    let detail = probe.available.as_ref().unwrap();
    let overlay = Overlay::from_detail(&detail.0)
        .unwrap_or_else(|| panic!("the detail records the overlay finding: {detail:?}"));
    eprintln!(
        "caches_shared_ro_in_namespaces: isolation_detail on this host: {}",
        detail.0
    );
    let home = Home::new("shared-ro");
    let lower = seeded_user_cache(&home);
    // gofix1-1 canary: the cache's parent (the caller's home) holds a file the view must not see
    fs::write(
        home.path().join("canary.txt"),
        "in the home directory, not in the cache",
    )
    .unwrap();
    let before = fingerprint(&lower);
    let (report, log) = cache_run(&home, overlay, Some(&lower));
    assert!(report.class.is_none(), "{report:?}\n{log}");
    assert!(
        log.contains(&format!("GOMODCACHE={}\n", lower.display())),
        "the admitted module cache by its host path:\n{log}"
    );
    assert!(
        log.contains("parent canary=absent\n"),
        "only the cache is bound, not its parent (the home directory):\n{log}"
    );
    assert!(
        log.contains("seed=seed-7f3a\n"),
        "the user's cache is readable inside:\n{log}"
    );
    assert!(
        log.contains("cache write failed:") && log.contains("Read-only file system"),
        "a resource bind is read-only under every overlay finding:\n{log}"
    );
    assert!(
        log.contains("build cache write ok"),
        "the scratch build cache is writable:\n{log}"
    );
    assert_eq!(
        fingerprint(&lower),
        before,
        "the user's cache is byte-identical afterwards"
    );
    assert!(!lower.join("indexer-wrote.txt").exists());
    assert!(report.warnings.is_empty(), "{report:?}");
}

// slice `ns_indexer_succeeds_with_ro_cache` (OD-G1-5 fallback): with the plain read-only
// bind forced (`overlay=none`), the fake Go indexer under `GOFLAGS=-mod=readonly GOPROXY=off`
// reads its module cache and exits 0; only its optional write fails.
#[test]
fn ns_indexer_succeeds_with_ro_cache() {
    if available("ns_indexer_succeeds_with_ro_cache").is_none() {
        return;
    }
    let home = Home::new("ro-cache");
    let lower = seeded_user_cache(&home);
    let before = fingerprint(&lower);
    let (report, log) = cache_run(&home, Overlay::None, Some(&lower));
    assert!(report.class.is_none(), "{report:?}\n{log}");
    assert_eq!(report.status.and_then(|s| s.code()), Some(0), "{log}");
    assert!(log.contains("GOFLAGS=-mod=readonly GOPROXY=off\n"), "{log}");
    assert!(log.contains("seed=seed-7f3a\n"), "{log}");
    assert!(log.contains("cache write failed:"), "{log}");
    assert!(log.contains("Read-only file system"), "{log}");
    assert_eq!(fingerprint(&lower), before);
}

// slice `caches_dedicated_when_configured` (G1; gofix1-7: no cache policy any more): with no
// admitted module cache `GOMODCACHE` is the scratch's own directory, the seed is absent inside
// and nothing touches the user's cache.
#[test]
fn caches_dedicated_when_configured() {
    if available("caches_dedicated_when_configured").is_none() {
        return;
    }
    let home = Home::new("dedicated");
    let lower = seeded_user_cache(&home);
    let before = fingerprint(&lower);
    let (report, log) = cache_run(&home, Overlay::None, None);
    assert!(log.contains("GOMODCACHE=/scratch/cache/gomod\n"), "{log}");
    assert!(
        log.contains("seed=absent\n"),
        "the user's cache is not in the view:\n{log}"
    );
    // the fake exits 1 when the seed is absent: the dedicated cache is empty by construction
    assert_eq!(
        report.class,
        Some(RunClass::IndexerFailed),
        "{report:?}\n{log}"
    );
    assert!(
        log.contains("cache write landed"),
        "the dedicated cache is writable:\n{log}"
    );
    assert_eq!(fingerprint(&lower), before);
}

// slice `probe_only_reports_availability` (S5.2 `--probe-only`): the probe's finding on this
// host is evidence whichever it is: available with a kaban-built detail (`unshare=<abs path>
// kernel=<release> overlay=overlayfs|none`, nothing from the helper's output but the overlay
// word), or unavailable with a reason from the closed set. The sysctl texts are printed too.
#[test]
fn probe_only_reports_availability() {
    let probe = probe_namespaces(&helper());
    assert_eq!(probe.tier, Tier::Namespaces);
    for sysctl in namespaces::USERNS_SYSCTLS {
        match fs::read_to_string(sysctl) {
            Ok(v) => eprintln!("probe_only_reports_availability: {sysctl} = {}", v.trim()),
            Err(e) => eprintln!("probe_only_reports_availability: {sysctl} absent ({e})"),
        }
    }
    match &probe.available {
        Ok(detail) => {
            eprintln!("probe_only_reports_availability: available ({})", detail.0);
            let words: Vec<&str> = detail.0.split(' ').collect();
            assert_eq!(words.len(), 3, "{detail:?}");
            assert!(words[0].starts_with("unshare=/"), "{detail:?}");
            assert_eq!(
                words[1],
                format!("kernel={}", namespaces::kernel_release()),
                "{detail:?}"
            );
            assert!(Overlay::from_detail(&detail.0).is_some(), "{detail:?}");
        }
        Err(reason) => {
            eprintln!(
                "probe_only_reports_availability: unavailable ({})",
                reason.as_str()
            );
            assert!(
                matches!(
                    reason,
                    ReasonClass::WrapperNotFound
                        | ReasonClass::WrapperRefused
                        | ReasonClass::HandleExecUnsupported
                        | ReasonClass::UnshareFailed
                        | ReasonClass::UsernsDisabled
                        | ReasonClass::SelfcheckFailed
                ),
                "{reason:?}"
            );
        }
    }
    // the JSON spelling of a probe is the tier name and the result, nothing else
    let json = serde_json::to_value(&probe).unwrap();
    assert_eq!(json["tier"], "namespaces");
    assert!(json.get("available").is_some(), "{json}");
}

// negative path (CG4.4): `unshare` present but user namespaces disabled by the kernel
// (`unprivileged_userns_clone = 0` or `max_user_namespaces = 0`) is the probe reason
// `userns_disabled`; `preferred` falls to `none` with the recorded warning, `required`
// refuses with exit 8; an absent sysctl file is not a restriction. Pure functions: this host
// has the namespaces enabled, so the sysctl texts are injected.
#[test]
fn userns_disabled_is_reported_and_preferred_falls_to_none() {
    assert!(namespaces::userns_disabled(Some("0\n"), None));
    assert!(namespaces::userns_disabled(Some("1\n"), Some("0\n")));
    assert!(namespaces::userns_disabled(None, Some("0")));
    assert!(
        !namespaces::userns_disabled(None, None),
        "absent files are not a restriction"
    );
    assert!(!namespaces::userns_disabled(Some("1\n"), Some("24158\n")));
    assert!(!namespaces::userns_disabled(Some("garbage"), None));
    assert_eq!(ReasonClass::UsernsDisabled.as_str(), "userns_disabled");
    let probes = [
        Probe {
            tier: Tier::Namespaces,
            available: Err(ReasonClass::UsernsDisabled),
        },
        Probe {
            tier: Tier::Container,
            available: Err(ReasonClass::NotInThisRelease),
        },
    ];
    let s = select(Isolation::Preferred, &probes).unwrap();
    assert_eq!((s.tier, s.warning), (Tier::None, Some(ISOLATION_NONE)));
    assert_eq!(s.detail.0, "none");
    let err = select(Isolation::Required, &probes).unwrap_err();
    assert_eq!(err, SelectError::RequiredUnavailable);
    assert_eq!(err.exit_code(), 8);
    // the deferred container tier never counts as an isolating tier (OWN-68 SC-11)
    assert_eq!(
        ReasonClass::NotInThisRelease.as_str(),
        "not_in_this_release"
    );
    assert_eq!(
        serde_json::to_value(&probes[1]).unwrap()["available"]["Err"],
        "not_in_this_release"
    );
}

// U2 RU2-1 (G1-S5.1, S5.2; GM4c): after the self-check the helper drops every capability
// (bounding, permitted, effective, inheritable, ambient), locks the securebits, closes nested
// user namespaces and sets `no_new_privs` before the `exec`, so an indexer with the helper's
// uid 0 cannot remount the read-only binds writable, mount anything new or regain privileges
// across its own `exec`s; the host checkout is byte-identical afterwards.
#[test]
fn ns_indexer_cannot_remount_or_mount_after_privilege_drop() {
    let Some(_probe) = available("ns_indexer_cannot_remount_or_mount_after_privilege_drop") else {
        return;
    };
    let home = Home::new("privdrop");
    let dir = home.repo_dir();
    let checkout = home.checkout();
    let before = fs::read(checkout.join("go.mod")).unwrap();
    let scratch = Scratch::create(&dir).unwrap();
    let policy = Policy::empty();
    let indexer = fake("escape.sh");
    let report = run_indexer(spec(&indexer, &[], &checkout, &scratch, &dir, &policy)).unwrap();
    let log = log_text(&report);
    eprintln!(
        "privilege state inside the view:\n{}attempts:\n{}",
        section(&log, "caps"),
        section(&log, "attempts")
    );
    let attempts = section(&log, "attempts");
    for name in ["remount_rw", "new_tmpfs", "nested_userns"] {
        assert!(
            attempts
                .lines()
                .any(|l| l.starts_with(&format!("{name}=-1 "))),
            "{name} must fail inside the view:\n{log}"
        );
    }
    assert!(
        attempts.lines().any(|l| l.starts_with("checkout_write=1 ")),
        "the checkout stays read-only after the remount attempt:\n{log}"
    );
    let caps = section(&log, "caps");
    for key in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        assert!(
            caps.lines()
                .any(|l| l.starts_with(key) && l.trim_end().ends_with("0000000000000000")),
            "{key} is empty before the indexer runs:\n{caps}"
        );
    }
    assert!(
        caps.lines().any(|l| l.trim() == "NoNewPrivs:\t1"),
        "no_new_privs is set:\n{caps}"
    );
    assert_eq!(report.class, None, "the fake exits 0: {report:?}\n{log}");
    assert!(
        !checkout.join("escaped.txt").exists(),
        "nothing reached the host checkout"
    );
    assert_eq!(fs::read(checkout.join("go.mod")).unwrap(), before);
}
