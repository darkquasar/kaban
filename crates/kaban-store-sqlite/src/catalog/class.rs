//! Index classes and the extents they own (spec 11 KER-01, ANA-07): the one trait every index
//! implementation commits through, and the class/location vocabulary of the catalog rows.

use kaban_core::Result;
use rusqlite::Connection;

use super::Catalog;
use super::capture::BatchChanges;
use super::commit::Stage;
use super::pin::Pin;
use super::tables::{ExtentRef, SedimentSeq};

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
    /// How many classes this build knows.
    #[cfg(not(any(test, feature = "failpoints")))]
    pub(crate) const COUNT: usize = 5;
    /// How many classes this build knows (the test class among them).
    #[cfg(any(test, feature = "failpoints"))]
    pub(crate) const COUNT: usize = 6;

    /// The class's position among the [`Self::COUNT`] classes.
    pub(crate) fn index(self) -> usize {
        match self {
            Self::TextTier => 0,
            Self::GraphBase => 1,
            Self::GraphLayer => 2,
            Self::CodeLayer => 3,
            Self::CodeBase => 4,
            #[cfg(any(test, feature = "failpoints"))]
            Self::Test => 5,
        }
    }
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

/// What a seal, fold or purge produced outside the writer lock: the new extent's bytes and the
/// inputs they replace, published by a later commit.
#[expect(dead_code, reason = "T6's compaction publishes it")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SealOutput {
    /// The new extent's bytes.
    pub(crate) bytes: Vec<u8>,
    /// The extents the output replaces.
    pub(crate) extents: Vec<ExtentRef>,
    /// The sediment rows the output folds in.
    pub(crate) sediment: Vec<SedimentSeq>,
}

/// One index implementation over the catalog. `stage` runs inside [`Catalog::commit`] with the
/// commit's frozen change set and must be idempotent per transaction; `seal`, `fold` and
/// `purge` run outside the writer lock against a pin and return bytes plus the inputs they
/// consumed. T7, T11 and T16 bring the concrete classes; T5 has a test class only.
#[expect(
    dead_code,
    reason = "T6's compaction and rebuild call the methods besides `stage`"
)]
pub(crate) trait IndexClass: Send + Sync {
    /// The class of the extents and sediment this index owns.
    fn class(&self) -> ExtentClass;
    /// Stage this commit's changes (sediment rows) inside the writer transaction; `txn` stages
    /// sediment and nothing else.
    ///
    /// # Errors
    /// Any error rolls the whole commit back.
    fn stage(&self, txn: &mut Stage<'_>, changes: &BatchChanges) -> Result<()>;
    /// Seal the pinned sediment into an extent.
    ///
    /// # Errors
    /// Read or encode failures.
    fn seal(&self, pin: &Pin) -> Result<Option<SealOutput>>;
    /// Fold the pinned extents into fewer.
    ///
    /// # Errors
    /// Read or encode failures.
    fn fold(&self, pin: &Pin) -> Result<Option<SealOutput>>;
    /// Rewrite every pinned extent that still holds one of `rids`, omitting them.
    ///
    /// # Errors
    /// Read or encode failures.
    fn purge(&self, pin: &Pin, rids: &[i64]) -> Result<Option<SealOutput>>;
    /// Rebuild this index from the canonical rows.
    ///
    /// # Errors
    /// Read or encode failures.
    fn rebuild(&self, catalog: &Catalog, rows: &Connection) -> Result<()>;
}
