//! The exec boundary: the executable as a path or a verified handle (OD-G1-4, RV-9), and the
//! admission of the descriptors that would cross `execve` (RG1-2); see the module documentation.

#[cfg(target_os = "linux")]
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ProcessError;

/// How the executable is named: by path (resolved by the kernel at `execve`, `PATH` lookup for a
/// bare name through the child's allowlisted `PATH` when present) or by an already verified
/// handle (see the module documentation).
#[derive(Debug)]
pub enum Exec {
    /// A path, absolute or looked up on the child's `PATH`.
    Path(PathBuf),
    /// An `O_PATH|O_NOFOLLOW|O_CLOEXEC` handle of an ELF regular file, executed as
    /// `/proc/self/fd/<n>` on Linux; `handle_exec_unsupported` elsewhere.
    Handle(OwnedFd),
}

impl Exec {
    /// Open `path` as a wrapper handle (`O_PATH|O_NOFOLLOW|O_CLOEXEC` on the final component)
    /// and check it: a regular file (a symlink at the final component opens as the link itself
    /// under `O_NOFOLLOW` and is refused by the `fstat`), owned by root or the invoking user,
    /// writable by neither group nor other, ELF magic read through the handle. [`run`] repeats
    /// the checks on the same handle immediately before the spawn.
    ///
    /// # Errors
    /// [`ProcessError::Spawn`] when the open fails (absent path, permission);
    /// [`ProcessError::Env`] with `wrapper_not_regular`, `wrapper_owner`, `wrapper_writable`,
    /// `wrapper_changed` or `wrapper_not_elf`.
    #[cfg(target_os = "linux")]
    pub fn open_handle(path: &Path) -> Result<Self, ProcessError> {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| ProcessError::Spawn(e.into()))?;
        verify_handle(&fd)?;
        Ok(Self::Handle(fd))
    }

    /// Handle execution exists on Linux only (RV-9).
    ///
    /// # Errors
    /// Always [`ProcessError::Env`]`("handle_exec_unsupported")` on this target.
    #[cfg(not(target_os = "linux"))]
    pub fn open_handle(_path: &Path) -> Result<Self, ProcessError> {
        Err(ProcessError::Env("handle_exec_unsupported"))
    }
}

/// What [`run`](super::run) does about descriptors this process holds without `FD_CLOEXEC`
/// (every one of them passes to the child at `execve`). Rust and rustix open with `CLOEXEC`, so
/// such a descriptor was inherited from an ancestor or had its flag cleared deliberately; the
/// runner cannot close or re-flag it without `unsafe` (it owns no handle to it), so it admits
/// or refuses the spawn instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum InheritedFds {
    /// Fail closed: `ProcessError::InheritedFd { fd }` before any spawn when one exists (the
    /// default; indexer runners, CG4.3), naming the lowest one.
    #[default]
    Refuse,
    /// Spawn anyway and trace once at `info` with the count only (no numbers, no targets): for
    /// a wrapper whose host may leak descriptors into the server (`GitRunner`; `info`, not
    /// `warn`, so a hook's stderr stays empty on a clean run under `RUST_LOG=warn`).
    Tolerate,
}

/// `O_CLOEXEC` as `/proc/self/fdinfo/<n>` prints it in `flags:` (octal).
#[cfg(target_os = "linux")]
const O_CLOEXEC: u64 = 0o2_000_000;

/// Admit the spawn under `policy`: count the descriptors above 2 whose `fdinfo` `flags:` lack
/// `O_CLOEXEC` (an unreadable or unparsable entry counts: fail closed), then refuse naming the
/// lowest one, or trace the count. A point check at the spawn boundary, not a sanitation: the
/// count is this thread's view at this instant, and the caller keeps every descriptor it had.
///
/// # Errors
/// [`ProcessError::InheritedFd`] under [`InheritedFds::Refuse`] when the count is not zero;
/// [`ProcessError::Spawn`] when `/proc/self/fd` cannot be read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn admit_inherited_fds(policy: InheritedFds) -> Result<(), ProcessError> {
    let fds = inheritable_fds().map_err(ProcessError::Spawn)?;
    admit_count(policy, fds.len(), fds.iter().copied().min())
}

/// Off Linux the flags cannot be read: `Refuse` fails closed (no number to name), `Tolerate`
/// traces that the count is unknown and spawns.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn admit_inherited_fds(policy: InheritedFds) -> Result<(), ProcessError> {
    match policy {
        InheritedFds::Refuse => Err(ProcessError::InheritedFd { fd: None }),
        InheritedFds::Tolerate => {
            tracing::info!("inherited descriptors not checkable on this target; spawning anyway");
            Ok(())
        }
    }
}

fn admit_count(policy: InheritedFds, count: usize, first: Option<u32>) -> Result<(), ProcessError> {
    if count == 0 {
        return Ok(());
    }
    match policy {
        InheritedFds::Refuse => Err(ProcessError::InheritedFd { fd: first }),
        InheritedFds::Tolerate => {
            tracing::info!(
                count,
                "descriptors without FD_CLOEXEC inherited by the child"
            );
            Ok(())
        }
    }
}

/// Enumerate descriptors above 2 without `FD_CLOEXEC`, without changing their flags.
///
/// A point observation: callers must still keep newly opened descriptors CLOEXEC.
///
/// # Errors
/// An unreadable descriptor table or malformed entry; unknown flags count as inheritable.
#[cfg(target_os = "linux")]
pub fn inheritable_fds() -> std::io::Result<Vec<u32>> {
    let mut fds = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd")? {
        let entry = entry?;
        let Ok(number) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if number <= 2 {
            continue;
        }
        let inherited = match std::fs::read_to_string(format!("/proc/self/fdinfo/{number}")) {
            // closed between the listing and the read: nothing to inherit
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
            Ok(info) => !has_cloexec(&info),
        };
        if inherited {
            fds.push(number);
        }
    }
    Ok(fds)
}

/// Enumerate the native descriptor table and query each original FD. Reopening a
/// `/dev/fd/N` path would observe the flags of a different descriptor.
///
/// # Errors
/// An unreadable descriptor table or malformed entry. Unknown flags fail closed.
#[cfg(target_os = "macos")]
pub fn inheritable_fds() -> std::io::Result<Vec<u32>> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let mut fds = Vec::new();
    for entry in std::fs::read_dir("/dev/fd")? {
        let entry = entry?;
        let number = entry
            .file_name()
            .to_string_lossy()
            .parse::<i32>()
            .map_err(|_| std::io::Error::other("descriptor_entry"))?;
        if number <= 2 {
            continue;
        }
        match fcntl(number, FcntlArg::F_GETFD) {
            Ok(flags) if flags & FdFlag::FD_CLOEXEC.bits() != 0 => (),
            Err(nix::errno::Errno::EBADF) => (), // closed since listing
            _ => fds.push(
                u32::try_from(number).map_err(|_| std::io::Error::other("descriptor_entry"))?,
            ),
        }
    }
    Ok(fds)
}

/// Whether an `fdinfo` text has a `flags:` line whose octal value includes `O_CLOEXEC`.
#[cfg(target_os = "linux")]
fn has_cloexec(fdinfo: &str) -> bool {
    fdinfo
        .lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .and_then(|f| u64::from_str_radix(f.trim(), 8).ok())
        .is_some_and(|flags| flags & O_CLOEXEC != 0)
}

/// The command for `exec`: the path, or `/proc/self/fd/<n>` after the handle checks.
#[cfg(target_os = "linux")]
pub(super) fn command_for(exec: &Exec) -> Result<Command, ProcessError> {
    match exec {
        Exec::Path(path) => Ok(Command::new(path)),
        Exec::Handle(fd) => {
            verify_handle(fd)?;
            Ok(Command::new(proc_fd_path(fd)))
        }
    }
}

/// The command for `exec`: the path; a handle is `handle_exec_unsupported` here.
#[cfg(not(target_os = "linux"))]
pub(super) fn command_for(exec: &Exec) -> Result<Command, ProcessError> {
    match exec {
        Exec::Path(path) => Ok(Command::new(path)),
        Exec::Handle(_) => Err(ProcessError::Env("handle_exec_unsupported")),
    }
}

#[cfg(target_os = "linux")]
fn proc_fd_path(fd: &OwnedFd) -> String {
    use std::os::fd::AsRawFd;
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// The handle contract (RV-9): a regular file, owned by root or the invoking user, writable by
/// neither group nor other, whose first four bytes (read through a second open of the same
/// handle) are the ELF magic.
#[cfg(target_os = "linux")]
fn verify_handle(fd: &OwnedFd) -> Result<(), ProcessError> {
    use rustix::fs::{FileType, Mode, OFlags};
    let stat = rustix::fs::fstat(fd).map_err(|e| ProcessError::Spawn(e.into()))?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ProcessError::Env("wrapper_not_regular"));
    }
    let me = rustix::process::geteuid().as_raw();
    if stat.st_uid != 0 && stat.st_uid != me {
        return Err(ProcessError::Env("wrapper_owner"));
    }
    if stat.st_mode & 0o022 != 0 {
        return Err(ProcessError::Env("wrapper_writable"));
    }
    let again = rustix::fs::open(
        proc_fd_path(fd),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| ProcessError::Spawn(e.into()))?;
    let same = rustix::fs::fstat(&again).map_err(|e| ProcessError::Spawn(e.into()))?;
    if (same.st_dev, same.st_ino) != (stat.st_dev, stat.st_ino) {
        return Err(ProcessError::Env("wrapper_changed"));
    }
    let mut magic = [0u8; 4];
    std::fs::File::from(again)
        .read_exact(&mut magic)
        .map_err(|_| ProcessError::Env("wrapper_not_elf"))?;
    if magic != [0x7f, b'E', b'L', b'F'] {
        return Err(ProcessError::Env("wrapper_not_elf"));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn fdinfo_flags_decide_cloexec_and_missing_flags_fail_closed() {
        assert!(has_cloexec("pos:\t0\nflags:\t02100000\nmnt_id:\t29\n"));
        assert!(!has_cloexec("pos:\t0\nflags:\t0100000\nmnt_id:\t29\n"));
        assert!(
            !has_cloexec("pos:\t0\nmnt_id:\t29\n"),
            "no flags line: not CLOEXEC"
        );
        assert!(!has_cloexec("flags:\tjunk\n"), "unparsable: not CLOEXEC");
    }

    #[test]
    fn a_zero_count_admits_and_refuse_fails_closed_on_one_naming_the_descriptor() {
        assert!(admit_count(InheritedFds::Refuse, 0, None).is_ok());
        assert!(admit_count(InheritedFds::Tolerate, 0, None).is_ok());
        assert!(matches!(
            admit_count(InheritedFds::Refuse, 1, Some(9)),
            Err(ProcessError::InheritedFd { fd: Some(9) })
        ));
        assert!(matches!(
            admit_count(InheritedFds::Refuse, 1, None),
            Err(ProcessError::InheritedFd { fd: None })
        ));
        assert!(admit_count(InheritedFds::Tolerate, 1, Some(9)).is_ok());
    }
}
