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
//! T5 Step 1 adds the signatures with empty bodies; Step 3 implements and routes the writers.

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
use std::sync::Arc;

use kaban_core::Result;
use rusqlite::Connection;

use class::{ExtentClass, ExtentLocation, IndexClass};
use kbx::Unpublished;

/// The catalog of one database: its container, its reclamation mode and its index classes.
pub(crate) struct Catalog {
    kbx: PathBuf,
    degraded_reclamation: bool,
    classes: Vec<Arc<dyn IndexClass>>,
}

impl Catalog {
    /// Create or open `kaban.kbx` beside `db_path`, probe hole punching (KER-11: without it,
    /// zero-fill mode and [`Self::degraded_reclamation`]) and open `kaban.kbx.pins/`.
    ///
    /// # Errors
    /// I/O and SQLite errors (T5 Step 3).
    pub(crate) fn open(db_path: &Path, conn: &Connection) -> Result<Self> {
        let _ = conn;
        Ok(Self {
            kbx: kbx::kbx_path(db_path),
            degraded_reclamation: false,
            classes: Vec::new(),
        })
    }

    /// Add an index class; every later commit stages it.
    pub(crate) fn register(&mut self, class: Arc<dyn IndexClass>) {
        self.classes.push(class);
    }

    /// Whether reclamation zero-fills because hole punching is unavailable (doctor reads it).
    pub(crate) fn degraded_reclamation(&self) -> bool {
        self.degraded_reclamation
    }

    /// Append `bytes` to the container outside the writer lock: reserve a range under a short
    /// `flock` on `kaban.kbx.alloc`, then `pwrite` and `fdatasync`.
    ///
    /// # Errors
    /// I/O errors (T5 Step 3).
    pub(crate) fn append_extent(&self, class: ExtentClass, bytes: &[u8]) -> Result<Unpublished> {
        let _ = bytes;
        Ok(Unpublished {
            class,
            location: ExtentLocation::Kbx { offset: 0, len: 0 },
        })
    }
}
