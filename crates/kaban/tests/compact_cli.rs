//! The catalog maintenance boundary (spec 11 T6 Step 5, PC-7, PC-16, KER-09): `kaban compact`,
//! `kaban reindex [text|graph|all]` and the global `--durable` are listed and run; `stats`
//! lists the catalog's indexes; a one-shot write acknowledges before its bounded post-ack
//! checkpoint; `doctor` verifies the catalog and `doctor --recover` recovers it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::Value;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The wall-clock bound of every child: past it the child is killed and the test fails.
const CHILD_BOUND: Duration = Duration::from_secs(30);

/// Wait for `child` within [`CHILD_BOUND`]; past it, kill it and fail.
fn wait_bounded(mut child: Child) -> ExitStatus {
    let deadline = Instant::now() + CHILD_BOUND;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child ran past {CHILD_BOUND:?} and was killed");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Names each run's output files apart.
static RUNS: AtomicU64 = AtomicU64::new(0);

/// Run the binary on the scratch database with `args` (stdin closed), within [`CHILD_BOUND`].
/// Its output goes to files, so a full pipe never stalls it.
fn kaban(s: &Scratch, args: &[&str]) -> Output {
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let (out, err) = (s.0.join(format!("run{n}.out")), s.0.join(format!("run{n}.err")));
    let child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(&s.0)
        .env("KABAN_HOME", s.home())
        .env_remove("KABAN_DB")
        .args(["--json", "--db", s.db().to_str().unwrap()])
        .args(args)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let status = wait_bounded(child);
    Output {
        status,
        stdout: std::fs::read(&out).unwrap(),
        stderr: std::fs::read(&err).unwrap(),
    }
}

/// The one JSON document a successful run printed.
fn document(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// A database with one remembered fact.
fn remembered(s: &Scratch) {
    document(&kaban(s, &["remember", "Fact", "the tide turns at noon"]));
}

/// A connection of the test's own to the scratch database.
fn sql(s: &Scratch) -> rusqlite::Connection {
    rusqlite::Connection::open(s.db()).unwrap()
}

#[test]
fn the_help_lists_compact_reindex_and_the_durable_flag() {
    let s = Scratch::new("kaban-compact-cli", "help");

    let top = kaban(&s, &["--help"]);
    let compact = kaban(&s, &["compact", "--help"]);
    let reindex = kaban(&s, &["reindex", "--help"]);

    let top = String::from_utf8(top.stdout).unwrap();
    for listed in ["compact", "reindex", "--durable"] {
        assert!(top.contains(listed), "kaban --help lacks {listed}: {top}");
    }
    let compact = String::from_utf8(compact.stdout).unwrap();
    for listed in ["seal", "fold", "punch", "checkpoint"] {
        assert!(
            compact.contains(listed),
            "compact --help lacks {listed}: {compact}"
        );
    }
    let reindex = String::from_utf8(reindex.stdout).unwrap();
    for listed in ["text", "graph", "all"] {
        assert!(
            reindex.contains(listed),
            "reindex --help lacks {listed}: {reindex}"
        );
    }
}

#[test]
fn compact_prints_its_report_and_runs_the_due_checkpoint() {
    // Arrange
    let s = Scratch::new("kaban-compact-cli", "compact");
    remembered(&s);

    // Act
    let report = document(&kaban(&s, &["compact"]));

    // Assert: nothing to seal or fold yet; the first checkpoint of a process is due
    assert_eq!(report["published"], serde_json::json!([]), "{report}");
    assert_eq!(report["checkpointed"], true, "{report}");
}

#[test]
fn reindex_defaults_to_every_index_and_rebuilds_text_from_the_rows() {
    // Arrange
    let s = Scratch::new("kaban-compact-cli", "reindex");
    remembered(&s);

    // Act
    let all = document(&kaban(&s, &["reindex"]));
    let graph = document(&kaban(&s, &["reindex", "graph"]));

    // Assert: the graph has no catalog index to rebuild until T11
    assert_eq!(all["rows"], 1, "{all}");
    assert_eq!(all["graph_rebuilt"], false, "{all}");
    assert_eq!(graph["rows"], Value::Null, "{graph}");
}

#[test]
fn stats_lists_the_text_graph_and_code_indexes_of_the_catalog() {
    // Arrange
    let s = Scratch::new("kaban-compact-cli", "stats");
    remembered(&s);

    // Act
    let stats = document(&kaban(&s, &["stats"]));

    // Assert
    let names: Vec<&str> = stats["indexes"]
        .as_array()
        .unwrap_or_else(|| panic!("no indexes: {stats}"))
        .iter()
        .map(|i| i["index"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["text", "graph", "code"], "{stats}");
    assert_eq!(stats["indexes"][0]["layers"], 0, "{stats}");
}

#[test]
fn a_write_over_a_10000_row_sediment_acknowledges_before_its_post_ack_checkpoint() {
    // Arrange: 10,000 pending text sediment rows, at the default seal trigger
    let s = Scratch::new("kaban-compact-cli", "sediment");
    remembered(&s);
    {
        let mut conn = sql(&s);
        let tx = conn.transaction().unwrap();
        for _ in 0..10_000 {
            tx.execute(
                "INSERT INTO cat_sediment(class, generation, payload) VALUES ('text_tier', 1, x'00')",
                [],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let log = s.0.join("remember.log");
    let file = std::fs::File::create(&log).unwrap();

    // Act: stdout and stderr into one file, in write order
    let child = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(&s.0)
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "kaban=debug")
        .args(["--json", "--db", s.db().to_str().unwrap()])
        .args(["remember", "Fact", "the lamp is lit"])
        .stdin(Stdio::null())
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    let status = wait_bounded(child);

    // Assert: the receipt came first, then the checkpoint; the write compacted nothing
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(status.success(), "{text}");
    let ack = text
        .find("\"receipt\"")
        .unwrap_or_else(|| panic!("no ack: {text}"));
    let tick = text
        .find("post-ack checkpoint")
        .unwrap_or_else(|| panic!("no post-ack checkpoint: {text}"));
    assert!(ack < tick, "{text}");
    assert!(text[tick..].contains("ran=true"), "{text}");
    let after = document(&kaban(&s, &["stats"]));
    assert_eq!(after["indexes"][0]["sediment_rows"], 10_000, "{after}");
}

#[test]
fn doctor_lists_a_catalog_inconsistency_and_doctor_recover_clears_it() {
    // Arrange: a live cold extent in main's chain, claimed by a process that is gone
    let s = Scratch::new("kaban-compact-cli", "doctor");
    remembered(&s);
    std::fs::write(s.0.join("cold.extent"), b"cold").unwrap();
    {
        let conn = sql(&s);
        conn.execute_batch(
            "INSERT INTO cat_extent(class, generation, relpath) VALUES ('graph_layer', 1, 'cold.extent');
             INSERT INTO cat_view_extent(view, position, extent_id) VALUES ('main', 0, last_insert_rowid());
             INSERT INTO cat_claim(extent, claim, claimed_at) SELECT id, 'gone', 0 FROM cat_extent;",
        )
        .unwrap();
    }
    let check = |report: &Value, name: &str| -> Value {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} check: {report}"))
    };

    // Act
    let found = kaban(&s, &["doctor"]);
    let recovered = kaban(&s, &["doctor", "--recover"]);

    // Assert: doctor fails naming the stale claim; recovery clears it
    assert_eq!(found.status.code(), Some(5), "{found:?}");
    let found: Value = serde_json::from_slice(&found.stdout).unwrap();
    let verify = check(&found, "catalog_verify");
    assert_eq!(verify["ok"], false, "{verify}");
    assert!(
        verify["detail"].as_str().unwrap().contains("stale claim"),
        "{verify}"
    );
    let recovered: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    let recover = check(&recovered, "catalog_recover");
    assert!(
        recover["detail"]
            .as_str()
            .unwrap()
            .contains("1 stale claim"),
        "{recover}"
    );
    assert_eq!(
        check(&recovered, "catalog_verify")["ok"],
        true,
        "{recovered}"
    );
}

#[test]
fn checkpoint_if_due_ticks_once_per_interval() {
    // Arrange
    let s = Scratch::new("kaban-compact-cli", "tick");
    let store = kaban_store_sqlite::SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap();

    // Act: twice within the default interval
    let first = store.checkpoint_if_due().unwrap();
    let second = store.checkpoint_if_due().unwrap();

    // Assert
    assert!(first.is_some_and(|tick| tick.backfilled), "{first:?}");
    assert_eq!(second, None);
}

/// The doctor check `name` of `report`.
fn doctor_check(report: &Value, name: &str) -> Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no {name} check: {report}"))
}

#[test]
fn doctor_fails_its_catalog_row_naming_a_view_that_needs_reimport() {
    // Arrange: a code view retired to needs_reimport
    let s = Scratch::new("kaban-compact-cli", "reimport");
    remembered(&s);
    let view = format!("code@{}", "3".repeat(40));
    sql(&s)
        .execute(
            "INSERT INTO cat_view(name, state) VALUES (?1, 'needs_reimport')",
            [&view],
        )
        .unwrap();

    // Act
    let out = kaban(&s, &["doctor"]);

    // Assert
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let verify = doctor_check(&report, "catalog_verify");
    assert_eq!(verify["ok"], false, "{verify}");
    assert!(
        verify["detail"].as_str().unwrap().contains(&view),
        "{verify}"
    );
}

#[test]
fn doctor_shows_the_catalog_budget_and_fails_it_after_refused_units() {
    // Arrange: the last compaction refused three units at their smallest size
    let s = Scratch::new("kaban-compact-cli", "budget");
    remembered(&s);
    let before = document(&kaban(&s, &["doctor"]));
    sql(&s)
        .execute(
            "INSERT INTO cat_meta(key, value) VALUES ('last_refused_units', 3)",
            [],
        )
        .unwrap();

    // Act
    let after = kaban(&s, &["doctor"]);

    // Assert: the effective values first, then the refusal and what to raise
    let budget = doctor_check(&before, "catalog_budget");
    assert_eq!(budget["ok"], true, "{budget}");
    let detail = budget["detail"].as_str().unwrap();
    for key in [
        "maintenance_memory_mib",
        "maintenance_deadline_ms",
        "seal_ops",
        "fold_depth",
        "fold_delta_ratio",
        "checkpoint_interval_ms",
        "view_retention",
    ] {
        assert!(detail.contains(key), "{detail}");
    }
    let after: Value = serde_json::from_slice(&after.stdout).unwrap();
    let budget = doctor_check(&after, "catalog_budget");
    assert_eq!(budget["ok"], false, "{budget}");
    assert!(
        budget["detail"]
            .as_str()
            .unwrap()
            .contains("3 units refused at the smallest size: raise catalog.maintenance_memory_mib"),
        "{budget}"
    );
}
