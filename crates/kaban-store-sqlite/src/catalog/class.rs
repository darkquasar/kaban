//! Index classes and the extents they own (spec 11 KER-01, ANA-07): the one trait every index
//! implementation commits through, and the class/location vocabulary of the catalog rows.

use kaban_core::Result;
use kaban_core::query_budget::QueryBudget;
use rusqlite::Connection;

use super::Catalog;
use super::capture::BatchChanges;
use super::commit::Stage;
use super::pin::Pin;
use super::tables::{ExtentRef, SedimentRow};

/// The class of an extent: which index wrote it and which index reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ExtentClass {
    /// The text index tier.
    TextTier,
    /// The memory graph's base.
    GraphBase,
    /// A memory graph layer over the base.
    GraphLayer,
    /// A code graph layer.
    CodeLayer,
    /// A code graph's base.
    CodeBase,
    /// The class of the test-only index class (`catalog::testing`); never written in production.
    #[cfg(any(test, feature = "failpoints"))]
    Test,
}

impl ExtentClass {
    /// The class's name in the catalog rows.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TextTier => "text_tier",
            Self::GraphBase => "graph_base",
            Self::GraphLayer => "graph_layer",
            Self::CodeLayer => "code_layer",
            Self::CodeBase => "code_base",
            #[cfg(any(test, feature = "failpoints"))]
            Self::Test => "test",
        }
    }
    /// The class a catalog row names, `None` for a name this build does not know.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "text_tier" => Some(Self::TextTier),
            "graph_base" => Some(Self::GraphBase),
            "graph_layer" => Some(Self::GraphLayer),
            "code_layer" => Some(Self::CodeLayer),
            "code_base" => Some(Self::CodeBase),
            #[cfg(any(test, feature = "failpoints"))]
            "test" => Some(Self::Test),
            _ => None,
        }
    }
}

/// Where an extent's bytes live. `File` exists so verify can report a missing cold file
/// (KER-08, ANA-07); no producer writes it in this stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtentLocation {
    /// A byte range of `kaban.kbx`.
    Kbx {
        /// Start of the range.
        offset: u64,
        /// Length of the range.
        len: u64,
    },
    /// A cold file relative to the database's directory.
    File {
        /// The path relative to the database's directory.
        relpath: String,
    },
}

/// One index implementation over the catalog. `stage` runs inside [`Catalog::commit`] with the
/// commit's frozen change set and must be idempotent per transaction; `seal`, `fold` and
/// `purge` run outside the writer lock against a pin, on inputs the compaction already claimed
/// (`super::compact`), and return the new extent's bytes (`None`: nothing to write). Each runs
/// under its maintenance budget (spec 11 7.6): it reserves what it holds in `budget` and fails
/// with the budget's error when a reservation is refused, so the compaction retries a smaller
/// unit. T7, T11 and T16 bring the concrete classes (`super::classes`); the test class lives in
/// `super::testing`.
#[expect(dead_code, reason = "T7's reindex calls `rebuild`")]
pub(crate) trait IndexClass: Send + Sync {
    /// The classes of the extents and sediment this index owns (a layer class and its base
    /// class): compaction and purge select by them. The first is the class its seals write.
    fn classes(&self) -> &[ExtentClass];
    /// Stage this commit's changes (sediment rows) inside the writer transaction; `txn` stages
    /// sediment and nothing else.
    ///
    /// # Errors
    /// Any error rolls the whole commit back.
    fn stage(&self, txn: &mut Stage<'_>, changes: &BatchChanges) -> Result<()>;
    /// Seal the pinned sediment rows `rows` (this class's, in sequence order) into one extent.
    ///
    /// # Errors
    /// The budget's refusal; read or encode failures.
    fn seal(
        &self,
        pin: &Pin,
        rows: &[&SedimentRow],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>>;
    /// Fold the pinned extents `extents` (consecutive in one view's chain, oldest first) into
    /// one extent that takes their place.
    ///
    /// # Errors
    /// The budget's refusal; read or encode failures.
    fn fold(
        &self,
        pin: &Pin,
        extents: &[ExtentRef],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>>;
    /// Rewrite the pinned extent `extent` without the records of `rids` (explicit omission,
    /// spec 11 7.8); `None` when it holds none of them.
    ///
    /// # Errors
    /// The budget's refusal; read or encode failures.
    fn purge(
        &self,
        pin: &Pin,
        extent: &ExtentRef,
        rids: &[i64],
        budget: &QueryBudget,
    ) -> Result<Option<Vec<u8>>>;
    /// Rebuild this index from the canonical rows.
    ///
    /// # Errors
    /// Read or encode failures.
    fn rebuild(&self, catalog: &Catalog, rows: &Connection) -> Result<()>;
}
