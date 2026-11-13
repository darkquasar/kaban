//! Database file preparation and the live-connection registry of
//! [`SqliteStore`](crate::SqliteStore): owner-only files (core-spec S5) and the POSIX record-lock
//! protection of stores already open in this process (C2.5a/b), the shared-open admission of
//! every writable open (spec 05 SS-23: [`admit_open`]); and the dedicated read connection the
//! code-graph snapshot is attached on ([`CgRead`]; CG-JOIN v1 §2 SO-4, E2.6).

use crate::sql_error::SqlResultExt;
use kaban_core::{DatabaseFileModes, Result, SnapshotAttach, StoreCapabilities, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The dedicated read connection of CG-JOIN v1 §2 (SO-4): a second connection to the memory
/// database, opened read-only, holding at most one `ATTACH DATABASE ? AS cg`. `PRAGMA
/// query_only` is connection-wide and never set; the attached file is read-only through its own
/// `?mode=ro&immutable=1` URI (the parameter came out of `kaban_codegraph::AttachUri`, sealed in
/// [`SnapshotAttach`]'s handle, which only this attach reads) and this connection cannot write the memory database either. The main
/// write connection never attaches anything. The attachment is keyed by the snapshot's commit
/// and file generation (RU3-2): a same-commit republication is another file and re-attaches.
#[derive(Debug)]
pub(crate) struct CgRead {
    conn: Connection,
    /// `(commit, generation)` of the attached snapshot, `None` between attachments.
    attached: Option<(String, String)>,
}

/// A second connection waits for the writer's checkpoint like the first one does.
const READ_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl CgRead {
    /// Open the read connection on the memory database at `path` (`READ_ONLY | URI |
    /// NO_MUTEX`; the URI flag is what lets `ATTACH` take the `file:` form). Nothing is
    /// attached yet. The path is the store's own file, already a regular file this process
    /// holds open, so no mode is tightened and no descriptor other than SQLite's is opened
    /// (C2.5a). Read-only: not an open the shared-open admission (SS-23) covers.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).sql()?;
        conn.busy_timeout(READ_BUSY_TIMEOUT).sql()?;
        Ok(Self {
            conn,
            attached: None,
        })
    }

    /// Attach `attach` as `cg` unless the same commit and generation already is; another
    /// attachment is detached first. Before the `ATTACH`, the file under the admitted path
    /// must still have the generation the handle was admitted at (SS-25): a stale handle, one
    /// presented after a replacement under the same name, is refused with
    /// `SnapshotMismatch`, nothing is attached and the current attachment stands. Nothing is
    /// attached when the `ATTACH` fails either.
    pub(crate) fn ensure(&mut self, attach: &SnapshotAttach) -> Result<()> {
        // RU3-2: commit equality is not file identity; a republished same-commit file has
        // another generation and must be re-attached
        if self.attached.as_ref().is_some_and(|(commit, generation)| {
            *commit == attach.commit && *generation == attach.generation
        }) {
            return Ok(());
        }
        let current = admitted_path(attach.handle.admitted_value())
            .and_then(|path| current_generation(&path));
        if current.as_deref() != Some(attach.generation.as_str()) {
            return Err(StoreError::SnapshotMismatch);
        }
        self.detach()?;
        self.conn
            .execute("ATTACH DATABASE ?1 AS cg", [attach.handle.admitted_value()])
            .sql()?;
        self.attached = Some((attach.commit.clone(), attach.generation.clone()));
        Ok(())
    }

    /// `DETACH` the snapshot when one is attached (the file closes with it; `immutable=1`
    /// holds no lock, so the descriptor goes at once).
    pub(crate) fn detach(&mut self) -> Result<()> {
        if self.attached.is_some() {
            self.conn.execute_batch("DETACH DATABASE cg").sql()?;
            self.attached = None;
        }
        Ok(())
    }

    /// The commit of the attached snapshot, if any.
    pub(crate) fn attached_commit(&self) -> Option<&str> {
        self.attached.as_ref().map(|(commit, _)| commit.as_str())
    }

    /// The connection, for the `cg` queries of `anchors.rs` (the only module that issues
    /// them). Statements over `cg` are prepared uncached so they are finalised before a
    /// `DETACH`.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl Drop for CgRead {
    fn drop(&mut self) {
        // the connection closes right after; the explicit DETACH keeps the order of the
        // contract (detach, then close) and is harmless when it fails
        let _ = self.detach();
    }
}

/// The path an admitted handle names: the `file:<percent-encoded path>?mode=ro&immutable=1`
/// form `kaban_codegraph::AttachUri` renders for this adapter (the one place the opaque
/// handle is read, SS-25); `%XX` decodes to the byte, everything else is the byte itself.
/// `None` when the value is not of that form.
fn admitted_path(handle: &str) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let encoded = handle.strip_prefix("file:")?;
    let encoded = encoded.split_once('?').map_or(encoded, |(path, _)| path);
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// The generation the file at `path` has now, rendered as the admission renders the one it
/// read and `SnapshotAttach::generation` carries: the `Debug` form of
/// `kaban_codegraph::FileGeneration`, `(dev, ino, size, mtime_s, mtime_ns)` of
/// `symlink_metadata` (RU3-2). The two renderings are pinned together by `tests/attach.rs`,
/// which admits through the real `validate_for_attach`. `None` when the path cannot be
/// inspected: a vanished or unreadable file has no generation to match.
fn current_generation(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(format!(
        "FileGeneration {{ dev: {}, ino: {}, size: {}, mtime_s: {}, mtime_ns: {} }}",
        meta.dev(),
        meta.ino(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec()
    ))
}

/// The refusal of an attach on a store without a file (an in-memory store): there is no
/// second connection to open. A fixed class behind `StoreError::Backend`, like every other
/// backend refusal.
#[derive(Debug)]
pub(crate) struct NoFileBacking;

impl std::fmt::Display for NoFileBacking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the store has no database file to open a read connection on")
    }
}

impl std::error::Error for NoFileBacking {}

fn fchmod_owner_only(file: &std::fs::File) -> std::io::Result<()> {
    use rustix::fs::Mode;
    rustix::fs::fchmod(file, Mode::RUSR | Mode::WUSR)?;
    Ok(())
}

/// Identity of a file on this host: `(st_dev, st_ino)`.
type FileIdent = (u64, u64);

/// One database file this process holds: through live [`SqliteStore`](crate::SqliteStore)s,
/// through an [`ExclusiveHold`], or both. The entry goes when the last of them is gone.
#[derive(Debug)]
struct LiveEntry {
    ident: FileIdent,
    /// The stores on the file (0 while only a hold keeps the entry).
    stores: usize,
    /// This process's one descriptor on `<db>.owner` (SS-23, SS-12), taken by whichever came
    /// first, a store's open or a hold, and kept for the entry's lifetime: the sole ownership
    /// of a non-sharing store, or the presence mark of a sharing one, upgraded in place by the
    /// hold. No second descriptor to the file is ever opened or closed while it is held (a
    /// close would release this process's record locks on the file).
    owner: OwnerLock,
    /// An [`ExclusiveHold`] of this process is live on the file (one at a time).
    exclusive: bool,
    /// Of `stores`, how many are openers waiting for another process's exclusive hold
    /// ([`LiftedPresence`]): not using the database. The process's presence mark is lifted
    /// only while every store of the process on the file is such a waiter (T8c review
    /// t8c-2: a waiter never unmarks an admitted store).
    waiting: usize,
}

/// Database files currently held by this process: by a live
/// [`SqliteStore`](crate::SqliteStore) (with a count of the stores on each, C2.5b, P-1) or by
/// an [`ExclusiveHold`]. Protects the stores' POSIX record locks: see
/// [`prepare_database_files`]; and carries the file's ownership: see [`admit_open`].
static LIVE_STORES: Mutex<Vec<LiveEntry>> = Mutex::new(Vec::new());

fn live_stores() -> std::sync::MutexGuard<'static, Vec<LiveEntry>> {
    LIVE_STORES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registration of one store's database file in [`LIVE_STORES`]; removed on drop (after the
/// connection, which is declared before it in [`SqliteStore`](crate::SqliteStore)).
#[derive(Debug)]
pub(crate) struct LiveGuard(FileIdent);

impl LiveGuard {
    /// Register one more store on `ident`; `owner` is the ownership [`admit_open`] handed out
    /// for this open: `None` when the file is this process's own already (its entry carries
    /// the ownership), the lock taken by this open otherwise.
    ///
    /// # Errors
    /// `StoreError::FileMode` when the open was admitted as this process's own and the entry
    /// is gone (cannot happen under the registry lock both run under).
    fn register(
        live: &mut Vec<LiveEntry>,
        ident: FileIdent,
        owner: Option<OwnerLock>,
    ) -> Result<Self> {
        match (live.iter_mut().find(|entry| entry.ident == ident), owner) {
            (Some(entry), _) => entry.stores += 1,
            (None, Some(owner)) => live.push(LiveEntry {
                ident,
                stores: 1,
                owner,
                exclusive: false,
                waiting: 0,
            }),
            (None, None) => {
                return Err(StoreError::FileMode(std::io::Error::other(
                    "the database was admitted as this process's own but is no longer registered",
                )));
            }
        }
        Ok(Self(ident))
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut live = live_stores();
        if let Some(pos) = live.iter().position(|entry| entry.ident == self.0) {
            live[pos].stores -= 1;
            if live[pos].stores == 0 && !live[pos].exclusive {
                // the last store of this process on the file, and no hold: the ownership goes
                // with it (a live hold keeps the entry, and the descriptor, until it is dropped)
                live.swap_remove(pos);
            } else if live[pos].waiting > 0 && live[pos].waiting == live[pos].stores {
                // the last admitted store closed and only waiters remain: their mark goes
                // (best effort; a mark left behind is waited out by the other process)
                let _ = live[pos].lift_mark();
            }
        }
    }
}

/// What a store holds on `<db>.owner` ([`owner_lock_path`]) through one descriptor, kept by
/// the [`LiveEntry`] for as long as the process has a store on the file and dropped by the
/// kernel with the process, so nothing a crash leaves behind, the lock file or a `-shm`
/// sidecar, refuses the next opener:
/// - the sole ownership of a store that cannot share its database between processes (SS-23,
///   `!shared_multiprocess`): an exclusive `flock` ([`Self::take`]);
/// - the presence mark of a sharing store (SS-12): a shared POSIX record lock on the same
///   file ([`Self::join`]), which an exclusive maintenance condition ([`exclusive_hold`]) reads
///   by trying the exclusive lock, so "no other process holds the database" is established by
///   the kernel's lock table and never by IPC.
///
/// Both lock kinds are per open file description or per process and independent of each
/// other. The file is opened exactly once per process (here, through the registry entry that
/// keeps it), so no other descriptor to it is ever opened or closed while a record lock is
/// held (closing one would release the process's record locks on the file).
#[derive(Debug)]
pub(crate) struct OwnerLock {
    /// The locked descriptor, held for its drop.
    file: std::fs::File,
    /// Taken as the sole owner (an exclusive `flock` is held).
    sole: bool,
}

/// How long a sharing open waits for its presence mark while an exclusive maintenance
/// condition of another process holds the file (an erasure: well under this), before it is
/// refused. It is never admitted unmarked: the condition's claim rests on every opener being
/// visible in the lock table (T4 review t4-1).
const PRESENCE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

impl OwnerLock {
    /// Open `<db>.owner` for locking: created owner-only when absent and refused when it is a
    /// symlink or not a regular file, like the database file.
    fn open(path: &Path) -> Result<std::fs::File> {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        let lock = owner_lock_path(path);
        regular_file_metadata(&lock).map_err(StoreError::FileMode)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock)
            .map_err(StoreError::FileMode)
    }

    /// Take the ownership of the database at `path`: `Some` when this process now owns it,
    /// `None` when another process holds the lock.
    fn take(path: &Path) -> Result<Option<Self>> {
        let file = Self::open(path)?;
        Ok(would_block(rustix::fs::flock(
            &file,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        ))?
        .then_some(Self { file, sole: true }))
    }

    /// Mark this process's presence on the database at `path` (a shared record lock), waiting
    /// up to [`PRESENCE_WAIT`] for another process's exclusive maintenance condition to be
    /// released; refused with `ExclusiveMaintenance` when it still holds the file then (a
    /// sharing store is admitted whatever the ownership says, SS-23, but never unmarked).
    fn join(path: &Path) -> Result<Self> {
        let file = Self::open(path)?;
        Self::mark(&file)?;
        Ok(Self { file, sole: false })
    }

    /// The presence mark on the opened lock file `file`, waited for up to [`PRESENCE_WAIT`].
    fn mark(file: &std::fs::File) -> Result<()> {
        let deadline = std::time::Instant::now() + PRESENCE_WAIT;
        loop {
            if would_block(rustix::fs::fcntl_lock(
                file,
                rustix::fs::FlockOperation::NonBlockingLockShared,
            ))? {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(StoreError::ExclusiveMaintenance);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The exclusive record lock in place of the shared one (atomic: a refused upgrade leaves
    /// the shared lock) and, for a sharing store, the exclusive `flock` too, so no other
    /// process marks its presence or owns the file: `true` when both are now held.
    fn upgrade(&self) -> Result<bool> {
        if !would_block(rustix::fs::fcntl_lock(
            &self.file,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        ))? {
            return Ok(false);
        }
        if self.sole
            || would_block(rustix::fs::flock(
                &self.file,
                rustix::fs::FlockOperation::NonBlockingLockExclusive,
            ))?
        {
            return Ok(true);
        }
        self.downgrade()?;
        Ok(false)
    }

    /// Back to what the store holds: the shared record lock, and no `flock` for a sharing
    /// store. Neither can wait: the shared lock conflicts only with an exclusive one, which
    /// this process holds.
    fn downgrade(&self) -> Result<()> {
        rustix::fs::fcntl_lock(&self.file, rustix::fs::FlockOperation::LockShared)
            .map_err(|e| StoreError::FileMode(e.into()))?;
        if !self.sole {
            rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock)
                .map_err(|e| StoreError::FileMode(e.into()))?;
        }
        Ok(())
    }
}

/// `Ok(true)` when the lock was taken, `Ok(false)` when another holder is in the way.
fn would_block(result: rustix::io::Result<()>) -> Result<bool> {
    use rustix::io::Errno;
    match result {
        Ok(()) => Ok(true),
        Err(e) if e == Errno::WOULDBLOCK || e == Errno::AGAIN || e == Errno::ACCESS => Ok(false),
        Err(e) => Err(StoreError::FileMode(e.into())),
    }
}

/// The exclusive maintenance condition on a database file (spec 05 SS-12): for its lifetime no
/// other process holds the database open, established without IPC from the kernel's lock
/// table on `<db>.owner` (every opener of this crate marks its presence there, see
/// [`OwnerLock`]). The hold is registered in [`LIVE_STORES`] like a store and owns the entry's
/// descriptor with it: the last store of this process can close while the hold lives and the
/// lock stays (T4 review t4-6). Dropping it gives the file back to what the process's stores
/// hold, or closes the descriptor when none is left.
#[derive(Debug)]
pub struct ExclusiveHold(FileIdent);

impl Drop for ExclusiveHold {
    fn drop(&mut self) {
        let mut live = live_stores();
        if let Some(pos) = live.iter().position(|entry| entry.ident == self.0) {
            live[pos].exclusive = false;
            if live[pos].stores == 0 {
                // nothing else of this process on the file: the descriptor closes with the
                // entry, which releases every lock on it
                live.swap_remove(pos);
            } else {
                // a downgrade cannot wait (see `OwnerLock::downgrade`); a failure here leaves
                // the exclusive lock with the store, which the next opener waits out
                let _ = live[pos].owner.downgrade();
            }
        }
    }
}

/// Try to establish the [`ExclusiveHold`] on the database at `path`: `None` when another
/// process holds the database open (or owns it) or when this process already holds one;
/// `Some` while this process alone has it. Decided under the registry lock: the entry of a
/// store of this process is upgraded in place, and without one the hold opens the file's one
/// descriptor itself and registers it, so a second attempt finds the entry and never opens a
/// descriptor of its own (closing one would release the first hold's record locks).
///
/// # Errors
/// `StoreError::FileMode` when the database is absent or not a regular file, or when the lock
/// file cannot be opened or locked.
pub fn exclusive_hold(path: &Path) -> Result<Option<ExclusiveHold>> {
    let meta = regular_file_metadata(path)
        .map_err(StoreError::FileMode)?
        .ok_or_else(|| {
            StoreError::FileMode(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", path.display()),
            ))
        })?;
    let ident = file_ident(&meta);
    let mut live = live_stores();
    if let Some(entry) = live.iter_mut().find(|entry| entry.ident == ident) {
        if entry.exclusive || !entry.owner.upgrade()? {
            return Ok(None);
        }
        entry.exclusive = true;
        return Ok(Some(ExclusiveHold(ident)));
    }
    let owner = OwnerLock {
        file: OwnerLock::open(path)?,
        sole: false,
    };
    if !owner.upgrade()? {
        return Ok(None);
    }
    live.push(LiveEntry {
        ident,
        stores: 0,
        owner,
        exclusive: true,
        waiting: 0,
    });
    Ok(Some(ExclusiveHold(ident)))
}

impl LiveEntry {
    /// Whether the entry's mark can move at all: a sharing store's, with no live hold (a sole
    /// owner has no other process to make room for; a hold holds the exclusive lock).
    const fn marks_presence(&self) -> bool {
        !self.exclusive && !self.owner.sole
    }

    /// The process's presence mark released (every store of the process on the file waits).
    fn lift_mark(&self) -> Result<()> {
        if self.marks_presence() {
            rustix::fs::fcntl_lock(&self.owner.file, rustix::fs::FlockOperation::Unlock)
                .map_err(|e| StoreError::FileMode(e.into()))?;
        }
        Ok(())
    }
}

/// One opener's presence lifted while it waits for another process's exclusive hold (an
/// index rebuild, `tantivy.rs`): a waiting opener is not using the database, and with every
/// concurrent opener of one file marked none of them could ever take the hold. Per handle
/// (T8c review t8c-2): the process's one mark is released only while every store of the
/// process on the file is such a waiter, never while an admitted store (a diagnostic open, a
/// serving store) is on it, and it comes back when the waiter goes on ([`Self::restore`]) or
/// gives up (drop), or with this process's own hold (whose release downgrades to it).
#[derive(Debug)]
#[must_use = "the presence comes back when this is restored or dropped"]
pub(crate) struct LiftedPresence(Option<FileIdent>);

/// Lift this opener's presence on the database at `path` (see [`LiftedPresence`]). Nothing is
/// lifted, and the token is inert, for a sole owner or while a hold of this process lives.
///
/// # Errors
/// `StoreError::FileMode` when the database is absent or the lock cannot be released.
pub(crate) fn lift_presence(path: &Path) -> Result<LiftedPresence> {
    let mut live = live_stores();
    let Some(pos) = presence_entry(&live, path)? else {
        return Ok(LiftedPresence(None));
    };
    let entry = &mut live[pos];
    entry.waiting += 1;
    if entry.waiting == entry.stores
        && let Err(e) = entry.lift_mark()
    {
        entry.waiting -= 1;
        return Err(e);
    }
    Ok(LiftedPresence(Some(entry.ident)))
}

impl LiftedPresence {
    /// The waiter's presence back before it goes on, waited for like a join (the other
    /// process's hold may still be releasing).
    ///
    /// # Errors
    /// `StoreError::ExclusiveMaintenance` when another process's hold outlasts the presence
    /// wait; `StoreError::FileMode` when the lock cannot be taken.
    pub(crate) fn restore(mut self) -> Result<()> {
        self.put_back()
    }

    fn put_back(&mut self) -> Result<()> {
        let Some(ident) = self.0.take() else {
            return Ok(());
        };
        let deadline = std::time::Instant::now() + PRESENCE_WAIT;
        loop {
            // the registry lock is held per attempt, not across the wait: a store closing
            // or opening in another thread meanwhile is not held up
            let mut live = live_stores();
            let Some(entry) = live.iter_mut().find(|entry| entry.ident == ident) else {
                return Ok(());
            };
            let unmarked = entry.waiting == entry.stores && entry.marks_presence();
            if !unmarked
                || would_block(rustix::fs::fcntl_lock(
                    &entry.owner.file,
                    rustix::fs::FlockOperation::NonBlockingLockShared,
                ))?
            {
                entry.waiting = entry.waiting.saturating_sub(1);
                return Ok(());
            }
            drop(live);
            if std::time::Instant::now() >= deadline {
                // the waiter leaves its claim: the mark comes back with the next restore or
                // with the last waiter's close
                let mut live = live_stores();
                if let Some(entry) = live.iter_mut().find(|entry| entry.ident == ident) {
                    entry.waiting = entry.waiting.saturating_sub(1);
                }
                return Err(StoreError::ExclusiveMaintenance);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for LiftedPresence {
    fn drop(&mut self) {
        // a waiter that gave up (an error, a timeout) is marked again, best effort
        let _ = self.put_back();
    }
}

/// The registry entry of `path` whose mark [`lift_presence`] moves: a sharing store's without
/// a live hold; `None` when there is nothing to move.
fn presence_entry(live: &[LiveEntry], path: &Path) -> Result<Option<usize>> {
    let meta = regular_file_metadata(path)
        .map_err(StoreError::FileMode)?
        .ok_or_else(|| {
            StoreError::FileMode(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", path.display()),
            ))
        })?;
    let ident = file_ident(&meta);
    Ok(live
        .iter()
        .position(|entry| entry.ident == ident && entry.marks_presence()))
}

/// The owner lock file beside the database, `<db>.owner`.
pub(crate) fn owner_lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".owner");
    PathBuf::from(lock)
}

/// The admission of one store's open of the database file `ident` at `path` (spec 05 SS-23,
/// the capability's second consumer), the one routine every writable open consults
/// ([`prepare_database_files`] and [`open_existing_as_is`]: the ordinary, hook, diagnostic
/// and pack-source opens), decided under the registry lock before anything is opened. A file
/// a store of this process already holds is this process's own: the first opener's admission
/// stands and its ownership sits in the registry entry. Otherwise `capabilities` decide
/// ([`StoreCapabilities::admit_shared_open`]): a sharing store is admitted owning nothing, its
/// presence marked for SS-12's [`exclusive_hold`] (refused, never admitted unmarked, while
/// another process's hold keeps it from marking); a non-sharing store takes the sole ownership
/// or is refused. Returns what to register with the open. Read-only opens ([`CgRead`], a
/// pack's immutable snapshot) are not admitted here: they join no writer set.
///
/// # Errors
/// `StoreError::SharedOpenUnsupported` when another process owns the file and the store cannot
/// share one; `StoreError::ExclusiveMaintenance` when another process's hold outlasts the
/// presence wait; `StoreError::FileMode` when the lock file cannot be opened.
fn admit_open(
    live: &[LiveEntry],
    ident: FileIdent,
    path: &Path,
    capabilities: StoreCapabilities,
) -> Result<Option<OwnerLock>> {
    if live.iter().any(|entry| entry.ident == ident) {
        return Ok(None);
    }
    match capabilities.admit_shared_open(|| OwnerLock::take(path))? {
        Some(owner) => Ok(Some(owner)),
        // a sharing store owns nothing; it marks its presence for SS-12's exclusive
        // maintenance condition
        None => OwnerLock::join(path).map(Some),
    }
}

/// `symlink_metadata` of `path` that refuses anything but a regular file: a symlink (followed
/// or dangling), a directory or a device is never opened or `fchmod`-ed (C2.5b, P-3).
/// `Ok(None)` when the path does not exist.
fn regular_file_metadata(path: &Path) -> std::io::Result<Option<std::fs::Metadata>> {
    use std::io::ErrorKind;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(meta)),
        Ok(_) => Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{} is a symlink or not a regular file; kaban only opens regular database files",
                path.display()
            ),
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn file_ident(meta: &std::fs::Metadata) -> FileIdent {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Make an existing regular file owner-only (0600) with `fchmod` on an opened descriptor (plan
/// C1.6). A file that is already owner-only is not opened at all: closing any descriptor to a
/// database file would release every POSIX record lock this process holds on it (SQLite's
/// WAL-mode SHARED lock on the main file, the wal-index guard on `-shm`), which matters when
/// another store in this process is already open on the same file (C2.5a).
fn tighten_regular_file(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o777 == 0o600 {
        return Ok(());
    }
    let existing = std::fs::OpenOptions::new().read(true).open(path)?;
    fchmod_owner_only(&existing)
}

/// Create the database file at `path` owner-only when it is absent, or tighten a loose existing
/// one, and tighten pre-existing loose `-wal`/`-shm` sidecars (core-spec S5); then open the
/// connection and register it as live. Returns the connection and its registration.
///
/// Every tightening descriptor is opened and closed *before* this process's connection exists:
/// POSIX drops all of a process's record locks on a file when any descriptor to it is closed,
/// and SQLite guards the shared wal-index with such a lock on `-shm` (a process that finds it
/// unlocked truncates `-shm` as the first opener; C2.5a, F15). When another store in this
/// process already holds the same database file open, nothing is tightened at all: the first
/// opener did that, and an opened descriptor would release the live store's locks (C2.5b, P-1).
/// The decision and the open run under the registry lock so two openers in one process cannot
/// interleave. The open is admitted by [`admit_open`] (SS-23) once the file exists and before
/// the connection is opened: a refusal opens nothing.
///
/// Permissions are checked and set by path in sequence, so this relies on a trusted parent
/// directory (not writable by other users): a path that is a symlink or not a regular file is
/// refused, but a replacement regular file placed between the check and SQLite's own open is
/// not detected (C2.5b, P-3; README S5 residual).
pub(crate) fn prepare_database_files(
    path: &Path,
    capabilities: StoreCapabilities,
) -> Result<(Connection, LiveGuard)> {
    use std::fs::OpenOptions;
    use std::io::ErrorKind;
    use std::os::unix::fs::OpenOptionsExt;
    let mut live = live_stores();
    let known_live = |meta: &std::fs::Metadata, live: &[LiveEntry]| {
        live.iter().any(|entry| entry.ident == file_ident(meta))
    };
    let meta = regular_file_metadata(path).map_err(StoreError::FileMode)?;
    let already_live = match meta {
        Some(ref meta) if known_live(meta, &live) => true,
        Some(ref meta) => {
            tighten_regular_file(path, meta).map_err(StoreError::FileMode)?;
            false
        }
        None => {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                // exact mode whatever the umask (P-6); SQLite treats an empty file as a new db
                Ok(created) => fchmod_owner_only(&created).map_err(StoreError::FileMode)?,
                // lost a creation race against another process: it is a regular file now
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if let Some(meta) = regular_file_metadata(path).map_err(StoreError::FileMode)? {
                        tighten_regular_file(path, &meta).map_err(StoreError::FileMode)?;
                    }
                }
                Err(e) => return Err(StoreError::FileMode(e)),
            }
            false
        }
    };
    if !already_live {
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            let sidecar = Path::new(&sidecar);
            if let Some(meta) = regular_file_metadata(sidecar).map_err(StoreError::FileMode)? {
                tighten_regular_file(sidecar, &meta).map_err(StoreError::FileMode)?;
            }
        }
    }
    let ident = regular_file_metadata(path)
        .map_err(StoreError::FileMode)?
        .ok_or_else(|| {
            StoreError::FileMode(std::io::Error::new(
                ErrorKind::NotFound,
                format!("{} vanished before it could be opened", path.display()),
            ))
        })
        .map(|meta| file_ident(&meta))?;
    let owner = admit_open(&live, ident, path, capabilities)?;
    let conn = Connection::open(path).sql()?;
    let guard = LiveGuard::register(&mut live, ident, owner)?;
    drop(live);
    Ok((conn, guard))
}

/// Permission bits of the database file and its `-wal`/`-shm` sidecars, read with
/// `symlink_metadata` before any connection exists (M4-19): the modes an operator sees, not
/// the ones [`prepare_database_files`] would leave behind. A missing or non-regular database
/// file is an error (the diagnostic open refuses it anyway); a missing sidecar is `None`.
///
/// # Errors
/// `StoreError::FileMode` when the database file is absent, a symlink or not a regular file.
pub(crate) fn database_file_modes(path: &Path) -> Result<DatabaseFileModes> {
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;
    let mode_of = |meta: &std::fs::Metadata| meta.permissions().mode() & 0o777;
    let db = regular_file_metadata(path)
        .map_err(StoreError::FileMode)?
        .ok_or_else(|| {
            StoreError::FileMode(std::io::Error::new(
                ErrorKind::NotFound,
                format!("{} does not exist", path.display()),
            ))
        })
        .map(|meta| mode_of(&meta))?;
    let mut sidecars = [None, None];
    for (slot, suffix) in sidecars.iter_mut().zip(["-wal", "-shm"]) {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        *slot = regular_file_metadata(Path::new(&sidecar))
            .map_err(StoreError::FileMode)?
            .map(|meta| mode_of(&meta));
    }
    Ok(DatabaseFileModes {
        db,
        wal: sidecars[0],
        shm: sidecars[1],
    })
}

/// Open an existing database file exactly as it is (M4-19, `doctor`; the hook open, R4-2):
/// nothing is created (the connection is opened without `SQLITE_OPEN_CREATE`), no mode is
/// tightened and no descriptor other than SQLite's own is opened, so the record locks of a
/// store already live in this process are never released (C2.5a). The file must be a regular
/// file, and so must an existing `-wal` or `-shm` sidecar (RL-4): the same `symlink_metadata`
/// refusal [`prepare_database_files`] applies, checked before SQLite opens anything, so a
/// symlinked sidecar is never read as a WAL or written through the link. A missing sidecar is
/// fine (SQLite creates its own). The open is read-write, so it is admitted by [`admit_open`]
/// (SS-23) like the ordinary open, and the connection is registered as live like every other.
///
/// # Errors
/// `StoreError::FileMode` when the file is absent, a symlink or not a regular file, or when an
/// existing sidecar is a symlink or not a regular file; `StoreError::SharedOpenUnsupported`
/// when the open is not admitted; SQLite open errors.
pub(crate) fn open_existing_as_is(
    path: &Path,
    capabilities: StoreCapabilities,
) -> Result<(Connection, LiveGuard)> {
    use rusqlite::OpenFlags;
    let mut live = live_stores();
    let meta = regular_file_metadata(path)
        .map_err(StoreError::FileMode)?
        .ok_or_else(|| {
            StoreError::FileMode(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", path.display()),
            ))
        })?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        // Ok(Some(_)) is an existing regular sidecar, Ok(None) a missing one; both are fine here
        regular_file_metadata(Path::new(&sidecar)).map_err(StoreError::FileMode)?;
    }
    let ident = file_ident(&meta);
    let owner = admit_open(&live, ident, path, capabilities)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags).sql()?;
    let guard = LiveGuard::register(&mut live, ident, owner)?;
    drop(live);
    Ok((conn, guard))
}

#[cfg(test)]
mod shared_open_tests {
    //! The shared-open admission (spec 05 SS-23, the capability's second consumer; E3 review e3-1,
    //! e3-2), exercised through the production open paths with a store that cannot share a
    //! database between processes: it owns the file for its lifetime through an OS-level
    //! exclusive lock on `<db>.owner`, a second process is refused with
    //! `SharedOpenUnsupported`, a leftover sidecar or lock file after a crash refuses nobody
    //! (the kernel drops the lock with the process), and the hook and diagnostic opens consult
    //! the same admission as the ordinary open. The other process is util-linux `flock(1)` on the
    //! lock file, as `kaban-api`'s repository-lock test does.

    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

    use super::owner_lock_path;
    use crate::SqliteStore;
    use kaban_core::{ActorId, StoreCapabilities, StoreError};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// A store that cannot share its database between processes.
    const EXCLUSIVE: StoreCapabilities = StoreCapabilities {
        physical_cleanup: true,
        shared_multiprocess: false,
    };

    /// Private scratch directory (db + keys), removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir()
                .join(format!("kaban-shared-open-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn db(&self) -> PathBuf {
            self.0.join("kaban.db")
        }
        fn keys(&self) -> PathBuf {
            self.0.join("keys")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Whether another process can take the exclusive lock on `lock` right now (a non-blocking
    /// `flock(1)` probe in a child process: the view from outside this process).
    fn free_for_another_process(lock: &Path) -> bool {
        Command::new("flock")
            .args(["-n", "-x"])
            .arg(lock)
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("flock(1) is available")
            .success()
    }

    /// Another process holding the exclusive lock on a file (util-linux `flock`, no fork so the
    /// killed process is the one holding the descriptor), alive until dropped.
    struct Holder(std::process::Child);

    impl Holder {
        fn of(lock: &Path) -> Self {
            let child = Command::new("flock")
                .args(["--no-fork", "-x"])
                .arg(lock)
                .args(["sleep", "120"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("flock(1) is available");
            let t0 = Instant::now();
            while free_for_another_process(lock) {
                assert!(
                    t0.elapsed() < Duration::from_secs(10),
                    "holder never locked"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Self(child)
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn refused(err: &StoreError) -> bool {
        matches!(err, StoreError::SharedOpenUnsupported)
    }

    /// e3-1: the ordinary open of a non-sharing store takes exclusive ownership for the store's
    /// lifetime (another process cannot take the lock until the store is dropped), and while
    /// another process owns the file the open is refused with nothing opened; a sharing store
    /// (this adapter) is admitted whatever the lock says.
    #[test]
    fn a_non_sharing_store_owns_its_database_for_its_lifetime_and_a_second_process_is_refused() {
        let s = Scratch::new("second-process");
        let (db, actor) = (s.db(), ActorId::default());
        let lock = owner_lock_path(&db);

        let first = SqliteStore::open_file(&db, &actor, EXCLUSIVE).unwrap();
        assert!(
            !free_for_another_process(&lock),
            "the open store owns the file: another process cannot take the lock"
        );
        drop(first);
        assert!(
            free_for_another_process(&lock),
            "the lock goes with the store"
        );

        let holder = Holder::of(&lock);
        let err = SqliteStore::open_file(&db, &actor, EXCLUSIVE).unwrap_err();
        assert!(refused(&err), "{err:?}");
        assert!(
            err.to_string().contains("close the other kaban process"),
            "the refusal says what to do: {err}"
        );
        // a sharing store joins the database (mode B): the lock is not its concern
        drop(SqliteStore::open_file(&db, &actor, SqliteStore::CAPABILITIES).unwrap());
        drop(holder);
        // the refusal registered nothing: the next sole opener is admitted and owns the file
        let again = SqliteStore::open_file(&db, &actor, EXCLUSIVE).unwrap();
        assert!(!free_for_another_process(&lock));
        drop(again);
    }

    /// Another process marking its presence on the lock file (a shared record lock, as a
    /// sharing store's open takes), alive until dropped.
    struct Presence(std::process::Child);

    impl Presence {
        fn of(lock: &Path) -> Self {
            let mut child = Command::new("python3")
                .args(["-I", "-c"])
                .arg("import fcntl, sys, time\nf = open(sys.argv[1], 'r+')\nfcntl.lockf(f, fcntl.LOCK_SH)\nprint('held', flush=True)\ntime.sleep(120)")
                .arg(lock)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("python3 is available");
            let mut line = String::new();
            std::io::BufRead::read_line(
                &mut std::io::BufReader::new(child.stdout.take().unwrap()),
                &mut line,
            )
            .unwrap();
            assert_eq!(line.trim(), "held");
            Self(child)
        }
    }

    impl Drop for Presence {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spec 05 SS-12: the exclusive maintenance condition on a database this process has
    /// open is refused while another process marks its presence on the file, established once
    /// that process is gone (the store's own mark is upgraded in place), owns the file against
    /// a sole opener while it lives, and is given back on drop, the store's mark with it.
    #[test]
    fn the_exclusive_hold_reads_the_other_processes_presence_marks() {
        let s = Scratch::new("exclusive-hold");
        let (db, keys) = (s.db(), s.keys());
        let store = SqliteStore::open(&db, &keys).unwrap();
        let lock = owner_lock_path(&db);
        let other = Presence::of(&lock);
        assert!(
            super::exclusive_hold(&db).unwrap().is_none(),
            "another process holds the database"
        );
        drop(other);
        let held = super::exclusive_hold(&db)
            .unwrap()
            .expect("this process alone has the database");
        assert!(
            super::exclusive_hold(&db).unwrap().is_none(),
            "one hold at a time"
        );
        assert!(
            !free_for_another_process(&lock),
            "the hold owns the file against a sole opener"
        );
        drop(held);
        assert!(free_for_another_process(&lock), "given back");
        assert!(super::exclusive_hold(&db).unwrap().is_some());
        drop(store);
        // without a store of this process on the file the hold takes a descriptor of its own
        let own = super::exclusive_hold(&db).unwrap().expect("nobody has it");
        assert!(!free_for_another_process(&lock));
        drop(own);
        assert!(free_for_another_process(&lock));
    }

    /// Whether another process can take a POSIX record lock of `kind` (`LOCK_SH`, the presence
    /// mark; `LOCK_EX`, what an exclusive hold takes) on `lock` right now: a non-blocking
    /// `lockf` probe in a child process, the view from outside this process.
    fn record_lock_free_for_another_process(lock: &Path, kind: &str) -> bool {
        Command::new("python3")
            .args(["-I", "-c"])
            .arg(format!(
                "import fcntl, sys\nf = open(sys.argv[1], 'r+')\nfcntl.lockf(f, fcntl.{kind} | fcntl.LOCK_NB)"
            ))
            .arg(lock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("python3 is available")
            .success()
    }

    /// The opener role of the two-process cases: in a child process (`KABAN_T4_OPENER_DB`
    /// set) open a sharing store on that database through the ordinary open, print `opened`
    /// or `refused: <error>` on stdout, and when opened keep the store until
    /// `<dir>/opener-release` appears (60 s at most). A no-op elsewhere.
    #[test]
    #[ignore = "a role of the two-process cases"]
    fn opener_role() {
        use std::io::Write as _;
        let Ok(db) = std::env::var("KABAN_T4_OPENER_DB") else {
            return;
        };
        let db = PathBuf::from(db);
        let dir = db.parent().unwrap().to_path_buf();
        let mut out = std::io::stdout().lock();
        match SqliteStore::open(&db, &dir.join("keys")) {
            Ok(store) => {
                writeln!(out, "opened").unwrap();
                out.flush().unwrap();
                let deadline = Instant::now() + Duration::from_secs(60);
                while !dir.join("opener-release").exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(store);
            }
            Err(e) => writeln!(out, "refused: {e}").unwrap(),
        }
    }

    /// An opener in another process (the role above), its first line read, released on drop.
    struct Opener(std::process::Child, PathBuf, String);

    impl Opener {
        fn start(s: &Scratch) -> Self {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "connection::shared_open_tests::opener_role",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KABAN_T4_OPENER_DB", s.db())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            // the harness prints its own header lines first; the role's line is the one that
            // starts with its verdict
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                line.clear();
                let read = std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                assert!(read > 0, "the opener ended without a verdict");
                assert!(Instant::now() < deadline, "the opener never answered");
                if line.starts_with("opened") || line.starts_with("refused") {
                    break;
                }
            }
            Self(child, s.0.join("opener-release"), line.trim().to_owned())
        }
        fn verdict(&self) -> &str {
            &self.2
        }
    }

    impl Drop for Opener {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.1, b"");
            let _ = self.0.wait();
            let _ = std::fs::remove_file(&self.1);
        }
    }

    /// Spec 05 SS-12 (T4 review t4-1): while this process holds the exclusive maintenance
    /// condition, an opener in another process is refused with an actionable error, never
    /// admitted without its presence mark; once the hold is released the same opener opens and
    /// is marked (no other process can take the exclusive record lock while it lives).
    #[test]
    fn an_opener_during_the_exclusive_hold_is_refused_and_opens_marked_afterwards() {
        let s = Scratch::new("opener-during-hold");
        let (db, keys) = (s.db(), s.keys());
        let store = SqliteStore::open(&db, &keys).unwrap();
        let lock = owner_lock_path(&db);
        let held = super::exclusive_hold(&db)
            .unwrap()
            .expect("this process alone has the database");
        let refused = Opener::start(&s);
        assert!(
            refused.verdict().starts_with("refused"),
            "an opener during the hold: {}",
            refused.verdict()
        );
        assert!(
            refused.verdict().contains("maintenance"),
            "the refusal says why: {}",
            refused.verdict()
        );
        drop(refused);
        drop(held);
        let opened = Opener::start(&s);
        assert_eq!(opened.verdict(), "opened");
        assert!(
            !record_lock_free_for_another_process(&lock, "LOCK_EX"),
            "the opener is marked: no exclusive record lock for anyone else"
        );
        drop(opened);
        drop(store);
    }

    /// Spec 05 SS-12 (T4 review t4-6): the hold owns the locking descriptor for its lifetime.
    /// The last store of this process can close while the hold lives and another process still
    /// cannot take the file or mark its presence; a second same-process hold attempted while
    /// the first lives (no store on the file by then) is refused and leaves the first intact.
    #[test]
    fn the_exclusive_hold_outlives_the_last_store_and_a_refused_second_hold_leaves_it_intact() {
        let s = Scratch::new("hold-lifetime");
        let (db, keys) = (s.db(), s.keys());
        let store = SqliteStore::open(&db, &keys).unwrap();
        let lock = owner_lock_path(&db);
        let held = super::exclusive_hold(&db)
            .unwrap()
            .expect("this process alone has the database");
        drop(store);
        assert!(
            !free_for_another_process(&lock),
            "the hold keeps the file after the last store closed"
        );
        assert!(
            !record_lock_free_for_another_process(&lock, "LOCK_SH"),
            "no presence mark can be taken while the hold lives"
        );
        assert!(
            super::exclusive_hold(&db).unwrap().is_none(),
            "one hold at a time"
        );
        assert!(
            !record_lock_free_for_another_process(&lock, "LOCK_SH"),
            "the refused second hold left the first one's record lock"
        );
        assert!(!free_for_another_process(&lock));
        drop(held);
        assert!(free_for_another_process(&lock), "given back");
        assert!(record_lock_free_for_another_process(&lock, "LOCK_SH"));
    }

    /// Another process present on the lock file (a shared record lock, as a sharing store's
    /// open takes) that tries the exclusive record lock on command, as its `exclusive_hold`
    /// would: `exclusive` when it got it (and gave it back to shared), `refused` otherwise.
    struct UpgradingPresence(
        std::process::Child,
        std::io::BufReader<std::process::ChildStdout>,
    );

    impl UpgradingPresence {
        fn of(lock: &Path) -> Self {
            let mut child = Command::new("python3")
                .args(["-I", "-c"])
                .arg("import fcntl, sys\nf = open(sys.argv[1], 'r+')\nfcntl.lockf(f, fcntl.LOCK_SH)\nprint('held', flush=True)\nfor _ in sys.stdin:\n    try:\n        fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB)\n        fcntl.lockf(f, fcntl.LOCK_SH)\n        print('exclusive', flush=True)\n    except OSError:\n        print('refused', flush=True)")
                .arg(lock)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("python3 is available");
            let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
            assert_eq!(line.trim(), "held");
            Self(child, reader)
        }

        fn try_exclusive(&mut self) -> String {
            use std::io::Write as _;
            let stdin = self.0.stdin.as_mut().unwrap();
            stdin.write_all(b"go\n").unwrap();
            stdin.flush().unwrap();
            let mut line = String::new();
            std::io::BufRead::read_line(&mut self.1, &mut line).unwrap();
            line.trim().to_owned()
        }
    }

    impl Drop for UpgradingPresence {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// T8c review t8c-2 (SS-12): a waiting opener of this process lifts only its own
    /// presence, never an admitted store's. A holds a store on the database and a second
    /// opener of A waits for the exclusive hold its index rebuild needs (B's presence keeps
    /// it from taking it): B's exclusive admission still fails while A's store is open; once
    /// A's store closes the waiter alone is left and B can take the file; once B is gone the
    /// waiter takes the hold itself, rebuilds and opens.
    #[test]
    fn a_waiting_opener_lifts_only_its_own_presence_never_an_admitted_stores_mark() {
        let s = Scratch::new("waiter-presence");
        let (db, keys) = (s.db(), s.keys());
        let lock = owner_lock_path(&db);
        drop(SqliteStore::open(&db, &keys).unwrap());
        // the index beside the database made unfit: another lineage than the database's,
        // which only a rebuild under the exclusive hold replaces
        {
            let raw = rusqlite::Connection::open(&db).unwrap();
            raw.execute(
                "UPDATE index_meta SET value = 'another-lineage' WHERE key = 'pack_lineage_id'",
                [],
            )
            .unwrap();
        }
        // A's admitted store: the diagnostic open serves the index as it stands
        let (admitted, _modes) =
            SqliteStore::open_diagnostic(&db, &keys, &ActorId::default()).unwrap();
        let mut other = UpgradingPresence::of(&lock);
        let waiter = std::thread::spawn({
            let (db, keys) = (db.clone(), keys.clone());
            move || SqliteStore::open(&db, &keys).map(drop)
        });
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            other.try_exclusive(),
            "refused",
            "A's admitted store is still marked while A's second opener waits"
        );
        drop(admitted);
        let t0 = Instant::now();
        while other.try_exclusive() != "exclusive" {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "the waiter alone left: its mark is lifted"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(other);
        waiter
            .join()
            .unwrap()
            .expect("the waiter takes the hold once B is gone and opens");
        assert!(
            record_lock_free_for_another_process(&lock, "LOCK_EX"),
            "the waiter closed: nothing of this process is left on the file"
        );
    }

    /// e3-1: after a crash the `-shm` sidecar and the lock file are left behind, but no process
    /// holds the lock; the sole opener is admitted.
    #[test]
    fn a_stale_sidecar_and_lock_file_after_a_crash_do_not_refuse_the_sole_opener() {
        let s = Scratch::new("stale-sidecar");
        let (db, actor) = (s.db(), ActorId::default());
        drop(SqliteStore::open_file(&db, &actor, EXCLUSIVE).unwrap());
        // what a crashed process leaves: the wal-index and the lock file, nobody holding either
        let mut shm = db.as_os_str().to_owned();
        shm.push("-shm");
        std::fs::write(&shm, b"").unwrap();
        std::fs::write(owner_lock_path(&db), b"").unwrap();

        let sole = SqliteStore::open_file(&db, &actor, EXCLUSIVE)
            .expect("a stale sidecar is not another process");
        assert!(!free_for_another_process(&owner_lock_path(&db)));
        drop(sole);
    }

    /// e3-2: the hook open and the diagnostic open consult the same admission as the ordinary
    /// open: refused while another process owns the file, admitted (and owning) once it is gone.
    #[test]
    fn the_hook_and_diagnostic_opens_consult_the_same_admission() {
        let s = Scratch::new("hook-diagnostic");
        let (db, keys, actor) = (s.db(), s.keys(), ActorId::default());
        drop(SqliteStore::open(&db, &keys).unwrap());
        let lock = owner_lock_path(&db);

        let holder = Holder::of(&lock);
        let err = SqliteStore::open_for_hook_with(&db, &keys, &actor, EXCLUSIVE).unwrap_err();
        assert!(refused(&err), "hook open: {err:?}");
        let err = SqliteStore::open_diagnostic_with(&db, &keys, &actor, EXCLUSIVE).unwrap_err();
        assert!(refused(&err), "diagnostic open: {err:?}");
        // this adapter shares: both admitted while the lock is held by someone else
        drop(SqliteStore::open_for_hook(&db, &keys, &actor).unwrap());
        drop(SqliteStore::open_diagnostic(&db, &keys, &actor).unwrap());
        drop(holder);

        let hook = SqliteStore::open_for_hook_with(&db, &keys, &actor, EXCLUSIVE).unwrap();
        assert!(
            !free_for_another_process(&lock),
            "the hook store owns the file while open"
        );
        drop(hook);
        let (diagnostic, _modes) =
            SqliteStore::open_diagnostic_with(&db, &keys, &actor, EXCLUSIVE).unwrap();
        assert!(
            !free_for_another_process(&lock),
            "the diagnostic store owns the file while open"
        );
        drop(diagnostic);
        assert!(free_for_another_process(&lock));
    }
}
