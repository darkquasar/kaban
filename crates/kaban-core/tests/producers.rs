//! PC-16: the complete producer vocabulary round-trips by its stored spelling and matches the
//! schema; the old layer and owner spellings are not producers.
#![allow(clippy::unwrap_used)]

use kaban_core::Producer;

#[test]
fn complete_producer_vocabulary_round_trips_and_matches_schema() {
    let expected = [
        "authored",
        "hook_events",
        "document_ingest",
        "gazetteer",
        "extractor:llm",
        "extractor:gliner",
        "extractor:script",
        "scip_import",
        "git_history",
    ];
    assert_eq!(
        Producer::ALL
            .iter()
            .map(|producer| producer.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    for (producer, spelling) in Producer::ALL.iter().zip(expected) {
        assert_eq!(Producer::parse(spelling), Some(*producer));
        let encoded = serde_json::to_string(producer).unwrap();
        assert_eq!(
            serde_json::from_str::<Producer>(&encoded).unwrap(),
            *producer
        );
    }
    for old in [
        "human",
        "v4",
        "reconcile",
        "llm",
        "embed",
        "fts",
        "kaban-ingest",
    ] {
        assert_eq!(Producer::parse(old), None, "{old}");
    }
    assert!(serde_json::from_str::<Producer>("\"unknown\"").is_err());
    let schema = serde_json::to_value(schemars::schema_for!(Producer)).unwrap();
    let variants: Vec<&str> = schema["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry.as_str().unwrap())
        .collect();
    assert_eq!(variants, expected);
}
