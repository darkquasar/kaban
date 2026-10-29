//! Runtime strategy projections consume the loaded registry, not a duplicated kinds table.
#![allow(clippy::unwrap_used)] // test assertions

use kaban_api::OntologyRegistry;
use kaban_ingest::gate_path;
use kaban_ingest::jobs::strategy::{SourceClass, StrategyError, strategy_for};
use kaban_policy::Policy;

#[test]
fn every_advisory_row_uses_loaded_kinds_and_code_is_refused() {
    let registry = OntologyRegistry::new(None).unwrap();
    let path = gate_path(&Policy::empty(), "docs/adr/0001-choice.md").unwrap();
    for class in [
        SourceClass::Prose,
        SourceClass::Adr,
        SourceClass::Logs,
        SourceClass::Chat,
        SourceClass::Config,
        SourceClass::Diff,
        SourceClass::Ticket,
    ] {
        let strategy = registry
            .with_ontology(|ontology| strategy_for(class, "generic", &path, ontology))
            .unwrap();
        assert_eq!(strategy.kinds, ["Decision", "Fact"]);
        assert_eq!(strategy.source_type, "adr");
        assert!(
            !strategy.richer.contains(&"sim"),
            "deferred embeddings must not be advertised"
        );
        assert_ne!(strategy.default_methods, [] as [&str; 0]);
    }
    assert_eq!(
        registry.with_ontology(|ontology| strategy_for(
            SourceClass::Code,
            "generic",
            &path,
            ontology
        )),
        Err(StrategyError::Code)
    );
}

#[test]
fn registry_projection_uses_default_and_refuses_unresolved_kind_mapping() {
    let registry = OntologyRegistry::new(None).unwrap();
    let path = gate_path(&Policy::empty(), "other/a.md").unwrap();
    let strategy = registry
        .with_ontology(|ontology| strategy_for(SourceClass::Prose, "missing", &path, ontology))
        .unwrap();
    assert_eq!(strategy.kinds, ["Fact"]);
    // A local invalid ontology cannot revive missing mapped kinds; no registry mutation.
    let mut ontology = kaban_core::shipped::resolved().unwrap();
    ontology.kinds.clear();
    assert!(strategy_for(SourceClass::Adr, "adr", &path, &ontology).is_err());
}

#[test]
fn strategy_table_has_explicit_retained_layers_and_source_methods() {
    let registry = OntologyRegistry::new(None).unwrap();
    let path = gate_path(&Policy::empty(), "other/a.txt").unwrap();
    for (class, method, layers) in [
        (SourceClass::Prose, "chunks", vec!["L0", "L1_opt_in"]),
        (SourceClass::Adr, "adr_suggestion", vec!["L0", "L1_opt_in"]),
        (SourceClass::Logs, "line_chunks", vec!["L0", "L1_opt_in"]),
        (SourceClass::Chat, "turn_chunks", vec!["L0", "L1_opt_in"]),
        (
            SourceClass::Config,
            "structural_keys",
            vec!["L0", "L1_opt_in"],
        ),
        (SourceClass::Diff, "hunk_chunks", vec!["L0", "L1_opt_in"]),
        (SourceClass::Ticket, "typed_export_fields", vec!["L0"]),
    ] {
        let row = registry
            .with_ontology(|ontology| strategy_for(class, "generic", &path, ontology))
            .unwrap();
        assert_eq!(row.default_layers, layers);
        assert!(row.default_methods.contains(&method));
        assert_eq!(row.kinds, ["Fact"]);
    }
}
