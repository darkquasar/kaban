//! A deterministic host-owned extractor: consumes real brief/next data, never runs a model.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use serde_json::{Value, json};

/// Emit genuine canonical `StructuredReturn` JSON; the selected dialect belongs to the brief.
pub(crate) fn decision(brief: &Value, page: &Value, rationale: &str) -> Value {
    assert_eq!(brief["with"], "llm");
    assert_eq!(page["job"], brief["job"]);
    let chunk = &page["chunk"];
    let quote = "never trusted from the client";
    assert!(chunk["text"].as_str().unwrap().contains(quote));
    json!({"job":brief["job"],"records":[{
        "local_id":"discounts", "kind":"Decision",
        "text":"Discounts are computed server-side",
        "confidence":0.95, "props":{"rationale":rationale},
        "path_scope":["src/checkout/**"],
        "evidence":[{"chunk_id":chunk["id"],"quote":quote}]
    }]})
}

pub(crate) fn gliner(brief: &Value, page: &Value) -> Value {
    assert_eq!(brief["with"], "gliner");
    assert_eq!(page["job"], brief["job"]);
    assert!(page["chunk"]["text"].as_str().unwrap().contains("Cart"));
    json!({"job":brief["job"],"records":[{
        "local_id":"checkout", "kind":"Fact",
        "text":"Checkout builds a Cart", "confidence":0.61
    },{
        "local_id":"org", "kind":"x:Org", "text":"Cart", "confidence":0.61
    }]})
}
