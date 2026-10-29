//! BR-7 (E2.10 boundary review; E2.11): the startup seam that hands a validated `ErPriors`
//! to the store. The thresholds a caller froze from `EngineConfig.er` decide the pass, not the
//! `[er]` defaults; an invalid section never reaches the store because `ErPriors::from_config`
//! refuses it first with the error category and the field path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, created};
use kaban_core::config::Er;
use kaban_core::{
    Actor, Assertion, BatchAssertion, BatchSubject, ClaimClass, ErConfigError, ErPriors, Fixed,
    IngestStore, Layer, Method, NewNode, NodeId, Ontology, Op, Rank, ReconcileCtx, Reconciler,
    Verdict, WorkspaceId, WriteBatch, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

fn twins(store: &SqliteStore) {
    created(
        store,
        "Fact",
        "Saffron ledger reconciliation\nfirst body line",
    );
    created(
        store,
        "Fact",
        "Saffron ledger reconciliation\nsecond body line",
    );
}

#[test]
fn the_er_priors_a_caller_froze_decide_the_pass_instead_of_the_defaults() {
    // defaults: equal title and kind (`W = 81549`) clear `match_threshold 8.0`
    let s = Scratch::new("e211-er-seam", "defaults");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    twins(&store);
    let report = store.er_resolve("default").unwrap();
    assert_eq!((report.matched, report.possible), (1, 0), "{report:?}");

    // the same pair under a stricter frozen section is only `Possible`
    let s = Scratch::new("e211-er-seam", "strict");
    let strict = ErPriors::from_config(&Er {
        match_threshold: 9.0,
        ..Er::default()
    })
    .unwrap();
    let store = SqliteStore::open(&s.db(), &s.keys())
        .unwrap()
        .with_er_priors(strict);
    twins(&store);
    let report = store.er_resolve("default").unwrap();
    assert_eq!((report.matched, report.possible), (0, 1), "{report:?}");
}

/// Negative: the section is validated before the seam, with the category and the path a
/// config error reports; the store never sees it.
#[test]
fn an_invalid_er_section_is_refused_before_the_seam_with_its_category_and_path() {
    let mut cfg = Er::default();
    cfg.m.insert("colour".to_owned(), 0.5);
    let err = ErPriors::from_config(&cfg).unwrap_err();
    // the category and the table, never the key (RW4-5)
    assert_eq!(
        err,
        ErConfigError::UnknownField {
            table: kaban_core::er::ErTable::M
        }
    );
    assert_eq!(err.to_string(), "er_unknown_field");
    assert!(!format!("{err:?}").contains("colour"));
    let err = ErPriors::from_config(&Er {
        match_threshold: 0.0,
        ..Er::default()
    })
    .unwrap_err();
    assert!(
        matches!(err, ErConfigError::Threshold { ref path } if path == "match_threshold"),
        "{err:?}"
    );
}

fn shipped() -> Arc<Ontology> {
    Arc::new(kaban_core::shipped::resolved().unwrap())
}

fn strict() -> ErPriors {
    ErPriors::from_config(&Er {
        match_threshold: 9.0,
        ..Er::default()
    })
    .unwrap()
}

/// One order of the store's `with_*` seams.
type Configure = fn(SqliteStore) -> SqliteStore;

/// Red (RW5-1): `with_ontology` swaps the ontology and its priors and keeps the frozen `[er]`
/// section, in either order: the twin pair stays only `Possible` under the stricter section.
#[test]
fn with_ontology_keeps_the_er_priors_a_caller_froze_in_either_order() {
    let orders: [(&str, Configure); 2] = [
        ("er then ontology", |s| {
            s.with_er_priors(strict()).with_ontology(shipped()).unwrap()
        }),
        ("ontology then er", |s| {
            s.with_ontology(shipped()).unwrap().with_er_priors(strict())
        }),
    ];
    for (case, configure) in orders {
        let s = Scratch::new("s19-c2-er-order", case.replace(' ', "-").as_str());
        let store = configure(SqliteStore::open(&s.db(), &s.keys()).unwrap());
        twins(&store);
        let report = store.er_resolve("default").unwrap();
        assert_eq!(
            (report.matched, report.possible),
            (0, 1),
            "{case}: the frozen [er] section decides the pass: {report:?}"
        );
    }
}

/// Rejects every group: an injected decision entry whose effect is visible (no edge).
struct RejectAll;

impl Reconciler for RejectAll {
    fn decide(&self, _group: &[Assertion], _ctx: &ReconcileCtx<'_>) -> Verdict {
        Verdict::Rejected {
            reason: "fixture".into(),
        }
    }
}

/// Red (RW5-1): an injected reconciler survives `with_ontology` (EN-E9: the trait is the only
/// decision entry); the positive control shows the same claim materialises an edge without it.
#[test]
fn with_ontology_keeps_an_injected_reconciler() {
    for (case, inject, edges) in [("injected", true, 0), ("control", false, 1)] {
        let s = Scratch::new("s19-c2-reconciler", case);
        let mut store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        if inject {
            store = store.with_reconciler(Box::new(RejectAll));
        }
        let store = store.with_ontology(shipped()).unwrap();
        let (a, b) = (NodeId::new(), NodeId::new());
        let upsert = |id: &NodeId, text: &str| Op::UpsertNode {
            id: id.clone(),
            node: NewNode {
                kind: "Fact".into(),
                text: text.into(),
                ..Default::default()
            },
        };
        let mut batch = WriteBatch::new(
            WorkspaceId::default(),
            vec![upsert(&a, "node a"), upsert(&b, "node b")],
        );
        batch.assertions = vec![BatchAssertion {
            subject: BatchSubject::Edge {
                src: a.clone(),
                rel: "RELATES_TO".into(),
                dst: b.clone(),
            },
            claim_class: ClaimClass::Edge,
            predicate: None,
            object: None,
            value: serde_json::json!({}),
            layer: Layer::Gliner,
            method: Method::Gliner,
            extractor_id: Some("fixture".into()),
            extractor_version: Some("1".into()),
            source_id: Some("doc-1".into()),
            lineage_id: "g1".into(),
            raw_score: Fixed::new(9000).unwrap(),
            rank: Rank::Normal,
            rank_reason: None,
            calibration_version: 1,
            supersedes: None,
        }];
        store
            .apply_batch(
                batch,
                &WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent),
            )
            .unwrap();
        let conn = rusqlite::Connection::open_with_flags(
            s.db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM edge", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n, edges,
            "{case}: the injected reconciler decides the group"
        );
    }
}
