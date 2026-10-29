//! Result shaping: percentile summaries per operation, `not_applicable` entries (MD-21) and the
//! machine profile recorded in every result (`bench_schema: 2`).

use serde_json::{Value, json};

/// Raw paired API samples, with summaries computed from a copy so round order survives.
pub(super) fn raw_series(us: &[f64]) -> Value {
    let mut sorted = us.to_vec();
    json!({ "n": us.len(), "us_runs": us, "p50_us": pct(&mut sorted, 0.5),
        "p95_us": pct(&mut sorted, 0.95), "max_us": pct(&mut sorted, 1.0) })
}

/// The `p` percentile of `v` (nearest rank; sorts `v` in place; 0 when empty).
pub(super) fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

/// One measured operation (`path` = `api` or `store`).
pub(super) fn summarize(name: &str, path: &str, mut us: Vec<f64>) -> Value {
    let n = us.len();
    let total: f64 = us.iter().sum();
    json!({
        "op": name, "path": path, "status": "measured", "n": n,
        "p50_us": pct(&mut us, 0.5), "p95_us": pct(&mut us, 0.95), "p99_us": pct(&mut us, 0.99),
        "max_us": pct(&mut us, 1.0),
        "ops_per_s": if total > 0.0 { (n as f64 / (total / 1e6)).round() } else { 0.0 },
    })
}

/// An operation the M2 surface cannot run yet (MD-21).
pub(super) fn not_applicable(name: &str, owner: &str, reason: &str) -> Value {
    json!({ "op": name, "path": "api", "status": "not_applicable", "owner_task": owner, "reason": reason })
}

/// First value of a `key: value` line in a `/proc` text file (`None` when absent).
fn proc_field(path: &str, key: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().and_then(|s| {
        s.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    })
}

/// A `/proc/self/status` memory field in kB.
pub(super) fn self_status_kb(key: &str) -> Option<u64> {
    proc_field("/proc/self/status", key)
        .and_then(|v| v.split_whitespace().next().and_then(|n| n.parse().ok()))
}

/// Machine profile recorded in every result (core-plan §16, `bench_schema: 2`). Fields that need
/// the toolchain or git (`rustc`, `cargo`, `git_head`, `binary_sha256`) are added by
/// `../specs/kaban/archive/benchmarks/kaban2/scripts/bench/bench_runs.py`, which wraps the runs; the binary itself spawns no subprocess.
pub(super) fn machine_profile() -> Value {
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    };
    let kernel = read("/proc/sys/kernel/osrelease").unwrap_or_default();
    let uname = format!(
        "{} {} {}",
        read("/proc/sys/kernel/ostype").unwrap_or_else(|| std::env::consts::OS.into()),
        kernel,
        std::env::consts::ARCH
    );
    // x86 exposes `model name`; aarch64 only `CPU implementer` / `CPU part`
    let cpu = proc_field("/proc/cpuinfo", "model name").unwrap_or_else(|| {
        format!(
            "implementer {} part {}",
            proc_field("/proc/cpuinfo", "CPU implementer").unwrap_or_else(|| "?".into()),
            proc_field("/proc/cpuinfo", "CPU part").unwrap_or_else(|| "?".into())
        )
    });
    json!({
        "uname": uname,
        "kernel": kernel,
        "cpu": cpu,
        "nproc": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        "mem_total_kb": proc_field("/proc/meminfo", "MemTotal")
            .and_then(|v| v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok())),
        "target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "kaban_version": env!("CARGO_PKG_VERSION"),
    })
}
