//! EN-F22 coverage and the EN-E7 pair check (engine-plan E3.12, RE-15), black-box through the
//! binary: every producer 3.0.0 ships writes through its public interface (plain `ingest` = L0,
//! the `fuzzy` recall layer, `ingest --link gazetteer`, one structured `script` submit, one
//! `ingest_claim` submit, one MCP `link` = authored), then the stored `(layer, method)` columns
//! of `edge` (the producer is stored in `layer` until T4 renames it, PC-16) are read back from
//! the database file. No embedding producer exists (PC-16).
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod common;

use common::{Scratch, Server};
use kaban_core::{Method, Producer};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// One stored `(layer, method)` pair; `None` is a SQL NULL.
type Pair = (Option<String>, Option<String>);

/// The pairs outside the EN-E7 closed vocabularies: a NULL column or a spelling
/// [`Producer::parse`] / [`Method::parse`] does not know.
fn outside_enums<'a>(pairs: impl IntoIterator<Item = &'a Pair>) -> Vec<Pair> {
    pairs
        .into_iter()
        .filter(|(layer, method)| {
            layer.as_deref().and_then(Producer::parse).is_none()
                || method.as_deref().and_then(Method::parse).is_none()
        })
        .cloned()
        .collect()
}

fn cli(s: &Scratch, root: &Path, args: &[&str], body: Option<&Value>) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .current_dir(root)
        .args(["--db", s.db().to_str().unwrap(), "--json"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(body) = body {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.to_string().as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());
    let out: Output = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

fn mcp(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    let response = srv.response(id, Duration::from_secs(10)).unwrap();
    assert_ne!(response["result"]["isError"], true, "{tool}: {response}");
    response["result"]["structuredContent"].clone()
}

/// The full-layer run; returns the repository root.
fn full_layer_run(s: &Scratch) -> PathBuf {
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(s.home()).unwrap();
    std::fs::write(
        root.join("a.md"),
        "# Alpha\n\nThe cart service owns checkout.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.md"),
        "# Beta\n\nPublic Cart Architecture describes the system.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("c.md"),
        "# Gamma\n\nThe ledger tracks payments.\n",
    )
    .unwrap();
    // L0
    cli(s, &root, &["ingest", "a.md", "c.md"], None);
    // the `fuzzy` recall layer (spec 05 OWN-M): typo-tolerant retrieval by the one text index,
    // nothing to build beside it
    cli(
        s,
        &root,
        &[
            "recall",
            "chekout",
            "--modes",
            "lexical,fuzzy",
            "--hops",
            "0",
        ],
        None,
    );
    // gazetteer: a known record, then a document that mentions it
    let target = cli(
        s,
        &root,
        &["remember", "Fact", "Public Cart Architecture"],
        None,
    );
    let linked = cli(s, &root, &["ingest", "b.md", "--link", "gazetteer"], None);
    assert_eq!(linked["gazetteer"]["outcome"], "ok", "{linked}");
    // L3 `script`: the deterministic structured protocol (`--with none`)
    let brief = cli(s, &root, &["ingest", "job", "brief", "a.md"], None);
    let job = brief["job"].as_str().unwrap();
    let body = json!({"job": job, "records": [
        {"local_id": "cart", "kind": "Fact", "text": "The cart service owns checkout"},
        {"local_id": "checkout", "kind": "Fact", "text": "Checkout is owned by the cart"},
    ], "edges": [{"from_local_id": "cart", "rel": "DEPENDS_ON", "to_local_id": "checkout"}]});
    let submitted = cli(
        s,
        &root,
        &["ingest", "job", "submit", job, "--stdin-json"],
        Some(&body),
    );
    assert_eq!(submitted["created"], 2, "{submitted}");
    let script_record = submitted["records"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        cli(s, &root, &["ingest", "job", "finish", job], None)["status"],
        "finished"
    );
    // `ingest_claim` (an external gliner-dialect worker, fenced)
    let claim = cli(
        s,
        &root,
        &[
            "ingest",
            "job",
            "claim",
            "--with",
            "gliner",
            "--path-prefix",
            "c.md",
        ],
        None,
    );
    let job = claim["job"].as_str().unwrap();
    let fence = claim["lease"]["fence"].as_u64().unwrap().to_string();
    let body = json!({"job": job, "fence": claim["lease"]["fence"], "records": [
        {"local_id": "ledger", "kind": "Fact", "text": "The ledger tracks payments"},
    ]});
    let submitted = cli(
        s,
        &root,
        &[
            "ingest",
            "job",
            "submit",
            job,
            "--fence",
            &fence,
            "--stdin-json",
        ],
        Some(&body),
    );
    assert_eq!(submitted["created"], 1, "{submitted}");
    assert_eq!(
        cli(
            s,
            &root,
            &["ingest", "job", "finish", job, "--fence", &fence],
            None
        )["status"],
        "finished"
    );
    // human: one MCP `link`
    let mut srv = Server::start_at(s, &root);
    mcp(
        &mut srv,
        "link",
        &json!({"from": target["id"], "rel": "DEPENDS_ON", "to": script_record}),
    );
    srv.finish();
    root
}

fn read_only(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn pairs(conn: &Connection, table: &str) -> BTreeSet<Pair> {
    conn.prepare(&format!("SELECT DISTINCT layer, method FROM {table}"))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn pair(layer: &str, method: &str) -> Pair {
    (Some(layer.to_owned()), Some(method.to_owned()))
}

#[test]
fn full_layer_run_stores_only_closed_vocabulary_pairs() {
    let s = Scratch::new("kaban-e312-coverage", "pairs");
    full_layer_run(&s);
    let conn = read_only(&s);
    let edges = pairs(&conn, "edge");
    // the document ingest's structural rows (the `PART_OF` hierarchy) carry no method by design
    let structural: Pair = (Some(Producer::DocumentIngest.as_str().to_owned()), None);
    let labelled: Vec<&Pair> = edges.iter().filter(|p| **p != structural).collect();
    assert_eq!(outside_enums(labelled), Vec::<Pair>::new(), "{edges:?}");
    // every producer of the run left its own pair (the coverage, not only the absence of bad rows)
    for expected in [
        pair("gazetteer", "exact"),
        pair("extractor:script", "ingest_submit"),
        pair("extractor:gliner", "ingest_submit"),
        pair("authored", "link"),
    ] {
        assert!(edges.contains(&expected), "{expected:?} in {edges:?}");
    }
}

/// EN-F22 for edges, under PC-16: every edge carries its producer, and every producer edge a
/// `method`. The document ingest's structural row (the `PART_OF` hierarchy) carries no method
/// by design, so a row without a producer, or a non-structural row without a method, is the
/// only failure.
#[test]
fn full_layer_run_leaves_no_edge_without_a_producer_or_method() {
    let s = Scratch::new("kaban-e312-coverage", "edges");
    full_layer_run(&s);
    let conn = read_only(&s);
    let unlabelled: Vec<(String, String)> = conn
        .prepare(
            "SELECT rel, coalesce(layer, '') FROM edge \
             WHERE layer IS NULL OR (method IS NULL \
             AND NOT (rel = 'PART_OF' AND layer = 'document_ingest'))",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(unlabelled, Vec::<(String, String)>::new());
}

#[test]
fn a_pair_outside_the_enums_is_rejected() {
    let known = pair("authored", "link");
    let injected = [
        pair("vector", "exact"),
        pair("authored", "guess"),
        // the removed pre-PC-16 producer spelling
        pair("human", "link"),
        (None, Some("link".to_owned())),
        (Some("gazetteer".to_owned()), None),
    ];
    assert_eq!(outside_enums([&known]), Vec::<Pair>::new());
    let mut all = vec![known];
    all.extend(injected.iter().cloned());
    assert_eq!(outside_enums(&all), injected.to_vec());
}
