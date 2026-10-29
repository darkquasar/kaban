//! The `SnapshotProvider` seam of the service (stage1-plan CG5.2; cg-join-v1 §4): the first
//! control shapes of the code side (stage1-spec §6.1, G1-S2) and the provider reached through
//! `KabanApi::with_snapshot_provider` with the test `FixedSnapshot`. Self-contained: no shared
//! harness.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaban_api::KabanApi;
use kaban_api::control::{ISOLATION_VALUES, POLICY_VALUES, SNAPSHOT_COMMIT, Shape};
use kaban_codegraph::testing::FixedSnapshot;
use kaban_codegraph::{HeadInput, IsolationTier, Policy as PolicyLabel, SCHEMA_VERSION, write};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;

/// A private scratch directory removed on drop.
struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("kaban-api-sp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An empty snapshot for `commit` through the public write helpers, at 0600 under `dir`.
fn write_snapshot(dir: &Path, commit: &str) -> PathBuf {
    let path = dir.join(format!("{commit}.cg.db"));
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap(),
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    write::begin(&conn).unwrap();
    write::insert_files(&conn, &[]).unwrap();
    write::insert_symbols(&conn, &[]).unwrap();
    write::insert_edges(&conn, &[]).unwrap();
    write::insert_occurrences(&conn, &[]).unwrap();
    write::insert_snippets(&conn, &[]).unwrap();
    write::insert_edge_types(&conn, &[]).unwrap();
    let schema = SCHEMA_VERSION.to_string();
    write::insert_meta(
        &conn,
        &[
            ("commit", commit),
            ("isolation", "none"),
            ("policy_version", "pv-api"),
            ("repo_id", "0123456789abcdef0123456789abcdef"),
            ("schema_version", schema.as_str()),
            ("symbols_fts_version", "1"),
        ],
    )
    .unwrap();
    write::rebuild_fts(&conn).unwrap();
    write::finish(&conn).unwrap();
    path
}

fn api_on(s: &Scratch) -> KabanApi {
    let store = Arc::new(SqliteStore::open(&s.0.join("kaban.db"), &s.0.join("keys")).unwrap());
    let policy = Policy::from_file(Policy::parse(b"words: []\n").unwrap(), "t".into()).unwrap();
    KabanApi::new(store.clone(), policy, store.signer(), None)
}

#[test]
fn control_entries_shapes() {
    // `snapshot_commit` and its siblings: 7 to 64 lowercase hex, nothing else (SO-2 admission)
    assert_eq!(SNAPSHOT_COMMIT, Shape::Hex { min: 7, max: 64 });
    assert!(SNAPSHOT_COMMIT.accepts(&"a".repeat(7)));
    assert!(SNAPSHOT_COMMIT.accepts(&"0".repeat(40)));
    assert!(SNAPSHOT_COMMIT.accepts(&"f".repeat(64)));
    assert!(!SNAPSHOT_COMMIT.accepts(&"a".repeat(6)));
    assert!(!SNAPSHOT_COMMIT.accepts(&"a".repeat(65)));
    assert!(!SNAPSHOT_COMMIT.accepts("ABCDEF1"), "uppercase is refused");
    assert!(!SNAPSHOT_COMMIT.accepts("abcdefg"), "`g` is not hex");
    assert!(!SNAPSHOT_COMMIT.accepts(""));

    // `isolation`: exactly the tiers `meta.isolation` can hold, in the spec's order
    let tiers = [
        IsolationTier::Namespaces,
        IsolationTier::Container,
        IsolationTier::None,
    ]
    .map(IsolationTier::as_str);
    assert_eq!(ISOLATION_VALUES, &tiers);
    let isolation = Shape::Enum(ISOLATION_VALUES);
    for tier in tiers {
        assert!(isolation.accepts(tier));
    }
    assert!(!isolation.accepts("vm"));
    assert!(!isolation.accepts("Namespaces"));

    // `policy`: the currency values of cg-join-v1 §4 plus DF-3's `off` (q1-3), the emitted
    // labels in variant order
    let labels =
        [PolicyLabel::Current, PolicyLabel::Stale, PolicyLabel::Off].map(PolicyLabel::as_str);
    assert_eq!(POLICY_VALUES, &labels);
    let policy = Shape::Enum(POLICY_VALUES);
    for label in labels {
        assert!(policy.accepts(label), "{label}");
    }
    assert!(!policy.accepts("Current") && !policy.accepts(""));
}

#[test]
fn api_resolves_through_fixed_snapshot() {
    let s = Scratch::new("fixed");
    let commit = "ab".repeat(20);
    let path = write_snapshot(&s.0, &commit);
    let api = api_on(&s);
    assert!(api.snapshot_provider().is_none(), "no provider by default");
    assert!(api.codegraph_dir().is_none());

    let dir =
        s.0.join("codegraph")
            .join("0123456789abcdef0123456789abcdef");
    let api = api.with_snapshot_provider(dir.clone(), Arc::new(FixedSnapshot::new(&path)));
    assert_eq!(api.codegraph_dir(), Some(dir.as_path()));
    assert!(
        !dir.exists(),
        "wiring a provider creates nothing (the directory need not exist)"
    );
    // the service's clones share the one provider (one LRU per process, SO-11)
    let clone = api.clone();
    let provider = api.snapshot_provider().unwrap();
    assert!(Arc::ptr_eq(provider, clone.snapshot_provider().unwrap()));

    // resolved through the trait object, as the engine's adapter will (cg-join-v1 §4)
    let head = HeadInput {
        commit: "cd".repeat(20),
        ancestors: vec![],
    };
    let resolved = provider
        .resolve("repo", &head)
        .expect("the fixed file resolves");
    assert!(resolved.r#ref.exact);
    assert_eq!(resolved.r#ref.commit, commit);
    assert_eq!(resolved.r#ref.isolation_tier, IsolationTier::None);
    assert_eq!(resolved.r#ref.policy_version, "pv-api");
    assert_eq!(resolved.r#ref.schema_version, SCHEMA_VERSION);
    assert_eq!(resolved.snapshot.path(), path);
    assert_eq!(resolved.attach.path(), path);
    assert!(
        resolved
            .attach
            .as_sql_param()
            .ends_with("?mode=ro&immutable=1")
    );
    assert!(
        format!("{api:?}").starts_with("KabanApi"),
        "Debug stays implemented"
    );
}
