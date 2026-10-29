//! Repository-free probes: only the host whose complete oracle passed is advertised.

use std::process::{Command, Output};

/// The one host whose 21-profile qualification evidence exists (../specs/kaban/specs/00-project/evidence/ri2-evidence/).
const QUALIFIED_HOST: &str = "aarch64-unknown-linux-gnu";

fn capabilities_probe() -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaban-rust-scip"))
        .args(["capabilities", "--json"])
        .env_clear()
        .current_dir("/")
        .output()
        .expect("start candidate")
}

#[test]
fn capabilities_are_bounded_and_do_not_advertise_unqualified_hosts() {
    let output = capabilities_probe();
    assert!(output.status.success());
    assert!(output.stdout.len() < 4096);
    assert!(output.stderr.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("capabilities JSON");
    assert_eq!(value["protocol_version"], 2);
    assert_eq!(value["semantic_profile"], "kaban-rust-v1");
    assert_eq!(value["ra_ap_version"], "0.0.358");
    let hosts = value["supported_host_triples"]
        .as_array()
        .expect("supported_host_triples is a list");
    assert!(
        hosts.iter().all(|host| host == QUALIFIED_HOST),
        "an unqualified host is advertised: {hosts:?}"
    );
}

#[test]
fn host_admission_lists_exactly_the_qualified_host() {
    assert_eq!(
        kaban_rust_scip::capabilities().supported_host_triples,
        [QUALIFIED_HOST]
    );
    let output = capabilities_probe();
    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("capabilities JSON");
    assert_eq!(
        value["supported_host_triples"],
        serde_json::json!([QUALIFIED_HOST])
    );
}

#[test]
fn incomplete_indexing_is_an_error_not_empty_successful_scip() {
    let output = Command::new(env!("CARGO_BIN_EXE_kaban-rust-scip"))
        .args(["index", "--workspace", "/does-not-exist"])
        .env_clear()
        .current_dir("/")
        .output()
        .expect("start candidate");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn version_identifies_producer_not_stock_analyzer() {
    let output = Command::new(env!("CARGO_BIN_EXE_kaban-rust-scip"))
        .arg("--version")
        .env_clear()
        .current_dir("/")
        .output()
        .expect("start candidate");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 version"),
        format!("kaban-rust-scip {}\n", env!("CARGO_PKG_VERSION"))
    );
}
