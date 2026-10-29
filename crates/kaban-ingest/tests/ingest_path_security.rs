//! Plain ingestion must not read a candidate through a replaced intermediate directory.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test assertions

mod common;

use kaban_api::KabanApi;
use kaban_ingest::{
    ChunkConfig, DocumentPreparer, ExtractCtx, ExtractError, IngestConfig, Ingestor, PrepareInput,
    Prepared, Preparer, WalkConfig, walk,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::{fs, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

struct SwapDirectory {
    inner: DocumentPreparer,
    source: PathBuf,
    outside: PathBuf,
}

impl Preparer for SwapDirectory {
    fn prepare(&self, input: PrepareInput, ctx: &ExtractCtx) -> Result<Prepared, ExtractError> {
        // Move the actual file, rather than replacing it: its final (dev, ino) still matches.
        fs::rename(&self.source, &self.outside).unwrap();
        std::os::unix::fs::symlink(&self.outside, &self.source).unwrap();
        self.inner.prepare(input, ctx)
    }
}

fn swapped_plain_run(follow: bool) {
    let root = common::Scratch::new("kaban-e38-path", if follow { "follow" } else { "nofollow" });
    let outside = common::Scratch::new(
        "kaban-e38-outside",
        if follow { "follow" } else { "nofollow" },
    );
    let source = root.join("docs");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("a.md"),
        "# Outside\n\nDo not ingest through the swapped directory.\n",
    )
    .unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let api = Arc::new(KabanApi::new(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
    ));
    let config = IngestConfig {
        workers: 1,
        queue_depth: 1,
        ..IngestConfig::default()
    };
    let inner = DocumentPreparer::new(
        &api,
        store.clone(),
        store.signer(),
        ChunkConfig::default(),
        config.clone(),
    );
    let preparer = Arc::new(SwapDirectory {
        inner,
        source,
        outside: outside.join("moved"),
    });
    let ingestor = Ingestor::new(api, store.clone(), store.signer(), preparer, config);
    let mut walk = WalkConfig::new(root.to_path_buf());
    walk.follow_symlinks = follow;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(ingestor.run(&walk, CancellationToken::new()))
        .unwrap();
    assert_eq!(
        report.documents, 0,
        "plain ingestion read through an escaping intermediate symlink"
    );
    assert_eq!(
        report.chunks_upserted, 0,
        "outside chunk content was published"
    );
}

#[test]
fn plain_ingestion_refuses_swapped_intermediate_directory() {
    swapped_plain_run(false);
}

#[test]
fn following_symlinks_does_not_permit_a_swapped_directory_escape() {
    swapped_plain_run(true);
}

#[test]
fn permitted_inside_root_symlink_still_reads_the_walked_file() {
    use std::io::Read;
    let root = common::Scratch::new("kaban-e38-path", "inside-link");
    fs::write(root.join("target.txt"), "inside content").unwrap();
    std::os::unix::fs::symlink("target.txt", root.join("link.txt")).unwrap();
    let mut config = WalkConfig::new(root.to_path_buf());
    config.roots = vec![root.join("link.txt")];
    config.follow_symlinks = true;
    let candidate = walk(&config).unwrap().next().unwrap().unwrap();
    let mut text = String::new();
    candidate
        .open_checked()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "inside content");
}

#[test]
fn replacing_workspace_root_does_not_rebind_a_candidate() {
    let parent = common::Scratch::new("kaban-e38-path", "root-swap");
    let root = parent.join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "content").unwrap();
    let candidate = walk(&WalkConfig::new(root.clone()))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    fs::rename(&root, parent.join("old-root")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::hard_link(parent.join("old-root/a.txt"), root.join("a.txt")).unwrap();
    assert!(
        candidate.open_checked().is_err(),
        "same final inode under a different root must refuse"
    );
}
