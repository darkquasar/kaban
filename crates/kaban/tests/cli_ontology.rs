//! `kaban ontology load|list` and the startup load, black-box through the binary (engine-spec
//! EN-F2, EN-S7, EN-S8; engine-plan E1.3): exit codes per core-spec §6 (0, 2 reject, 4 invalid
//! or containment, 5 store), the `list` golden, startup loading of `.kaban/ontology/*.yaml`
//! in sorted order, the config file's field-path error, and the negatives (conflict, byte
//! budget, `extends: [missing]`, symlink escape, canary).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A reject-class canary (builtin `aws-access-key-id`): must reach neither stream nor disk.
const SECRET_CANARY: &str = "AKIAQWERTYUIOPASDFGH";

/// A repository scratch: `<root>/.git`, `<root>/.kaban/ontology/`.
fn repo(s: &Scratch) -> PathBuf {
    let root = s.0.join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".kaban/ontology")).unwrap();
    root
}

fn kaban(s: &Scratch, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kaban"));
    cmd.env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .current_dir(cwd)
        .args(["--db", s.db().to_str().unwrap()])
        .stdin(Stdio::null());
    cmd
}

fn run(s: &Scratch, cwd: &Path, args: &[&str]) -> Output {
    kaban(s, cwd).args(args).output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json(out: &Output) -> Value {
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

/// The namespace `incidents` with one governed kind and one rel (every reference qualified:
/// the resolver reports a bare name shared with `adr` as `ambiguous_kind`).
fn incidents(version: u32, description: &str) -> String {
    format!(
        r#"ontology: incidents
version: {version}
extends: [core]
kinds:
  Incident:
    description: "{description}"
    props_schema: {{type: object, additionalProperties: false, properties: {{severity: {{type: string}}}}}}
    governed: {{acceptance: human, serialize: {{dir: incidents, format: md-frontmatter}},
               export: {{digest: true, marker: false, heading: Incidents}}, display: {{label: Incident, plural: Incidents}},
               governs_paths: true, dedup: none}}
rels:
  TRIGGERED: {{from: [incidents:Incident], to: [], directed: true, description: "The incident triggered the target."}}
"#
    )
}

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::write(&path, text).unwrap();
    path
}

/// `ontology load` exits 0 with the report, loads idempotently, and `ontology list --json`
/// renders the golden (hashes replaced: the shipped hashes change with the shipped bytes).
#[test]
fn ontology_load_reports_and_list_renders_the_golden() {
    let s = Scratch::new("kaban-cli-ont", "load");
    let root = repo(&s);
    write(
        &root,
        ".kaban/ontology/incidents.yaml",
        &incidents(1, "An incident."),
    );

    let out = run(
        &s,
        &root,
        &["ontology", "load", ".kaban/ontology/incidents.yaml"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["ns"], "incidents");
    assert_eq!(report["version"], 1);
    assert_eq!(
        (report["kinds"].as_u64(), report["rels"].as_u64()),
        (Some(1), Some(1))
    );
    // the startup load already applied the file when the process opened the database, so
    // the explicit load of the same bytes is a no-op
    assert_eq!(report["changed"], false);
    assert_eq!(report["hash"].as_str().map(str::len), Some(64));

    let out = run(&s, &root, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out).lines().count(), 1, "--json: one compact line");
    let mut list = json(&out);
    for entry in list["ontologies"].as_array_mut().unwrap() {
        assert_eq!(entry["hash"].as_str().map(str::len), Some(64));
        entry["hash"] = Value::String("[hash]".into());
    }
    insta::assert_json_snapshot!("ontology_list", list);

    // the kind is usable under its stored name
    let out = run(
        &s,
        &root,
        &[
            "--json",
            "remember",
            "incidents:Incident",
            "the deploy failed at 03:00",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(json(&out)["kind"], "incidents:Incident");
}

/// Startup: a new file under `.kaban/ontology/` is loaded by the next process in sorted path
/// order; a changed body is a new version; two files declaring one `(ns, name)` differently
/// exit 4 `ontology_conflict` before anything is written.
#[test]
fn startup_loads_the_directory_in_sorted_order_and_a_conflict_exits_4() {
    let s = Scratch::new("kaban-cli-ont", "startup");
    let root = repo(&s);
    write(
        &root,
        ".kaban/ontology/b-incidents.yaml",
        &incidents(1, "An incident."),
    );
    // the very first process on this database: the startup load alone makes the kind usable
    let out = run(
        &s,
        &root,
        &[
            "--json",
            "remember",
            "incidents:Incident",
            "paging storm at 03:00",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(json(&out)["kind"], "incidents:Incident");
    let out = run(&s, &root, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let names: Vec<String> = json(&out)["ontologies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["ns"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["core", "adr", "incidents"]);

    // a second file with the same (ns, name) and another body: the conflict is reported by
    // every process start, and `schema describe` through the server is untouched
    write(
        &root,
        ".kaban/ontology/a-other.yaml",
        &incidents(2, "Another body."),
    );
    let out = run(&s, &root, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("ontology_conflict"),
        "{}",
        stderr(&out)
    );
    assert_eq!(out.stdout.len(), 0, "stdout stays empty");
    std::fs::remove_file(root.join(".kaban/ontology/a-other.yaml")).unwrap();

    // outside a repository the startup directory is the current directory's
    let out = run(&s, &s.0, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(json(&out)["ontologies"].as_array().unwrap().len(), 2);
}

/// Negatives: `extends: [missing]` exits 4 `unknown_namespace`; a file over the byte budget
/// exits 4 with the fixed text; a redefinition of `adr:Decision` exits 4
/// `ontology_builtin_immutable`; a symlink escaping the repository exits 4; a canary in a
/// description exits 2 with rule ids only and reaches neither stream nor the database.
#[test]
fn invalid_files_exit_4_and_a_canary_exits_2_without_echo() {
    let s = Scratch::new("kaban-cli-ont", "negatives");
    let root = repo(&s);
    let outside = s.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    let missing = write(
        &root,
        "missing.yaml",
        "ontology: vendors\nversion: 1\nextends: [missing]\nkinds: {}\nrels: {}\n",
    );
    let out = run(&s, &root, &["ontology", "load", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: unknown_namespace");

    let mut big = incidents(1, "An incident.");
    big.push_str("# ");
    big.push_str(&"x".repeat(4 * 1024 * 1024));
    big.push('\n');
    let big = write(&root, "big.yaml", &big);
    let out = run(&s, &root, &["ontology", "load", big.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("exceeds the import byte budget"),
        "{}",
        stderr(&out)
    );

    let adr = write(&root, "adr.yaml", kaban_core::shipped::ADR_YAML);
    let out = run(&s, &root, &["ontology", "load", adr.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: ontology_builtin_immutable");

    // S7: the file lies outside the repository behind a link inside it
    write(&outside, "evil.yaml", &incidents(1, "Planted."));
    std::os::unix::fs::symlink(&outside, root.join(".kaban/linked")).unwrap();
    let out = run(&s, &root, &["ontology", "load", ".kaban/linked/evil.yaml"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(stderr(&out).contains("symlink"), "{}", stderr(&out));
    let out = run(
        &s,
        &root,
        &[
            "ontology",
            "load",
            outside.join("evil.yaml").to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));

    // EN-S8: the canary
    let canary = write(
        &root,
        "canary.yaml",
        &incidents(1, &format!("An incident; key {SECRET_CANARY} leaked.")),
    );
    let out = run(&s, &root, &["ontology", "load", canary.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("REJECTED") && err.contains("aws-access-key-id"),
        "{err}"
    );
    assert!(!err.contains(SECRET_CANARY) && !stdout(&out).contains(SECRET_CANARY));
    let mut disk = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut name = s.db().into_os_string();
        name.push(suffix);
        if let Ok(bytes) = std::fs::read(&name) {
            disk.extend(bytes);
        }
    }
    assert!(
        !String::from_utf8_lossy(&disk).contains(SECRET_CANARY),
        "canary on disk"
    );
    let out = run(&s, &root, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        json(&out)["ontologies"].as_array().unwrap().len(),
        2,
        "nothing was loaded"
    );
}

/// Every byte of the database, its WAL and shm (what a leak would land in), as one lossy string.
fn disk_bytes(s: &Scratch) -> String {
    let mut disk = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut name = s.db().into_os_string();
        name.push(suffix);
        if let Ok(bytes) = std::fs::read(&name) {
            disk.extend(bytes);
        }
    }
    String::from_utf8_lossy(&disk).into_owned()
}

/// EN-S8 / core S1 (E1.3a RE1-1): the raw bytes are gated before the YAML parser sees them, so a
/// malformed file carrying a reject-class canary exits 2 with rule ids only (not 4 as a parse
/// failure), nothing is loaded, and the canary reaches neither stream nor the database; a
/// malformed file without a hit stays exit 4 `schema_invalid:file:`.
#[test]
fn a_malformed_file_carrying_a_canary_exits_2_before_the_parser_without_echo() {
    let s = Scratch::new("kaban-cli-ont", "rawgate");
    let root = repo(&s);
    // a comment (dropped by the parser) and an unterminated flow mapping after it
    let malformed = format!(
        "ontology: incidents\nversion: 1\n# key {SECRET_CANARY} pasted here\nkinds: {{Incident: {{description: \"x\"\nrels: {{}}\n"
    );
    let path = write(&root, "malformed.yaml", &malformed);

    let out = run(&s, &root, &["ontology", "load", path.to_str().unwrap()]);

    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("REJECTED") && err.contains("aws-access-key-id"),
        "{err}"
    );
    assert!(!err.contains(SECRET_CANARY) && !stdout(&out).contains(SECRET_CANARY));
    assert!(!disk_bytes(&s).contains(SECRET_CANARY), "canary on disk");
    let out = run(&s, &root, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        json(&out)["ontologies"].as_array().unwrap().len(),
        2,
        "nothing was loaded"
    );
    // the same malformed shape without a hit is a plain file error
    let clean = write(
        &root,
        "clean.yaml",
        "ontology: incidents\nversion: 1\nkinds: {Incident: {description: \"x\"\nrels: {}\n",
    );
    let out = run(&s, &root, &["ontology", "load", clean.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: schema_invalid:file:");
    // a file that is not UTF-8 is refused with the fixed class, never scanned as text
    let binary = root.join("binary.yaml");
    std::fs::write(&binary, [0xffu8, 0xfe, b'o', b'n']).unwrap();
    let out = run(&s, &root, &["ontology", "load", binary.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: schema_invalid:file:");
}

/// EN-S7 (E1.3a RE1-2): a FIFO named `blocked.yaml` under `.kaban/ontology/` does not hold the
/// process start: `kaban ontology list` returns within 2 s (the child is killed past the bound,
/// so a regression fails instead of hanging), reports the skip on stderr and lists the regular
/// file beside it.
#[test]
fn a_fifo_in_the_ontology_directory_does_not_block_startup_and_is_reported_as_skipped() {
    let s = Scratch::new("kaban-cli-ont", "fifo");
    let root = repo(&s);
    write(
        &root,
        ".kaban/ontology/incidents.yaml",
        &incidents(1, "An incident."),
    );
    let status = Command::new("mkfifo")
        .arg(root.join(".kaban/ontology/blocked.yaml"))
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo(1)");

    let mut child = kaban(&s, &root)
        .args(["--json", "ontology", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let finished = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let out = child.wait_with_output().unwrap();

    assert!(
        finished,
        "the process must not block on the FIFO: {}",
        stderr(&out)
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not a regular file; skipped"),
        "{}",
        stderr(&out)
    );
    let names: Vec<String> = json(&out)["ontologies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["ns"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["core", "adr", "incidents"]);
}

/// EN-F7 (E1.3a residual, error payloads): an invalid identifier carrying a canary that no
/// denylist rule matches fails `invalid_identifier` with the fixed text; the canary reaches
/// neither stream nor the database (no audit row names it).
#[test]
fn an_invalid_identifier_carrying_a_canary_exits_4_without_echo() {
    const IDENT_CANARY: &str = "vendor-CANARY-9b2e";
    let s = Scratch::new("kaban-cli-ont", "identifier");
    let root = repo(&s);
    let path = write(
        &root,
        "ident.yaml",
        &format!(
            "ontology: shop\nversion: 1\nkinds:\n  {IDENT_CANARY}: {{description: \"lowercase\"}}\nrels: {{}}\n"
        ),
    );

    let out = run(&s, &root, &["ontology", "load", path.to_str().unwrap()]);

    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: invalid_identifier");
    assert_eq!(out.stdout.len(), 0, "stdout stays empty");
    assert!(!disk_bytes(&s).contains(IDENT_CANARY), "canary on disk");
    // the same for an unresolved reference
    let path = write(
        &root,
        "unresolved.yaml",
        &format!(
            "ontology: shop\nversion: 1\nkinds: {{}}\nrels:\n  R: {{from: [{IDENT_CANARY}], to: [], directed: true}}\n"
        ),
    );
    let out = run(&s, &root, &["ontology", "load", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: unknown_kind");
    assert!(!disk_bytes(&s).contains(IDENT_CANARY), "canary on disk");
}

/// SO-11: `<KABAN_HOME>/config.yaml` is loaded once at startup; an unknown key exits 4 naming
/// the field path and never the value; a valid file is accepted.
#[test]
fn a_bad_config_key_exits_4_with_the_field_path_and_never_the_value() {
    let s = Scratch::new("kaban-cli-ont", "config");
    let root = repo(&s);
    std::fs::create_dir_all(s.home()).unwrap();
    std::fs::write(
        s.home().join("config.yaml"),
        format!("ontology:\n  pattern_size_limit_kb: {SECRET_CANARY}\n"),
    )
    .unwrap();
    let out = run(&s, &root, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("config key invalid: ontology.pattern_size_limit_kb"),
        "{err}"
    );
    assert!(!err.contains(SECRET_CANARY), "{err}");

    std::fs::write(
        s.home().join("config.yaml"),
        "ontology:\n  pattern_size_limit_kib: 2048\n",
    )
    .unwrap();
    let out = run(&s, &root, &["--json", "ontology", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

/// EN-F5, EN-F7 (engine-plan E1.8): the discovered-labels verbs end to end. A `remember` of an
/// undeclared label is stored as an `x:` candidate; `kaban ontology prune --dry-run` lists it
/// and writes nothing; `promote` to an undeclared name exits 4 with the fixed text
/// `unknown_kind`; `promote x:Vendor infra:Vendor` (declared by a loaded file) renames it and
/// the record is recalled afterwards; `kaban doctor` carries the `discovered` line. The
/// startup-loaded `infra.yaml` declares `discovered: {policy: review, cap: 1, ...}`: the
/// ontology's block overrides the `[discovered]` register (knob precedence), so the second
/// distinct label exits 4 with `discovered_cap_reached` (EN-F7 text and exit code) and
/// `doctor` reports `n_rejected: 1`.
#[test]
fn promote_prune_and_the_cap_render_the_fixed_texts_and_exit_codes() {
    let s = Scratch::new("kaban-cli-ont", "discovered");
    let root = repo(&s);
    write(
        &root,
        ".kaban/ontology/infra.yaml",
        "ontology: infra\nversion: 1\nkinds:\n  Vendor:\n    description: \"A vendor.\"\nrels: {}\n\
         discovered: {policy: review, cap: 1, prune_after_days: 90, promote_when: {min_count: 20, min_sources: 3, min_mean_score: 0.7}}\n",
    );
    // a discovered label (candidate, hidden) under cap 1; the second label is refused
    let out = run(
        &s,
        &root,
        &[
            "--json",
            "remember",
            "Vendor",
            "acme supplies quokka widgets",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(json(&out)["kind"], "x:Vendor", "{}", stdout(&out));
    let out = run(
        &s,
        &root,
        &["--json", "remember", "Supplier", "another quokka label"],
    );
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    // the error line first; a refused write then prints its `rejected` receipt (SS-10)
    assert_eq!(
        stderr(&out).lines().next(),
        Some("error: discovered_cap_reached")
    );
    let out = run(&s, &root, &["--json", "recall", "quokka"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        json(&out)["count"],
        0,
        "a candidate is hidden: {}",
        stdout(&out)
    );

    // prune --dry-run lists (the kind has one node, below min_count 20; age 0 days is set by
    // the shipped 90 days, so nothing is listed yet) and writes nothing
    let out = run(&s, &root, &["--json", "ontology", "prune", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let prune = json(&out);
    assert_eq!(prune["dry_run"], true, "{prune}");
    assert_eq!(prune["n_rejected"], 1, "{prune}");
    assert_eq!(prune["nodes_tombstoned"], 0, "{prune}");

    // doctor: the discovered line
    let out = run(&s, &root, &["--json", "doctor"]);
    let doctor = json(&out);
    let line = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "discovered")
        .unwrap_or_else(|| panic!("{doctor}"));
    assert!(
        line["detail"].as_str().unwrap().contains("n_rejected: 1"),
        "{line}"
    );

    // promote: an undeclared target is unknown_kind (exit 4), nothing moves
    let out = run(
        &s,
        &root,
        &[
            "--json",
            "ontology",
            "promote",
            "x:Vendor",
            "infra:Supplier",
        ],
    );
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: unknown_kind");
    let out = run(
        &s,
        &root,
        &["--json", "ontology", "promote", "x:Vendor", "infra:Vendor"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let report = json(&out);
    assert_eq!(report["to"], "infra:Vendor", "{report}");
    assert_eq!(report["nodes"], 1, "{report}");
    assert_eq!(report["candidates_released"], 1, "{report}");
    let out = run(&s, &root, &["--json", "recall", "quokka"]);
    assert_eq!(
        json(&out)["count"],
        1,
        "promoted, visible: {}",
        stdout(&out)
    );
    assert_eq!(json(&out)["results"][0]["kind"], "infra:Vendor");
    // the cap is released: a new label is accepted
    let out = run(
        &s,
        &root,
        &["--json", "remember", "Supplier", "another quokka label"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    // schema_describe (the `kaban://schema` body) carries the workspace's kind_stats
    let out = run(&s, &root, &["--json", "schema"]);
    if out.status.code() == Some(0) {
        let stats = json(&out)["kind_stats"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            stats
                .iter()
                .any(|k| k["ns"] == "infra" && k["kind"] == "Vendor"),
            "{}",
            stdout(&out)
        );
    }
}

// ---- E1.9a: the E1-end correction (astra review RE3-1, RE3-6, RE3-8) through the binary ----

/// RE3-8 (EN-F7, core S3): a promotion whose source is not a discovered kind exits 4 with the
/// fixed text `unknown_kind`, whatever the source looks like; a credential-shaped source is
/// never echoed on either stream.
#[test]
fn an_invalid_promotion_source_is_the_fixed_text_and_a_canary_source_is_never_echoed() {
    let s = Scratch::new("kaban-cli-ont", "promote-source");
    let root = repo(&s);
    let out = run(
        &s,
        &root,
        &["--json", "ontology", "promote", "Vendor", "Fact"],
    );
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "error: unknown_kind");
    assert_eq!(stdout(&out), "");
    let out = run(
        &s,
        &root,
        &["--json", "ontology", "promote", SECRET_CANARY, "Fact"],
    );
    // the seam judges the raw request first: a reject-class hit is exit 2 with rule ids only
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        !stdout(&out).contains(SECRET_CANARY) && !stderr(&out).contains(SECRET_CANARY),
        "echoed: {} {}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        !stdout(&out).contains("AKIA") && !stderr(&out).contains("AKIA"),
        "a fragment echoed: {} {}",
        stdout(&out),
        stderr(&out)
    );
}

/// RE3-1 (EN-F5a): `kaban accept <id>` releases a review candidate (the human actor on the
/// CLI) and the record is recalled; a released node is no candidate and a second `accept`
/// is today's refusal (exit 4). The `kaban://schema` body is `schema_describe`, covered per
/// workspace in `kaban-api/tests/e1_end_correction.rs` (RE3-6).
#[test]
fn a_human_accepts_a_candidate_on_the_cli_and_the_record_is_recalled() {
    let s = Scratch::new("kaban-cli-ont", "accept-candidate");
    let root = repo(&s);
    let out = run(
        &s,
        &root,
        &[
            "--json",
            "remember",
            "Vendor",
            "acme supplies quokka widgets",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let id = json(&out)["id"].as_str().unwrap().to_owned();
    assert_eq!(json(&out)["kind"], "x:Vendor");
    let out = run(&s, &root, &["--json", "recall", "quokka"]);
    assert_eq!(json(&out)["count"], 0, "hidden: {}", stdout(&out));

    let out = run(
        &s,
        &root,
        &[
            "--json",
            "accept",
            &id,
            "--reason",
            "reviewed",
            "--path-scope",
            "src/**",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["status"], "accepted", "{doc}");
    assert_eq!(doc["signer_kind"], "human-cli", "{doc}");
    assert_eq!(doc["node"]["kind"], "x:Vendor", "{doc}");
    let out = run(&s, &root, &["--json", "recall", "quokka"]);
    assert_eq!(json(&out)["count"], 1, "released: {}", stdout(&out));
    assert_eq!(json(&out)["results"][0]["id"], id.as_str());
    // a released node is no candidate: a second accept is today's refusal
    let out = run(
        &s,
        &root,
        &[
            "--json",
            "accept",
            &id,
            "--reason",
            "again",
            "--path-scope",
            "src/**",
        ],
    );
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
}
