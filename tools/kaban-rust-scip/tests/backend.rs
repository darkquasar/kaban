//! The backend chain pin (astra ri4-3bc-1, hd1-2) proven through the real toolchain: cargo
//! links the `shadow-backend` fixture (the `workspace` fixture plus a C file its build script
//! compiles with `-flto` through the admitted `CC`, so that `cc1`, `as`, `collect2`,
//! `lto-wrapper` and `ld` all run) while an `ld`/`as`/`collect2`/`lto-wrapper` that records its
//! execution sits first on the semantic child's `PATH`; no shadow must ever run.

use std::{fs, path::PathBuf, process::Command};

/// A refusal profile must fail even if the caller normally runs optimized Python.
#[cfg(target_os = "macos")]
#[test]
fn native_qualification_rejects_unexpected_success_with_optimized_python() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Scratch::new();
    let producer = fixture.root.join("invented-producer");
    fs::write(&producer, "#!/bin/sh\nexit 0\n").expect("invented producer");
    fs::set_permissions(&producer, fs::Permissions::from_mode(0o700)).expect("executable");
    let toolchain = fixture.root.join("toolchain");
    for name in [
        "bin/cargo",
        "bin/rustc",
        "libexec/rust-analyzer-proc-macro-srv",
    ] {
        let path = toolchain.join(name);
        fs::create_dir_all(path.parent().expect("parent")).expect("tool directory");
        fs::write(&path, "#!/bin/sh\nexit 91\n").expect("invented resource");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("executable");
    }
    let run = fixture.root.join("run");
    fs::create_dir(&run).expect("empty scratch");
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("bash")
        .arg(manifest.join("scripts/qualify-loading.sh"))
        .arg(&producer)
        .arg(&toolchain)
        .arg("metadata-failure")
        .arg(&run)
        .env(
            "KABAN_QUALIFICATION_SOURCE",
            manifest.join("tests/fixtures/metadata-failure"),
        )
        .env("PYTHONOPTIMIZE", "1")
        .output()
        .expect("qualification script starts");
    assert_eq!(
        fs::read_to_string(run.join("producer.exit"))
            .expect("producer actually ran")
            .trim(),
        "0"
    );
    assert!(
        !output.status.success(),
        "a refusal profile accepted success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// The `shadow-backend` profile of `scripts/qualify-loading.sh`, which binds the shadows as
/// `/tools/ld`, `/tools/as`, `/tools/collect2`, `/tools/lto-wrapper` (and the triple-prefixed
/// names) ahead of `/usr/bin`, requires the C compile to have run (`c-compiled.txt`) and fails
/// when the marker a shadow writes appears in the scratch. Without the pin (`COMPILER_PATH`
/// unset) gcc's `PATH` fallback runs the shadow `as` and `ld`; the marker names them.
#[test]
#[ignore = "requires the real pinned toolchain; Linux bwrap or trusted native Darwin qualification"]
fn a_shadow_backend_first_on_the_child_path_never_executes() {
    // Arrange
    let toolchain = std::env::var_os("KABAN_TEST_RUST_SYSROOT")
        .expect("KABAN_TEST_RUST_SYSROOT names the pinned toolchain root");
    let scratch = Scratch::new();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/qualify-loading.sh");

    // Act
    let output = Command::new("bash")
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_kaban-rust-scip"))
        .arg(&toolchain)
        .arg("shadow-backend")
        .arg(&scratch.root)
        .arg("--expect-scip")
        .output()
        .expect("qualification script starts");

    // Assert
    let report = String::from_utf8_lossy(&output.stderr);
    assert!(
        !scratch.root.join("tmp/shadow-backend-ran").exists(),
        "the shadow backend executed: {report}"
    );
    assert!(
        output.status.success(),
        "shadow-backend profile failed: {report}"
    );
    assert!(scratch.root.join("out/index.scip").is_file());
    assert!(
        walk(&scratch.root.join("target")).any(|p| p.ends_with("c-compiled.txt")),
        "the C compile ran inside the semantic child: {report}"
    );
}

/// Every file below `root`, depth first.
fn walk(root: &std::path::Path) -> Box<dyn Iterator<Item = PathBuf>> {
    let Ok(entries) = fs::read_dir(root) else {
        return Box::new(std::iter::empty());
    };
    Box::new(entries.flatten().flat_map(|entry| {
        let path = entry.path();
        if path.is_dir() {
            walk(&path)
        } else {
            Box::new(std::iter::once(path))
        }
    }))
}

/// An empty scratch directory the script fills; removed with the fixture.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("kaban-producer-backend-{}", std::process::id()));
        fs::create_dir(&root).expect("unique scratch directory");
        Self {
            root: fs::canonicalize(&root).expect("canonical scratch root"),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
