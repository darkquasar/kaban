//! `doctor`'s `stale_anchors` check (engine-plan E3.11 Step 6; walkthrough-example Step 6): after
//! a rename the resolved snapshot no longer holds a `CodeRef`'s `symbol`, and the report names
//! that anchor with `code: snapshot_mismatch`. An intact symbol, a `CodeRef` of another
//! workspace and one of another repository are not listed; without a snapshot the check says
//! it cannot inspect and names nothing; the report writes no `changes` row.

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
        allocator_adapter: None,
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
fn write_counts(s: &Scratch) -> [i64; 1] {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    ["changes"].map(|t| {
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

/// Real store operations used by doctor and reindex, without the CLI adapter dependency.
struct TestCatalog(Arc<SqliteStore>);

impl kaban_api::CatalogMaintenance for TestCatalog {
    fn compact(&self) -> Result<kaban_api::CompactReport, kaban_core::StoreError> {
        panic!("these tests inspect the store's complete compaction report directly")
    }
    fn recover(&self) -> Result<String, kaban_core::StoreError> {
        Ok(format!("{:?}", self.0.recover_catalog()?))
    }
    fn verify(&self) -> Result<kaban_api::CatalogVerify, kaban_core::StoreError> {
        let found = self.0.verify_catalog()?;
        Ok(kaban_api::CatalogVerify {
            inconsistencies: if found.inconsistencies() == 0 {
                Vec::new()
            } else {
                vec![format!("{found:?}")]
            },
            needs_reimport: found.needs_reimport,
            ..Default::default()
        })
    }
    fn status(&self) -> Result<kaban_api::CatalogStatus, kaban_core::StoreError> {
        Ok(kaban_api::CatalogStatus {
            refused_units: self.0.catalog_status()?.refused_units,
            ..Default::default()
        })
    }
    fn checkpoint_if_due(&self) -> Result<bool, kaban_core::StoreError> {
        Ok(self.0.checkpoint_if_due()?.is_some())
    }
    fn reindex_graph(&self) -> Result<(), kaban_core::StoreError> {
        self.0.reindex_graph()
    }
}

fn catalog_api(store: &Arc<SqliteStore>, catalog: kaban_core::config::CatalogCfg) -> KabanApi {
    KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        ApiConfig {
            engine: Arc::new(kaban_core::config::EngineConfig {
                catalog,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .with_catalog(Arc::new(TestCatalog(store.clone())))
}

#[test]
fn doctor_retains_a_real_three_extent_fold_refusal_until_a_larger_budget_succeeds() {
    use kaban_core::config::{CatalogCfg, SealOps};
    use kaban_core::{Actor, IngestStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch, WriteCtx};
    use kaban_store_sqlite::{SkipReason, WorkKind};

    let s = Scratch::new("doctor-budget", "fold-retry");
    let policy = CatalogCfg {
        seal_ops: SealOps {
            graph: 1,
            ..Default::default()
        },
        fold_depth: 100,
        fold_delta_ratio: 100.0,
        checkpoint_interval_ms: 60_000,
        ..Default::default()
    };
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(policy.clone())
        .unwrap();
    let ids: Vec<_> = (0..300).map(|_| NodeId::new()).collect();
    let mut ops: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: format!("budget node {i}"),
                ..Default::default()
            },
        })
        .collect();
    for source in 0..300 {
        for offset in 1..=200 {
            ops.push(Op::UpsertEdge {
                src: ids[source].clone(),
                dst: ids[(source + offset) % 300].clone(),
                rel: "RELATES_TO".into(),
                props: None,
            });
        }
    }
    let write = |ops| {
        store
            .apply_batch(
                WriteBatch::new(WorkspaceId::default(), ops),
                &WriteCtx::new(WorkspaceId::default(), Actor::Agent),
            )
            .unwrap();
    };
    write(ops);
    store.reindex_graph().unwrap();
    for source in [0, 1] {
        write(vec![Op::UpsertEdge {
            src: ids[source].clone(),
            dst: ids[299].clone(),
            rel: "RELATES_TO".into(),
            props: None,
        }]);
        let sealed = store.compact().unwrap();
        assert!(
            sealed.published.iter().any(|u| u.kind == WorkKind::Seal),
            "{sealed:?}"
        );
    }
    let before = store.catalog_status().unwrap().classes;
    assert_eq!(
        before
            .iter()
            .filter(|c| c.class.starts_with("graph_"))
            .map(|c| c.live)
            .sum::<u64>(),
        3
    );
    let tight = CatalogCfg {
        maintenance_memory_mib: 16,
        fold_depth: 1,
        ..policy
    };
    let store = Arc::new(store.with_catalog_config(tight.clone()).unwrap());
    let api = catalog_api(&store, tight.clone());
    let refused = store.compact().unwrap();
    assert!(
        refused.pass_skipped.is_none(),
        "must reach the fold: {refused:?}"
    );
    assert_eq!(refused.refused_units, 1, "{refused:?}");
    assert!(refused.published.is_empty(), "{refused:?}");
    assert!(refused.skipped.iter().any(|u| u.kind == WorkKind::Fold
        && matches!(u.reason, SkipReason::Budget(e) if e.limit == kaban_core::query_budget::Limit::Memory)), "{refused:?}");
    assert_eq!(store.catalog_status().unwrap().classes, before);
    let check = api
        .doctor(&context(&s))
        .checks
        .into_iter()
        .find(|c| c.name == "catalog_budget")
        .unwrap();
    assert!(!check.ok, "{check:?}");
    assert!(
        check
            .detail
            .contains("1 units refused at the smallest size"),
        "{check:?}"
    );
    assert!(
        check.detail.contains("maintenance_memory_mib 16"),
        "{check:?}"
    );
    drop(api);
    drop(store);

    let ample = CatalogCfg {
        maintenance_memory_mib: 128,
        ..tight
    };
    let store = Arc::new(
        SqliteStore::open(&s.db(), &s.keys())
            .unwrap()
            .with_catalog_config(ample.clone())
            .unwrap(),
    );
    let api = catalog_api(&store, ample);
    let folded = store.compact().unwrap();
    assert!(
        folded.published.iter().any(|u| u.kind == WorkKind::Fold),
        "{folded:?}"
    );
    assert_eq!(store.catalog_status().unwrap().refused_units, 0);
    let check = api
        .doctor(&context(&s))
        .checks
        .into_iter()
        .find(|c| c.name == "catalog_budget")
        .unwrap();
    assert!(check.ok, "{check:?}");
    assert!(
        check.detail.contains("maintenance_memory_mib 128"),
        "{check:?}"
    );
}

#[test]
fn reindex_all_reports_both_classes_after_a_mixed_container_is_lost() {
    use kaban_api::{LinkRequest, ReindexTarget};
    use kaban_core::MemoryStore;
    use kaban_core::config::CatalogCfg;

    let s = Scratch::new("api-reindex", "lost-mixed");
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = catalog_api(&store, CatalogCfg::default());
    let alpha = api.remember(common::fact("zebra alpha")).unwrap().id;
    let beta = api.remember(common::fact("zebra beta")).unwrap().id;
    let link = |from, to| LinkRequest {
        from,
        to,
        rel: "RELATES_TO".into(),
        confidence: None,
        props: None,
    };
    api.link(link(alpha, beta.clone())).unwrap();
    assert!(api.reindex(ReindexTarget::Graph).unwrap().graph_rebuilt);
    assert_eq!(api.reindex(ReindexTarget::Text).unwrap().rows, Some(2));
    let pending = api.remember(common::fact("zebra pending")).unwrap().id;
    api.link(link(beta, pending)).unwrap();
    let rows = rusqlite::Connection::open(s.db()).unwrap();
    assert_eq!(
        rows.query_row(
            "SELECT class FROM cat_extent WHERE kbx_offset = 0",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "graph_base"
    );
    let hits = |store: &SqliteStore| {
        let mut ids: Vec<_> = store
            .recall(&kaban_core::RecallQuery {
                query: "zebra".into(),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|hit| hit.node.id)
            .collect();
        ids.sort();
        ids
    };
    let expected = hits(&store);
    assert_eq!(expected.len(), 3);
    drop(rows);
    drop(api);
    drop(store);
    std::fs::remove_file(s.0.join("kaban.kbx")).unwrap();
    let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
    let api = catalog_api(&store, CatalogCfg::default());
    let report = api.reindex(ReindexTarget::All).unwrap();
    assert_eq!(report.rows, Some(3));
    assert!(report.graph_rebuilt);
    assert_eq!(hits(&store), expected);
    let verified = store.verify_catalog().unwrap();
    assert_eq!(verified.inconsistencies(), 0, "{verified:?}");
}
