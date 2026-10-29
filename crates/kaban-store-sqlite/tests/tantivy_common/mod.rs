//! Shared helpers for the adapter suites: a private scratch directory (database, keys, index)
//! removed on drop, the small write helpers, the adapter over the store's own `Rows`, and a
//! scripted `Rows` fake for the payload and snapshot cases.

#![allow(dead_code)] // each test binary uses a subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::config::Recall;
use kaban_core::text_index::{
    Canonical, IndexIdentity, IndexRow, MatchMode, PurgeOutcome, Replay, Rows, Searched,
    SeedFilter, TextHit, TextIndex, TextQuery, Visibility,
};
use kaban_core::{
    Actor, MemoryStore, NewNode, Node, Ontology, RecallQuery, RememberOutcome, Result, WorkspaceId,
    WriteCtx,
};
use kaban_index_tantivy::TantivyIndex;
use kaban_store_sqlite::SqliteStore;
use kaban_store_sqlite::index_rows::IndexRows;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Private scratch directory `<temp>/ss08-<pid>-<name>`, removed on drop (also on a panic).
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("ss08-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    pub(crate) fn db(&self) -> PathBuf {
        self.0.join("kaban.db")
    }
    pub(crate) fn keys(&self) -> PathBuf {
        self.0.join("keys")
    }
    /// The directory of the suites' own adapter ([`Self::adapter`]): private, so the store's
    /// own index beside the database (built and projected by the store since OWN-M) and the
    /// adapter a test drives never share a directory; both read the same database.
    pub(crate) fn index_dir(&self) -> PathBuf {
        self.0.join("adapter").join("tantivy")
    }
    pub(crate) fn store(&self) -> SqliteStore {
        SqliteStore::open(&self.db(), &self.keys()).unwrap()
    }
    /// The store with its own publication silenced (review t8a-7): an injected index the
    /// store projects nothing into, so the `text-index` consumer and the directory lock are
    /// the test's adapter's alone and a projector case builds the exact crash state it names
    /// (a page committed by a transient writer and never acknowledged, a cursor that lags).
    pub(crate) fn store_unpublished(&self) -> SqliteStore {
        self.store().with_text_index(Box::new(Silent))
    }
    /// The adapter over this scratch's database under the shipped ontology, not yet built.
    pub(crate) fn adapter(&self) -> TantivyIndex {
        self.adapter_with(&kaban_core::shipped::resolved().unwrap())
    }

    /// The adapter over this scratch's database under `ontology` (the one the store deploys),
    /// not yet built.
    pub(crate) fn adapter_with(&self, ontology: &Ontology) -> TantivyIndex {
        TantivyIndex::open(
            &self.index_dir(),
            ontology,
            &Recall::default(),
            Box::new(IndexRows::open(&self.db()).unwrap()),
        )
        .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether `node.text` of `id` is stored as a `BLOB` (an encoded body) rather than `TEXT`.
pub(crate) fn encoded(db: &std::path::Path, id: &str) -> bool {
    let kind: String = rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT typeof(text) FROM node WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    kind == "blob"
}

pub(crate) fn ctx(ws: &str) -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::new(ws), Actor::Agent)
}

/// Remember a fresh `kind` note in workspace `ws` and return it.
pub(crate) fn note(store: &SqliteStore, ws: &str, kind: &str, text: &str, tags: &[&str]) -> Node {
    let outcome = store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                tags: tags.iter().map(|t| (*t).to_owned()).collect(),
                workspace_id: Some(ws.into()),
                ..Default::default()
            },
            &[],
            &ctx(ws),
        )
        .unwrap();
    // the write marks its commit pending (fix16); the fixture publishes it as the barrier
    // would, so the projector cases start from an acknowledged head (nothing for a store
    // whose publication is silenced: its injected index takes no projection)
    store
        .text_publish(
            u64::MAX,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
    match outcome {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
    }
}

/// `(rid, rev)` of `node`, read beside the store.
pub(crate) fn row(db: &std::path::Path, node: &Node) -> (i64, i64) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT rid, rev FROM node WHERE id = ?1", [&node.id], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .unwrap()
}

/// A seed query of recall's shape over `q`'s filters.
pub(crate) fn seed(q: &RecallQuery, mode: MatchMode, limit: usize, skip: usize) -> TextQuery<'_> {
    TextQuery {
        text: &q.query,
        mode,
        filter: SeedFilter::of(q),
        limit,
        sufficient: limit,
        skip,
    }
}

/// The hits of one search, or a panic on `Unavailable`.
pub(crate) fn hits(index: &dyn TextIndex, query: &TextQuery<'_>) -> (Vec<TextHit>, bool) {
    match index.search(query).unwrap() {
        Searched::Hits { hits, exhausted } => (hits, exhausted),
        Searched::Unavailable => panic!("the index serves"),
    }
}

pub(crate) fn rids(index: &dyn TextIndex, query: &TextQuery<'_>) -> Vec<i64> {
    hits(index, query).0.into_iter().map(|h| h.rid).collect()
}

/// A scripted `Rows`: fixed rows, an empty change log and a canonical position the test moves
/// through the shared handle [`FakeRows::new`] returns beside it (the adapter owns the box).
pub(crate) struct FakeRows {
    rows: Vec<IndexRow>,
    canonical: Arc<Mutex<Canonical>>,
}

impl FakeRows {
    pub(crate) fn new(rows: Vec<IndexRow>, seq: u64) -> (Self, Arc<Mutex<Canonical>>) {
        let canonical = Arc::new(Mutex::new(Canonical {
            seq,
            identity: IndexIdentity {
                instance_id: "db-1".into(),
                pack_lineage_id: "lineage-1".into(),
                projection_signature: "sig-1".into(),
                ident_terms_version: "2".into(),
            },
        }));
        (
            Self {
                rows,
                canonical: Arc::clone(&canonical),
            },
            canonical,
        )
    }
}

impl Rows for FakeRows {
    fn current(&self, ids: &[String]) -> Result<Vec<IndexRow>> {
        Ok(self
            .rows
            .iter()
            .filter(|r| ids.contains(&r.id))
            .cloned()
            .collect())
    }
    fn active(&self, visit: &mut dyn FnMut(IndexRow) -> Result<()>) -> Result<Canonical> {
        for row in &self.rows {
            visit(row.clone())?;
        }
        self.canonical()
    }
    fn canonical(&self) -> Result<Canonical> {
        Ok(self.canonical.lock().unwrap().clone())
    }
    fn replay(&self, _after: u64, _limit: usize) -> Result<Replay> {
        Ok(Replay {
            oldest: 0,
            at: self.canonical()?,
            changes: vec![],
        })
    }
}

/// A plain active row of `kind` in `ws`.
pub(crate) fn index_row(rid: i64, ws: &str, kind: &str, text: &str) -> IndexRow {
    IndexRow {
        id: format!("id-{rid}"),
        rid,
        revision: 1,
        kind: kind.into(),
        workspace_id: ws.into(),
        tags: vec![],
        text: text.into(),
        summary: String::new(),
        stored_summary: None,
        ident_values: vec![],
        anchor_keys: vec![],
    }
}

/// An index that serves nothing and accepts every projection: the store's own publication,
/// silenced ([`Scratch::store_unpublished`]).
struct Silent;

impl TextIndex for Silent {
    fn search(&self, _query: &TextQuery<'_>) -> Result<Searched> {
        Ok(Searched::Unavailable)
    }
    fn visible(&self) -> Result<Visibility> {
        Ok(Visibility::Unavailable)
    }
    fn project(&self, _changes: &[kaban_core::Change]) -> Result<()> {
        Ok(())
    }
    fn rebuild(&self) -> Result<()> {
        Ok(())
    }
    fn purge(&self, _rids: &[i64]) -> Result<PurgeOutcome> {
        Ok(PurgeOutcome {
            cleaned: false,
            detail: None,
        })
    }
    fn pin(&self) -> Result<Box<dyn TextIndex + '_>> {
        Ok(Box::new(Silent))
    }
}
