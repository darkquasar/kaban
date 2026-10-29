//! Both extractor transports, deterministic database-clock expiry and process contention.
#![cfg(feature = "ingest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::{Scratch, Server};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

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
fn ok(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}
fn seed(s: &Scratch, root: &Path, path: &str, text: &str) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(path), text).unwrap();
    ok(&cli(s, root, &["ingest", path], None));
}
fn db(s: &Scratch) -> rusqlite::Connection {
    rusqlite::Connection::open(s.db()).unwrap()
}
fn expire(s: &Scratch) {
    db(s)
        .execute(
            "UPDATE leases SET expires_at=unixepoch()-1 WHERE name LIKE 'ingest:%'",
            [],
        )
        .unwrap();
}
fn state(s: &Scratch) -> Vec<String> {
    let conn = db(s);
    let tables=conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let mut rows = Vec::new();
    for table in tables {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
            .unwrap();
        let count = stmt.column_count();
        let mut query = stmt.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            rows.push(format!(
                "{table}:{:?}",
                (0..count)
                    .map(|n| format!("{:?}", row.get_ref(n).unwrap()))
                    .collect::<Vec<_>>()
            ));
        }
    }
    // the store's own text-index bookkeeping (its lease and cursor, spec 05 OWN-M) moves at
    // every open; it is not a holder's write (the dump renders text as bytes)
    let text_index = format!("{:?}", b"text-index");
    rows.retain(|row| !row.contains(&text_index));
    rows.sort();
    rows
}
struct Driver<'a> {
    s: &'a Scratch,
    root: &'a Path,
    server: Option<Server>,
}
impl Driver<'_> {
    fn call(&mut self, tool: &str, args: &Value) -> Result<Value, String> {
        if let Some(server) = &mut self.server {
            let id = server.request("tools/call", &json!({"name":tool,"arguments":args}));
            let response = server.response(id, Duration::from_secs(10)).unwrap();
            if response["result"]["isError"] == true || response.get("error").is_some() {
                return Err(response.to_string());
            }
            let value: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(value, response["result"]["structuredContent"]);
            return Ok(value);
        }
        let mut command_args = vec![
            "ingest".to_owned(),
            "job".into(),
            tool.strip_prefix("ingest_").unwrap().into(),
        ];
        if tool == "ingest_claim" {
            command_args.extend(["--with".into(), args["with"].as_str().unwrap().into()]);
            for (key, flag) in [
                ("worker", "--worker"),
                ("path_prefix", "--path-prefix"),
                ("type", "--type"),
            ] {
                if let Some(v) = args[key].as_str() {
                    command_args.extend([flag.into(), v.into()]);
                }
            }
        } else {
            command_args.push(args["job"].as_str().unwrap().into());
            if tool == "ingest_next" {
                command_args.extend(["--chunk".into(), "0".into()]);
            }
            if tool == "ingest_submit" {
                command_args.push("--stdin-json".into());
            }
            if let Some(fence) = args["fence"].as_u64() {
                command_args.extend(["--fence".into(), fence.to_string()]);
            }
        }
        let out = cli(
            self.s,
            self.root,
            &command_args.iter().map(String::as_str).collect::<Vec<_>>(),
            (tool == "ingest_submit").then_some(args),
        );
        if !out.status.success() {
            return Err(format!(
                "exit{}:{}",
                out.status.code().unwrap(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(serde_json::from_slice(&out.stdout).unwrap())
    }
    fn claim(&mut self) -> Value {
        self.call(
            "ingest_claim",
            &json!({"with":"gliner","path_prefix":"a.md","worker":"fixture:1"}),
        )
        .unwrap()
    }
}

fn workflow(mcp: bool) {
    let s = Scratch::new("kaban-e310b", if mcp { "mcp" } else { "cli" });
    let root = s.0.join("repo");
    seed(&s, &root, "a.md", "# Source\n\nHarmless body.\n");
    let mut d = Driver {
        s: &s,
        root: &root,
        server: mcp.then(|| Server::start_at(&s, &root)),
    };
    let refused = d
        .call(
            "ingest_claim",
            &json!({"with":"gliner","path_prefix":"AKIAQWERTYUIOPASDFGH"}),
        )
        .unwrap_err();
    assert!(!refused.contains("AKIAQWERTYUIOPASDFGH"));
    assert_eq!(
        db(&s)
            .query_row("SELECT count(*) FROM ingest_jobs", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let first = d.claim();
    let job = first["job"].as_str().unwrap();
    let fence = first["lease"]["fence"].as_u64().unwrap();
    assert_eq!(fence, 1);
    let ttl: i64 = db(&s)
        .query_row(
            "SELECT expires_at-unixepoch() FROM leases WHERE name LIKE 'ingest:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!((299..=300).contains(&ttl));
    let old = json!({"job":job,"fence":fence});
    for tool in ["ingest_next", "ingest_submit"] {
        db(&s)
            .execute(
                "UPDATE leases SET expires_at=unixepoch()+2 WHERE name LIKE 'ingest:%'",
                [],
            )
            .unwrap();
        let mut request = old.clone();
        if tool == "ingest_next" {
            request["n"] = json!(0);
        }
        d.call(tool, &request).unwrap();
        let ttl: i64 = db(&s)
            .query_row(
                "SELECT expires_at-unixepoch() FROM leases WHERE name LIKE 'ingest:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!((299..=300).contains(&ttl), "implicit {tool} renewal");
    }
    expire(&s);
    for takeover in [false, true] {
        if takeover {
            let second = d.claim();
            assert_eq!(second["job"], first["job"]);
            assert_eq!(second["lease"]["fence"], 2);
        }
        for tool in ["ingest_next", "ingest_submit", "ingest_finish"] {
            let mut request = old.clone();
            if tool == "ingest_next" {
                request["n"] = json!(0);
            }
            let before = state(&s);
            let error = d.call(tool, &request).unwrap_err();
            assert!(error.contains("lease_lost"), "{error}");
            if !mcp {
                assert!(error.contains("exit4:"));
            }
            assert_eq!(
                state(&s),
                before,
                "{tool}: no expired or superseded holder writes"
            );
        }
    }
    let current = json!({"job":job,"fence":2});
    let report = d.call("ingest_finish", &current).unwrap();
    assert_eq!(d.call("ingest_finish", &current).unwrap(), report);
    let marker: i64 = db(&s)
        .query_row(
            "SELECT count(*) FROM derived_artifact WHERE layer='gliner'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marker, 1);
    let before = state(&s);
    assert_eq!(d.claim(), json!({"job":null}));
    assert_eq!(state(&s), before, "idle claim writes nothing");
    // Advance only retention, then ordinary brief invokes the public bounded prune path.
    db(&s)
        .execute(
            "UPDATE ingest_jobs SET expires_at=unixepoch()-1 WHERE status='finished'",
            [],
        )
        .unwrap();
    seed(&s, &root, "b.md", "# Second\n\nOther source.\n");
    ok(&cli(&s, &root, &["ingest", "job", "brief", "b.md"], None));
    let retained: i64 = db(&s)
        .query_row(
            "SELECT count(*) FROM ingest_jobs WHERE job_id=?",
            [job],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retained, 0);
    assert_eq!(d.claim(), json!({"job":null}));
    ok(&cli(&s, &root, &["reindex", "layer", "gliner"], None));
    let reset = d
        .call(
            "ingest_claim",
            &json!({"with":"gliner","path_prefix":"a.md","worker":"replacement:2"}),
        )
        .unwrap();
    assert!(reset["job"].is_string());
    assert_eq!(
        reset["job"], first["job"],
        "job identity stays deterministic"
    );
    let reset_fence = reset["lease"]["fence"].as_u64().unwrap();
    assert!(
        reset_fence > 2,
        "retention and reset cannot recycle a generation"
    );
    let before = state(&s);
    let late_submit = d.call("ingest_submit", &old);
    let late_finish = d.call("ingest_finish", &old);
    assert!(
        late_submit
            .as_ref()
            .is_err_and(|error| error.contains("lease_lost"))
            && late_finish
                .as_ref()
                .is_err_and(|error| error.contains("lease_lost")),
        "prune and reindex must not revive a superseded holder: submit={late_submit:?}, finish={late_finish:?}"
    );
    assert_eq!(
        state(&s),
        before,
        "superseded pre-reset holder cannot write"
    );
    for expected in [reset_fence + 1, reset_fence + 2] {
        expire(&s);
        assert_eq!(d.claim()["lease"]["fence"], expected);
    }
    expire(&s);
    let before = state(&s);
    assert_eq!(d.claim(), json!({"job":null}));
    assert_eq!(state(&s), before);
    let doctor = cli(&s, &root, &["doctor"], None);
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "claim_exhausted" && c["detail"] == "1"),
        "{report}"
    );
    seed(
        &s,
        &root,
        "a.md",
        "# Edited\n\nChanged body resets attempts.\n",
    );
    let edited = d.claim();
    assert!(edited["lease"]["fence"].as_u64().unwrap() > reset_fence + 2);
    // A return to the original content after obsolete-row pruning must not revive old proof.
    expire(&s);
    ok(&cli(&s, &root, &["ingest", "job", "brief", "b.md"], None));
    seed(&s, &root, "a.md", "# Source\n\nHarmless body.\n");
    let reverted = d.claim();
    assert_eq!(reverted["job"], first["job"]);
    assert!(
        reverted["lease"]["fence"].as_u64().unwrap() > edited["lease"]["fence"].as_u64().unwrap()
    );
    let before = state(&s);
    for tool in ["ingest_submit", "ingest_finish"] {
        assert!(d.call(tool, &old).unwrap_err().contains("lease_lost"));
    }
    assert_eq!(
        state(&s),
        before,
        "content reversion cannot revive old proof"
    );
    ok(&cli(&s, &root, &["forget", "--document", "a.md"], None));
    let error = d
        .call(
            "ingest_next",
            &json!({"job":reverted["job"],"fence":reverted["lease"]["fence"],"n":0}),
        )
        .unwrap_err();
    assert!(error.contains("job_revoked"), "{error}");
    assert_eq!(d.claim(), json!({"job":null}));
    let live: i64 = db(&s)
        .query_row(
            "SELECT count(*) FROM leases WHERE name LIKE 'ingest:%' AND expires_at>=unixepoch()",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 0);
    let after_forget = d
        .call(
            "ingest_claim",
            &json!({"with":"gliner","path_prefix":"b.md"}),
        )
        .unwrap();
    assert!(
        after_forget["lease"]["fence"].as_u64().unwrap()
            > reverted["lease"]["fence"].as_u64().unwrap(),
        "revocation must preserve the workspace allocator"
    );
    if let Some(server) = d.server.take() {
        server.finish();
    }
}
#[test]
fn cli_claim_complete_lifecycle() {
    workflow(false);
}
#[test]
fn mcp_claim_complete_lifecycle() {
    workflow(true);
}

fn concurrent_claimers(mcp: bool) {
    let s = Scratch::new(
        "kaban-e310b",
        if mcp {
            "concurrent-mcp"
        } else {
            "concurrent-cli"
        },
    );
    let root = s.0.join("repo");
    seed(&s, &root, "a.md", "# First\n\nOne source.\n");
    seed(&s, &root, "b.md", "# Second\n\nTwo source.\n");
    let barrier = std::sync::Barrier::new(3);
    let mut driver_a = Driver {
        s: &s,
        root: &root,
        server: mcp.then(|| Server::start_at(&s, &root)),
    };
    let mut driver_b = Driver {
        s: &s,
        root: &root,
        server: mcp.then(|| Server::start_at(&s, &root)),
    };
    let run = |d: &mut Driver<'_>| {
        barrier.wait();
        d.call("ingest_claim", &json!({"with":"gliner"})).unwrap()
    };
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| run(&mut driver_a));
        let b = scope.spawn(|| run(&mut driver_b));
        barrier.wait();
        (a.join().unwrap(), b.join().unwrap())
    });
    assert!(a["job"].is_string());
    assert!(b["job"].is_string());
    assert_ne!(a["brief"]["document"]["id"], b["brief"]["document"]["id"]);
    if let Some(server) = driver_a.server.take() {
        server.finish();
    }
    if let Some(server) = driver_b.server.take() {
        server.finish();
    }
}
#[test]
fn independent_process_claimers_take_different_documents() {
    concurrent_claimers(false);
}
#[test]
fn independent_mcp_claimers_take_different_documents() {
    concurrent_claimers(true);
}
