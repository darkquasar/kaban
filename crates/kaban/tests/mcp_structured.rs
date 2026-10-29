//! `structuredContent` and `content[0].text` of a tool reply are the same gated value (core-spec
//! §6, S1 "before returning"; core-plan C3.7; M3-16): a stored string that hits a redact rule
//! only at the response gate is masked identically in both, and one that hits a reject rule
//! withholds both (a text-only error). The nodes are seeded through the store with the policy's
//! own version stamp (no startup rescan) under a policy with `scan_on_read: false`, so the
//! response gate is the only defence between the stored bytes and the wire.
//!
//! A redaction may also hit one of kaban's own constants (a `follow_ups[].tool` name, an output
//! enum value, a property name): then the gated value no longer has the declared `outputSchema`
//! shape and the result is withheld rather than sent as malformed structured output (M3 review
//! R3-9, fail closed).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use kaban_core::{Actor, MemoryStore, NewNode, WorkspaceId, WriteCtx};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);

/// Redact-class word of the test policy (the example policy's `old-password`); not a secret.
const REDACT_CANARY: &str = "hunter2";

/// Reject-class phrase of the test policy (the example policy's `codename-nightjar`).
const REJECT_CANARY: &str = "Project Nightjar";

/// The example policy's two user terms, with the read projection switched off.
const POLICY: &str = "version: 1\n\
defaults:\n  action: reject\n  mask: \"[REDACTED:{rule}]\"\n  scan_on_read: false\n\
words:\n  - { term: hunter2, action: redact, id: old-password }\n\
phrases:\n  - { term: \"Project Nightjar\", id: codename-nightjar }\n";

/// Workspace bound by the server and stamped on the seeded nodes.
const WORKSPACE: &str = "det";

/// Write the test policy, seed one Fact with `text` stamped with that policy's version (so the
/// server's startup rescan leaves it alone), and start a server bound to the same workspace.
fn server_with_seeded_text(s: &Scratch, text: &str) -> Server {
    let policy = s.0.join("policy.yaml");
    std::fs::write(&policy, POLICY).unwrap();
    let version = Policy::load(&policy).unwrap().effective_hash().to_owned();
    {
        let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
        let ctx = WriteCtx::new(version, WorkspaceId::new(WORKSPACE), Actor::Agent);
        store
            .remember(
                NewNode {
                    kind: "Fact".into(),
                    text: text.to_owned(),
                    workspace_id: Some(WORKSPACE.into()),
                    ..Default::default()
                },
                &[],
                &ctx,
            )
            .unwrap();
    }
    Server::start_with(
        s,
        &[
            "--policy",
            policy.to_str().unwrap(),
            "--workspace",
            WORKSPACE,
        ],
        DEADLINE,
    )
    .0
}

fn recall(srv: &mut Server, query: &str) -> Value {
    let id = srv.request(
        "tools/call",
        &json!({"name": "recall", "arguments": {"query": query, "full_text": true, "hops": 0}}),
    );
    srv.response(id, DEADLINE).unwrap()["result"].clone()
}

/// A policy with the read projection switched off and one redact rule for `term` (a constant
/// of kaban's own wire vocabulary in these tests; not a secret).
fn redact_policy(term: &str) -> String {
    format!(
        "version: 1\n\
         defaults:\n  action: reject\n  mask: \"[REDACTED:{{rule}}]\"\n  scan_on_read: false\n\
         words:\n  - {{ term: {term}, action: redact, id: own-word }}\n"
    )
}

/// Start a server under `policy`, bound to [`WORKSPACE`].
fn server_with_policy(s: &Scratch, policy: &str) -> Server {
    let path = s.0.join("policy.yaml");
    std::fs::write(&path, policy).unwrap();
    Server::start_with(
        s,
        &["--policy", path.to_str().unwrap(), "--workspace", WORKSPACE],
        DEADLINE,
    )
    .0
}

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    srv.response(id, DEADLINE).unwrap()["result"].clone()
}

/// The result is a withheld, text-only error that names no caller or stored data.
fn assert_withheld(result: &Value) {
    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(
        result.get("structuredContent").is_none(),
        "a withheld result must carry no structuredContent: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("response withheld"), "{result}");
}

/// A successful result whose `structuredContent` still parses as `recall`'s response type is
/// returned; its text and structured forms agree.
fn assert_delivered(result: &Value) -> Value {
    assert_ne!(result["isError"], json!(true), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    let structured = result["structuredContent"].clone();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), structured);
    structured
}

/// R3-9: a redact rule matching `serena` would turn `follow_ups[].tool` (declared as one of two
/// constants) into a mask; the result is withheld instead. The same policy leaves a result the
/// rule does not touch alone.
#[test]
fn a_redaction_of_a_follow_up_tool_name_withholds_the_result_instead_of_breaking_its_schema() {
    let s = Scratch::new("kaban-structured", "reshape-tool");
    let mut srv = server_with_policy(&s, &redact_policy("serena"));
    let coderef = json!({"repo": "kaban", "commit": "8d10f88", "tool": "manual", "relative_path": "src/pool.rs", "name_path": "Pool/open"});
    let stored = call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "the pool opens lazily on first use", "refs": [coderef]}),
    );
    assert_eq!(assert_delivered(&stored)["status"], json!("created"));
    call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "the cache warms on the second request"}),
    );

    // the note with a pointer: its follow-ups would carry a masked tool name
    let id = srv.request(
        "tools/call",
        &json!({"name": "recall", "arguments": {"query": "pool opens lazily", "hops": 1}}),
    );
    let result = srv.response(id, DEADLINE).unwrap()["result"].clone();
    assert_withheld(&result);
    assert!(
        !result.to_string().contains("serena"),
        "the masked constant must not leak either way: {result}"
    );

    // the note without a pointer under the same policy: nothing to mask, delivered as usual
    let plain = recall(&mut srv, "cache warms second request");
    let structured = assert_delivered(&plain);
    assert_eq!(structured["count"], json!(1), "{structured}");
    assert_eq!(structured["follow_ups"], json!([]), "{structured}");
    srv.finish();
}

/// R3-9: a redact rule matching an output enum value (`remember.status` is `created` or
/// `existing`) withholds the result; the write itself happened (the second `remember` of the
/// same text reports `existing`, which the rule does not touch).
#[test]
fn a_redaction_of_an_output_enum_value_withholds_the_result() {
    let s = Scratch::new("kaban-structured", "reshape-enum");
    let mut srv = server_with_policy(&s, &redact_policy("created"));
    let note = json!({"kind": "Fact", "text": "the index is rebuilt after a purge"});
    assert_withheld(&call(&mut srv, "remember", &note));
    let again = assert_delivered(&call(&mut srv, "remember", &note));
    assert_eq!(again["status"], json!("existing"), "{again}");
    srv.finish();
}

/// R3-9: a redact rule matching a required property name (`recall.results`) withholds the
/// result; a tool whose response has no such property is unaffected.
#[test]
fn a_redaction_of_a_required_property_name_withholds_the_result() {
    let s = Scratch::new("kaban-structured", "reshape-key");
    let mut srv = server_with_policy(&s, &redact_policy("results"));
    let stored = assert_delivered(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Fact", "text": "the changelog is pruned nightly"}),
    ));
    assert_eq!(stored["status"], json!("created"), "{stored}");
    let result = recall(&mut srv, "changelog pruned nightly");
    assert_withheld(&result);
    assert!(
        !result.to_string().contains("REDACTED"),
        "no masked key reaches the wire: {result}"
    );
    srv.finish();
}

#[test]
fn a_redact_hit_at_the_response_gate_is_masked_identically_in_text_and_structured_content() {
    let s = Scratch::new("kaban-structured", "redact");
    let mut srv = server_with_seeded_text(
        &s,
        &format!("the build box password used to be {REDACT_CANARY} before the rotation"),
    );
    let result = recall(&mut srv, "build box rotation");
    assert_ne!(result["isError"], json!(true), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    let structured = &result["structuredContent"];
    assert!(
        structured.is_object(),
        "structuredContent missing: {result}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        *structured,
        "text content and structuredContent differ"
    );
    let card = &structured["results"][0];
    assert!(
        card["text"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:old-password]"),
        "{structured}"
    );
    let wire = result.to_string().to_lowercase();
    assert!(
        !wire.contains(REDACT_CANARY),
        "raw canary on the wire: {result}"
    );
    srv.finish();
}

#[test]
fn a_reject_hit_at_the_response_gate_withholds_both_text_and_structured_content() {
    let s = Scratch::new("kaban-structured", "reject");
    let mut srv = server_with_seeded_text(
        &s,
        &format!("the launch codename was {REJECT_CANARY} for the whole quarter"),
    );
    let result = recall(&mut srv, "launch codename quarter");
    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(
        result.get("structuredContent").is_none(),
        "a withheld result must carry no structuredContent: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("response withheld"), "{result}");
    let wire = result.to_string().to_lowercase();
    assert!(
        !wire.contains(&REJECT_CANARY.to_lowercase()),
        "raw canary on the wire: {result}"
    );
    srv.finish();
}
