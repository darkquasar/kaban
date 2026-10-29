//! Goldens of the prepared graph over the Markdown fixtures (docs-ingest-plan.md DI1.7;
//! docs-ingest-spec D2, D3, D5; `crates/kaban-ingest/docs/ids.md`): for every fixture under
//! `tests/fixtures/md/` one `insta` snapshot of what the committer would write, in the shape
//! the ids page freezes: the document id, the section ids with their heading paths in order,
//! the chunk ids with kind and token estimate, the edges of `build_batch` grouped by type as
//! `(src, dst)` ids, and the sanitized `content_hash`.
//!
//! Every value here is derived from the sanitized texts (ids.md: no timestamp, no minted id,
//! no signer input on a clean document), so the snapshots carry the real ids with no `insta`
//! redaction and are stable across runs and machines; the second preparation of the same bytes
//! is asserted byte-identical to the first before the snapshot is compared.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::{Op, WorkspaceId};
use kaban_ingest::{
    ChunkConfig, ExtractCtx, Format, IngestConfig, PrepareRequest, PreparedDocument, SourceId,
    build_batch, gate_path, prepare_document, resolve_links,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The fixtures, each prepared as `docs/<name>` of the default workspace: the four DI1.2
/// fixtures under `tests/fixtures/md/` and the DI1.7 `mixed.md` under `tests/fixtures/md-golden/`
/// (a sibling directory because `crates/kaban/tests/mcp_docs.rs` and its `index_status_fixtures`
/// golden ingest the whole `md/` directory and count four documents).
const FIXTURES: [(&str, &str); 5] = [
    ("md", "code-tables.md"),
    ("md", "crlf.md"),
    ("md", "headings.md"),
    ("md", "links.md"),
    ("md-golden", "mixed.md"),
];

/// What the snapshot pins (field order is the snapshot order).
#[derive(Serialize)]
struct Golden {
    path: String,
    source_id: String,
    document_id: String,
    title: Option<String>,
    content_hash: String,
    sections: Vec<GoldenSection>,
    chunks: Vec<GoldenChunk>,
    /// `rel -> [(src, dst)]` in the order `build_batch` emits them.
    edges: BTreeMap<String, Vec<(String, String)>>,
    tags: Vec<String>,
    links: Vec<String>,
}

#[derive(Serialize)]
struct GoldenSection {
    id: String,
    heading_path: Vec<String>,
    kind: String,
}

#[derive(Serialize)]
struct GoldenChunk {
    id: String,
    section: String,
    kind: String,
    token_estimate: u32,
}

fn fixture(dir: &str, name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(dir)
            .join(name),
    )
    .unwrap()
}

/// Prepare `docs/<name>` from `bytes` and shape the committer's view of it.
fn golden(name: &str, bytes: &[u8]) -> Golden {
    let policy = Policy::empty();
    let signer = SqliteStore::open_memory().unwrap().signer();
    let ws = WorkspaceId::default();
    let path = format!("docs/{name}");
    let gated = gate_path(&policy, &path).unwrap();
    let source = SourceId::new(&ws, &gated);
    let ctx = ExtractCtx::new(
        CancellationToken::new(),
        tokio::time::Instant::now() + Duration::from_secs(10),
        source.clone(),
    );
    let cfg = ChunkConfig::default();
    let prepared = prepare_document(
        &policy,
        signer.as_ref(),
        &ctx,
        &ws,
        &PrepareRequest {
            source: &source,
            path: &gated,
            format: Format::Markdown,
            size: u64::try_from(bytes.len()).unwrap(),
            mtime: 1_793_004_407,
            bytes,
            config_hash: "golden",
        },
        &cfg,
    )
    .unwrap();
    let PreparedDocument::Ready { graph, .. } = prepared else {
        panic!("{name}: a clean fixture prepares as Ready, got {prepared:?}");
    };

    // no target is stored: every wikilink binds to a placeholder id, which is derived too
    let links = resolve_links(&policy, &ws, &path, graph.links.clone(), |_, _| false);
    let batch = build_batch(&graph, &links, None, &[], &IngestConfig::default(), &ws);
    let mut edges: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for op in &batch.ops {
        if let Op::UpsertEdge { src, dst, rel, .. } = op {
            edges
                .entry(rel.clone())
                .or_default()
                .push((src.to_string(), dst.to_string()));
        }
    }

    Golden {
        path,
        source_id: source.as_str().to_owned(),
        document_id: graph.document.id.to_string(),
        title: graph.document.props.title.clone(),
        content_hash: graph
            .document
            .props
            .content_hash
            .clone()
            .expect("a Ready document carries its content_hash"),
        sections: graph
            .sections
            .iter()
            .map(|s| GoldenSection {
                id: s.id.to_string(),
                heading_path: s.props.heading_path.clone(),
                kind: kind_name(&s.props.kind),
            })
            .collect(),
        chunks: graph
            .sections
            .iter()
            .flat_map(|s| {
                s.chunks.iter().map(move |c| GoldenChunk {
                    id: c.id.to_string(),
                    section: s.id.to_string(),
                    kind: kind_name(&c.props.kind),
                    token_estimate: c.props.token_estimate,
                })
            })
            .collect(),
        edges,
        tags: graph.tags.iter().map(|t| t.tag.clone()).collect(),
        links: graph.links.iter().map(|l| l.raw_target.clone()).collect(),
    }
}

/// The serde spelling of a kind enum (`heading`, `table`, `{"code":{"lang":"rust"}}`), so the
/// snapshot shows what the store persists, not the Rust variant.
fn kind_name<T: Serialize>(kind: &T) -> String {
    match serde_json::to_value(kind).unwrap() {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// Focused postcheck (insta, DI1.7): the prepared graph of every fixture. A second preparation
/// of the same bytes is byte-identical (ids.md: every id is a function of the sanitized texts),
/// then the snapshot pins the ids, the section order, the chunk kinds and estimates, the edges
/// by type and the sanitized `content_hash`.
#[test]
fn prepared_graphs_of_the_markdown_fixtures_are_deterministic_and_match_their_goldens() {
    for (dir, name) in FIXTURES {
        let bytes = fixture(dir, name);
        let first = serde_json::to_string_pretty(&golden(name, &bytes)).unwrap();
        let second = serde_json::to_string_pretty(&golden(name, &bytes)).unwrap();
        assert_eq!(first, second, "{name}: two preparations differ");
        insta::assert_snapshot!(name, first);
    }
}

/// The fifth fixture covers what the four earlier ones split between them: frontmatter, a
/// fenced block with an info string, a table and two wikilinks.
#[test]
fn the_mixed_fixture_covers_frontmatter_fence_table_and_two_wikilinks() {
    let g = golden("mixed.md", &fixture("md-golden", "mixed.md"));
    assert_eq!(g.title.as_deref(), Some("Mixed fixture"));
    assert!(
        g.sections.iter().any(|s| s.kind == "frontmatter"),
        "{:?}",
        g.sections.iter().map(|s| &s.kind).collect::<Vec<_>>()
    );
    assert!(
        g.chunks.iter().any(|c| c.kind.contains("code")),
        "{:?}",
        g.chunks.iter().map(|c| &c.kind).collect::<Vec<_>>()
    );
    assert!(g.chunks.iter().any(|c| c.kind == "table"));
    assert_eq!(g.links.len(), 2, "{:?}", g.links);
    assert_eq!(g.edges["LINKS_TO"].len(), 2);
    assert_eq!(g.tags, vec!["golden"]);
}
