//! Identifier routing and ontology-bound lexical/graph ranking through public recall.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test assertions
mod common;
use common::{created, ctx, nid};
use kaban_core::ontology::meta::{ColumnWeights, RankHints};
use kaban_core::{MemoryStore, NewNode, RecallQuery};
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;

fn ontology_with(
    kind: &str,
    edit: impl FnOnce(&mut kaban_core::ontology::resolve::ResolvedKind),
) -> kaban_core::Ontology {
    let mut ontology = kaban_core::shipped::resolved().unwrap();
    edit(
        ontology
            .kinds
            .iter_mut()
            .find(|(name, _)| kaban_core::stored_name(name) == kind)
            .unwrap()
            .1,
    );
    ontology
}

fn query(text: &str, hops: u8) -> RecallQuery {
    RecallQuery {
        query: text.into(),
        hops,
        ..Default::default()
    }
}

#[test]
fn identifier_router_reports_class_but_natural_language_does_not() {
    let store = SqliteStore::open_memory().unwrap();
    let node = created(&store, "Fact", "SqliteStore::apply_batch ordinary prose");
    let identifier = store.recall(&query("apply_batch", 0)).unwrap();
    assert_eq!(identifier[0].node.id, node.id);
    assert!(
        identifier[0]
            .why
            .iter()
            .any(|why| why == "query_class: identifier")
    );
    let natural = store.recall(&query("ordinary prose", 0)).unwrap();
    assert!(
        !natural[0]
            .why
            .iter()
            .any(|why| why.starts_with("query_class:"))
    );
    assert_eq!(
        serde_json::to_string(&identifier).unwrap(),
        serde_json::to_string(&store.recall(&query("apply_batch", 0)).unwrap()).unwrap()
    );
}

fn routed_identifier(store: &SqliteStore, text: &str) -> bool {
    store.recall(&query(text, 0)).unwrap()[0]
        .why
        .iter()
        .any(|why| why == "query_class: identifier")
}

#[test]
fn sentence_periods_and_ellipses_route_prose_to_natural_language() {
    let store = SqliteStore::open_memory().unwrap();
    let node = created(&store, "Fact", "SqliteStore::apply_batch ordinary prose");
    created(&store, "Fact", "node.value ordinary words");
    for prose in ["ordinary prose.", "ordinary prose...", "ordinary. prose."] {
        assert!(!routed_identifier(&store, prose), "{prose}");
    }
    for identifier in ["SqliteStore::apply_batch.", "apply_batch...", "node.value."] {
        assert!(routed_identifier(&store, identifier), "{identifier}");
    }
    assert_eq!(
        store
            .recall(&query("SqliteStore::apply_batch.", 0))
            .unwrap()[0]
            .node
            .id,
        node.id
    );
}

#[test]
fn identifier_query_doubles_ident_column_weight_before_fusion() {
    let ontology = ontology_with("Fact", |kind| {
        kind.ident_fields = vec!["props.symbol".into()];
        kind.column_weights = Some(ColumnWeights {
            text: Some(1.5),
            summary: Some(0.0),
            tags: Some(0.0),
            ident_terms: Some(1.0),
        });
    });
    let store = SqliteStore::open_memory()
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap();
    store
        .remember(
            NewNode {
                kind: "Fact".into(),
                text: "filler".into(),
                props: Some(serde_json::json!({"symbol":"quokka_foo"})),
                ..Default::default()
            },
            &[],
            &ctx(),
        )
        .unwrap();
    created(&store, "Fact", "quokka filler filler filler filler");
    assert_eq!(
        store.recall(&query("quokka", 0)).unwrap()[0].node.text,
        "quokka filler filler filler filler"
    );
    assert_eq!(
        store.recall(&query("\"quokka\"", 0)).unwrap()[0].node.text,
        "filler"
    );
}

#[test]
fn weights_are_selected_for_each_rows_kind_not_the_first_match() {
    let store = SqliteStore::open_memory().unwrap();
    created(&store, "Convention", "quokka");
    created(&store, "Fact", "quokka");
    let weighted = |fact, convention| {
        let mut ontology = kaban_core::shipped::resolved().unwrap();
        for (name, kind) in &mut ontology.kinds {
            let weight = match kaban_core::stored_name(name).as_str() {
                "Fact" => Some(fact),
                "Convention" => Some(convention),
                _ => None,
            };
            if let Some(weight) = weight {
                kind.column_weights = Some(ColumnWeights {
                    text: Some(weight),
                    summary: None,
                    tags: None,
                    ident_terms: None,
                });
            }
        }
        ontology
    };
    let store = store.with_ontology(Arc::new(weighted(10.0, 1.0))).unwrap();
    assert_eq!(
        store.recall(&query("quokka", 0)).unwrap()[0].node.kind,
        "Fact"
    );
    let store = store.with_ontology(Arc::new(weighted(1.0, 10.0))).unwrap();
    assert_eq!(
        store.recall(&query("quokka", 0)).unwrap()[0].node.kind,
        "Convention"
    );
}

#[test]
fn configured_boost_is_applied_once_to_final_rank() {
    let ontology = ontology_with("Fact", |kind| {
        kind.rank = Some(RankHints {
            boost: Some(2.0),
            expand_rels: vec![],
            expand_depth: Some(0),
        });
    });
    let store = SqliteStore::open_memory()
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap();
    let decision = created(&store, "Decision", "quokka");
    let fact = created(&store, "Fact", "quokka");
    let hits = store.recall(&query("quokka", 0)).unwrap();
    assert_eq!(hits[0].node.id, fact.id);
    assert_eq!(hits[1].node.id, decision.id);
    assert!((hits[0].score - 2.0 * 0.85 / 62.0).abs() < 1e-12);
    assert!((hits[1].score - 1.2 * 0.85 / 61.0).abs() < 1e-12);
}

#[test]
fn column_weights_reorder_equal_length_equal_frequency_rows() {
    let store = SqliteStore::open_memory().unwrap();
    let remember = |text: &str, summary: &str| {
        store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: text.into(),
                    summary: Some(summary.into()),
                    ..Default::default()
                },
                &[],
                &ctx(),
            )
            .unwrap()
    };
    remember("quokka", "filler");
    remember("filler", "quokka");
    let weighted = |text, summary| {
        ontology_with("Fact", |kind| {
            kind.column_weights = Some(ColumnWeights {
                text: Some(text),
                summary: Some(summary),
                tags: Some(0.0),
                ident_terms: Some(0.0),
            });
        })
    };
    let store = store.with_ontology(Arc::new(weighted(10.0, 1.0))).unwrap();
    assert_eq!(
        store.recall(&query("quokka", 0)).unwrap()[0].node.text,
        "quokka"
    );
    let store = store.with_ontology(Arc::new(weighted(1.0, 10.0))).unwrap();
    let hits = store.recall(&query("quokka", 0)).unwrap();
    assert_eq!(hits[0].node.text, "filler");
    // Equal document length and one occurrence: weighted tf10 outranks tf1 under FTS5 BM25.
    assert!((hits[0].score - 0.85 / 61.0).abs() < 1e-12);
    assert_eq!(
        store.recall(&query("Fact", 0)).unwrap().len(),
        0,
        "kind remains a filter-only column"
    );
}

#[test]
fn configured_relations_and_depth_bound_each_seed_path() {
    let ontology = ontology_with("Fact", |kind| {
        kind.rank = Some(RankHints {
            boost: None,
            expand_rels: vec!["RELATES_TO".into()],
            expand_depth: Some(1),
        });
    });
    let store = SqliteStore::open_memory()
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap();
    let a = created(&store, "Fact", "quokka seed");
    let b = created(&store, "Fact", "first neighbour");
    let c = created(&store, "Fact", "second neighbour");
    let d = created(&store, "Fact", "excluded relation");
    for (src, dst, rel) in [
        (&a, &b, "RELATES_TO"),
        (&b, &c, "RELATES_TO"),
        (&a, &d, "DERIVED_FROM"),
    ] {
        store
            .link(&nid(&src.id), &nid(&dst.id), rel, None, None, &ctx())
            .unwrap();
    }
    let ids: Vec<_> = store
        .recall(&query("quokka", 2))
        .unwrap()
        .into_iter()
        .map(|hit| hit.node.id)
        .collect();
    assert_eq!(ids, vec![a.id.clone(), b.id.clone()]);
    let ontology = ontology_with("Fact", |kind| {
        kind.rank = Some(RankHints {
            boost: None,
            expand_rels: vec!["RELATES_TO".into()],
            expand_depth: Some(2),
        });
    });
    let store = store.with_ontology(Arc::new(ontology)).unwrap();
    let ids: Vec<_> = store
        .recall(&query("quokka", 2))
        .unwrap()
        .into_iter()
        .map(|hit| hit.node.id)
        .collect();
    assert!(ids.contains(&c.id));
    assert!(!ids.contains(&d.id));
    assert!(
        !store
            .recall(&query("quokka", 1))
            .unwrap()
            .iter()
            .any(|hit| hit.node.id == c.id)
    );
}

#[test]
fn unhinted_second_hop_keeps_legacy_causal_restriction_and_decay() {
    let store = SqliteStore::open_memory().unwrap();
    let a = created(&store, "Fact", "quokka seed");
    let b = created(&store, "Fact", "first neighbour");
    let c = created(&store, "Fact", "noncausal second neighbour");
    let d = created(&store, "Fact", "causal second neighbour");
    for (src, dst, rel) in [
        (&a, &b, "RELATES_TO"),
        (&b, &c, "RELATES_TO"),
        (&b, &d, "DERIVED_FROM"),
    ] {
        store
            .link(&nid(&src.id), &nid(&dst.id), rel, None, None, &ctx())
            .unwrap();
    }
    let hits = store.recall(&query("quokka", 2)).unwrap();
    assert!(!hits.iter().any(|hit| hit.node.id == c.id));
    let first = hits.iter().find(|hit| hit.node.id == b.id).unwrap();
    let second = hits.iter().find(|hit| hit.node.id == d.id).unwrap();
    assert!((first.score - 0.85 * 0.6 / 61.0).abs() < 1e-12);
    assert!((second.score - 0.85 * 0.6f64.powi(3) / 61.0).abs() < 1e-12);
}

#[test]
fn relation_policy_filters_before_neighbour_limit() {
    let ontology = ontology_with("Fact", |kind| {
        kind.rank = Some(RankHints {
            boost: None,
            expand_rels: vec!["RELATES_TO".into()],
            expand_depth: Some(1),
        });
    });
    let store = SqliteStore::open_memory()
        .unwrap()
        .with_ontology(Arc::new(ontology))
        .unwrap();
    let root = created(&store, "Fact", "quokka seed");
    for i in 0..205 {
        let excluded = created(&store, "Fact", &format!("excluded neighbour {i}"));
        store
            .link(
                &nid(&root.id),
                &nid(&excluded.id),
                "DERIVED_FROM",
                None,
                None,
                &ctx(),
            )
            .unwrap();
    }
    let target = created(&store, "Fact", "wanted neighbour");
    store
        .link(
            &nid(&root.id),
            &nid(&target.id),
            "RELATES_TO",
            None,
            None,
            &ctx(),
        )
        .unwrap();
    let ids: Vec<_> = store
        .recall(&query("quokka", 1))
        .unwrap()
        .into_iter()
        .map(|hit| hit.node.id)
        .collect();
    assert_eq!(ids, vec![root.id, target.id]);
}

#[test]
fn fts5_weights_use_combined_document_length_not_bm25f_field_normalisation() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE probe USING fts5(text,summary);
        INSERT INTO probe VALUES('quokka filler filler filler filler filler filler filler filler filler','filler');
        INSERT INTO probe VALUES('filler','quokka');
        INSERT INTO probe VALUES('filler','filler filler filler filler filler filler filler filler filler filler');").unwrap();
    let scores: Vec<f64> = conn
        .prepare("SELECT bm25(probe,1.0,1.5) FROM probe WHERE probe MATCH 'quokka' ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // N=3, df=2 => FTS5 clamps negative IDF to 1e-6. Total lengths11,2,11 => avg8.
    let fts = |tf: f64, dl: f64| -1e-6 * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * dl / 8.0));
    assert!((scores[0] - fts(1.0, 11.0)).abs() < 1e-15);
    assert!((scores[1] - fts(1.5, 2.0)).abs() < 1e-15);
    // A's text length10 against avg field length4 gives normalized tf=1/(.25+.75*10/4).
    let bm25f_tf = 1.0 / (0.25 + 0.75 * 10.0 / 4.0);
    let bm25f = -1e-6 * bm25f_tf * 2.2 / (bm25f_tf + 1.2);
    assert!(
        (scores[0] - bm25f).abs() > 1e-7,
        "FTS5 weights are not field-normalized BM25F"
    );
}
