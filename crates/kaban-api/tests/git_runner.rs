//! `GitRunner` deadline (core-plan C3.3, R-C3.2-4; the surviving oracle of the deleted
//! currency goldens, DEL-08): a git that hangs with a grandchild holding its stdout pipe is
//! answered `GitError::Timeout` within the slice budget, and the process-group kill reaches
//! the grandchild. Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_api::{GitError, GitRunner};
use std::time::Duration;

/// Whether `pid` is still a live process (a reaped or zombie process is not).
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            // "<pid> (<comm>) <state> ..."; comm may contain spaces, so split after ')'
            let after = s.rsplit(')').next()?.trim_start();
            after.chars().next()
        })
        .is_some_and(|state| state != 'Z' && state != 'X')
}

#[test]
fn a_hanging_git_with_a_grandchild_holding_the_pipe_times_out_within_the_budget() {
    let scratch = common::Scratch::new("kaban-git-runner", "timeout");
    let root = scratch.0.clone();
    let pidfile = root.join("grandchild.pid");
    let wrapper = root.join("slow-git.sh");
    // the wrapper forks a grandchild that inherits the stdout pipe, then hangs itself
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nexec sleep 30\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let git = GitRunner {
        program: wrapper,
        timeout: Duration::from_millis(200),
    };
    let started = std::time::Instant::now();
    let result = git.run(&root, &["log", "--format="]);
    let took = started.elapsed();
    assert!(matches!(result, Err(GitError::Timeout(_))), "{result:?}");
    // the product deadline is 200 ms; the slice budget (core-plan C3.3) is 250 ms
    assert!(took < Duration::from_millis(250), "took {took:?}");
    // the grandchild inherited the pipe; the group kill reached it (R-C3.2-4)
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    assert_ne!(pid, "");
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    while alive(&pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!alive(&pid), "grandchild {pid} survived the group kill");
}
