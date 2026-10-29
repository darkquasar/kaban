//! `kaban_core::process` through its public surface (stage1-plan CG1.1 slices 1..6): group kill
//! at the deadline, output caps, monitors, the clean environment, handle execution (OD-G1-4,
//! RV-9) and the RV-1 allowlist refusals. Linux-only where handles are involved.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kaban_core::process::{EnvAllowlist, Exec, InheritedFds, KillReason, Spec, Stream, run};

/// A scratch directory removed on drop, panics included (test-fixture-raii).
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-process-{name}-{}-{}",
            std::process::id(),
            kaban_core::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A descendant the wrapper script forked and whose pid it wrote to `pidfile`: `SIGKILL`ed on
/// drop if it is still alive (test-fixture-raii; RG1-4). Armed before the assertions, so a
/// failing or panicking test leaves no sleeping process behind for the other tests.
struct Descendant {
    pidfile: PathBuf,
}

impl Descendant {
    fn armed(pidfile: PathBuf) -> Self {
        Self { pidfile }
    }

    /// The pid the wrapper wrote (panics when the wrapper never got that far).
    fn pid(&self) -> String {
        let pid = std::fs::read_to_string(&self.pidfile)
            .unwrap()
            .trim()
            .to_owned();
        assert_ne!(pid, "", "the wrapper wrote no pid");
        pid
    }

    /// Wait up to `grace` for the descendant to die; whether it is dead.
    fn dead_within(&self, grace: Duration) -> bool {
        let pid = self.pid();
        let until = Instant::now() + grace;
        while alive(&pid) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        !alive(&pid)
    }
}

impl Drop for Descendant {
    fn drop(&mut self) {
        let Ok(text) = std::fs::read_to_string(&self.pidfile) else {
            return;
        };
        let Ok(raw) = text.trim().parse::<i32>() else {
            return;
        };
        if !alive(&raw.to_string()) {
            return;
        }
        if let Some(pid) = rustix::process::Pid::from_raw(raw) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            // the test's own panic is the failure; this is the guard's trace of the cleanup
            #[allow(clippy::print_stderr)]
            {
                eprintln!("Descendant guard: killed surviving descendant {raw}");
            }
        }
    }
}

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn spec<'a>(exec: Exec, argv: &[&str], cwd: &'a Path, deadline: Duration) -> Spec<'a> {
    Spec {
        exec,
        args: args(argv),
        cwd,
        env: EnvAllowlist::new(),
        deadline,
        stdout_cap: 1024 * 1024,
        stderr_cap: 0,
        monitors: Vec::new(),
        join_bound: Duration::from_millis(25),
        // the test runner may itself hold a descriptor without CLOEXEC (nextest under
        // `flock(1)` leaks its lock fd); the RG1-2 tests cover `Refuse` explicitly
        inherited_fds: InheritedFds::Tolerate,
    }
}

/// Whether `pid` is still a live process (a reaped or zombie process is not).
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            let after = s.rsplit(')').next()?.trim_start();
            after.chars().next()
        })
        .is_some_and(|state| state != 'Z' && state != 'X')
}

const GRACE: Duration = Duration::from_millis(200);

// slice 1: a wrapper forks a grandchild that inherits the stdout pipe, then hangs; at the
// deadline the whole group dies and `run` returns within deadline + 50 ms
#[test]
fn a_grandchild_holding_the_pipe_dies_with_the_group_at_the_deadline() {
    let scratch = Scratch::new("group");
    let pidfile = scratch.0.join("grandchild.pid");
    let wrapper = scratch.script(
        "hang.sh",
        &format!(
            "sleep 30 &\necho $! > '{}'\nexec sleep 30\n",
            pidfile.display()
        ),
    );
    let grandchild = Descendant::armed(pidfile);
    let deadline = Duration::from_millis(200);
    let started = Instant::now();
    let outcome = run(spec(Exec::Path(wrapper), &[], &scratch.0, deadline)).unwrap();
    let took = started.elapsed();
    assert_eq!(outcome.killed, Some(KillReason::Deadline));
    assert_eq!(
        outcome.kill_error, None,
        "the group kill reached a live group"
    );
    assert!(took < deadline + Duration::from_millis(50), "took {took:?}");
    assert!(outcome.wall <= took);
    assert!(
        grandchild.dead_within(GRACE),
        "grandchild {} survived the group kill",
        grandchild.pid()
    );
}

// GM2a residual (astra review of GM2): `Outcome::killed` says a group kill was issued, not that
// it reached anything. The leader moves a `sleep` into its own session (outside the group) that
// keeps the stdout pipe open, then exits; the loop waits for the pipe until the deadline, by
// which time the leader was reaped and the group has no member: `kill_process_group` fails and
// the child-only fallback runs. The outcome names that with a fixed class.
#[test]
fn a_group_kill_on_an_already_reaped_group_is_reported_in_the_outcome() {
    let scratch = Scratch::new("gone-group");
    let pidfile = scratch.0.join("escaped.pid");
    let wrapper = scratch.script(
        "escape.sh",
        &format!(
            "setsid sleep 30 &\necho $! > '{}'\nexit 0\n",
            pidfile.display()
        ),
    );
    let escaped = Descendant::armed(pidfile);
    let deadline = Duration::from_millis(200);
    let outcome = run(spec(Exec::Path(wrapper), &[], &scratch.0, deadline)).unwrap();
    assert_eq!(outcome.killed, Some(KillReason::Deadline), "{outcome:?}");
    assert_eq!(
        outcome.status.map(|s| s.success()),
        Some(true),
        "the leader exited on its own before the deadline"
    );
    assert_eq!(
        outcome.kill_error,
        Some("group_not_found"),
        "the group kill found no group: {outcome:?}"
    );
    // the escaped process is outside the group by construction: the guard ends it
    assert!(
        alive(&escaped.pid()),
        "the escaped sleep is not the runner's to kill"
    );
    drop(escaped);
}

/// The descendant's handshake (`GM2a` residual, timing evidence): it polls the leader's pid until
/// the leader is gone, so the overflow is placed after the leader exited and was reaped by
/// construction rather than by a fixed sleep. `$$` is the wrapper's own pid, the group leader;
/// `kill -0` still succeeds on a zombie, so the loop ends only once `run` reaped it.
const AFTER_LEADER_EXIT: &str = "while kill -0 $$ 2>/dev/null; do sleep 0.005; done";

// RG1-1 (a): the leader exits at once; its descendant writes past the stdout cap once the
// leader is gone. The cap kill must reach the descendant (the group), not only be reported.
#[test]
fn a_descendant_overflowing_stdout_after_the_leader_exited_is_killed_with_the_group() {
    let scratch = Scratch::new("late-stdout");
    let pidfile = scratch.0.join("descendant.pid");
    let wrapper = scratch.script(
        "late.sh",
        &format!(
            "( {AFTER_LEADER_EXIT}; head -c 8192 /dev/zero; sleep 30 ) &\necho $! > '{}'\nexit 0\n",
            pidfile.display()
        ),
    );
    let descendant = Descendant::armed(pidfile);
    let deadline = Duration::from_secs(5);
    let mut s = spec(Exec::Path(wrapper), &[], &scratch.0, deadline);
    s.stdout_cap = 4096;
    let outcome = run(s).unwrap();
    assert_eq!(
        outcome.killed,
        Some(KillReason::OutputCap {
            stream: Stream::Stdout
        }),
        "{outcome:?}"
    );
    assert_eq!(outcome.stdout.len(), 4096);
    assert!(
        outcome.wall < deadline / 2,
        "the cap, not the deadline, ended the run: {:?}",
        outcome.wall
    );
    assert!(
        descendant.dead_within(GRACE),
        "descendant {} survived the cap kill",
        descendant.pid()
    );
}

// RG1-1 (b): the leader exits; a descendant holds stdout open and overflows stderr later. The
// stderr cap kills within deadline / 2; stdout being held must not defer it to the deadline.
#[test]
fn a_descendant_overflowing_stderr_while_holding_stdout_is_killed_by_the_cap_not_the_deadline() {
    let scratch = Scratch::new("late-stderr");
    let pidfile = scratch.0.join("descendant.pid");
    let wrapper = scratch.script(
        "late-err.sh",
        &format!(
            "( {AFTER_LEADER_EXIT}; head -c 8192 /dev/zero >&2; sleep 30 ) &\necho $! > '{}'\nexit 0\n",
            pidfile.display()
        ),
    );
    let descendant = Descendant::armed(pidfile);
    let deadline = Duration::from_secs(2);
    let mut s = spec(Exec::Path(wrapper), &[], &scratch.0, deadline);
    s.stderr_cap = 4096;
    let outcome = run(s).unwrap();
    assert_eq!(
        outcome.killed,
        Some(KillReason::OutputCap {
            stream: Stream::Stderr
        }),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr.len(), 4096);
    assert!(
        outcome.wall < deadline / 2,
        "the cap, not the deadline, ended the run: {:?}",
        outcome.wall
    );
    assert!(
        descendant.dead_within(GRACE),
        "descendant {} survived the cap kill",
        descendant.pid()
    );
}

// RG1-1 (c): monitors keep sampling after the leader exited; a failing sample kills the
// surviving descendant within two intervals, not at the deadline.
#[test]
fn a_monitor_failing_after_the_leader_exited_kills_the_surviving_descendant() {
    let scratch = Scratch::new("late-monitor");
    let pidfile = scratch.0.join("descendant.pid");
    let wrapper = scratch.script(
        "leave.sh",
        &format!("sleep 30 &\necho $! > '{}'\nexit 0\n", pidfile.display()),
    );
    let descendant = Descendant::armed(pidfile);
    let interval = Duration::from_millis(50);
    let deadline = Duration::from_secs(2);
    let mut s = spec(Exec::Path(wrapper), &[], &scratch.0, deadline);
    s.monitors.push(Box::new(SecondSampleQuota {
        samples: 0,
        interval,
    }));
    let outcome = run(s).unwrap();
    assert_eq!(
        outcome.killed,
        Some(KillReason::Monitor { class: "quota" }),
        "{outcome:?}"
    );
    assert!(
        outcome.wall < deadline / 2,
        "the monitor, not the deadline, ended the run: {:?}",
        outcome.wall
    );
    assert!(
        descendant.dead_within(GRACE),
        "descendant {} survived the monitor kill",
        descendant.pid()
    );
}

/// A monitor whose first sample blocks for `sleep` (RG1-3: a slow filesystem quota walk);
/// `started` records that a sample actually began (`GM2a` residual: the test asserts the slow
/// sample was in flight, not only that the deadline held).
struct SlowSample {
    sleep: Duration,
    interval: Duration,
    started: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl kaban_core::process::Monitor for SlowSample {
    fn sample(&mut self) -> Result<(), KillReason> {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(self.sleep);
        Ok(())
    }

    fn interval(&self) -> Duration {
        self.interval
    }
}

// RG1-3: a sample that blocks past the deadline must not delay the deadline kill: the group is
// killed at the deadline and `run` returns while the sample is still running.
#[test]
fn a_sample_blocking_past_the_deadline_does_not_delay_the_deadline_kill() {
    let scratch = Scratch::new("slow-sample");
    let pidfile = scratch.0.join("grandchild.pid");
    let wrapper = scratch.script(
        "hang.sh",
        &format!(
            "sleep 30 &\necho $! > '{}'\nexec sleep 30\n",
            pidfile.display()
        ),
    );
    let grandchild = Descendant::armed(pidfile);
    let deadline = Duration::from_millis(200);
    let mut s = spec(Exec::Path(wrapper), &[], &scratch.0, deadline);
    let sampling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    s.monitors.push(Box::new(SlowSample {
        sleep: deadline * 2,
        interval: Duration::from_millis(10),
        started: std::sync::Arc::clone(&sampling),
    }));
    let started = Instant::now();
    let outcome = run(s).unwrap();
    let took = started.elapsed();
    assert!(
        sampling.load(std::sync::atomic::Ordering::SeqCst),
        "the slow sample never started: the test proved nothing about a sample in flight"
    );
    assert_eq!(outcome.killed, Some(KillReason::Deadline), "{outcome:?}");
    assert!(
        took < deadline + Duration::from_millis(50),
        "the sample delayed the kill: took {took:?}"
    );
    assert!(
        grandchild.dead_within(GRACE),
        "grandchild {} survived the deadline kill",
        grandchild.pid()
    );
}

// slice 2: stdout past the cap kills the group; the buffer is truncated at the cap
#[test]
fn stdout_past_the_cap_kills_the_group_and_truncates_at_the_cap() {
    let scratch = Scratch::new("cap");
    let mut s = spec(
        Exec::Path(PathBuf::from("/usr/bin/yes")),
        &[],
        &scratch.0,
        Duration::from_secs(5),
    );
    s.stdout_cap = 4096;
    let started = Instant::now();
    let outcome = run(s).unwrap();
    assert_eq!(
        outcome.killed,
        Some(KillReason::OutputCap {
            stream: kaban_core::process::Stream::Stdout
        })
    );
    assert_eq!(outcome.stdout.len(), 4096);
    assert!(outcome.stdout.starts_with(b"y\ny\n"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the cap, not the deadline, ended the run"
    );
}

/// A monitor that passes its first sample and asks for the kill on its second.
struct SecondSampleQuota {
    samples: u32,
    interval: Duration,
}

impl kaban_core::process::Monitor for SecondSampleQuota {
    fn sample(&mut self) -> Result<(), KillReason> {
        self.samples += 1;
        if self.samples >= 2 {
            Err(KillReason::Monitor { class: "quota" })
        } else {
            Ok(())
        }
    }

    fn interval(&self) -> Duration {
        self.interval
    }
}

// slice 3: a monitor erroring on its second sample kills within two intervals
#[test]
fn a_monitor_erroring_on_its_second_sample_kills_within_two_intervals() {
    let scratch = Scratch::new("monitor");
    let interval = Duration::from_millis(50);
    let mut s = spec(
        Exec::Path(PathBuf::from("/usr/bin/sleep")),
        &["30"],
        &scratch.0,
        Duration::from_secs(5),
    );
    s.monitors.push(Box::new(SecondSampleQuota {
        samples: 0,
        interval,
    }));
    let outcome = run(s).unwrap();
    assert_eq!(outcome.killed, Some(KillReason::Monitor { class: "quota" }));
    // two intervals plus the poll and kill slack (OWN-11 margins under parallel load)
    assert!(
        outcome.wall >= interval * 2,
        "killed too early: {:?}",
        outcome.wall
    );
    assert!(
        outcome.wall < interval * 2 + Duration::from_millis(50),
        "took {:?}",
        outcome.wall
    );
    assert!(outcome.status.is_some_and(|st| !st.success()));
}

fn lines(bytes: &[u8]) -> Vec<String> {
    let mut v: Vec<String> = String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    v.sort();
    v
}

// slice 4: the child sees exactly the allowlisted environment; PATH is not inherited unless
// carried
#[test]
fn the_child_sees_exactly_the_allowlisted_environment() {
    let scratch = Scratch::new("env");
    let mut env = EnvAllowlist::new();
    env.insert("KABAN_PROBE", "one").unwrap();
    env.insert("LC_ALL", "C").unwrap();
    let mut s = spec(
        Exec::Path(PathBuf::from("/usr/bin/env")),
        &[],
        &scratch.0,
        Duration::from_secs(5),
    );
    s.env = env;
    let outcome = run(s).unwrap();
    assert_eq!(outcome.killed, None);
    assert!(outcome.status.unwrap().success());
    assert_eq!(lines(&outcome.stdout), vec!["KABAN_PROBE=one", "LC_ALL=C"]);

    // the caller has a PATH; carried by name it reaches the child, nothing else does
    let mut env = EnvAllowlist::new();
    env.carry("PATH").unwrap();
    let mut s = spec(
        Exec::Path(PathBuf::from("/usr/bin/env")),
        &[],
        &scratch.0,
        Duration::from_secs(5),
    );
    s.env = env;
    let outcome = run(s).unwrap();
    let expected = format!("PATH={}", std::env::var("PATH").unwrap());
    assert_eq!(lines(&outcome.stdout), vec![expected]);
}

// slice 6 (RV-1), the refusal direction: caller `GIT_*` variables never reach a child through
// `insert` or `carry`, and the internal keys are not settable from outside
#[test]
fn env_allowlist_rejects_caller_git_vars() {
    let mut env = EnvAllowlist::internal_git();
    for (name, value) in [
        ("GIT_SSH_COMMAND", "ssh -i key"),
        ("GIT_OPTIONAL_LOCKS", "1"),
    ] {
        let err = env.insert(name, value).unwrap_err();
        assert!(
            matches!(
                err,
                kaban_core::process::ProcessError::Env("env_key_refused")
            ),
            "{name}: {err:?}"
        );
    }
    let err = env.carry("GIT_DIR").unwrap_err();
    assert!(matches!(
        err,
        kaban_core::process::ProcessError::Env("env_key_refused")
    ));
    // the seed is untouched by the refused calls
    assert_eq!(env.get("GIT_OPTIONAL_LOCKS").unwrap(), "0");
    assert_eq!(env.get("GIT_TERMINAL_PROMPT").unwrap(), "0");
    assert_eq!(env.len(), 2);
    // the other refused families (G1-S5.3)
    for name in [
        "MY_TOKEN",
        "API_KEY",
        "DB_SECRET",
        "SSH_AUTH_SOCK",
        "AWS_PROFILE",
        "my_token",
    ] {
        assert!(env.insert(name, "x").is_err(), "{name} must be refused");
    }
}

// SO-8: no caller proxy key passes, whatever its case; a constant list is the only way
#[test]
fn env_allowlist_rejects_other_proxy_keys() {
    let mut env = EnvAllowlist::new();
    for name in [
        "HTTP_PROXY",
        "https_proxy",
        "NO_PROXY",
        "all_proxy",
        "GOPROXY",
        "npm_config_proxy",
    ] {
        assert!(
            env.insert(name, "x").is_err(),
            "{name} must be refused by insert"
        );
        assert!(env.carry(name).is_err(), "{name} must be refused by carry");
    }
    assert!(env.is_empty());
}

// slice 6 (RV-1), the presence direction at the core level: `internal_git()` alone gives the
// child exactly the two Git keys
#[test]
fn env_allowlist_internal_git_keys_present() {
    let scratch = Scratch::new("gitenv");
    let mut s = spec(
        Exec::Path(PathBuf::from("/usr/bin/env")),
        &[],
        &scratch.0,
        Duration::from_secs(5),
    );
    s.env = EnvAllowlist::internal_git();
    let outcome = run(s).unwrap();
    assert_eq!(
        lines(&outcome.stdout),
        vec!["GIT_OPTIONAL_LOCKS=0", "GIT_TERMINAL_PROMPT=0"]
    );
}

/// A single-utility ELF for `name`: this host's `/bin/true`, `/bin/false` and `/bin/ls` may be a
/// multicall binary that dispatches on `argv[0]` (uutils), which a `/proc/self/fd/<n>` exec
/// cannot provide, so the GNU single binaries are preferred where the distribution ships them.
fn elf_tool(name: &str) -> PathBuf {
    let gnu = PathBuf::from(format!("/bin/gnu{name}"));
    if gnu.is_file() {
        gnu
    } else {
        PathBuf::from(format!("/bin/{name}"))
    }
}

fn is_elf(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|b| b.starts_with(b"\x7fELF"))
}

// slice 5 (OD-G1-4, RV-9): the handle runs the checked bytes even after the file on disk was
// replaced
#[cfg(target_os = "linux")]
#[test]
fn a_handle_runs_the_checked_bytes_after_the_file_was_replaced_on_disk() {
    let scratch = Scratch::new("swap");
    let (truth, lie) = (elf_tool("true"), elf_tool("false"));
    assert!(is_elf(&truth) && is_elf(&lie), "fixture tools must be ELF");
    let wrapper = scratch.0.join("wrapper");
    std::fs::copy(&truth, &wrapper).unwrap();
    let handle = Exec::open_handle(&wrapper).unwrap();
    // replace the bytes behind the path (a new inode) and then the old inode's content too
    std::fs::remove_file(&wrapper).unwrap();
    std::fs::copy(&lie, &wrapper).unwrap();
    let outcome = run(spec(handle, &[], &scratch.0, Duration::from_secs(5))).unwrap();
    assert_eq!(outcome.killed, None);
    assert!(
        outcome.status.unwrap().success(),
        "the old bytes (true) must have run"
    );
    // the path now executes the replacement: the swap was real
    let outcome = run(spec(
        Exec::Path(wrapper),
        &[],
        &scratch.0,
        Duration::from_secs(5),
    ))
    .unwrap();
    assert!(
        !outcome.status.unwrap().success(),
        "the replaced path runs false"
    );
}

// slice 5b: a `#!` script is refused before spawn (`wrapper_not_elf`), at the open and again at
// `run` for a handle built elsewhere
#[cfg(target_os = "linux")]
#[test]
fn a_script_handle_is_refused_before_spawn() {
    use kaban_core::process::ProcessError;
    use rustix::fs::{Mode, OFlags};
    let scratch = Scratch::new("script");
    let marker = scratch.0.join("ran");
    let script = scratch.script("wrapper.sh", &format!("touch '{}'\n", marker.display()));
    assert!(matches!(
        Exec::open_handle(&script),
        Err(ProcessError::Env("wrapper_not_elf"))
    ));
    let raw = rustix::fs::open(
        &script,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    let err = run(spec(
        Exec::Handle(raw),
        &[],
        &scratch.0,
        Duration::from_secs(5),
    ))
    .unwrap_err();
    assert!(
        matches!(err, ProcessError::Env("wrapper_not_elf")),
        "{err:?}"
    );
    assert!(!marker.exists(), "the script must not have run");
    // the other handle checks: a symlink at the final component is the link itself under
    // O_NOFOLLOW and not a regular file, a directory is not either, a group-writable file is
    // refused, an absent path does not open
    let link = scratch.0.join("link");
    std::os::unix::fs::symlink(elf_tool("true"), &link).unwrap();
    assert!(matches!(
        Exec::open_handle(&link),
        Err(ProcessError::Env("wrapper_not_regular"))
    ));
    assert!(matches!(
        Exec::open_handle(&scratch.0),
        Err(ProcessError::Env("wrapper_not_regular"))
    ));
    let loose = scratch.0.join("loose");
    std::fs::copy(elf_tool("true"), &loose).unwrap();
    std::fs::set_permissions(&loose, std::os::unix::fs::PermissionsExt::from_mode(0o775)).unwrap();
    assert!(matches!(
        Exec::open_handle(&loose),
        Err(ProcessError::Env("wrapper_writable"))
    ));
    assert!(matches!(
        Exec::open_handle(&scratch.0.join("absent")),
        Err(ProcessError::Spawn(_))
    ));
}

/// A `tracing` subscriber that keeps every event as one line `LEVEL field=value ...`
/// (the message is the `message` field), so a test can assert what the runner traced.
#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl Capture {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                let _ = write!(self.0, " {}={value:?}", field.name());
            }
        }
        let mut fields = Fields(event.metadata().level().to_string());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// An owned descriptor of a file in `scratch` whose `FD_CLOEXEC` flag is cleared through the
/// owned handle (safe): the invented inherited descriptor of RG1-2. Closed on drop.
#[cfg(target_os = "linux")]
fn non_cloexec_fixture(scratch: &Scratch) -> (std::fs::File, PathBuf) {
    let path = scratch.0.join("inherited.txt");
    std::fs::write(&path, "leak").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty()).unwrap();
    assert!(
        rustix::io::fcntl_getfd(&file).unwrap().is_empty(),
        "the fixture descriptor has FD_CLOEXEC cleared"
    );
    (file, std::fs::canonicalize(&path).unwrap())
}

// RG1-2, `Refuse` (the default): a descriptor this process holds without `FD_CLOEXEC` would pass
// to the child, so `run` refuses before any spawn and names the descriptor for what it is
// (G7, GO-FIX-1): `InheritedFd { fd }` with its number, the message saying so and the action.
#[cfg(target_os = "linux")]
#[test]
fn a_non_cloexec_descriptor_is_refused_before_spawn_under_refuse() {
    use std::os::fd::AsRawFd;

    use kaban_core::process::ProcessError;
    let scratch = Scratch::new("refuse-fd");
    let (leak, _) = non_cloexec_fixture(&scratch);
    let leaked = u32::try_from(leak.as_raw_fd()).unwrap();
    let marker = scratch.0.join("ran");
    let script = scratch.script("mark.sh", &format!("touch '{}'\n", marker.display()));
    let mut s = spec(Exec::Path(script), &[], &scratch.0, Duration::from_secs(5));
    s.inherited_fds = InheritedFds::Refuse;
    let err = run(s).unwrap_err();
    assert!(
        matches!(err, ProcessError::InheritedFd { fd: Some(fd) } if fd == leaked),
        "{err:?} names descriptor {leaked}"
    );
    assert_eq!(
        err.to_string(),
        format!("inherited an open file descriptor (fd {leaked}); close it in the caller")
    );
    assert!(!marker.exists(), "no child may have been spawned");
    assert!(
        matches!(InheritedFds::default(), InheritedFds::Refuse),
        "Refuse is the default"
    );
}

// RG1-2, `Tolerate`: the child is spawned, inherits the descriptor (visible in its
// `/proc/self/fd`), and the runner traced once at `info` with the count only: no fd number, no
// path (OWN-61 lane decision: `info`, not `warn`, so a hook's stderr stays empty under
// `RUST_LOG=warn`).
#[cfg(target_os = "linux")]
#[test]
fn a_non_cloexec_descriptor_is_tolerated_with_a_count_only_info_trace() {
    let scratch = Scratch::new("tolerate-fd");
    let (_leak, target) = non_cloexec_fixture(&scratch);
    let capture = Capture::default();
    let mut s = spec(
        Exec::Path(PathBuf::from("/bin/ls")),
        &["-l", "/proc/self/fd"],
        &scratch.0,
        Duration::from_secs(5),
    );
    s.inherited_fds = InheritedFds::Tolerate;
    let outcome = tracing::subscriber::with_default(capture.clone(), || run(s)).unwrap();
    assert!(outcome.status.unwrap().success(), "{outcome:?}");
    let listing = String::from_utf8(outcome.stdout).unwrap();
    assert!(
        listing.contains(&format!(" -> {}", target.display())),
        "the tolerated descriptor reached the child: {listing}"
    );
    let lines = capture.lines();
    let traces: Vec<&String> = lines.iter().filter(|l| l.contains("inherited")).collect();
    assert_eq!(traces.len(), 1, "exactly one trace: {lines:?}");
    let trace = traces[0];
    assert!(trace.starts_with("INFO"), "info, not warn: {trace}");
    let count: usize = trace
        .split(" count=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("a count field: {trace}"));
    assert!(count >= 1, "{trace}");
    assert!(
        !trace.contains(&*scratch.0.to_string_lossy()),
        "no path in the trace: {trace}"
    );
    assert!(
        !trace.contains("fd="),
        "no descriptor number in the trace: {trace}"
    );
}

/// The numbers of the descriptors this process holds without `FD_CLOEXEC` (above 2): they were
/// inherited from an ancestor (nextest under `flock(1)` leaves its lock fd open) and pass to
/// every child. The clean-process test closes exactly these in the shell before re-executing
/// the test binary.
#[cfg(target_os = "linux")]
fn non_cloexec_fds() -> Vec<u32> {
    const O_CLOEXEC: u64 = 0o2_000_000;
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        let entry = entry.unwrap();
        let Ok(number) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if number <= 2 {
            continue;
        }
        let Ok(info) = std::fs::read_to_string(format!("/proc/self/fdinfo/{number}")) else {
            continue; // the read_dir handle itself, closed by now
        };
        let flags = info
            .lines()
            .find_map(|l| l.strip_prefix("flags:"))
            .and_then(|f| u64::from_str_radix(f.trim(), 8).ok())
            .unwrap_or(0);
        if flags & O_CLOEXEC == 0 {
            out.push(number);
        }
    }
    out
}

const CLEAN_PROCESS_TEST: &str =
    "a_handle_child_in_a_clean_process_inherits_exactly_the_standard_descriptors";

// slice 5c: descriptor lifecycle: under `Refuse`, a child started through a handle sees exactly
// 0, 1, 2 and the directory descriptor `ls` opens itself; the handle and every descriptor Rust
// opened are CLOEXEC and gone at exec. The assertion needs a process holding no non-CLOEXEC
// descriptor, which the test runner does not guarantee (nextest under `flock(1)` leaks the lock
// fd), so this test re-executes the test binary for the ignored clean-process test below
// through `run` itself, with a shell closing exactly the leaked descriptors before the exec.
#[cfg(target_os = "linux")]
#[test]
fn a_handle_child_inherits_only_the_standard_descriptors() {
    let scratch = Scratch::new("fds-outer");
    let exe = std::env::current_exe().unwrap();
    let closes = non_cloexec_fds().iter().fold(String::new(), |mut acc, n| {
        use std::fmt::Write;
        let _ = write!(acc, " {n}>&-");
        acc
    });
    let script = format!("exec{closes}\nexec \"$1\" --exact \"$2\" --ignored --nocapture\n");
    let mut s = spec(
        Exec::Path(PathBuf::from("/bin/sh")),
        &[
            "-c",
            &script,
            "sh",
            &exe.to_string_lossy(),
            CLEAN_PROCESS_TEST,
        ],
        &scratch.0,
        Duration::from_secs(60),
    );
    s.stderr_cap = 64 * 1024;
    s.inherited_fds = InheritedFds::Tolerate;
    let outcome = run(s).unwrap();
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    assert_eq!(outcome.killed, None, "{stdout}\n{stderr}");
    assert!(
        outcome.status.unwrap().success(),
        "the clean-process test failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the clean-process test ran:\n{stdout}\n{stderr}"
    );
}

// The clean-process half of slice 5c: run by the test above in a process whose only open
// descriptors are 0, 1 and 2 (run directly under a launcher that leaks a descriptor, `Refuse`
// refuses, which is RG1-2's contract, not this test's subject).
#[cfg(target_os = "linux")]
#[test]
#[ignore = "re-executed in a clean child by a_handle_child_inherits_only_the_standard_descriptors"]
fn a_handle_child_in_a_clean_process_inherits_exactly_the_standard_descriptors() {
    let scratch = Scratch::new("fds-clean");
    let wrapper = scratch.0.join("ls");
    std::fs::copy(elf_tool("ls"), &wrapper).unwrap();
    // an extra parent descriptor that must not leak (Rust opens with CLOEXEC)
    let held = std::fs::File::open("/proc/self/stat").unwrap();
    let handle = Exec::open_handle(&wrapper).unwrap();
    let mut s = spec(
        handle,
        &["-l", "/proc/self/fd"],
        &scratch.0,
        Duration::from_secs(5),
    );
    s.inherited_fds = InheritedFds::Refuse;
    let outcome = run(s).unwrap();
    drop(held);
    assert!(outcome.status.unwrap().success(), "{outcome:?}");
    let listing = String::from_utf8(outcome.stdout).unwrap();
    let mut seen: Vec<(u32, String)> = listing
        .lines()
        .filter_map(|line| {
            let (left, target) = line.split_once(" -> ")?;
            let number = left.rsplit(' ').next()?.parse().ok()?;
            Some((number, target.to_owned()))
        })
        .collect();
    seen.sort();
    let numbers: Vec<u32> = seen.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        numbers,
        vec![0, 1, 2, 3],
        "exactly the standard descriptors and ls's own: {listing}"
    );
    assert!(
        seen[3].1.starts_with("/proc/") && seen[3].1.ends_with("/fd"),
        "fd 3 is ls's own directory descriptor: {listing}"
    );
}
