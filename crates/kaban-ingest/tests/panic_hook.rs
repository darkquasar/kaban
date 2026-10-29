//! Review RD3-2 (docs-ingest-spec S1, D8; DI2.2a): a panic inside an extractor must not print
//! its payload. (a) The process-global hook `kaban_ingest::panic_hook::install()` prints one
//! fixed line with the thread name and the panic location and not one byte of the payload:
//! proved by re-executing this test binary (nextest runs one process per test, so the hook of
//! the child touches nothing else) with an env marker that selects the child branch. (b) An
//! extractor that panics is recorded `failed` with the fixed `extractor panicked` text, its
//! sibling documents complete, and the pool accounting is not disturbed by the unwinding
//! closure. Shared rig and fake preparer: `common/pool.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::pool::{Fake, Mode, rig, source_of};
use kaban_core::MemoryStore;
use kaban_ingest::{IngestConfig, document_id};
use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Set in the child's environment only: selects the branch that installs the hook and panics.
const CHILD_MARKER: &str = "KABAN_PANIC_HOOK_CHILD";
/// An invented payload with a path-like tail; none of its fragments may reach stderr.
const PAYLOAD: &str = "CANARY_PANIC_7731 /srv/customers/alice-7781/statement.pdf";
const PAYLOAD_FRAGMENTS: [&str; 3] = ["CANARY_PANIC_7731", "alice-7781", "statement.pdf"];
const THREAD: &str = "extractor-7731";
const FIXED_PREFIX: &str = "kaban: internal error in thread ";

/// (a) The child (this test, re-executed with `--exact` and `--nocapture`) panics on a named
/// thread with the invented payload; the parent reads its stderr and finds the fixed line with
/// a `file:line` location and no fragment of the payload.
#[test]
fn installed_hook_prints_the_location_and_no_byte_of_the_payload() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        // the red (the base bytes) ran this branch without the hook: the default hook printed
        // the payload (`di22a-red-rd3-2.log`)
        kaban_ingest::panic_hook::install();
        let joined = std::thread::Builder::new()
            .name(THREAD.to_owned())
            .spawn(|| panic!("{PAYLOAD}"))
            .unwrap()
            .join();
        assert!(joined.is_err(), "the child thread must have panicked");
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("installed_hook_prints_the_location_and_no_byte_of_the_payload")
        .arg("--nocapture")
        .env(CHILD_MARKER, "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the child failed: {}", out.status);
    for fragment in PAYLOAD_FRAGMENTS {
        assert!(
            !stderr.contains(fragment),
            "the payload fragment {fragment:?} reached stderr:\n{stderr}"
        );
    }
    let line = stderr
        .lines()
        .find(|l| l.starts_with(FIXED_PREFIX))
        .unwrap_or_else(|| panic!("no fixed line in the child's stderr:\n{stderr}"));
    let expected = format!("{FIXED_PREFIX}{THREAD} at ");
    assert!(line.starts_with(&expected), "unexpected line: {line}");
    let (file, number) = line[expected.len()..]
        .rsplit_once(':')
        .expect("a file:line location");
    assert!(
        file.ends_with("panic_hook.rs"),
        "the location names this file as compiled: {file}"
    );
    number.parse::<u32>().expect("a line number");
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.starts_with(FIXED_PREFIX))
            .count(),
        1,
        "one line per panic:\n{stderr}"
    );
}

/// (b) Three documents on a one-worker pool; the second (commit sequence 1) panics inside the
/// preparer. Its record is `failed` with the fixed text, the first and third documents are
/// committed `ok`, and the run-local accounting counts the unwound task as returned: the live
/// gauge never exceeds the worker count and every blocking task is counted complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_extractor_is_recorded_failed_and_its_siblings_complete() {
    let fake = Fake::new(Mode::Panic { on_seq: 1 });
    let r = rig(
        "panic",
        fake.clone(),
        IngestConfig {
            workers: 1,
            queue_depth: 2,
            doc_timeout: Duration::from_secs(10),
            ..IngestConfig::default()
        },
    );
    // sorted order: a-ok.md (seq 0), b-panics.md (seq 1), c-ok.md (seq 2)
    for name in ["a-ok.md", "b-panics.md", "c-ok.md"] {
        fs::write(r.root.join(name), format!("# {name}\n")).unwrap();
    }
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        r.ingestor.run(&r.cfg, CancellationToken::new()),
    )
    .await
    .expect("hang guard")
    .unwrap();
    assert_eq!(
        (report.documents, report.failed, report.timed_out),
        (2, 1, 0),
        "{report:?}"
    );
    let node = r
        .store
        .get(&document_id(&source_of("b-panics.md")), true)
        .unwrap()
        .expect("failed Document recorded");
    assert_eq!(node.props["status"], "failed");
    assert_eq!(node.props["error"], "extractor panicked");
    for ok in ["a-ok.md", "c-ok.md"] {
        let node = r
            .store
            .get(&document_id(&source_of(ok)), true)
            .unwrap()
            .expect(ok);
        assert_eq!(node.props["status"], "ok", "{ok}");
    }
    let m = r.ingestor.metrics();
    assert!(
        m.max_live_blocking <= 1,
        "the unwound task still counts as live: {m:?}"
    );
    assert_eq!(
        m.blocking_completed, 3,
        "the unwound task is not counted as returned: {m:?}"
    );
}
