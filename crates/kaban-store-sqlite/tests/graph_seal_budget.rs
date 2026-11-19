//! Cold seals account for input-sized scratch before allocation and return their charges.
#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use common::Scratch;
use kaban_core::StoreError;
use kaban_core::query_budget::{Limit, QueryBudget};
use kaban_index_graph::{Attr, EdgeOp, LocalId, NodeOp, OpKind, Records, RelId, build_base};
use kaban_store_sqlite::{SqliteStore, catalog_testing};

fn refuses_intermediates(edges: bool) {
    let s = Scratch::new("graph-seal-budget", &format!("edges-{edges}"));
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_catalog_config(kaban_core::config::CatalogCfg {
            seal_ops: kaban_core::config::SealOps {
                graph: 1,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    let node = |n| NodeOp {
        node: LocalId(n),
        kind: OpKind::Add,
    };
    let base = Records {
        nodes: (0..if edges { 2000 } else { 1 }).map(node).collect(),
        ..Records::default()
    };
    catalog_testing::publish_graph_base(&store, &build_base(&base, 0)).unwrap();
    let pending = if edges {
        Records {
            edges: (0..1000)
                .map(|n| EdgeOp {
                    src: LocalId(n),
                    dst: LocalId(n + 1000),
                    rel: RelId(0),
                    kind: OpKind::Add,
                    attr: Attr::default(),
                })
                .collect(),
            ..Records::default()
        }
    } else {
        Records {
            nodes: (1..1001).map(node).collect(),
            ..Records::default()
        }
    };
    catalog_testing::stage_graph_sediment(&store, &pending.encode()).unwrap();
    assert!(!catalog_testing::graph_cache_holds(&store).unwrap());
    let before = catalog_testing::graph_contents(&store).unwrap();
    let tight = QueryBudget::new(if edges { 200_000 } else { 100_000 }, None);
    let result = catalog_testing::seal_graph(&store, &tight);
    assert!(
        matches!(result, Err(StoreError::BudgetExceeded(e)) if e.limit == Limit::Memory),
        "cold seal must refuse its scratch ceiling, edges={edges}"
    );
    assert_eq!(tight.live_bytes(), 0);
    let report = catalog_testing::compact(
        &store,
        kaban_store_sqlite::CompactBudget {
            memory_bytes: if edges { 200_000 } else { 100_000 },
            deadline: std::time::Duration::from_secs(30),
        },
    )
    .unwrap();
    assert_eq!(report.published, []);
    assert!(report.skipped.iter().any(|s| matches!(s.reason, kaban_store_sqlite::SkipReason::Budget(e) if e.limit == Limit::Memory)));
    assert_eq!(
        catalog_testing::graph_contents(&store).unwrap().extents,
        before.extents
    );
    assert_eq!(
        catalog_testing::graph_contents(&store).unwrap().sediment,
        before.sediment
    );
    let ample = QueryBudget::new(u64::MAX, None);
    assert!(
        catalog_testing::seal_graph(&store, &ample)
            .unwrap()
            .is_some()
    );
    assert_eq!(ample.live_bytes(), 0);
}

#[test]
fn uncached_isolated_node_seal_reserves_scratch_and_releases_charges() {
    refuses_intermediates(false);
}

#[test]
fn uncached_edge_seal_reserves_scratch_and_releases_charges() {
    refuses_intermediates(true);
}
