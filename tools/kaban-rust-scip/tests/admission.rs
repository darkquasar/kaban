//! Public prepared-invocation boundary; invented resources are never executed.

#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
fn apple_path(tool: &str, args: &[&str]) -> PathBuf {
    let output = Command::new(tool)
        .args(args)
        .env_clear()
        .current_dir("/")
        .output()
        .expect("system Apple resource query");
    assert!(output.status.success());
    fs::canonicalize(String::from_utf8(output.stdout).expect("path UTF-8").trim())
        .expect("canonical Apple resource")
}

#[cfg(target_os = "macos")]
#[test]
fn repository_cannot_redirect_the_admitted_sdk() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("repo/.cargo")).expect("config directory");
    for text in [
        "[env]\nSDKROOT = { value = '/invented/sdk', force = true }\n",
        "[env]\nDEVELOPER_DIR = { value = '/invented/developer', force = true }\n",
        "[env]\nTOOLCHAINS = 'invented'\n",
        "[env]\nDYLD_INSERT_LIBRARIES = '/invented/library'\n",
        "[env]\nRUSTC_WRAPPER = '/invented/proxy'\n",
        "[env]\nRUSTC = { value = '/invented/rustc', force = true }\n",
        "[build]\nrustc-wrapper = '/invented/proxy'\n",
        "[target.aarch64-apple-darwin]\nrustflags = ['-C', 'link-arg=-isysroot/invented/sdk']\n",
        "[build]\nrustflags = ['-C', 'link-arg=-fuse-ld=/invented/ld']\n",
    ] {
        fs::write(fixture.root.join("repo/.cargo/config.toml"), text).expect("config");
        fixture.rejects(fixture.invoke(), "redirects the compiler backend");
    }
}

struct Fixture {
    root: PathBuf,
    config: serde_json::Value,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kaban-producer-admission-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("unique fixture directory");
        // Canonical: the admitted linker must equal its own canonical path.
        let root = fs::canonicalize(&root).expect("canonical fixture root");
        for directory in [
            "repo",
            "toolchain/bin",
            "toolchain/libexec",
            "toolchain/lib/rustlib/src/rust/library/core/src",
            "toolchain/lib/rustlib/src/rust/library/std/src",
        ] {
            fs::create_dir_all(root.join(directory)).expect("fixture directory");
        }
        fs::write(root.join("repo/Cargo.toml"), "[workspace]\n").expect("manifest");
        for file in [
            "bin/cargo",
            "bin/rustc",
            "libexec/rust-analyzer-proc-macro-srv",
            "lib/rustlib/src/rust/library/core/src/lib.rs",
            "lib/rustlib/src/rust/library/std/src/lib.rs",
        ] {
            fs::write(
                root.join("toolchain").join(file),
                "invented resource, must never execute",
            )
            .expect("resource");
        }
        let linker = root.join("toolchain/bin/cc");
        fs::write(&linker, "invented linker, must never execute").expect("linker");
        fs::set_permissions(&linker, fs::Permissions::from_mode(0o755)).expect("executable");
        #[cfg(target_os = "macos")]
        let linker = apple_path("/usr/bin/xcrun", &["--find", "clang"]);
        let target = if cfg!(target_os = "macos") {
            "aarch64-apple-darwin"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64-unknown-linux-gnu"
        } else {
            "x86_64-unknown-linux-gnu"
        };
        let config = serde_json::json!({"protocol_version":2,"target":target,"cargo":root.join("toolchain/bin/cargo"),"rustc":root.join("toolchain/bin/rustc"),"sysroot":root.join("toolchain"),"proc_macro_server":root.join("toolchain/libexec/rust-analyzer-proc-macro-srv"),"linker":linker});
        Self { root, config }
    }

    fn invoke(&self) -> Output {
        fs::write(
            self.root.join("prepared.json"),
            serde_json::to_vec(&self.config).expect("JSON"),
        )
        .expect("config");
        self.invoke_config(&self.root.join("prepared.json"))
    }

    fn invoke_config(&self, config: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kaban-rust-scip"))
            .arg("index")
            .arg("--workspace")
            .arg(self.root.join("repo"))
            .arg("--output")
            .arg(self.root.join("index.scip"))
            .arg("--config")
            .arg(config)
            .args(["--threads", "4"])
            .env_clear()
            .env("RUSTC_WRAPPER", self.root.join("repository-wrapper"))
            .env("CARGO", self.root.join("repository-cargo-proxy"))
            .current_dir(&self.root)
            .output()
            .expect("candidate starts")
    }

    fn rejects(&self, output: Output, message: &str) {
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.len() <= 4096);
        let diagnostic = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
        assert!(
            diagnostic.contains(message),
            "expected {message:?}, got {diagnostic:?}"
        );
        assert!(
            !self.root.join("index.scip").exists(),
            "refusal must not create output"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove only owned invented fixture");
    }
}

#[test]
fn unknown_config_keys_are_rejected_before_loading_or_output() {
    let mut fixture = Fixture::new();
    fixture.config["extra_args"] = serde_json::json!(["--online"]);
    fixture.rejects(fixture.invoke(), "invalid prepared configuration");
}

/// DF-2: the consumer admits the content of `.cargo/config[.toml]` and `rust-toolchain[.toml]`
/// and passes the resolved sysroot; the producer runs that sysroot's own `cargo` (never a rustup
/// proxy, `RUSTUP_TOOLCHAIN` unset), so the files are not its concern: admission proceeds to the
/// next refusal in line (the unqualified-exporter diagnostic of a fixture never loaded).
#[test]
fn repository_selector_files_are_not_refused_by_the_producer() {
    for selector in [
        ".cargo/config",
        ".cargo/config.toml",
        "rust-toolchain",
        "rust-toolchain.toml",
    ] {
        let fixture = Fixture::new();
        let path = fixture.root.join("repo").join(selector);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent directory");
        fs::write(path, "not executable configuration").expect("selector");
        fixture.rejects(
            fixture.invoke(),
            "semantic exporter qualification is incomplete",
        );
    }
}

/// hd1-1: the one content of a repository `.cargo/config[.toml]` the producer refuses, mirroring
/// the consumer, is a redirect of the compiler backend for the active target (a forced `[env]`
/// entry on a variable of the pin, rustflags with `-B`/`-C linker=`/`-C link-self-contained`,
/// the active target's `linker`), named in plain words; another target's linker (kaban's own
/// musl one) stays inert and admission proceeds to the next refusal in line.
#[test]
fn repository_cargo_config_that_redirects_the_backend_is_refused_by_name() {
    let target = Fixture::new().config["target"]
        .as_str()
        .expect("target")
        .to_owned();
    for (text, message) in [
        (
            "[env]\nCOMPILER_PATH = { value = \"x\", force = true }\n".to_owned(),
            ".cargo/config.toml: [env] COMPILER_PATH with force = true redirects the compiler backend; remove it to index".to_owned(),
        ),
        (
            format!("[target.{target}]\nrustflags = [\"-C\", \"link-arg=-B/tmp/x/\"]\n"),
            format!(".cargo/config.toml: [target.{target}] rustflags carries -C link-arg=-B/tmp/x/, which redirects the compiler backend; remove it to index"),
        ),
        (
            format!("[target.{target}]\nlinker = \"cc\"\n"),
            format!(".cargo/config.toml: [target.{target}] linker redirects the compiler backend; remove it to index"),
        ),
        (
            "[target.x86_64-unknown-linux-musl]\nlinker = \"musl-gcc\"\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n".to_owned(),
            "semantic exporter qualification is incomplete".to_owned(),
        ),
    ] {
        let fixture = Fixture::new();
        let path = fixture.root.join("repo/.cargo/config.toml");
        fs::create_dir_all(path.parent().expect("parent")).expect(".cargo");
        fs::write(path, text).expect("config");
        fixture.rejects(fixture.invoke(), &message);
    }
}

#[test]
fn supported_input_shape_still_refuses_unqualified_index_without_execution() {
    let fixture = Fixture::new();
    fixture.rejects(
        fixture.invoke(),
        "semantic exporter qualification is incomplete",
    );
}

#[test]
fn inherited_wrappers_cannot_reach_semantic_tools_without_clean_scratch() {
    let fixture = Fixture::new();
    fixture.rejects(fixture.invoke(), "clean scratch environment is required");
}

#[test]
fn internal_dispatch_spelling_cannot_bypass_clean_child_environment() {
    let fixture = Fixture::new();
    fixture.invoke();
    for name in ["home", "cargo-home", "target", "tmp"] {
        fs::create_dir(fixture.root.join(name)).expect("scratch directory");
    }
    fs::write(
        fixture.root.join("cargo-home/config.toml"),
        b"[net]\noffline = true\n",
    )
    .expect("offline config");
    let output = Command::new(env!("CARGO_BIN_EXE_kaban-rust-scip"))
        .arg("--load-prepared")
        .arg("--workspace")
        .arg(fixture.root.join("repo"))
        .arg("--output")
        .arg(fixture.root.join("index.scip"))
        .arg("--config")
        .arg(fixture.root.join("prepared.json"))
        .args(["--threads", "4"])
        .env_clear()
        .env("HOME", fixture.root.join("home"))
        .env("CARGO_HOME", fixture.root.join("cargo-home"))
        .env("CARGO_TARGET_DIR", fixture.root.join("target"))
        .env("TMPDIR", fixture.root.join("tmp"))
        .env("RUSTC_WRAPPER", fixture.root.join("hostile-wrapper"))
        .output()
        .expect("candidate starts");
    fixture.rejects(output, "unclean semantic child environment");
}

#[test]
fn protocol_target_and_required_resources_fail_closed() {
    let mut fixture = Fixture::new();
    let original = fixture.config.clone();
    for (key, value, error) in [
        (
            "protocol_version",
            serde_json::json!(1),
            "unsupported prepared protocol version",
        ),
        (
            "linker",
            serde_json::json!("relative/cc"),
            "linker resource is unavailable",
        ),
        (
            "linker",
            serde_json::json!(fixture.root.join("missing-cc")),
            "linker resource is unavailable",
        ),
        (
            "target",
            serde_json::json!("other-host"),
            "unsupported target",
        ),
        (
            "cargo",
            serde_json::json!("relative/cargo"),
            "cargo resource is unavailable",
        ),
        (
            "rustc",
            serde_json::json!(fixture.root.join("missing-rustc")),
            "rustc resource is unavailable",
        ),
        (
            "proc_macro_server",
            serde_json::json!(fixture.root.join("missing-server")),
            "proc-macro server resource is unavailable",
        ),
        (
            "sysroot",
            serde_json::json!(fixture.root.join("missing-sysroot")),
            "sysroot is unavailable",
        ),
    ] {
        fixture.config = original.clone();
        fixture.config[key] = value;
        fixture.rejects(fixture.invoke(), error);
    }
    fixture.config = original;
    fs::remove_file(
        fixture
            .root
            .join("toolchain/lib/rustlib/src/rust/library/core/src/lib.rs"),
    )
    .expect("remove invented source resource");
    fixture.rejects(
        fixture.invoke(),
        "matching rust-src resources are unavailable",
    );
}

/// The same for an ancestor's Cargo configuration and a dangling selector link: the consumer's
/// `rust_cargo_config_unsupported` / `rust_toolchain_selector_unsupported` checks own them.
#[test]
fn ancestor_configuration_and_dangling_selector_links_are_the_consumers_concern() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join(".cargo")).expect("ancestor cargo directory");
    fs::write(fixture.root.join(".cargo/config.toml"), "[build]\n").expect("ancestor config");
    std::os::unix::fs::symlink("missing-selector", fixture.root.join("repo/rust-toolchain"))
        .expect("dangling fixture selector");
    fixture.rejects(
        fixture.invoke(),
        "semantic exporter qualification is incomplete",
    );
}

#[test]
fn malformed_missing_duplicate_and_oversized_config_are_bounded_errors() {
    let fixture = Fixture::new();
    let config = fixture.root.join("prepared.json");
    for raw in [
        "{".to_owned(),
        "{}".to_owned(),
        "{\"protocol_version\":2,\"protocol_version\":2}".to_owned(),
        format!("{{\"{}\": 1}}", "é".repeat(7000)),
    ] {
        fs::write(&config, raw).expect("invalid config");
        fixture.rejects(
            fixture.invoke_config(&config),
            "invalid prepared configuration",
        );
    }
    fs::write(&config, vec![b' '; 16 * 1024 + 1]).expect("oversized config");
    fixture.rejects(
        fixture.invoke_config(&config),
        "prepared configuration exceeds 16 KiB",
    );
}

#[test]
fn unqualified_index_does_not_overwrite_existing_scratch_output() {
    let fixture = Fixture::new();
    let output = fixture.root.join("index.scip");
    fs::write(&output, "existing scratch content").expect("scratch sentinel");
    let result = fixture.invoke();
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .expect("diagnostic")
            .contains("semantic exporter qualification is incomplete")
    );
    assert_eq!(
        fs::read(&output).expect("preserved scratch output"),
        b"existing scratch content"
    );
}

#[test]
fn non_normalized_paths_and_ambiguous_options_are_refused() {
    let fixture = Fixture::new();
    fixture.invoke(); // Write the valid prepared config for the direct invocations.
    let root = fixture.root.to_str().expect("fixture path");
    for (workspace, last_flag, last_value, expected) in [
        (
            format!("{root}/./repo"),
            "--threads",
            "4",
            "prepared paths must be absolute and normalized",
        ),
        (
            format!("{root}/repo"),
            "--threads",
            "8",
            "threads must be 4",
        ),
        (
            format!("{root}/repo"),
            "--workspace",
            "4",
            "duplicate option",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kaban-rust-scip"))
            .args(["index", "--workspace", &workspace, "--output"])
            .arg(fixture.root.join("index.scip"))
            .arg("--config")
            .arg(fixture.root.join("prepared.json"))
            .args([last_flag, last_value])
            .env_clear()
            .current_dir(&fixture.root)
            .output()
            .expect("candidate starts");
        fixture.rejects(output, expected);
    }
}

/// Contract v2 (RI-3d): a v1 configuration, with or without the `linker` field, is refused
/// before any tool runs; the v2 shape passes admission and reaches the scratch check.
#[test]
fn contract_v1_configurations_are_refused_and_v2_passes_admission() {
    let mut fixture = Fixture::new();
    fixture.rejects(fixture.invoke(), "clean scratch environment is required");
    let v2 = fixture.config.clone();
    fixture.config["protocol_version"] = serde_json::json!(1);
    fixture.rejects(fixture.invoke(), "unsupported prepared protocol version");
    fixture.config = v2;
    fixture
        .config
        .as_object_mut()
        .expect("object")
        .remove("linker");
    fixture.rejects(fixture.invoke(), "invalid prepared configuration");
}

/// The linker cargo is told is admitted like the toolchain resources: an existing regular
/// executable at its canonical path, outside the workspace being indexed. A non-executable
/// file, a symlink to the executable and a compiler inside the checkout are refused.
#[test]
fn linker_must_be_a_canonical_executable_outside_the_workspace() {
    let mut fixture = Fixture::new();
    let original = fixture.config.clone();
    let link = fixture.root.join("toolchain/bin/cc-link");
    std::os::unix::fs::symlink(fixture.root.join("toolchain/bin/cc"), &link).expect("symlink");
    let checkout_cc = fixture.root.join("repo/cc");
    fs::write(&checkout_cc, "checkout compiler, must never execute").expect("checkout cc");
    fs::set_permissions(&checkout_cc, fs::Permissions::from_mode(0o755)).expect("executable");
    for linker in [
        fixture.root.join("toolchain/bin/cargo"), // regular file, not executable
        link,
        checkout_cc,
    ] {
        fixture.config = original.clone();
        fixture.config["linker"] = serde_json::json!(linker);
        fixture.rejects(fixture.invoke(), "linker resource is unavailable");
    }
}

/// The semantic child's fixed environment is exactly the v1 set plus one entry,
/// `CARGO_TARGET_<TRIPLE>_LINKER=<linker>` with the triple from the config's `target`, `CC` and
/// the backend pin `COMPILER_PATH=<linker directory>` (HD-1): with them the direct dispatch
/// passes the environment check (and is then refused at the invented toolchain); without any
/// one of them, or under another triple's name, the environment is unclean.
#[test]
fn child_environment_is_the_v1_set_plus_the_linker_as_target_linker_cc_and_backend_pin() {
    let fixture = Fixture::new();
    fixture.invoke();
    for name in ["home", "cargo-home", "target", "tmp"] {
        fs::create_dir(fixture.root.join(name)).expect("scratch directory");
    }
    fs::write(
        fixture.root.join("cargo-home/config.toml"),
        b"[net]\noffline = true\n",
    )
    .expect("offline config");
    let sysroot = fixture.root.join("toolchain");
    let target = fixture.config["target"].as_str().expect("target");
    let variable = format!(
        "CARGO_TARGET_{}_LINKER",
        target.to_ascii_uppercase().replace('-', "_")
    );
    let dispatch = |extra: &[(&str, &str)]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kaban-rust-scip"));
        command
            .arg("--load-prepared")
            .arg("--workspace")
            .arg(fixture.root.join("repo"))
            .arg("--output")
            .arg(fixture.root.join("index.scip"))
            .arg("--config")
            .arg(fixture.root.join("prepared.json"))
            .args(["--threads", "4"])
            .env_clear()
            .env("CARGO", sysroot.join("bin/cargo"))
            .env("RUSTC", sysroot.join("bin/rustc"))
            .env("HOME", fixture.root.join("home"))
            .env("CARGO_HOME", fixture.root.join("cargo-home"))
            .env("CARGO_TARGET_DIR", fixture.root.join("target"))
            .env("TMPDIR", fixture.root.join("tmp"))
            .env(
                "PATH",
                format!("{}:/tools:/usr/bin:/bin", sysroot.join("bin").display()),
            )
            .env("RUSTDOC", sysroot.join("bin/rustdoc"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_BUILD_JOBS", "2")
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_PROFILE_DEV_DEBUG", "0")
            .env("LANG", "C.UTF-8");
        #[cfg(target_os = "macos")]
        command
            .env(
                "__CF_USER_TEXT_ENCODING",
                format!(
                    "0x{:X}:0x0:0x0",
                    fs::metadata(&fixture.root).expect("fixture owner").uid()
                ),
            )
            .env(
                "SDKROOT",
                apple_path("/usr/bin/xcrun", &["--show-sdk-path"]),
            )
            .env(
                "DEVELOPER_DIR",
                apple_path("/usr/bin/xcode-select", &["-p"]),
            )
            .env(
                "PATH",
                format!(
                    "{}:{}:/tools:/usr/bin:/bin",
                    Path::new(fixture.config["linker"].as_str().expect("linker"))
                        .parent()
                        .expect("compiler bin")
                        .display(),
                    sysroot.join("bin").display()
                ),
            );
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("candidate starts")
    };
    let linker = PathBuf::from(fixture.config["linker"].as_str().expect("linker"));
    let linker = linker.to_str().expect("UTF-8 linker path");
    let linker_dir = Path::new(linker).parent().expect("compiler directory");
    let linker_dir = linker_dir.to_str().expect("UTF-8 linker directory");
    let output = dispatch(&[
        (&variable, linker),
        ("CC", linker),
        ("COMPILER_PATH", linker_dir),
    ]);
    assert!(!output.status.success());
    let diagnostic = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
    assert!(
        !diagnostic.contains("unclean semantic child environment"),
        "the v2 environment is the clean one, got {diagnostic:?}"
    );
    assert!(
        diagnostic.contains("pinned toolchain"),
        "refused at the invented toolchain, got {diagnostic:?}"
    );
    assert!(!fixture.root.join("index.scip").exists());
    fixture.rejects(dispatch(&[]), "unclean semantic child environment");
    // DF-2: `CC` is part of the fixed set (the linker variable alone is the pre-DF-2 shape)
    fixture.rejects(
        dispatch(&[(&variable, linker), ("COMPILER_PATH", linker_dir)]),
        "unclean semantic child environment",
    );
    // HD-1: so is the backend pin (the linker variable and `CC` alone are the pre-HD-1 shape)
    fixture.rejects(
        dispatch(&[(&variable, linker), ("CC", linker)]),
        "unclean semantic child environment",
    );
    fixture.rejects(
        dispatch(&[
            ("CARGO_TARGET_OTHER_HOST_LINKER", linker),
            ("CC", linker),
            ("COMPILER_PATH", linker_dir),
        ]),
        "unclean semantic child environment",
    );
    #[cfg(target_os = "macos")]
    fixture.rejects(
        dispatch(&[
            (&variable, linker),
            ("CC", linker),
            ("COMPILER_PATH", linker_dir),
            ("SDKROOT", "/invented/hostile-sdk"),
        ]),
        "unclean semantic child environment",
    );
}
