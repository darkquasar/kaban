//! `kaban accept` on the CLI goes through the field-class seam like the MCP adapter (core-spec
//! S1, S4; OWN-10; review R3-1): the raw request is scanned before any typed parse, so a
//! malformed, credential-shaped `--confirmation-token` is content and is never echoed (clap
//! never formats the value), and every error text and success value passes the response gate,
//! so stored content a newer policy forbids cannot leave through an error message. The stdin
//! token route (`--token-stdin`, owner decision OWN-20) keeps the token out of the process
//! arguments and is judged by the same seam.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{AcceptRequest, AcceptResponse, ApiConfig, Caller, KabanApi, RememberRequest};
use kaban_core::{
    Actor, MemoryStore, NewNode, RememberOutcome, SchemaStore, TypeDef, WorkspaceId, WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view.
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";
const WORKSPACE: &str = "default";

/// Seed one node of `kind` through the store (no policy gate), with the key under the
/// scratch's `$KABAN_HOME` so the CLI reuses it.
fn seed(s: &Scratch, kind: &str, text: &str) -> String {
    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let ctx = WriteCtx::new("seed", WorkspaceId::default(), Actor::Agent);
    if !["Decision", "Convention", "Failure", "Fact"].contains(&kind) {
        store
            .schema_extend(TypeDef {
                kind: "label".into(),
                name: kind.into(),
                ns: None,
                description: "a custom kind".into(),
                from_kinds: vec![],
                to_kinds: vec![],
                directed: true,
                props_schema: None,
                builtin: false,
                declaration: kaban_core::TypeDeclaration::default(),
            })
            .unwrap();
    }
    match store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                workspace_id: Some(WORKSPACE.into()),
                ..Default::default()
            },
            &[],
            &ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(n) | RememberOutcome::Existing { node: n, .. } => n.id,
    }
}

fn kaban(s: &Scratch, policy: Option<&std::path::Path>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "trace")
        .args(["--db", s.db().to_str().unwrap(), "--workspace", WORKSPACE]);
    if let Some(p) = policy {
        cmd.args(["--policy", p.to_str().unwrap()]);
    }
    cmd
}

fn cli(s: &Scratch, policy: Option<&std::path::Path>, args: &[&str]) -> Output {
    kaban(s, policy)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// `cli` with `body` piped to stdin (the subcommand may exit before reading it: EPIPE on our
/// side is not a failure of the thing under test).
fn cli_with_stdin(s: &Scratch, args: &[&str], body: &[u8]) -> Output {
    let mut child = kaban(s, None)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(body);
    child.wait_with_output().unwrap()
}

/// A proposed Decision and the one-time token an agent was issued for it over the API (as the
/// MCP server would issue it), with the key under the scratch's `$KABAN_HOME`; the service is
/// dropped before the CLI runs.
fn issued_token(s: &Scratch, text: &str) -> (String, String) {
    let store = Arc::new(SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap());
    let api = KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new(WORKSPACE),
            repo_root: None,
            repo: None,
            record_signer_identity: false,
            engine: std::sync::Arc::default(),
        },
    );
    let id = api
        .remember(RememberRequest {
            kind: "Decision".into(),
            text: text.into(),
            props: Some(json!({"rationale": "because the team agreed"})),
            ..Default::default()
        })
        .unwrap()
        .id;
    let issued = api
        .accept(
            AcceptRequest {
                id: id.clone(),
                reason: Some("reviewed in standup".into()),
                path_scope: vec!["src/api/**".into()],
                ..Default::default()
            },
            &Caller {
                kind: Actor::Agent,
                host: None,
                session: None,
            },
        )
        .unwrap();
    let AcceptResponse::ConfirmationRequired {
        confirmation_token, ..
    } = issued
    else {
        panic!("an agent accepting a Decision is asked to confirm: {issued:?}");
    };
    // the issuing response is the one place the token is serialized
    let token = serde_json::to_value(&confirmation_token)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    (id, token)
}

fn text_of(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn governance_of(s: &Scratch, id: &str) -> String {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    conn.query_row("SELECT governance FROM node WHERE id = ?", [id], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn a_credential_shaped_token_is_rejected_by_the_denylist_and_never_echoed() {
    let s = Scratch::new("kaban-cli-accept", "pasted-credential");
    let id = seed(&s, "Decision", "ship on mondays");
    // the pasted value is not a kaban token: it is content, and content that hits a credential
    // rule is rejected by the pre-parse scan, before any typed parse could format it
    let out = cli(
        &s,
        None,
        &[
            "accept",
            &id,
            "--confirmation-token",
            CANARY,
            "--confirmed-by-user",
        ],
    );
    let text = text_of(&out);
    assert!(
        !text.contains(CANARY),
        "the pasted value was echoed: {text}"
    );
    assert_eq!(out.status.code(), Some(2), "rejected by policy: {text}");
    assert!(
        text.contains("aws-access-key-id") || text.contains("credential-key"),
        "the rejection names its rule ids: {text}"
    );
    assert!(out.stdout.is_empty(), "stdout stays empty on error");
    assert_eq!(governance_of(&s, &id), "proposed", "nothing was accepted");
}

/// OWN-20: `--token-stdin --confirmed-by-user` reads the token from stdin (one line, trailing
/// newline trimmed) into the same raw request leaf as `--confirmation-token`, so the token is
/// never in the process arguments and still judged by the seam: the MCP-issued token redeems
/// the acceptance as `human-via-chat` and appears in neither stdout nor stderr; a
/// credential-shaped line on stdin is content and exits 2 without echo; an empty stdin exits 4;
/// `--token-stdin` together with `--confirmation-token` or with `--stdin-json` exits 4 with a
/// fixed text and no echo.
#[test]
fn a_token_on_stdin_redeems_the_acceptance_without_appearing_in_argv_or_output() {
    let s = Scratch::new("kaban-cli-accept", "token-stdin");
    let (id, token) = issued_token(&s, "ship on mondays");

    // negatives first: the token stays unredeemed
    let both = cli_with_stdin(
        &s,
        &[
            "accept",
            &id,
            "--token-stdin",
            "--confirmation-token",
            CANARY,
            "--confirmed-by-user",
        ],
        format!("{token}\n").as_bytes(),
    );
    let text = text_of(&both);
    assert_eq!(both.status.code(), Some(4), "both routes: {text}");
    assert!(
        !text.contains(CANARY) && !text.contains(&token),
        "a refused route echoed a value: {text}"
    );
    assert!(text.starts_with("error: "), "fixed text: {text}");
    let with_body = cli_with_stdin(
        &s,
        &["accept", "--stdin-json", "--token-stdin"],
        format!("{token}\n").as_bytes(),
    );
    let text = text_of(&with_body);
    assert_eq!(with_body.status.code(), Some(4), "body plus flag: {text}");
    assert!(!text.contains(&token), "{text}");
    let empty = cli_with_stdin(
        &s,
        &["accept", &id, "--token-stdin", "--confirmed-by-user"],
        b"\n",
    );
    let text = text_of(&empty);
    assert_eq!(empty.status.code(), Some(4), "empty stdin: {text}");
    assert!(text.contains("no token on stdin"), "{text}");
    let credential = cli_with_stdin(
        &s,
        &["accept", &id, "--token-stdin", "--confirmed-by-user"],
        format!("{CANARY}\n").as_bytes(),
    );
    let text = text_of(&credential);
    assert_eq!(
        credential.status.code(),
        Some(2),
        "credential on stdin: {text}"
    );
    assert!(!text.contains(CANARY), "the pasted line was echoed: {text}");
    assert!(credential.stdout.is_empty(), "stdout stays empty on error");
    assert_eq!(
        governance_of(&s, &id),
        "proposed",
        "nothing was accepted yet"
    );

    // the token on stdin redeems; the arguments the test spawned never carried it
    let args = [
        "accept",
        id.as_str(),
        "--token-stdin",
        "--confirmed-by-user",
    ];
    assert!(args.iter().all(|a| !a.contains(&token)));
    let out = cli_with_stdin(&s, &args, format!("{token}\n").as_bytes());
    let text = text_of(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        !text.contains(&token),
        "the token left through the output: {text}"
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["status"], json!("accepted"), "{doc}");
    assert_eq!(doc["signer_kind"], json!("human-via-chat"), "{doc}");
    assert_eq!(doc["node"]["governance"], json!("accepted"), "{doc}");
    assert_eq!(governance_of(&s, &id), "accepted");

    // one-time: the same line again fails without echoing it
    let replay = cli_with_stdin(&s, &args, format!("{token}\n").as_bytes());
    let text = text_of(&replay);
    assert_ne!(replay.status.code(), Some(0), "{text}");
    assert!(!text.contains(&token), "{text}");
}

#[test]
fn stored_content_a_newer_policy_forbids_does_not_leave_through_an_error() {
    let s = Scratch::new("kaban-cli-accept", "policy-stale-kind");
    // a custom kind stored under an older policy; the kind is content (S1) and carries no
    // governance, so `accept` fails with an error that quotes it
    let id = seed(&s, "Quokka", "a note of a custom kind");
    let policy = s.0.join("policy.yaml");
    std::fs::write(
        &policy,
        "rules:\n  - id: quokka-reject\n    regex: Quokka\n    action: reject\n",
    )
    .unwrap();
    let out = cli(
        &s,
        Some(&policy),
        &["accept", &id, "--reason", "reviewed", "--path-scope", "src"],
    );
    let text = text_of(&out);
    assert_ne!(out.status.code(), Some(0), "{text}");
    assert!(
        !text.contains("Quokka"),
        "the forbidden stored value left through the error text: {text}"
    );
    assert!(out.stdout.is_empty(), "stdout stays empty on error");
    assert!(
        text.contains("withheld"),
        "the fixed replacement text is printed instead: {text}"
    );
}
