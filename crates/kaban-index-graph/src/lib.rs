//! The graph engine (spec 11 T9): one extent format for the base and sealed layers, a merged
//! view over base, layers and sediment records with one neighbour accessor, and the structural
//! functions over it, plus the scored and set functions (T10). The crate is pure: no SQL, no
//! I/O and no `unsafe` code.

mod base;
mod charge;
mod error;
mod facts;
mod id;
mod layer;
mod merge;
mod op;
mod paths;
mod presence;
#[cfg(test)]
mod probe;
mod rank;
mod sediment;
mod similarity;
mod structure;
mod traverse;
mod triangles;
mod view;
mod weighted;

pub use base::{Base, build_base, fold};
pub use charge::{Charged, Hold};
pub use error::GraphError;
pub use id::{Direction, LocalId, RelId, RelSelection, StableOrder, TieRank};
pub use layer::{Layer, seal};
pub use op::{Attr, DecodedRecords, EdgeOp, FactOp, NodeOp, OpKind, Records};
pub use paths::{meet, shortest_path};
pub use rank::{RankParams, Ranked, pagerank, ppr};
pub use sediment::{MAX_SEDIMENT_RUNS, PreparedSediment, Sealed};
pub use similarity::{adamic_adar, jaccard};
pub use structure::{Components, common_neighbours, degree, refuse_not_planned, topo_order, wcc};
pub use traverse::{Caps, Reached, RowCap, closure, expand, reachable, reverse_closure};
pub use triangles::{clustering, triangle_count};
pub use view::{BUDGET_CHECK_INTERVAL, View};
pub use weighted::{time_window_expand, weighted_shortest_path};
