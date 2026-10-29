//! Deterministic multiprocess publication barriers: no timing sleeps or product retries.
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::{Scratch, Server};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

fn launch(s: &Scratch, root: &std::path::Path, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .current_dir(root)
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
fn send(mut child: Child, body: &Value) -> Output {
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
fn parse(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}
fn call(server: &mut Server, tool: &str, args: &Value) -> Value {
    let id = server.request("tools/call", &json!({"name":tool,"arguments":args}));
    let response = server.response(id, Duration::from_secs(10)).unwrap();
    assert_ne!(response["result"]["isError"], true, "{response}");
    response["result"]["structuredContent"].clone()
}
fn repo(s: &Scratch) -> std::path::PathBuf {
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.md"), "# Source\n\nHarmless source.\n").unwrap();
    root
}
fn graph_rows(s: &Scratch) -> Value {
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    let nodes = conn
        .prepare(
            "SELECT id,kind,kaban_body(text),props,governance,lifecycle,content_hash FROM node ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?
            ]))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let claims = conn
        .prepare("SELECT key,rank,rank_reason FROM assertion ORDER BY key")
        .unwrap()
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?
            ]))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let edges=conn.prepare("SELECT s.id,d.id,e.rel,e.props,e.layer,e.method,e.score,e.source_id FROM edge e JOIN node s ON s.rid=e.src JOIN node d ON d.rid=e.dst ORDER BY s.id,d.id,e.rel").unwrap().query_map([],|r|Ok(json!([r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<f64>>(6)?,r.get::<_,Option<String>>(7)?]))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    json!({"nodes":nodes,"claims":claims,"edges":edges})
}

#[test]
fn two_processes_duplicate_submit_create_once_and_equal_single_submit_graph() {
    let s = Scratch::new("kaban-e310-race", "duplicate");
    let control = Scratch::new("kaban-e310-race", "control");
    let root = repo(&s);
    let mut first = Server::start_at(&s, &root);
    let prepared = call(&mut first, "ingest_brief", &json!({"path":"a.md"}));
    let job = prepared["job"].as_str().unwrap();
    // Clone the same pristine graph and fixture audit signer: DI fingerprints are
    // keyed, so an independently generated key would make graph bytes incomparable.
    let checkpoint = rusqlite::Connection::open(s.db()).unwrap();
    checkpoint
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(checkpoint);
    std::fs::copy(s.db(), control.db()).unwrap();
    std::fs::create_dir_all(control.home().join("keys")).unwrap();
    for key in std::fs::read_dir(s.home().join("keys")).unwrap() {
        let key = key.unwrap();
        std::fs::copy(
            key.path(),
            control.home().join("keys").join(key.file_name()),
        )
        .unwrap();
    }
    let body =
        json!({"job":job,"records":[{"local_id":"r","kind":"Fact","text":"Harmless assertion"}]});
    let mut second = Server::start_at(&s, &root);
    // Both established worker processes cross this request-send barrier before either
    // response is collected; SQLite arbitrates their atomic publication order.
    let one = first.request(
        "tools/call",
        &json!({"name":"ingest_submit","arguments":body}),
    );
    let two = second.request(
        "tools/call",
        &json!({"name":"ingest_submit","arguments":body}),
    );
    let a = first.response(one, Duration::from_secs(10)).unwrap();
    let b = second.response(two, Duration::from_secs(10)).unwrap();
    assert_ne!(a["result"]["isError"], true, "{a}");
    assert_ne!(b["result"]["isError"], true, "{b}");
    assert_eq!(
        a["result"]["structuredContent"]["created"]
            .as_u64()
            .unwrap()
            + b["result"]["structuredContent"]["created"]
                .as_u64()
                .unwrap(),
        1
    );
    assert_eq!(
        a["result"]["structuredContent"]["unchanged"]
            .as_u64()
            .unwrap()
            + b["result"]["structuredContent"]["unchanged"]
                .as_u64()
                .unwrap(),
        1
    );
    let mut single = Server::start_at(&control, &root);
    let control_prepared = call(&mut single, "ingest_brief", &json!({"path":"a.md"}));
    assert_eq!(control_prepared["job"], prepared["job"]);
    call(&mut single, "ingest_submit", &body);
    assert_eq!(graph_rows(&s), graph_rows(&control));
    let one = first.request(
        "tools/call",
        &json!({"name":"ingest_finish","arguments":{"job":job}}),
    );
    let two = second.request(
        "tools/call",
        &json!({"name":"ingest_finish","arguments":{"job":job}}),
    );
    let a = first.response(one, Duration::from_secs(10)).unwrap();
    let b = second.response(two, Duration::from_secs(10)).unwrap();
    assert_ne!(a["result"]["isError"], true, "{a}");
    assert_ne!(b["result"]["isError"], true, "{b}");
    assert_eq!(a["result"]["content"], b["result"]["content"]);
    first.finish();
    second.finish();
    single.finish();
}

#[test]
fn acceptance_before_pending_submit_preserves_authority_and_makes_revision_candidate() {
    let s = Scratch::new("kaban-e310-race", "accept");
    let root = repo(&s);
    let mut server = Server::start_at(&s, &root);
    let prepared = call(
        &mut server,
        "ingest_brief",
        &json!({"path":"a.md","type":"adr"}),
    );
    let job = prepared["job"].as_str().unwrap();
    let original = json!({"job":job,"records":[{"local_id":"d","kind":"Decision","text":"Original decision","props":{"rationale":"Original rationale"},"confidence":1.0}]});
    let placed = call(&mut server, "ingest_submit", &original);
    let id = placed["records"][0]["id"].as_str().unwrap();
    // Stdin is the deterministic publication barrier. The pending independent submit
    // process exists before acceptance and cannot read/overwrite the accepted bytes.
    let pending = launch(&s, &root, &["ingest", "job", "submit", job, "--stdin-json"]);
    let accepted = launch(
        &s,
        &root,
        &["accept", id, "--reason", "reviewed", "--path-scope", "a.md"],
    )
    .wait_with_output()
    .unwrap();
    parse(&accepted);
    let conn = rusqlite::Connection::open(s.db()).unwrap();
    kaban_store_sqlite::register_body_reader(&conn).unwrap();
    let before: (String, String, i64) = conn
        .query_row(
            "SELECT kaban_body(text),props,rev FROM node WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let changed = json!({"job":job,"records":[{"local_id":"d","kind":"Decision","text":"New decision","props":{"rationale":"New rationale"},"confidence":1.0}]});
    let result = parse(&send(pending, &changed));
    assert_eq!(result["records"][0]["outcome"], "revision_candidate");
    assert_eq!(result["revision_candidates"][0]["of"], id);
    let after: (String, String, i64) = conn
        .query_row(
            "SELECT kaban_body(text),props,rev FROM node WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(after, before);
    let governance: String = conn
        .query_row("SELECT governance FROM node WHERE id=?", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(governance, "accepted");
    server.finish();
}

#[test]
fn forget_and_purge_before_pending_submit_revoke_without_late_rows() {
    for mode in ["tombstone", "purge"] {
        let s = Scratch::new("kaban-e310-race", mode);
        let root = repo(&s);
        let mut server = Server::start_at(&s, &root);
        let prepared = call(&mut server, "ingest_brief", &json!({"path":"a.md"}));
        let job = prepared["job"].as_str().unwrap();
        let pending = launch(&s, &root, &["ingest", "job", "submit", job, "--stdin-json"]);
        let forgotten = launch(
            &s,
            &root,
            &[
                "forget",
                prepared["document"]["id"].as_str().unwrap(),
                "--mode",
                mode,
            ],
        )
        .wait_with_output()
        .unwrap();
        parse(&forgotten);
        let out = send(
            pending,
            &json!({"job":job,"records":[{"local_id":"late","kind":"Fact","text":"late assertion"}]}),
        );
        assert_eq!(out.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&out.stderr).contains("job_revoked"));
        let conn = rusqlite::Connection::open(s.db()).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM ingest_jobs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM node WHERE kind='Fact'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        server.finish();
    }
}
