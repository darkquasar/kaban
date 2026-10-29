//! The SQLite-local conversion of driver failures into [`StoreError::Backend`] (MT-8).
//!
//! `kaban-core` knows no driver, so there is no `From<rusqlite::Error> for StoreError`; every
//! `?` on a rusqlite result in this crate goes through [`SqlResultExt::sql`] instead. The driver
//! error stays intact behind the [`BackendSource`] holder (operators downcast through
//! `BackendSource::driver`, review RM-1), while every rendering is the content-free one of
//! `StoreError::Backend` (core-spec S1: driver messages may quote SQL text or bound values).

use kaban_core::{BackendSource, Result, StoreError};

/// Wrap a rusqlite failure as [`StoreError::Backend`], keeping it as the source.
pub(crate) fn backend_error(e: rusqlite::Error) -> StoreError {
    StoreError::Backend {
        source: BackendSource::new(e),
    }
}

/// `?`-friendly conversion for the result of a rusqlite call: `conn.execute(..).sql()?`.
pub(crate) trait SqlResultExt<T> {
    /// Map a driver failure into [`StoreError::Backend`]; `Ok` passes through.
    fn sql(self) -> Result<T>;
}

impl<T> SqlResultExt<T> for rusqlite::Result<T> {
    fn sql(self) -> Result<T> {
        self.map_err(backend_error)
    }
}
