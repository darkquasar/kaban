//! Derived artifact identity is stable, boundary-safe, and policy/source-sensitive.
#![allow(clippy::unwrap_used)]

use kaban_core::{Layer, SourceRev, WorkspaceId, WriteBatch, artifact_key};

#[test]
fn shipped_gazetteer_authority_keeps_legacy_keys_and_protects_off_mode() {
    use kaban_core::{BuiltinProducer, CurrentProducerAuthority, config::EngineConfig};
    let mut config = EngineConfig::default();
    let producer = BuiltinProducer::Gazetteer;
    assert_eq!(producer.producer_id(), "gazetteer@1");
    assert_eq!(producer.extractor_id(), "gazetteer");
    assert_eq!(producer.extractor_version(), "1");
    assert!(CurrentProducerAuthority::from_config(&config, Layer::Gazetteer).is_some());
    assert!(CurrentProducerAuthority::from_config(&config, Layer::Embed).is_none());
    config.link.default = "unknown".into();
    assert!(CurrentProducerAuthority::from_config(&config, Layer::Gazetteer).is_none());
    for version in ["1", "1:set:0", "1:set:42"] {
        assert!(producer.matches_legacy_assertion(Layer::Gazetteer, None, Some(version)));
    }
    for version in ["2", "1:set:", "1:set:-1", "1:set:1x"] {
        assert!(!producer.matches_legacy_assertion(Layer::Gazetteer, None, Some(version)));
    }
    assert!(!producer.matches_legacy_assertion(Layer::Human, None, Some("1")));
    assert!(!producer.matches_legacy_assertion(Layer::Gazetteer, Some("caller"), Some("1")));
}

#[test]
fn artifact_identity_separates_fields_source_layer_and_policy() {
    let source = SourceRev {
        content_hash: "source".into(),
        rev: 7,
    };
    let key = artifact_key(&source, Layer::Gazetteer, "ab", "c", "policy");
    assert_eq!(
        key,
        artifact_key(&source, Layer::Gazetteer, "ab", "c", "policy")
    );
    assert_ne!(
        key,
        artifact_key(&source, Layer::Gazetteer, "a", "bc", "policy")
    );
    assert_ne!(
        key,
        artifact_key(&source, Layer::Gazetteer, "ab", "c", "other-policy")
    );
    assert_ne!(
        key,
        artifact_key(
            &SourceRev {
                rev: 8,
                ..source.clone()
            },
            Layer::Gazetteer,
            "ab",
            "c",
            "policy"
        )
    );
    assert_ne!(
        key,
        artifact_key(&source, Layer::Script, "ab", "c", "policy")
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
