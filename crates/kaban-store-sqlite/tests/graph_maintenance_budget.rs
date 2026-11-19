//! Late maintenance refusal must preserve the published graph and release all claims.
#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use catalog_testing::BudgetWindow;
use common::{Scratch, created};
use kaban_core::StoreError;
use kaban_core::query_budget::{Limit, QueryBudget};
use kaban_store_sqlite::{SqliteStore, catalog_testing};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn rebuild_refuses_cancelled_or_expired_work_at_loading_hashing_and_publication() {
    for window in [
        BudgetWindow::GraphLoading,
        BudgetWindow::GraphHashing,
        BudgetWindow::BeforePublish,
    ] {
        for expired in [false, true] {
            let s = Scratch::new("graph-budget", &format!("{window:?}-{expired}"));
            let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
            created(&store, "Fact", "an isolated graph node");
            catalog_testing::rebuild_graph(&store).unwrap();
            let before = catalog_testing::graph_contents(&store).unwrap();
            let generation = catalog_testing::pin(&store, &QueryBudget::new(u64::MAX, None))
                .unwrap()
                .view()
                .generation();
            catalog_testing::set_budget_hook(
                &store,
                Some(Arc::new(move |at, budget| {
                    if at == window {
                        if expired {
                            *budget = QueryBudget::new(u64::MAX, Some(Duration::ZERO));
                        } else {
                            budget.cancel();
                        }
                    }
                })),
            );
            let result = catalog_testing::rebuild_graph(&store);
            let limit = if expired {
                Limit::Time
            } else {
                Limit::Cancelled
            };
            assert!(
                matches!(result, Err(StoreError::BudgetExceeded(e)) if e.limit == limit),
                "rebuild must refuse at {window:?}: {result:?}"
            );
            let after = catalog_testing::graph_contents(&store).unwrap();
            assert_eq!(before.extents, after.extents);
            assert_eq!(before.sediment, after.sediment);
            assert_eq!(
                catalog_testing::pin(&store, &QueryBudget::new(u64::MAX, None))
                    .unwrap()
                    .view()
                    .generation(),
                generation
            );
            assert_eq!(store.verify_catalog().unwrap().stale_claims, [] as [u64; 0]);
            catalog_testing::set_budget_hook(&store, None);
            catalog_testing::rebuild_graph(&store).unwrap();
            assert_eq!(
                catalog_testing::graph_contents(&store).unwrap().extents,
                before.extents
            );
        }
    }
}

#[test]
fn fold_refuses_cancelled_or_expired_output_and_releases_its_claim() {
    use kaban_core::config::{CatalogCfg, SealOps};
    use kaban_index_graph::{LocalId, NodeOp, OpKind, Records, build_base};
    use kaban_store_sqlite::{SkipReason, WorkKind};
    for expired in [false, true] {
        let s = Scratch::new("graph-budget", &format!("fold-{expired}"));
        let store = SqliteStore::open(&s.db(), &s.keys())
            .unwrap()
            .with_catalog_config(CatalogCfg {
                seal_ops: SealOps {
                    graph: 1,
                    ..SealOps::default()
                },
                fold_depth: 100,
                fold_delta_ratio: 100.0,
                ..CatalogCfg::default()
            })
            .unwrap();
        let records = Records {
            nodes: vec![NodeOp {
                node: LocalId(1),
                kind: OpKind::Add,
            }],
            ..Records::default()
        };
        catalog_testing::publish_graph_base(&store, &build_base(&records, 0)).unwrap();
        catalog_testing::stage_graph_sediment(&store, &records.encode()).unwrap();
        store.compact().unwrap();
        let store = store
            .with_catalog_config(CatalogCfg {
                fold_depth: 1,
                ..CatalogCfg::default()
            })
            .unwrap();
        let before = catalog_testing::graph_contents(&store).unwrap();
        let status = store.catalog_status().unwrap();
        catalog_testing::set_budget_hook(
            &store,
            Some(Arc::new(move |at, budget| {
                if at == BudgetWindow::BeforePublish {
                    if expired {
                        *budget = QueryBudget::new(u64::MAX, Some(Duration::ZERO));
                    } else {
                        budget.cancel();
                    }
                }
            })),
        );
        let result = store.compact().unwrap();
        let limit = if expired {
            Limit::Time
        } else {
            Limit::Cancelled
        };
        assert!(
            result.skipped.iter().any(|s| s.kind == WorkKind::Fold
                && matches!(s.reason, SkipReason::Budget(e) if e.limit == limit)),
            "late refusal must skip the fold: {result:?}"
        );
        assert_eq!(result.published, []);
        assert_eq!(
            catalog_testing::graph_contents(&store).unwrap().extents,
            before.extents
        );
        assert_eq!(store.catalog_status().unwrap().classes, status.classes);
        assert_eq!(store.verify_catalog().unwrap().stale_claims, [] as [u64; 0]);
        catalog_testing::set_budget_hook(&store, None);
        assert_eq!(store.compact().unwrap().published.len(), 1);
    }
}
