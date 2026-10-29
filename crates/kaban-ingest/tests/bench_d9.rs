//! D9 performance campaign (docs-ingest-plan.md §8 DI2.2 and §8.1; docs-ingest-spec D9;
//! core-plan.md §16 corpus seed and machine profile; OWN-12 and OWN-68 SC-13: one run).
//!
//! `#[ignore]`: run by `bash scripts/bench/bench.sh d9 <binary>` under the project lock, never with the
//! suite. The test is a driver around the release `kaban` binary named in `KABAN_D9_BIN` (the
//! §8.1 "exact release binary with `pdf_oxide` enabled"; its sha256 is recorded, and checked
//! against `KABAN_D9_SHA256` when that is set). It generates two corpora into a [`Scratch`]
//! tree (removed on drop) from the §16 xorshift seed and a 5,000-term Zipfian vocabulary
//! (s = 1.07): `small/` = 200 Markdown files + 5 born-digital PDFs written with `pdf_oxide`'s
//! `PdfWriter`, `large/` = 10,000 Markdown files. Every measured run is one child process
//! `KABAN_WORKSPACE=d9 <bin> --db <fresh.db> --json ingest <corpus> --workers 4`, timed from
//! spawn to exit. **Warm** means: the corpus files were just written (page cache warm) and the
//! binary ran once before on a 5-file `warmup/` directory with its own fresh database.
//!
//! Peak RSS of the large run is the child's `VmHWM` read from `/proc/<pid>/status` while it
//! runs (polled every few milliseconds; the high-water mark is monotonic, so the last read
//! before exit is the peak up to that moment). `rustix` 1.1.5 exposes no `getrusage`, and the
//! workspace forbids `unsafe`, so the libc route is not available; the bound is informational
//! (OD-D2).
//!
//! The result file (`KABAN_D9_OUT`) is written **before** the gates are asserted, so a miss
//! still leaves the figure on disk; `../specs/kaban/archive/benchmarks/kaban2/scripts/bench/bench_validate.py` validates it against
//! `../specs/kaban/archive/benchmarks/kaban-original/benchmarks/results/schema-d9.json`.
//!
//! `KABAN_D9_SMOKE=1` is a development aid, not a campaign: the corpora shrink to 2 md + 1 PDF
//! and 100 md, the counts are still asserted, the gates are recorded but not asserted, and the
//! result carries `"smoke": true` (it does not validate against the campaign schema).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::print_stderr)] // the bench driver narrates its steps on stderr
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // timings and sizes into f64/usize

mod common;

use common::Scratch;
use pdf_oxide::writer::PdfWriter;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// core-plan.md §16: the deterministic corpus seed.
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// §16: a 5,000-term vocabulary with Zipfian frequencies, s = 1.07.
const VOCAB: usize = 5_000;
const ZIPF_S: f64 = 1.07;
/// §8.1 row 1: 200 Markdown files + 5 PDFs; row 2: 10,000 Markdown files.
const SMALL_MD: usize = 200;
const SMALL_PDF: usize = 5;
const LARGE_MD: usize = 10_000;
const WARMUP_MD: usize = 4;
const WARMUP_PDF: usize = 1;
/// The gates (docs-ingest-plan.md DI2.2): small warm < 5 s, large < 2 min; RSS informational
/// (OD-D2: 300 MB = 307,200 kB).
const SMALL_LIMIT_S: f64 = 5.0;
const LARGE_LIMIT_S: f64 = 120.0;
const RSS_LIMIT_KB: u64 = 307_200;
const WORKERS: &str = "4";
const WORKSPACE: &str = "d9";
const FEATURES: &str = "ingest,kaban-ingest/pdf";

/// xorshift64 (the §16 generator shape), seeded once per corpus.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The Zipfian vocabulary: 5,000 pronounceable pseudo-words and the cumulative weights
/// `1 / k^s` that rank them.
struct Vocab {
    terms: Vec<String>,
    cdf: Vec<f64>,
}

impl Vocab {
    fn new(rng: &mut Rng) -> Self {
        const ONSETS: [&str; 20] = [
            "b", "d", "f", "g", "k", "l", "m", "n", "p", "r", "s", "t", "v", "z", "br", "kr", "st",
            "tr", "pl", "gr",
        ];
        const NUCLEI: [&str; 10] = ["a", "e", "i", "o", "u", "ai", "ea", "io", "ou", "ue"];
        const CODAS: [&str; 8] = ["", "", "n", "r", "s", "l", "t", "k"];
        let mut terms = Vec::with_capacity(VOCAB);
        let mut seen = std::collections::HashSet::with_capacity(VOCAB);
        while terms.len() < VOCAB {
            let syllables = rng.range(1, 3);
            let mut t = String::new();
            for _ in 0..syllables {
                t.push_str(ONSETS[rng.below(ONSETS.len())]);
                t.push_str(NUCLEI[rng.below(NUCLEI.len())]);
                t.push_str(CODAS[rng.below(CODAS.len())]);
            }
            if t.len() >= 3 && seen.insert(t.clone()) {
                terms.push(t);
            }
        }
        let mut cdf = Vec::with_capacity(VOCAB);
        let mut acc = 0.0;
        for k in 1..=VOCAB {
            acc += 1.0 / (k as f64).powf(ZIPF_S);
            cdf.push(acc);
        }
        let total = acc;
        for c in &mut cdf {
            *c /= total;
        }
        Self { terms, cdf }
    }

    /// One term drawn by Zipfian rank.
    fn term(&self, rng: &mut Rng) -> &str {
        let u = rng.unit();
        let i = self.cdf.partition_point(|&c| c < u).min(VOCAB - 1);
        &self.terms[i]
    }

    fn words(&self, rng: &mut Rng, n: usize) -> String {
        let mut s = String::new();
        for i in 0..n {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(self.term(rng));
        }
        s
    }

    fn title(&self, rng: &mut Rng) -> String {
        let mut words: Vec<String> = (0..rng.range(2, 4))
            .map(|_| {
                let t = self.term(rng);
                let mut c = t.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
            })
            .collect();
        words.dedup();
        words.join(" ")
    }
}

/// One Markdown document: frontmatter title, 2 to 6 headings, 3 to 8 paragraphs of 40 to 120
/// words, a few `[[wikilinks]]` and `#tags` so link resolution and tagging run (D3, D5).
fn markdown(vocab: &Vocab, rng: &mut Rng) -> String {
    let title = vocab.title(rng);
    let mut s = format!("---\ntitle: {title}\n---\n\n# {title}\n\n");
    let headings = rng.range(2, 6);
    let paragraphs = rng.range(3, 8).max(headings);
    let links = rng.range(1, 3);
    let tags = rng.range(1, 2);
    for h in 0..headings {
        let _ = writeln!(s, "## {}\n", vocab.title(rng));
        // spread the paragraphs over the headings, at least one each
        let share = if h + 1 == headings {
            paragraphs - (paragraphs / headings) * h
        } else {
            paragraphs / headings
        };
        for _ in 0..share.max(1) {
            let n = rng.range(40, 120);
            let mut p = vocab.words(rng, n);
            p.push('.');
            let _ = writeln!(s, "{p}\n");
        }
    }
    let mut tail = String::from("See ");
    for i in 0..links {
        if i > 0 {
            tail.push_str(", ");
        }
        let _ = write!(tail, "[[{}]]", vocab.term(rng));
    }
    for _ in 0..tags {
        let _ = write!(tail, " #{}", vocab.term(rng));
    }
    tail.push_str(".\n");
    s.push_str(&tail);
    s
}

/// One born-digital PDF of 3 to 8 pages written the way `examples/gen_pdf_fixtures.rs` does:
/// a bold heading and hand-placed Helvetica lines per page (no layout engine, no dates). The
/// bytes are not committed, so the generator's object-order canonicalisation is not needed.
fn pdf(vocab: &Vocab, rng: &mut Rng) -> Vec<u8> {
    const BODY: &str = "Helvetica";
    const BOLD: &str = "Helvetica-Bold";
    const LINE_CHARS: usize = 80;
    let mut writer = PdfWriter::new();
    for _ in 0..rng.range(3, 8) {
        let mut page = writer.add_a4_page();
        page.add_text(&vocab.title(rng), 72.0, 760.0, BOLD, 18.0);
        let mut y = 720.0;
        let paragraphs = rng.range(2, 4);
        for _ in 0..paragraphs {
            let n = rng.range(50, 110);
            let words = vocab.words(rng, n);
            let mut line = String::new();
            for w in words.split(' ') {
                if line.len() + w.len() + 1 > LINE_CHARS {
                    page.add_text(&line, 72.0, y, BODY, 11.0);
                    y -= 14.0;
                    line.clear();
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(w);
                if y < 90.0 {
                    break;
                }
            }
            if !line.is_empty() && y >= 90.0 {
                page.add_text(&format!("{line}."), 72.0, y, BODY, 11.0);
                y -= 14.0;
            }
            y -= 20.0;
            if y < 90.0 {
                break;
            }
        }
        page.finish();
    }
    writer
        .finish()
        .expect("pdf_oxide writes the generated document")
}

/// What one corpus directory holds.
#[derive(Debug, Default)]
struct CorpusStats {
    md: usize,
    pdf: usize,
    bytes: u64,
}

/// Write `md` Markdown files and `pdfs` PDFs under `dir`; the generator is seeded from
/// `SEED ^ salt`, so the three corpora differ but are reproducible run to run.
fn write_corpus(dir: &Path, vocab: &Vocab, salt: u64, md: usize, pdfs: usize) -> CorpusStats {
    std::fs::create_dir_all(dir).unwrap();
    let mut rng = Rng(SEED ^ salt);
    let mut stats = CorpusStats::default();
    for i in 0..md {
        let text = markdown(vocab, &mut rng);
        stats.bytes += text.len() as u64;
        std::fs::write(dir.join(format!("doc-{i:05}.md")), text).unwrap();
        stats.md += 1;
    }
    for i in 0..pdfs {
        let bytes = pdf(vocab, &mut rng);
        stats.bytes += bytes.len() as u64;
        std::fs::write(dir.join(format!("paper-{i:02}.pdf")), bytes).unwrap();
        stats.pdf += 1;
    }
    stats
}

/// One measured child run.
struct Run {
    wall_s: f64,
    max_rss_kb: Option<u64>,
    report: Value,
}

fn vm_hwm_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

/// `KABAN_WORKSPACE=d9 <bin> --db <db> --json ingest <corpus> --workers 4` from `root`, timed
/// from spawn to exit; stdout is the one JSON report (`IndexDocsResponse`).
fn ingest(bin: &Path, root: &Path, home: &Path, db: &Path, corpus: &str) -> Run {
    let started = Instant::now();
    let mut child = Command::new(bin)
        .args(["--db"])
        .arg(db)
        .args(["--json", "ingest", corpus, "--workers", WORKERS])
        .env("KABAN_WORKSPACE", WORKSPACE)
        .env("KABAN_HOME", home)
        .env_remove("KABAN_POLICY")
        .env_remove("KABAN_DB")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the campaign binary starts");
    let pid = child.id();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).unwrap();
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).unwrap();
        s
    });
    let mut max_rss_kb = None;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if let Some(kb) = vm_hwm_kb(pid) {
            max_rss_kb = Some(max_rss_kb.map_or(kb, |m: u64| m.max(kb)));
        }
        std::thread::sleep(Duration::from_millis(3));
    };
    let wall_s = started.elapsed().as_secs_f64();
    let stdout = out.join().unwrap();
    let stderr = err.join().unwrap();
    assert!(
        status.success(),
        "ingest {corpus} exited {status}; stderr tail: {}",
        &stderr[stderr.len().saturating_sub(2_000)..]
    );
    let report: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("ingest {corpus}: stdout is not one JSON report ({e}): {stdout:?}")
    });
    Run {
        wall_s,
        max_rss_kb,
        report,
    }
}

fn count(report: &Value, key: &str) -> u64 {
    report[key]
        .as_u64()
        .unwrap_or_else(|| panic!("report has no {key}: {report}"))
}

fn counts(report: &Value) -> Value {
    json!({
        "documents": count(report, "documents"),
        "skipped": count(report, "skipped"),
        "failed": count(report, "failed"),
        "partial": count(report, "partial"),
        "blocked": count(report, "blocked"),
        "chunks_upserted": count(report, "chunks_upserted"),
        "chunks_unchanged": count(report, "chunks_unchanged"),
        "outbox_rows": count(report, "outbox_rows"),
        "elapsed_ms": count(report, "elapsed_ms"),
    })
}

/// Bytes of the database and its sidecars.
fn db_bytes(db: &Path) -> u64 {
    let mut total = std::fs::metadata(db).map_or(0, |m| m.len());
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        total += std::fs::metadata(side).map_or(0, |m| m.len());
    }
    total
}

fn sh(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Option<String> {
    let mut c = Command::new(cmd);
    c.args(args);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = c.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn proc_field(path: &str, key: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_owned())
    })
}

fn sha256(path: &Path) -> String {
    let out = sh("sha256sum", &[path.to_str().unwrap()], None).expect("sha256sum runs");
    out.split_whitespace().next().unwrap().to_owned()
}

/// The `schema.json` machine block (core-plan.md §16) for the campaign binary, plus `features`.
fn machine(bin: &Path, sha: &str) -> Value {
    let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned());
    let kernel = read("/proc/sys/kernel/osrelease").unwrap_or_default();
    let uname = format!(
        "{} {} {}",
        read("/proc/sys/kernel/ostype").unwrap_or_else(|| std::env::consts::OS.to_owned()),
        kernel,
        std::env::consts::ARCH
    );
    let cpu = proc_field("/proc/cpuinfo", "model name").unwrap_or_else(|| {
        format!(
            "implementer {} part {}",
            proc_field("/proc/cpuinfo", "CPU implementer").unwrap_or_else(|| "?".into()),
            proc_field("/proc/cpuinfo", "CPU part").unwrap_or_else(|| "?".into())
        )
    });
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let version = sh(bin.to_str().unwrap(), &["--version"], None)
        .and_then(|v| v.split_whitespace().last().map(str::to_owned))
        .unwrap_or_default();
    let load = read("/proc/loadavg").unwrap_or_default();
    json!({
        "uname": uname,
        "kernel": kernel,
        "cpu": cpu,
        "nproc": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        "mem_total_kb": proc_field("/proc/meminfo", "MemTotal")
            .and_then(|v| v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok())),
        "rustc": sh("rustc", &["--version"], None),
        "cargo": sh("cargo", &["--version"], None),
        "git_head": sh("git", &["rev-parse", "HEAD"], Some(&repo)),
        "git_dirty": sh("git", &["status", "--porcelain"], Some(&repo)).is_some_and(|s| !s.is_empty()),
        "binary_path": bin.display().to_string(),
        "binary_sha256": sha,
        "binary_bytes": std::fs::metadata(bin).unwrap().len(),
        "target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        "profile": "release",
        "kaban_version": version,
        "features": FEATURES,
        "loadavg_at_start": load,
    })
}

fn gate(value: f64, limit: f64) -> Value {
    json!({ "value": value, "limit": limit, "pass": value < limit })
}

/// The one D9 campaign: both §8.1 rows on the same binary, one run each.
#[test]
#[ignore = "D9 campaign: run by scripts/bench/bench.sh d9 <binary> under the project lock"]
fn d9_campaign_small_warm_and_large_corpora_on_the_campaign_binary() {
    let bin = PathBuf::from(
        std::env::var_os("KABAN_D9_BIN").expect("KABAN_D9_BIN names the release kaban binary"),
    );
    assert!(
        bin.is_file(),
        "KABAN_D9_BIN {} is not a file",
        bin.display()
    );
    let out = PathBuf::from(
        std::env::var_os("KABAN_D9_OUT").expect("KABAN_D9_OUT names the result file to write"),
    );
    let sha = sha256(&bin);
    if let Ok(expected) = std::env::var("KABAN_D9_SHA256") {
        assert_eq!(
            sha,
            expected,
            "KABAN_D9_SHA256 does not match {}",
            bin.display()
        );
    }
    let machine = machine(&bin, &sha);
    eprintln!(
        "d9: binary {} sha256 {sha} ({} B)",
        bin.display(),
        machine["binary_bytes"]
    );
    let smoke = std::env::var_os("KABAN_D9_SMOKE").is_some_and(|v| v == "1");
    let (small_md, small_pdf, large_md) = if smoke {
        (SMALL_MD / 100, 1, LARGE_MD / 100)
    } else {
        (SMALL_MD, SMALL_PDF, LARGE_MD)
    };

    // Arrange: the corpora, written fresh (page cache warm), and the warm-up run
    let scratch = Scratch::new("kaban-d9", "campaign");
    let home = scratch.join("home");
    let mut rng = Rng(SEED);
    let vocab = Vocab::new(&mut rng);
    let t = Instant::now();
    let warm_stats = write_corpus(&scratch.join("warmup"), &vocab, 0xA5, WARMUP_MD, WARMUP_PDF);
    let small_stats = write_corpus(&scratch.join("small"), &vocab, 0x01, small_md, small_pdf);
    let large_stats = write_corpus(&scratch.join("large"), &vocab, 0x02, large_md, 0);
    eprintln!(
        "d9: corpora written in {:.2} s: warmup {warm_stats:?}, small {small_stats:?}, large {large_stats:?}",
        t.elapsed().as_secs_f64()
    );
    assert_eq!((small_stats.md, small_stats.pdf), (small_md, small_pdf));
    assert_eq!(large_stats.md, large_md);

    let warm = ingest(&bin, &scratch, &home, &scratch.join("warmup.db"), "warmup");
    eprintln!("d9: warm-up {:.2} s: {}", warm.wall_s, counts(&warm.report));
    assert_eq!(
        count(&warm.report, "failed"),
        0,
        "the campaign binary must carry the PDF extractor (§8.1): the warm-up PDF failed: {}",
        warm.report
    );
    assert_eq!(
        count(&warm.report, "documents"),
        (WARMUP_MD + WARMUP_PDF) as u64
    );

    // Act: (i) small warm, (ii) the second ingest of small (the D5 skip path), (iii) large
    let small_db = scratch.join("small.db");
    let small = ingest(&bin, &scratch, &home, &small_db, "small");
    eprintln!("d9: small {:.3} s: {}", small.wall_s, counts(&small.report));
    let rerun = ingest(&bin, &scratch, &home, &small_db, "small");
    eprintln!(
        "d9: small rerun {:.3} s: {}",
        rerun.wall_s,
        counts(&rerun.report)
    );
    let large_db = scratch.join("large.db");
    let large = ingest(&bin, &scratch, &home, &large_db, "large");
    let large_db_bytes = db_bytes(&large_db);
    eprintln!(
        "d9: large {:.2} s, VmHWM {:?} kB, db {large_db_bytes} B: {}",
        large.wall_s,
        large.max_rss_kb,
        counts(&large.report)
    );

    // Record before asserting (a miss still leaves the figure on disk)
    let rss_kb = large.max_rss_kb.unwrap_or(0);
    let mut rss_gate = gate(rss_kb as f64, RSS_LIMIT_KB as f64);
    rss_gate["informational"] = Value::Bool(true);
    if large.max_rss_kb.is_none() {
        rss_gate["value"] = Value::Null;
        rss_gate["pass"] = Value::Null;
    }
    let gates = json!({
        "small_warm_s": gate(small.wall_s, SMALL_LIMIT_S),
        "large_s": gate(large.wall_s, LARGE_LIMIT_S),
        "large_max_rss_kb": rss_gate,
    });
    let gates_pass = gates["small_warm_s"]["pass"] == true && gates["large_s"]["pass"] == true;
    let mut result = json!({
        "bench_schema": 2,
        "kind": "d9",
        "protocol": "docs-ingest-plan.md §8.1 (DI2.2); core-plan.md §16 seed and machine profile",
        "date": sh("date", &["+%F"], None).unwrap_or_default(),
        "seed": format!("{SEED:#x}"),
        "machine": machine,
        "warm": "corpus files just written (page cache warm); the binary ran once before on warmup/ (5 files) with its own fresh database; every measured run uses a fresh --db",
        "command": format!("KABAN_WORKSPACE={WORKSPACE} <bin> --db <fresh.db> --json ingest <corpus> --workers {WORKERS}"),
        "rss_method": "VmHWM of the child from /proc/<pid>/status, polled every 3 ms while it runs",
        "corpus": {
            "warmup": { "md": warm_stats.md, "pdf": warm_stats.pdf, "bytes": warm_stats.bytes },
            "small": { "md": small_stats.md, "pdf": small_stats.pdf, "bytes": small_stats.bytes },
            "large": { "md": large_stats.md, "bytes": large_stats.bytes },
        },
        "warmup": { "ingest_s": warm.wall_s, "counts": counts(&warm.report) },
        "small": {
            "ingest_s": small.wall_s,
            "rerun_s": rerun.wall_s,
            "counts": counts(&small.report),
            "rerun_counts": counts(&rerun.report),
            "db_bytes": db_bytes(&small_db),
        },
        "large": {
            "ingest_s": large.wall_s,
            "max_rss_kb": large.max_rss_kb,
            "db_bytes": large_db_bytes,
            "counts": counts(&large.report),
        },
        "gates": gates,
        "gates_pass": gates_pass,
        "runs": 1,
        "deferred": "OWN-68 SC-13: two repetitions",
    });
    if smoke {
        result["smoke"] = Value::Bool(true);
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let mut text = serde_json::to_string_pretty(&result).unwrap();
    text.push('\n');
    std::fs::write(&out, text).unwrap();
    eprintln!("d9: result written to {}", out.display());

    // Assert: the counts the corpora imply, then the two asserted gates (no tuning on a miss)
    assert_eq!(
        count(&small.report, "documents"),
        (small_md + small_pdf) as u64,
        "{}",
        small.report
    );
    assert_eq!(count(&small.report, "failed"), 0, "{}", small.report);
    assert_eq!(count(&small.report, "partial"), 0, "{}", small.report);
    assert_eq!(count(&rerun.report, "documents"), 0, "{}", rerun.report);
    assert_eq!(
        count(&rerun.report, "skipped"),
        (small_md + small_pdf) as u64,
        "{}",
        rerun.report
    );
    assert_eq!(
        count(&large.report, "documents"),
        large_md as u64,
        "{}",
        large.report
    );
    assert_eq!(count(&large.report, "failed"), 0, "{}", large.report);
    assert_eq!(count(&large.report, "partial"), 0, "{}", large.report);
    if smoke {
        eprintln!("d9: KABAN_D9_SMOKE=1, gates recorded, not asserted: {gates}");
        return;
    }
    assert!(
        small.wall_s < SMALL_LIMIT_S,
        "D9 small warm: {:.3} s >= {SMALL_LIMIT_S} s",
        small.wall_s
    );
    assert!(
        large.wall_s < LARGE_LIMIT_S,
        "D9 large: {:.2} s >= {LARGE_LIMIT_S} s",
        large.wall_s
    );
}
