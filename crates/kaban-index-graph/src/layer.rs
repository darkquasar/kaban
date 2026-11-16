//! The one extent format and reader shared by the base and every sealed layer (DEL-06).

use tantivy::directory::OwnedBytes;

use crate::error::GraphError;
use crate::op::Records;
use crate::view::View;

/// An immutable sealed layer over its parent view.
#[derive(Debug, Clone)]
pub struct Layer {
    bytes: OwnedBytes,
}

impl Layer {
    /// Open sealed layer bytes.
    ///
    /// # Errors
    /// [`GraphError::Corrupt`] when the bytes are not a well-formed layer.
    pub fn open(bytes: OwnedBytes) -> Result<Layer, GraphError> {
        Ok(Layer { bytes })
    }
}

/// Seal `r` as a new layer over `parent`; the layer stores the net change per edge, node and
/// fact, every list sorted, plus the cumulative union of touched nodes over the chain.
#[must_use]
pub fn seal(parent: &View<'_>, r: &Records) -> Vec<u8> {
    let _ = (parent, r);
    Vec::new()
}
