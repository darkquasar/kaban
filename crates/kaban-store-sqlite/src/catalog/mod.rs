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
pub(crate) mod phase;
pub(crate) mod pin;
pub(crate) mod tables;
#[cfg(feature = "failpoints")]
pub mod testing;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
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
    /// Set only while [`Self::commit`] commits (and around an explicit maintenance statement,
    /// [`Self::outside_commit`]): the writer guard of test builds refuses any other commit on
    /// the writer connection.
    committing: Arc<AtomicBool>,
    /// The writer connection's commit phase, which its authorizer enforces (`phase`).
    phases: Arc<phase::Phases>,
    /// A unit test's derived-row hook ([`Self::set_test_derived_rows`]).
    #[cfg(test)]
    test_derived: std::sync::Mutex<Option<commit::TestDerivedRows>>,
    /// A unit test's observer of every row a pin copies ([`Self::set_test_copy_hook`]).
    #[cfg(test)]
    test_copy: std::sync::Mutex<Option<TestCopyHook>>,
}

/// A unit test's observer of the rows a pin copies, by copy index.
#[cfg(test)]
pub(crate) type TestCopyHook = Box<dyn Fn(usize) + Send>;

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("kbx", &self.kbx)
            .field("degraded_reclamation", &self.degraded_reclamation)
            .field("classes", &self.classes().len())
            .field("committing", &self.committing)
            .field("phases", &self.phases)
            .finish_non_exhaustive()
    }
}

impl Catalog {
    /// Create or open `kaban.kbx`, `kaban.kbx.alloc` and `kaban.kbx.pins/` beside `db_path`
    /// and check that the database on `conn` holds the catalog tables, then take `conn` as the
    /// writer connection (its capture triggers; [`Self::attach_writer`]). Called only once the
    /// open admitted the database and published its version (spec 11 CMP-01) and, where it
    /// admits a key, after the key ([`Self::create_artefacts`]): a refused project never gains
    /// a catalog artefact. The hole-punch probe (KER-11) arrives with the
    /// container I/O (T5 Step 5); until then reclamation is not degraded.
    ///
    /// # Errors
    /// `StoreError::FileMode` for an artefact that cannot be created or is not of its type;
    /// SQLite errors (a database without the catalog tables among them).
    pub(crate) fn open(db_path: &Path, conn: &mut Connection) -> Result<Self> {
        let catalog = Self::open_as_is(db_path, conn)?;
        catalog.create_artefacts()?;
        Ok(catalog)
    }

    /// Create the container, the allocation lock and the pins directory where they are
    /// missing. An open that admits a key calls it only after the key's admission, so a
    /// project refused for its key gains no catalog artefact (the catalog itself attaches
    /// before, through [`Self::open_as_is`]: the key record commits through it).
    ///
    /// # Errors
    /// `StoreError::FileMode` for an artefact that cannot be created or is not of its type.
    pub(crate) fn create_artefacts(&self) -> Result<()> {
        kbx::create_artefacts(&self.kbx)
    }

    /// [`Self::open`] for an open that serves the database as it is (verification, doctor, a
    /// host hook: they create nothing): the artefacts are used where they exist and never
    /// created. Commit, sediment and pins need the rows only.
    ///
    /// # Errors
    /// SQLite errors (a database without the catalog tables among them).
    pub(crate) fn open_as_is(db_path: &Path, conn: &mut Connection) -> Result<Self> {
        reclaim_floor(conn)?;
        Self::with_container(kbx::kbx_path(db_path)).attach_writer(conn)
    }

    /// The catalog of an in-memory database: its rows only, no container beside it.
    ///
    /// # Errors
    /// SQLite errors (a database without the catalog tables among them).
    pub(crate) fn in_memory(conn: &mut Connection) -> Result<Self> {
        reclaim_floor(conn)?;
        Self::with_container(PathBuf::new()).attach_writer(conn)
    }

    /// The catalog as a read connection of its own sees it (`crate::index_rows`): the one
    /// read primitive, [`Self::read`], over that connection. Nothing is checked or created and
    /// no writer is attached; `db_path` is `None` for the in-memory store's shared connection.
    pub(crate) fn reader(db_path: Option<&Path>) -> Self {
        Self::with_container(db_path.map(kbx::kbx_path).unwrap_or_default())
    }

    fn with_container(kbx: PathBuf) -> Self {
        Self {
            kbx,
            degraded_reclamation: false,
            classes: RwLock::new(Vec::new()),
            committing: Arc::new(AtomicBool::new(false)),
            phases: Arc::new(phase::Phases::default()),
            #[cfg(test)]
            test_derived: std::sync::Mutex::new(None),
            #[cfg(test)]
            test_copy: std::sync::Mutex::new(None),
        }
    }

    /// Take `conn` as this catalog's writer connection: the capture triggers its commits
    /// drain ([`capture::install`]), the phase authorizer ([`phase::guard_phases`]) and, in
    /// test builds, the writer guard.
    fn attach_writer(self, conn: &mut Connection) -> Result<Self> {
        capture::install(conn)?;
        phase::guard_phases(conn, &self.phases)?;
        commit::guard_writer(conn, &self.committing)?;
        Ok(self)
    }

    /// Install a unit test's observer of every row a pin copies.
    #[cfg(test)]
    pub(crate) fn set_test_copy_hook(&self, hook: TestCopyHook) {
        *self
            .test_copy
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Tell the unit test's observer that a pin copied row `n`.
    #[cfg(test)]
    fn copied(&self, n: usize) {
        if let Some(hook) = self
            .test_copy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            hook(n);
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
    #[expect(clippy::unused_self, reason = "T5 Step 4 writes the catalog's container")]
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
pub(crate) fn not_yet(op: &'static str) -> StoreError {
    StoreError::Backend {
        source: kaban_core::BackendSource::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("catalog {op} is not implemented yet"),
        )),
    }
}
