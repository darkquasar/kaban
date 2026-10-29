//! Shared helpers for the `kaban-api` integration suites: a private scratch directory (db + keys)
//! removed on drop (`test-fixture-raii`; the sibling crates' pattern, no `tempfile` dev-dependency)
//! and the request builder several suites used to duplicate.

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::RememberRequest;
use std::path::PathBuf;

/// Private scratch directory `<temp>/<prefix>-<pid>-<name>` (db + keys), removed on drop (also
/// when the test panics).
pub(crate) struct Scratch(pub(crate) PathBuf);
impl Scratch {
    pub(crate) fn new(prefix: &str, name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("{prefix}-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    pub(crate) fn db(&self) -> PathBuf {
        self.0.join("kaban.db")
    }
    pub(crate) fn keys(&self) -> PathBuf {
        self.0.join("keys")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `Fact` remember request with `text` and default everything else.
pub(crate) fn fact(text: &str) -> RememberRequest {
    RememberRequest {
        kind: "Fact".into(),
        text: text.into(),
        ..Default::default()
    }
}
