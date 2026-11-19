//! Derived artifact identity is stable, boundary-safe, and source/producer-sensitive.
#![allow(clippy::unwrap_used)]

use kaban_core::{ExtractorKind, Producer, SourceRev, WorkspaceId, WriteBatch, artifact_key};

#[test]
fn shipped_gazetteer_descriptor_has_a_stable_identity() {
    use kaban_core::BuiltinProducer;
    let producer = BuiltinProducer::Gazetteer;
    assert_eq!(producer.producer_id(), "gazetteer@1");
    assert_eq!(producer.extractor_id(), "gazetteer");
    assert_eq!(producer.extractor_version(), "1");
}

#[test]
fn artifact_identity_separates_fields_source_and_producer() {
    let source = SourceRev {
        content_hash: "source".into(),
        rev: 7,
    };
    let key = artifact_key(&source, Producer::Gazetteer, "ab", "c");
    assert_eq!(key, artifact_key(&source, Producer::Gazetteer, "ab", "c"));
    assert_ne!(key, artifact_key(&source, Producer::Gazetteer, "a", "bc"));
    assert_ne!(
        key,
        artifact_key(&source, Producer::Gazetteer, "ab", "other-config")
    );
    assert_ne!(
        key,
        artifact_key(
            &SourceRev {
                rev: 8,
                ..source.clone()
            },
            Producer::Gazetteer,
            "ab",
            "c"
        )
    );
    assert_ne!(
        key,
        artifact_key(
            &source,
            Producer::Extractor(ExtractorKind::Script),
            "ab",
            "c"
        )
    );
    assert_eq!(key.as_str().len(), 64);
}

#[test]
fn old_batch_defaults_empty_publications_and_producer_sets() {
    let value = serde_json::to_value(WriteBatch::new(WorkspaceId::default(), vec![])).unwrap();
    assert!(value.get("artifacts").is_none());
    assert!(value.get("gazetteer_claim_sets").is_none());
    let batch: WriteBatch = serde_json::from_value(value).unwrap();
    assert_eq!(batch.artifacts.len(), 0);
    assert_eq!(batch.gazetteer_claim_sets.len(), 0);
}
