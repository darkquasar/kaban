//! `accept` over MCP (core-spec F4; core-plan C3.2): the two-process handshake (M3-14: the MCP
//! server issues the token, `kaban accept` on the CLI redeems it) and the elicitation probe
//! (M3-15: a client that declares form elicitation is asked first; `confirm: false` falls back
//! to the token path, `confirm: true` redeems it in place). The token never reaches stderr,
//! the database files, the audit export or any non-issuing response.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, Server};
use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);
const WORKSPACE: &str = "two-proc";

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    srv.response(id, DEADLINE).expect("tools/call answered")
}

fn result_json(resp: &Value) -> Value {
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn remember_decision(srv: &mut Server, text: &str) -> String {
    result_json(&call(
        srv,
        "remember",
        &json!({"kind": "Decision", "text": text, "props": {"rationale": "because"}}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn accept_args(id: &str) -> Value {
    json!({"id": id, "reason": "reviewed in standup", "path_scope": ["src/api/**"]})
}

/// Every byte SQLite and the audit export wrote under the scratch directory (not the key).
fn disk_bytes(s: &Scratch) -> Vec<u8> {
    let mut out = Vec::new();
    for f in std::fs::read_dir(&s.0).unwrap() {
        let p = f.unwrap().path();
        if p.is_file() {
            out.extend(std::fs::read(&p).unwrap());
        }
    }
    out
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Whether `s` holds 64 consecutive lowercase hex digits (a token shape).
fn has_token_shape(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut run = 0;
    for b in bytes {
        if b.is_ascii_digit() || (b'a'..=b'f').contains(b) {
            run += 1;
            if run >= 64 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

#[test]
fn token_issued_over_mcp_is_redeemed_by_the_cli_in_another_process_as_human_via_chat() {
    let s = Scratch::new("kaban-accept-mcp", "two-process");
    let export = s.0.join("audit.jsonl");
    let mut server = Server::start_traced(
        &s,
        &[
            "--workspace",
            WORKSPACE,
            "--audit-export",
            export.to_str().unwrap(),
        ],
    );
    let id = remember_decision(&mut server, "ship on mondays");
    // process 1 (MCP, agent): confirmation required, token issued once
    let issued = result_json(&call(&mut server, "accept", &accept_args(&id)));
    assert_eq!(issued["status"], json!("confirmation_required"), "{issued}");
    let token = issued["confirmation_token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64);
    let prompt = issued["prompt"].as_str().unwrap();
    assert!(
        !prompt.contains(&token) && !has_token_shape(prompt),
        "{prompt}"
    );
    assert_eq!(issued["expires_in_s"], json!(600));

    // process 2 (CLI, the user): redeems while process 1 still serves the same database
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .env("RUST_LOG", "trace")
            .args(["--db", s.db().to_str().unwrap(), "--workspace", WORKSPACE])
            .args(["--audit-export", export.to_str().unwrap()])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let out = cli(&[
        "accept",
        &id,
        "--confirmation-token",
        &token,
        "--confirmed-by-user",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr2 = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr2}"
    );
    let accepted: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(accepted["status"], json!("accepted"), "{accepted}");
    assert_eq!(accepted["signer_kind"], json!("human-via-chat"));
    assert_eq!(accepted["node"]["governance"], json!("accepted"));
    assert!(!stdout.contains(&token) && !stderr2.contains(&token));

    // the event, through the database
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let (kind, signer_kind, reason, host): (String, String, String, Option<String>) = conn
        .query_row(
            "SELECT kind, signer_kind, reason, host FROM events WHERE node_id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), signer_kind.as_str()),
        ("accepted", "human-via-chat")
    );
    assert_eq!(reason, "reviewed in standup");
    assert_eq!(
        host.as_deref(),
        Some("t"),
        "the issuing client's name is the event host"
    );
    // EN-F12c through the handshake (wave-3 review residual): the person who confirmed is
    // the accepter, so the acceptance writes the `preferred` record claim as a CLI
    // acceptance does; an agent's own acceptance writes none (`accept_auto.rs`)
    let claims: i64 = conn
        .query_row(
            "SELECT count(*) FROM assertion WHERE claim_class = 'kind_of_record'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        claims, 1,
        "a confirmed human-via-chat acceptance writes the EN-F12c record claim"
    );
    drop(conn);

    // replay from the CLI: refused, nothing echoed
    let again = cli(&[
        "accept",
        &id,
        "--confirmation-token",
        &token,
        "--confirmed-by-user",
    ]);
    assert_ne!(again.status.code(), Some(0));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&again.stdout),
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(
        text.contains("confirmation_token") && !text.contains(&token),
        "{text}"
    );

    // every other surface: the server's recall of the record, its stderr, the files
    let seen = call(
        &mut server,
        "recall",
        &json!({"query": "mondays", "full_text": true}),
    );
    assert!(!seen.to_string().contains(&token));
    let (_, stderr1) = server.finish_with_stderr();
    assert!(
        !stderr1.contains(&token),
        "token in the server's trace output"
    );
    assert!(
        !contains(&disk_bytes(&s), &token),
        "token found in db, wal, shm or audit export bytes"
    );
    assert!(
        export.exists(),
        "the audit export holds the CLI's accept entry"
    );
}

#[test]
fn a_client_declaring_form_elicitation_is_asked_first() {
    let s = Scratch::new("kaban-accept-mcp", "elicitation");
    let mut srv = Server::start_elicitation_client(&s, &["--workspace", WORKSPACE]);
    let declined = remember_decision(&mut srv, "ship on mondays");
    // the server asks through elicitation/create before answering the call
    let call_id = srv.request(
        "tools/call",
        &json!({"name": "accept", "arguments": accept_args(&declined)}),
    );
    let ask = srv
        .server_request("elicitation/create", DEADLINE)
        .expect("the server elicits a confirmation");
    let message = ask["params"]["message"].as_str().unwrap();
    assert!(message.contains("ship on mondays"), "{message}");
    assert!(
        !has_token_shape(message),
        "the prompt carries the token: {message}"
    );
    assert!(
        ask["params"]["requestedSchema"]["properties"]["confirm"].is_object(),
        "{ask}"
    );
    // `confirm: false`: the token path applies unchanged
    srv.reply(
        &ask,
        &json!({"action": "accept", "content": {"confirm": false}}),
    );
    let resp = srv.response(call_id, DEADLINE).unwrap();
    let body = result_json(&resp);
    assert_eq!(body["status"], json!("confirmation_required"), "{body}");
    let token = body["confirmation_token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64);

    // `confirm: true`: redeemed in place, human-via-chat, and the token never shown
    let confirmed = remember_decision(&mut srv, "ship on wednesdays");
    let call_id = srv.request(
        "tools/call",
        &json!({"name": "accept", "arguments": accept_args(&confirmed)}),
    );
    let ask = srv.server_request("elicitation/create", DEADLINE).unwrap();
    srv.reply(
        &ask,
        &json!({"action": "accept", "content": {"confirm": true}}),
    );
    let resp = srv.response(call_id, DEADLINE).unwrap();
    let body = result_json(&resp);
    assert_eq!(body["status"], json!("accepted"), "{body}");
    assert_eq!(body["signer_kind"], json!("human-via-chat"));
    assert!(!has_token_shape(&resp.to_string()), "{resp}");

    // the declined token is still live and redeems through MCP (the chat path)
    let redeemed = result_json(&call(
        &mut srv,
        "accept",
        &json!({"id": declined, "confirmation_token": token, "confirmed_by_user": true}),
    ));
    assert_eq!(redeemed["status"], json!("accepted"), "{redeemed}");
    // EN-F12c through the handshake (wave-3 review residual): both Decisions were accepted
    // by the person who confirmed, in the agent's context (`confirm: true` in place, then the
    // token back through MCP), so each acceptance writes the `preferred` record claim a CLI
    // acceptance writes; an agent's own acceptance writes none (`accept_auto.rs`)
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let claims: i64 = conn
        .query_row(
            "SELECT count(*) FROM assertion WHERE claim_class = 'kind_of_record'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        claims, 2,
        "a confirmed human-via-chat acceptance through MCP writes the EN-F12c record claim"
    );
    drop(conn);
    assert_eq!(redeemed["signer_kind"], json!("human-via-chat"));
    assert!(!redeemed.to_string().contains(&token));
    let (_, stderr) = srv.finish_with_stderr();
    assert!(
        !stderr.contains(&token),
        "token in the server's trace output"
    );
}

/// OWN-84 (4) (E2.5g): the one-sitting surface runs end to end by an agent through MCP.
/// Positive: a `remember` of an undeclared kind under the shipped `review` policy is a review
/// candidate (`kaban review` lists it); the `accept` tool, an agent with `reason` and
/// `path_scope ["**"]`, answers `accepted` with `signer_kind: agent` (the events row carries
/// the tier and the client name as host); `explain <id>` then reports it settled and `kaban
/// review` lists no item for it. Negative: a Decision over the same server still answers
/// `confirmation_required` (OWN-84 (3): the handshake is unchanged).
#[test]
fn an_agent_clears_a_review_candidate_over_mcp_and_a_decision_keeps_the_handshake() {
    let s = Scratch::new("kaban-accept-mcp", "delegated");
    let mut srv = Server::start_with(&s, &["--workspace", WORKSPACE], DEADLINE).0;
    let candidate = result_json(&call(
        &mut srv,
        "remember",
        &json!({"kind": "Widget", "text": "a widget nobody declared"}),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let review = || -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
            .env("KABAN_HOME", s.home())
            .args(["--db", s.db().to_str().unwrap(), "--workspace", WORKSPACE])
            .arg("review")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let lists = |doc: &Value| {
        doc["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["subject"]["node"] == json!(candidate))
    };
    let before = review();
    assert!(lists(&before), "the candidate is pending: {before}");

    // the agent accepts it directly: no token, the tier is the event's signer_kind
    let accepted = result_json(&call(
        &mut srv,
        "accept",
        &json!({"id": candidate, "reason": "reviewed", "path_scope": ["**"]}),
    ));
    assert_eq!(accepted["status"], json!("accepted"), "{accepted}");
    assert_eq!(accepted["signer_kind"], json!("agent"));
    assert!(accepted.get("confirmation_token").is_none());
    let explained = result_json(&call(&mut srv, "explain", &json!({"address": candidate})));
    assert_eq!(
        explained["status"],
        json!({"settled": "active"}),
        "{explained}"
    );
    assert_eq!(explained["next_actions"], json!([]));
    let after = review();
    assert!(
        !lists(&after),
        "the accepted record is still listed: {after}"
    );
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let (kind, signer_kind, host): (String, String, Option<String>) = conn
        .query_row(
            "SELECT kind, signer_kind, host FROM events WHERE node_id = ?",
            [&candidate],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), signer_kind.as_str(), host.as_deref()),
        ("accepted", "agent", Some("t"))
    );
    drop(conn);

    // the negative: a Decision over the same server keeps the handshake
    let decision = remember_decision(&mut srv, "ship on fridays");
    let issued = result_json(&call(&mut srv, "accept", &accept_args(&decision)));
    assert_eq!(issued["status"], json!("confirmation_required"), "{issued}");
    assert_eq!(issued["confirmation_token"].as_str().unwrap().len(), 64);
    srv.finish();
}
