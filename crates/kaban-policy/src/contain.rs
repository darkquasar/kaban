//! The S7 containment walk (engine-spec EN-S7, RA-1; docs-ingest AMD-7; engine-plan E1.3):
//! open a file that must lie below a root directory without ever following a symlink, so a
//! planted link cannot read bytes from outside the root and a directory swapped for a link
//! between a caller's `realpath` and the open is refused instead of followed.
//!
//! The walk: the root is resolved once (`realpath`), opened as a directory with `O_NOFOLLOW`
//! and its `(dev, ino)` recorded; every component of the file's root-relative path is then
//! opened with `openat(dirfd, component, O_NOFOLLOW)` (intermediate components as directories;
//! the final one with `O_NONBLOCK`, so a FIFO with no writer cannot hold the open), the final
//! descriptor must be a regular file by `fstat` before anything else touches it, `O_NONBLOCK`
//! is cleared again on the admitted regular file, and the root is re-checked by `(dev, ino)`
//! after the open. Any symlink at any component, a `..`, an absolute or empty relative path,
//! a path outside the root or a root whose identity changed fails closed with
//! [`ContainError::Escapes`]; a FIFO, socket, device or directory at the final component is
//! [`ContainError::NotRegularFile`]. Messages carry no path.
//!
//! Ported from `kaban-ingest`'s `Candidate::open_checked` (RD-2), which keeps its own copy until
//! DI1 switches to this helper (AMD-7). `rustix` is the project's `O_NOFOLLOW` pin (S5): no
//! `unsafe`, no `libc`.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// Why a contained open failed. Content-free: no path, no file bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ContainError {
    /// A symlink at any component, a path outside the root, a `..`, or a root whose
    /// `(dev, ino)` changed during the walk (TOCTOU swap): nothing was read.
    #[error("the file is a symlink, lies outside the root, or the root changed during the open")]
    Escapes,
    /// The final component is not a regular file.
    #[error("the path is not a regular file")]
    NotRegularFile,
    /// The file is larger than the caller's byte budget: nothing was read.
    #[error("the file exceeds the byte budget")]
    TooLarge,
    /// Another I/O failure, by class only.
    #[error("open failed ({kind:?})")]
    Io {
        /// The `io::ErrorKind`.
        kind: std::io::ErrorKind,
    },
}

impl From<rustix::io::Errno> for ContainError {
    fn from(e: rustix::io::Errno) -> Self {
        match e {
            // `O_NOFOLLOW` on a symlink is ELOOP; `O_DIRECTORY` on a non-directory is ENOTDIR
            Errno::LOOP | Errno::NOTDIR => Self::Escapes,
            other => Self::Io {
                kind: std::io::Error::from(other).kind(),
            },
        }
    }
}

impl From<std::io::Error> for ContainError {
    fn from(e: std::io::Error) -> Self {
        match e.raw_os_error().map(Errno::from_raw_os_error) {
            Some(Errno::LOOP | Errno::NOTDIR) => Self::Escapes,
            _ => Self::Io { kind: e.kind() },
        }
    }
}

/// `(dev, ino)` of an open descriptor or a path.
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

/// The root-relative components of `path`: `path` (made absolute lexically, never through the
/// filesystem) must start with `root` or with its `realpath` `real_root`; every component must
/// be a plain name (no `..`, `.`, prefix or root component).
fn relative_components(root: &Path, real_root: &Path, path: &Path) -> Option<Vec<PathBuf>> {
    let abs = std::path::absolute(path).ok()?;
    let rel = abs
        .strip_prefix(real_root)
        .or_else(|_| abs.strip_prefix(root))
        .ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(name) => parts.push(PathBuf::from(name)),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() { None } else { Some(parts) }
}

/// Open `path`, which must be a regular file below `root`, following no symlink at any
/// component (see the module doc).
///
/// # Errors
/// [`ContainError::Escapes`] for a symlink at any component, a path outside the root or a root
/// identity change; [`ContainError::NotRegularFile`]; [`ContainError::Io`] otherwise.
pub fn open_contained(root: &Path, path: &Path) -> Result<File, ContainError> {
    let walk = Walk::start(root)?;
    walk.open(path)
}

/// The flags of every directory open of the walk.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A contained walk: the root resolved once and held open with its identity recorded. The two
/// steps are separate so the root re-check can be exercised on its own (a root swapped between
/// [`Walk::start`] and [`Walk::open`] fails the open).
struct Walk {
    root: PathBuf,
    real_root: PathBuf,
    root_dir: File,
    root_identity: (u64, u64),
}

impl Walk {
    /// Resolve and open the root, record its `(dev, ino)`.
    fn start(root: &Path) -> Result<Self, ContainError> {
        let real_root = std::fs::canonicalize(root)?;
        let root_dir = File::from(rustix::fs::open(&real_root, DIR_FLAGS, Mode::empty())?);
        let root_identity = identity(&root_dir.metadata()?);
        if root_identity != identity(&std::fs::metadata(&real_root)?) {
            return Err(ContainError::Escapes);
        }
        Ok(Self {
            root: root.to_path_buf(),
            real_root,
            root_dir,
            root_identity,
        })
    }

    /// Walk `path` below the root component by component and admit the final descriptor.
    fn open(&self, path: &Path) -> Result<File, ContainError> {
        let parts =
            relative_components(&self.root, &self.real_root, path).ok_or(ContainError::Escapes)?;
        let (last, dirs) = parts.split_last().ok_or(ContainError::Escapes)?;
        let mut dir = self.root_dir.try_clone()?;
        for component in dirs {
            dir = File::from(rustix::fs::openat(
                &dir,
                component,
                DIR_FLAGS,
                Mode::empty(),
            )?);
        }
        // the final component: non-blocking, so a FIFO with no writer returns at once instead
        // of holding the open; admitted by `fstat` before anything else touches it
        let file = File::from(rustix::fs::openat(
            &dir,
            last,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(ContainError::NotRegularFile);
        }
        // a regular file never blocks on read; the flag is cleared so the handle behaves as a
        // plain blocking descriptor for every caller
        let flags = rustix::fs::fcntl_getfl(&file)?;
        rustix::fs::fcntl_setfl(&file, flags.difference(OFlags::NONBLOCK))?;
        // the root re-check after the open: the directory the walk started from must still be
        // the root the caller named
        if self.root_identity != identity(&std::fs::metadata(&self.real_root)?) {
            return Err(ContainError::Escapes);
        }
        Ok(file)
    }
}

/// [`open_contained`] and read at most `max_bytes` bytes; a file whose size (from the opened
/// descriptor) exceeds the budget is refused before a byte is read.
///
/// # Errors
/// As [`open_contained`], plus [`ContainError::TooLarge`].
pub fn read_contained(root: &Path, path: &Path, max_bytes: u64) -> Result<Vec<u8>, ContainError> {
    let file = open_contained(root, path)?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        return Err(ContainError::TooLarge);
    }
    let mut buf = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)?;
    if u64::try_from(buf.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(ContainError::TooLarge);
    }
    Ok(buf)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    /// A private scratch tree removed on drop (`test-fixture-raii`).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("kaban-contain-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SECRET: &str = "OUTSIDE-CANARY-3c9e";

    #[test]
    fn a_regular_file_below_the_root_is_read_within_its_budget() {
        let s = Scratch::new("ok");
        let root = s.0.join("root");
        std::fs::create_dir_all(root.join(".kaban/ontology")).unwrap();
        std::fs::write(root.join(".kaban/ontology/x.yaml"), b"ontology: x\n").unwrap();

        let bytes = read_contained(&root, &root.join(".kaban/ontology/x.yaml"), 1024).unwrap();
        assert_eq!(bytes, b"ontology: x\n");
        assert_eq!(
            read_contained(&root, &root.join(".kaban/ontology/x.yaml"), 5),
            Err(ContainError::TooLarge)
        );
        assert_eq!(
            open_contained(&root, &root.join(".kaban/ontology")).unwrap_err(),
            ContainError::NotRegularFile
        );
    }

    /// EN-S7: a symlink escaping the root at any component (an intermediate directory, the
    /// final file) fails closed; a `..` or a path outside the root never opens anything.
    #[test]
    fn a_symlink_escaping_the_root_at_any_component_fails_closed() {
        let s = Scratch::new("escape");
        let root = s.0.join("root");
        let outside = s.0.join("outside");
        std::fs::create_dir_all(root.join(".kaban")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.yaml"), SECRET).unwrap();
        // intermediate component is a link out of the root
        std::os::unix::fs::symlink(&outside, root.join(".kaban/ontology")).unwrap();
        // final component is a link out of the root
        std::os::unix::fs::symlink(outside.join("x.yaml"), root.join(".kaban/leaf.yaml")).unwrap();

        for path in [
            root.join(".kaban/ontology/x.yaml"),
            root.join(".kaban/leaf.yaml"),
            root.join(".kaban/../outside/x.yaml"),
            outside.join("x.yaml"),
        ] {
            let err = read_contained(&root, &path, 1024).unwrap_err();
            assert_eq!(err, ContainError::Escapes, "{}", path.display());
            assert!(!err.to_string().contains(SECRET));
        }
    }

    /// EN-S7 TOCTOU (RP-10): a caller resolved `realpath` of `.kaban/ontology/x.yaml` while it
    /// was a real file; the `ontology` directory is then swapped for a symlink to a directory
    /// outside the root holding another `x.yaml`. The open after the swap must refuse the link
    /// and read nothing.
    #[test]
    fn a_directory_swapped_for_a_symlink_after_realpath_is_refused_by_the_walk() {
        let s = Scratch::new("swap");
        let root = s.0.join("root");
        let outside = s.0.join("outside");
        std::fs::create_dir_all(root.join(".kaban/ontology")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join(".kaban/ontology/x.yaml"), b"ontology: x\n").unwrap();
        std::fs::write(outside.join("x.yaml"), SECRET).unwrap();
        let resolved = std::fs::canonicalize(root.join(".kaban/ontology/x.yaml")).unwrap();
        assert!(
            read_contained(&root, &resolved, 1024).is_ok(),
            "before the swap"
        );

        // the swap, between the caller's realpath and the open
        std::fs::remove_dir_all(root.join(".kaban/ontology")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".kaban/ontology")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            SECRET,
            "a naive open would now read the outside file"
        );

        assert_eq!(
            read_contained(&root, &resolved, 1024),
            Err(ContainError::Escapes)
        );
    }

    /// EN-S7 (E1.3a RE1-2): a FIFO with no writer at the final component is refused as not a
    /// regular file at once; a blocking open would wait for a writer forever. The call runs on
    /// its own thread under a 2 s bound so a regression is a failure, not a hang.
    #[test]
    fn a_fifo_at_the_final_component_is_refused_without_blocking() {
        let s = Scratch::new("fifo");
        let root = s.0.join("root");
        std::fs::create_dir_all(root.join(".kaban/ontology")).unwrap();
        let fifo = root.join(".kaban/ontology/blocked.yaml");
        #[cfg(not(target_os = "macos"))]
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        #[cfg(target_os = "macos")]
        {
            // Darwin's rustix backend has no mkfifoat. Create the same FIFO fixture;
            // the nonblocking containment assertion below remains unchanged.
            assert!(
                std::process::Command::new("/usr/bin/mkfifo")
                    .arg("-m")
                    .arg("600")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let root_for_thread = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(open_contained(&root_for_thread, &fifo).map(|_| ()));
        });

        let result = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the open returned within 2 s (a FIFO must not block the walk)");
        assert_eq!(result, Err(ContainError::NotRegularFile));
    }

    /// EN-S7 (E1.3a, the reviewer's root-identity case): the root's identity changes after the
    /// root descriptor is opened (the directory is renamed away and another, holding the same
    /// relative file, put in its place). Every component of the walk below the held descriptor
    /// still opens, so only the final `(dev, ino)` re-check can refuse the load, and it must.
    #[test]
    fn a_root_swapped_after_the_root_descriptor_opened_fails_the_identity_re_check() {
        let s = Scratch::new("identity");
        let root = s.0.join("root");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/f.yaml"), b"ok").unwrap();
        let walk = Walk::start(&root).unwrap();
        assert!(walk.open(&root.join("d/f.yaml")).is_ok(), "before the swap");

        // the swap: the opened root moves away, a fresh directory takes its name
        std::fs::rename(&root, s.0.join("moved")).unwrap();
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/f.yaml"), SECRET).unwrap();
        assert_ne!(
            identity(&std::fs::metadata(&root).unwrap()),
            walk.root_identity,
            "the name now resolves to another directory"
        );

        assert_eq!(
            walk.open(&root.join("d/f.yaml")).map(|_| ()),
            Err(ContainError::Escapes)
        );
    }

    /// A root reached through a link is resolved once and walked below; a cwd-relative path
    /// that does not lie below it is refused.
    #[test]
    fn a_root_reached_through_a_link_resolves_once_and_a_path_outside_it_is_refused() {
        let s = Scratch::new("root");
        let real = s.0.join("real");
        std::fs::create_dir_all(real.join("d")).unwrap();
        std::fs::write(real.join("d/f.yaml"), b"ok").unwrap();
        // a root reached through a link is fine: it is resolved once, and the walk is below it
        std::os::unix::fs::symlink(&real, s.0.join("link-root")).unwrap();
        let via_link = s.0.join("link-root");
        assert_eq!(
            read_contained(&via_link, &via_link.join("d/f.yaml"), 16).unwrap(),
            b"ok"
        );
        // but a relative path that climbs out of it is not
        assert_eq!(
            read_contained(&via_link, Path::new("d/f.yaml"), 16),
            Err(ContainError::Escapes),
            "a cwd-relative path outside the root"
        );
    }
}
