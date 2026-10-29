//! EN-E7's complete source vocabulary, not the matrix display categories.
#![allow(clippy::unwrap_used)]

use kaban_core::Layer;

#[test]
fn complete_layer_vocabulary_round_trips_and_matches_schema() {
    let expected = [
        "human",
        "v4",
        "reconcile",
        "script",
        "llm",
        "gliner",
        "gazetteer",
        "embed",
        "fts",
        "fuzzy",
    ];
    assert_eq!(
        Layer::ALL
            .iter()
            .map(|layer| layer.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    for (layer, spelling) in Layer::ALL.iter().zip(expected) {
        assert_eq!(Layer::parse(spelling), Some(*layer));
        let encoded = serde_json::to_string(layer).unwrap();
        assert_eq!(serde_json::from_str::<Layer>(&encoded).unwrap(), *layer);
    }
    assert_eq!(Layer::parse("extractors"), None);
    assert!(serde_json::from_str::<Layer>("\"unknown\"").is_err());
    let schema = serde_json::to_value(schemars::schema_for!(Layer)).unwrap();
    let variants: Vec<_> = schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["const"].as_str().unwrap())
        .collect();
    assert_eq!(variants, expected);
}
