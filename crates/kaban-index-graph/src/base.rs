//! The base extent: a flattened view in the shared extent format.

use tantivy::directory::OwnedBytes;

use crate::error::GraphError;
use crate::id::{Direction, LocalId, RelSelection};
use crate::layer::{Extent, KIND_BASE, Net};
use crate::op::{OpKind, Records};
use crate::view::View;

/// An immutable base: the oldest member of a view's chain.
#[derive(Debug, Clone)]
pub struct Base {
    pub(crate) extent: Extent,
}

impl Base {
    /// Open base bytes.
    ///
    /// # Errors
    /// [`GraphError::Corrupt`] when the bytes are not a well-formed base: truncated, padded, a
    /// layer, an unknown or duplicate section, or a missing presence, edge or fact section.
    pub fn open(bytes: OwnedBytes) -> Result<Base, GraphError> {
        Ok(Base {
            extent: Extent::open(bytes, KIND_BASE)?,
        })
    }
}

/// Build a base from `r` applied in order to an empty graph, over the id domain `0..n` (widened
/// to cover every id in `r`). Only surviving nodes, edges and facts are stored.
#[must_use]
pub fn build_base(r: &Records, n: u32) -> Vec<u8> {
    let mut net = Net::default();
    net.apply(r);
    net.nodes.retain(|_, k| *k == OpKind::Add);
    net.out.retain(|_, (k, _)| *k == OpKind::Add);
    net.inc.retain(|_, (k, _)| *k == OpKind::Add);
    net.facts.retain(|_, (k, _)| *k == OpKind::Add);
    let domain = u64::from(n).max(net.domain());
    Extent::write(KIND_BASE, 0, domain, &net, None)
}

/// Flatten `view` into base bytes, dropping the `omit` nodes, their edges and their facts (the
/// purge path). Edges with an absent endpoint are not written.
#[must_use]
pub fn fold(view: &View<'_>, omit: &[LocalId]) -> Vec<u8> {
    let mut omit = omit.to_vec();
    omit.sort_unstable();
    let kept = |n: LocalId| omit.binary_search(&n).is_err();
    let mut net = Net::default();
    for n in view.nodes().filter(|&n| kept(n)) {
        net.nodes.insert(n.0, OpKind::Add);
        for (m, rel, attr) in view.neighbours(n, &RelSelection::All, Direction::Out) {
            if kept(m) {
                net.out.insert((n.0, m.0, rel.0), (OpKind::Add, attr));
                net.inc.insert((m.0, n.0, rel.0), (OpKind::Add, attr));
            }
        }
    }
    net.facts = view.merged_facts();
    net.facts
        .retain(|&(n, _), (k, _)| *k == OpKind::Add && net.nodes.contains_key(&n));
    Extent::write(KIND_BASE, 0, view.domain(), &net, None)
}
