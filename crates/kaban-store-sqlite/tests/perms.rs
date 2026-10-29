//! File permissions and purge hygiene (core-spec S5; core-plan C1.6): the database, its WAL and
//! SHM sidecars and the audit key are owner-only (0600), pre-existing loose modes are tightened on
//! open, and a purged note leaves no bytes behind in the database files.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{Actor, ActorId, MemoryStore, NewNode, WorkspaceId, WriteCtx};
use kaban_store_sqlite::SqliteStore;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn ctx() -> WriteCtx {
    WriteCtx::new("v1", WorkspaceId::default(), Actor::Agent)
}

fn note(text: &str) -> NewNode {
    NewNode {
        kind: "Fact".into(),
        text: text.into(),
        ..Default::default()
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[test]
fn freshly_created_database_wal_shm_and_key_are_owner_only() {
    let s = Scratch::new("kaban-perms", "fresh");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    store
        .remember(note("first note creates the wal"), &[], &ctx())
        .unwrap();
    let wal = sidecar(&s.db(), "-wal");
    let shm = sidecar(&s.db(), "-shm");
    assert!(
        wal.exists() && shm.exists(),
        "WAL mode sidecars expected after a write"
    );
    assert_eq!(mode(&s.db()), 0o600, "database file mode");
    assert_eq!(mode(&wal), 0o600, "-wal file mode");
    assert_eq!(mode(&shm), 0o600, "-shm file mode");
    let key = std::fs::read_dir(s.keys())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(mode(&key), 0o600, "audit key file mode");
}

#[test]
fn pre_existing_loose_database_and_sidecars_are_tightened_on_open() {
    let s = Scratch::new("kaban-perms", "loose");
    {
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        store
            .remember(note("note before the loosening"), &[], &ctx())
            .unwrap();
    }
    // another process keeps the database open, so the sidecars persist with their loose modes
    let other_process = rusqlite::Connection::open(s.db()).unwrap();
    other_process
        .execute_batch("PRAGMA journal_mode=WAL; SELECT count(*) FROM node;")
        .unwrap();
    let wal = sidecar(&s.db(), "-wal");
    let shm = sidecar(&s.db(), "-shm");
    for f in [&s.db(), &wal, &shm] {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode(f), 0o644, "fixture loosened");
    }
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(mode(&s.db()), 0o600, "database tightened on open");
    assert_eq!(mode(&wal), 0o600, "-wal tightened on open");
    assert_eq!(mode(&shm), 0o600, "-shm tightened on open");
    // and the data is still there
    let hits = store
        .recall(&kaban_core::RecallQuery {
            query: "loosening".into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1, "existing note still recalled");
    drop(other_process);
}

/// Every byte of the database files, lower-cased so FTS5's case-folded tokens count too. Read
/// by a helper process (`cat`): opening and closing the files in this process while the store
/// is live would release the store's POSIX record locks on them (C2.5a; review note).
fn db_bytes_lower(db: &Path) -> Vec<u8> {
    let out = std::process::Command::new("cat")
        .arg(db)
        .arg(sidecar(db, "-wal"))
        .arg(sidecar(db, "-shm"))
        .stderr(std::process::Stdio::null()) // a missing sidecar is not an error here
        .output()
        .expect("cat is available");
    out.stdout.to_ascii_lowercase()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn purged_note_leaves_no_bytes_in_db_wal_or_shm() {
    let s = Scratch::new("kaban-perms", "purge");
    let canary = "PurgeCanaryZq8vK3";
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let id = match store
        .remember(
            note(&format!("rotate {canary} before release")),
            &[],
            &ctx(),
        )
        .unwrap()
    {
        kaban_core::RememberOutcome::Created(n) => n.id,
        kaban_core::RememberOutcome::Existing { .. } => panic!("fresh database"),
    };
    // a few more writes so the WAL carries frames after the canary's page
    for i in 0..5 {
        store
            .remember(note(&format!("filler note {i}")), &[], &ctx())
            .unwrap();
    }
    assert!(
        contains(
            &db_bytes_lower(&s.db()),
            canary.to_ascii_lowercase().as_bytes()
        ),
        "fixture: canary is on disk before the purge"
    );
    store
        .forget(
            &kaban_core::NodeId::parse(&id).unwrap(),
            kaban_core::ForgetMode::Purge,
            &ctx(),
        )
        .unwrap();
    assert!(
        !contains(
            &db_bytes_lower(&s.db()),
            canary.to_ascii_lowercase().as_bytes()
        ),
        "canary still present in db/-wal/-shm bytes after purge"
    );
}

/// One POSIX record lock as the Linux `/proc/locks` table reports it.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
struct PosixLock {
    /// `READ` or `WRITE`.
    kind: String,
    start: u64,
    /// `u64::MAX` for `EOF`.
    end: u64,
}

/// POSIX record locks held by this process on `path`, as the Linux `/proc/locks` table reports
/// them (`<n>: POSIX ADVISORY <READ|WRITE> <pid> <maj>:<min>:<inode> <start> <end|EOF>`): the
/// kernel's view of what SQLite's unix VFS believes it holds. The file is matched on its full
/// `major:minor:inode` identity, so an unrelated file with the same inode number on another
/// filesystem cannot produce a false positive (C2.5b, P-4).
#[cfg(target_os = "linux")]
fn own_posix_locks_on(path: &Path) -> Vec<PosixLock> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    let dev = meta.dev();
    // glibc `gnu_dev_major` / `gnu_dev_minor` split of `st_dev`, printed as `/proc/locks` does
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let ident = format!("{major:02x}:{minor:02x}:{}", meta.ino());
    let pid = std::process::id().to_string();
    let table = std::fs::read_to_string("/proc/locks").expect("/proc/locks is readable");
    table
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            (f.len() >= 8 && f[1] == "POSIX" && f[4] == pid && f[5] == ident).then(|| PosixLock {
                kind: f[3].to_owned(),
                start: f[6].parse().unwrap(),
                end: if f[7] == "EOF" {
                    u64::MAX
                } else {
                    f[7].parse().unwrap()
                },
            })
        })
        .collect()
}

/// SQLite's wal-index guard: a `READ` lock covering byte 128 of `-shm` (the "DMS" byte that a
/// later opener probes before deciding it is the first one and truncating the file).
#[cfg(target_os = "linux")]
fn holds_shm_dms_read_lock(shm: &Path) -> bool {
    own_posix_locks_on(shm)
        .iter()
        .any(|l| l.kind == "READ" && l.start <= 128 && l.end >= 128)
}

/// SQLite's WAL-mode SHARED lock on the main file (a `READ` lock in the reserved byte range).
#[cfg(target_os = "linux")]
fn holds_main_file_read_lock(db: &Path) -> bool {
    own_posix_locks_on(db).iter().any(|l| l.kind == "READ")
}

/// C2.5a (core-spec F15, S5): SQLite's unix VFS protects the shared wal-index with a POSIX
/// record lock on the `-shm` file (the "DMS" byte): a process that finds it unlocked assumes it
/// is the first opener and truncates `-shm` to zero bytes. POSIX releases every lock a process
/// holds on a file when *any* descriptor to that file is closed, so the S5 tightening of the
/// sidecars must never open and close a descriptor on `-shm` while this process's connection is
/// open: otherwise the next `kaban` process truncates the wal-index under the live one (SIGBUS in
/// the 8-process test). The lock must still be there once `open` returns.
#[test]
#[cfg(target_os = "linux")]
fn open_keeps_this_process_lock_on_the_shm_so_other_processes_never_truncate_it() {
    let s = Scratch::new("kaban-perms", "shm-lock");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    store
        .remember(note("a write keeps the wal-index mapped"), &[], &ctx())
        .unwrap();
    let shm = sidecar(&s.db(), "-shm");
    assert!(shm.exists(), "WAL mode sidecar expected after a write");
    assert!(
        holds_shm_dms_read_lock(&shm),
        "this process holds no READ lock covering byte 128 of {}: a later opener in another process would truncate the wal-index under this connection (locks: {:?})",
        shm.display(),
        own_posix_locks_on(&shm)
    );
    drop(store);
}

/// C2.5a, same rule for a second store in one process (the 8-process test's three contenders,
/// the migrator race tests): tightening files that are already owner-only must not open and
/// close descriptors on them either, or the first store's locks on the main file (its WAL-mode
/// SHARED lock, which stops a closing process from checkpointing and deleting `-wal`/`-shm`
/// under it) and on `-shm` are released.
#[test]
#[cfg(target_os = "linux")]
fn a_second_store_in_the_same_process_keeps_the_first_stores_locks() {
    let s = Scratch::new("kaban-perms", "second-opener");
    let first = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    first
        .remember(note("the first store maps the wal-index"), &[], &ctx())
        .unwrap();
    let shm = sidecar(&s.db(), "-shm");
    assert!(
        holds_shm_dms_read_lock(&shm),
        "fixture: the first store holds its -shm DMS lock"
    );
    assert!(
        holds_main_file_read_lock(&s.db()),
        "fixture: the first store holds its main-file lock"
    );
    let second = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert!(
        holds_shm_dms_read_lock(&shm),
        "opening a second store released this process's -shm DMS lock"
    );
    assert!(
        holds_main_file_read_lock(&s.db()),
        "opening a second store released this process's main-file lock"
    );
    drop(second);
    drop(first);
}

/// C2.5b (review P-1): the same rule when the files are *loose* while the first store is live.
/// The first opener in this process already tightened them (S5); a second opener must not open
/// and close a descriptor to tighten them again, because that would release the live store's
/// kernel locks. The mode is left to the next process-level open instead.
#[test]
#[cfg(target_os = "linux")]
fn a_second_store_on_a_file_loosened_under_a_live_store_keeps_the_first_stores_locks() {
    let s = Scratch::new("kaban-perms", "loose-second-opener");
    let first = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    first
        .remember(note("the first store maps the wal-index"), &[], &ctx())
        .unwrap();
    let wal = sidecar(&s.db(), "-wal");
    let shm = sidecar(&s.db(), "-shm");
    for f in [&s.db(), &wal, &shm] {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode(f), 0o644, "fixture loosened under the live store");
    }
    assert!(
        holds_shm_dms_read_lock(&shm),
        "fixture: the first store holds its -shm DMS lock"
    );
    assert!(
        holds_main_file_read_lock(&s.db()),
        "fixture: the first store holds its main-file lock"
    );
    let second = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert!(
        holds_shm_dms_read_lock(&shm),
        "opening a second store on a loose file released this process's -shm DMS lock"
    );
    assert!(
        holds_main_file_read_lock(&s.db()),
        "opening a second store on a loose file released this process's main-file lock"
    );
    second
        .remember(note("the second store writes too"), &[], &ctx())
        .unwrap();
    drop(second);
    drop(first);
}

/// C2.5b (review P-3): a database path that is a symlink is refused before anything is opened
/// or `fchmod`-ed (the link target is left exactly as it was), and so is a symlinked sidecar.
#[test]
fn symlinked_database_or_sidecar_path_is_refused_before_any_open() {
    let s = Scratch::new("kaban-perms", "symlink");
    let target = s.0.join("elsewhere.db");
    std::fs::write(&target, b"not a kaban database").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&target, s.db()).unwrap();
    let err = SqliteStore::open(&s.db(), &s.keys()).expect_err("symlink refused");
    assert!(
        matches!(err, kaban_core::StoreError::FileMode(_)),
        "expected FileMode, got {err:?}"
    );
    assert_eq!(mode(&target), 0o644, "the link target was not touched");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"not a kaban database",
        "the link target was not opened as a database"
    );
    assert!(!s.keys().exists(), "no key directory for a refused open");

    // a regular database with a symlinked sidecar is refused the same way
    let s2 = Scratch::new("kaban-perms", "symlink-sidecar");
    {
        let store = SqliteStore::open(&s2.db(), &s2.keys()).unwrap();
        store.remember(note("one note"), &[], &ctx()).unwrap();
    }
    let decoy = s2.0.join("decoy-wal");
    std::fs::write(&decoy, b"decoy").unwrap();
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&decoy, sidecar(&s2.db(), "-wal")).unwrap();
    let err = SqliteStore::open(&s2.db(), &s2.keys()).expect_err("symlinked -wal refused");
    assert!(
        matches!(err, kaban_core::StoreError::FileMode(_)),
        "expected FileMode, got {err:?}"
    );
    assert_eq!(
        mode(&decoy),
        0o644,
        "the sidecar link target was not touched"
    );
}

/// R4-2: the hook open is existing-only. A path that is a dangling name (the parent directory
/// writable, so a creating open would have succeeded) is refused as `FileMode`/`NotFound` and
/// no file appears; a symlinked database path is refused the same way with its target
/// untouched; and a loose database is opened without its mode changing.
#[test]
fn the_hook_open_never_creates_a_database_never_follows_a_symlink_and_never_tightens() {
    // a dangling name in a writable directory: nothing is created
    let s = Scratch::new("kaban-perms", "hook-missing");
    assert!(
        std::fs::metadata(&s.0).unwrap().permissions().mode() & 0o200 != 0,
        "the scratch directory is writable: a creating open would succeed here"
    );
    let err = SqliteStore::open_for_hook(&s.db(), &s.keys(), &ActorId::default())
        .expect_err("missing refused");
    match err {
        kaban_core::StoreError::FileMode(io) => {
            assert_eq!(io.kind(), std::io::ErrorKind::NotFound, "{io}");
        }
        other => panic!("expected FileMode, got {other:?}"),
    }
    assert!(
        !s.db().exists()
            && !sidecar(&s.db(), "-wal").exists()
            && !sidecar(&s.db(), "-shm").exists(),
        "a hook open must never create the database or a sidecar"
    );
    assert!(!s.keys().exists(), "no key directory for a refused open");

    // a symlink to a real kaban database: refused, the target is neither opened nor chmod-ed
    let s2 = Scratch::new("kaban-perms", "hook-symlink");
    let target = s2.0.join("elsewhere.db");
    {
        let store = SqliteStore::open(&target, &s2.keys()).unwrap();
        store.remember(note("one note"), &[], &ctx()).unwrap();
    }
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = std::fs::read(&target).unwrap();
    std::os::unix::fs::symlink(&target, s2.db()).unwrap();
    let err = SqliteStore::open_for_hook(&s2.db(), &s2.keys(), &ActorId::default())
        .expect_err("symlink refused");
    assert!(
        matches!(err, kaban_core::StoreError::FileMode(_)),
        "expected FileMode, got {err:?}"
    );
    assert_eq!(mode(&target), 0o644, "the link target was not tightened");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "the link target was not written through the link"
    );

    // a loose database nobody else holds (so the in-process live registry cannot mask a
    // tightening): opened as it is
    let s3 = Scratch::new("kaban-perms", "hook-loose");
    {
        let store = SqliteStore::open(&s3.db(), &s3.keys()).unwrap();
        store.remember(note("one note"), &[], &ctx()).unwrap();
    }
    std::fs::set_permissions(s3.db(), std::fs::Permissions::from_mode(0o644)).unwrap();
    let hook = SqliteStore::open_for_hook(&s3.db(), &s3.keys(), &ActorId::default())
        .expect("loose modes are not an error for a hook");
    assert_eq!(mode(&s3.db()), 0o644, "the hook open must not tighten");
    drop(hook);
}

/// RL-4: the hook open keeps the sidecar contract `prepare_database_files` enforces: an existing
/// `-wal` or `-shm` path that is a symlink or not a regular file is refused with non-mutating
/// `symlink_metadata` checks before SQLite opens anything, so the link target is neither read as
/// a WAL nor written through the link.
#[test]
fn the_hook_open_refuses_symlinked_or_non_regular_sidecars_and_leaves_their_targets_alone() {
    // a symlinked -wal beside a real, closed database
    let s = Scratch::new("kaban-perms", "hook-wal-symlink");
    {
        let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
        store.remember(note("one note"), &[], &ctx()).unwrap();
    }
    assert!(
        !sidecar(&s.db(), "-wal").exists(),
        "fixture: the sidecars are gone after a clean close"
    );
    let decoy = s.0.join("decoy-wal");
    std::fs::write(&decoy, b"decoy bytes, not a WAL").unwrap();
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&decoy, sidecar(&s.db(), "-wal")).unwrap();
    let err = SqliteStore::open_for_hook(&s.db(), &s.keys(), &ActorId::default())
        .expect_err("symlinked -wal refused");
    assert!(
        matches!(err, kaban_core::StoreError::FileMode(_)),
        "expected FileMode, got {err:?}"
    );
    assert_eq!(
        std::fs::read(&decoy).unwrap(),
        b"decoy bytes, not a WAL",
        "the -wal link target was not touched"
    );
    assert_eq!(
        mode(&decoy),
        0o644,
        "the -wal link target was not tightened"
    );
    assert!(
        !sidecar(&s.db(), "-shm").exists(),
        "nothing was created beside the refused database"
    );

    // a symlinked -shm (dangling: the target never existed) is refused the same way
    let s2 = Scratch::new("kaban-perms", "hook-shm-symlink");
    {
        let store = SqliteStore::open(&s2.db(), &s2.keys()).unwrap();
        store.remember(note("one note"), &[], &ctx()).unwrap();
    }
    let dangling = s2.0.join("never-created-shm");
    std::os::unix::fs::symlink(&dangling, sidecar(&s2.db(), "-shm")).unwrap();
    let err = SqliteStore::open_for_hook(&s2.db(), &s2.keys(), &ActorId::default())
        .expect_err("symlinked -shm refused");
    assert!(
        matches!(err, kaban_core::StoreError::FileMode(_)),
        "expected FileMode, got {err:?}"
    );
    assert!(
        !dangling.exists(),
        "the dangling -shm target was not created"
    );

    // a directory at the -wal path is not a regular file: refused
    let s3 = Scratch::new("kaban-perms", "hook-wal-dir");
    {
        let store = SqliteStore::open(&s3.db(), &s3.keys()).unwrap();
        store.remember(note("one note"), &[], &ctx()).unwrap();
    }
    std::fs::create_dir(sidecar(&s3.db(), "-wal")).unwrap();
    let err = SqliteStore::open_for_hook(&s3.db(), &s3.keys(), &ActorId::default())
        .expect_err("directory -wal refused");
    assert!(
        matches!(err, kaban_core::StoreError::FileMode(_)),
        "expected FileMode, got {err:?}"
    );
}
