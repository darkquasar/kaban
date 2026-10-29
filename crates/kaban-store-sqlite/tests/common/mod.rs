//! Shared helpers for the `kaban-store-sqlite` integration suites: a private scratch directory
//! (db + keys) removed on drop (`test-fixture-raii`; the sibling crates' pattern, no `tempfile`
//! dev-dependency) and the small write helpers several suites used to duplicate.

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

pub(crate) mod orange;
pub(crate) mod replay;

use kaban_core::{
    Actor, MemoryStore, NewNode, Node, NodeId, RememberOutcome, WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
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

/// An agent write context in the default workspace under policy version `pv`.
pub(crate) fn ctx() -> WriteCtx {
    WriteCtx::new("pv", WorkspaceId::default(), Actor::Agent)
}

pub(crate) fn nid(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}

/// Remember a fresh `kind` node with `text` under [`ctx`] and return it (a fresh store never
/// reports an existing node).
pub(crate) fn created(store: &SqliteStore, kind: &str, text: &str) -> Node {
    match store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}
