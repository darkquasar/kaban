//! Worker-pool rig shared by the `pool_*` suites: a fake preparer standing in for the extractor
//! pipeline (one Document and one Chunk per path, with selectable timing behaviour), an
//! `Ingestor` over an in-memory store on a scratch tree, and the small corpus/lookup helpers.

use super::Scratch;
use kaban_api::KabanApi;
use kaban_core::{MemoryStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch};
use kaban_ingest::{
    ExtractCtx, ExtractError, IngestConfig, Ingestor, PIPELINE_VERSION, PrepareInput, Prepared,
    Preparer, SourceId, WalkConfig, document_id, gate_path,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A scratch tree for one pool test, removed on drop.
pub(crate) fn scratch(name: &str) -> Scratch {
    Scratch::new("kaban-ingest-pool", name)
}

/// `n` markdown files whose sorted order is not the creation order.
pub(crate) fn corpus(root: &Path, n: usize) -> Vec<String> {
    let mut names: Vec<String> = (0..n)
        .map(|i| format!("doc-{:02}.md", (i * 7) % n))
        .collect();
    for name in &names {
        fs::write(root.join(name), format!("# {name}\n").as_bytes()).unwrap();
    }
    names.sort();
    names
}

/// How the fake behaves for one path.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    /// Return at once.
    Fast,
    /// Sleep a path-dependent 0 to 40 ms so completion order differs from path order.
    Jitter,
    /// Even-numbered paths sleep 50 ms, odd ones return at once: the later of two in-flight
    /// documents usually finishes first and waits in the committer's reorder buffer.
    Alternate,
    /// Sleep in 100 ms steps calling `check()` between them, up to 5 s (a cooperative parser).
    CooperativeSlow,
    /// Block, ignoring `check()`, until the test calls [`Fake::release`] when the path contains
    /// `slow` (a non-cooperative parser); a 10 s guard ends the block if the test never does.
    NonCooperativeIf,
    /// Panic (an unwinding extractor, RD3-2) for the document whose commit sequence number is
    /// `on_seq`; every other document returns at once. The payload is an invented canary with a
    /// path-like tail, so a hook that prints it is caught by the stderr assertions.
    Panic { on_seq: u64 },
}

pub(crate) struct Fake {
    mode: Mode,
    live: AtomicUsize,
    pub(crate) max_live: AtomicUsize,
    pub(crate) completed: Mutex<Vec<Instant>>,
    pub(crate) started: Mutex<Vec<Instant>>,
    /// Release barrier for `NonCooperativeIf` (set once by the test, never cleared).
    released: AtomicBool,
}

impl Fake {
    pub(crate) fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            live: AtomicUsize::new(0),
            max_live: AtomicUsize::new(0),
            completed: Mutex::new(Vec::new()),
            started: Mutex::new(Vec::new()),
            released: AtomicBool::new(false),
        })
    }

    /// Let every blocked `NonCooperativeIf` task finish.
    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    /// Block (ignoring the extraction context) until released or the hang guard fires.
    fn block_until_released(&self) {
        let guard = Instant::now();
        while !self.released.load(Ordering::SeqCst) && guard.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Preparer for Fake {
    fn prepare(&self, input: PrepareInput, ctx: &ExtractCtx) -> Result<Prepared, ExtractError> {
        self.started.lock().unwrap().push(Instant::now());
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(live, Ordering::SeqCst);
        let result = self.body(&input, ctx);
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.completed.lock().unwrap().push(Instant::now());
        result
    }
}

impl Fake {
    fn body(&self, input: &PrepareInput, ctx: &ExtractCtx) -> Result<Prepared, ExtractError> {
        match self.mode {
            Mode::Panic { on_seq } if input.seq == on_seq => {
                panic!("CANARY_PANIC_7731 /srv/customers/alice-7781/statement.pdf");
            }
            Mode::Fast | Mode::Panic { .. } => {}
            Mode::Jitter => {
                let ms = u64::from(
                    input
                        .path
                        .relative_path()
                        .bytes()
                        .fold(0u8, |a, b| a.wrapping_mul(31).wrapping_add(b)),
                ) % 41;
                std::thread::sleep(Duration::from_millis(ms));
            }
            Mode::Alternate => {
                let digit = input
                    .path
                    .relative_path()
                    .bytes()
                    .rev()
                    .find(u8::is_ascii_digit)
                    .unwrap_or(b'0');
                if digit % 2 == 0 {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Mode::CooperativeSlow => {
                for _ in 0..50 {
                    std::thread::sleep(Duration::from_millis(100));
                    ctx.check()?;
                }
            }
            Mode::NonCooperativeIf => {
                if input.path.relative_path().contains("slow") {
                    self.block_until_released();
                }
            }
        }
        ctx.check()?;
        let bytes = fs::read(&input.abs).unwrap();
        let chunk = NodeId::new();
        let doc = NewNode {
            kind: "Document".into(),
            text: String::new(),
            props: Some(serde_json::json!({
                "source_id": input.source.as_str(),
                "path": input.path.relative_path(),
                "status": "ok",
                "content_hash": blake3::hash(&bytes).to_hex().to_string(),
                "pipeline_version": PIPELINE_VERSION,
                "extractor_version": "fake@0",
                "config_hash": "",
            })),
            ..NewNode::default()
        };
        let chunk_node = NewNode {
            kind: "Chunk".into(),
            text: String::from_utf8_lossy(&bytes).into_owned(),
            ..NewNode::default()
        };
        Ok(Prepared {
            seq: input.seq,
            source: Some(input.source.clone()),
            document_id: input.document_id.clone(),
            status: kaban_ingest::DocStatus::Ok,
            graph: None,
            batch: WriteBatch::new(
                input.workspace.clone(),
                vec![
                    Op::UpsertNode {
                        id: input.document_id.clone(),
                        node: doc,
                    },
                    Op::UpsertNode {
                        id: chunk.clone(),
                        node: chunk_node,
                    },
                    Op::UpsertEdge {
                        src: chunk,
                        dst: input.document_id.clone(),
                        rel: "PART_OF".into(),
                        props: None,
                    },
                ],
            ),
            audit: Vec::new(),
            skip: None,
        })
    }
}

/// A `tracing` writer into a shared buffer (what `RUST_LOG=trace` would print to stderr).
#[derive(Clone, Default)]
pub(crate) struct LogBuf(pub(crate) Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) struct Rig {
    pub(crate) root: Scratch,
    pub(crate) store: Arc<SqliteStore>,
    pub(crate) ingestor: Ingestor,
    pub(crate) cfg: WalkConfig,
}

/// An `Ingestor` over an existing store: the adapter builds one per call (`cli/ingest.rs`), so
/// a test of successive adapter calls builds a second one over the first rig's store.
pub(crate) fn ingestor_over(
    store: &Arc<SqliteStore>,
    fake: Arc<Fake>,
    cfg: IngestConfig,
) -> Ingestor {
    let api = Arc::new(KabanApi::new(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
    ));
    Ingestor::new(api, store.clone(), store.signer(), fake, cfg)
}

pub(crate) fn rig(name: &str, fake: Arc<Fake>, cfg: IngestConfig) -> Rig {
    let root = scratch(name);
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let ingestor = ingestor_over(&store, fake, cfg);
    Rig {
        cfg: WalkConfig::new(root.to_path_buf()),
        root,
        store,
        ingestor,
    }
}

pub(crate) fn source_of(rel: &str) -> SourceId {
    SourceId::new(
        &WorkspaceId::default(),
        &gate_path(&Policy::empty(), rel).unwrap(),
    )
}

/// Which of the sorted sources have a Document node (committed whole or not at all).
pub(crate) fn committed_flags(store: &SqliteStore, sorted: &[String]) -> Vec<bool> {
    sorted
        .iter()
        .map(|rel| {
            store
                .get(&document_id(&source_of(rel)), true)
                .unwrap()
                .is_some()
        })
        .collect()
}
