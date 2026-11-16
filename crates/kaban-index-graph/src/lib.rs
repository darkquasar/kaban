//! The graph engine (spec 11 T9): one extent format for the base and sealed layers, a merged
//! view over base, layers and sediment records with one neighbour accessor, and the structural
//! functions over it. The crate is pure: no SQL, no I/O and no `unsafe` code.

mod base;
mod error;
mod facts;
mod id;
mod layer;
mod op;
mod paths;
mod presence;
mod structure;
mod traverse;
mod view;

pub use base::{Base, build_base, fold};
pub use error::GraphError;
pub use id::{Direction, LocalId, RelId, RelSelection, StableOrder, TieRank};
pub use layer::{Layer, seal};
pub use op::{Attr, EdgeOp, FactOp, NodeOp, OpKind, Records};
pub use paths::{meet, shortest_path};
pub use structure::{common_neighbours, degree, refuse_not_planned, topo_order, wcc};
pub use traverse::{Caps, Reached, RowCap, closure, expand, reachable, reverse_closure};
pub use view::View;
