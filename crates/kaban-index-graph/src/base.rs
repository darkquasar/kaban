//! The base extent: a flattened view in the shared extent format.

use tantivy::directory::OwnedBytes;

use crate::error::GraphError;
use crate::id::LocalId;
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
    /// layer, a section count other than four, sections out of tag order, or any invariant of
    /// a section broken (see [`crate::Layer::open`]).
    pub fn open(bytes: OwnedBytes) -> Result<Base, GraphError> {
        Ok(Base {
            extent: Extent::open(bytes, KIND_BASE)?,
        })
    }
}

/// Build a base from `r` applied in order to an empty graph. The id domain is `n` widened to
/// one past the highest id that survives in the net change (removed nodes, edges and facts are
/// dropped first, so ids named only by removals do not widen it). Only surviving nodes, edges
/// and facts are stored.
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

/// Flatten `view` into base bytes, dropping the `omit` nodes, their edges (both directions)
/// and their facts (the purge path). Everything else that survives is written, including the
/// edges and facts of absent nodes that no removal deleted, so folding at any point of a
/// history never changes a later view (MED-02); presence still hides them.
#[must_use]
pub fn fold(view: &View<'_>, omit: &[LocalId]) -> Vec<u8> {
    let mut omit = omit.to_vec();
    omit.sort_unstable();
    let kept = |n: u32| omit.binary_search(&LocalId(n)).is_err();
    let mut net = Net::default();
    for n in view.present(None).filter_map(Result::ok) {
        if kept(n.0) {
            net.nodes.insert(n.0, OpKind::Add);
        }
    }
    for n in view.listed(true).into_iter().filter(|&n| kept(n)) {
        for (m, rel, attr) in view.alive(n, true) {
            if kept(m) {
                net.out.insert((n, m, rel), (OpKind::Add, attr));
                net.inc.insert((m, n, rel), (OpKind::Add, attr));
            }
        }
    }
    net.facts = view
        .alive_facts()
        .into_iter()
        .filter(|&((n, _), _)| kept(n))
        .map(|(k, p)| (k, (OpKind::Add, p)))
        .collect();
    Extent::write(KIND_BASE, 0, view.domain(), &net, None)
}
