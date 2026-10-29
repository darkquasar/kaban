//! The backend chain pin (astra ri4-3bc-1, hd1-2) proven through the real toolchain: cargo
//! links the `shadow-backend` fixture (the `workspace` fixture plus a C file its build script
//! compiles with `-flto` through the admitted `CC`, so that `cc1`, `as`, `collect2`,
//! `lto-wrapper` and `ld` all run) while an `ld`/`as`/`collect2`/`lto-wrapper` that records its
//! execution sits first on the semantic child's `PATH`; no shadow must ever run.

use std::{fs, path::PathBuf, process::Command};

/// The `shadow-backend` profile of `scripts/qualify-loading.sh`, which binds the shadows as
/// `/tools/ld`, `/tools/as`, `/tools/collect2`, `/tools/lto-wrapper` (and the triple-prefixed
/// names) ahead of `/usr/bin`, requires the C compile to have run (`c-compiled.txt`) and fails
/// when the marker a shadow writes appears in the scratch. Without the pin (`COMPILER_PATH`
/// unset) gcc's `PATH` fallback runs the shadow `as` and `ld`; the marker names them.
#[test]
#[ignore = "requires the real pinned toolchain (KABAN_TEST_RUST_SYSROOT) and bwrap"]
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
