//! The `serve` wiring of the code-graph provider (stage1-plan CG5.2; stage1-spec G1-F8 serve
//! side, G1-S7): `kaban serve` reads `<KABAN_HOME>/keys/codegraph.key` and never creates it
//! nor the `codegraph/` directory; a key readable by others is refused at startup as the audit
//! key is (S5 class), with a fixed text that names no path.
//!
//! The server runs in the test's working directory (the crate directory inside this
//! repository), so the `ready` state is reached through the real `git rev-parse
//! --git-common-dir` of the enclosing checkout.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn codegraph_key(s: &Scratch) -> PathBuf {
    s.home().join("keys").join("codegraph.key")
}

/// `kaban serve` over `s` with stdin closed, under `RUST_LOG=trace`, optionally with its
/// address space bounded (`ulimit -v <KiB>` in a shell): the exit code and stderr once it
/// finished within `deadline`, `None` (the process killed) when it did not.
fn serve_outcome(
    s: &Scratch,
    limit_as_kib: Option<u64>,
    deadline: Duration,
) -> Option<(Option<i32>, String)> {
    let mut cmd = match limit_as_kib {
        Some(kib) => {
            let mut sh = Command::new("sh");
            sh.arg("-c")
                .arg(format!("ulimit -v {kib} && exec \"$0\" \"$@\""))
                .arg(env!("CARGO_BIN_EXE_kaban"));
            sh
        }
        None => Command::new(env!("CARGO_BIN_EXE_kaban")),
    };
    let mut child = cmd
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "trace")
        .args(["--db", s.db().to_str().unwrap(), "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut err, &mut text);
        text
    });
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some((status.code(), reader.join().unwrap()));
        }
        if started.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The one refusal shape of the three RU3-4 cases: exit 4, the fixed text with `class`, no
/// path in the text.
fn assert_key_refused(s: &Scratch, outcome: Option<(Option<i32>, String)>, class: &str) {
    let (code, stderr) = outcome.expect("serve finished within the deadline");
    assert_eq!(code, Some(4), "a config error: {stderr}");
    assert!(
        stderr.contains(&format!("codegraph key refused ({class})")),
        "the fixed text with the class {class}: {stderr}"
    );
    let key_path = codegraph_key(s).to_string_lossy().into_owned();
    assert!(!stderr.contains(&key_path), "no path in the text: {stderr}");
}

/// RU3-4: a FIFO at the key path is refused without blocking startup (the open is
/// non-blocking and the object is inspected on the descriptor, never read).
#[test]
fn serve_refuses_fifo_codegraph_key_without_blocking() {
    let s = Scratch::new("kaban-serve-codegraph", "fifo-key");
    let path = codegraph_key(&s);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let made = Command::new("mkfifo")
        .args(["-m", "600"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success(), "mkfifo");
    let outcome = serve_outcome(&s, None, Duration::from_secs(10));
    assert!(
        outcome.is_some(),
        "a FIFO with no writer must not block the key read past the 10 s wall clock"
    );
    assert_key_refused(&s, outcome, "not_regular_file");
}

/// RU3-4: the key read is bounded to `KEY_LEN + 1` bytes. A 16 GiB sparse key under a 6 GiB
/// address-space limit: a reader that sizes its buffer by the file cannot even allocate
/// (the base answered `io`); the bounded reader stops after 33 bytes and answers `length`.
#[test]
fn serve_refuses_oversize_codegraph_key_after_a_bounded_read() {
    let s = Scratch::new("kaban-serve-codegraph", "oversize-key");
    write_key(&s, 0o600);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(codegraph_key(&s))
        .unwrap();
    file.set_len(16 << 30).unwrap();
    drop(file);
    let outcome = serve_outcome(&s, Some(6 << 20), Duration::from_secs(10));
    assert_key_refused(&s, outcome, "length");
}

/// RU3-4: a symbolic link at the key path is refused by the open itself (`O_NOFOLLOW`), the
/// target never read.
#[test]
fn serve_refuses_symlinked_codegraph_key() {
    let s = Scratch::new("kaban-serve-codegraph", "symlink-key");
    let path = codegraph_key(&s);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let real = s.0.join("real.key");
    std::fs::write(&real, (0..32u8).collect::<Vec<u8>>()).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&real, &path).unwrap();
    let outcome = serve_outcome(&s, None, Duration::from_secs(10));
    assert_key_refused(&s, outcome, "symlink");
}

/// A 32-byte key at `keys/codegraph.key` with `mode`.
fn write_key(s: &Scratch, mode: u32) -> Vec<u8> {
    let path = codegraph_key(s);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes: Vec<u8> = (0..32u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    bytes
}

/// The provider's one startup line, `state=<s>` in either field rendering.
fn has_state(stderr: &str, state: &str) -> bool {
    stderr.contains("codegraph provider")
        && (stderr.contains(&format!("state=\"{state}\""))
            || stderr.contains(&format!("state={state}")))
}

#[test]
fn serve_reads_key_never_creates() {
    let s = Scratch::new("kaban-serve-codegraph", "key");

    // no key file: the provider is absent, nothing is created
    let srv = Server::start_traced(&s, &[]);
    let (status, stderr) = srv.finish_with_stderr();
    assert!(status.success(), "serve without a key runs: {stderr}");
    assert!(
        has_state(&stderr, "no_key"),
        "state no_key traced: {stderr}"
    );
    assert!(
        !codegraph_key(&s).exists(),
        "serve never creates the codegraph key"
    );
    assert!(
        !s.home().join("codegraph").exists(),
        "serve never creates the codegraph directory"
    );

    // a 0600 key in a repository: the provider is ready, still nothing is created or changed
    let bytes = write_key(&s, 0o600);
    let srv = Server::start_traced(&s, &[]);
    let (status, stderr) = srv.finish_with_stderr();
    assert!(status.success(), "serve with a key runs: {stderr}");
    assert!(has_state(&stderr, "ready"), "state ready traced: {stderr}");
    assert_eq!(
        std::fs::read(codegraph_key(&s)).unwrap(),
        bytes,
        "key unchanged"
    );
    assert_eq!(
        std::fs::metadata(codegraph_key(&s))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        !s.home().join("codegraph").exists(),
        "the provider is built over a directory that need not exist; nothing is created"
    );
    let key_path = codegraph_key(&s).to_string_lossy().into_owned();
    assert!(!stderr.contains(&key_path), "the key path is never logged");
}

#[test]
fn serve_refuses_loose_codegraph_key() {
    let s = Scratch::new("kaban-serve-codegraph", "loose-key");
    write_key(&s, 0o644);
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "trace")
        .args(["--db", s.db().to_str().unwrap(), "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "a config error: {stderr}");
    assert!(
        stderr.contains("codegraph key refused (mode)"),
        "the fixed text with the class: {stderr}"
    );
    let key_path = codegraph_key(&s).to_string_lossy().into_owned();
    assert!(!stderr.contains(&key_path), "no path in the text: {stderr}");
    assert!(
        out.stdout.is_empty(),
        "stdout stays empty on a startup failure"
    );
}

/// CG5.2a regression (core S-series fixed-error contract, EN-F7; `scripts/mcp/mcp_smoke.py`
/// `fixed_error`): a CLI error path prints exactly the fixed line on stderr under
/// `RUST_LOG=trace`; the provider's state line is `serve`'s alone.
#[test]
fn cli_error_path_stderr_is_the_fixed_line_only() {
    let s = Scratch::new("kaban-serve-codegraph", "cli-stderr");
    // the CLI runs in a repository (as the smoke does); the ontology file sits inside it
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "init", "-q", "-b", "main"])
        .status()
        .unwrap();
    assert!(git.success());
    std::fs::write(
        repo.join("ident.yaml"),
        "ontology: shop\nversion: 1\nkinds:\n  vendor-thing: {description: \"lowercase\"}\nrels: {}\n",
    )
    .unwrap();
    let kaban = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "trace")
            .current_dir(&repo)
            .args(["--db", s.db().to_str().unwrap(), "--json"])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    // a first open creates and migrates the database (its own log line); the error path runs
    // on the warm database, as in the smoke
    let warm = kaban(&["ontology", "list"]);
    assert_eq!(warm.status.code(), Some(0));
    let out = kaban(&["ontology", "load", "ident.yaml"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(out.stdout.is_empty(), "stdout stays empty on a fixed error");
    assert_eq!(
        stderr.trim(),
        "error: invalid_identifier",
        "stderr is the fixed line and nothing else"
    );
}
