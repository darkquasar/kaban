//! The base extent: a flattened view in the shared extent format.

use tantivy::directory::OwnedBytes;

use crate::error::GraphError;
use crate::id::LocalId;
use crate::op::Records;
use crate::view::View;

/// An immutable base: the oldest member of a view's chain.
#[derive(Debug, Clone)]
pub struct Base {
    bytes: OwnedBytes,
}

impl Base {
    /// Open base bytes.
    ///
    /// # Errors
    /// [`GraphError::Corrupt`] when the bytes are not a well-formed base.
    pub fn open(bytes: OwnedBytes) -> Result<Base, GraphError> {
        Ok(Base { bytes })
    }
}

/// Build a base from `r` applied in order to an empty graph, over the id domain `0..n` (widened
/// to cover every id in `r`).
#[must_use]
pub fn build_base(r: &Records, n: u32) -> Vec<u8> {
    let _ = (r, n);
    Vec::new()
}

/// Flatten `view` into base bytes, dropping the `omit` nodes, their edges and their facts (the
/// purge path).
#[must_use]
pub fn fold(view: &View<'_>, omit: &[LocalId]) -> Vec<u8> {
    let _ = (view, omit);
    Vec::new()
}
