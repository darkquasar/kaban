//! Public default-feature recall harness workflow and refusal cases.
#![allow(clippy::unwrap_used)]
#![allow(clippy::print_stderr)] // the Tantivy run's wall time is the test's report
mod common;
use common::Scratch;
use serde_json::Value;
use std::process::Command;

fn command(scratch: &Scratch) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kaban"));
    command
        .env("KABAN_HOME", scratch.home())
        .arg("--db")
        .arg(scratch.db());
    command.current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    command
}

#[test]
fn default_feature_recall_cli_retains_map_samples_and_refuses_existing_database() {
    let scratch = Scratch::new("e35-runtime", "recall");
    let out = scratch.0.join("result.json");
    let output = command(&scratch)
        .args([
            "bench",
            "recall",
            "--n",
            "5",
            "--edges-per-node",
            "1",
            "--warmup",
            "1",
            "--samples",
            "2",
        ])
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["bench_schema"], 3);
    assert_eq!(
        result["recall_evaluation"]["layer_sets"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(result["recall_evaluation"]["paired"]["samples"], 30);
    assert_eq!(
        result["recall_evaluation"]["paired"]["n2"]["status"],
        "harness_only"
    );
    assert!(
        result["recall_evaluation"]["corpus"]["source_map"]
            .as_array()
            .unwrap()
            .len()
            >= 25
    );
    let retained: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(retained, result);
    let edges = command(&scratch)
        .args(["bench", "edges", "--layer", "gazetteer", "--map"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        edges.status.success(),
        "{}",
        String::from_utf8_lossy(&edges.stderr)
    );
    let edges: Value = serde_json::from_slice(&edges.stdout).unwrap();
    assert_eq!(edges["evaluation_kind"], "edges");
    assert_eq!(edges["recall_evaluation"]["edge_precision"]["predicted"], 0);
    assert!(edges["recall_evaluation"]["edge_precision"]["precision"].is_null());
    let before = std::fs::read(scratch.db()).unwrap();
    let refused = command(&scratch)
        .args(["bench", "recall", "--n", "5"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(4));
    assert_eq!(std::fs::read(scratch.db()).unwrap(), before);
}

/// The budget of the whole `--text-index tantivy` run below (fix11): the corpus is
/// materialized at the store's own speed, the index built once from the rows; before, every
/// write of the materialization waited the 5 s barrier on an index nothing had built.
#[cfg(feature = "text-index-tantivy")]
const TANTIVY_RUN: std::time::Duration = std::time::Duration::from_secs(60);

/// `--text-index tantivy` (spec 05 SS-13a) measures the production adapter: the index is
/// built beside the database from the materialized rows, the run names it, and the judged
/// queries recall through it (hits, not the empty pages of an unbuilt index), within
/// [`TANTIVY_RUN`]. Without the `text-index-tantivy` feature the same flag is refused before
/// any write (`text_index_tantivy_is_refused_without_its_feature`).
#[cfg(feature = "text-index-tantivy")]
#[test]
fn text_index_tantivy_measures_the_production_adapter() {
    let scratch = Scratch::new("e35-runtime", "tantivy");
    let started = std::time::Instant::now();
    let output = command(&scratch)
        .args([
            "bench",
            "recall",
            "--n",
            "5",
            "--edges-per-node",
            "1",
            "--warmup",
            "1",
            "--samples",
            "2",
        ])
        .output()
        .unwrap();
    let took = started.elapsed();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("bench recall --text-index tantivy: {took:?}");
    assert!(took < TANTIVY_RUN, "the run took {took:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["recall_evaluation"]["text_index"], "tantivy");
    let index_dir = scratch.db().with_extension("index").join("tantivy");
    assert!(index_dir.join("meta.json").is_file(), "{index_dir:?}");
    let with_hits = result["recall_evaluation"]["layer_sets"][0]["queries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|q| !q["hits"].as_array().unwrap().is_empty())
        .count();
    assert!(
        with_hits > 0,
        "the rebuilt adapter seeds the judged queries"
    );
}

#[test]
fn cli_refuses_cc_sim_embed_bad_classes_and_effective_config_mismatch_before_writes() {
    for (name, flags, message) in [
        (
            "cc",
            vec!["--fusion", "cc:0.7"],
            "not_available_in_this_release",
        ),
        (
            "sim",
            vec!["--layers", "fts,fuzzy,sim"],
            "not_available_in_this_release",
        ),
        (
            "badclass",
            vec!["--classes", "invented"],
            "unknown query class",
        ),
        (
            "fusion",
            vec!["--fusion", "rrf:1"],
            "differs from effective",
        ),
    ] {
        let scratch = Scratch::new("e35-runtime", name);
        let output = command(&scratch)
            .args(["bench", "recall", "--n", "5"])
            .args(flags)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4));
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
        assert!(!scratch.db().exists());
    }
    let scratch = Scratch::new("e35-runtime", "embed");
    let output = command(&scratch)
        .args([
            "bench",
            "edges",
            "--layer",
            "embed",
            "--map",
            "not-used.json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not_available_in_this_release"));
    assert!(!scratch.db().exists());
}

#[test]
fn cli_honors_effective_query_minimum_and_rrf_and_sample_minimum() {
    let scratch = Scratch::new("e35-runtime", "config");
    std::fs::create_dir_all(scratch.home()).unwrap();
    std::fs::write(
        scratch.home().join("config.yaml"),
        "recall:\n  judged_min_queries: 101\n",
    )
    .unwrap();
    let output = command(&scratch)
        .args(["bench", "recall", "--n", "5"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(!scratch.db().exists());
    std::fs::write(
        scratch.home().join("config.yaml"),
        "recall:\n  rrf_k: 7\nbench:\n  min_samples: 31\n",
    )
    .unwrap();
    let output = command(&scratch)
        .args([
            "bench",
            "recall",
            "--n",
            "5",
            "--fusion",
            "rrf:7",
            "--classes",
            "identifier",
            "--layers",
            "fts,fuzzy",
            "--warmup",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["recall_evaluation"]["effective_fusion"], "rrf(7)");
    assert_eq!(result["recall_evaluation"]["paired"]["samples"], 31);
}
