//! CG4.1 (stage1-spec G1-S6, §4.1; stage1-plan CG4.1): repository identity, `<repo-id>`, the
//! trust mark and its revalidation, through the `kaban-index` binary and the public library
//! surface. Every repository is a scripted `git init` in a scratch directory under `/tmp`
//! (`test-integration-dir`, `test-fixture-raii`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kaban_codegraph::identity::{self, FsIdentity, IdentityError, RepoIdKey, RepoIdentity};
use kaban_core::process::InheritedFds;
use kaban_index::identity_cli;
use kaban_index::trust::{self, TRUST_TEXT, TrustError};
use kaban_policy::Policy;

/// A scratch directory deleted on drop, test failure included.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-cg41-{tag}-{}-{}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `git <args>` in `cwd` with a fixed identity and dates, so equal content gives equal hashes.
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

/// A repository with one commit `file -> content` at `dir`.
fn repo_with_commit(dir: &Path, content: &str) {
    fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("a.txt"), content).expect("write");
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
}

/// The numbers of the descriptors this process holds without `FD_CLOEXEC` (above 2): inherited
/// from an ancestor (nextest under `flock(1)` leaks its lock descriptor); closed in the shell
/// before the binary starts (the CG1.1 clean-process pattern), so the production descriptor
/// policy of the binary's git calls (`InheritedFds::Refuse`, U2 RU2-4) sees a clean process.
fn non_cloexec_fds() -> Vec<u32> {
    kaban_core::process::inheritable_fds().expect("descriptor observation is required")
}

/// Run the binary with `KABAN_HOME=<home>` in a child holding only the standard descriptors.
fn kaban_index(home: &Path, args: &[&str]) -> Output {
    let closes = non_cloexec_fds().iter().fold(String::new(), |mut acc, n| {
        let _ = write!(acc, " {n}>&-");
        acc
    });
    let script = format!("exec{closes}\nexec \"$0\" \"$@\"\n");
    Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_kaban-index"))
        .args(args)
        .env("KABAN_HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("kaban-index runs")
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("one JSON report on stdout")
}

fn repo_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(home.join("codegraph")) else {
        return Vec::new();
    };
    rd.map(|e| e.expect("entry").path()).collect()
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Darwin opens tool descriptors read-only rather than with Linux O_PATH. A FIFO must
/// still be rejected before waiting for a writer. Run the attempted admission in a child
/// so a blocking regression is killed and reaped before the assertion fails.
#[cfg(target_os = "macos")]
#[test]
fn tool_admission_refuses_fifo_without_blocking() {
    use kaban_index::tools::{ToolError, admit_executable};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    const CHILD_FIFO: &str = "KABAN_TEST_ADMISSION_FIFO";
    if let Some(path) = std::env::var_os(CHILD_FIFO) {
        assert_eq!(
            admit_executable(Path::new(&path), &[]),
            Err(ToolError::UntrustedMode)
        );
        return;
    }

    let scratch = Scratch::new("tool-fifo");
    let fifo = scratch.path().join("not-an-executable");
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tool_admission_refuses_fifo_without_blocking",
            "--nocapture",
        ])
        .env(CHILD_FIFO, &fifo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let completed = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        completed,
        "tool admission blocked on a FIFO without a writer"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_tool_admission_tracks_installer_links_and_refuses_replacement() {
    use kaban_index::tools::{ToolError, admit_executable, recheck};

    let scratch = Scratch::new("tool-installer-link");
    let installed = scratch.path().join("installed/bin");
    let links = scratch.path().join("links");
    fs::create_dir_all(&installed).unwrap();
    fs::create_dir(&links).unwrap();
    let tool = installed.join("producer");
    fs::write(&tool, b"invented executable bytes").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let link = links.join("producer");
    std::os::unix::fs::symlink(&tool, &link).unwrap();
    let admitted = admit_executable(&link, &[]).unwrap();
    assert_eq!(admitted.path, fs::canonicalize(&tool).unwrap());
    assert_eq!(recheck(&admitted), Ok(()));
    assert_eq!(
        admit_executable(&link, &[scratch.path()]),
        Err(ToolError::UntrustedLocation)
    );

    // Retain the old object under another name so inode reuse cannot hide replacement.
    fs::rename(&tool, installed.join("previous-producer")).unwrap();
    fs::write(&tool, b"replacement executable bytes").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(recheck(&admitted), Err(ToolError::Changed));
}

#[cfg(target_os = "macos")]
#[test]
fn native_namespace_backend_refuses_and_preserves_isolation_policy() {
    use kaban_core::config::{Index, Isolation};
    use kaban_index::isolate::{self, ReasonClass, SelectError, Tier, namespaces};
    use namespaces::{Expect, HelperMode, HelperOpts, NsError, NsInodes, View};

    // No helper or producer exists here. An unavailable mechanism must be reported before
    // any attempt to open their paths, read procfs, construct a view or spawn a process.
    let absent = Path::new("/kaban-m1-nonexistent-helper");
    let probes = isolate::probe_all(&Index::default(), absent);
    assert_eq!(probes.len(), 2);
    assert_eq!(probes[0].tier, Tier::Namespaces);
    assert_eq!(probes[0].available, Err(ReasonClass::HandleExecUnsupported));
    assert_eq!(probes[1].available, Err(ReasonClass::NotInThisRelease));
    let preferred = isolate::select(Isolation::Preferred, &probes).unwrap();
    assert_eq!(preferred.tier, Tier::None);
    assert_eq!(preferred.warning, Some(isolate::ISOLATION_NONE));
    let off = isolate::select(Isolation::Off, &probes).unwrap();
    assert_eq!((off.tier, off.warning), (Tier::None, None));
    let refused = isolate::select(Isolation::Required, &probes).unwrap_err();
    assert_eq!(refused, SelectError::RequiredUnavailable);
    assert_eq!(refused.exit_code(), 8);
    assert!(namespaces::NsInodes::current().is_err());
    assert_eq!(
        namespaces::run_tool_in_view(absent, absent, &[], std::time::Duration::from_secs(1), &[])
            .unwrap_err(),
        ReasonClass::HandleExecUnsupported
    );
    let opts = HelperOpts {
        parent: NsInodes {
            user: 1,
            net: 2,
            mnt: 3,
        },
        view: View {
            checkout: absent.to_owned(),
            scratch: absent.to_owned(),
            ro: Vec::new(),
            caches: Vec::new(),
        },
        mode: HelperMode::Probe,
        expect: Expect::default(),
    };
    assert!(matches!(
        namespaces::run_helper(&opts),
        Err(NsError::Setup {
            op: "namespaces_unsupported",
            source: rustix::io::Errno::NOSYS
        })
    ));
    assert_eq!(
        namespaces::self_exe().unwrap(),
        std::env::current_exe().unwrap()
    );
}

/// Every leaf of the record is a number, a 40-hex id, a semver-shaped version or null (RS2-2).
fn assert_no_strings(v: &serde_json::Value, path: &str) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                assert_no_strings(child, &format!("{path}.{k}"));
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_no_strings(item, path);
            }
        }
        serde_json::Value::String(s) => {
            let ok = if path.ends_with(".root_commits") {
                is_hex(s, 40)
            } else if path.ends_with(".kaban_index_version") {
                s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+')
            } else {
                false
            };
            assert!(ok, "free string at {path}");
        }
        serde_json::Value::Number(_) | serde_json::Value::Null => {}
        serde_json::Value::Bool(_) => panic!("unexpected bool at {path}"),
    }
}

fn live_identity(repo: &Path) -> (identity_cli::Resolved, RepoIdentity) {
    let resolved = identity_cli::resolve_with(repo, InheritedFds::Tolerate).expect("resolves");
    let id = resolved.identity.clone();
    (resolved, id)
}

fn admitted(home: &Path, repo: &Path) -> identity_cli::AdmittedDir {
    let resolved = identity_cli::resolve_with(repo, InheritedFds::Tolerate).expect("resolves");
    let key = identity_cli::open_key(home).expect("key");
    let id = identity_cli::repo_id_gated(&Policy::empty(), &key, &resolved.canonical_common_dir)
        .expect("gated");
    identity_cli::admit_repo_dir(home, &id).expect("admitted")
}

/// RU2-15 (OWN-75): a redact-class hit on the canonical common directory refuses the derivation
/// like a reject hit does; a masked path is still a path kaban must not key on.
#[test]
fn repo_id_gated_refuses_redact_hit() {
    let s = Scratch::new("redact-gate");
    let home = s.path().join("home");
    let repo = s.path().join("CANARY_REDACT_4b2d");
    repo_with_commit(&repo, "one");
    fs::create_dir_all(&home).unwrap();
    let key = identity_cli::open_key(&home).expect("key");
    let resolved = identity_cli::resolve_with(&repo, InheritedFds::Tolerate).expect("resolves");
    let redacting = Policy::from_file(
        Policy::parse(b"words:\n  - { term: CANARY_REDACT_4b2d, action: redact, id: canary }\n")
            .unwrap(),
        "t".into(),
    )
    .unwrap();
    assert!(
        !redacting.scan(&resolved.canonical_common_dir).rejected,
        "the fixture is a redact hit, not a reject hit"
    );
    let r = identity_cli::repo_id_gated(&redacting, &key, &resolved.canonical_common_dir);
    assert!(
        matches!(r, Err(identity_cli::IdentityCliError::RepoPathRefused)),
        "a redact hit is refused like a reject hit: {r:?}"
    );
    // a clean policy still derives (the refusal is the policy's verdict, not the path's shape)
    identity_cli::repo_id_gated(&Policy::empty(), &key, &resolved.canonical_common_dir)
        .expect("clean policy derives");
}

// --- slice 1: `trust` writes the mark (CLI), the record has no strings ---------------------

#[test]
fn cli_trust_writes_mark_0600_and_trust_record_has_no_strings() {
    let s = Scratch::new("mark");
    let home = s.path().join("home");
    let repo = s.path().join("repo");
    repo_with_commit(&repo, "one");

    let out = kaban_index(&home, &["trust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = stdout_json(&out);
    assert_eq!(report["verb"], "trust");
    assert_eq!(report["ok"], true);
    let repo_id = report["repo_id"].as_str().expect("repo_id");
    assert!(
        is_hex(repo_id, 32),
        "repo_id is 32 lowercase hex: {repo_id}"
    );

    // the fixed text, one sentence per line, on stderr
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in TRUST_TEXT {
        assert!(
            stderr.lines().any(|l| l == *line),
            "trust text line missing: {line}"
        );
    }

    let dir = home.join("codegraph").join(repo_id);
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    let mark = dir.join("trust");
    assert_eq!(
        fs::metadata(&mark).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&mark).unwrap()).unwrap();
    assert_eq!(record["v"], 1);
    assert_eq!(record["root_commits"].as_array().unwrap().len(), 1);
    assert_no_strings(&record, "");
    // no path anywhere in the record bytes
    let bytes = fs::read(&mark).unwrap();
    assert!(
        !bytes.windows(4).any(|w| w == b"repo"),
        "a path fragment in the record"
    );

    // a second `trust` is refused as already trusted, exit 4
    let again = kaban_index(&home, &["trust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(4));
    assert_eq!(stdout_json(&again)["class"], "already_trusted");

    // `untrust` removes the mark; a second one is `trust_refused`
    let un = kaban_index(&home, &["untrust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(un.status.code(), Some(0));
    assert!(!mark.exists(), "cli_untrust_removes");
    let un2 = kaban_index(&home, &["untrust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(un2.status.code(), Some(4));
    assert_eq!(stdout_json(&un2)["class"], "trust_refused");
}

// --- slice 2: revalidation refuses without a mark; survives new commits -------------------

#[test]
fn cli_refuses_without_trust_mark_and_trust_survives_same_dir_same_history() {
    let s = Scratch::new("survive");
    let home = s.path().join("home");
    let repo = s.path().join("repo");
    repo_with_commit(&repo, "one");
    let dir = admitted(&home, &repo);
    let (_, id) = live_identity(&repo);
    assert_eq!(
        trust::revalidate(&dir.handle, &id),
        Err(TrustError::NotTrusted)
    );

    trust::trust(&dir.handle, &id, "0.0.0-test").expect("mark");
    // new commits on the same repository: still trusted; the recorded version is never compared
    fs::write(repo.join("b.txt"), "two").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-q", "-m", "c2"]);
    let (_, id2) = live_identity(&repo);
    assert_eq!(trust::revalidate(&dir.handle, &id2), Ok(()));
    assert!(dir.path.join("trust").exists());
}

// --- slice 3: the mark binds the directory object --------------------------------------------

#[test]
fn trust_binds_dev_inode() {
    let s = Scratch::new("inode");
    let home = s.path().join("home");
    let repo = s.path().join("repo");
    repo_with_commit(&repo, "one");
    let dir = admitted(&home, &repo);
    let (_, id) = live_identity(&repo);
    trust::trust(&dir.handle, &id, "t").expect("mark");

    // rm -rf, then the same history at the same path: a new directory object
    fs::remove_dir_all(&repo).unwrap();
    repo_with_commit(&repo, "one");
    let (_, id2) = live_identity(&repo);
    assert_eq!(
        id.root_commits, id2.root_commits,
        "same history reproduces the root commit"
    );
    assert_eq!(
        trust::revalidate(&dir.handle, &id2),
        Err(TrustError::Revoked)
    );
    assert!(
        !dir.path.join("trust").exists(),
        "mark file gone after revocation"
    );
    // the text is the fixed one
    assert_eq!(TrustError::Revoked.to_string(), trust::REVOKED_TEXT);
    assert_eq!(TrustError::Revoked.exit_code(), 4);
}

// --- slice 4: the mark binds the root commits -------------------------------------------------

#[test]
fn trust_binds_root_commits() {
    let s = Scratch::new("roots");
    let home = s.path().join("home");
    let repo = s.path().join("repo");
    repo_with_commit(&repo, "one");
    let dir = admitted(&home, &repo);
    let (_, id) = live_identity(&repo);
    trust::trust(&dir.handle, &id, "t").expect("mark");

    // replace the common directory contents in place: the `.git` object survives, history differs
    for entry in fs::read_dir(repo.join(".git")).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            fs::remove_dir_all(&p).unwrap();
        } else {
            fs::remove_file(&p).unwrap();
        }
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "different").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "other"]);
    let (_, id2) = live_identity(&repo);
    assert_eq!(
        (id.fs.dev, id.fs.ino),
        (id2.fs.dev, id2.fs.ino),
        "same directory object"
    );
    assert_ne!(id.root_commits, id2.root_commits);
    assert_eq!(
        trust::revalidate(&dir.handle, &id2),
        Err(TrustError::Revoked)
    );
    assert!(!dir.path.join("trust").exists());
}

// --- slice 5: symlink races, repository path and data directory -------------------------------

#[test]
fn trust_refuses_symlink_race() {
    let s = Scratch::new("race");
    let real = s.path().join("real");
    fs::create_dir_all(real.join("inner")).unwrap();
    let link = s.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // the path was canonical when checked; a component is a symlink now
    let raced = link.join("inner");
    assert_eq!(
        identity::walk(&raced).unwrap_err(),
        IdentityError::SymlinkComponent
    );
    assert_eq!(identity::walk(&real.join("inner")).map(|_| ()), Ok(()));
    assert_eq!(
        identity::walk(Path::new("rel/x")).unwrap_err(),
        IdentityError::NotAbsolute
    );

    // the sibling case: the data directory `<home>/codegraph` is a symlink: refused, nothing
    // created behind it, nothing persisted
    let home = s.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let elsewhere = s.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.join("codegraph")).unwrap();
    let err = identity_cli::admit_repo_dir(&home, "0123456789abcdef0123456789abcdef").unwrap_err();
    assert!(
        matches!(err, identity_cli::IdentityCliError::DataDir("symlink")),
        "{err:?}"
    );
    assert!(
        fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing created behind the link"
    );

    // a group-readable repository directory is refused too
    let home2 = s.path().join("home2");
    let loose = home2
        .join("codegraph")
        .join("0123456789abcdef0123456789abcdef");
    fs::create_dir_all(&loose).unwrap();
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o750)).unwrap();
    let err = identity_cli::admit_repo_dir(&home2, "0123456789abcdef0123456789abcdef").unwrap_err();
    assert!(
        matches!(err, identity_cli::IdentityCliError::DataDir("mode")),
        "{err:?}"
    );
}

// --- slice 6: no commits ------------------------------------------------------------------------

#[test]
fn trust_refuses_no_commits() {
    let s = Scratch::new("nocommit");
    let home = s.path().join("home");
    let repo = s.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let out = kaban_index(&home, &["trust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(stdout_json(&out)["class"], "trust_refused");
    assert!(
        repo_dirs(&home).is_empty(),
        "no repository directory created"
    );
    // the library class on the rev-list output itself
    assert_eq!(identity::root_commits("\n"), Err(IdentityError::NoCommits));
    assert_eq!(
        identity::root_commits("abc\n"),
        Err(IdentityError::MalformedCommit)
    );
    assert_eq!(
        IdentityError::NoCommits.to_string(),
        "repository has no commits"
    );
    // sorted and de-duplicated
    let b = "b".repeat(40);
    let a = "a".repeat(40);
    assert_eq!(
        identity::root_commits(&format!("{b}\n{a}\n{b}\n")).unwrap(),
        vec![a, b]
    );
}

// --- slice 7: linked worktrees -----------------------------------------------------------------

#[test]
fn worktree_reciprocal_validation() {
    let s = Scratch::new("wt");
    let home = s.path().join("home");
    let repo = s.path().join("repo");
    repo_with_commit(&repo, "one");
    let wt = s.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
    );

    // the legitimate linked worktree resolves to the main repository's common directory
    let main = identity_cli::resolve_with(&repo, InheritedFds::Tolerate).expect("main");
    let linked = identity_cli::resolve_with(&wt, InheritedFds::Tolerate).expect("linked worktree");
    assert_eq!(main.canonical_common_dir, linked.canonical_common_dir);
    assert_eq!(main.identity, linked.identity);

    // trusted through the main repository's mark
    let dir = admitted(&home, &repo);
    trust::trust(&dir.handle, &main.identity, "t").unwrap();
    assert_eq!(trust::revalidate(&dir.handle, &linked.identity), Ok(()));

    // a forged `.git` file pointing at another repository's common directory: refused
    let other = s.path().join("other");
    repo_with_commit(&other, "other");
    let forged = s.path().join("forged");
    fs::create_dir_all(&forged).unwrap();
    fs::write(
        forged.join(".git"),
        format!("gitdir: {}\n", other.join(".git").display()),
    )
    .unwrap();
    let err = identity_cli::resolve_with(&forged, InheritedFds::Tolerate).unwrap_err();
    assert!(
        matches!(
            err,
            identity_cli::IdentityCliError::Identity(IdentityError::WorktreeForged)
                | identity_cli::IdentityCliError::NotARepository
        ),
        "{err:?}"
    );
    // a `.git` file pointing into the real worktrees directory of another worktree's slot
    // (the back pointer names the other worktree): refused
    let forged2 = s.path().join("forged2");
    fs::create_dir_all(&forged2).unwrap();
    let slot = fs::read_to_string(wt.join(".git")).unwrap();
    fs::write(forged2.join(".git"), slot).unwrap();
    let err = identity_cli::resolve_with(&forged2, InheritedFds::Tolerate).unwrap_err();
    assert!(
        matches!(
            err,
            identity_cli::IdentityCliError::Identity(IdentityError::WorktreeForged)
                | identity_cli::IdentityCliError::NotARepository
        ),
        "{err:?}"
    );
}

// --- slice 8: the gate before `repo_id`, keyed ids -------------------------------------------

fn policy_file(dir: &Path, action: &str, pattern: &str) -> PathBuf {
    let path = dir.join("policy.yaml");
    fs::write(
        &path,
        format!(
            "version: 1\ndefaults:\n  action: {action}\n  mask: \"[REDACTED:{{rule}}]\"\n  scan_on_read: true\nextend:\n  use_builtin: false\nrules:\n  - id: canary-path\n    description: test\n    regex: '{pattern}'\n    class: generic\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn repo_id_requires_gated_path() {
    let s = Scratch::new("gate");
    let home = s.path().join("home");
    let repo = s.path().join("CANARY_PATH_7f3a");
    repo_with_commit(&repo, "one");
    let policy = policy_file(s.path(), "reject", "CANARY_PATH_7f3a");
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "trust",
            "--repo",
            repo.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(stdout_json(&out)["class"], "repo_path_refused");
    assert!(!home.join("codegraph").exists(), "no directory created");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("CANARY_PATH_7f3a"),
        "the path is not echoed"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("CANARY_PATH_7f3a"),
        "the path is not in the report"
    );
}

#[test]
fn repo_id_is_keyed() {
    let k1 = RepoIdKey::new([1u8; 32]);
    let k2 = RepoIdKey::new([2u8; 32]);
    let a = identity::repo_id(&k1, "/srv/repo/.git");
    let b = identity::repo_id(&k2, "/srv/repo/.git");
    assert!(is_hex(&a, 32));
    assert_ne!(a, b, "two keys, two ids");
    assert_eq!(
        a,
        identity::repo_id(&RepoIdKey::new([1u8; 32]), "/srv/repo/.git")
    );
    assert_ne!(a, identity::repo_id(&k1, "/srv/other/.git"));
    assert_eq!(format!("{k1:?}"), "RepoIdKey([redacted])");
}

// --- slice 9: the live path is scanned, never persisted -----------------------------------------

/// Assert `needle` is on no byte of any file under `dir`.
fn scan(dir: &Path, needle: &[u8]) {
    for entry in fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            scan(&p, needle);
        } else {
            let bytes = fs::read(&p).unwrap();
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle),
                "canary persisted in {}",
                p.display()
            );
        }
    }
}

/// The live path is scanned wherever it is shown: `display_path` masks a redact hit, and the
/// CLI never echoes it. Since OWN-75 (RU2-15) a redact hit on the repository path refuses the
/// derivation like a reject hit, so `trust` exits 4 here instead of printing the masked line.
#[test]
fn status_shows_live_path_scanned() {
    let s = Scratch::new("live");
    let home = s.path().join("home");
    let repo = s.path().join("CANARY_LIVE_9c1e");
    repo_with_commit(&repo, "one");
    let policy = policy_file(s.path(), "redact", "CANARY_LIVE_9c1e");
    let shown = identity_cli::display_path(
        &Policy::from_file(
            Policy::parse(&fs::read(&policy).unwrap()).unwrap(),
            "t".into(),
        )
        .unwrap(),
        &repo,
    );
    assert!(
        shown.contains("[REDACTED:canary-path]"),
        "redacted: {shown}"
    );
    assert!(!shown.contains("CANARY_LIVE_9c1e"));
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "trust",
            "--repo",
            repo.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(stdout_json(&out)["class"], "repo_path_refused");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("CANARY_LIVE_9c1e"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("CANARY_LIVE_9c1e"));
    assert!(!home.join("codegraph").exists(), "no directory created");
    // byte scan of everything under the data directory (the key directory, when created)
    if home.exists() {
        scan(&home, b"CANARY_LIVE_9c1e");
    }
}

// --- negative paths ---------------------------------------------------------------------------

#[test]
fn btime_present_on_one_side_only_is_not_compared_and_version_never_revokes() {
    let with = FsIdentity {
        dev: 1,
        ino: 2,
        btime: Some(3),
    };
    let without = FsIdentity {
        dev: 1,
        ino: 2,
        btime: None,
    };
    let other_btime = FsIdentity {
        dev: 1,
        ino: 2,
        btime: Some(4),
    };
    assert!(
        with.matches(&without),
        "a side without a birth time is not compared"
    );
    assert!(without.matches(&with));
    assert!(
        !with.matches(&other_btime),
        "both present and different: mismatch"
    );
    assert!(!with.matches(&FsIdentity {
        dev: 1,
        ino: 3,
        btime: Some(3)
    }));

    // a record written by another kaban-index version revalidates
    let s = Scratch::new("version");
    let home = s.path().join("home");
    let repo = s.path().join("repo");
    repo_with_commit(&repo, "one");
    let dir = admitted(&home, &repo);
    let (_, id) = live_identity(&repo);
    trust::trust(&dir.handle, &id, "0.0.1-other").unwrap();
    assert_eq!(trust::revalidate(&dir.handle, &id), Ok(()));
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path.join("trust")).unwrap()).unwrap();
    assert_eq!(record["kaban_index_version"], "0.0.1-other");
}
