//! The E3 1,000-record scale test (engine-spec EN-N8 scale half, EN-F8a; engine-plan E3.12;
//! LESSONS 2026-10-26: one informational scale test per milestone), through the public
//! workflow: `ontology load` of a user ontology, plain `index_docs` of 100 documents, 50
//! structured `script` jobs (`ingest_brief`, `ingest_submit`, `ingest_finish`) and 50 fenced
//! `ingest_claim` jobs (gliner dialect) of ten records each, the gazetteer over the 1,000
//! records (`index_docs --link gazetteer`), reconciliation in every finish and one ER pass
//! (`SqliteStore::er_resolve`: ER has no CLI or MCP verb in 3.0.0). Afterwards
//! `PRAGMA foreign_key_check` is empty, the record counts are exact, and a claim and a submit
//! bound to a foreign workspace write nothing. The phase times are printed (informational,
//! never a gate).
//!
//! Ignored by default (about 300 tool calls over one stdio server); runner:
//!
//! ```text
//! cargo nextest run -p kaban --all-features --run-ignored only -E 'binary(scale_e3)' --no-capture
//! ```
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stdout)] // the phase times are the test's informational output
mod common;

use common::{Scratch, Server};
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const DOCS: usize = 100;
/// Documents `0..BRIEF_DOCS` go through `ingest_brief`, the rest through `ingest_claim`.
const BRIEF_DOCS: usize = 50;
const SERVICES_PER_DOC: usize = 2;
const COMPONENTS_PER_DOC: usize = 8;
const RECORDS: usize = DOCS * (SERVICES_PER_DOC + COMPONENTS_PER_DOC);
/// Component 0 of document `d` and of document `d + BRIEF_DOCS` share a first line: one ER
/// `Match` pair per brief/claim document pair.
const ER_PAIRS: u64 = BRIEF_DOCS as u64;
/// The workspace the repository binds (its root directory's name).
const WS: &str = "repo";

const ONTOLOGY: &str = r#"ontology: scale
version: 1
extends: [core]
kinds:
  Service:
    description: "A deployed service."
  Component:
    description: "A component a service uses."
rels:
  USES: {from: [scale:Service], to: [scale:Component], directed: true, description: "The service uses the component."}
source_types: {inventory: {kinds: [scale:Service, scale:Component], paths: ["inv/**"]}}
"#;

/// The ten records and eight `USES` edges of document `d`.
fn submission(d: usize) -> (Vec<Value>, Vec<Value>) {
    let mut records: Vec<Value> = (0..SERVICES_PER_DOC)
        .map(|j| json!({"local_id": format!("s{j}"), "kind": "scale:Service", "text": format!("Orion service {d} {j}")}))
        .collect();
    let mut edges = Vec::new();
    for j in 0..COMPONENTS_PER_DOC {
        let text = if j == 0 {
            format!("Shared gateway {}\nlisted in document {d}", d % BRIEF_DOCS)
        } else {
            format!("Vega component {d} {j}")
        };
        records.push(json!({"local_id": format!("c{j}"), "kind": "scale:Component", "text": text}));
        edges.push(json!({"from_local_id": format!("s{}", j * SERVICES_PER_DOC / COMPONENTS_PER_DOC), "rel": "scale:USES", "to_local_id": format!("c{j}")}));
    }
    (records, edges)
}

/// The document text names every record's first line, so the gazetteer links each one.
fn document(d: usize) -> String {
    let (records, _) = submission(d);
    let names: Vec<String> = records
        .iter()
        .map(|r| format!("{}.", r["text"].as_str().unwrap().lines().next().unwrap()))
        .collect();
    format!("# Inventory {d}\n\n{}\n", names.join(" "))
}

fn doc_path(d: usize) -> String {
    let dir = if d < BRIEF_DOCS { "brief" } else { "claim" };
    format!("inv/{dir}/d{d:03}.md")
}

fn cli(s: &Scratch, root: &Path, args: &[&str], body: Option<&Value>) -> Output {
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
    child.wait_with_output().unwrap()
}

fn call(srv: &mut Server, tool: &str, args: &Value) -> Value {
    let id = srv.request("tools/call", &json!({"name": tool, "arguments": args}));
    let response = srv.response(id, Duration::from_secs(60)).unwrap();
    assert_ne!(response["result"]["isError"], true, "{tool}: {response}");
    response["result"]["structuredContent"].clone()
}

/// Every row of every table, sorted: the no-write witness of the foreign negatives.
fn state(conn: &rusqlite::Connection) -> Vec<String> {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut rows = Vec::new();
    for table in tables {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
            .unwrap();
        let columns = stmt.column_count();
        let mut query = stmt.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let cells: Vec<String> = (0..columns)
                .map(|n| format!("{:?}", row.get_ref(n).unwrap()))
                .collect();
            rows.push(format!("{table}:{cells:?}"));
        }
    }
    rows.sort();
    rows
}

fn count(conn: &rusqlite::Connection, sql: &str, params: &[&str]) -> usize {
    let n: i64 = conn
        .query_row(sql, rusqlite::params_from_iter(params), |r| r.get(0))
        .unwrap();
    usize::try_from(n).unwrap()
}

struct Finishes {
    total: Duration,
    max: Duration,
    created: u64,
}

impl Finishes {
    fn record(&mut self, took: Duration, report: &Value) {
        assert_eq!(report["status"], "finished", "{report}");
        assert_eq!(report["reconcile"], "complete", "{report}");
        self.total += took;
        self.max = self.max.max(took);
        self.created += report["created"].as_u64().unwrap();
    }
}

#[test]
#[ignore = "scale test (100 documents, 1,000 records, about 300 tool calls); run with --run-ignored only"]
fn one_thousand_records_through_ontology_structured_claim_gazetteer_and_er() {
    let s = Scratch::new("kaban-scale-e3", "workflow");
    let root = s.0.join(WS);
    for dir in [".git", ".kaban/ontology", "inv/brief", "inv/claim"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join(".kaban/ontology/scale.yaml"), ONTOLOGY).unwrap();
    for d in 0..DOCS {
        std::fs::write(root.join(doc_path(d)), document(d)).unwrap();
    }
    let out = cli(
        &s,
        &root,
        &["ontology", "load", ".kaban/ontology/scale.yaml"],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    let loaded: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        (loaded["kinds"].as_u64(), loaded["rels"].as_u64()),
        (Some(2), Some(1))
    );

    let mut srv = Server::start_at(&s, &root);
    let t = Instant::now();
    let plain = call(&mut srv, "index_docs", &json!({"paths": ["inv"]}));
    assert_eq!(plain["documents"], DOCS, "{plain}");
    let plain_took = t.elapsed();

    let mut finishes = Finishes {
        total: Duration::ZERO,
        max: Duration::ZERO,
        created: 0,
    };
    let t = Instant::now();
    for d in 0..BRIEF_DOCS {
        let brief = call(&mut srv, "ingest_brief", &json!({"path": doc_path(d)}));
        let job = brief["job"].clone();
        let (records, edges) = submission(d);
        let submitted = call(
            &mut srv,
            "ingest_submit",
            &json!({"job": job, "records": records, "edges": edges}),
        );
        assert_eq!(submitted["created"], records.len(), "{submitted}");
        let f = Instant::now();
        let report = call(&mut srv, "ingest_finish", &json!({"job": job}));
        finishes.record(f.elapsed(), &report);
    }
    let brief_took = t.elapsed();

    let t = Instant::now();
    let mut claimed = Vec::new();
    loop {
        let claim = call(
            &mut srv,
            "ingest_claim",
            &json!({"with": "gliner", "path_prefix": "inv/claim/", "worker": "scale:1"}),
        );
        if claim["job"].is_null() {
            break;
        }
        let path = claim["brief"]["document"]["path"].as_str().unwrap();
        let d = (BRIEF_DOCS..DOCS).find(|&d| doc_path(d) == path).unwrap();
        claimed.push(d);
        let (job, fence) = (claim["job"].clone(), claim["lease"]["fence"].clone());
        let (records, edges) = submission(d);
        let submitted = call(
            &mut srv,
            "ingest_submit",
            &json!({"job": job, "fence": fence, "records": records, "edges": edges}),
        );
        assert_eq!(submitted["created"], records.len(), "{submitted}");
        let f = Instant::now();
        let report = call(
            &mut srv,
            "ingest_finish",
            &json!({"job": job, "fence": fence}),
        );
        finishes.record(f.elapsed(), &report);
    }
    let claim_took = t.elapsed();
    claimed.sort_unstable();
    assert_eq!(claimed, (BRIEF_DOCS..DOCS).collect::<Vec<_>>());
    assert_eq!(finishes.created, RECORDS as u64);

    let t = Instant::now();
    let linked = call(
        &mut srv,
        "index_docs",
        &json!({"paths": ["inv"], "link": "gazetteer"}),
    );
    assert_eq!(linked["gazetteer"]["outcome"], "ok", "{linked}");
    let gazetteer_took = t.elapsed();
    srv.finish();

    let store = SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();
    let t = Instant::now();
    let er = store.er_resolve(WS).unwrap();
    let er_took = t.elapsed();
    drop(store);
    assert_eq!(er.matched, ER_PAIRS, "{er:?}");

    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let violations: Vec<String> = conn
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |r| {
            Ok(format!("{:?}/{:?}", r.get_ref(0)?, r.get_ref(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(violations, Vec::<String>::new());
    let kinds = |kind: &str| {
        count(
            &conn,
            "SELECT count(*) FROM node WHERE kind = ?1 AND workspace_id = ?2",
            &[kind, WS],
        )
    };
    assert_eq!(kinds("scale:Service"), DOCS * SERVICES_PER_DOC);
    assert_eq!(kinds("scale:Component"), DOCS * COMPONENTS_PER_DOC);
    assert_eq!(kinds("Document"), DOCS);
    // every record's first line is in its own document: the gazetteer linked each at least once
    let mentioned = count(
        &conn,
        "SELECT count(DISTINCT dst) FROM edge WHERE rel = 'MENTIONS' AND layer = 'gazetteer'",
        &[],
    );
    assert!(mentioned >= RECORDS, "{mentioned} records mentioned");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM node_identity", &[]),
        usize::try_from(ER_PAIRS).unwrap()
    );

    // negative: a claimer and a submitter bound to another workspace write nothing
    let before = state(&conn);
    let foreign = cli(
        &s,
        &root,
        &[
            "--workspace",
            "foreign",
            "ingest",
            "job",
            "claim",
            "--with",
            "gliner",
        ],
        None,
    );
    assert!(foreign.status.success(), "{foreign:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&foreign.stdout).unwrap(),
        json!({"job": null})
    );
    let job = conn
        .query_row("SELECT job_id FROM ingest_jobs LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    let (records, _) = submission(0);
    let foreign = cli(
        &s,
        &root,
        &[
            "--workspace",
            "foreign",
            "ingest",
            "job",
            "submit",
            &job,
            "--stdin-json",
        ],
        Some(&json!({"job": job, "records": records})),
    );
    assert_eq!(foreign.status.code(), Some(3), "{foreign:?}");
    assert_eq!(
        state(&conn),
        before,
        "foreign claim and submit write nothing"
    );

    println!(
        "scale_e3: {RECORDS} records in {DOCS} documents: plain index {plain_took:?}, \
         {BRIEF_DOCS} brief jobs {brief_took:?}, {} claim jobs {claim_took:?}, finish total {:?} \
         (max {:?}, {DOCS} finishes), gazetteer {gazetteer_took:?}, er {er_took:?} ({er:?})",
        DOCS - BRIEF_DOCS,
        finishes.total,
        finishes.max,
    );
}
