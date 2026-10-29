//! The bounds of G1-F12 (stage1-plan CG2.2; cg-join-v1 §9): the list limits applied by the
//! caller after ranking, and the `neighbours` traversal bounds the reader applies itself.

/// The fixed limits of G1-F12. `default` and `max` bound every list at the tool layer
/// (CG7.1, after ranking); `neighbours_per_hub`, `neighbours_total` and `depth_max` bound the
/// `neighbours` traversal inside the reader (graph bounds, not list limits, RV-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Limits {
    /// List limit when the request gives none.
    pub default: usize,
    /// The largest list limit a request may ask for.
    pub max: usize,
    /// Edges expanded per hub in `neighbours`.
    pub neighbours_per_hub: usize,
    /// Edges returned by one `neighbours` call in total.
    pub neighbours_total: usize,
    /// The deepest `neighbours` traversal.
    pub depth_max: u8,
}

impl Limits {
    /// The spec's values: 50, 200, 200, 500, 3.
    pub const DEFAULT: Limits = Limits {
        default: 50,
        max: 200,
        neighbours_per_hub: 200,
        neighbours_total: 500,
        depth_max: 3,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A `neighbours` depth in `1..=limits.depth_max` (G1-F12) for the `Limits` it was built under.
/// The invariant is relative to those limits: the reader re-checks the value against its own
/// `Limits::depth_max` at the query boundary and refuses a deeper one with
/// `ReadError::OutOfRange` (RG2-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Depth(u8);

impl Depth {
    /// `None` outside `1..=limits.depth_max`.
    #[must_use]
    pub fn new(depth: u8, limits: &Limits) -> Option<Self> {
        (1..=limits.depth_max)
            .contains(&depth)
            .then_some(Self(depth))
    }

    /// The depth value.
    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }
}
