//! Shared helpers for the `kaban-ingest` integration suites: a scratch tree under the OS temp
//! dir that is removed on drop (`test-fixture-raii`; the sibling crates' pattern, no `tempfile`
//! dev-dependency) and the worker-pool rig in [`pool`].

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

pub(crate) mod pool;
pub(crate) mod real;

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// A fresh scratch tree `<temp>/<prefix>-<pid>-<name>`, removed on drop (also when the test
/// panics). Derefs to its path, so it is used wherever a `&Path` root is expected.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new(prefix: &str, name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}
impl Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}
impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
