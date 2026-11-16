//! The catalog (spec 11 KER-01, KER-03, KER-07, KER-11): one container of index extents
//! beside the database (`kaban.kbx`), one catalog of its generations in the database, one
//! commit entry for every writer and race-free pins for every reader.
//!
//! ```text
//!   +----------+  commit  +-------------+  stage   +-------------+
//!   |  writer  | =======> |   Catalog   | =======> | IndexClass  |
//!   +----------+          +-------------+          +-------------+
//!                           |  capture ~> BatchChanges (frozen once)
//!                           v
//!   +----------+   pin    +-------------+  bytes   +-------------+
//!   |  reader  | =======> |     Pin     | =======> |  kaban.kbx  |
//!   +----------+          +-------------+          +-------------+
//! ```
//!
//! T5 Step 3 delivers the commit entry, capture, sediment and a pin of the committed rows; the
//! pin protocol of PC-8 (Step 4) and the container I/O (Steps 4, 5) refuse until delivered.

pub(crate) mod capture;
pub(crate) mod class;
pub(crate) mod commit;
pub(crate) mod io;
pub(crate) mod kbx;
pub(crate) mod pin;
pub(crate) mod tables;
#[cfg(feature = "failpoints")]
pub mod testing;

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use kaban_core::{Result, StoreError};
use rusqlite::Connection;

use class::{ExtentClass, IndexClass};
use kbx::Unpublished;

use crate::sql_error::SqlResultExt;

/// The catalog of one database: its container, its reclamation mode and its index classes.
pub(crate) struct Catalog {
    kbx: PathBuf,
    degraded_reclamation: bool,
    classes: RwLock<Vec<Arc<dyn IndexClass>>>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("kbx", &self.kbx)
            .field("degraded_reclamation", &self.degraded_reclamation)
            .field("classes", &self.classes().len())
            .finish()
    }
}

impl Catalog {
    /// Create or open `kaban.kbx`, `kaban.kbx.alloc` and `kaban.kbx.pins/` beside `db_path`
    /// and check that the database on `conn` holds the catalog tables. Called only once the
    /// open admitted the database and published its version (spec 11 CMP-01): a refused
    /// project never gains a catalog artefact. The hole-punch probe (KER-11) arrives with the
    /// container I/O (T5 Step 5); until then reclamation is not degraded.
    ///
    /// # Errors
    /// `StoreError::FileMode` for an artefact that cannot be created or is not of its type;
    /// SQLite errors (a database without the catalog tables among them).
    pub(crate) fn open(db_path: &Path, conn: &Connection) -> Result<Self> {
        reclaim_floor(conn)?;
        kbx::create_artefacts(db_path)?;
        Ok(Self::with_container(kbx::kbx_path(db_path)))
    }

    /// [`Self::open`] for an open that serves the database as it is (verification, doctor, a
    /// host hook: they create nothing): the artefacts are used where they exist and never
    /// created. Commit, sediment and pins need the rows only.
    ///
    /// # Errors
    /// SQLite errors (a database without the catalog tables among them).
    pub(crate) fn open_as_is(db_path: &Path, conn: &Connection) -> Result<Self> {
        reclaim_floor(conn)?;
        Ok(Self::with_container(kbx::kbx_path(db_path)))
    }

    /// The catalog of an in-memory database: its rows only, no container beside it.
    ///
    /// # Errors
    /// SQLite errors (a database without the catalog tables among them).
    pub(crate) fn in_memory(conn: &Connection) -> Result<Self> {
        reclaim_floor(conn)?;
        Ok(Self::with_container(PathBuf::new()))
    }

    fn with_container(kbx: PathBuf) -> Self {
        Self {
            kbx,
            degraded_reclamation: false,
            classes: RwLock::new(Vec::new()),
        }
    }

    /// Add an index class; every later commit stages it.
    pub(crate) fn register(&self, class: Arc<dyn IndexClass>) {
        self.classes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(class);
    }

    /// The registered classes, in registration order.
    pub(crate) fn classes(&self) -> Vec<Arc<dyn IndexClass>> {
        self.classes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether reclamation zero-fills because hole punching is unavailable (doctor reads it).
    #[expect(
        dead_code,
        reason = "T5 Step 5 probes hole punching; T6's doctor reads the flag"
    )]
    pub(crate) fn degraded_reclamation(&self) -> bool {
        self.degraded_reclamation
    }

    /// Append `bytes` to the container outside the writer lock: reserve a range under a short
    /// `flock` on `kaban.kbx.alloc`, then `pwrite` and `fdatasync`.
    ///
    /// # Errors
    /// Always, until T5 Step 4 delivers the container writes: an unsupported-operation
    /// `StoreError::Backend`.
    #[expect(
        dead_code,
        clippy::unused_self,
        reason = "T5 Steps 4-5 deliver the container I/O and the pin protocol that use it"
    )]
    pub(crate) fn append_extent(&self, class: ExtentClass, bytes: &[u8]) -> Result<Unpublished> {
        let _ = (class, bytes);
        Err(not_yet("append_extent"))
    }
}

/// The catalog's `reclaim_floor` (PC-8).
///
/// # Errors
/// SQLite errors, a missing `cat_meta` table or row among them.
pub(crate) fn reclaim_floor(conn: &Connection) -> Result<u64> {
    conn.query_row(
        "SELECT value FROM cat_meta WHERE key = 'reclaim_floor'",
        [],
        |r| tables::uint(r, 0),
    )
    .sql()
}

/// An operation of the catalog that a later step of T5 delivers: refused, never faked.
#[expect(
    dead_code,
    reason = "T5 Steps 4-5 deliver the container I/O and the pin protocol that use it"
)]
pub(crate) fn not_yet(op: &'static str) -> StoreError {
    StoreError::Backend {
        source: kaban_core::BackendSource::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("catalog {op} is not implemented yet"),
        )),
    }
}
