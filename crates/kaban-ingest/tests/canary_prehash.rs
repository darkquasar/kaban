//! Pre-hash canaries and the skip decision (docs-ingest-plan.md DI0.2, PR-2; docs-ingest-spec D5,
//! D6; core-spec S1, S3; dispositions D-4, D-5, D-6, D-12): a secret-bearing file name is
//! rejected by the path gate before any identity is derived from it, it reaches no byte of the
//! database, WAL, audit chain or trace log, and the blocked document is recognised on a re-run
//! through its keyed id alone.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, AuditStore, ChangeLogStore, IngestStore, MemoryStore, NodeId, WorkspaceId, WriteCtx,
};
use kaban_ingest::{
    Candidate, PIPELINE_VERSION, Skip, SkipKey, SourceId, WalkConfig, blocked_path_document,
    config_hash, should_skip_blocked, walk,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A builtin reject-class credential (`aws-access-key-id`); never a real key.
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";

/// A fresh scratch tree under the OS temp dir, removed on drop.
fn scratch(name: &str) -> Scratch {
    Scratch::new("kaban-ingest-canary", name)
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

/// A `tracing` writer into a shared buffer (what `RUST_LOG=trace` would print to stderr).
#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Everything persisted on disk for a file-backed store: db, WAL and SHM bytes.
fn disk_bytes(db: &Path) -> Vec<u8> {
    let mut out = fs::read(db).unwrap();
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            out.extend(fs::read(side).unwrap());
        }
    }
    out
}

struct Tree {
    store: SqliteStore,
    db: PathBuf,
    policy: Policy,
    cfg: WalkConfig,
    /// Declared last so the store closes before the scratch tree is removed.
    root: Scratch,
}

impl Tree {
    fn open(name: &str) -> Self {
        let root = scratch(name);
        let docs = root.join("docs");
        fs::create_dir_all(&docs).unwrap();
        write(
            &docs,
            &format!("notes-{CANARY}.md"),
            b"# nothing secret inside\n",
        );
        write(&docs, "clean.md", b"# clean\n");
        let db = root.join("kaban.db");
        let store = SqliteStore::open(&db, &root.join("keys")).unwrap();
        Self {
            root,
            store,
            db,
            policy: Policy::empty(),
            cfg: WalkConfig::new(docs),
        }
    }

    fn candidates(&self) -> Vec<Candidate> {
        walk(&self.cfg)
            .unwrap()
            .map(|i| i.expect("the walker itself does not scan names"))
            .collect()
    }

    fn key(&self) -> SkipKey {
        SkipKey {
            identity: kaban_ingest::Identity::Keyed(String::new()),
            pipeline_version: PIPELINE_VERSION,
            extractor_version: String::new(),
            config_hash: config_hash(&self.cfg, &self.policy),
            position_fingerprint: None,
        }
    }
}

/// Gate every candidate; write the blocked ones through `apply_batch` (D-6) unless the keyed
/// skip says they are already stored. Returns the blocked node ids written in this run.
fn ingest_blocked(tree: &Tree) -> Vec<NodeId> {
    let ws = WorkspaceId::default();
    let signer = tree.store.signer();
    let mut written = Vec::new();
    for c in tree.candidates() {
        match c.gate(&tree.policy) {
            Ok(gated) => {
                let _source = SourceId::new(&ws, &gated); // the only way to make a SourceId
            }
            Err(Skip::PathRejected { rule_ids }) => {
                let keyed = c.keyed_path_id(signer.as_ref(), &ws);
                let mut key = tree.key();
                key.identity = kaban_ingest::Identity::Keyed(keyed.clone());
                if should_skip_blocked(&tree.store, &keyed, &key).unwrap() {
                    continue;
                }
                let blocked = blocked_path_document(
                    &ws,
                    &keyed,
                    &rule_ids,
                    &key,
                    tree.policy.effective_hash(),
                );
                let mut ctx = WriteCtx::new(tree.policy.effective_hash(), ws.clone(), Actor::Agent);
                ctx.audit.push(blocked.audit);
                tree.store.apply_batch(blocked.batch, &ctx).unwrap();
                written.push(blocked.id);
            }
            Err(other) => panic!("unexpected skip {other:?}"),
        }
    }
    written
}

/// Plan slice 7 (D-6 scope): `notes-<CANARY>.md` is `PathRejected` with the rule id, no
/// `SourceId` is derived from it, and the canary appears nowhere: database, WAL, SHM, audit
/// chain, the blocked node's props or a `trace`-level log capture.
#[test]
fn a_canary_in_a_file_name_is_rejected_before_hashing_and_reaches_no_byte_store() {
    let tree = Tree::open("prehash");
    let logs = LogBuf::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .finish();

    let written = tracing::subscriber::with_default(subscriber, || {
        let gates: Vec<_> = tree
            .candidates()
            .iter()
            .map(|c| c.gate(&tree.policy))
            .collect();
        assert_eq!(gates.len(), 2);
        let rejected: Vec<&Vec<String>> = gates
            .iter()
            .filter_map(|g| match g {
                Err(Skip::PathRejected { rule_ids }) => Some(rule_ids),
                _ => None,
            })
            .collect();
        assert_eq!(
            rejected,
            vec![&vec!["aws-access-key-id".to_owned()]],
            "{gates:?}"
        );
        assert_eq!(
            gates.iter().filter(|g| g.is_ok()).count(),
            1,
            "clean.md passes"
        );
        ingest_blocked(&tree)
    });

    assert_eq!(written.len(), 1, "one blocked Document written");
    let node = tree
        .store
        .get(&written[0], true)
        .unwrap()
        .expect("blocked node stored");
    assert_eq!(node.kind, "Document");
    assert_eq!(node.props["status"], "blocked");
    assert!(
        node.props.get("path").is_none(),
        "a blocked node carries no path: {}",
        node.props
    );
    let props = serde_json::to_string(&node.props).unwrap();
    assert!(
        !props.contains(CANARY) && !props.contains("notes-"),
        "{props}"
    );

    let audit = tree.store.audit_verify().unwrap();
    assert!(audit.ok && audit.entries >= 1, "{audit:?}");
    let disk = disk_bytes(&tree.db);
    assert!(!contains(&disk, CANARY), "canary in db/wal/shm bytes");
    assert!(
        !contains(&disk, &format!("notes-{CANARY}")),
        "file name in db/wal/shm bytes"
    );
    let log = logs.0.lock().unwrap().clone();
    assert!(!log.is_empty(), "the trace capture saw events");
    assert!(
        !contains(&log, CANARY),
        "canary in trace logs: {}",
        String::from_utf8_lossy(&log)
    );
    assert!(!contains(&log, "notes-"), "file name in trace logs");
    let _ = &tree.root;
}

fn outbox_rows(store: &SqliteStore) -> usize {
    store.changes_read(0, 100_000).unwrap().len()
}

/// Plan slice 8 (D-6 scope): on a second run the rejected file is recognised through its keyed
/// id alone (`should_skip_blocked`), so nothing is written: no node update, no changes row, no
/// audit row. Its name is re-scanned, never hashed unkeyed (the keyed id is the only identity).
/// A changed configuration (D-12) re-admits it for a fresh blocked write.
#[test]
fn the_same_rejected_file_is_skipped_on_a_second_run_via_its_keyed_identity() {
    let tree = Tree::open("rerun");
    let first = ingest_blocked(&tree);
    assert_eq!(first.len(), 1);
    let outbox_after_first = outbox_rows(&tree.store);
    let audit_after_first = tree.store.audit_verify().unwrap().entries;
    let rev_after_first = tree.store.get(&first[0], true).unwrap().unwrap().rev;

    let second = ingest_blocked(&tree);
    assert!(
        second.is_empty(),
        "second run must skip the blocked document: {second:?}"
    );
    assert_eq!(
        outbox_rows(&tree.store),
        outbox_after_first,
        "no changes row on the re-run"
    );
    assert_eq!(
        tree.store.audit_verify().unwrap().entries,
        audit_after_first,
        "no audit row on the re-run"
    );
    assert_eq!(
        tree.store.get(&first[0], true).unwrap().unwrap().rev,
        rev_after_first
    );

    // a policy or walk configuration change invalidates the skip (D-12)
    let mut changed = Tree {
        cfg: WalkConfig {
            max_file_bytes: 1024 * 1024,
            ..tree.cfg.clone()
        },
        ..tree
    };
    let third = ingest_blocked(&changed);
    assert_eq!(third, first, "same keyed id, same blocked node id");
    assert!(
        outbox_rows(&changed.store) > outbox_after_first,
        "the config change rewrote the node"
    );
    changed.cfg = WalkConfig::new(changed.root.join("docs"));
    let _ = &changed.db;
}

/// Plan slice 9: `should_skip` is true for an unchanged `(identity, pipeline, extractor,
/// config)` and false when any one component differs, when the identity kind differs
/// (`content_hash` vs `blocked_hmac`) or when no Document exists for the source.
#[test]
fn should_skip_is_true_only_when_every_component_is_unchanged() {
    use kaban_core::{NewNode, Op, WriteBatch};
    use kaban_ingest::{Identity, document_id, gate_path, should_skip};

    let store = SqliteStore::open_memory().unwrap();
    let ws = WorkspaceId::default();
    let policy = Policy::empty();
    let gated = gate_path(&policy, "docs/clean.md").unwrap();
    let source = SourceId::new(&ws, &gated);
    let hash = blake3::hash(b"# clean\n").to_hex().to_string();
    let keyed_lookalike = hash.clone();
    let cfg = WalkConfig::new(PathBuf::from("/nonexistent"));
    let key = SkipKey {
        identity: Identity::ContentHash(hash.clone()),
        pipeline_version: PIPELINE_VERSION,
        extractor_version: "pulldown-cmark@0.13.4".into(),
        config_hash: config_hash(&cfg, &policy),
        position_fingerprint: None,
    };
    assert!(
        !should_skip(&store, &source, &key).unwrap(),
        "no Document yet"
    );

    let node = NewNode {
        kind: "Document".into(),
        text: String::new(),
        props: Some(serde_json::json!({
            "source_id": source.as_str(),
            "content_hash": hash,
            "pipeline_version": key.pipeline_version,
            "extractor_version": key.extractor_version,
            "config_hash": key.config_hash,
            "status": "ok",
        })),
        ..NewNode::default()
    };
    let ctx = WriteCtx::new(policy.effective_hash(), ws.clone(), Actor::Agent);
    store
        .apply_batch(
            WriteBatch::new(
                ws.clone(),
                vec![Op::UpsertNode {
                    id: document_id(&source),
                    node,
                }],
            ),
            &ctx,
        )
        .unwrap();
    assert!(
        should_skip(&store, &source, &key).unwrap(),
        "unchanged key skips"
    );

    let variants = [
        SkipKey {
            identity: Identity::ContentHash(blake3::hash(b"# edited\n").to_hex().to_string()),
            ..key.clone()
        },
        SkipKey {
            pipeline_version: PIPELINE_VERSION + 1,
            ..key.clone()
        },
        SkipKey {
            extractor_version: "pulldown-cmark@0.14.0".into(),
            ..key.clone()
        },
        SkipKey {
            config_hash: config_hash(
                &WalkConfig {
                    include_code: true,
                    ..cfg.clone()
                },
                &policy,
            ),
            ..key.clone()
        },
        SkipKey {
            identity: Identity::Keyed(keyed_lookalike),
            ..key.clone()
        },
    ];
    for (i, k) in variants.iter().enumerate() {
        assert!(
            !should_skip(&store, &source, k).unwrap(),
            "variant {i} must not skip: {k:?}"
        );
    }
    let other = SourceId::new(&ws, &gate_path(&policy, "docs/other.md").unwrap());
    assert!(
        !should_skip(&store, &other, &key).unwrap(),
        "another source has no Document"
    );
}
