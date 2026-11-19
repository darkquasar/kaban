//! The SQLite-local conversion of driver failures into [`StoreError::Backend`] (MT-8).
//!
//! `kaban-core` knows no driver, so there is no `From<rusqlite::Error> for StoreError`; every
//! `?` on a rusqlite result in this crate goes through [`SqlResultExt::sql`] instead. The driver
//! error stays intact behind the [`BackendSource`] holder (operators downcast through
//! `BackendSource::driver`, review RM-1), while every rendering is the content-free one of
//! `StoreError::Backend` (core-spec S1: driver messages may quote SQL text or bound values).
//!
//! One family is not a backend error: SQLite's own failed sync (spec 11 KER-11, END-MED-01).
//! A commit, a checkpoint or any statement that commits syncs through SQLite's VFS, and a
//! failed `fsync` there is [`StoreError::StorageFatal`] like a failed sync of a catalog
//! artefact: never retried, the driver error kept behind the same content-free holder.
//!
//! ```text
//!   rusqlite::Error => extended code in SYNC_FAILURES ? => StorageFatal { op, source }
//!                                                      : => Backend { source }
//! ```

use kaban_core::{BackendSource, Result, StoreError};
use rusqlite::ffi;

/// The operation a failed sync inside SQLite names when the call site names none.
pub(crate) const SQLITE_SYNC: &str = "sqlite_sync";

/// SQLite's extended result codes of a failed sync: `SQLITE_IOERR_FSYNC` (the database, its
/// WAL or a journal could not be synced: a commit at `synchronous=FULL`, a checkpoint) and
/// `SQLITE_IOERR_DIR_FSYNC` (the directory holding a created or deleted journal or WAL could
/// not be synced). After either the kernel may have dropped the dirty pages, so the outcome on
/// disk is unknown and a retry would prove nothing. Read, write, truncate and lock failures
/// are not syncs: they keep their classification.
const SYNC_FAILURES: [std::ffi::c_int; 2] = [ffi::SQLITE_IOERR_FSYNC, ffi::SQLITE_IOERR_DIR_FSYNC];

/// Whether `e` is SQLite's report of a failed sync ([`SYNC_FAILURES`]).
fn sync_failure(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if SYNC_FAILURES.contains(&f.extended_code))
}

/// Wrap a rusqlite failure as [`StoreError::Backend`], keeping it as the source; a failed
/// sync inside SQLite is [`StoreError::StorageFatal`] for [`SQLITE_SYNC`] instead
/// ([`store_error_at`]).
pub(crate) fn backend_error(e: rusqlite::Error) -> StoreError {
    store_error_at(SQLITE_SYNC, e)
}

/// [`backend_error`] for a call that syncs on the store's behalf as the operation `op`: a
/// failed sync is [`StoreError::StorageFatal`] naming `op`, its source the driver error behind
/// the content-free [`BackendSource`]; any other failure is [`StoreError::Backend`].
pub(crate) fn store_error_at(op: &'static str, e: rusqlite::Error) -> StoreError {
    if sync_failure(&e) {
        StoreError::StorageFatal {
            op,
            source: std::io::Error::other(BackendSource::new(e)),
        }
    } else {
        StoreError::Backend {
            source: BackendSource::new(e),
        }
    }
}

/// `?`-friendly conversion for the result of a rusqlite call: `conn.execute(..).sql()?`.
pub(crate) trait SqlResultExt<T> {
    /// Map a driver failure into [`StoreError::Backend`]; `Ok` passes through.
    fn sql(self) -> Result<T>;
    /// [`Self::sql`] for a call that syncs on the store's behalf as the operation `op`
    /// ([`store_error_at`]).
    fn sql_at(self, op: &'static str) -> Result<T>;
}

impl<T> SqlResultExt<T> for rusqlite::Result<T> {
    fn sql(self) -> Result<T> {
        self.map_err(backend_error)
    }
    fn sql_at(self, op: &'static str) -> Result<T> {
        self.map_err(|e| store_error_at(op, e))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;
    use rusqlite::ffi;

    /// A driver failure carrying the extended result code `code`.
    fn failure(code: std::ffi::c_int) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(ffi::Error::new(code), None)
    }

    /// The extended code of the driver error behind a `StorageFatal`'s source.
    fn fatal_code(source: &std::io::Error) -> std::ffi::c_int {
        let held = source
            .get_ref()
            .and_then(|e| e.downcast_ref::<BackendSource>())
            .expect("the source does not hold the driver error");
        match held.driver().downcast_ref::<rusqlite::Error>() {
            Some(rusqlite::Error::SqliteFailure(f, _)) => f.extended_code,
            other => panic!("the held driver error is {other:?}"),
        }
    }

    #[test]
    fn a_sync_failure_inside_sqlite_is_storage_fatal_naming_its_operation() {
        for code in [ffi::SQLITE_IOERR_FSYNC, ffi::SQLITE_IOERR_DIR_FSYNC] {
            for (op, err) in [
                (
                    "catalog_commit",
                    store_error_at("catalog_commit", failure(code)),
                ),
                (SQLITE_SYNC, backend_error(failure(code))),
            ] {
                let StoreError::StorageFatal { op: named, source } = &err else {
                    panic!("code {code} as {op}: expected StorageFatal, got {err:?}");
                };
                assert_eq!(*named, op, "code {code}");
                assert_eq!(fatal_code(source), code, "the driver error was not kept");
                // content-free, as every driver failure's rendering (core-spec S1)
                assert_eq!(
                    source.to_string(),
                    "driver error details withheld",
                    "code {code}"
                );
            }
        }
    }

    #[test]
    fn every_other_driver_failure_keeps_its_backend_classification() {
        for code in [
            ffi::SQLITE_IOERR,
            ffi::SQLITE_IOERR_READ,
            ffi::SQLITE_IOERR_WRITE,
            ffi::SQLITE_IOERR_TRUNCATE,
            ffi::SQLITE_FULL,
            ffi::SQLITE_BUSY,
            ffi::SQLITE_CONSTRAINT_UNIQUE,
        ] {
            for err in [
                store_error_at("catalog_commit", failure(code)),
                backend_error(failure(code)),
            ] {
                assert!(
                    matches!(err, StoreError::Backend { .. }),
                    "code {code}: {err:?}"
                );
            }
        }
        let other = backend_error(rusqlite::Error::QueryReturnedNoRows);
        assert!(matches!(other, StoreError::Backend { .. }), "{other:?}");
    }
}
