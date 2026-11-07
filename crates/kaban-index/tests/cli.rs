//! CG4.6 (stage1-spec G1-F7 `index`, `gc`, `status`, `purge-derived`, the complete `doctor`,
//! the JSON report and the exit codes; G1-S5.8 revalidation before every spawn; G1-S8 fixed
//! classes; §7; stage1-plan CG4.6) through the `kaban-index` binary over scripted git
//! repositories under `/tmp` and fake indexers written per test (`test-integration-dir`,
//! `test-fixture-raii`). The binary is started through `/bin/bash` closing exactly the
//! descriptors this test process inherited without `FD_CLOEXEC` (nextest under `flock(1)`
//! leaks its lock fd), so the production descriptor policy (`Refuse`) holds in the child.
//! The tier is whatever `doctor` selects on this host (`namespaces` where unprivileged user
//! namespaces work, `none` with the warning otherwise); tests that need a fixed tier pass
//! `--isolation off`. `index_go_drivers_real` runs only when a `scip-go` is reachable and
//! prints a visible `NOT RUN` marker otherwise.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the visible `NOT RUN` markers and the evidence lines

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
use kaban_index::identity_cli;
use kaban_index::isolate::namespaces::SELFCHECK_FAILED_TEXT;
use kaban_index::writer::{self, Commit, Meta, Provenance, Rows};
use kaban_policy::Policy;
use protobuf::Message;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

mod common;

// ---------------------------------------------------------------------------------------------
// harness

/// A scratch directory deleted on drop, test failure included.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-cg46-{tag}-{}-{}",
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
        // a read-only data directory (the write-error slice) must be deletable again
        if let Ok(rd) = fs::read_dir(self.0.join("home").join("codegraph")) {
            for e in rd.flatten() {
                let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(0o700));
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `git <args>` in `cwd` with a fixed identity and `date` for both author and committer.
fn git_dated(cwd: &Path, args: &[&str], date: &str) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
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

fn git(cwd: &Path, args: &[&str]) -> String {
    git_dated(cwd, args, "2026-10-26T08:16:02Z")
}

/// A Go repository (`go.mod` marker) with one commit at `dir`; returns the canonical path.
fn go_repo(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("go.mod"), "module example.com/x\n\ngo 1.22\n").unwrap();
    fs::write(dir.join("x.go"), "package x\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    fs::canonicalize(dir).unwrap()
}

fn head(repo: &Path) -> String {
    git(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// One more commit changing `x.go`, dated `date`; returns its id.
fn commit_more(repo: &Path, n: u32, date: &str) -> String {
    fs::write(repo.join("x.go"), format!("package x\n// {n}\n")).unwrap();
    git_dated(repo, &["add", "-A"], date);
    git_dated(repo, &["commit", "-q", "-m", &format!("c{n}")], date);
    head(repo)
}

/// The numbers of the descriptors this process holds without `FD_CLOEXEC` (above 2): inherited
/// from an ancestor; closed in the shell before the binary starts (the CG1.1 clean-process
/// pattern), so the binary's `Refuse` policy sees a clean process.
fn non_cloexec_fds() -> Vec<u32> {
    kaban_core::process::inheritable_fds().expect("descriptor observation is required")
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
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // the caller's cgo setting is a test input (U2 RU2-12): unset unless a test sets it
        .env_remove("CGO_ENABLED");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("kaban-index runs")
}

fn kaban_index(home: &Path, args: &[&str]) -> Output {
    kaban_index_env(home, args, &[])
}

/// Like [`kaban_index_env`], bounded: the binary is killed and `None` returned when it has not
/// exited within `bound` (a parent blocked on a child's FIFO must be visible, not hang the
/// suite); `address_space_kb` caps the binary's address space (`ulimit -v`) so a parent that
/// reads a device without a bound aborts instead of exhausting the host. Streams go to files
/// under `dir`.
fn kaban_index_bounded(
    home: &Path,
    dir: &Path,
    args: &[&str],
    bound: Duration,
    address_space_kb: Option<u64>,
) -> Option<Output> {
    let closes = non_cloexec_fds().iter().fold(String::new(), |mut acc, n| {
        let _ = write!(acc, " {n}>&-");
        acc
    });
    let ulimit = address_space_kb.map_or(String::new(), |kb| format!("ulimit -v {kb}\n"));
    let script = format!("exec{closes}\n{ulimit}exec \"$0\" \"$@\"\n");
    let out_path = dir.join("bounded.stdout");
    let err_path = dir.join("bounded.stderr");
    let mut child = Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_kaban-index"))
        .args(args)
        .env("KABAN_HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(fs::File::create(&out_path).unwrap())
        .stderr(fs::File::create(&err_path).unwrap())
        .spawn()
        .expect("kaban-index starts");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > bound {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Some(Output {
        status,
        stdout: fs::read(&out_path).unwrap_or_default(),
        stderr: fs::read(&err_path).unwrap_or_default(),
    })
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
    let out = kaban_index(home, &["trust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "trust: {}", stderr(&out));
    json(&out)["repo_id"].as_str().unwrap().to_owned()
}

/// `<home>/codegraph/<repo-id>`.
fn repo_dir(home: &Path, repo_id: &str) -> PathBuf {
    home.join("codegraph").join(repo_id)
}

fn snapshot_path(home: &Path, repo_id: &str, commit: &str) -> PathBuf {
    repo_dir(home, repo_id).join(format!("{commit}.cg.db"))
}

/// The entry names of the repository's data directory, sorted.
fn names(home: &Path, repo_id: &str) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(repo_dir(home, repo_id))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Every file under `root` holding `needle`.
fn files_containing(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    fn walk(dir: &Path, needle: &[u8], out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, needle, out);
            } else if fs::read(&p)
                .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
            {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, needle, &mut out);
    out
}

/// `index --repo <repo> <args>`.
fn index(home: &Path, repo: &Path, args: &[&str]) -> Output {
    let mut all = vec!["index", "--repo", repo.to_str().unwrap()];
    all.extend_from_slice(args);
    kaban_index(home, &all)
}

#[test]
#[ignore = "requires explicit trusted qualified Rust producer and real toolchain"]
fn rust_actual_cli_publication_query() {
    use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
    let producer = PathBuf::from(
        std::env::var_os("KABAN_TEST_RUST_PRODUCER").expect("explicit trusted producer"),
    );
    let sysroot = PathBuf::from(
        std::env::var_os("KABAN_TEST_RUST_SYSROOT").expect("explicit trusted toolchain"),
    );
    for fixture in ["minimal", "workspace"] {
        let scratch = Scratch::new("rust-real-cli");
        let home = scratch.home();
        let repo = scratch.path().join("repo");
        common::copy_trusted_tree(
            &common::fixtures_dir()
                .join("rust-qualification")
                .join(fixture),
            &repo,
        );
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "fixture"]);
        let id = trust(&home, &repo);
        let override_arg = format!("rust={}", producer.display());
        let pinned_path = format!("{}:/usr/bin:/bin", sysroot.join("bin").display());
        let caller_cargo = scratch.path().join("caller-cargo");
        fs::create_dir_all(caller_cargo.join("registry")).unwrap();
        // A real initialized Cargo registry carries this tag. Cargo initializes it in an
        // empty directory even offline on the explicitly non-isolating tier used here.
        // Keep this input initialized, then require its entries, bytes and mtime unchanged.
        let registry_tag = caller_cargo.join("registry/CACHEDIR.TAG");
        let cache_tag = b"Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by cargo.\n\
# For information about cache directory tags see https://bford.info/cachedir/\n";
        fs::write(&registry_tag, cache_tag).unwrap();
        let tag_modified = fs::metadata(&registry_tag).unwrap().modified().unwrap();
        fs::write(
            caller_cargo.join("config.toml"),
            "unsupported caller config",
        )
        .unwrap();
        fs::write(
            caller_cargo.join("credentials.toml"),
            "invented caller credentials",
        )
        .unwrap();
        let wrapper_marker = scratch.path().join("wrapper-ran");
        let wrapper = fake_indexer(
            &caller_cargo,
            "wrapper",
            &format!("touch '{}'; exit 99", wrapper_marker.display()),
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
                "--isolation",
                "off",
            ],
            &[
                ("PATH", &pinned_path),
                ("CARGO_HOME", caller_cargo.to_str().unwrap()),
                ("RUSTC_WRAPPER", wrapper.to_str().unwrap()),
                ("CARGO_NET_OFFLINE", "false"),
                (
                    "CARGO_REGISTRIES_CRATES_IO_TOKEN",
                    "invented-must-not-reach-child",
                ),
            ],
        );
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        assert_eq!(json(&out)["languages"], serde_json::json!(["rust"]));
        assert_eq!(
            json(&out)["resources"],
            serde_json::json!(["registry", "toolchain"]),
            "the caller's registry and the pinned toolchain were the admitted resources"
        );
        let snapshot = SqliteSnapshot::open_ro(&snapshot_path(&home, &id, &head(&repo))).unwrap();
        let source = if fixture == "minimal" {
            "src/lib.rs"
        } else {
            "app/src/lib.rs"
        };
        assert_ne!(snapshot.nodes_in_file(source).unwrap(), []);
        assert!(!repo.join("target").exists());
        assert!(!wrapper_marker.exists());
        assert_eq!(
            fs::read_to_string(caller_cargo.join("credentials.toml")).unwrap(),
            "invented caller credentials"
        );
        assert_eq!(
            fs::read_dir(caller_cargo.join("registry"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [std::ffi::OsString::from("CACHEDIR.TAG")]
        );
        assert_eq!(fs::read(&registry_tag).unwrap(), cache_tag);
        assert_eq!(
            fs::metadata(&registry_tag).unwrap().modified().unwrap(),
            tag_modified
        );
        assert!(
            names(&home, &id)
                .iter()
                .all(|name| !name.starts_with("run-"))
        );
    }
}

/// The real-producer inputs of the RI-3 acceptance (`runner.rs`'s env names): the producer
/// and the real toolchain root, or `None` after a visible `NOT RUN` marker when either is
/// unset or not an executable.
fn rust_real_inputs() -> Option<(PathBuf, PathBuf)> {
    let executable = |var: &str, probe: &str| {
        let path = std::env::var_os(var).map(PathBuf::from)?;
        let target = if probe.is_empty() {
            path.clone()
        } else {
            path.join(probe)
        };
        let found =
            fs::metadata(target).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        if !found {
            eprintln!("NOT RUN: {var}={} is not an executable", path.display());
        }
        found.then_some(path)
    };
    let Some(producer) = executable("KABAN_TEST_RUST_PRODUCER", "") else {
        eprintln!("NOT RUN: set KABAN_TEST_RUST_PRODUCER to the installed kaban-rust-scip");
        return None;
    };
    let Some(sysroot) = executable("KABAN_TEST_RUST_SYSROOT", "bin/rustc") else {
        eprintln!("NOT RUN: set KABAN_TEST_RUST_SYSROOT to the real 1.99.0 toolchain root");
        return None;
    };
    Some((producer, sysroot))
}

/// A fresh one-commit git repository holding `fixtures/codegraph/rust-qualification/<fixture>`.
fn rust_fixture_repo(dir: &Path, fixture: &str) -> PathBuf {
    common::copy_trusted_tree(
        &common::fixtures_dir()
            .join("rust-qualification")
            .join(fixture),
        dir,
    );
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "fixture"]);
    fs::canonicalize(dir).unwrap()
}

/// The fixture oracle (`fixtures/codegraph/rust-qualification/README.md`) over a published
/// snapshot, every expectation derived from the fixture SOURCE (0-based lines, UTF-8 byte
/// columns, end-exclusive): exact reference endpoints with their enclosing symbol, the three
/// distinct `execute` symbols and the `IMPLEMENTS_MEMBER` edges between them, the Unicode-named
/// item's byte range, one document-local and one external reference fact, the trait's two
/// implementers and the tests of the production function (ri3-5).
fn assert_rust_oracle(snapshot: &kaban_codegraph::SqliteSnapshot, fixture: &str) {
    use kaban_codegraph::reader::TestsTarget;
    use kaban_codegraph::{CodeGraphReader, Depth, EdgeType, Limits, NodeKind, NodeRow, Range};
    type Site = (String, String, (u32, u32, u32, u32));
    let nodes_of = |path: &str| {
        let nodes = snapshot.nodes_in_file(path).unwrap();
        assert_ne!(nodes, [], "{fixture}: nodes in {path}");
        nodes
    };
    let one = |nodes: &[NodeRow], name: &str| {
        let found: Vec<&NodeRow> = nodes.iter().filter(|node| node.name == name).collect();
        assert_eq!(found.len(), 1, "{fixture}: definition {name}: {found:?}");
        found[0].clone()
    };
    // the definition named `name` whose definition range starts on `line`
    let at_line = |nodes: &[NodeRow], name: &str, line: u32| {
        nodes
            .iter()
            .find(|node| node.name == name && node.def.is_some_and(|def| def.sl == line))
            .unwrap_or_else(|| panic!("{fixture}: definition {name} on line {line}"))
            .clone()
    };
    let span = |sl: u32, sc: u32, ec: u32| Range { sl, sc, el: sl, ec };
    // every reference site of `node` as (enclosing ref, path, range)
    let sites = |node: &NodeRow, include_tests: bool| -> std::collections::BTreeSet<Site> {
        snapshot
            .references(node.id, include_tests)
            .unwrap()
            .iter()
            .flat_map(|group| {
                group.sites.iter().map(|site| {
                    let Range { sl, sc, el, ec } = site.site;
                    (
                        group.enclosing.r#ref.clone(),
                        site.path.clone(),
                        (sl, sc, el, ec),
                    )
                })
            })
            .collect()
    };
    let site = |enclosing: &NodeRow, path: &str, range: Range| -> Site {
        let Range { sl, sc, el, ec } = range;
        (enclosing.r#ref.clone(), path.to_owned(), (sl, sc, el, ec))
    };
    let references_edges = |node: &NodeRow| {
        snapshot
            .neighbours(
                node.id,
                Depth::new(1, &Limits::default()).unwrap(),
                &[EdgeType::References],
            )
            .unwrap()
    };
    if fixture == "minimal" {
        let lib = "src/lib.rs";
        let nodes = nodes_of(lib);
        for expected in [
            "DatabaseDriver",
            "Postgres",
            "Sqlite",
            "NotADriver",
            "new",
            "production",
            "middle",
            "leaf",
            "function_value",
            "café",
        ] {
            one(&nodes, expected);
        }

        // the trait method and each implementation method are three distinct symbols; the
        // implementation methods carry IMPLEMENTS_MEMBER to the trait method and nothing else
        let trait_execute = at_line(&nodes, "execute", 4);
        let postgres_execute = at_line(&nodes, "execute", 12);
        let sqlite_execute = at_line(&nodes, "execute", 18);
        let executes: std::collections::BTreeSet<&str> = nodes
            .iter()
            .filter(|node| node.name == "execute")
            .map(|node| node.r#ref.as_str())
            .collect();
        assert_eq!(
            executes,
            [&trait_execute, &postgres_execute, &sqlite_execute]
                .map(|node| node.r#ref.as_str())
                .into(),
            "{fixture}: execute symbols"
        );
        let implements_member = snapshot
            .neighbours(
                trait_execute.id,
                Depth::new(1, &Limits::default()).unwrap(),
                &[EdgeType::ImplementsMember],
            )
            .unwrap();
        let edges: std::collections::BTreeSet<(String, String)> = implements_member
            .edges
            .iter()
            .map(|edge| (edge.from.clone(), edge.to.clone()))
            .collect();
        assert_eq!(
            edges,
            [&postgres_execute, &sqlite_execute]
                .map(|node| (node.r#ref.clone(), trait_execute.r#ref.clone()))
                .into(),
            "{fixture}: IMPLEMENTS_MEMBER of the trait method"
        );
        let trait_node = one(&nodes, "DatabaseDriver");
        let trait_members = snapshot.members(trait_node.id, &[]).unwrap();
        assert_eq!(
            trait_members
                .iter()
                .map(|node| &node.r#ref)
                .collect::<Vec<_>>(),
            [&trait_execute.r#ref],
            "{fixture}: trait members"
        );
        let implementers: std::collections::BTreeSet<String> = snapshot
            .implementers(trait_node.id, false)
            .unwrap()
            .iter()
            .map(|node| node.name.clone())
            .collect();
        assert_eq!(
            implementers,
            ["Postgres", "Sqlite"].map(str::to_owned).into(),
            "{fixture}: implementers"
        );
        // `Postgres::new` is a production definition (asserted above); the producer does not
        // make inherent-impl methods CONTAINS children of the struct, so `members(Postgres)` is
        // not an oracle fact here (RI-3 hand-back: unconfirmed, not weakened).

        // exact reference endpoints: `production` -> `middle` -> `leaf`, `function_value` ->
        // `leaf` as a value, `Postgres::execute` -> `production`; the two test sites of
        // `production` (inline `tests::production_works`, `tests/integration.rs`) only with
        // `include_tests`
        let production = one(&nodes, "production");
        let middle = one(&nodes, "middle");
        let function_value = one(&nodes, "function_value");
        assert_eq!(
            sites(&middle, false),
            [site(&production, lib, span(30, 4, 10))].into(),
            "{fixture}: references of middle"
        );
        assert_eq!(
            sites(&one(&nodes, "leaf"), false),
            [
                site(&middle, lib, span(34, 4, 8)),
                site(&function_value, lib, span(42, 4, 8)),
            ]
            .into(),
            "{fixture}: references of leaf"
        );
        let production_site = site(&postgres_execute, lib, span(13, 8, 18));
        assert_eq!(
            sites(&production, false),
            [production_site.clone()].into(),
            "{fixture}: production references without tests"
        );
        let production_works = one(&nodes, "production_works");
        let integration_works = one(&nodes_of("tests/integration.rs"), "integration_works");
        assert_eq!(
            sites(&production, true),
            [
                production_site,
                site(&production_works, lib, span(54, 26, 36)),
                site(&integration_works, "tests/integration.rs", span(2, 43, 53)),
            ]
            .into(),
            "{fixture}: production references with tests"
        );
        let tests: std::collections::BTreeSet<String> = snapshot
            .tests_for(TestsTarget::Symbol(production.id))
            .unwrap()
            .iter()
            .map(|link| link.test.r#ref.clone())
            .collect();
        assert_eq!(
            tests,
            [
                production_works.r#ref.clone(),
                integration_works.r#ref.clone()
            ]
            .into(),
            "{fixture}: tests_for production"
        );
        assert!(production_works.is_test && integration_works.is_test);
        assert!(!production.is_test && !postgres_execute.is_test);

        // the Unicode-named item's byte range, its document-local `local` and the external
        // `std::mem::size_of` it references
        let cafe = one(&nodes, "café");
        assert_eq!(cafe.def, Some(span(45, 7, 12)), "{fixture}: café range");
        let from_cafe = references_edges(&cafe);
        let referenced: std::collections::BTreeSet<&str> = from_cafe
            .edges
            .iter()
            .filter(|edge| edge.from == cafe.r#ref)
            .map(|edge| edge.to.as_str())
            .collect();
        let local = from_cafe
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::Local)
            .unwrap_or_else(|| panic!("{fixture}: local of café: {from_cafe:?}"));
        let size_of = from_cafe
            .nodes
            .iter()
            .find(|node| node.name == "size_of")
            .unwrap_or_else(|| panic!("{fixture}: size_of from café: {from_cafe:?}"));
        assert!(
            size_of.external,
            "{fixture}: size_of is external: {size_of:?}"
        );
        assert!(
            referenced.contains(local.r#ref.as_str())
                && referenced.contains(size_of.r#ref.as_str()),
            "{fixture}: REFERENCES from café: {referenced:?}"
        );
        assert_eq!(local.def, Some(span(46, 8, 13)), "{fixture}: local range");
        assert_eq!(
            sites(local, false),
            [site(&cafe, lib, span(47, 4, 9))].into(),
            "{fixture}: references of local"
        );
        assert_eq!(
            sites(size_of, false),
            [site(&cafe, lib, span(47, 28, 35))].into(),
            "{fixture}: references of size_of"
        );
    } else {
        // `answer` references `qualification_support::base_value` and the macro-generated
        // `generated_value`, each exactly once, on its one body line
        let app_lib = "app/src/lib.rs";
        let app = nodes_of(app_lib);
        let answer = one(&app, "answer");
        let generated_value = one(&app, "generated_value");
        assert_eq!(
            sites(&generated_value, false),
            [site(&answer, app_lib, span(5, 42, 57))].into(),
            "{fixture}: references of generated_value"
        );
        let support = nodes_of("support/src/lib.rs");
        assert_eq!(
            sites(&one(&support, "base_value"), false),
            [site(&answer, app_lib, span(5, 27, 37))].into(),
            "{fixture}: references of base_value"
        );
    }
}

/// RI-3 Task 1, one cell of the acceptance matrix: `trust`, `doctor` and `index` through the
/// `kaban-index` binary with the installed producer and the real toolchain, on the host's
/// selected tier or with `index.isolation: off`, then the fixture oracle over the snapshot.
fn rust_real_acceptance(producer: &Path, sysroot: &Path, fixture: &str, isolation_off: bool) {
    let override_arg = format!("rust={}", producer.display());
    let pinned_path = format!("{}:/usr/bin:/bin", sysroot.join("bin").display());
    let env = [("PATH", pinned_path.as_str())];
    let s = Scratch::new(&format!("rust-accept-{fixture}"));
    let home = s.home();
    if isolation_off {
        fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    }
    let repo = rust_fixture_repo(&s.path().join("repo"), fixture);
    let repo_s = repo.to_str().unwrap();
    let id = trust(&home, &repo);

    let started = Instant::now();
    let out = kaban_index_env(
        &home,
        &["doctor", "--repo", repo_s, "--indexer", &override_arg],
        &env,
    );
    let doctor_wall = started.elapsed();
    assert_eq!(code(&out), 0, "{fixture}: doctor: {}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("language: rust (markers: Cargo.toml) indexer: found"),
        "{fixture}: {err}"
    );
    assert!(
        err.contains("indexer rust version: kaban-rust-scip "),
        "{fixture}: {err}"
    );
    let report = json(&out);
    let rust_check = report["doctor"]["indexers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["lang"] == "rust")
        .unwrap_or_else(|| panic!("{fixture}: rust indexer check: {report}"))
        .clone();
    assert!(
        rust_check["version"]["Ok"]
            .as_str()
            .is_some_and(|v| v.starts_with("kaban-rust-scip ")),
        "{fixture}: no refusal class: {rust_check}"
    );
    let selected = report["doctor"]["isolation"]["selected"]["Ok"]["tier"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo_s,
            "--lang",
            "rust",
            "--indexer",
            &override_arg,
        ],
        &env,
    );
    assert_eq!(code(&out), 0, "{fixture}: index: {}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["languages"], serde_json::json!(["rust"]));
    let tier = if isolation_off {
        "none"
    } else {
        selected.as_str()
    };
    assert_eq!(report["isolation"], tier, "{fixture}: {report}");
    let path = snapshot_path(&home, &id, &head(&repo));
    assert!(path.is_file(), "{fixture}: {}", path.display());
    let bytes = fs::metadata(&path).unwrap().len();
    assert_eq!(report["snapshot_bytes"], bytes);
    eprintln!(
        "rust acceptance {fixture} tier {tier}: doctor {} ms, index {} ms, snapshot {bytes} bytes, peak rss {}",
        doctor_wall.as_millis(),
        report["wall_ms_per_indexer"]["rust"],
        report["peak_rss_bytes_per_indexer"]["rust"]
    );
    let snapshot = kaban_codegraph::SqliteSnapshot::open_ro(&path).unwrap();
    assert_rust_oracle(&snapshot, fixture);
    assert!(
        !repo.join("target").exists(),
        "build outputs stay in scratch"
    );
}

/// RI-3 Task 1: the green surface of the acceptance matrix on this host: `minimal` on the
/// selected tier and with isolation off, `workspace` (build script and proc macro) with
/// isolation off. The negative through the same entry point (a producer whose capabilities
/// list no host refuses with `rust_capabilities_refused`) is
/// `rust_cli_refuses_unqualified_producer_and_repository_config`.
#[test]
#[ignore = "requires the installed Rust producer and real toolchain"]
fn rust_real_producer_cli_acceptance() {
    let Some((producer, sysroot)) = rust_real_inputs() else {
        return;
    };
    for (fixture, isolation_off) in [("minimal", false), ("minimal", true), ("workspace", true)] {
        rust_real_acceptance(&producer, &sysroot, fixture, isolation_off);
    }
}

/// RI-3 Task 1's open cell, closed by RI-3d: `workspace` on the host's selected tier. Under
/// `namespaces` the view holds the checkout, the pinned toolchain, the scratch and the admitted
/// linker's helper directory; the producer (contract v2) tells cargo the admitted canonical
/// linker, so the build script and the proc macro compile without a `cc` on `PATH`.
#[test]
#[ignore = "requires the installed Rust producer and real toolchain"]
fn rust_real_producer_workspace_on_selected_tier() {
    let Some((producer, sysroot)) = rust_real_inputs() else {
        return;
    };
    rust_real_acceptance(&producer, &sysroot, "workspace", false);
}

/// An unqualified producer refuses before anything runs; a committed `.cargo/config.toml` does
/// not (DF-2: kaban's environment beats the file in cargo's precedence), so the class stays the
/// producer's.
#[test]
fn rust_cli_refuses_unqualified_producer_not_repository_config() {
    let s = Scratch::new("rust-admission");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    fs::remove_file(repo.join("go.mod")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "Cargo root"]);
    let id = trust(&home, &repo);
    let producer = fake_indexer(
        &fakes(&s),
        "kaban-rust-scip",
        r#"
if [ "$1" = "capabilities" ]; then
  echo '{"protocol_version":2,"semantic_profile":"kaban-rust-v1","producer_version":"0.1.0-dev","ra_ap_version":"0.0.358","supported_host_triples":[]}'
  exit 0
fi
echo repository-execution-must-not-start >&2
exit 99
"#,
    );
    let override_arg = format!("rust={}", producer.display());
    let out = index(
        &home,
        &repo,
        &[
            "--lang",
            "rust",
            "--indexer",
            &override_arg,
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "rust_capabilities_refused");
    assert!(!snapshot_path(&home, &id, &head(&repo)).exists());
    fs::create_dir(repo.join(".cargo")).unwrap();
    fs::write(
        repo.join(".cargo/config.toml"),
        "[build]\nrustc-wrapper = \"./malicious\"\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "unsupported config"]);
    let out = index(
        &home,
        &repo,
        &["--indexer", &override_arg, "--isolation", "off"],
    );
    assert_eq!(json(&out)["class"], "rust_capabilities_refused");
    assert!(!snapshot_path(&home, &id, &head(&repo)).exists());
}

fn fake_rust_chain(root: &Path, body: &str) -> PathBuf {
    for file in [
        "lib/rustlib/src/rust/library/core/src/lib.rs",
        "lib/rustlib/src/rust/library/std/src/lib.rs",
        "lib/rustlib/components",
    ] {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }
    fake_indexer(&root.join("bin"), "cargo", "exit 0");
    fake_indexer(&root.join("bin"), "rustdoc", "exit 0");
    fake_indexer(
        &root.join("libexec"),
        "rust-analyzer-proc-macro-srv",
        "exit 0",
    );
    fake_indexer(&root.join("bin"), "rustc", body)
}

fn qualified_rust_probe(dir: &Path) -> PathBuf {
    fake_indexer(
        dir,
        "kaban-rust-scip",
        &format!(
            "if [ \"$1\" = --version ]; then echo qualified; else echo '{{\"protocol_version\":2,\"semantic_profile\":\"kaban-rust-v1\",\"producer_version\":\"0.1.0-dev\",\"ra_ap_version\":\"0.0.358\",\"supported_host_triples\":[\"{}\"]}}'; fi",
            kaban_index::doctor::rust_host()
        ),
    )
}

#[test]
fn doctor_rust_refuses_checkout_compiler_through_relative_and_symlink_aliases() {
    let s = Scratch::new("doctor-rust-alias");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    let repo = go_repo(&s.path().join("repo"));
    fs::remove_file(repo.join("go.mod")).unwrap();
    fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
    let marker = s.path().join("compiler-ran");
    fake_rust_chain(
        &repo.join("chain"),
        &format!("touch '{}'; exit 99", marker.display()),
    );
    let producer = qualified_rust_probe(&fakes(&s));
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "rust tools"]);
    let link = s.path().join("alias");
    std::os::unix::fs::symlink(&repo, &link).unwrap();
    let cwd = std::env::current_dir().unwrap();
    let mut relative = PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(repo.strip_prefix("/").unwrap());
    let path = format!("{}:/usr/bin:/bin", repo.join("chain/bin").display());
    for alias in [relative, link] {
        let out = kaban_index_env(
            &home,
            &[
                "doctor",
                "--repo",
                alias.to_str().unwrap(),
                "--indexer",
                &format!("rust={}", producer.display()),
            ],
            &[("PATH", &path)],
        );
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        assert_eq!(
            json(&out)["doctor"]["indexers"][0]["version"]["Err"],
            "rust_toolchain_refused"
        );
        assert!(!marker.exists(), "checkout compiler must never execute");
    }
}

/// ri12-1: the forbidden root of detection is the ENCLOSING checkout, not the given directory:
/// `--repo <checkout>/member` (a nested `Cargo.toml`), through a symlink alias too, never
/// admits `<checkout>/bin/kaban-rust-scip` from `PATH`, and the fake producer never runs. The
/// checkout root itself keeps today's behaviour (a producer outside the checkout is found).
#[test]
fn doctor_nested_member_never_admits_a_checkout_producer() {
    let s = Scratch::new("doctor-nested");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    let repo = go_repo(&s.path().join("repo"));
    fs::remove_file(repo.join("go.mod")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = ['member']\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("member")).unwrap();
    fs::write(
        repo.join("member/Cargo.toml"),
        "[package]\nname = 'member'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let marker = s.path().join("producer-ran");
    fake_indexer(
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
    let link = s.path().join("alias");
    std::os::unix::fs::symlink(repo.join("member"), &link).unwrap();
    let checkout_bin = format!("{}:/usr/bin:/bin", repo.join("bin").display());
    for nested in [repo.join("member"), link] {
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
            "{}: {}",
            nested.display(),
            stderr(&out)
        );
        assert!(!marker.exists(), "checkout producer must never execute");
    }
    qualified_rust_probe(&fakes(&s));
    let outside = format!("{}:/usr/bin:/bin", fakes(&s).display());
    let out = kaban_index_env(
        &home,
        &["doctor", "--repo", repo.to_str().unwrap()],
        &[("PATH", &outside)],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("language: rust (markers: Cargo.toml) indexer: found"),
        "{}",
        stderr(&out)
    );
    assert!(!marker.exists());
}

#[test]
fn doctor_failed_rust_producer_never_probes_compiler() {
    let s = Scratch::new("doctor-rust-failed");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  isolation: off\n").unwrap();
    let repo = go_repo(&s.path().join("repo"));
    fs::remove_file(repo.join("go.mod")).unwrap();
    fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "rust"]);
    let marker = s.path().join("compiler-ran");
    let root = s.path().join("chain");
    fake_rust_chain(&root, &format!("touch '{}'; exit 99", marker.display()));
    let producer = fake_indexer(&fakes(&s), "kaban-rust-scip", "exit 99");
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &format!("rust={}", producer.display()),
        ],
        &[(
            "PATH",
            &format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        json(&out)["doctor"]["indexers"][0]["version"]["Err"],
        "indexer_failed"
    );
    assert!(!marker.exists());

    let producer = fake_indexer(
        &fakes(&s),
        "kaban-rust-scip",
        "if [ \"$1\" = --version ]; then echo qualified; else echo '{}'; fi",
    );
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &format!("rust={}", producer.display()),
        ],
        &[(
            "PATH",
            &format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )],
    );
    assert_eq!(
        json(&out)["doctor"]["indexers"][0]["version"]["Err"],
        "rust_capabilities_refused"
    );
    assert!(!marker.exists());

    // A symlinked caller HOME still forbids binding the whole home as a sysroot.
    let home_alias = s.path().join("caller-home-alias");
    std::os::unix::fs::symlink(&root, &home_alias).unwrap();
    let producer = qualified_rust_probe(&fakes(&s));
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &format!("rust={}", producer.display()),
        ],
        &[
            (
                "PATH",
                &format!("{}:/usr/bin:/bin", root.join("bin").display()),
            ),
            ("HOME", home_alias.to_str().unwrap()),
        ],
    );
    assert_eq!(
        json(&out)["doctor"]["indexers"][0]["version"]["Err"],
        "rust_toolchain_refused"
    );
    assert!(!marker.exists());
}

#[test]
fn rust_required_unavailable_never_runs_native_producer_probes() {
    let s = Scratch::new("rust-required-probes");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    if doctor_selected_tier(&home, &repo) != "none" {
        eprintln!("NOT RUN: required-unavailable Rust probes: isolating tier available");
        return;
    }
    fs::remove_file(repo.join("go.mod")).unwrap();
    fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "rust"]);
    trust(&home, &repo);
    let marker = s.path().join("producer-ran");
    let producer = fake_indexer(
        &fakes(&s),
        "kaban-rust-scip",
        &format!("touch '{}'; exit 99", marker.display()),
    );
    let override_arg = format!("rust={}", producer.display());
    fs::write(home.join("config.yaml"), "index:\n  isolation: required\n").unwrap();
    let out = kaban_index(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &override_arg,
        ],
    );
    assert_eq!(
        json(&out)["doctor"]["indexers"][0]["version"]["Err"],
        "isolation_unavailable"
    );
    assert!(!marker.exists());
    let out = index(&home, &repo, &["--indexer", &override_arg]);
    assert_eq!(code(&out), 8, "{}", stderr(&out));
    assert!(!marker.exists());
}

#[test]
fn rust_probes_refuse_unavailable_isolation_and_use_namespace_resources() {
    use kaban_index::detect::{self, Lang};
    use kaban_index::doctor::{self, ProbeSite};
    use kaban_index::tools;
    let s = Scratch::new("rust-probe-site");
    let marker = s.path().join("native-ran");
    let root = s.path().join("chain");
    let compiler = fake_rust_chain(
        &root,
        &format!(
            "[ \"$$\" = 1 ] || {{ touch '{}'; exit 99; }}\n[ -f '{}' ] || exit 98\necho 'release: 1.99.0'\necho 'host: {}'",
            marker.display(),
            root.join("lib/rustlib/components").display(),
            doctor::rust_host()
        ),
    );
    let compiler = tools::admit_executable(&compiler, &[]).unwrap();
    let producer = qualified_rust_probe(&fakes(&s));
    let producer = detect::resolve_indexer(Lang::Rust, Some(&producer), None, &[]).unwrap();
    assert_eq!(
        doctor::rust_capabilities(&producer, ProbeSite::Unavailable),
        Err("isolation_unavailable")
    );
    assert_eq!(
        doctor::rust_toolchain(&compiler, ProbeSite::Unavailable),
        Err("isolation_unavailable")
    );
    assert!(!marker.exists());
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    if doctor_selected_tier(&home, &repo) != "namespaces" {
        eprintln!("NOT RUN: Rust namespace probe resources: namespaces unavailable");
        return;
    }
    // The namespaced half runs through the binary (a clean process: the in-process probe
    // would be refused by the production descriptor policy, this test process holding the
    // runner's inherited descriptors): `doctor` probes the producer and the compiler inside
    // the view with the toolchain root bound as a resource, so the compiler sees its
    // `components` file and runs as pid 1 of the view.
    fs::remove_file(repo.join("go.mod")).unwrap();
    fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "rust"]);
    let override_arg = format!("rust={}", producer.path.display());
    let path_env = std::env::join_paths([
        root.join("bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &override_arg,
        ],
        &[("PATH", path_env.to_str().unwrap())],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(
        report["doctor"]["isolation"]["selected"]["Ok"]["tier"],
        "namespaces"
    );
    assert_eq!(
        report["doctor"]["languages"]["Ok"][0]["indexer"],
        "found",
        "{}",
        stderr(&out)
    );
    assert_eq!(
        report["doctor"]["indexers"][0]["version"],
        serde_json::json!({"Ok": "qualified"}),
        "{}",
        stderr(&out)
    );
    assert!(!marker.exists());
}

/// A policy file rejecting `CANARY_*` (the §8.1 canary rule), builtin rules off.
fn canary_policy_file(dir: &Path) -> PathBuf {
    let path = dir.join("policy.yaml");
    fs::write(
        &path,
        "version: 1\ndefaults: { action: reject }\nextend: { use_builtin: false }\nrules:\n  - { id: canary-cred, class: credential, regex: 'CANARY_[A-Za-z0-9_]+' }\n",
    )
    .unwrap();
    path
}

// Invented SCIP through the existing fake Go runner exercises source admission, not
// Rust tool admission or producer qualification.
#[test]
fn rust_committed_source_policy_hit_skips_the_file_and_counts() {
    let s = Scratch::new("rust-source-policy");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  policy_scan: true\n").unwrap();
    let repo = go_repo(&s.path().join("repo"));
    fs::write(
        repo.join("lib.rs"),
        "// CANARY_SOURCE\n#[test] fn check() {}\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "rust source"]);
    let repo_id = trust(&home, &repo);
    let source = fakes(&s).join("rust.scip");
    let symbol = "kaban-rust-scip cargo fixture 0.1.0 lib/check().";
    let mut document = common::document(
        "lib.rs",
        vec![common::sym_info_kind(
            symbol,
            scip::types::symbol_information::Kind::Function,
        )],
        vec![common::definition_enclosing(
            symbol,
            &[1, 11, 16],
            &[1, 0, 22],
        )],
    );
    document.language = "rust".into();
    let mut input = common::index(vec![document]);
    input.metadata.as_mut().unwrap().project_root = format!("file://{}", repo.display());
    fs::write(&source, common::bytes(&input)).unwrap();
    let fake = fake_indexer(
        &fakes(&s),
        "scip-go",
        &format!(
            "if [ \"$1\" = \"--version\" ]; then echo v1; exit 0; fi\n{OUT_ARG}\ncp '{}' \"$out\"",
            source.display()
        ),
    );
    let policy = canary_policy_file(s.path());
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&fake),
            "--isolation",
            "off",
        ],
    );
    // DF-3: the file is classified by its path only, counted and reported; never a refusal
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["policy_scan"], true);
    assert_eq!(report["counts"]["policy_skipped_source"], 1);
    // keyed by the indexer that ran (the fake Go runner delivers this Rust document)
    assert_eq!(
        report["policy_skipped_source_per_indexer"]["go"],
        serde_json::json!(["canary-cred"])
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("policy_skipped_source")),
        "{report}"
    );
    let snapshot = snapshot_path(&home, &repo_id, &head(&repo));
    assert!(snapshot.exists(), "published");
    assert_eq!(meta_of(&snapshot)["counts:policy_skipped_source"], "1");
    assert_no_canary(&out, "committed Rust source");
}

/// A committed `src/` file over the classifier's bound is classified by its path only (DF-3),
/// yet the producer's positive test facts stand (df23-5): a definition carrying the SCIP `Test`
/// role is `is_test`, and an untagged reference inside such a symbol, or to such a symbol, is
/// a test reference that `include_tests:false` excludes.
#[test]
fn rust_over_bound_source_keeps_scip_test_roles_and_excludes_test_references() {
    let s = Scratch::new("rust-over-bound-roles");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        format!(
            "// {}\npub fn prod() {{}}\n\n#[cfg(test)]\nmod tests {{\n    fn helper() {{\n        \
             super::prod();\n    }}\n\n    #[test]\n    fn t() {{\n        helper();\n    }}\n}}\n",
            "(".repeat(16_385)
        ),
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "rust source over the bound"]);
    let repo_id = trust(&home, &repo);
    let source = fakes(&s).join("rust.scip");
    let prod = "kaban-rust-scip cargo fixture 0.1.0 lib/prod().";
    let helper = "kaban-rust-scip cargo fixture 0.1.0 lib/tests/helper().";
    let t = "kaban-rust-scip cargo fixture 0.1.0 lib/tests/t().";
    let kind = scip::types::symbol_information::Kind::Function;
    let test_role = 32;
    let mut helper_def = common::definition_enclosing(helper, &[5, 7, 13], &[5, 4, 7, 5]);
    helper_def.symbol_roles |= test_role;
    // `t` has the Test role but no enclosing range: the site inside it falls to the file node
    let mut t_def = common::definition(t, &[10, 7, 8]);
    t_def.symbol_roles |= test_role;
    let mut document = common::document(
        "src/lib.rs",
        vec![
            common::sym_info_kind(prod, kind),
            common::sym_info_kind(helper, kind),
            common::sym_info_kind(t, kind),
        ],
        vec![
            common::definition_enclosing(prod, &[1, 7, 11], &[1, 0, 16]),
            helper_def,
            common::occurrence(prod, &[6, 15, 19]),
            t_def,
            common::occurrence(helper, &[11, 8, 14]),
        ],
    );
    document.language = "rust".into();
    let mut input = common::index(vec![document]);
    input.metadata.as_mut().unwrap().project_root = format!("file://{}", repo.display());
    fs::write(&source, common::bytes(&input)).unwrap();
    let fake = fake_indexer(
        &fakes(&s),
        "scip-go",
        &format!(
            "if [ \"$1\" = \"--version\" ]; then echo v1; exit 0; fi\n{OUT_ARG}\ncp '{}' \"$out\"",
            source.display()
        ),
    );
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&fake), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["counts"]["source_classified_by_path"], 1, "{report}");
    let snap = SqliteSnapshot::open_ro(&snapshot_path(&home, &repo_id, &head(&repo))).unwrap();
    let node = |sym: &str| snap.node(&format!("scip:{sym}")).unwrap().expect(sym);
    let (prod, helper, t) = (node(prod), node(helper), node(t));
    assert!(!prod.is_test, "a production symbol under src/");
    assert!(
        helper.is_test,
        "the SCIP Test role on the definition stands"
    );
    assert!(t.is_test, "the SCIP Test role on the definition stands");
    let sites = |id, include_tests| -> usize {
        snap.references(id, include_tests)
            .unwrap()
            .iter()
            .map(|g| g.sites.len())
            .sum()
    };
    assert_eq!(sites(prod.id, true), 1, "the call inside helper");
    assert_eq!(
        sites(prod.id, false),
        0,
        "a reference inside a test symbol is a test reference"
    );
    assert_eq!(sites(helper.id, true), 1, "the call inside t");
    assert_eq!(
        sites(helper.id, false),
        0,
        "a reference to a test symbol is a test reference"
    );
}

/// The project root is validated as an identity before any redaction (df23-6): with scanning
/// on and a rule matching `file://`, a valid root is still recognised and the run publishes;
/// the masked text is counted under `policy_redacted_meta`, never turned into a refusal.
#[test]
fn project_root_hit_is_masked_and_counted_but_the_valid_root_is_not_refused() {
    let s = Scratch::new("project-root-policy-hit");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  policy_scan: true\n").unwrap();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let policy = s.path().join("policy.yaml");
    fs::write(
        &policy,
        "version: 1\ndefaults: { action: reject }\nextend: { use_builtin: false }\nrules:\n  - { id: uri-scheme, class: credential, regex: 'file://' }\n",
    )
    .unwrap();
    let scip = fakes(&s).join("rooted.scip");
    fs::write(
        &scip,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    let fake = copying_indexer_named(&fakes(&s), "scip-go", &scip);
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&fake),
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["policy_scan"], true);
    // Go runs twice (A and B): the root text is masked in both runs' metadata
    assert_eq!(report["counts"]["policy_redacted_meta"], 2, "{report}");
    assert_eq!(
        report["policy_redacted_meta_per_indexer"]["go"],
        serde_json::json!(["uri-scheme"])
    );
    assert!(
        snapshot_path(&home, &repo_id, &head(&repo)).exists(),
        "published"
    );
    // a root naming another repository is still refused, redaction or not
    fs::write(&scip, scip_bytes("file:///somewhere/else", "scip-go", None)).unwrap();
    let other = go_repo(&s.path().join("other"));
    trust(&home, &other);
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "index",
            "--repo",
            other.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&fake),
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    assert_eq!(
        json(&out)["reason"],
        "project_root_mismatch",
        "{}",
        stderr(&out)
    );
}

/// `index.policy_scan` is off by default (DF-3, owner decision): the snapshot stays on this
/// machine, so the content policy does not scan the code-graph output at all. A tool name that
/// would hit the canary rule is published untouched, nothing is counted, and the report says
/// `policy_scan: false`.
#[test]
fn policy_scan_off_by_default_publishes_a_meta_hit_untouched() {
    let s = Scratch::new("policy-scan-off");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let policy = canary_policy_file(s.path());
    let fakes_dir = fakes(&s);
    let canary_tool = fakes_dir.join("canary-tool.scip");
    fs::write(
        &canary_tool,
        scip_bytes(&format!("file://{}", repo.display()), "CANARY_TOOL", None),
    )
    .unwrap();
    let indexer = copying_indexer_named(&fakes_dir, "scip-go-canary", &canary_tool);
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["policy_scan"], false);
    assert!(report.get("policy_redacted_meta_per_indexer").is_none());
    let snapshot = snapshot_path(&home, &repo_id, &head(&repo));
    assert!(snapshot.exists(), "published");
    let meta = meta_of(&snapshot);
    assert!(
        meta["indexer:go"].starts_with("CANARY_TOOL "),
        "untouched: {}",
        meta["indexer:go"]
    );
    assert_eq!(meta["policy_version"], "off");
    assert_eq!(meta["counts:policy_redacted_meta"], "0", "nothing counted");
}

// ---------------------------------------------------------------------------------------------
// fake indexers

/// A small SCIP index naming `project_root` (what `scip-go` writes), one document with a
/// struct and a method, `tool_name` in `tool_info`, and an optional second document whose
/// documentation holds `extra_doc`.
fn scip_bytes(project_root: &str, tool_name: &str, extra_doc: Option<&str>) -> Vec<u8> {
    use scip::types::symbol_information::Kind;
    let v = "730da842221a844e8c2c82891dda9094f6428858";
    let s = common::go_sym(v, "Store#");
    let m = common::go_sym(v, "Store#Open().");
    let mut documents = vec![common::document(
        "x.go",
        vec![
            common::sym_info_kind(&s, Kind::Struct),
            common::sym_info_kind(&m, Kind::Method),
        ],
        vec![
            common::definition(&s, &[1, 5, 10]),
            common::definition_enclosing(&m, &[3, 20, 24], &[3, 0, 5, 1]),
        ],
    )];
    if let Some(doc) = extra_doc {
        let mut info = common::sym_info_kind(&s, Kind::Struct);
        info.documentation = vec![doc.to_owned()];
        documents.push(common::document(
            "y.go",
            vec![info],
            vec![common::occurrence(&s, &[2, 1, 6])],
        ));
    }
    let tool_info = scip::types::ToolInfo {
        name: tool_name.to_owned(),
        version: "0".to_owned(),
        arguments: vec!["index".to_owned()],
        ..Default::default()
    };
    let metadata = scip::types::Metadata {
        project_root: project_root.to_owned(),
        tool_info: protobuf::MessageField::some(tool_info),
        ..Default::default()
    };
    scip::types::Index {
        metadata: protobuf::MessageField::some(metadata),
        documents,
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap()
}

/// A fake indexer executable `dir/name` (0755) with `body` after the `#!/bin/sh` line.
fn fake_indexer(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Shell prelude: `out` = the value after `--output`, `skip` = 1 when `--skip-tests` is present.
const OUT_ARG: &str = r#"out=""; skip=0; prev=""
for a in "$@"; do
  if [ "$prev" = "--output" ]; then out="$a"; fi
  if [ "$a" = "--skip-tests" ]; then skip=1; fi
  prev="$a"
done"#;

/// The fakes directory of a scratch: the indexer scripts and the index bytes they copy live
/// together (one read-only bind inside the namespaces view).
fn fakes(s: &Scratch) -> PathBuf {
    let d = s.path().join("fakes");
    fs::create_dir_all(&d).unwrap();
    d
}

/// A fake `scip-go` under `dir/<name>` that copies `src` to `--output` (both runs) and exits 0.
fn copying_indexer_named(dir: &Path, name: &str, src: &Path) -> PathBuf {
    fake_indexer(
        dir,
        name,
        &format!("{OUT_ARG}\ncp '{}' \"$out\"\nexit 0", src.display()),
    )
}

/// A copying fake whose index names `repo` as its `project_root`.
fn good_indexer(s: &Scratch, repo: &Path) -> PathBuf {
    let dir = fakes(s);
    let src = dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    copying_indexer_named(&dir, "scip-go", &src)
}

/// A copying fake that stays alive 300 ms after the copy, so the RSS monitor (50 ms interval)
/// samples it the way it samples a real indexer.
fn lingering_indexer(s: &Scratch, repo: &Path) -> PathBuf {
    let dir = fakes(s);
    let src = dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    fake_indexer(
        &dir,
        "scip-go-linger",
        &format!(
            "{OUT_ARG}\ncp '{}' \"$out\"\nsleep 0.3\nexit 0",
            src.display()
        ),
    )
}

fn indexer_arg(path: &Path) -> String {
    format!("go={}", path.display())
}

/// Publish a snapshot straight into the admitted directory through the library (the writer
/// tests' pipeline) with the given provenance: the `gc` and `status` fixtures.
fn publish_with(
    home: &Path,
    repo_id: &str,
    commit: &str,
    provenance: Provenance,
    policy_version: &str,
) {
    let policy = Policy::empty();
    let bytes = common::fixture_bytes("synthetic/typed-only.scip");
    let gated = common::gated(&bytes, &policy);
    let admitted = kaban_index::admit_index(&gated);
    let rows = Rows::derive(&admitted, None, &policy);
    let meta = Meta {
        commit: Commit::parse(commit).unwrap(),
        repo_id: repo_id.to_owned(),
        policy_version: policy_version.to_owned(),
        provenance,
    };
    let dir = identity_cli::admit_repo_dir(home, repo_id).unwrap();
    writer::publish_admitted(&dir.handle, &rows, &meta, &policy, writer::dir_fsync).unwrap();
}

fn provenance(tier: kaban_codegraph::IsolationTier, detail: &str) -> Provenance {
    Provenance {
        isolation: tier,
        isolation_detail: detail.to_owned(),
        resources: vec!["modcache"],
        container_image_digest: None,
    }
}

/// The `meta` table of a snapshot.
fn meta_of(path: &Path) -> BTreeMap<String, String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn.prepare("SELECT key, value FROM meta").unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The `doctor` selection line's tier word (`tier selected: <tier> (...)`).
fn doctor_selected_tier(home: &Path, repo: &Path) -> String {
    let out = kaban_index(home, &["doctor", "--repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    json(&out)["doctor"]["isolation"]["selected"]["Ok"]["tier"]
        .as_str()
        .unwrap()
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// slice 1: the flag the spec forbids is unknown; `index` without a mark is refused (exit 4,
// `trust_refused`), nothing written, no indexer started

#[test]
fn cli_no_keep_run_dir_flag() {
    let s = Scratch::new("keep-run-dir");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let out = index(&home, &repo, &["--keep-run-dir"]);
    // U2 RU2-8: the diagnostic is fixed text naming the kind of error; the argument itself is
    // caller input and is never echoed
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("usage error (UnknownArgument); see kaban-index --help"),
        "{err}"
    );
    assert!(
        !err.contains("--keep-run-dir"),
        "the flag is not echoed: {err}"
    );
    assert_eq!(json(&out)["class"], "usage_error");
    assert_eq!(json(&out)["exit"], 2);
}

#[test]
fn cli_refuses_without_trust_mark() {
    let s = Scratch::new("no-mark");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let canary = s.path().join("ran");
    let indexer = fake_indexer(
        &fakes(&s),
        "scip-go",
        &format!("touch '{}'\nexit 0", canary.display()),
    );
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&indexer)]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["verb"], "index");
    assert_eq!(report["ok"], false);
    assert_eq!(report["class"], "trust_refused");
    assert_eq!(report["exit"], 4);
    assert!(
        stderr(&out).contains("repository not trusted; run kaban-index trust first"),
        "{}",
        stderr(&out)
    );
    assert!(!canary.exists(), "no indexer started");
    let written: Vec<PathBuf> = fs::read_dir(home.join("codegraph"))
        .map(|rd| {
            rd.flatten()
                .flat_map(|e| fs::read_dir(e.path()).into_iter().flatten().flatten())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "db" || x == "log"))
                .collect()
        })
        .unwrap_or_default();
    assert!(written.is_empty(), "nothing written: {written:?}");
}

// ---------------------------------------------------------------------------------------------
// slice 2: the exit-code table of G1-F7 through fake indexers (3, 4 x 6, 6 x 2, 7); exit 5 and
// 8 have their own tests below

#[test]
fn cli_index_exit_codes() {
    let s = Scratch::new("exit-codes");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_s = repo.to_str().unwrap();
    let fakes_dir = fakes(&s);
    let good = good_indexer(&s, &repo);
    let good_arg = indexer_arg(&good);
    let off = ["--isolation", "off"];

    // 4: not trusted (trust_refused)
    let out = index(&home, &repo, &["--indexer", &good_arg]);
    assert_eq!(code(&out), 4);
    assert_eq!(json(&out)["class"], "trust_refused");
    let repo_id = trust(&home, &repo);

    // 3: indexer not found (an override path that does not exist)
    let out = index(
        &home,
        &repo,
        &["--indexer", "go=/nonexistent/kaban-cg46/scip-go"],
    );
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "indexer_not_found");

    // 4: commit not checked out (another spelling, an unknown spelling)
    let first = head(&repo);
    let second = commit_more(&repo, 2, "2026-10-26T09:04:36Z");
    assert_ne!(first, second);
    let out = index(&home, &repo, &["--indexer", &good_arg, "--commit", &first]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "commit_not_checked_out");
    let out = index(
        &home,
        &repo,
        &["--indexer", &good_arg, "--commit", "no-such-ref"],
    );
    assert_eq!(code(&out), 4);
    assert_eq!(json(&out)["class"], "commit_not_checked_out");

    // 4: dirty tree
    fs::write(repo.join("x.go"), "package x\n// dirty\n").unwrap();
    let out = index(&home, &repo, &["--indexer", &good_arg]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "tree_dirty");
    git(&repo, &["checkout", "--", "x.go"]);

    // 4: config invalid (an unknown key under index:)
    fs::write(home.join("config.yaml"), "index:\n  keep_run_dir: true\n").unwrap();
    let out = index(&home, &repo, &["--indexer", &good_arg]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "config_invalid");
    assert!(
        !stderr(&out).contains("true"),
        "field path only, never the value: {}",
        stderr(&out)
    );
    fs::remove_file(home.join("config.yaml")).unwrap();

    // a meta value (the tool name) hitting the reject rule is masked and counted, never exit 4
    // (DF-3); the snapshot is removed again so the table goes on at `second`
    fs::write(home.join("config.yaml"), "index:\n  policy_scan: true\n").unwrap();
    let policy = canary_policy_file(s.path());
    let bad_tool = fakes_dir.join("canary-tool.scip");
    fs::write(
        &bad_tool,
        scip_bytes(&format!("file://{}", repo.display()), "CANARY_TOOL", None),
    )
    .unwrap();
    let canary_indexer = copying_indexer_named(&fakes_dir, "scip-go-canary", &bad_tool);
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "index",
            "--repo",
            repo_s,
            "--indexer",
            &indexer_arg(&canary_indexer),
            off[0],
            off[1],
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    // Go runs twice (A and B, G1-F3): the tool name is masked in both runs' metadata
    assert_eq!(report["counts"]["policy_redacted_meta"], 2);
    assert_eq!(
        report["policy_redacted_meta_per_indexer"]["go"],
        serde_json::json!(["canary-cred"])
    );
    assert!(!stderr(&out).contains("CANARY_TOOL"));
    let masked = snapshot_path(&home, &repo_id, &second);
    let meta = meta_of(&masked);
    assert!(
        meta["indexer:go"].starts_with("[policy:canary-cred] "),
        "{}",
        meta["indexer:go"]
    );
    assert_eq!(meta["counts:policy_redacted_meta"], "2");
    fs::remove_file(&masked).unwrap();
    fs::remove_file(home.join("config.yaml")).unwrap();

    // 4: an indexer in untrusted mode (group writable)
    let loose = fake_indexer(&fakes_dir, "loose-scip-go", "exit 0");
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o775)).unwrap();
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&loose)]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "indexer_untrusted_mode");

    // 6: indexer failed (non-zero exit), the gated log kept, no snapshot
    let failing = fake_indexer(&fakes_dir, "failing", "echo failing >&2\nexit 3");
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&failing), off[0], off[1]],
    );
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["class"], "indexer_failed");
    assert_eq!(report["exit"], 6);
    assert_eq!(report["isolation"], "none");
    assert!(
        repo_dir(&home, &repo_id)
            .join(format!("{second}.go.a.log"))
            .exists(),
        "the gated log is kept: {:?}",
        names(&home, &repo_id)
    );
    assert!(!snapshot_path(&home, &repo_id, &second).exists());

    // 6: output cap
    let flood = fake_indexer(&fakes_dir, "flood", "exec cat /dev/zero");
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&flood), off[0], off[1]],
    );
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "output_cap");

    // 7: timeout
    let hang = fake_indexer(&fakes_dir, "hang", "exec sleep 30");
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &indexer_arg(&hang),
            "--timeout",
            "1s",
            off[0],
            off[1],
        ],
    );
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "indexer_timeout");
    assert!(
        json(&out)["overrides"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o[0] == "index.deadline_secs" && o[1] == "1"),
        "the per-run override is listed: {}",
        json(&out)["overrides"]
    );

    // no raw file survives any of the above (the scratch is gone)
    assert!(
        names(&home, &repo_id)
            .iter()
            .all(|n| !n.starts_with("run-")),
        "{:?}",
        names(&home, &repo_id)
    );
    // 0: the good indexer, the snapshot written
    let out = index(&home, &repo, &["--indexer", &good_arg, off[0], off[1]]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(snapshot_path(&home, &repo_id, &second).exists());
}

// exit 5: the data directory cannot take the scratch or the snapshot
#[test]
fn cli_index_write_error_is_exit_5() {
    let s = Scratch::new("write-error");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    fs::set_permissions(repo_dir(&home, &repo_id), fs::Permissions::from_mode(0o500)).unwrap();
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    fs::set_permissions(repo_dir(&home, &repo_id), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert_eq!(json(&out)["exit"], 5);
    assert!(!snapshot_path(&home, &repo_id, &head(&repo)).exists());
}

// exit 8: `required` without an isolating tier. On a host where the namespaces tier is
// available (this one, per `doctor`) the refusal cannot be produced through the CLI; the
// library-level negative path (`userns_disabled_is_reported_and_preferred_falls_to_none`)
// and `SelectError::exit_code` pin it. The marker is printed, not a silent pass.
#[test]
fn cli_index_required_without_isolating_tier_is_exit_8() {
    let s = Scratch::new("required");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    trust(&home, &repo);
    let selected = doctor_selected_tier(&home, &repo);
    if selected != "none" {
        eprintln!(
            "NOT RUN: cli_index_required_without_isolating_tier_is_exit_8: the {selected} tier is available on this host; exit 8 is pinned by SelectError::exit_code and the CG4.4 negative path"
        );
        assert_eq!(
            kaban_index::isolate::SelectError::RequiredUnavailable.exit_code(),
            8
        );
        return;
    }
    let marker = s.path().join("producer-probed");
    let original = fs::read_to_string(&good).unwrap();
    fs::write(
        &good,
        original.replacen(
            "#!/bin/sh\n",
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
            1,
        ),
    )
    .unwrap();
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "required"],
    );
    assert!(
        !marker.exists(),
        "required refuses before any producer probe"
    );
    assert_eq!(code(&out), 8, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "isolation_unavailable");
    assert!(stderr(&out).contains("isolation required but no isolating tier is available"));
}

// ---------------------------------------------------------------------------------------------
// slices 3, 4: the two refusals of RS-5 as their own tests (the table above runs them too)

#[test]
fn cli_refuses_dirty_tree() {
    let s = Scratch::new("dirty");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    fs::write(repo.join("x.go"), "package x\n// edited\n").unwrap();
    // an untracked file alone is not dirt (`--untracked-files=no`)
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&good)]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "tree_dirty");
    assert!(stderr(&out).contains("tracked files are modified"));
    assert!(names(&home, &repo_id).iter().all(|n| n == "trust"));
    git(&repo, &["checkout", "--", "x.go"]);
    fs::write(repo.join("untracked.txt"), "x").unwrap();
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn cli_refuses_commit_not_head() {
    let s = Scratch::new("not-head");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    let first = head(&repo);
    let second = commit_more(&repo, 2, "2026-10-26T09:04:36Z");
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--commit", &first],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "commit_not_checked_out");
    assert!(stderr(&out).contains("not the checked-out HEAD"));
    assert!(!stderr(&out).contains(&first), "the argument is not echoed");
    assert!(names(&home, &repo_id).iter().all(|n| n == "trust"));
    // the explicit HEAD spelling and the full id both pass
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &indexer_arg(&good),
            "--commit",
            &second,
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(json(&out)["commit"], second);
}

// ---------------------------------------------------------------------------------------------
// slice 5: gc keeps HEAD + N by commit date, unknown commits first; dry-run; prune-corrupt;
// stale-policy; leftovers swept

#[test]
fn cli_gc_keep() {
    let s = Scratch::new("gc");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let c1 = head(&repo);
    let c2 = commit_more(&repo, 2, "2026-10-26T09:04:36Z");
    let c3 = commit_more(&repo, 3, "2026-10-26T09:53:10Z");
    let c4 = commit_more(&repo, 4, "2026-10-26T10:41:44Z");
    let repo_id = trust(&home, &repo);
    let unknown = "ff".repeat(20);
    let prov = || provenance(kaban_codegraph::IsolationTier::None, "none");
    for c in [&c1, &c2, &c3, &c4, &unknown] {
        publish_with(&home, &repo_id, c, prov(), "pv-current");
    }
    let dir = repo_dir(&home, &repo_id);
    fs::write(dir.join(format!("{c1}.go.a.log")), "log").unwrap();
    // a dead run's scratch and temp file (§7 leftovers)
    fs::create_dir_all(dir.join("run-999999").join("out")).unwrap();
    fs::write(dir.join(format!("{c2}.cg.db.tmp-999999")), "tmp").unwrap();
    let repo_s = repo.to_str().unwrap();

    // dry run: the plan, nothing deleted (the leftovers are swept even so)
    let out = kaban_index(&home, &["gc", "--repo", repo_s, "--keep", "1", "--dry-run"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let gc = &json(&out)["gc"];
    assert_eq!(gc["dry_run"], true);
    assert_eq!(gc["keep"], serde_json::json!([c4, c3]));
    assert_eq!(
        gc["collect"],
        serde_json::json!([
            [unknown, "unknown_commit"],
            [c2, "beyond_keep"],
            [c1, "beyond_keep"]
        ])
    );
    assert_eq!(gc["removed"], serde_json::json!([]));
    assert_eq!(gc["swept_leftovers"], 2);
    for c in [&c1, &c2, &c3, &c4, &unknown] {
        assert!(
            snapshot_path(&home, &repo_id, c).exists(),
            "dry-run kept {c}"
        );
    }
    assert!(!dir.join("run-999999").exists());
    assert!(!dir.join(format!("{c2}.cg.db.tmp-999999")).exists());
    let err = stderr(&out);
    assert!(err.contains(&format!("gc: keep {c4}")), "{err}");
    assert!(
        err.contains(&format!("gc: collect {unknown} (unknown_commit) [dry-run]")),
        "{err}"
    );

    // the real collection: the snapshot, its log and nothing else of HEAD and c3
    let out = kaban_index(&home, &["gc", "--repo", repo_s, "--keep", "1"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        json(&out)["gc"]["removed"],
        serde_json::json!([unknown, c2, c1])
    );
    let mut expected = vec![
        format!("{c3}.cg.db"),
        format!("{c4}.cg.db"),
        "trust".to_owned(),
    ];
    expected.sort();
    assert_eq!(names(&home, &repo_id), expected);

    // --prune-corrupt: garbage under the final name of a known commit (within keep) goes only
    // with the flag; the good ones stay
    fs::write(dir.join(format!("{c1}.cg.db")), b"not a database").unwrap();
    fs::set_permissions(
        dir.join(format!("{c1}.cg.db")),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let out = kaban_index(&home, &["gc", "--repo", repo_s, "--keep", "5"]);
    assert_eq!(code(&out), 0);
    assert!(
        dir.join(format!("{c1}.cg.db")).exists(),
        "without the flag it stays"
    );
    let out = kaban_index(
        &home,
        &["gc", "--repo", repo_s, "--keep", "5", "--prune-corrupt"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(json(&out)["gc"]["removed"], serde_json::json!([c1]));
    assert!(stderr(&out).contains(&format!("gc: collect {c1} (corrupt)")));
    assert!(snapshot_path(&home, &repo_id, &c3).exists());

    // --stale-policy: a policy_version other than the current policy's hash goes
    let out = kaban_index(
        &home,
        &["gc", "--repo", repo_s, "--keep", "5", "--stale-policy"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        json(&out)["gc"]["removed"],
        serde_json::json!([c4, c3]),
        "both carry pv-current, not the empty policy's hash"
    );
    publish_with(
        &home,
        &repo_id,
        &c4,
        prov(),
        Policy::empty().effective_hash(),
    );
    publish_with(&home, &repo_id, &c3, prov(), "pv-other");
    let out = kaban_index(
        &home,
        &["gc", "--repo", repo_s, "--keep", "5", "--stale-policy"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(json(&out)["gc"]["removed"], serde_json::json!([c3]));
    assert!(snapshot_path(&home, &repo_id, &c4).exists());
}

// ---------------------------------------------------------------------------------------------
// slice 6: doctor lists every tier (the deferred container row), the selection, the platform
// notes, the trust state before and after the mark, the trust text

#[test]
fn cli_doctor_lists_tiers() {
    let s = Scratch::new("doctor");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_s = repo.to_str().unwrap();
    let out = kaban_index(&home, &["doctor", "--repo", repo_s]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("tier namespaces: "), "{err}");
    assert!(
        err.contains("tier container: deferred (not_in_this_release)"),
        "{err}"
    );
    assert!(err.contains("tier selected: "), "{err}");
    assert!(err.contains("platform kernel: "), "{err}");
    assert!(
        err.contains("platform unprivileged_userns_clone: "),
        "{err}"
    );
    assert!(err.contains("platform max_user_namespaces: "), "{err}");
    assert!(err.contains("platform unshare: "), "{err}");
    assert!(err.contains("trust: not trusted"), "{err}");
    assert!(err.contains("identity: resolved"), "{err}");
    assert!(err.contains("trust text:"), "{err}");
    for line in kaban_index::trust::TRUST_TEXT {
        assert!(err.contains(line), "trust text line present: {line}");
    }
    let report = json(&out);
    let probes = report["doctor"]["isolation"]["probes"].as_array().unwrap();
    assert_eq!(probes.len(), 2);
    assert_eq!(probes[1]["tier"], "container");
    assert_eq!(probes[1]["available"]["Err"], "not_in_this_release");
    assert_eq!(report["doctor"]["trust"]["state"], "not_trusted");
    assert_eq!(report["doctor"]["trust"]["identity"], "resolved");
    let selected = report["doctor"]["isolation"]["selected"]["Ok"]["tier"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(selected == "namespaces" || selected == "none", "{selected}");
    if selected == "namespaces" {
        let detail = report["doctor"]["isolation"]["selected"]["Ok"]["detail"]
            .as_str()
            .unwrap();
        assert!(detail.starts_with("unshare=/"), "{detail}");
        assert!(detail.contains(" kernel="), "{detail}");
        assert!(detail.contains(" overlay="), "{detail}");
    }
    eprintln!("doctor on this host: tier selected {selected}");
    trust(&home, &repo);
    let out = kaban_index(&home, &["doctor", "--repo", repo_s]);
    assert_eq!(code(&out), 0);
    assert!(stderr(&out).contains("trust: trusted"), "{}", stderr(&out));
    assert_eq!(json(&out)["doctor"]["trust"]["state"], "trusted");
    // without a repository: no trust block, the tiers still listed
    let out = kaban_index(&home, &["doctor"]);
    assert_eq!(code(&out), 0);
    assert!(json(&out)["doctor"]["trust"].is_null());
    assert!(stderr(&out).contains("tier container: deferred"));
}

// ---------------------------------------------------------------------------------------------
// slice 7: status shows the live path, trust, identity and every snapshot's provenance; a
// `published_unsynced` snapshot is re-verified and its marker cleared

#[test]
fn cli_status_shows_provenance() {
    let s = Scratch::new("status");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_s = repo.to_str().unwrap();
    let c1 = head(&repo);
    let repo_id = trust(&home, &repo);
    let c2 = "22".repeat(20);
    publish_with(
        &home,
        &repo_id,
        &c1,
        provenance(
            kaban_codegraph::IsolationTier::Namespaces,
            "unshare=/usr/bin/unshare kernel=7.0.12 overlay=none",
        ),
        "pv",
    );
    publish_with(
        &home,
        &repo_id,
        &c2,
        provenance(kaban_codegraph::IsolationTier::None, "none"),
        "pv",
    );
    let dir = repo_dir(&home, &repo_id);
    // the marker of a publication whose directory fsync failed
    let marker = dir.join(format!("{c2}.cg.db.unsynced"));
    fs::write(&marker, b"").unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    // a corrupt third
    let c3 = "33".repeat(20);
    fs::write(dir.join(format!("{c3}.cg.db")), b"garbage").unwrap();
    fs::set_permissions(
        dir.join(format!("{c3}.cg.db")),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    let out = kaban_index(&home, &["status", "--repo", repo_s]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains(&format!("repository: {}", repo.display())),
        "{err}"
    );
    assert!(err.contains(&format!("repo_id: {repo_id}")), "{err}");
    assert!(err.contains("trust: trusted"), "{err}");
    assert!(err.contains("identity: resolved"), "{err}");
    assert!(
        err.contains(&format!(
            "snapshot {c1}: isolation=namespaces isolation_detail=unshare=/usr/bin/unshare kernel=7.0.12 overlay=none resources=modcache languages=go"
        )),
        "{err}"
    );
    assert!(err.contains("state=ok"), "{err}");
    assert!(
        err.contains(&format!(
            "snapshot {c2}: isolation=none isolation_detail=none resources=modcache"
        )),
        "{err}"
    );
    assert!(
        err.contains("state=re-verified (unsynced marker cleared)"),
        "{err}"
    );
    assert!(
        !marker.exists(),
        "the marker is cleared after re-verification"
    );
    assert!(
        err.contains(&format!("snapshot {c3}: isolation=? isolation_detail=? resources=? languages=? bytes=7 state=corrupt")),
        "{err}"
    );
    let report = json(&out);
    assert_eq!(report["status"]["trust"], "trusted");
    let snaps = report["status"]["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 3, "sorted by commit");
    let by_commit = |snaps: &[Value], c: &str| {
        snaps
            .iter()
            .find(|s| s["commit"] == c)
            .cloned()
            .unwrap_or_else(|| panic!("snapshot {c} listed"))
    };
    let s1 = by_commit(snaps, &c1);
    assert_eq!(s1["isolation"], "namespaces");
    assert_eq!(s1["resources"], "modcache");
    assert_eq!(s1["state"], "ok");
    assert_eq!(by_commit(snaps, &c2)["state"], "reverified");
    assert_eq!(by_commit(snaps, &c3)["state"], "corrupt");
    // the second status: the re-verified one is ordinary now
    let out = kaban_index(&home, &["status", "--repo", repo_s]);
    let snaps = json(&out)["status"]["snapshots"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(by_commit(&snaps, &c2)["state"], "ok");
    // a repository without a mark: status still answers (exit 0), trust not trusted
    let other = go_repo(&s.path().join("other"));
    let out = kaban_index(&home, &["status", "--repo", other.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stderr(&out).contains("trust: not trusted"));
    assert!(stderr(&out).contains("snapshots: none"));
}

// the `index` side of U1 residual 3: an existing snapshot with the marker is re-verified by
// `index` (already_indexed, marker cleared); a corrupt one is exit 5 and never overwritten
#[test]
fn index_reverifies_an_unsynced_snapshot_and_refuses_to_overwrite_a_corrupt_one() {
    let s = Scratch::new("unsynced-index");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let c1 = head(&repo);
    let repo_id = trust(&home, &repo);
    publish_with(
        &home,
        &repo_id,
        &c1,
        provenance(kaban_codegraph::IsolationTier::None, "none"),
        "pv",
    );
    let dir = repo_dir(&home, &repo_id);
    let marker = dir.join(format!("{c1}.cg.db.unsynced"));
    fs::write(&marker, b"").unwrap();
    let before = fs::read(snapshot_path(&home, &repo_id, &c1)).unwrap();
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "already_indexed");
    assert!(!marker.exists(), "re-verified, marker cleared");
    assert_eq!(
        fs::read(snapshot_path(&home, &repo_id, &c1)).unwrap(),
        before
    );
    // corrupt existing: exit 5, untouched
    fs::write(snapshot_path(&home, &repo_id, &c1), b"garbage").unwrap();
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "snapshot_corrupt");
    assert_eq!(
        fs::read(snapshot_path(&home, &repo_id, &c1)).unwrap(),
        b"garbage"
    );
}

// ---------------------------------------------------------------------------------------------
// slice 8: purge-derived removes everything derived and keeps the mark

#[test]
fn cli_purge_derived_removes_all() {
    let s = Scratch::new("purge");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let c1 = head(&repo);
    let repo_id = trust(&home, &repo);
    let prov = provenance(kaban_codegraph::IsolationTier::None, "none");
    publish_with(&home, &repo_id, &c1, prov.clone(), "pv");
    publish_with(&home, &repo_id, &"44".repeat(20), prov, "pv");
    let dir = repo_dir(&home, &repo_id);
    fs::write(dir.join(format!("{c1}.go.a.log")), "log").unwrap();
    fs::write(dir.join(format!("{c1}.cg.db.unsynced")), "").unwrap();
    fs::create_dir_all(dir.join("run-424242").join("cache")).unwrap();
    fs::write(dir.join("run-424242").join("cache").join("x"), "x").unwrap();
    assert!(names(&home, &repo_id).len() > 4);
    let out = kaban_index(&home, &["purge-derived", "--repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["verb"], "purge-derived");
    assert_eq!(report["purged"]["files"], 4);
    assert_eq!(report["purged"]["directories"], 1);
    assert_eq!(names(&home, &repo_id), vec!["trust".to_owned()]);
    assert!(stderr(&out).contains("purged: 4 files, 1 directories"));
    // the mark survives: index still allowed
    let good = good_indexer(&s, &repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

// ---------------------------------------------------------------------------------------------
// slice 9: untrust removes the mark; the next index is refused

#[test]
fn cli_untrust_removes() {
    let s = Scratch::new("untrust");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    assert!(repo_dir(&home, &repo_id).join("trust").exists());
    let out = kaban_index(&home, &["untrust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(json(&out)["verb"], "untrust");
    assert!(!repo_dir(&home, &repo_id).join("trust").exists());
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&good)]);
    assert_eq!(code(&out), 4);
    assert_eq!(json(&out)["class"], "trust_refused");
    let out = kaban_index(&home, &["untrust", "--repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 4, "a second untrust is trust_refused");
    assert_eq!(json(&out)["class"], "trust_refused");
}

// ---------------------------------------------------------------------------------------------
// slice 10 (G1-S5.8): the identity is revalidated immediately before every spawn, run B
// included: a fake scip-go that rewrites the repository's history during run A makes run B
// refused, the mark revoked, nothing published

#[test]
fn trust_revalidated_before_each_spawn() {
    let s = Scratch::new("revalidate");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    let ran_b = s.path().join("ran-b");
    // run A: copy the index, then replace the repository's history in place (new .git, new
    // root commit); run B: leave a canary
    let body = format!(
        "{OUT_ARG}\nif [ \"$skip\" = 1 ]; then touch '{}'; exit 0; fi\ncp '{}' \"$out\"\nrm -rf .git\ngit init -q -b main >/dev/null 2>&1\ngit add -A\ngit -c user.name=t -c user.email=t@example.invalid commit -q -m rewritten\nexit 0",
        ran_b.display(),
        src.display()
    );
    let indexer = fake_indexer(&fakes_dir, "scip-go", &body);
    let repo_id = trust(&home, &repo);
    let c1 = head(&repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&indexer), "--isolation", "off"],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["class"], "trust_revoked");
    assert_eq!(report["exit"], 4);
    assert!(
        stderr(&out).contains(kaban_index::trust::REVOKED_TEXT),
        "{}",
        stderr(&out)
    );
    assert!(!ran_b.exists(), "run B never started");
    assert!(
        !repo_dir(&home, &repo_id).join("trust").exists(),
        "the mark is revoked"
    );
    assert!(!snapshot_path(&home, &repo_id, &c1).exists());
    assert!(
        repo_dir(&home, &repo_id)
            .join(format!("{c1}.go.a.log"))
            .exists(),
        "run A's gated log stays"
    );
}

// ---------------------------------------------------------------------------------------------
// slice 11: the mark changes nothing about the run except whether the indexer starts: the
// same selected tier with and without it (`doctor`), the S5.3 environment in the gated log
// with it, no spawn and no log without it

#[test]
fn trust_does_not_change_runner_properties() {
    let s = Scratch::new("properties");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let env_indexer = fake_indexer(&fakes(&s), "scip-go", "env | sort\nexit 0");
    let without = doctor_selected_tier(&home, &repo);
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&env_indexer)]);
    assert_eq!(code(&out), 4);
    let repo_id = trust(&home, &repo);
    assert!(
        names(&home, &repo_id).iter().all(|n| n == "trust"),
        "no log without the mark"
    );
    let with = doctor_selected_tier(&home, &repo);
    assert_eq!(without, with, "the mark never selects a tier");
    // with the mark: the indexer starts (prints its environment, writes no output: the run
    // is `indexer_failed`), the log holds exactly the S5.3 values
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&env_indexer)]);
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    assert_eq!(json(&out)["isolation"], with);
    let log =
        fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{}.go.a.log", head(&repo))))
            .unwrap();
    assert!(log.contains("\nGOPROXY=off\n"), "{log}");
    assert!(log.contains("\nGOSUMDB=off\n"), "{log}");
    assert!(log.contains("\nGOTOOLCHAIN=local\n"), "{log}");
    assert!(log.contains("\nLC_ALL=C\n"), "{log}");
    assert!(log.contains("\nGOFLAGS=-mod=readonly\n"), "{log}");
    let path_line = log
        .lines()
        .find(|l| l.starts_with("PATH="))
        .expect("PATH line");
    assert!(
        path_line.contains(&fakes(&s).to_string_lossy().into_owned()),
        "the indexer's directory is on the pinned tool path: {path_line}"
    );
    assert!(
        !path_line.contains(".cargo/bin"),
        "never the caller's PATH: {path_line}"
    );
    let home_line = log
        .lines()
        .find(|l| l.starts_with("HOME="))
        .expect("HOME line");
    if with == "namespaces" {
        assert_eq!(home_line, "HOME=/scratch/home");
    } else {
        assert!(home_line.contains("/run-"), "{home_line}");
    }
}

// ---------------------------------------------------------------------------------------------
// slice 12: the tier that ran is in the snapshot's meta and in the report, identically; the
// admitted resources are recorded as kinds (gofix1-7: no cache mode any more)

#[test]
fn isolation_recorded_in_provenance_and_report() {
    let s = Scratch::new("provenance");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    let selected = doctor_selected_tier(&home, &repo);
    let c1 = head(&repo);
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&good)]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["isolation"], selected);
    let meta = meta_of(&snapshot_path(&home, &repo_id, &c1));
    assert_eq!(meta["isolation"], selected);
    assert_eq!(
        meta["isolation_detail"],
        report["isolation_detail"].as_str().unwrap()
    );
    assert_eq!(
        meta["resources"],
        report["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(!meta.contains_key("caches"), "{meta:?}");
    assert_eq!(meta["commit"], c1);
    assert_eq!(meta["repo_id"], repo_id);
    assert_eq!(meta["languages"], "go");
    assert_eq!(
        meta["policy_version"], "off",
        "policy_scan is off by default (DF-3)"
    );
    assert_eq!(report["policy_scan"], false);
    let warnings = report["warnings"].as_array().unwrap();
    if selected == "namespaces" {
        assert!(meta["isolation_detail"].starts_with("unshare=/"));
        assert!(meta["isolation_detail"].contains(" overlay="));
        assert!(!warnings.iter().any(|w| w == "isolation_none"));
        assert!(!stderr(&out).contains("running the indexer without isolation"));
    } else {
        assert_eq!(meta["isolation_detail"], "none");
        assert!(warnings.iter().any(|w| w == "isolation_none"));
        assert!(stderr(&out).contains(kaban_index::isolate::ISOLATION_NONE_TEXT));
    }
    eprintln!(
        "provenance on this host: isolation={} detail={} resources={}",
        meta["isolation"], meta["isolation_detail"], meta["resources"]
    );
    // --isolation off: none tier, the same resources row
    let c2 = commit_more(&repo, 2, "2026-10-26T09:04:36Z");
    let good2 = good_indexer(&s, &repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good2), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["isolation"], "none");
    let meta = meta_of(&snapshot_path(&home, &repo_id, &c2));
    assert_eq!(meta["isolation"], "none");
    assert!(meta.contains_key("resources"), "{meta:?}");
}

// ---------------------------------------------------------------------------------------------
// slice 13: the report carries the indexer's peak RSS and the effective overrides

#[test]
fn report_has_peak_rss_and_overrides() {
    let s = Scratch::new("report");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = lingering_indexer(&s, &repo);
    trust(&home, &repo);
    fs::write(
        home.join("config.yaml"),
        "index:\n  deadline_secs: 120\n  quota:\n    file_bytes: 1048576\n",
    )
    .unwrap();
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    let overrides: Vec<(String, String)> = report["overrides"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o[0].as_str().unwrap().to_owned(),
                o[1].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        overrides.contains(&("index.deadline_secs".to_owned(), "120".to_owned())),
        "{overrides:?}"
    );
    assert!(
        overrides.contains(&("index.quota.file_bytes".to_owned(), "1048576".to_owned())),
        "{overrides:?}"
    );
    assert!(
        overrides.contains(&("index.isolation".to_owned(), "off".to_owned())),
        "the per-run override is an effective override: {overrides:?}"
    );
    #[cfg(target_os = "linux")]
    let peak = report["peak_rss_bytes_per_indexer"]["go"]
        .as_u64()
        .expect("peak RSS sampled on Linux");
    #[cfg(target_os = "linux")]
    assert!(peak > 0, "{report}");
    #[cfg(not(target_os = "linux"))]
    assert!(
        report["peak_rss_bytes_per_indexer"]["go"].is_null(),
        "RSS unavailable"
    );
    assert!(report["wall_ms_per_indexer"]["go"].is_number());
    assert_eq!(report["languages"], serde_json::json!(["go"]));
    let counts = &report["counts"];
    assert_eq!(counts["files"], 1);
    assert!(
        counts["symbols"].as_u64().unwrap() >= 3,
        "the file node, the struct and the method at least: {counts}"
    );
    assert!(report["snapshot_bytes"].as_u64().unwrap() > 0);
    assert_eq!(report["exit"], 0);
    assert_eq!(report["ok"], true);
}

// ---------------------------------------------------------------------------------------------
// slice 14 (G1-S8): every error path with a canary in the input: the canary reaches neither
// stdout nor stderr nor any file under the data directory

#[test]
fn error_classes_fixed() {
    let s = Scratch::new("classes");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  policy_scan: true\n").unwrap();
    let repo = go_repo(&s.path().join("repo"));
    let policy = canary_policy_file(s.path());
    let policy_s = policy.to_str().unwrap();
    let repo_s = repo.to_str().unwrap();
    trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let check = |out: &Output, canary: &str, expect_code: i32| {
        assert_eq!(code(out), expect_code, "{}", stderr(out));
        assert!(!stderr(out).contains(canary), "stderr: {}", stderr(out));
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains(canary),
            "stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let hits = files_containing(&home, canary.as_bytes());
        assert!(hits.is_empty(), "persisted: {hits:?}");
    };
    // the commit argument
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy_s,
            "index",
            "--repo",
            repo_s,
            "--commit",
            "CANARY_COMMIT_7f3a",
        ],
    );
    check(&out, "CANARY_COMMIT_7f3a", 4);
    assert_eq!(json(&out)["class"], "commit_not_checked_out");
    // an indexer path that does not exist
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy_s,
            "index",
            "--repo",
            repo_s,
            "--indexer",
            "go=/nonexistent/CANARY_PATH_9b2c/scip-go",
        ],
    );
    check(&out, "CANARY_PATH_9b2c", 3);
    // an indexer printing the canary on both streams and failing: the log is gated
    let loud = fake_indexer(
        &fakes_dir,
        "loud",
        "echo token CANARY_OUT_1d4e >&1\necho token CANARY_ERR_5c6f >&2\nexit 2",
    );
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy_s,
            "index",
            "--repo",
            repo_s,
            "--indexer",
            &indexer_arg(&loud),
            "--isolation",
            "off",
        ],
    );
    check(&out, "CANARY_OUT_1d4e", 6);
    check(&out, "CANARY_ERR_5c6f", 6);
    assert_eq!(json(&out)["class"], "indexer_failed");
    let logs = files_containing(&home, b"[removed: canary-cred]");
    assert_eq!(logs.len(), 1, "the gated log holds the marker: {logs:?}");
    // an index whose documentation carries the canary: the row is dropped, the snapshot holds
    // no canary, the report's warning is the fixed class
    let doc = fakes_dir.join("doc.scip");
    fs::write(
        &doc,
        scip_bytes(
            &format!("file://{}", repo.display()),
            "scip-go",
            Some("secret CANARY_DOC_8e9f here"),
        ),
    )
    .unwrap();
    let doc_indexer = copying_indexer_named(&fakes_dir, "doc-indexer", &doc);
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy_s,
            "index",
            "--repo",
            repo_s,
            "--indexer",
            &indexer_arg(&doc_indexer),
            "--isolation",
            "off",
        ],
    );
    check(&out, "CANARY_DOC_8e9f", 0);
    assert!(
        json(&out)["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w == "policy_dropped"),
        "{}",
        json(&out)["warnings"]
    );
    // the repository path itself: refused before anything is derived or persisted
    let canary_repo = go_repo(&s.path().join("CANARY_REPO_2a7b"));
    let out = kaban_index(
        &home,
        &[
            "--policy",
            policy_s,
            "index",
            "--repo",
            canary_repo.to_str().unwrap(),
        ],
    );
    check(&out, "CANARY_REPO_2a7b", 4);
    assert_eq!(json(&out)["class"], "repo_path_refused");
}

// ---------------------------------------------------------------------------------------------
// slice 15 (§7 multi-language): without --partial a failing language writes nothing; with it
// the snapshot holds the languages that succeeded and the report names the failed one. Two
// languages both succeeding is the documented limitation of this release (one language per
// snapshot): refused with a fixed class, nothing written.

#[test]
fn partial_writes_only_successful_languages() {
    let s = Scratch::new("partial");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    fs::write(repo.join("package.json"), "{\"name\": \"x\"}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "ts marker"]);
    let c = head(&repo);
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let good = good_indexer(&s, &repo);
    let failing_ts = fake_indexer(&fakes_dir, "scip-typescript", "exit 1");
    let go_arg = indexer_arg(&good);
    let ts_arg = format!("typescript={}", failing_ts.display());
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &go_arg,
            "--indexer",
            &ts_arg,
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "indexer_failed");
    assert!(
        !snapshot_path(&home, &repo_id, &c).exists(),
        "nothing written"
    );
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &go_arg,
            "--indexer",
            &ts_arg,
            "--isolation",
            "off",
            "--partial",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["languages"], serde_json::json!(["go"]));
    assert_eq!(report["failed"]["typescript"], "indexer_failed");
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w == "partial"),
        "{}",
        report["warnings"]
    );
    let meta = meta_of(&snapshot_path(&home, &repo_id, &c));
    assert_eq!(meta["languages"], "go");
    // --languages narrows the run to one language: no partial needed
    let c2 = commit_more(&repo, 2, "2026-10-26T09:04:36Z");
    let good2 = good_indexer(&s, &repo);
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &indexer_arg(&good2),
            "--languages",
            "go",
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(json(&out)["languages"], serde_json::json!(["go"]));
    assert!(snapshot_path(&home, &repo_id, &c2).exists());
    // both succeeding: the documented limitation, nothing written
    let c3 = commit_more(&repo, 3, "2026-10-26T09:53:10Z");
    let ts_src = fakes_dir.join("ts.scip");
    fs::write(
        &ts_src,
        scip_bytes(
            &format!("file://{}", repo.display()),
            "scip-typescript",
            None,
        ),
    )
    .unwrap();
    let good_ts = copying_indexer_named(&fakes_dir, "good-ts", &ts_src);
    let good3 = good_indexer(&s, &repo);
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &indexer_arg(&good3),
            "--indexer",
            &format!("typescript={}", good_ts.display()),
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "multi_language_not_in_this_release");
    assert!(!snapshot_path(&home, &repo_id, &c3).exists());
    // a requested language without a marker
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good3), "--languages", "go,rust"],
    );
    assert_ne!(code(&out), 0, "rust is not a stage 1 language");
}

// ---------------------------------------------------------------------------------------------
// negative paths: project_root_mismatch (exit 6, indexer_failed, reason in the report); a
// snapshot already present is not overwritten (already_indexed, exit 0)

#[test]
fn project_root_mismatch_refuses_the_snapshot() {
    let s = Scratch::new("project-root");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("elsewhere.scip");
    fs::write(&src, scip_bytes("file:///somewhere/else", "scip-go", None)).unwrap();
    let indexer = copying_indexer_named(&fakes_dir, "scip-go", &src);
    let repo_id = trust(&home, &repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&indexer), "--isolation", "off"],
    );
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["class"], "indexer_failed");
    assert_eq!(report["reason"], "project_root_mismatch");
    assert!(
        !stderr(&out).contains("somewhere"),
        "the value is not echoed"
    );
    assert!(!snapshot_path(&home, &repo_id, &head(&repo)).exists());
}

#[test]
fn already_indexed_writes_nothing_and_exits_0() {
    let s = Scratch::new("already");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    let c = head(&repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(json(&out)["class"].is_null());
    let path = snapshot_path(&home, &repo_id, &c);
    let before = fs::read(&path).unwrap();
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let canary = s.path().join("ran-again");
    let spy = fake_indexer(
        &fakes(&s),
        "spy",
        &format!("touch '{}'\nexit 0", canary.display()),
    );
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&spy), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["class"], "already_indexed");
    assert_eq!(report["ok"], true);
    assert_eq!(report["commit"], c);
    assert!(stderr(&out).contains("already indexed"));
    assert!(!canary.exists(), "no indexer started");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
}

// a successful `index` applies `gc --keep 5`: seven indexed commits leave at most HEAD plus 5
// snapshots, the newest among them; the outcome is in the report's `gc` half
#[test]
fn index_applies_gc_keep_5_after_a_successful_write() {
    let s = Scratch::new("index-gc");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    let mut commits = vec![head(&repo)];
    let mut last = None;
    for n in 1..=7u32 {
        if n > 1 {
            commits.push(commit_more(&repo, n, &format!("2026-11-{n:02}T00:00:00Z")));
        }
        let out = index(
            &home,
            &repo,
            &["--indexer", &indexer_arg(&good), "--isolation", "off"],
        );
        assert_eq!(code(&out), 0, "index {n}: {}", stderr(&out));
        last = Some(json(&out));
    }
    let snapshots: Vec<String> = names(&home, &repo_id)
        .into_iter()
        .filter(|n| n.ends_with(".cg.db"))
        .collect();
    assert!(
        snapshots.len() <= 6,
        "HEAD plus 5 at most, got {snapshots:?}"
    );
    assert!(snapshot_path(&home, &repo_id, &commits[6]).is_file());
    assert!(!snapshot_path(&home, &repo_id, &commits[0]).exists());
    let report = last.unwrap();
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w != "gc_failed")
    );
    assert_eq!(report["gc"]["removed"], serde_json::json!([commits[0]]));
    assert_eq!(report["gc"]["keep"][0], commits[6]);
}

// ---------------------------------------------------------------------------------------------
// the real producer: scip-go 0.2.7 over the scripted go-drivers history under the tier doctor
// selects (gated on a reachable scip-go: `$SCIP_GO`, `~/go/bin/scip-go`, else PATH; a visible
// marker otherwise). The published rows are compared with the CG3.4 goldens table by table
// (the committed `index.scip` was produced by the same version); the `meta` table differs by
// construction (repo_id, policy_version, provenance, the `--output` argv), so bytes are not
// compared and the difference is reported, not hidden.
//
// cgo: the namespaces view binds `gcc` (`/usr/bin`) but not `/usr/include`, so Go's cgo
// autodetection turns cgo on and the cgo-dependent standard-library packages fail to build
// inside the view; scip-go then indexes implementations over a different package set (70
// symbols and 166 edges measured here against the golden's 66 and 162). The strict comparison
// therefore runs with `CGO_ENABLED=0` carried from the caller (S5.3: `CGO_ENABLED` as the
// caller's), which reproduces the goldens exactly under both tiers; the autodetect run is
// executed afterwards and its counts printed as evidence (the CG4.6 report records the
// recommendation: default `CGO_ENABLED=0` under the namespaces tier, or bind the C toolchain).

fn find_scip_go() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SCIP_GO") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("go/bin/scip-go");
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("scip-go"))
            .find(|p| p.is_file())
    })
}

/// The scripted go-drivers history (C1, C2) rebuilt from `fixtures/codegraph/go-drivers/repo`
/// as `mkfixture.sh` does, asserted against `COMMITS`.
fn go_drivers_repo(dir: &Path) -> PathBuf {
    let src = common::fixtures_dir().join("go-drivers").join("repo");
    let st = Command::new("cp")
        .arg("-R")
        .arg(&src)
        .arg(dir)
        .status()
        .unwrap();
    assert!(st.success());
    let fixture_git = |args: &[&str], date: &str| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "kaban fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@kaban.invalid")
            .env("GIT_COMMITTER_NAME", "kaban fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@kaban.invalid")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let d1 = "2026-10-26T08:16:02Z";
    let d2 = "2026-10-26T09:04:36Z";
    fixture_git(&["init", "-q", "-b", "main"], d1);
    fixture_git(&["add", "-A"], d1);
    fixture_git(&["commit", "-q", "-m", "C1: drivers fixture"], d1);
    fixture_git(&["mv", "drivers/mem.go", "drivers/memory.go"], d2);
    let pg = dir.join("drivers").join("postgres.go");
    let text = fs::read_to_string(&pg).unwrap();
    let edited = text.replace("\t_ = marker\n", "\t_ = marker\n\t_ = dsn\n");
    assert_ne!(edited, text, "the marker line is present in the fixture");
    fs::write(&pg, edited).unwrap();
    fixture_git(&["add", "-A"], d2);
    fixture_git(
        &[
            "commit",
            "-q",
            "-m",
            "C2: rename mem.go, extend PostgresDriver.Open",
        ],
        d2,
    );
    let c2 = fixture_git(&["rev-parse", "HEAD"], d2).trim().to_owned();
    let commits =
        fs::read_to_string(common::fixtures_dir().join("go-drivers").join("COMMITS")).unwrap();
    assert!(
        commits.contains(&format!("C2 {c2}")),
        "the scripted history reproduces COMMITS: {c2} vs {commits}"
    );
    fs::canonicalize(dir).unwrap()
}

/// The golden table dumps of `tests/goldens.rs` (same SQL, same formatting).
fn table_dumps(path: &Path) -> Vec<(&'static str, String)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let dump = |sql: &str| {
        let mut stmt = conn.prepare(sql).unwrap();
        let cols = stmt.column_count();
        let mut out = String::new();
        let rows = stmt
            .query_map([], |r| {
                (0..cols)
                    .map(|i| {
                        r.get_ref(i).map(|v| match v {
                            rusqlite::types::ValueRef::Null => "NULL".to_owned(),
                            rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                            rusqlite::types::ValueRef::Real(f) => f.to_string(),
                            rusqlite::types::ValueRef::Text(t) => {
                                String::from_utf8_lossy(t).replace(['\t', '\n'], " ")
                            }
                            rusqlite::types::ValueRef::Blob(b) => format!("<{} bytes>", b.len()),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        for r in rows {
            let _ = writeln!(out, "{}", r.unwrap().join("\t"));
        }
        out
    };
    vec![
        (
            "files",
            dump("SELECT id, path, language, is_test, lines FROM files ORDER BY id"),
        ),
        (
            "symbols",
            dump(
                "SELECT id, kind, ref, key, label, name, file_id, def_sl, def_sc, def_el, def_ec, \
                 enc_sl, enc_sc, enc_el, enc_ec, external, is_test, ident_terms, \
                 length(signature), length(doc) FROM symbols ORDER BY id",
            ),
        ),
        (
            "edges",
            dump(
                "SELECT (SELECT ref FROM symbols WHERE id = e.src), e.type, \
                 (SELECT ref FROM symbols WHERE id = e.dst), e.weight FROM edges e \
                 ORDER BY e.src, e.type, e.dst",
            ),
        ),
        (
            "occurrences",
            dump(
                "SELECT f.path, o.sl, o.sc, o.el, o.ec, (SELECT ref FROM symbols WHERE id = o.symbol_id), \
                 o.roles, (SELECT ref FROM symbols WHERE id = o.enclosing_id) \
                 FROM occurrences o JOIN files f ON f.id = o.file_id \
                 ORDER BY o.file_id, o.sl, o.sc, o.el, o.ec, o.symbol_id, o.roles",
            ),
        ),
    ]
}

#[test]
fn index_go_drivers_real() {
    let Some(scip_go) = find_scip_go() else {
        eprintln!(
            "NOT RUN: index_go_drivers_real: no scip-go reachable (SCIP_GO, ~/go/bin/scip-go, PATH)"
        );
        return;
    };
    let version = Command::new(&scip_go).arg("--version").output().unwrap();
    let version = String::from_utf8_lossy(&version.stdout).trim().to_owned();
    let s = Scratch::new("go-drivers");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  policy_scan: true\n").unwrap();
    let repo = go_drivers_repo(&s.path().join("go-drivers"));
    let repo_id = trust(&home, &repo);
    let c2 = head(&repo);
    assert_eq!(c2, common::GO_DRIVERS_COMMIT);
    let selected = doctor_selected_tier(&home, &repo);
    let policy =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/policy-test-canaries.yaml");
    let real_args = [
        "--policy",
        policy.to_str().unwrap(),
        "index",
        "--repo",
        repo.to_str().unwrap(),
        "--indexer",
        &indexer_arg(&scip_go),
        "--timeout",
        "10m",
    ];
    let out = kaban_index_env(&home, &real_args, &[("CGO_ENABLED", "0")]);
    let log_path = repo_dir(&home, &repo_id).join(format!("{c2}.go.a.log"));
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert_eq!(
        code(&out),
        0,
        "stderr: {}\n--- gated log\n{log}",
        stderr(&out)
    );
    let report = json(&out);
    assert_eq!(report["isolation"], selected);
    assert_eq!(report["languages"], serde_json::json!(["go"]));
    let counts = &report["counts"];
    eprintln!(
        "index_go_drivers_real: scip-go {version} under {selected} ({}); counts files={} symbols={} edges={} occurrences={}; snapshot_bytes={}; wall_ms={}; peak_rss={}",
        report["isolation_detail"],
        counts["files"],
        counts["symbols"],
        counts["edges"],
        counts["occurrences"],
        report["snapshot_bytes"],
        report["wall_ms_per_indexer"]["go"],
        report["peak_rss_bytes_per_indexer"]["go"]
    );
    let path = snapshot_path(&home, &repo_id, &c2);
    let meta = meta_of(&path);
    assert_eq!(meta["isolation"], selected);
    assert!(!meta["union_reason"].is_empty(), "run B was unioned in");
    let golden_meta: BTreeMap<String, String> =
        fs::read_to_string(common::fixtures_dir().join("go-drivers/goldens/meta.txt"))
            .unwrap()
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
    if version == "0.2.7" {
        for key in [
            "counts:files",
            "counts:symbols",
            "counts:edges",
            "counts:occurrences",
        ] {
            assert_eq!(meta[key], golden_meta[key], "{key} equals the CG3.4 golden");
        }
        for (name, observed) in table_dumps(&path) {
            let golden = fs::read_to_string(
                common::fixtures_dir().join(format!("go-drivers/goldens/{name}.txt")),
            )
            .unwrap();
            assert_eq!(
                observed, golden,
                "{name}: the real run's rows equal the CG3.4 golden"
            );
        }
        let differing: Vec<&str> = golden_meta
            .iter()
            .filter(|(k, v)| meta.get(*k) != Some(*v))
            .map(|(k, _)| k.as_str())
            .collect();
        eprintln!(
            "index_go_drivers_real: meta keys differing from the golden by construction: {differing:?}"
        );
    } else {
        eprintln!(
            "index_go_drivers_real: scip-go {version} is not the fixture's 0.2.7; sizes and counts compared only: golden files={} symbols={} edges={} occurrences={} bytes={}",
            golden_meta["counts:files"],
            golden_meta["counts:symbols"],
            golden_meta["counts:edges"],
            golden_meta["counts:occurrences"],
            fs::read_to_string(
                common::fixtures_dir().join("go-drivers/goldens/snapshot_bytes.txt")
            )
            .unwrap()
            .trim()
        );
        assert!(counts["files"].as_u64().unwrap() > 0);
    }
    // the second run: already indexed
    let out = kaban_index(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&scip_go),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "already_indexed");
    // evidence run: cgo as the caller's (unset: Go autodetects); files and occurrences must
    // still equal the golden, the implementation-dependent counts are printed
    let out = kaban_index(&home, &["purge-derived", "--repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 0);
    let out = kaban_index(&home, &real_args);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    let counts = &report["counts"];
    eprintln!(
        "index_go_drivers_real (CGO_ENABLED unset, {selected}): files={} symbols={} edges={} occurrences={} snapshot_bytes={} wall_ms={}",
        counts["files"],
        counts["symbols"],
        counts["edges"],
        counts["occurrences"],
        report["snapshot_bytes"],
        report["wall_ms_per_indexer"]["go"]
    );
    if version == "0.2.7" {
        assert_eq!(counts["files"].to_string(), golden_meta["counts:files"]);
        assert_eq!(
            counts["occurrences"].to_string(),
            golden_meta["counts:occurrences"]
        );
    }
}

/// `mkzip <srcdir> <out.zip>`: the module zip of `example.com/dep@v1.0.0` from `<srcdir>`,
/// written by Go's own `archive/zip` so the file proxy below serves what `go` expects.
const MKZIP_GO: &str = r#"package main

import (
	"archive/zip"
	"os"
	"path/filepath"
)

func main() {
	src, out := os.Args[1], os.Args[2]
	f, err := os.Create(out)
	if err != nil {
		panic(err)
	}
	w := zip.NewWriter(f)
	for _, name := range []string{"go.mod", "dep.go"} {
		data, err := os.ReadFile(filepath.Join(src, name))
		if err != nil {
			panic(err)
		}
		e, err := w.Create("example.com/dep@v1.0.0/" + name)
		if err != nil {
			panic(err)
		}
		if _, err := e.Write(data); err != nil {
			panic(err)
		}
	}
	if err := w.Close(); err != nil {
		panic(err)
	}
	if err := f.Close(); err != nil {
		panic(err)
	}
}
"#;

/// A Go module cache under the scratch holding `example.com/dep v1.0.0` (served to the real
/// `go` from a file proxy and downloaded by `go mod tidy`), and a committed fixture module
/// `example.com/app` whose one function calls `dep.Answer()`. Returns `(modcache, repo)`;
/// `None` when no `go` runs on this host.
fn go_external_fixture(s: &Scratch) -> Option<(PathBuf, PathBuf)> {
    if !Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return None;
    }
    let base = s.path().join("ext");
    let dep = base.join("dep-src");
    fs::create_dir_all(&dep).unwrap();
    fs::write(dep.join("go.mod"), "module example.com/dep\n\ngo 1.22\n").unwrap();
    fs::write(
        dep.join("dep.go"),
        "package dep\n\n// Answer is the dependency's one function.\nfunc Answer() int { return 42 }\n",
    )
    .unwrap();
    let proxy = base
        .join("proxy")
        .join("example.com")
        .join("dep")
        .join("@v");
    fs::create_dir_all(&proxy).unwrap();
    fs::write(proxy.join("list"), "v1.0.0\n").unwrap();
    fs::write(
        proxy.join("v1.0.0.info"),
        "{\"Version\":\"v1.0.0\",\"Time\":\"2026-10-26T08:16:02Z\"}\n",
    )
    .unwrap();
    fs::copy(dep.join("go.mod"), proxy.join("v1.0.0.mod")).unwrap();
    let mkzip = base.join("mkzip");
    fs::create_dir_all(&mkzip).unwrap();
    fs::write(mkzip.join("go.mod"), "module mkzip\n\ngo 1.22\n").unwrap();
    fs::write(mkzip.join("main.go"), MKZIP_GO).unwrap();
    let modcache = base.join("modcache");
    let go = |cwd: &Path, args: &[&str]| {
        let out = Command::new("go")
            .args(args)
            .current_dir(cwd)
            .env("GOCACHE", base.join("gocache"))
            .env("GOMODCACHE", &modcache)
            .env(
                "GOPROXY",
                format!("file://{}", base.join("proxy").display()),
            )
            .env("GOSUMDB", "off")
            .env("GOFLAGS", "-mod=mod")
            .env("GOTOOLCHAIN", "local")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "go {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    go(
        &mkzip,
        &[
            "run",
            ".",
            dep.to_str().unwrap(),
            proxy.join("v1.0.0.zip").to_str().unwrap(),
        ],
    );
    let repo = s.path().join("ext-repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("go.mod"),
        "module example.com/app\n\ngo 1.22\n\nrequire example.com/dep v1.0.0\n",
    )
    .unwrap();
    fs::write(
        repo.join("app.go"),
        "package app\n\nimport \"example.com/dep\"\n\n// Twice doubles the dependency's answer.\nfunc Twice() int { return 2 * dep.Answer() }\n",
    )
    .unwrap();
    go(&repo, &["mod", "tidy"]);
    assert!(
        modcache.join("example.com").join("dep@v1.0.0").is_dir(),
        "the dependency is in the module cache"
    );
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &["commit", "-q", "-m", "app with one external dependency"],
    );
    Some((modcache, fs::canonicalize(&repo).unwrap()))
}

/// The `ref` of every external symbol of a snapshot.
fn external_symbol_refs(path: &Path) -> Vec<String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn
        .prepare("SELECT ref FROM symbols WHERE external = 1 ORDER BY ref")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

// G1 (GO-FIX-1, safety of results): the user's module cache is an input of every Go run on
// both tiers, with no flag. `index --isolation off` of a module with one external dependency
// resolves the external symbol from `GOMODCACHE` (a dedicated empty cache left a third of the
// external references out, exit 0, no warning), and the counts equal the namespaces run's.
#[test]
fn go_module_cache_is_an_input_on_every_tier() {
    let Some(scip_go) = find_scip_go() else {
        eprintln!("NOT RUN: go_module_cache_is_an_input_on_every_tier: no scip-go reachable");
        return;
    };
    let s = Scratch::new("go-modcache");
    let home = s.home();
    let Some((modcache, repo)) = go_external_fixture(&s) else {
        eprintln!("NOT RUN: go_module_cache_is_an_input_on_every_tier: no go on this host");
        return;
    };
    let repo_id = trust(&home, &repo);
    let c1 = head(&repo);
    let env = [
        ("GOMODCACHE", modcache.to_str().unwrap()),
        ("CGO_ENABLED", "0"),
    ];
    let indexer = indexer_arg(&scip_go);
    let args = |isolation: &'static str| {
        vec![
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            indexer.as_str(),
            "--timeout",
            "10m",
            "--isolation",
            isolation,
        ]
    };
    let out = kaban_index_env(&home, &args("off"), &env);
    let log = fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c1}.go.a.log")))
        .unwrap_or_default();
    assert_eq!(
        code(&out),
        0,
        "stderr: {}\n--- gated log\n{log}",
        stderr(&out)
    );
    let report = json(&out);
    assert_eq!(report["isolation"], "none");
    assert!(
        !report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w == "caches_shared"),
        "no cache flag and no cache warning: {}",
        report["warnings"]
    );
    let externals = external_symbol_refs(&snapshot_path(&home, &repo_id, &c1));
    assert!(
        externals
            .iter()
            .any(|r| r.contains("example.com/dep") && r.contains("Answer")),
        "the external symbol resolves from the module cache on the none tier; externals: {externals:?}\n--- gated log\n{log}"
    );
    let off_counts = report["counts"].clone();
    eprintln!("go_module_cache_is_an_input_on_every_tier: none tier counts {off_counts}");
    if doctor_selected_tier(&home, &repo) != "namespaces" {
        eprintln!(
            "go_module_cache_is_an_input_on_every_tier: no namespaces tier on this host; the tier comparison did not run"
        );
        return;
    }
    let out = kaban_index(&home, &["purge-derived", "--repo", repo.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = kaban_index_env(&home, &args("required"), &env);
    let log = fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c1}.go.a.log")))
        .unwrap_or_default();
    assert_eq!(
        code(&out),
        0,
        "stderr: {}\n--- gated log\n{log}",
        stderr(&out)
    );
    let report = json(&out);
    assert_eq!(report["isolation"], "namespaces");
    assert_eq!(
        report["counts"], off_counts,
        "the namespaces run publishes the counts of the none run"
    );
    let externals = external_symbol_refs(&snapshot_path(&home, &repo_id, &c1));
    assert!(
        externals
            .iter()
            .any(|r| r.contains("example.com/dep") && r.contains("Answer")),
        "the external symbol resolves inside the view too; externals: {externals:?}"
    );
}

/// The version of the `go` on this host as `go version` prints it (`1.26.0`), and the next
/// minor (`1.27`) as a `go.mod` directive no installed toolchain satisfies; `None` without go.
fn go_versions_for_directive_tests() -> Option<(String, String)> {
    let out = Command::new("go").arg("version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let word = text.split_whitespace().nth(2)?.strip_prefix("go")?;
    let mut parts = word.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((word.to_owned(), format!("{major}.{}", minor + 1)))
}

/// `go env GOOS GOARCH` of this host.
fn go_os_arch() -> (String, String) {
    let out = Command::new("go")
        .args(["env", "GOOS", "GOARCH"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    (
        lines.next().unwrap().to_owned(),
        lines.next().unwrap().to_owned(),
    )
}

/// A committed Go module at `dir` whose `go.mod` holds `directive`.
fn go_repo_with_directive(dir: &Path, directive: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(
        dir.join("go.mod"),
        format!("module example.com/x\n\n{directive}\n"),
    )
    .unwrap();
    fs::write(dir.join("x.go"), "package x\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    fs::canonicalize(dir).unwrap()
}

// G2 (GO-FIX-1): a `go.mod` directive above the installed toolchain is said in plain words:
// doctor shows the row, `index` refuses with the same text and the class `go_toolchain_too_old`
// before any indexer is spawned (today: the indexer runs and fails in 50 ms as a bare
// `indexer_failed`, doctor says nothing).
#[test]
fn go_directive_above_the_toolchain_is_refused_in_plain_words() {
    let Some((installed, required)) = go_versions_for_directive_tests() else {
        eprintln!("NOT RUN: go_directive_above_the_toolchain_is_refused_in_plain_words: no go");
        return;
    };
    let s = Scratch::new("go-directive");
    let home = s.home();
    let repo = go_repo_with_directive(&s.path().join("repo"), &format!("go {required}"));
    let modcache = s.path().join("modcache");
    fs::create_dir_all(&modcache).unwrap();
    let marker = s.path().join("indexer-ran");
    let indexer = fake_indexer(
        &fakes(&s),
        "scip-go",
        &format!("touch '{}'\nexit 0", marker.display()),
    );
    trust(&home, &repo);
    let env = [("GOMODCACHE", modcache.to_str().unwrap())];
    let expected = format!("go toolchain: {installed}, go.mod requires {required}: ");
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
        ],
        &env,
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&expected),
        "doctor says it in plain words; expected {expected:?} in:\n{}",
        stderr(&out)
    );
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
            "--isolation",
            "off",
        ],
        &env,
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "go_toolchain_too_old");
    assert!(
        stderr(&out).contains(&expected),
        "index refuses with the same text:\n{}",
        stderr(&out)
    );
    assert!(!marker.exists(), "the indexer was never spawned");
}

// gofix1-1 (FIX-E): the module cache root is a narrow resource. `GOMODCACHE` naming the caller's
// home directory, an ancestor of it, or a directory that contains the checkout is refused with
// `go_modcache_refused` in plain words before any spawn (today: admitted, and the whole
// directory is bound read-only into the view).
#[test]
fn a_module_cache_that_is_home_or_contains_the_checkout_is_refused() {
    let s = Scratch::new("go-modcache-wide");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let caller_home = s.path().join("caller-home");
    fs::create_dir_all(caller_home.join("go/pkg/mod/cache/download")).unwrap();
    fs::write(caller_home.join("secret.txt"), "not an input of any run").unwrap();
    let marker = s.path().join("indexer-ran");
    let indexer = fake_indexer(
        &fakes(&s),
        "scip-go",
        &format!("touch '{}'\nexit 0", marker.display()),
    );
    trust(&home, &repo);
    let cases: [(&str, PathBuf); 3] = [
        ("the home directory", caller_home.clone()),
        ("an ancestor of the home directory", s.path().to_path_buf()),
        (
            "a directory containing the checkout",
            s.path().to_path_buf(),
        ),
    ];
    for (what, cache) in &cases {
        let out = kaban_index_env(
            &home,
            &[
                "index",
                "--repo",
                repo.to_str().unwrap(),
                "--indexer",
                &indexer_arg(&indexer),
                "--isolation",
                "off",
            ],
            &[
                ("HOME", caller_home.to_str().unwrap()),
                ("GOMODCACHE", cache.to_str().unwrap()),
            ],
        );
        assert_eq!(code(&out), 4, "{what}: {}", stderr(&out));
        assert_eq!(json(&out)["class"], "go_modcache_refused", "{what}");
        assert!(
            stderr(&out).contains("the Go module cache"),
            "{what}: plain words:\n{}",
            stderr(&out)
        );
        assert!(!marker.exists(), "{what}: the indexer was never spawned");
    }
    // the legitimate default under that home is admitted (no `go` on the fake's PATH is a
    // different, later refusal than the cache's)
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
            "--isolation",
            "off",
        ],
        &[("HOME", caller_home.to_str().unwrap())],
    );
    assert_ne!(
        json(&out)["class"],
        "go_modcache_refused",
        "$HOME/go/pkg/mod with cache/download is a module cache:\n{}",
        stderr(&out)
    );
}

// G2 (GO-FIX-1): when the module cache holds `golang.org/toolchain@v0.0.1-go<ver>.<os>-<arch>`
// satisfying the directive, its `bin/go` is admitted as the run's toolchain: first on the
// child's PATH and its GOROOT, no user action, doctor shows it.
#[test]
fn module_cache_toolchain_is_admitted_for_a_newer_directive() {
    let Some((installed, required)) = go_versions_for_directive_tests() else {
        eprintln!("NOT RUN: module_cache_toolchain_is_admitted_for_a_newer_directive: no go");
        return;
    };
    let (goos, goarch) = go_os_arch();
    let s = Scratch::new("go-modcache-toolchain");
    let home = s.home();
    let repo = go_repo_with_directive(&s.path().join("repo"), &format!("go {required}"));
    let modcache = s.path().join("modcache");
    let newer = format!("{required}.0");
    let root = modcache
        .join("golang.org")
        .join(format!("toolchain@v0.0.1-go{newer}.{goos}-{goarch}"));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("VERSION"), format!("go{newer}\n")).unwrap();
    fake_indexer(
        &root.join("bin"),
        "go",
        &format!("echo \"go version go{newer} {goos}/{goarch}\""),
    );
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    let indexer = fake_indexer(
        &fakes_dir,
        "scip-go",
        &format!(
            "env | sort\n{OUT_ARG}\ncp '{}' \"$out\"\nexit 0",
            src.display()
        ),
    );
    let repo_id = trust(&home, &repo);
    let c1 = head(&repo);
    let env = [("GOMODCACHE", modcache.to_str().unwrap())];
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
        ],
        &env,
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let row = format!(
        "go toolchain: {newer} (module cache, installed {installed}), go.mod requires {required}"
    );
    assert!(
        stderr(&out).contains(&row),
        "doctor shows the admitted toolchain; expected {row:?} in:\n{}",
        stderr(&out)
    );
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
            "--isolation",
            "off",
        ],
        &env,
    );
    let log = fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c1}.go.a.log")))
        .unwrap_or_default();
    assert_eq!(
        code(&out),
        0,
        "stderr: {}\n--- gated log\n{log}",
        stderr(&out)
    );
    let canonical_root = fs::canonicalize(&root).unwrap();
    assert!(
        log.contains(&format!("GOROOT={}\n", canonical_root.display())),
        "GOROOT names the module-cache toolchain:\n{log}"
    );
    let path_line = log
        .lines()
        .find_map(|l| l.strip_prefix("PATH="))
        .expect("the fake printed PATH");
    let bin = canonical_root.join("bin");
    assert_eq!(
        std::env::split_paths(path_line).next().as_deref(),
        Some(bin.as_path()),
        "the module-cache toolchain's bin is first on PATH: {path_line}"
    );
}

// gofix1-7 (FIX-E, backlog 25): `--share-caches` is retired. The flag is unknown to the CLI
// (exit 2, the fixed usage diagnostic), the snapshot meta has no `caches` row, and the
// provenance of what was bound is derived from the admitted resources: a `resources` row
// listing kinds (`modcache` here; `registry`, `toolchain`), the same list in the report.
#[test]
fn share_caches_is_retired_and_resources_are_recorded() {
    let s = Scratch::new("share-caches-retired");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let good = good_indexer(&s, &repo);
    let repo_id = trust(&home, &repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--share-caches"],
    );
    assert_eq!(code(&out), 2, "unknown flag: {}", stderr(&out));
    assert!(
        stderr(&out).contains("usage error (UnknownArgument)"),
        "the fixed usage diagnostic (U2 RU2-8):\n{}",
        stderr(&out)
    );
    let modcache = s.path().join("modcache");
    fs::create_dir_all(modcache.join("cache/download")).unwrap();
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&good),
            "--isolation",
            "off",
        ],
        &[("GOMODCACHE", modcache.to_str().unwrap())],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert!(report.get("caches").is_none(), "{report}");
    assert_eq!(report["resources"], serde_json::json!(["modcache"]));
    let meta = meta_of(&snapshot_path(&home, &repo_id, &head(&repo)));
    assert!(!meta.contains_key("caches"), "{meta:?}");
    assert_eq!(meta["resources"], "modcache");
}

// gofix1-6 (FIX-E): a `go.mod` that cannot be read whole (here: over the 1 MiB bound, its
// directive after the bound) is said as such: doctor says "go.mod unreadable: <reason>" rather
// than "requires nothing", and `index` refuses with `go_mod_unreadable` before any spawn.
#[test]
fn an_unreadable_go_mod_is_said_in_plain_words() {
    if go_versions_for_directive_tests().is_none() {
        eprintln!("NOT RUN: an_unreadable_go_mod_is_said_in_plain_words: no go");
        return;
    }
    let s = Scratch::new("go-mod-oversized");
    let home = s.home();
    let mut text = String::from("module example.com/x\n");
    text.push_str(&" ".repeat(1 << 20));
    text.push_str("\ngo 1.99\n");
    let repo = go_repo_with_directive(&s.path().join("repo"), &text);
    let modcache = s.path().join("modcache");
    fs::create_dir_all(&modcache).unwrap();
    let marker = s.path().join("indexer-ran");
    let indexer = fake_indexer(
        &fakes(&s),
        "scip-go",
        &format!("touch '{}'\nexit 0", marker.display()),
    );
    trust(&home, &repo);
    let env = [("GOMODCACHE", modcache.to_str().unwrap())];
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
        ],
        &env,
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("go toolchain: go.mod unreadable: larger than 1 MiB"),
        "doctor says why:\n{}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("requires nothing"),
        "{}",
        stderr(&out)
    );
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
            "--isolation",
            "off",
        ],
        &env,
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "go_mod_unreadable");
    assert!(
        stderr(&out).contains("go.mod unreadable: larger than 1 MiB"),
        "{}",
        stderr(&out)
    );
    assert!(!marker.exists(), "the indexer was never spawned");
}

// gofix1-3 (FIX-E): the cached `go` is revalidated at each spawn, not once in `prepare()`: a
// replacement between run A and run B (here by run A itself, as a host process could) refuses
// run B with a class naming the tool, before it is executed.
#[test]
fn a_cached_go_replaced_between_the_two_runs_refuses_the_second() {
    let Some((_, required)) = go_versions_for_directive_tests() else {
        eprintln!("NOT RUN: a_cached_go_replaced_between_the_two_runs_refuses_the_second: no go");
        return;
    };
    let (goos, goarch) = go_os_arch();
    let s = Scratch::new("go-modcache-swap");
    let home = s.home();
    let repo = go_repo_with_directive(&s.path().join("repo"), &format!("go {required}"));
    let modcache = s.path().join("modcache");
    let newer = format!("{required}.0");
    let root = modcache
        .join("golang.org")
        .join(format!("toolchain@v0.0.1-go{newer}.{goos}-{goarch}"));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("VERSION"), format!("go{newer}\n")).unwrap();
    fake_indexer(
        &root.join("bin"),
        "go",
        &format!("echo \"go version go{newer} {goos}/{goarch}\""),
    );
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    // run A rewrites the admitted go (same path, new content); run B must not run it
    let indexer = fake_indexer(
        &fakes_dir,
        "scip-go",
        &format!(
            "{OUT_ARG}\nif [ \"$skip\" = 0 ]; then echo '# replaced after run A' >> \"$GOROOT/bin/go\"; fi\ncp '{}' \"$out\"\nexit 0",
            src.display()
        ),
    );
    let repo_id = trust(&home, &repo);
    let c1 = head(&repo);
    let env = [("GOMODCACHE", modcache.to_str().unwrap())];
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
            "--isolation",
            "off",
        ],
        &env,
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "go_toolchain_changed");
    assert!(
        stderr(&out).contains("go toolchain"),
        "the refusal names the tool:\n{}",
        stderr(&out)
    );
    let logs = repo_dir(&home, &repo_id);
    assert!(
        logs.join(format!("{c1}.go.a.log")).is_file(),
        "run A happened"
    );
    assert!(
        !logs.join(format!("{c1}.go.b.log")).exists(),
        "run B was refused before the spawn"
    );
}

// gofix1-2 (FIX-E): a cached toolchain is admitted as a whole, not by its root: `go` executes
// `pkg/tool/<host>/compile` and the rest of `bin/` from `GOROOT` unchecked, so a toolchain whose
// helper is a symlink into the checkout runs repository code. Such a toolchain is refused (the
// cache holds no admissible toolchain: `go_toolchain_too_old`, the row with the action) before
// any spawn; an otherwise identical benign one is admitted.
#[test]
fn a_cached_toolchain_with_a_helper_linked_into_the_checkout_is_refused() {
    let Some((installed, required)) = go_versions_for_directive_tests() else {
        eprintln!(
            "NOT RUN: a_cached_toolchain_with_a_helper_linked_into_the_checkout_is_refused: no go"
        );
        return;
    };
    let (goos, goarch) = go_os_arch();
    let s = Scratch::new("go-modcache-evil-toolchain");
    let home = s.home();
    let repo = go_repo_with_directive(&s.path().join("repo"), &format!("go {required}"));
    let modcache = s.path().join("modcache");
    let newer = format!("{required}.0");
    let root = modcache
        .join("golang.org")
        .join(format!("toolchain@v0.0.1-go{newer}.{goos}-{goarch}"));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("VERSION"), format!("go{newer}\n")).unwrap();
    fake_indexer(
        &root.join("bin"),
        "go",
        &format!("echo \"go version go{newer} {goos}/{goarch}\""),
    );
    let host_tools = root
        .join("pkg")
        .join("tool")
        .join(format!("{goos}_{goarch}"));
    fake_indexer(&host_tools, "link", "exit 0");
    fake_indexer(&repo, "evil", "exit 0");
    std::os::unix::fs::symlink(repo.join("evil"), host_tools.join("compile")).unwrap();
    let marker = s.path().join("indexer-ran");
    // Probe output remains observable when the namespace view makes the marker path read-only.
    let probe_version = "test-scip-go-probed";
    let indexer = fake_indexer(
        &fakes(&s),
        "scip-go",
        &format!(
            r#"touch '{}' 2>/dev/null || true
if [ "$1" = --version ] || [ "$2" = --help ]; then
  echo '{probe_version}'
fi
exit 0"#,
            marker.display()
        ),
    );
    trust(&home, &repo);
    let env = [("GOMODCACHE", modcache.to_str().unwrap())];
    let expected = format!("go toolchain: {installed}, go.mod requires {required}: ");
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
        ],
        &env,
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&expected),
        "doctor does not show the tainted toolchain as admitted; expected {expected:?} in:\n{}",
        stderr(&out)
    );
    assert_eq!(
        json(&out)["doctor"]["indexers"][0]["version"]["Err"],
        "go_toolchain_too_old",
        "doctor must refuse the toolchain before probing the indexer"
    );
    assert!(
        !marker.exists(),
        "doctor must refuse the toolchain before probing the indexer"
    );
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
            "--isolation",
            "off",
        ],
        &env,
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "go_toolchain_too_old");
    assert!(!marker.exists(), "the indexer was never spawned");
    // the same toolchain with a regular `compile` inside the root is admitted
    fs::remove_file(host_tools.join("compile")).unwrap();
    fake_indexer(&host_tools, "compile", "exit 0");
    let out = kaban_index_env(
        &home,
        &[
            "doctor",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&indexer),
        ],
        &env,
    );
    let row = format!(
        "go toolchain: {newer} (module cache, installed {installed}), go.mod requires {required}"
    );
    assert!(
        stderr(&out).contains(&row),
        "the benign toolchain is admitted; expected {row:?} in:\n{}",
        stderr(&out)
    );
    assert_eq!(
        json(&out)["doctor"]["indexers"][0]["version"]["Ok"],
        probe_version,
        "doctor probes the admitted benign toolchain's indexer"
    );
}

// G7 (GO-FIX-1): a descriptor the kaban-index process inherited without `FD_CLOEXEC` (a
// launcher's `flock(1)` on fd 9, `cargo-slot.sh`) is named for what it is: `status` refuses
// with the class `env` and a line naming the descriptor and the action, not a bare "git could
// not run".
#[test]
fn an_inherited_descriptor_is_named_in_the_refusal() {
    let s = Scratch::new("inherited-fd");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let closes = non_cloexec_fds().iter().fold(String::new(), |mut acc, n| {
        let _ = write!(acc, " {n}>&-");
        acc
    });
    // a clean descriptor table, then fd 9 opened by the launcher without CLOEXEC
    let script = format!("exec{closes}\nexec 9>/dev/null\nexec \"$0\" \"$@\"\n");
    let out = Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_kaban-index"))
        .args(["status", "--repo", repo.to_str().unwrap()])
        .env("KABAN_HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "env");
    assert!(
        stderr(&out).contains(
            "git could not run: the kaban-index process inherited an open file descriptor (fd 9); close it in the caller"
        ),
        "the refusal names the inherited descriptor:\n{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------------------------
// U2 corrections (GM4c): one adversarial test per finding through the binary

// RU2-9 (G1-S5.2 "exit 8 under every policy", §7): a fatal isolation class is never a
// per-language failure that `--partial` may skip. Two languages requested: Go succeeds, the
// TypeScript indexer ends the way the helper's refusal ends (exit 8 and the fixed line, which
// the runner classifies as `isolation_selfcheck_failed` on the namespaces tier: the
// deterministic injection CG4.6 D5 asked for). Exit 8, nothing published, both gated logs.
#[test]
fn partial_never_masks_an_isolation_refusal() {
    let s = Scratch::new("partial-isolation");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    fs::write(repo.join("package.json"), "{\"name\": \"x\"}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "ts marker"]);
    let c = head(&repo);
    let repo_id = trust(&home, &repo);
    if doctor_selected_tier(&home, &repo) != "namespaces" {
        eprintln!(
            "NOT RUN: partial_never_masks_an_isolation_refusal: the namespaces tier is unavailable on this host"
        );
        return;
    }
    let good = good_indexer(&s, &repo);
    let refusing = fake_indexer(
        &fakes(&s),
        "scip-typescript",
        &format!("echo '{SELFCHECK_FAILED_TEXT}' >&2\nexit 8"),
    );
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &indexer_arg(&good),
            "--indexer",
            &format!("typescript={}", refusing.display()),
            "--partial",
        ],
    );
    let report = json(&out);
    eprintln!("report: {report}");
    assert_eq!(code(&out), 8, "{}", stderr(&out));
    assert_eq!(report["class"], "isolation_selfcheck_failed");
    assert_eq!(report["exit"], 8);
    assert!(
        !snapshot_path(&home, &repo_id, &c).exists(),
        "nothing published under an isolation refusal"
    );
    let present = names(&home, &repo_id);
    assert!(present.contains(&format!("{c}.go.a.log")), "{present:?}");
    assert!(
        present.contains(&format!("{c}.typescript.a.log")),
        "{present:?}"
    );
    assert!(
        stderr(&out).contains(SELFCHECK_FAILED_TEXT),
        "{}",
        stderr(&out)
    );
}

// RU2-6 (G1-S5.4, S5.6): the child's output file is opened relative to the owned output
// directory handle with `O_NOFOLLOW`, checked to be a regular file and read with a bound; the
// parent never blocks on a FIFO, never follows a link to a device or to a file outside the
// output directory, and refuses a replaced output directory. Each case is a fixed class with a
// fixed reason and no snapshot; every case completes within the bound.
#[test]
fn child_output_is_read_through_the_output_handle_and_refuses_nonregular_files() {
    let s = Scratch::new("output-handle");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let outside = fakes_dir.join("outside.scip");
    fs::write(
        &outside,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    let outdir = fakes_dir.join("outdir");
    fs::create_dir_all(&outdir).unwrap();
    fs::copy(&outside, outdir.join("index.scip")).unwrap();
    let cases: [(&str, String, &str); 4] = [
        (
            "fifo",
            format!("{OUT_ARG}\nrm -f \"$out\"\nmkfifo \"$out\"\nexit 0"),
            "output_not_regular",
        ),
        (
            "device_link",
            format!("{OUT_ARG}\nln -sf /dev/zero \"$out\"\nexit 0"),
            "output_symlink",
        ),
        (
            "outside_link",
            format!("{OUT_ARG}\nln -sf '{}' \"$out\"\nexit 0", outside.display()),
            "output_symlink",
        ),
        (
            "replaced_dir",
            format!(
                "{OUT_ARG}\nd=$(dirname \"$out\")\nrm -rf \"$d\"\nln -s '{}' \"$d\"\nexit 0",
                outdir.display()
            ),
            "output_dir_replaced",
        ),
    ];
    let mut results = Vec::new();
    for (n, (name, body, _)) in cases.iter().enumerate() {
        let c = commit_more(
            &repo,
            10 + u32::try_from(n).unwrap(),
            "2026-10-26T11:30:19Z",
        );
        let indexer = fake_indexer(&fakes_dir, &format!("scip-go-{name}"), body);
        let started = Instant::now();
        let out = kaban_index_bounded(
            &home,
            s.path(),
            &[
                "index",
                "--repo",
                repo.to_str().unwrap(),
                "--indexer",
                &indexer_arg(&indexer),
                "--isolation",
                "off",
                "--timeout",
                "20s",
            ],
            Duration::from_secs(15),
            Some(2_097_152),
        );
        eprintln!(
            "case {name}: {} after {:?}",
            out.as_ref().map_or_else(
                || "parent did not exit (killed)".to_owned(),
                |o| format!(
                    "exit {:?} stdout {} stderr {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout).trim(),
                    String::from_utf8_lossy(&o.stderr).trim()
                )
            ),
            started.elapsed()
        );
        results.push((name, c, out));
    }
    for ((name, _, reason), (_, c, out)) in cases.iter().zip(results.iter()) {
        let out = out
            .as_ref()
            .unwrap_or_else(|| panic!("case {name}: the parent blocked"));
        assert_eq!(code(out), 6, "case {name}: {}", stderr(out));
        let report = json(out);
        assert_eq!(report["class"], "indexer_failed", "case {name}: {report}");
        assert_eq!(report["reason"], *reason, "case {name}: {report}");
        assert!(
            !snapshot_path(&home, &repo_id, c).exists(),
            "case {name}: nothing published"
        );
    }
}

// RU2-4 (G1-S6, G1-S5.3): git is a pinned trusted executable run with repository-configured
// command execution disabled and an allowlisted environment: a `core.fsmonitor` canary hook
// never runs under `trust`, `index` (which runs `git status`) or `status`; a `git` on the
// caller's `PATH` is never selected.
#[test]
fn git_runs_pinned_and_never_executes_repository_configured_commands() {
    let s = Scratch::new("git-pinned");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let canary = s.path().join("fsmonitor-ran.txt");
    let hook = s.path().join("fsmonitor.sh");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\necho \"fsmonitor ran $*\" >> '{}'\necho /\n",
            canary.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git(&repo, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
    let repo_id = trust(&home, &repo);
    assert!(
        !canary.exists(),
        "trust ran no repository-configured command"
    );
    let good = good_indexer(&s, &repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&good), "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        !canary.exists(),
        "index ran git status without executing the fsmonitor hook: {:?}",
        fs::read_to_string(&canary)
    );
    assert!(snapshot_path(&home, &repo_id, &head(&repo)).exists());
    // a git on the caller's PATH is never the git kaban runs
    let path_git_dir = s.path().join("path-git");
    let path_canary = s.path().join("path-git-ran.txt");
    fake_indexer(
        &path_git_dir,
        "git",
        &format!(
            "echo \"path git ran $*\" >> '{}'\nexit 1",
            path_canary.display()
        ),
    );
    let out = kaban_index_env(
        &home,
        &["status", "--repo", repo.to_str().unwrap()],
        &[("PATH", path_git_dir.to_str().unwrap())],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        !path_canary.exists(),
        "the PATH git never ran: {:?}",
        fs::read_to_string(&path_canary)
    );
    assert!(stderr(&out).contains("trust: trusted"), "{}", stderr(&out));
}

// RU2-2 and RU2-3 (G1-S5.3 pinned tool path, S5.1 view): executable search directories are
// separated from runtime-resource mounts, a toolchain root is admitted only when it is a
// toolchain layout outside `$HOME`, the checkout and the data directory, and a repository-
// controlled tool is refused with a fixed class before anything runs.
#[test]
fn toolchain_roots_are_narrow_and_repository_local_tools_are_refused() {
    let s = Scratch::new("tool-roots");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    // a private `$HOME/bin/go` whose grandparent is `$HOME` itself, dressed as a Go root
    let user_home = s.path().join("user-home");
    fs::create_dir_all(user_home.join("src")).unwrap();
    fs::write(user_home.join("VERSION"), "go1.26\n").unwrap();
    // a private file beside the toolchain (not credential-shaped: the builtin gate would mask
    // a `secret=` line in the log and hide what the fake saw)
    let secret = user_home.join(".private-note");
    fs::write(&secret, "home-marker-bytes\n").unwrap();
    fake_indexer(
        &user_home.join("bin"),
        "go",
        "echo go version go1.26\nexit 0",
    );
    let reader = fake_indexer(
        &fakes_dir,
        "scip-go-reader",
        &format!(
            "{OUT_ARG}\nif [ -r '{secret}' ]; then echo \"probe_result=$(cat '{secret}')\"; else echo probe_result=absent; fi\necho \"go=$(go version 2>/dev/null || echo absent)\"\ncp '{src}' \"$out\"\nexit 0",
            secret = secret.display(),
            src = src.display()
        ),
    );
    let path_env = format!("{}:/usr/bin:/bin", user_home.join("bin").display());
    if doctor_selected_tier(&home, &repo) == "namespaces" {
        let c1 = head(&repo);
        let out = kaban_index_env(
            &home,
            &[
                "index",
                "--repo",
                repo.to_str().unwrap(),
                "--indexer",
                &indexer_arg(&reader),
            ],
            &[("HOME", user_home.to_str().unwrap()), ("PATH", &path_env)],
        );
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        let log =
            fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c1}.go.a.log"))).unwrap();
        eprintln!("inside the view:\n{log}");
        assert!(
            log.contains("probe_result=absent"),
            "nothing above the toolchain's bin directory is in the view:\n{log}"
        );
        assert!(
            log.contains("go=go version go1.26"),
            "the toolchain's bin directory is on the pinned PATH:\n{log}"
        );
        let warnings = json(&out)["warnings"].clone();
        assert!(
            warnings
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w == "toolchain_root_refused"),
            "the refused root is a fixed warning: {warnings}"
        );
    } else {
        eprintln!(
            "NOT RUN: toolchain_roots_are_narrow (view half): the namespaces tier is unavailable on this host"
        );
    }
    // a repository-local toolchain entry on PATH is refused with a fixed class
    fake_indexer(&repo.join("bin"), "go", "echo go version go1.26\nexit 0");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "repo-local go"]);
    let c2 = head(&repo);
    let repo_path = format!("{}:/usr/bin:/bin", repo.join("bin").display());
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&reader),
            "--isolation",
            "off",
        ],
        &[("PATH", &repo_path)],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "toolchain_refused");
    assert!(!snapshot_path(&home, &repo_id, &c2).exists());
    // a repository-controlled indexer, and one under the kaban data directory, are refused
    copying_indexer_named(&repo, "scip-go", &src);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "repo-local indexer"]);
    let c3 = head(&repo);
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &format!("go={}", repo.join("scip-go").display()),
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "indexer_untrusted_location");
    let under_home = copying_indexer_named(&home.join("tools"), "scip-go", &src);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&under_home), "--isolation", "off"],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "indexer_untrusted_location");
    assert!(!snapshot_path(&home, &repo_id, &c3).exists());
}

// RU2-3: the admitted indexer is bound by identity: a candidate replaced between admission
// and spawn (run A's fake replaces itself with a new inode) is refused before run B with a
// fixed class; run A's gated log stays, the replacement never runs, nothing is published.
#[test]
fn indexer_replaced_after_admission_is_refused() {
    let s = Scratch::new("indexer-replaced");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    let replacement = fake_indexer(
        &fakes_dir,
        "replacement",
        &format!(
            "{OUT_ARG}\necho REPLACED_RAN\ncp '{}' \"$out\"\nexit 0",
            src.display()
        ),
    );
    let indexer = fake_indexer(
        &fakes_dir,
        "scip-go",
        &format!(
            "{OUT_ARG}\ncp '{}' \"$out\"\ncp '{}' \"$0.new\" && mv -f \"$0.new\" \"$0\"\nexit 0",
            src.display(),
            replacement.display()
        ),
    );
    let c = head(&repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&indexer), "--isolation", "off"],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "indexer_changed");
    assert!(!snapshot_path(&home, &repo_id, &c).exists());
    assert!(
        repo_dir(&home, &repo_id)
            .join(format!("{c}.go.a.log"))
            .exists(),
        "run A's gated log stays"
    );
    assert!(
        files_containing(&repo_dir(&home, &repo_id), b"REPLACED_RAN").is_empty(),
        "the replacement never ran"
    );
}

// RU2-3 (`doctor`): only an admitted tool is probed, inside the isolation tier when one is
// available, with the production descriptor policy: a repository-controlled candidate's
// `--version` never runs; an admitted fake printing its pid sees pid 1 inside the view.
#[test]
fn doctor_probes_only_admitted_tools_inside_isolation() {
    let s = Scratch::new("doctor-probe");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let canary = s.path().join("doctor-ran.txt");
    fake_indexer(
        &repo,
        "scip-go",
        &format!("echo ran >> '{}'\necho v0\nexit 0", canary.display()),
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "repo-local indexer"]);
    let repo_s = repo.to_str().unwrap();
    let out = kaban_index(
        &home,
        &[
            "doctor",
            "--repo",
            repo_s,
            "--indexer",
            &format!("go={}", repo.join("scip-go").display()),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        !canary.exists(),
        "doctor never executed the repository-controlled candidate: {:?}",
        fs::read_to_string(&canary)
    );
    assert!(
        stderr(&out).contains("indexer: indexer_untrusted_location"),
        "{}",
        stderr(&out)
    );
    let probe = fake_indexer(&fakes(&s), "scip-go", "echo \"v-pid-$$\"\nexit 0");
    let out = kaban_index(
        &home,
        &[
            "doctor",
            "--repo",
            repo_s,
            "--indexer",
            &indexer_arg(&probe),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    let version = report["doctor"]["indexers"][0]["version"]["Ok"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let tier = report["doctor"]["isolation"]["selected"]["Ok"]["tier"]
        .as_str()
        .unwrap()
        .to_owned();
    eprintln!("doctor probe on this host: tier {tier}, version line {version}");
    if tier == "namespaces" {
        assert_eq!(version, "v-pid-1", "the probe ran inside the view (pid 1)");
    } else {
        eprintln!(
            "NOT RUN: doctor_probes_only_admitted_tools_inside_isolation (inside half): no isolating tier"
        );
        assert!(version.starts_with("v-pid-"), "{version}");
    }
}

// RU2-5 (G1-S6, S5.8): the checkout admitted at the start is the one the indexer runs in.
// Run A's fake moves the checkout aside and builds a replacement at the same path holding the
// same `.git` (the identity still revalidates); run B is refused with a fixed class, nothing
// runs in the replacement, nothing is published.
#[test]
fn checkout_replaced_between_revalidation_and_spawn_is_refused() {
    let s = Scratch::new("checkout-replaced");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    let body = format!(
        "{OUT_ARG}\nif [ \"$skip\" = 1 ]; then echo \"marker=$(cat marker.txt 2>/dev/null || echo absent)\"; cp '{src}' \"$out\"; exit 0; fi\ncp '{src}' \"$out\"\nhere=$(pwd -P); parent=$(dirname \"$here\"); name=$(basename \"$here\")\ncd \"$parent\" && mv \"$name\" \"$name.old\" && mkdir \"$name\" && mv \"$name.old/.git\" \"$name/.git\" && cp \"$name.old/go.mod\" \"$name.old/x.go\" \"$name/\" && echo replaced > \"$name/marker.txt\"\nexit 0",
        src = src.display()
    );
    let indexer = fake_indexer(&fakes_dir, "scip-go", &body);
    let c = head(&repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&indexer), "--isolation", "off"],
    );
    eprintln!("report: {}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "checkout_replaced");
    assert!(!snapshot_path(&home, &repo_id, &c).exists());
    assert!(
        files_containing(&repo_dir(&home, &repo_id), b"marker=").is_empty(),
        "run B never ran in the replacement"
    );
}

/// Recover the owner permission on the mode-000 directories a fake left under the run
/// directories, so the scratch can be removed whatever the outcome (test cleanup only).
fn recover_scratch_perms(home: &Path, repo_id: &str) {
    for name in names(home, repo_id) {
        if name.starts_with("run-") {
            let deep = repo_dir(home, repo_id).join(&name).join("tmp").join("deep");
            let _ = fs::set_permissions(&deep, fs::Permissions::from_mode(0o700));
        }
    }
}

// RU2-7 (G1-S5.6, RS2-7): a subtree the child made unlistable (mode 000) is still accounted
// by the quota walk and still removed before success is reported; no raw byte remains.
#[test]
fn unreadable_child_subtree_is_accounted_and_removed() {
    let s = Scratch::new("unreadable-subtree");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    fs::write(
        home.join("config.yaml"),
        "index:\n  quota:\n    file_bytes: 1048576\n",
    )
    .unwrap();
    let small = fake_indexer(
        &fakes_dir,
        "scip-go-small",
        &format!(
            "{OUT_ARG}\nmkdir -p \"$TMPDIR/deep\"\nprintf 'canary-raw-bytes' > \"$TMPDIR/deep/x\"\nchmod 000 \"$TMPDIR/deep\"\ncp '{}' \"$out\"\nexit 0",
            src.display()
        ),
    );
    let c1 = head(&repo);
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&small), "--isolation", "off"],
    );
    let leftovers: Vec<String> = names(&home, &repo_id)
        .into_iter()
        .filter(|n| n.starts_with("run-"))
        .collect();
    recover_scratch_perms(&home, &repo_id);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(snapshot_path(&home, &repo_id, &c1).exists());
    assert!(leftovers.is_empty(), "the scratch is gone: {leftovers:?}");
    // 2 MiB under a mode-000 directory with a 1 MiB file quota: accounted, so the quota kills
    let c2 = commit_more(&repo, 2, "2026-10-26T12:18:53Z");
    let big = fake_indexer(
        &fakes_dir,
        "scip-go-big",
        &format!(
            "{OUT_ARG}\nmkdir -p \"$TMPDIR/deep\"\nhead -c 2097152 /dev/zero > \"$TMPDIR/deep/big\"\nchmod 000 \"$TMPDIR/deep\"\nsleep 0.6\ncp '{}' \"$out\"\nexit 0",
            src.display()
        ),
    );
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&big), "--isolation", "off"],
    );
    let leftovers: Vec<String> = names(&home, &repo_id)
        .into_iter()
        .filter(|n| n.starts_with("run-"))
        .collect();
    recover_scratch_perms(&home, &repo_id);
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "quota_exceeded");
    assert!(!snapshot_path(&home, &repo_id, &c2).exists());
    assert!(leftovers.is_empty(), "the scratch is gone: {leftovers:?}");
}

/// A policy file rejecting `CANARY_*` and the whole token `secret-token-<6 digits>`.
fn canary_token_policy_file(dir: &Path) -> PathBuf {
    let path = dir.join("policy-token.yaml");
    fs::write(
        &path,
        "version: 1\ndefaults: { action: reject }\nextend: { use_builtin: false }\nrules:\n  - { id: canary-cred, class: credential, regex: 'CANARY_[A-Za-z0-9_]+' }\n  - { id: canary-token, class: credential, regex: 'secret-token-[0-9]{6}' }\n",
    )
    .unwrap();
    path
}

fn assert_no_canary(out: &Output, what: &str) {
    let o = String::from_utf8_lossy(&out.stdout);
    let e = stderr(out);
    assert!(!o.contains("CANARY"), "{what}: stdout leaks: {o}");
    assert!(!e.contains("CANARY"), "{what}: stderr leaks: {e}");
    assert!(!o.contains("secret-tok"), "{what}: stdout leaks: {o}");
    assert!(!e.contains("secret-tok"), "{what}: stderr leaks: {e}");
}

// RU2-8 (G1-S8, obs-no-sensitive-data): every route to stdout and stderr renders through one
// final gating pass under the current policy: a canary in a config value (and the overrides
// derived from it), in old snapshot metadata, in an invalid CLI value, in an indexer path shown
// by doctor, and past the version line's truncation point never reaches either channel.
#[test]
fn canaries_in_config_overrides_metadata_and_cli_values_never_reach_output() {
    let s = Scratch::new("gated-output");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let policy = canary_token_policy_file(s.path());
    let p = policy.to_str().unwrap();
    let repo_s = repo.to_str().unwrap();
    // (a) a config value and the override list
    fs::write(
        home.join("config.yaml"),
        "index:\n  container_image: CANARY_IMG\n",
    )
    .unwrap();
    let out = kaban_index(&home, &["--policy", p, "doctor", "--repo", repo_s]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_no_canary(&out, "doctor with a canary config value");
    let good = good_indexer(&s, &repo);
    let out = kaban_index(
        &home,
        &[
            "--policy",
            p,
            "index",
            "--repo",
            repo_s,
            "--indexer",
            &indexer_arg(&good),
            "--isolation",
            "off",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_no_canary(&out, "index report overrides");
    // (b) old snapshot metadata
    let c2 = commit_more(&repo, 2, "2026-10-27T07:24:56Z");
    publish_with(
        &home,
        &repo_id,
        &c2,
        provenance(kaban_codegraph::IsolationTier::None, "CANARY_META"),
        "pv",
    );
    let out = kaban_index(&home, &["--policy", p, "status", "--repo", repo_s]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_no_canary(&out, "status over canary metadata");
    // (c) an invalid CLI value
    let out = kaban_index(
        &home,
        &[
            "--policy",
            p,
            "index",
            "--repo",
            repo_s,
            "--timeout",
            "CANARY_T",
        ],
    );
    assert_ne!(code(&out), 0);
    assert_no_canary(&out, "clap diagnostic");
    // (d) an indexer path shown by doctor
    let canary_dir = fakes_dir.join("CANARY_PATH");
    let in_canary_dir = fake_indexer(&canary_dir, "scip-go", "echo v1\nexit 0");
    let out = kaban_index(
        &home,
        &[
            "--policy",
            p,
            "doctor",
            "--repo",
            repo_s,
            "--indexer",
            &indexer_arg(&in_canary_dir),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_no_canary(&out, "doctor indexer path");
    // (e) a token past the version line's truncation point: the whole line is scanned first
    let long = format!("{}secret-token-123456", "x".repeat(190));
    let ver = fake_indexer(&fakes_dir, "scip-go-ver", &format!("echo '{long}'\nexit 0"));
    let out = kaban_index(
        &home,
        &[
            "--policy",
            p,
            "doctor",
            "--repo",
            repo_s,
            "--indexer",
            &indexer_arg(&ver),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_no_canary(&out, "doctor version line");
}

// RU2-11 (§7, RS-3): a failed git ordering query aborts `gc` with a fixed class and deletes
// nothing (`log.date = bogus` makes `git log` fail while `rev-parse` and `rev-list` succeed).
#[test]
fn gc_aborts_when_the_git_ordering_query_fails() {
    let s = Scratch::new("gc-git-failure");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let c1 = head(&repo);
    let c2 = commit_more(&repo, 2, "2026-10-26T09:04:36Z");
    let c3 = commit_more(&repo, 3, "2026-10-26T09:53:10Z");
    let repo_id = trust(&home, &repo);
    for c in [&c1, &c2, &c3] {
        publish_with(
            &home,
            &repo_id,
            c,
            provenance(kaban_codegraph::IsolationTier::None, "none"),
            "pv",
        );
    }
    git(&repo, &["config", "log.date", "bogus"]);
    let repo_s = repo.to_str().unwrap();
    let out = kaban_index(&home, &["gc", "--repo", repo_s, "--keep", "1"]);
    eprintln!(
        "gc under the injected failure: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["class"], "git_query_failed");
    for c in [&c1, &c2, &c3] {
        assert!(
            snapshot_path(&home, &repo_id, c).exists(),
            "nothing deleted under a failed query: {c}"
        );
    }
    git(&repo, &["config", "--unset", "log.date"]);
    let out = kaban_index(&home, &["gc", "--repo", repo_s, "--keep", "1", "--dry-run"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["gc"]["keep"], serde_json::json!([c3, c2]));
    assert_eq!(report["gc"]["collect"][0][0], c1);
}

// RU2-12 (G1-S5.3, the accepted default): under the namespaces tier an unset `CGO_ENABLED`
// becomes `0` and the effective setting is in the report; an explicit caller value is kept.
#[test]
fn cgo_defaults_to_disabled_under_namespaces_and_is_reported() {
    let s = Scratch::new("cgo-default");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("good.scip");
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    if doctor_selected_tier(&home, &repo) != "namespaces" {
        eprintln!(
            "NOT RUN: cgo_defaults_to_disabled_under_namespaces_and_is_reported: the namespaces tier is unavailable on this host"
        );
        return;
    }
    let env_fake = fake_indexer(
        &fakes_dir,
        "scip-go-env",
        &format!(
            "{OUT_ARG}\nenv | sort\ncp '{}' \"$out\"\nexit 0",
            src.display()
        ),
    );
    let c1 = head(&repo);
    let out = index(&home, &repo, &["--indexer", &indexer_arg(&env_fake)]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let log = fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c1}.go.a.log"))).unwrap();
    assert!(
        log.lines().any(|l| l == "CGO_ENABLED=0"),
        "unset becomes 0 under namespaces:\n{log}"
    );
    assert_eq!(json(&out)["cgo_enabled"], "0");
    let c2 = commit_more(&repo, 2, "2026-10-27T08:07:26Z");
    let out = kaban_index_env(
        &home,
        &[
            "index",
            "--repo",
            repo.to_str().unwrap(),
            "--indexer",
            &indexer_arg(&env_fake),
        ],
        &[("CGO_ENABLED", "1")],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let log = fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c2}.go.a.log"))).unwrap();
    assert!(
        log.lines().any(|l| l == "CGO_ENABLED=1"),
        "an explicit caller value is carried:\n{log}"
    );
    assert_eq!(json(&out)["cgo_enabled"], "1");
}

// RU2-13 (G1-F20, stage1-spec:408): a package-only TypeScript project is invoked with
// `--infer-tsconfig`; a project with `tsconfig.json` is not.
#[test]
fn typescript_invocation_adds_infer_tsconfig_without_a_tsconfig() {
    let s = Scratch::new("infer-tsconfig");
    let home = s.home();
    let dir = s.path().join("tsrepo");
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("package.json"), "{\"name\": \"x\"}\n").unwrap();
    fs::write(dir.join("index.ts"), "export const x = 1;\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "c1"]);
    let repo = fs::canonicalize(&dir).unwrap();
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let src = fakes_dir.join("ts.scip");
    fs::write(
        &src,
        scip_bytes(
            &format!("file://{}", repo.display()),
            "scip-typescript",
            None,
        ),
    )
    .unwrap();
    let fake = fake_indexer(
        &fakes_dir,
        "scip-typescript",
        &format!(
            "{OUT_ARG}\necho \"argv: $*\"\ncp '{}' \"$out\"\nexit 0",
            src.display()
        ),
    );
    let ts_arg = format!("typescript={}", fake.display());
    let c1 = head(&repo);
    let out = index(&home, &repo, &["--indexer", &ts_arg, "--isolation", "off"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let log = fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c1}.typescript.a.log")))
        .unwrap();
    assert!(
        log.contains("--infer-tsconfig"),
        "package-only project: --infer-tsconfig passed:\n{log}"
    );
    fs::write(repo.join("tsconfig.json"), "{}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "tsconfig"]);
    let c2 = head(&repo);
    let out = index(&home, &repo, &["--indexer", &ts_arg, "--isolation", "off"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let log = fs::read_to_string(repo_dir(&home, &repo_id).join(format!("{c2}.typescript.a.log")))
        .unwrap();
    assert!(
        log.contains("argv:") && !log.contains("--infer-tsconfig"),
        "tsconfig present: the flag is not passed:\n{log}"
    );
}

// ---------------------------------------------------------------------------------------------
// R-1: the producer's coverage sidecar lands in the snapshot as typed meta rows

/// A copying fake that also writes `coverage.json` beside its `--output` (the Rust producer's
/// sidecar, `tools/kaban-rust-scip/src/semantic.rs`): `coverage` is the file's text.
fn coverage_indexer(s: &Scratch, repo: &Path, name: &str, coverage: &str) -> PathBuf {
    let dir = fakes(s);
    let src = dir.join(format!("{name}.scip"));
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    let sidecar = dir.join(format!("{name}.coverage.json"));
    fs::write(&sidecar, coverage).unwrap();
    fake_indexer(
        &dir,
        name,
        &format!(
            "{OUT_ARG}\ncp '{}' \"$out\"\ncp '{}' \"$(dirname \"$out\")/coverage.json\"\nexit 0",
            src.display(),
            sidecar.display()
        ),
    )
}

/// r1-3: a coverage sidecar belongs to the invocation that wrote it. The first language's
/// producer writes a valid `coverage.json` beside its output and exits non-zero (skipped under
/// `--partial`); the next language's producer succeeds without one. The published snapshot
/// carries no coverage row and the run does not say `coverage_invalid`: the failed producer's
/// counters are not inherited.
#[test]
fn a_failed_producers_sidecar_is_not_inherited_by_the_next_language() {
    let s = Scratch::new("coverage-sidecar-inherited");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    fs::write(repo.join("package.json"), "{\"name\": \"x\"}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "ts marker"]);
    let c = head(&repo);
    let repo_id = trust(&home, &repo);
    let fakes_dir = fakes(&s);
    let sidecar = fakes_dir.join("failed.coverage.json");
    fs::write(&sidecar, COVERAGE_JSON).unwrap();
    let failing_ts = fake_indexer(
        &fakes_dir,
        "scip-typescript-coverage",
        &format!(
            "{OUT_ARG}\ncp '{}' \"$(dirname \"$out\")/coverage.json\"\nexit 1",
            sidecar.display()
        ),
    );
    let good = good_indexer(&s, &repo);
    let out = index(
        &home,
        &repo,
        &[
            "--indexer",
            &indexer_arg(&good),
            "--indexer",
            &format!("typescript={}", failing_ts.display()),
            "--languages",
            "typescript,go",
            "--isolation",
            "off",
            "--partial",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["languages"], serde_json::json!(["go"]), "{report}");
    assert_eq!(report["failed"]["typescript"], "indexer_failed", "{report}");
    assert!(
        !report["warnings"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("coverage_invalid")),
        "{report}"
    );
    let meta = meta_of(&snapshot_path(&home, &repo_id, &c));
    assert_eq!(meta["languages"], "go");
    assert!(
        !meta.keys().any(|k| k.starts_with("coverage:")),
        "the failed producer's sidecar is not the Go snapshot's coverage: {meta:?}"
    );
}

/// A copying fake whose `coverage.json` is made by `sidecar_cmd` (a shell line that may use
/// `$side`, the sidecar's path beside `--output`), for what a plain file cannot be.
fn odd_sidecar_indexer(s: &Scratch, repo: &Path, name: &str, sidecar_cmd: &str) -> PathBuf {
    let dir = fakes(s);
    let src = dir.join(format!("{name}.scip"));
    fs::write(
        &src,
        scip_bytes(&format!("file://{}", repo.display()), "scip-go", None),
    )
    .unwrap();
    fake_indexer(
        &dir,
        name,
        &format!(
            "{OUT_ARG}\ncp '{}' \"$out\"\nside=\"$(dirname \"$out\")/coverage.json\"\n{sidecar_cmd}\nexit 0",
            src.display()
        ),
    )
}

/// The `coverage:*` rows of a snapshot, and whether the run said `coverage_invalid`.
fn coverage_rows(meta: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    meta.iter()
        .filter(|(k, _)| k.starts_with("coverage:"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn warned_coverage_invalid(report: &Value) -> bool {
    report["warnings"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("coverage_invalid"))
}

/// r1-5, r1-6: the sidecar boundary. What is refused (an unknown key inside a `per_crate` row,
/// a directory or a symlink where the file goes, a negative, fractional or over-`u64` counter)
/// publishes no coverage row with the `coverage_invalid` warning and the run succeeds; the
/// accepted maximum `u64` is published as written. No refused value is echoed.
#[test]
fn sidecar_boundaries_refuse_what_is_not_a_plain_counted_table() {
    let s = Scratch::new("coverage-sidecar-boundaries");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let valid = fakes(&s).join("valid.coverage.json");
    fs::write(&valid, COVERAGE_JSON).unwrap();
    let mut n = 0;
    let mut run = |indexer: &Path| -> (Value, BTreeMap<String, String>, String) {
        n += 1;
        let c = commit_more(&repo, n, &format!("2026-11-{:02}T00:00:00Z", n + 1));
        let out = index(
            &home,
            &repo,
            &["--indexer", &indexer_arg(indexer), "--isolation", "off"],
        );
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        let meta = meta_of(&snapshot_path(&home, &repo_id, &c));
        (json(&out), coverage_rows(&meta), stderr(&out))
    };
    let refused = [
        (
            "nested-unknown-key",
            r#"{"declarations": 5, "per_crate": [{"crate": "a", "declarations": 1, "block_items_unique": 0, "block_item_lines": 0, "path": "/home/someone/secret"}]}"#,
        ),
        ("negative", r#"{"declarations": -1, "targets_skipped": 1}"#),
        ("fractional", r#"{"declarations": 1.5}"#),
        ("over-u64", r#"{"declarations": 18446744073709551616}"#),
    ];
    for (name, text) in refused {
        let indexer = coverage_indexer(&s, &repo, name, text);
        let (report, rows, err) = run(&indexer);
        assert!(warned_coverage_invalid(&report), "{name}: {report}");
        assert!(rows.is_empty(), "{name}: nothing published: {rows:?}");
        assert!(
            !err.contains("secret") && !report.to_string().contains("secret"),
            "{name}: no sidecar value is echoed"
        );
    }
    // a directory, and a symlink to a valid sidecar (followed, it would be accepted)
    let dir = odd_sidecar_indexer(&s, &repo, "dir-sidecar", "mkdir \"$side\"");
    let (report, rows, _) = run(&dir);
    assert!(warned_coverage_invalid(&report), "directory: {report}");
    assert!(rows.is_empty(), "directory: {rows:?}");
    let link = odd_sidecar_indexer(
        &s,
        &repo,
        "link-sidecar",
        &format!("ln -s '{}' \"$side\"", valid.display()),
    );
    let (report, rows, _) = run(&link);
    assert!(warned_coverage_invalid(&report), "symlink: {report}");
    assert!(rows.is_empty(), "symlink: {rows:?}");
    // the accepted maximum is published as written
    let max = coverage_indexer(
        &s,
        &repo,
        "max-sidecar",
        r#"{"declarations": 18446744073709551615, "targets_skipped": 1}"#,
    );
    let (report, rows, _) = run(&max);
    assert!(!warned_coverage_invalid(&report), "{report}");
    assert_eq!(
        rows.get("coverage:declarations").map(String::as_str),
        Some("18446744073709551615"),
        "{rows:?}"
    );
    assert_eq!(
        rows.get("coverage:targets_skipped").map(String::as_str),
        Some("1")
    );
}

/// r1-5: with `index.policy_scan: true` the per-crate table is descriptive text under the meta
/// rule and the counters are operational. A rule hitting a crate name masks the one
/// `coverage:per_crate` value (counted, never echoed), every counter stays as written, and
/// the snapshot is admitted by `discover`. A table longer than the policy's field limit
/// (20,000 characters by default) is masked the same way.
#[test]
fn policy_scan_masks_a_crate_name_hit_in_the_table_and_keeps_the_counters() {
    let s = Scratch::new("coverage-policy-scan");
    let home = s.home();
    fs::write(home.join("config.yaml"), "index:\n  policy_scan: true\n").unwrap();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);
    let policy = canary_policy_file(s.path());
    let mut n = 0;
    let mut run = |indexer: &Path| -> (Value, BTreeMap<String, String>, String, String) {
        n += 1;
        let c = commit_more(&repo, n, &format!("2026-11-{:02}T00:00:00Z", n + 1));
        let out = kaban_index(
            &home,
            &[
                "--policy",
                policy.to_str().unwrap(),
                "index",
                "--repo",
                repo.to_str().unwrap(),
                "--indexer",
                &indexer_arg(indexer),
                "--isolation",
                "off",
            ],
        );
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        let path = snapshot_path(&home, &repo_id, &c);
        let head = kaban_codegraph::HeadInput {
            commit: c.clone(),
            ancestors: Vec::new(),
        };
        let (found, r#ref) =
            kaban_codegraph::discover(path.parent().unwrap(), &head).expect("admitted");
        assert_eq!(found, path);
        assert!(r#ref.exact);
        (json(&out), meta_of(&path), stderr(&out), c)
    };

    // a crate name hitting the reject rule
    let hit = coverage_indexer(
        &s,
        &repo,
        "canary-crate",
        r#"{"declarations": 61569, "targets_skipped": 3, "block_items_unique": 9000,
            "per_crate": [{"crate": "CANARY_CRATE", "declarations": 1, "block_items_unique": 2, "block_item_lines": 3}]}"#,
    );
    let (report, meta, err, _) = run(&hit);
    assert_eq!(report["policy_scan"], true);
    assert!(
        !warned_coverage_invalid(&report),
        "a hit is masking, not refusal: {report}"
    );
    assert!(
        meta["coverage:per_crate"].starts_with("[policy:canary-cred]"),
        "{}",
        meta["coverage:per_crate"]
    );
    assert_eq!(
        meta["coverage:declarations"], "61569",
        "operational, intact"
    );
    assert_eq!(meta["coverage:targets_skipped"], "3");
    assert_eq!(meta["coverage:block_items_unique"], "9000");
    assert_eq!(meta["counts:policy_redacted_meta"], "1", "{meta:?}");
    assert_eq!(report["counts"]["policy_redacted_meta"], 1, "{report}");
    assert!(
        !err.contains("CANARY") && !report.to_string().contains("CANARY"),
        "no crate name is echoed"
    );
    assert!(
        !meta.values().any(|v| v.contains("CANARY")),
        "no secret byte in the snapshot's meta: {meta:?}"
    );

    // a table over the field limit: masked as too long, the counters intact
    let rows: Vec<String> = (0..300)
        .map(|i| {
            format!(
                r#"{{"crate": "crate-{i:04}", "declarations": 100, "block_items_unique": 10, "block_item_lines": 40}}"#
            )
        })
        .collect();
    let long = format!(
        r#"{{"declarations": 7, "per_crate": [{}]}}"#,
        rows.join(", ")
    );
    assert!(long.len() > 20_000, "{} chars", long.len());
    let long_indexer = coverage_indexer(&s, &repo, "long-table", &long);
    let (report, meta, _, _) = run(&long_indexer);
    assert!(!warned_coverage_invalid(&report), "{report}");
    assert!(
        meta["coverage:per_crate"].starts_with("[policy:input-too-long]"),
        "{}",
        meta["coverage:per_crate"]
    );
    assert_eq!(meta["coverage:declarations"], "7");
    assert_eq!(meta["counts:policy_redacted_meta"], "1", "{meta:?}");
}

/// The `coverage.json` the Rust producer writes (every key of its `Coverage` struct).
const COVERAGE_JSON: &str = r#"{
  "targets_skipped": 3, "proc_macros_not_built": 1, "contexts": 277, "declarations": 61569,
  "block_items_skipped": 28242, "block_item_references": 41, "shared_file_contexts_skipped": 352,
  "incomplete_modules": 9, "divergent_bodies": 1, "cfg_divergent_declarations": 4,
  "tokens_without_symbol": 405, "positions_skipped": 2, "scip_bytes": 95184043,
  "block_items_unique": 9000, "block_item_lines": 31000, "derived_impls_generic_skipped": 764,
  "per_crate": [
    {"crate": "kaban-api", "declarations": 100, "block_items_unique": 10, "block_item_lines": 40},
    {"crate": "kaban-index", "declarations": 200, "block_items_unique": 20, "block_item_lines": 80}
  ]
}"#;

/// A run whose indexer leaves no `coverage.json` publishes no `coverage:*` row (Go, TS); a run
/// whose indexer writes one publishes every counter as `coverage:<key>` and the per-crate table
/// as one `coverage:per_crate` JSON value; a sidecar that fails validation (an unknown key) is
/// ignored with the `coverage_invalid` warning, never a refusal (R-1 Task 2).
#[test]
fn coverage_sidecar_lands_as_typed_meta_rows_and_is_absent_without_it() {
    let s = Scratch::new("coverage-sidecar");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    let repo_id = trust(&home, &repo);

    // no sidecar: no rows, no warning
    let good = good_indexer(&s, &repo);
    let good_arg = indexer_arg(&good);
    let out = index(
        &home,
        &repo,
        &["--indexer", &good_arg, "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let meta = meta_of(&snapshot_path(&home, &repo_id, &head(&repo)));
    assert!(
        !meta.keys().any(|k| k.starts_with("coverage:")),
        "no coverage rows without the sidecar: {meta:?}"
    );

    // the sidecar: every counter typed, the table as one JSON value, counts never redacted
    let c2 = commit_more(&repo, 2, "2026-10-26T09:04:36Z");
    let with = coverage_indexer(&s, &repo, "with-coverage", COVERAGE_JSON);
    let with_arg = indexer_arg(&with);
    let out = index(
        &home,
        &repo,
        &["--indexer", &with_arg, "--isolation", "off"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert!(
        !report["warnings"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("coverage_invalid")),
        "{report}"
    );
    let meta = meta_of(&snapshot_path(&home, &repo_id, &c2));
    assert_eq!(meta["coverage:declarations"], "61569", "{meta:?}");
    assert_eq!(meta["coverage:block_items_skipped"], "28242");
    assert_eq!(meta["coverage:block_items_unique"], "9000");
    assert_eq!(meta["coverage:block_item_lines"], "31000");
    assert_eq!(meta["coverage:incomplete_modules"], "9");
    assert_eq!(meta["coverage:targets_skipped"], "3");
    assert_eq!(meta["coverage:scip_bytes"], "95184043");
    assert_eq!(
        meta["coverage:derived_impls_generic_skipped"], "764",
        "the derived-impl gap counter is a key (FIX-G Task 3)"
    );
    let per_crate: Value = serde_json::from_str(&meta["coverage:per_crate"]).unwrap();
    assert_eq!(
        per_crate,
        serde_json::json!([
            {"crate": "kaban-api", "declarations": 100, "block_items_unique": 10, "block_item_lines": 40},
            {"crate": "kaban-index", "declarations": 200, "block_items_unique": 20, "block_item_lines": 80}
        ])
    );
    assert_eq!(
        meta.keys().filter(|k| k.starts_with("coverage:")).count(),
        17,
        "16 counters and the table: {meta:?}"
    );

    // an unknown key: the sidecar is ignored and the run says so
    let c3 = commit_more(&repo, 3, "2026-10-26T09:53:10Z");
    let bad = coverage_indexer(
        &s,
        &repo,
        "bad-coverage",
        r#"{"declarations": 5, "surprise": "/home/someone/secret"}"#,
    );
    let bad_arg = indexer_arg(&bad);
    let out = index(&home, &repo, &["--indexer", &bad_arg, "--isolation", "off"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = json(&out);
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("coverage_invalid")),
        "{report}"
    );
    let meta = meta_of(&snapshot_path(&home, &repo_id, &c3));
    assert!(
        !meta.keys().any(|k| k.starts_with("coverage:")),
        "an invalid sidecar publishes nothing: {meta:?}"
    );
    assert!(
        !stderr(&out).contains("secret") && !report.to_string().contains("secret"),
        "no sidecar value is echoed"
    );
}

/// backlog 22 (PERF-1): `indexer_failed` says how the indexer died. An indexer killed by signal 9 (the
/// host's OOM killer seen from kaban) is reported with the signal, the resources the monitors
/// saw and the "no quota breach" fact, in the JSON report next to `wall_ms_per_indexer` and in
/// one plain stderr line; an indexer that exits non-zero is reported with its code and the log.
#[test]
fn cli_failed_run_says_how_it_died() {
    let s = Scratch::new("how-died");
    let home = s.home();
    let repo = go_repo(&s.path().join("repo"));
    trust(&home, &repo);
    let off = ["--isolation", "off"];
    // killed by signal 9: no exit code, no quota breach, the OOM hint
    let killed = fake_indexer(&fakes(&s), "killed", "kill -9 $$");
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&killed), off[0], off[1]],
    );
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["class"], "indexer_failed");
    let failure = &report["failure_per_indexer"]["go"];
    assert_eq!(failure["signal"], 9, "{report}");
    assert!(failure["exit_code"].is_null(), "{report}");
    assert_eq!(failure["quota_breached"], false, "{report}");
    assert!(failure["wall_ms"].is_number(), "{report}");
    assert!(failure["observed_scratch_bytes"].is_number(), "{report}");
    assert!(
        failure["peak_rss_bytes"].is_number() || failure["peak_rss_bytes"].is_null(),
        "{report}"
    );
    let err = stderr(&out);
    assert!(
        err.contains("go indexer failed: killed by signal 9 after "),
        "{err}"
    );
    assert!(
        err.contains("no quota breach; the host was probably out of memory)"),
        "{err}"
    );
    // exited 101: the code and the gated log's path
    let crashed = fake_indexer(&fakes(&s), "crashed", "echo boom >&2\nexit 101");
    let out = index(
        &home,
        &repo,
        &["--indexer", &indexer_arg(&crashed), off[0], off[1]],
    );
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["class"], "indexer_failed");
    let failure = &report["failure_per_indexer"]["go"];
    assert_eq!(failure["exit_code"], 101, "{report}");
    assert!(failure["signal"].is_null(), "{report}");
    let err = stderr(&out);
    assert!(
        err.contains("go indexer failed: exited with code 101 after "),
        "{err}"
    );
    assert!(
        err.contains(" s (see the gated log ") && err.contains(".go.a.log)"),
        "{err}"
    );
}
