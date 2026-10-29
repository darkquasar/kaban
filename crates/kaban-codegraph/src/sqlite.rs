//! Read-only snapshot admission (stage1-spec §4.1 reader rules, §7; stage1-plan CG2.2;
//! cg-join-v1 §2): [`SqliteSnapshot::open_ro`] for this crate's reader and
//! [`validate_for_attach`] for the engine's `ATTACH`, both running the same checks in the same
//! order:
//!
//! ```text
//!   symlink_metadata regular  ==> mode group|other == 0  ==> final name <commit40>.cg.db
//!   ==> open ?mode=ro (O_NOFOLLOW)  ==> meta.schema_version major <= SCHEMA_VERSION
//!   ==> PRAGMA quick_check once per process per (dev, ino, size, mtime)  ==> admitted
//! ```
//!
//! **Trusted-parent assumption (the one place it applies: [`admit`]).** The checks run by path
//! in sequence; SQLite opens the path itself (`SQLITE_OPEN_NOFOLLOW`, so a symbolic link swapped
//! in at the final component between `symlink_metadata` and the kernel's `open` is refused by
//! the kernel, not only by the earlier check). A regular file *replaced* in that window is not
//! detected: the opened handle cannot be `fstat`ed against the inspected inode without `unsafe`
//! (rusqlite exposes no descriptor and SQLite's VFS resolves a `/proc/self/fd/<n>` path back to
//! its target), so admission relies on the snapshot directory being trusted: `0700`, owned by
//! the invoking user, written only by `kaban-index` through a `tmp-<pid>` rename, never inside a
//! checkout (stage1-spec §4.1, core-plan C1.6). The same assumption covers the engine's `ATTACH`
//! by [`AttachUri`], which is by path by construction. The `quick_check` memo is keyed by the
//! file *generation* `(dev, ino, size, mtime_ns)` (cg-join-v1 §2 as clarified at `GM2a`, RG2-9):
//! final-name snapshots are published once and never rewritten, so a changed `mtime` with the
//! same size is a different file and is checked again, whichever verdict the previous one had.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, OpenFlags};

use crate::error::SnapshotError;
use crate::limits::Limits;
use crate::schema::SCHEMA_VERSION;

/// The snapshot file suffix after the 40-hex commit.
pub const FINAL_SUFFIX: &str = ".cg.db";

/// An open, admitted, read-only snapshot. Dropping it closes the connection. The connection sits
/// behind a `Mutex` so that an `Arc<SqliteSnapshot>` is `Send + Sync` for the provider of
/// cg-join-v1 §4 (`rusqlite::Connection` is `Send` only); every query locks it for its duration.
#[derive(Debug)]
pub struct SqliteSnapshot {
    conn: Mutex<Connection>,
    path: PathBuf,
    schema_version: u32,
    generation: FileGeneration,
}

impl SqliteSnapshot {
    /// Open `path` read-only after the admission checks of the module doc; the connection's
    /// page cache is bounded (`cache_size = -2000`, G1-N6).
    ///
    /// # Errors
    /// [`SnapshotError`]: the first failed check, as its class.
    pub fn open_ro(path: &Path) -> Result<Self, SnapshotError> {
        let (conn, schema_version, generation) = admit(path)?;
        conn.execute_batch("PRAGMA cache_size = -2000;")
            .map_err(|_| SnapshotError::Corrupt)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            schema_version,
            generation,
        })
    }

    /// The path the snapshot was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `meta.schema_version` major as read at open.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// The file generation admission inspected for this open (RU3-2): what the cache compares
    /// the path against before reusing this connection, and what the attach of the same
    /// `Resolved` must equal.
    pub(crate) fn generation(&self) -> FileGeneration {
        self.generation
    }

    /// The traversal bounds this reader applies (the fixed spec values, G1-F12).
    #[must_use]
    pub fn limits(&self) -> &'static Limits {
        &Limits::DEFAULT
    }

    /// Run `f` on the read-only connection (the query modules' one entry point).
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&conn)
    }
}

/// The admitted `file:` URI the engine attaches (cg-join-v1 §2): constructible only through
/// [`validate_for_attach`], no `Deref` to `&str`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachUri {
    uri: String,
    path: PathBuf,
    schema_version: u32,
    generation: FileGeneration,
}

impl AttachUri {
    /// The `ATTACH DATABASE ?` parameter: `file:<encoded>?mode=ro&immutable=1`.
    #[must_use]
    pub fn as_sql_param(&self) -> &str {
        &self.uri
    }

    /// The validated path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `meta.schema_version` major as read during validation.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// The file generation admission inspected during validation (RU3-2): the provider
    /// requires it to equal the leased snapshot's before both go into one `Resolved`, and the
    /// engine's `SnapshotAttach` carries it (opaquely, as its `Debug` rendering) so the store
    /// re-attaches a same-commit republication (engine E2.6, cg-join-v1 §2).
    #[must_use]
    pub fn generation(&self) -> FileGeneration {
        self.generation
    }
}

/// Run the admission checks on `path` and build its [`AttachUri`]; the validating connection is
/// closed before returning.
///
/// # Errors
/// [`SnapshotError`]: the first failed check, as its class.
pub fn validate_for_attach(path: &Path) -> Result<AttachUri, SnapshotError> {
    let (conn, schema_version, generation) = admit(path)?;
    drop(conn);
    Ok(AttachUri {
        uri: format!("file:{}?mode=ro&immutable=1", percent_encode(path)),
        path: path.to_path_buf(),
        schema_version,
        generation,
    })
}

/// Whether `name` is a final snapshot name `<commit40>.cg.db` (40 lowercase hex).
#[must_use]
pub fn is_final_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(commit) = name.strip_suffix(FINAL_SUFFIX) else {
        return false;
    };
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The checks of the module doc, in order; returns the read-only connection, the schema major
/// and the file generation the checks inspected.
fn admit(path: &Path) -> Result<(Connection, u32, FileGeneration), SnapshotError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| io_class(&e))?;
    if meta.file_type().is_symlink() {
        return Err(SnapshotError::SymlinkRefused);
    }
    if !meta.is_file() {
        return Err(SnapshotError::NotRegularFile);
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(SnapshotError::ModeRefused);
    }
    if !path.file_name().is_some_and(is_final_name) {
        return Err(SnapshotError::TempNameRefused);
    }
    let uri = format!("file:{}?mode=ro", percent_encode(path));
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn =
        Connection::open_with_flags(uri, flags).map_err(|_| SnapshotError::Io { class: "open" })?;
    let major = schema_major(&conn)?;
    if major > SCHEMA_VERSION {
        return Err(SnapshotError::SnapshotTooNew {
            major,
            supported: SCHEMA_VERSION,
        });
    }
    let generation = FileGeneration::of(&meta);
    quick_check_once(&conn, generation)?;
    Ok((conn, major, generation))
}

/// `meta.schema_version` as `major` or `major.minor`, both unsigned decimal; anything else
/// (`1.invalid`, `1.2.3`, an empty part, a sign, whitespace) is `Corrupt`.
fn schema_major(conn: &Connection) -> Result<u32, SnapshotError> {
    let value: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| SnapshotError::Corrupt)?;
    parse_schema_version(&value).ok_or(SnapshotError::Corrupt)
}

/// The major of a well-formed `major[.minor]`; `None` otherwise.
fn parse_schema_version(value: &str) -> Option<u32> {
    let (major, minor) = match value.split_once('.') {
        Some((major, minor)) => (major, Some(minor)),
        None => (value, None),
    };
    let decimal = |part: &str| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    let major = decimal(major)?;
    match minor {
        Some(minor) => decimal(minor).map(|_| major),
        None => Some(major),
    }
}

/// The file generation the `quick_check` memo is keyed by (RG2-9): the inode identity plus the
/// size and the modification time, so a same-size rewrite or a replacement under a reused inode
/// is a different key. Also the identity of an admitted open (RU3-2): the cache reuses a
/// connection only while the path still has the generation it was admitted at, and one
/// `Resolved` carries one generation for its lease and its attach. Opaque outside this crate
/// (private fields; compared for equality, rendered through `Debug`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileGeneration {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_s: i64,
    mtime_ns: i64,
}

impl FileGeneration {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_s: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
        }
    }

    /// The generation `path` has now (`symlink_metadata`, never a follow).
    ///
    /// # Errors
    /// [`SnapshotError::Io`] by class when the path cannot be inspected.
    pub(crate) fn current(path: &Path) -> Result<Self, SnapshotError> {
        std::fs::symlink_metadata(path)
            .map(|meta| Self::of(&meta))
            .map_err(|e| io_class(&e))
    }
}

/// The process-wide memo of `quick_check` verdicts by file generation.
fn checked() -> &'static Mutex<HashMap<FileGeneration, bool>> {
    static CHECKED: OnceLock<Mutex<HashMap<FileGeneration, bool>>> = OnceLock::new();
    CHECKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `PRAGMA quick_check` once per process per file generation (stage1-spec §4.1 "on first
/// open"); a remembered failure stays `Corrupt` without a second scan until the file changes.
fn quick_check_once(conn: &Connection, identity: FileGeneration) -> Result<(), SnapshotError> {
    let mut memo = checked()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ok = if let Some(&verdict) = memo.get(&identity) {
        verdict
    } else {
        #[cfg(any(test, feature = "instrumented"))]
        testing::count_quick_check();
        let verdict = run_quick_check(conn);
        memo.insert(identity, verdict);
        verdict
    };
    if ok {
        Ok(())
    } else {
        Err(SnapshotError::Corrupt)
    }
}

fn run_quick_check(conn: &Connection) -> bool {
    conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .is_ok_and(|verdict| verdict == "ok")
}

fn io_class(e: &std::io::Error) -> SnapshotError {
    use std::io::ErrorKind;
    SnapshotError::Io {
        class: match e.kind() {
            ErrorKind::NotFound => "not_found",
            ErrorKind::PermissionDenied => "permission_denied",
            _ => "other",
        },
    }
}

/// Percent-encode every byte outside the URI unreserved set (`A-Z a-z 0-9 - . _ ~`), keeping
/// `/`: `?`, `#`, `%`, space, control and non-ASCII bytes are all encoded (cg-join-v1 §2).
fn percent_encode(path: &Path) -> String {
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Test-only observation (`quick_check_once_per_process`).
#[cfg(any(test, feature = "instrumented"))]
pub mod testing {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static QUICK_CHECKS: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn count_quick_check() {
        QUICK_CHECKS.fetch_add(1, Ordering::SeqCst);
    }

    /// How many `PRAGMA quick_check` statements this process has run so far.
    #[must_use]
    pub fn quick_checks_run() -> usize {
        QUICK_CHECKS.load(Ordering::SeqCst)
    }
}
