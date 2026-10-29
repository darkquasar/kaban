//! [`SchemaStore`]: the label and relationship-type registry role trait.

use crate::error::Result;
use crate::model::TypeDef;

/// The label and relationship-type registry (core-spec F5): read it, extend it. Used by the
/// service layer's schema path and the MCP adapter.
pub trait SchemaStore: Send + Sync {
    /// All schema entries (labels first, builtin first, then by name).
    ///
    /// # Errors
    /// Backend errors.
    fn schema(&self) -> Result<Vec<TypeDef>>;
    /// Register or update a non-builtin schema entry.
    ///
    /// # Errors
    /// `InvalidName`, `UnknownKind` (endpoint kinds), `Builtin` or a backend error.
    fn schema_extend(&self, def: TypeDef) -> Result<TypeDef>;
}
