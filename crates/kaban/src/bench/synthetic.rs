//! A synthetic graph layer (spec 11 7.3, 7.6): seeded change records of `edges` edges over a
//! node set a tenth that size, with a power-law (Zipf, s = 1) out-degree, for the peak-memory
//! measurement of `seal`, `fold` and `build_base` here and the one-shot layer measurement of
//! T11 (C6).

use kaban_index_graph::{Attr, EdgeOp, LocalId, NodeOp, OpKind, Records, RelId};

/// Relation types the edges are spread over.
const RELATIONS: u64 = 8;

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
    let mut rng = Rng(seed);
    let ln_nodes = (nodes as f64).ln();
    let edges = (0..edges)
        .map(|_| {
            let src = ((rng.unit() * ln_nodes).exp() as u64 - 1).min(nodes - 1);
            EdgeOp {
                src: LocalId(src as u32),
                rel: RelId((rng.next() % RELATIONS) as u16),
                dst: LocalId((rng.next() % nodes) as u32),
                kind: OpKind::Add,
                attr: Attr {
                    weight: (rng.next() % 1_000_000).cast_signed(),
                    ts: (rng.next() % 1_000_000_000).cast_signed(),
                },
            }
        })
        .collect();
    Records {
        edges,
        nodes: (0..nodes)
            .map(|n| NodeOp {
                node: LocalId(n as u32),
                kind: OpKind::Add,
            })
            .collect(),
        facts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_generates_the_same_records_of_the_asked_size() {
        let a = generate(7, 1_000);
        assert_eq!(a.edges.len(), 1_000);
        assert!(!a.nodes.is_empty());
        assert_eq!(a, generate(7, 1_000));
        assert_ne!(a, generate(8, 1_000));
    }
}
