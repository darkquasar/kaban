//! A synthetic graph layer (spec 11 7.3, 7.6): seeded change records of `edges` edges over a
//! node set a tenth that size, with a power-law (Zipf, s = 1) out-degree, for the peak-memory
//! measurement of `seal`, `fold` and `build_base` here and the one-shot layer measurement of
//! T11 (C6).

use anyhow::{Result, ensure};
use kaban_core::{Actor, IngestStore, NewNode, NodeId, Op, WorkspaceId, WriteBatch, WriteCtx};
use kaban_index_graph::{Attr, EdgeOp, LocalId, NodeOp, OpKind, Records, RelId};
use kaban_store_sqlite::SqliteStore;

/// Relation types the edges are spread over.
const RELATIONS: [&str; 8] = [
    "MENTIONS",
    "ABOUT",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "PART_OF",
    "NEXT",
    "LINKS_TO",
    "RELATES_TO",
];

const BATCH_SIZE: usize = 1024;

/// A splitmix64 stream.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Seeded records of `edges` edge additions over `max(edges / 10, 1)` nodes, every node
/// added. Sources follow Zipf with s = 1 (node `k` is drawn with weight about `1 / (k + 1)`, by
/// the inverse of the continuous CDF), so out-degree is power-law with node 0 the largest hub;
/// targets and relations are uniform. A repeated (source, relation, target) collapses to one
/// edge when applied, so the net edge count is slightly below `edges`.
pub(crate) fn generate(seed: u64, edges: u64) -> Records {
    let nodes = (edges / 10).clamp(1, u64::from(u32::MAX));
    generate_with_nodes(seed, nodes as u32, edges)
}

/// Raw graph records for independently sized node and edge populations.
pub(crate) fn generate_with_nodes(seed: u64, nodes: u32, edges: u64) -> Records {
    Records {
        edges: edge_records(seed, nodes, edges).collect(),
        nodes: (0..nodes)
            .map(|node| NodeOp {
                node: LocalId(node),
                kind: OpKind::Add,
            })
            .collect(),
        facts: Vec::new(),
    }
}

fn edge_records(seed: u64, nodes: u32, edges: u64) -> impl Iterator<Item = EdgeOp> {
    let mut rng = Rng(seed);
    let nodes = u64::from(nodes);
    let ln_nodes = (nodes as f64).ln();
    (0..if nodes == 0 { 0 } else { edges }).map(move |_| {
        let src = ((rng.unit() * ln_nodes).exp() as u64 - 1).min(nodes - 1);
        EdgeOp {
            src: LocalId(src as u32),
            rel: RelId((rng.next() % RELATIONS.len() as u64) as u16),
            dst: LocalId((rng.next() % nodes) as u32),
            kind: OpKind::Add,
            attr: Attr {
                weight: (rng.next() % 1_000_000).cast_signed(),
                ts: (rng.next() % 1_000_000_000).cast_signed(),
            },
        }
    })
}

fn public_id(ordinal: u32) -> Result<NodeId> {
    Ok(NodeId::parse(&format!(
        "00000000-0000-7000-8000-{ordinal:012x}"
    ))?)
}

#[derive(Clone, Copy)]
enum BatchPhase {
    Setup,
    Append,
}

fn write_batches(
    store: &SqliteStore,
    mut ops: impl Iterator<Item = Result<Op>>,
    batch_size: usize,
    phase: BatchPhase,
) -> Result<()> {
    let ctx = WriteCtx::new(WorkspaceId::default(), Actor::Agent);
    loop {
        let batch = ops.by_ref().take(batch_size).collect::<Result<Vec<_>>>()?;
        if batch.is_empty() {
            return Ok(());
        }
        store.apply_batch(WriteBatch::new(WorkspaceId::default(), batch), &ctx)?;
        if matches!(phase, BatchPhase::Setup) {
            store.checkpoint_if_due()?;
        }
    }
}

/// Write all canonical nodes before the edge-only batches, under stable public UUIDs.
pub(crate) fn populate(store: &SqliteStore, seed: u64, nodes: u32, edges: u64) -> Result<()> {
    ensure!(edges == 0 || nodes >= 2, "edges require at least two nodes");
    let ops = (0..nodes).map(|ordinal| {
        Ok(Op::UpsertNode {
            id: public_id(ordinal)?,
            node: NewNode {
                kind: "Fact".into(),
                text: format!("node {ordinal}"),
                ..Default::default()
            },
        })
    });
    write_batches(store, ops, BATCH_SIZE, BatchPhase::Setup)?;
    write_batches(
        store,
        edge_ops(seed, nodes, edges),
        BATCH_SIZE,
        BatchPhase::Setup,
    )
}

/// Append edge-only batches. Re-upserting source nodes would reconcile away their
/// existing outgoing edges.
pub(crate) fn append_edges(
    store: &SqliteStore,
    seed: u64,
    nodes: u32,
    edges: u64,
    batch_size: usize,
) -> Result<()> {
    ensure!(batch_size > 0, "edge batch size must be positive");
    ensure!(edges == 0 || nodes >= 2, "edges require at least two nodes");
    write_batches(
        store,
        edge_ops(seed, nodes, edges),
        batch_size,
        BatchPhase::Append,
    )
}

fn edge_ops(seed: u64, nodes: u32, edges: u64) -> impl Iterator<Item = Result<Op>> {
    edge_records(seed, nodes, edges).map(move |edge| {
        // A raw layer can contain loops. If its target is the source, use
        // another draw from this record to select among the other ordinals.
        let dst = if edge.dst == edge.src {
            let draw = (edge.attr.ts.cast_unsigned() % u64::from(nodes - 1)) as u32;
            draw + u32::from(draw >= edge.src.0)
        } else {
            edge.dst.0
        };
        Ok(Op::UpsertEdge {
            src: public_id(edge.src.0)?,
            dst: public_id(dst)?,
            rel: RELATIONS[usize::from(edge.rel.0)].into(),
            props: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_generates_the_same_records_of_the_asked_size() {
        let a = generate(7, 1_000);
        assert_eq!(a.edges.len(), 1_000);
        assert_eq!(a.nodes.len(), 100);
        assert_eq!(a, generate(7, 1_000));
        assert_ne!(a, generate(8, 1_000));
    }

    #[test]
    fn canonical_batches_keep_prior_edges_and_persist_valid_links() -> anyhow::Result<()> {
        use kaban_core::{ChangeEntity, ChangeLogStore, ChangeOp, MemoryStore};

        let store = SqliteStore::open_memory()?;
        populate(&store, 7, 8, 40)?;
        let initial = store.stats(None)?;
        assert_eq!(initial["nodes"], 8);
        let edges = initial["edges"].as_u64().unwrap_or_default();
        assert!(edges > 0 && edges <= 40);
        let cursor = store.changes_read(0, 128)?.last().map_or(0, |c| c.seq);
        append_edges(&store, 8, 8, 40, 3)?;
        let after = store.stats(None)?;
        assert_eq!(after["nodes"], 8);
        assert!(after["edges"].as_u64().unwrap_or_default() > edges);
        assert!(
            store
                .changes_read(cursor, 128)?
                .iter()
                .all(|c| c.entity == ChangeEntity::Edge && c.op != ChangeOp::Delete)
        );
        Ok(())
    }
}
