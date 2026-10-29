//! The index directory a pack captured, as a `--pack` read opens it (spec 05 SS-19; T6 review
//! t6-2): reads go to the mmap directory underneath; every write, delete and lock file is
//! refused or elided, since nothing may be written into an immutable pack. Tantivy takes its
//! meta lock (a `.tantivy-meta.lock` file) whenever a reader opens the searchable segments, to
//! keep a garbage collection from deleting them meanwhile; nothing collects in a pack, so the
//! lock is a no-op token here and no file appears beside the captured ones.

use std::io;
use std::path::Path;
use std::sync::Arc;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};

/// See the module doc.
#[derive(Clone, Debug)]
pub(crate) struct CapturedDirectory(MmapDirectory);

impl CapturedDirectory {
    /// Open the captured directory at `dir` for reads.
    ///
    /// # Errors
    /// The directory cannot be opened.
    pub(crate) fn open(dir: &Path) -> tantivy::Result<Self> {
        Ok(Self(MmapDirectory::open(dir)?))
    }
}

fn read_only(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::ReadOnlyFilesystem,
        format!("the captured index is immutable: {}", path.display()),
    )
}

impl Directory for CapturedDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.0.get_file_handle(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(read_only(path)),
            filepath: path.to_path_buf(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.0.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(
            read_only(path),
            path.to_path_buf(),
        ))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.0.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(read_only(path))
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, _watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}
