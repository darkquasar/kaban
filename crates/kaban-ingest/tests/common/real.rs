//! The real-pipeline rig (DI1.5): `Ingestor` + `DocumentPreparer` over a scratch tree and an
//! in-memory store, plus store readers the incremental and idempotency suites share.

use super::Scratch;
use kaban_api::KabanApi;
use kaban_core::{ChangeLogStore, Edge, MemoryStore, Node, NodeId, TraverseQuery, WorkspaceId};
use kaban_ingest::{
    ChunkConfig, DocumentPreparer, IngestConfig, IngestReport, Ingestor, SourceId, WalkConfig,
    document_id, gate_path,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct Rig {
    pub(crate) root: Scratch,
    pub(crate) store: Arc<SqliteStore>,
    pub(crate) ingestor: Ingestor,
    pub(crate) cfg: WalkConfig,
    /// The database file when the rig is file-backed (`rig_on_disk`), for byte inspection.
    pub(crate) db: Option<std::path::PathBuf>,
}

pub(crate) fn rig(prefix: &str, name: &str) -> Rig {
    rig_with(prefix, name, Policy::empty(), false)
}

/// A rig over a file-backed store (db, wal and shm under `<root>/store/`) with `policy`, for
/// the canaries that inspect on-disk bytes through the real `Ingestor`.
#[allow(dead_code)] // each test binary uses the helpers it needs
pub(crate) fn rig_on_disk(prefix: &str, name: &str, policy: Policy) -> Rig {
    rig_with(prefix, name, policy, true)
}

fn rig_with(prefix: &str, name: &str, policy: Policy, on_disk: bool) -> Rig {
    let root = Scratch::new(prefix, name);
    let (store, db) = if on_disk {
        let dir = root.join("store");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("kaban.db");
        (
            Arc::new(SqliteStore::open(&db, &dir.join("keys")).unwrap()),
            Some(db),
        )
    } else {
        (Arc::new(SqliteStore::open_memory().unwrap()), None)
    };
    let api = Arc::new(KabanApi::new(store.clone(), policy, store.signer(), None));
    let ingest_cfg = IngestConfig {
        workers: 2,
        queue_depth: 4,
        ..IngestConfig::default()
    };
    let preparer = Arc::new(DocumentPreparer::new(
        &api,
        store.clone(),
        store.signer(),
        ChunkConfig::default(),
        ingest_cfg.clone(),
    ));
    let ingestor = Ingestor::new(api, store.clone(), store.signer(), preparer, ingest_cfg);
    let mut cfg = WalkConfig::new(root.to_path_buf());
    if on_disk {
        // the store's own files sit under the root: keep them out of the walk
        cfg.deny_globs.push("store/**".to_owned());
    }
    Rig {
        root,
        store,
        ingestor,
        cfg,
        db,
    }
}

impl Rig {
    /// Take a file-backed rig's store past the body codec migration (spec 05 SS-17) before
    /// its first run: the production decision over a complete index takes it through 0011
    /// and 0012, so every body the real pipeline writes from here on is a tagged, compressed
    /// `BLOB` (T7 review t7-5: the ingest fixtures run over both shapes).
    #[allow(dead_code)]
    pub(crate) fn compress(&self) {
        assert!(self.db.is_some(), "a file-backed rig");
    }

    /// How many encoded bodies the database file and its WAL hold: the tag every encoded
    /// body starts with (`body_codec.rs`: a NUL, `KZ`, format 1, dictionary 1), counted as raw
    /// bytes, since this crate reads the store through its API only (file-backed rigs only).
    #[allow(dead_code)]
    pub(crate) fn encoded_bodies(&self) -> usize {
        const TAG: &[u8] = &[0, b'K', b'Z', 1, 1];
        self.disk_bytes()
            .windows(TAG.len())
            .filter(|w| *w == TAG)
            .count()
    }

    pub(crate) fn run(&self) -> IngestReport {
        self.run_with(&self.cfg)
    }

    pub(crate) fn run_with(&self, cfg: &WalkConfig) -> IngestReport {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(self.ingestor.run(cfg, CancellationToken::new()))
            .unwrap()
    }

    pub(crate) fn outbox_len(&self) -> usize {
        self.store.changes_read(0, 1_000_000).unwrap().len()
    }

    /// Node ids named by changes rows after `after`.
    pub(crate) fn touched_since(&self, after: u64) -> Vec<String> {
        let mut ids: Vec<String> = self
            .store
            .changes_read(after, 1_000_000)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    pub(crate) fn last_seq(&self) -> u64 {
        self.store
            .changes_read(0, 1_000_000)
            .unwrap()
            .last()
            .map_or(0, |c| c.seq)
    }

    /// Every byte of the db, wal and shm files (file-backed rigs only).
    #[allow(dead_code)]
    pub(crate) fn disk_bytes(&self) -> Vec<u8> {
        let db = self.db.as_ref().expect("a file-backed rig");
        let mut out = std::fs::read(db).unwrap();
        for suffix in ["-wal", "-shm"] {
            let side = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
            if side.exists() {
                out.extend(std::fs::read(side).unwrap());
            }
        }
        out
    }

    pub(crate) fn document(&self, rel: &str) -> Option<Node> {
        self.store.get(&doc_id(rel), true).unwrap()
    }

    /// Chunk ids of the stored document, in the order `build_batch` wrote them.
    pub(crate) fn chunk_ids(&self, rel: &str) -> Vec<String> {
        ids_of(&self.document(rel).expect("document stored"), "chunk_ids")
    }

    pub(crate) fn section_ids(&self, rel: &str) -> Vec<String> {
        ids_of(&self.document(rel).expect("document stored"), "section_ids")
    }

    /// Active nodes and edges within one hop of `start` over `rels`.
    pub(crate) fn hop(&self, start: &NodeId, rels: &[&str], depth: u8) -> (Vec<Node>, Vec<Edge>) {
        let r = self
            .store
            .traverse(&TraverseQuery {
                start_ids: vec![start.clone()],
                rels: rels.iter().map(|r| (*r).to_owned()).collect(),
                direction: kaban_core::TraverseDirection::Both,
                max_depth: depth,
                node_kinds: Vec::new(),
                governance: Vec::new(),
                limit: 500,
                workspace_id: None,
                ..Default::default()
            })
            .unwrap();
        (r.nodes, r.edges)
    }
}

pub(crate) fn source_of(rel: &str) -> SourceId {
    SourceId::new(
        &WorkspaceId::default(),
        &gate_path(&Policy::empty(), rel).unwrap(),
    )
}

pub(crate) fn doc_id(rel: &str) -> NodeId {
    document_id(&source_of(rel))
}

fn ids_of(node: &Node, key: &str) -> Vec<String> {
    node.props[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}
