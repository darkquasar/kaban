//! `doctor`'s `stale_anchors` check (engine-plan E3.11 Step 6; walkthrough-example Step 6): after
//! a rename the resolved snapshot no longer holds a `CodeRef`'s `symbol`, and the report names
//! that anchor with `code: snapshot_mismatch`. An intact symbol, a `CodeRef` of another
//! workspace and one of another repository are not listed; without a snapshot the check says
//! it cannot inspect and names nothing; the report writes no `changes`, `events` or `audit` row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/cg_fixture.rs"]
mod cg_fixture;
mod common;

use cg_fixture::{AFERO_COMMIT, AFERO_KEY, AFERO_PATH, AFERO_RENAMED_KEY, REPO_ID, Variant};
use common::Scratch;
use kaban_api::{ApiConfig, DoctorCheck, DoctorContext, KabanApi, RememberRequest};
use kaban_codegraph::SnapshotProvider;
use kaban_codegraph::testing::FixedSnapshot;
use kaban_core::{DatabaseFileModes, GraphStore};
use kaban_policy::Policy;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use serde_json::json;
use std::sync::Arc;

const REPO: &str = "kaban";

fn context(s: &Scratch) -> DoctorContext {
    DoctorContext {
        files: DatabaseFileModes {
            db: DatabaseFileModes::OWNER_ONLY,
            wal: None,
            shm: None,
        },
        schema_version: LATEST_SCHEMA_VERSION,
        keys_dir: s.keys(),
        vacuum: false,
        priors: false,
    }
}

/// The service over `store` bound to repository `repo` (head pinned, no git), with `provider`.
fn api(
    s: &Scratch,
    store: &Arc<SqliteStore>,
    repo: &str,
    provider: Option<Arc<dyn SnapshotProvider>>,
) -> KabanApi {
    let api = KabanApi::with_config(
        Arc::clone(store) as Arc<dyn GraphStore>,
        Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap(),
        store.signer(),
        None,
        ApiConfig {
            repo: Some(repo.into()),
            repo_root: Some(s.0.clone()),
            ..Default::default()
        },
    )
    .with_pinned_head(AFERO_COMMIT);
    match provider {
        Some(p) => api.with_snapshot_provider(s.0.join("codegraph").join(REPO_ID), p),
        None => api,
    }
}

/// A `CodeRef` on `symbol` (`label` keeps the texts, and so the dedup keys, apart).
fn coderef(api: &KabanApi, label: &str, symbol: &str, workspace: Option<&str>) -> String {
    api.remember(RememberRequest {
        kind: "CodeRef".into(),
        text: format!("{label} {AFERO_PATH} {symbol}"),
        workspace_id: workspace.map(str::to_owned),
        props: Some(json!({
            "repo": REPO, "commit": AFERO_COMMIT, "tool": "fixture", "relative_path": AFERO_PATH,
            "name_path": "MemMapFs/Create", "kind": "Method", "line": 7, "symbol": symbol,
            "snapshot_commit": AFERO_COMMIT,
        })),
        ..Default::default()
    })
    .unwrap()
    .id
}

fn stale_check(api: &KabanApi, s: &Scratch) -> DoctorCheck {
    api.doctor(&context(s))
        .checks
        .into_iter()
        .find(|c| c.name == "stale_anchors")
        .expect("a stale_anchors check")
}

/// Rows of the tables a write would touch.
fn write_counts(s: &Scratch) -> [i64; 3] {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    ["changes", "events", "audit"].map(|t| {
        conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
            .unwrap()
    })
}

#[test]
fn renamed_symbol_is_listed_stale_and_nothing_else_is() {
    let s = Scratch::new("doctor-snapshot", "renamed");
    let built = cg_fixture::build(&s.0, Variant::AferoRenamed, AFERO_COMMIT);
    let provider: Arc<dyn SnapshotProvider> = Arc::new(FixedSnapshot::new(&built.path));
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let bound = api(&s, &store, REPO, Some(provider));
    let renamed = coderef(&bound, "renamed", AFERO_KEY, None);
    let intact = coderef(&bound, "intact", AFERO_RENAMED_KEY, None);
    let foreign_ws = coderef(&bound, "foreign-ws", AFERO_KEY, Some("elsewhere"));
    let other = api(&s, &store, "other-repo", None);
    let foreign_repo = coderef(&other, "foreign-repo", AFERO_KEY, None);
    let before = write_counts(&s);

    let check = stale_check(&bound, &s);
    assert!(check.ok, "a stale anchor is a fact, not damage: {check:?}");
    assert!(check.detail.starts_with("n: 1;"), "{check:?}");
    assert!(
        check.detail.contains(&format!(
            "{renamed} code: snapshot_mismatch symbol: {AFERO_KEY}"
        )),
        "{check:?}"
    );
    assert!(check.detail.contains(AFERO_COMMIT), "{check:?}");
    for absent in [&intact, &foreign_ws, &foreign_repo] {
        assert!(
            !check.detail.contains(absent.as_str()),
            "{absent}: {check:?}"
        );
    }
    assert_eq!(write_counts(&s), before, "doctor writes nothing");

    // the same records without a resolved snapshot: nothing is called renamed
    let check = stale_check(&api(&s, &store, REPO, None), &s);
    assert!(check.ok, "{check:?}");
    assert_eq!(check.detail, "cannot inspect: no snapshot", "{check:?}");
    assert_eq!(write_counts(&s), before, "doctor writes nothing");
}
