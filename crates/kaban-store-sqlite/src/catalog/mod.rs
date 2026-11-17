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
//! A pin follows PC-8 (`pin`, `pins`): it locks a candidate generation in `kaban.kbx.pins/`,
//! then builds itself from a fresh read transaction that checks `reclaim_floor`, so reclamation
//! never destroys bytes a live pin can reach. Extents are appended outside the writer lock
//! (`kbx`) and published by a commit (`commit::Txn::publish`). Every sync of an artefact, the
//! hole-punch probe and the release of a reclaimed range go through one I/O seam (`io`): a
//! failed sync is `StoreError::StorageFatal`, never retried (KER-11).

pub(crate) mod capture;
pub(crate) mod class;
pub(crate) mod commit;
pub(crate) mod failpoint;
pub(crate) mod io;
pub(crate) mod kbx;
pub(crate) mod phase;
pub(crate) mod pin;
pub(crate) mod pins;
pub(crate) mod tables;
#[cfg(feature = "failpoints")]
pub mod testing;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, PoisonError, RwLock};

use kaban_core::Result;
use rusqlite::Connection;

use class::{ExtentClass, IndexClass};
use kbx::Unpublished;

use crate::sql_error::SqlResultExt;

/// The catalog of one database: its container, its reclamation mode and its index classes.
pub(crate) struct Catalog {
    kbx: PathBuf,
    /// The container's mapping, shared by this catalog's pins (and kept by a detached one).
    mapping: Arc<kbx::Mapping>,
    /// The pins directory beside the container (`None` for an in-memory database: no
    /// container, nothing to reclaim).
    pins: Option<Arc<Path>>,
    /// How reclaimed ranges are released, as the open's probe found it (KER-11); `None` when
    /// nothing was probed (an in-memory database, a reader, or an as-is open whose container
    /// is absent or cannot be probed): such a catalog reclaims nothing.
    reclamation: Option<io::Reclamation>,
    classes: RwLock<Vec<Arc<dyn IndexClass>>>,
    /// Set only while [`Self::commit`] commits (and around an explicit maintenance statement,
    /// [`Self::outside_commit`]): the writer guard of test builds refuses any other commit on
    /// the writer connection.
    committing: Arc<AtomicBool>,
    /// The writer connection's commit phase, which its authorizer enforces (`phase`).
    phases: Arc<phase::Phases>,
    /// The race tests' hooks in the pin protocol's windows.
    #[cfg(any(test, feature = "failpoints"))]
    failpoints: failpoint::Failpoints,
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
            .field("reclamation", &self.reclamation)
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
    /// a catalog artefact. The artefacts are made durable and hole punching is probed once
    /// (KER-11, [`Self::create_artefacts`]).
    ///
    /// # Errors
    /// `StoreError::FileMode` for an artefact that cannot be created or is not of its type;
    /// `StoreError::StorageFatal` for a failed sync of one; SQLite errors (a database without
    /// the catalog tables among them).
    pub(crate) fn open(db_path: &Path, conn: &mut Connection) -> Result<Self> {
        let mut catalog = Self::open_as_is(db_path, conn)?;
        catalog.create_artefacts()?;
        Ok(catalog)
    }

    /// Create the container, the allocation lock and the pins directory where they are
    /// missing and sync them and their directory entries ([`kbx::create_artefacts`]), then
    /// probe hole punching unless [`Self::open_as_is`] already did. An open that admits a key
    /// calls it only after the key's admission, so a project refused for its key gains no
    /// catalog artefact (the catalog itself attaches before, through [`Self::open_as_is`]: the
    /// key record commits through it). A failed sync refuses the open; the artefacts it left
    /// are synced again by the next open, which serves the catalog only once they are durable.
    ///
    /// # Errors
    /// `StoreError::FileMode` for an artefact that cannot be created or is not of its type;
    /// `StoreError::StorageFatal` for a failed sync; the probe's open or lock errors.
    pub(crate) fn create_artefacts(&mut self) -> Result<()> {
        kbx::create_artefacts(&self.kbx)?;
        if self.reclamation.is_none() {
            self.reclamation = Some(io::probe_hole_punch(&self.kbx)?);
        }
        Ok(())
    }

    /// [`Self::open`] for an open that serves the database as it is (verification, doctor, a
    /// host hook: they create nothing): the artefacts are used where they exist and never
    /// created. Commit, sediment and pins need the rows only. Where the container and its
    /// allocation lock exist, hole punching is probed (once, [`io::probe_hole_punch`]: it
    /// changes no byte and no length); where they do not, or the probe cannot open or lock
    /// them, nothing is probed and the catalog reclaims nothing.
    ///
    /// # Errors
    /// SQLite errors (a database without the catalog tables among them).
    pub(crate) fn open_as_is(db_path: &Path, conn: &mut Connection) -> Result<Self> {
        reclaim_floor(conn)?;
        let mut catalog = Self::with_container(kbx::kbx_path(db_path));
        catalog.reclamation = io::probe_hole_punch(&catalog.kbx).ok();
        catalog.attach_writer(conn)
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
        let pins = (!kbx.as_os_str().is_empty()).then(|| Arc::from(kbx::pins_dir(&kbx).as_path()));
        Self {
            kbx,
            mapping: Arc::default(),
            pins,
            #[cfg(any(test, feature = "failpoints"))]
            failpoints: failpoint::Failpoints::default(),
            reclamation: None,
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

    /// Run the race tests' hook of `window` (test builds only).
    #[cfg(any(test, feature = "failpoints"))]
    pub(crate) fn failpoint(&self, window: failpoint::Window) {
        self.failpoints.hit(window);
    }

    /// The pin protocol's windows are a test-build hook (see the test-build twin).
    #[cfg(not(any(test, feature = "failpoints")))]
    #[expect(clippy::unused_self, reason = "the test build's twin runs a hook")]
    #[inline]
    pub(crate) fn failpoint(&self, window: failpoint::Window) {
        let _ = window;
    }

    /// Count a reader's retry (test builds only: the race tests read the count).
    #[cfg(any(test, feature = "failpoints"))]
    pub(crate) fn retried(&self) {
        self.failpoints.retried();
    }

    /// Reader retries are counted in test builds only.
    #[cfg(not(any(test, feature = "failpoints")))]
    #[expect(clippy::unused_self, reason = "the test build's twin counts")]
    #[inline]
    pub(crate) fn retried(&self) {}

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

    /// Whether reclamation zero-fills because the open's probe found no keep-size hole
    /// punching (KER-11: doctor and maintenance report `space_reclaim_unsupported`, and a
    /// zero-filled block never counts as reclaimed space).
    #[cfg_attr(
        not(feature = "failpoints"),
        expect(dead_code, reason = "T6's doctor and maintenance read the flag")
    )]
    pub(crate) fn degraded_reclamation(&self) -> bool {
        self.reclamation == Some(io::Reclamation::ZeroFill)
    }

    /// Release the container ranges `ranges` (`(offset, len)`) of extents no pin can reach any
    /// more (the caller follows PC-8's two-phase rule): punched keeping the size, or
    /// zero-filled in degraded mode ([`Self::degraded_reclamation`]), then synced once (no
    /// range, no sync). Every
    /// range reads as zeros afterwards and the container neither shrinks nor grows.
    ///
    /// # Errors
    /// `kbx::ContainerError` for a failed open, punch or write, or a catalog that probed no
    /// reclamation mode; `StoreError::StorageFatal` for the failed sync (never retried: the
    /// caller keeps the ranges' catalog rows).
    #[cfg_attr(
        not(feature = "failpoints"),
        expect(dead_code, reason = "T6's reclamation releases through it")
    )]
    pub(crate) fn reclaim_ranges(&self, ranges: &[(u64, u64)]) -> Result<()> {
        if ranges.is_empty() {
            // nothing released, nothing to sync
            return Ok(());
        }
        let Some(mode) = self.reclamation else {
            return Err(kbx::failed(
                "reclaim ranges of",
                &self.kbx,
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "the catalog probed no container to reclaim in",
                ),
            ));
        };
        let file = kbx::open_existing(&self.kbx).map_err(|e| kbx::failed("open", &self.kbx, e))?;
        for &(offset, len) in ranges {
            io::release(&file, &self.kbx, offset, len, mode)?;
        }
        io::sync_data(&file, &self.kbx, io::RECLAIM)
    }

    /// Append `bytes` to the container outside the writer lock: reserve a range under a short
    /// `flock` on `kaban.kbx.alloc`, then `pwrite` and `fdatasync` ([`kbx::append`]). Nothing
    /// names the bytes until a commit publishes them ([`commit::Txn::publish`]).
    ///
    /// # Errors
    /// The container's I/O errors (`kbx::ContainerError`); `StoreError::StorageFatal` for the
    /// failed sync, attempted once: the range stays unnamed and nothing is published.
    pub(crate) fn append_extent(&self, class: ExtentClass, bytes: &[u8]) -> Result<Unpublished> {
        kbx::append(&self.kbx, class, bytes)
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
