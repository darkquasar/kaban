//! The store's own text index (spec 11 T7): the Tantivy adapter over the catalog, composed by
//! every open. It holds no storage of its own: a commit stages its text pack through the
//! catalog's text class (`crate::catalog::text_class`), so the commit is searchable by every
//! store of the database once it returns, and a search reads one pinned snapshot of the
//! catalog's text tiers and packs.
//!
//! Lock order (constraint 1): the store's connection lock, then the adapter, never the
//! reverse. Recall releases the lock before every adapter call (`recall::Access`).

use kaban_core::text_index::TextIndex;
use kaban_index_tantivy::TantivyIndex;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

/// The seed index of a store (spec 05 SS-07): the adapter every open composes, or, under
/// `text-index-override`, one a test or bench injected through `SqliteStore::with_text_index`.
pub(crate) enum Attached {
    Tantivy(Arc<TantivyIndex>),
    #[cfg(feature = "text-index-override")]
    Override(Box<dyn TextIndex + Send + Sync>),
}

impl std::fmt::Debug for Attached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tantivy(index) => index.fmt(f),
            #[cfg(feature = "text-index-override")]
            Self::Override(_) => f.write_str("TextIndexOverride"),
        }
    }
}

impl Attached {
    /// The index behind the seam.
    pub(crate) fn text_index(&self) -> &(dyn TextIndex + Send + Sync) {
        match self {
            Self::Tantivy(index) => index.as_ref(),
            #[cfg(feature = "text-index-override")]
            Self::Override(index) => index.as_ref(),
        }
    }

    /// The composed adapter this store configures, `None` for an injected index.
    #[cfg_attr(not(feature = "text-index-override"), allow(clippy::unnecessary_wraps))]
    pub(crate) fn tantivy(&self) -> Option<&Arc<TantivyIndex>> {
        match self {
            Self::Tantivy(index) => Some(index),
            #[cfg(feature = "text-index-override")]
            Self::Override(_) => None,
        }
    }
}

/// The store's connection as the store shares it.
pub(crate) type SharedConnection = Arc<Mutex<Connection>>;
