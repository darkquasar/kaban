//! Shared harness of the `kaban hook` black-box tests (core-plan C4.3 to C4.5): a scratch
//! repository (a directory with a `.git` entry, so the workspace binds to its name), a database
//! seeded through `kaban-api` the way the MCP server would, the invented-data envelope fixtures
//! under `tests/fixtures/hooks/<host>/`, and one spawn helper that pipes an envelope into the
//! binary and captures stdout, stderr and the exit status.

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::{AcceptRequest, ApiConfig, Caller, KabanApi, RememberRequest};
use kaban_core::{Actor, WorkspaceId};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The repository directory name every hook test binds as its workspace.
pub(crate) const REPO: &str = "nightjar-service";

/// Private scratch: `<temp>/kaban-hook-<pid>-<name>/` with `kaban.db`, `home/keys` and the
/// fake repository `nightjar-service/.git`; removed on drop.
pub(crate) struct HookScratch(pub(crate) PathBuf);

impl HookScratch {
    pub(crate) fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("kaban-hook-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(REPO).join(".git")).unwrap();
        std::fs::create_dir_all(d.join("home").join("keys")).unwrap();
        Self(d)
    }
    pub(crate) fn db(&self) -> PathBuf {
        self.0.join("kaban.db")
    }
    pub(crate) fn home(&self) -> PathBuf {
        self.0.join("home")
    }
    pub(crate) fn keys(&self) -> PathBuf {
        self.home().join("keys")
    }
    pub(crate) fn repo(&self) -> PathBuf {
        self.0.join(REPO)
    }

    /// Open the service on the scratch database bound to the repository's workspace, as
    /// `kaban serve` started inside the repository would (creates and migrates the database).
    /// Drop it before running the binary so no lock is held across the hook.
    pub(crate) fn api(&self) -> KabanApi {
        let store = Arc::new(SqliteStore::open(&self.db(), &self.keys()).unwrap());
        KabanApi::with_config(
            store.clone(),
            Policy::empty(),
            store.signer(),
            None,
            ApiConfig {
                workspace: WorkspaceId::new(REPO),
                repo_root: Some(self.repo()),
                repo: Some(REPO.to_owned()),
                record_signer_identity: false,
                engine: std::sync::Arc::default(),
            },
        )
    }

    /// A raw read-only connection for assertions on the database bytes; `kaban_body(text)`
    /// decodes a body column (every body is a tagged `BLOB`, spec 05 OWN-M).
    pub(crate) fn raw(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(self.db()).unwrap();
        kaban_store_sqlite::register_body_reader(&conn).unwrap();
        conn
    }

    /// The bytes of the database and its sidecars (a canary must appear nowhere in them).
    pub(crate) fn db_bytes(&self) -> Vec<u8> {
        let mut all = Vec::new();
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.db().into_os_string();
            name.push(suffix);
            if let Ok(bytes) = std::fs::read(PathBuf::from(name)) {
                all.extend(bytes);
            }
        }
        all
    }
}

impl Drop for HookScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        host: None,
        session: None,
    }
}

/// An accepted Decision governing `scope`, with `importance`; returns its id.
pub(crate) fn accepted_decision(
    api: &KabanApi,
    text: &str,
    scope: &[&str],
    importance: Option<f64>,
) -> String {
    let id = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: text.into(),
            props: Some(json!({"rationale": "because the team agreed"})),
            importance,
            ..Default::default()
        })
        .unwrap()
        .id;
    api.accept(
        AcceptRequest {
            id: id.clone(),
            reason: Some("reviewed".into()),
            path_scope: scope.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        },
        &human(),
    )
    .unwrap();
    id
}

/// Load `tests/fixtures/hooks/<host>/<name>.json`.
pub(crate) fn fixture(host: &str, name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(host)
        .join(format!("{name}.json"));
    serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

/// Set `pointer` (a JSON pointer) in `v`.
pub(crate) fn set(v: &mut Value, pointer: &str, to: Value) {
    *v.pointer_mut(pointer).unwrap() = to;
}

/// What a hook run produced.
pub(crate) struct Run {
    pub(crate) output: Output,
    pub(crate) elapsed: Duration,
}

impl Run {
    pub(crate) fn code(&self) -> Option<i32> {
        self.output.status.code()
    }
    pub(crate) fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }
    pub(crate) fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }
    /// stdout parsed as exactly one JSON value.
    pub(crate) fn json(&self) -> Value {
        let text = self.stdout();
        serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}: {text:?}"))
    }
}

/// `kaban --db <db> hook <event> [--host <host>] --stdin-json` run from inside the repository
/// with the envelope on stdin and `KABAN_HOME` on the scratch.
pub(crate) fn hook(s: &HookScratch, event: &str, host: Option<&str>, envelope: &Value) -> Run {
    hook_with_env(s, event, host, envelope, &[])
}

/// [`hook`] with extra environment variables.
pub(crate) fn hook_with_env(
    s: &HookScratch,
    event: &str,
    host: Option<&str>,
    envelope: &Value,
    env: &[(&str, &str)],
) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .env_remove("KABAN_WORKSPACE")
        .current_dir(s.repo())
        .args(["--db", s.db().to_str().unwrap(), "hook", event]);
    if let Some(host) = host {
        cmd.args(["--host", host]);
    }
    cmd.arg("--stdin-json");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let started = Instant::now();
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(serde_json::to_string(envelope).unwrap().as_bytes());
    }
    let output = child.wait_with_output().unwrap();
    Run {
        output,
        elapsed: started.elapsed(),
    }
}

/// `session_edits` rows `(kind, path)` of a session, oldest first.
pub(crate) fn session_rows(s: &HookScratch, session_id: &str) -> Vec<(String, String)> {
    let conn = s.raw();
    let mut st = conn
        .prepare("SELECT kind, path FROM session_edits WHERE session_id = ? ORDER BY ts, rowid")
        .unwrap();
    st.query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
