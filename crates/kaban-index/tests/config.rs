//! CG4.2 (stage1-plan CG4.2; stage1-spec §7 `doctor`, §10 config keys; OWN-54/SD-11): the
//! `doctor` skeleton through the binary: configuration in effect, `overrides:`, detected
//! languages and indexers through the runner with gated output, no repository path in the
//! output, and the `examples/config.yaml` block loading as the defaults.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-cg42c-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("scratch");
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

/// The numbers of the descriptors this process holds without `FD_CLOEXEC` (above 2): inherited
/// from an ancestor (nextest under `flock(1)` leaks its lock fd); closed in the shell before
/// the binary starts, so the production descriptor policy (`Refuse`) of doctor's probes holds
/// (U2 RU2-3; the CG4.6 D13 pattern of `tests/cli.rs`).
fn non_cloexec_fds() -> Vec<u32> {
    kaban_core::process::inheritable_fds().expect("descriptor observation is required")
}

/// Run `kaban-index doctor <args>` with `KABAN_HOME=<home>` and `PATH=<path_env>`, in a child
/// holding only the standard descriptors.
fn doctor(home: &Path, path_env: &Path, args: &[&str]) -> Output {
    use std::fmt::Write as _;
    let closes = non_cloexec_fds().iter().fold(String::new(), |mut acc, n| {
        let _ = write!(acc, " {n}>&-");
        acc
    });
    let script = format!("exec{closes}\nexec \"$0\" \"$@\"\n");
    Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_kaban-index"))
        .arg("doctor")
        .args(args)
        .env("KABAN_HOME", home)
        .env("PATH", path_env)
        .output()
        .expect("kaban-index runs")
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("JSON report")
}

#[test]
fn doctor_lists_effective_overrides() {
    let s = Scratch::new("overrides");
    let home = s.path().join("home");
    fs::create_dir_all(&home).unwrap();
    // defaults print none
    let out = doctor(&home, Path::new("/nonexistent"), &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("overrides:\n"), "{stderr}");
    let after = stderr.split("overrides:\n").nth(1).unwrap();
    assert!(
        !after.starts_with("  "),
        "no override line under overrides: {after}"
    );
    assert_eq!(
        json(&out)["doctor"]["overrides"].as_array().unwrap().len(),
        0
    );

    // exactly the two changed keys
    fs::write(
        home.join("config.yaml"),
        "index:\n  deadline_secs: 30\n  quota:\n    tmp_bytes: 1\n",
    )
    .unwrap();
    let out = doctor(&home, Path::new("/nonexistent"), &[]);
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr
        .split("overrides:\n")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|l| l.starts_with("  "))
        .collect();
    assert_eq!(
        lines,
        vec!["  index.deadline_secs: 30", "  index.quota.tmp_bytes: 1"]
    );
    let report = json(&out);
    assert_eq!(
        report["doctor"]["overrides"],
        serde_json::json!([
            ["index.deadline_secs", "30"],
            ["index.quota.tmp_bytes", "1"]
        ])
    );
    assert_eq!(report["doctor"]["index"]["deadline_secs"], 30);

    // an invalid file: the field path, never the value, exit 4
    fs::write(home.join("config.yaml"), "index:\n  isolation: sometimes\n").unwrap();
    let out = doctor(&home, Path::new("/nonexistent"), &[]);
    assert_eq!(out.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("index.isolation"), "{stderr}");
    assert!(!stderr.contains("sometimes"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("sometimes"));
}

#[test]
fn examples_config_yaml_loads_as_the_defaults() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/config.yaml");
    let cfg = kaban_core::config::load(&path).unwrap();
    assert_eq!(cfg, kaban_core::config::EngineConfig::default());
    assert_eq!(cfg.effective_overrides(), Vec::new());
}

#[test]
fn doctor_output_contains_no_repository_path_and_probes_indexers_through_the_runner() {
    let s = Scratch::new("doctor");
    let home = s.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let bin = s.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    // a fake scip-go that prints a version with a credential-shaped token and a help text
    // naming --module-version under `index --help` (where scip-go 0.2.7 lists it); a fake
    // scip-typescript that hangs
    let go = bin.join("scip-go");
    fs::write(
        &go,
        "#!/bin/sh\nif [ \"$1\" = index ] && [ \"$2\" = --help ]; then echo 'usage: scip-go index [--module-version <v>]'; exit 0; fi\necho 'scip-go 9.9.9 token=AKIAIOSFODNN7EXAMPLE'\n",
    )
    .unwrap();
    fs::set_permissions(&go, fs::Permissions::from_mode(0o755)).unwrap();
    let ts = bin.join("scip-typescript");
    fs::write(&ts, "#!/bin/sh\nsleep 30\n").unwrap();
    fs::set_permissions(&ts, fs::Permissions::from_mode(0o755)).unwrap();
    let repo = s.path().join("CANARY_REPO_DIR_5d2b");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("go.mod"), "module x\n").unwrap();
    fs::write(repo.join("package.json"), "{}").unwrap();
    // PATH must still hold `sh` for the fake tools
    let path_env = std::env::join_paths([
        bin.clone(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();

    let started = std::time::Instant::now();
    let out = doctor(
        &home,
        Path::new(&path_env),
        &["--repo", repo.to_str().unwrap()],
    );
    let wall = started.elapsed();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wall < std::time::Duration::from_secs(20),
        "the 5 s probe deadline bounds doctor: {wall:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stderr.contains("CANARY_REPO_DIR_5d2b"), "{stderr}");
    assert!(!stdout.contains("CANARY_REPO_DIR_5d2b"), "{stdout}");
    assert!(
        stderr.contains("language: go (markers: go.mod) indexer: found"),
        "{stderr}"
    );
    assert!(
        stderr.contains("language: typescript (markers: package.json) indexer: found"),
        "{stderr}"
    );
    // the version line is gated: the builtin credential rule removes the AWS key id
    assert!(!stderr.contains("AKIAIOSFODNN7EXAMPLE"), "{stderr}");
    assert!(!stdout.contains("AKIAIOSFODNN7EXAMPLE"), "{stdout}");
    assert!(
        stderr.contains("indexer go version: [removed: "),
        "{stderr}"
    );
    assert!(
        stderr.contains("indexer go --module-version: present"),
        "{stderr}"
    );
    assert!(
        stderr.contains("indexer typescript version: (indexer_timeout)"),
        "{stderr}"
    );
    let report = json(&out);
    assert_eq!(
        report["doctor"]["indexers"][1]["version"],
        serde_json::json!({"Err": "indexer_timeout"})
    );
    // a plain directory: detection and the probes still run; identity is the trust check's
    assert_eq!(report["doctor"]["trust"]["class"], "not_a_repository");
    assert_eq!(
        report["doctor"]["indexers"][0]["module_version_flag"],
        "present"
    );

    // scip-go absent: reported absent, nothing probed
    let out = doctor(
        &home,
        Path::new("/nonexistent"),
        &["--repo", repo.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("language: go (markers: go.mod) indexer: indexer_not_found"),
        "{stderr}"
    );
}
