//! Linux namespace construction and verification; shared argument types stay in the owner.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::path::Path;

use rustix::fs::{Mode, OFlags, StatVfsMountFlags};
use rustix::io::Errno;
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use rustix::thread::{CapabilitiesSecureBits, CapabilitySet, CapabilitySets};

use super::{
    DEV_NODES, HelperMode, HelperOpts, NsError, NsInodes, Overlay, ProbeFinding, SCRATCH_MOUNT,
    USERNS_SYSCTLS, mount_points,
};

/// The private root under the run scratch.
const NEWROOT: &str = ".newroot";
/// Build and verify the Linux namespace view before probing or replacing the helper.
///
/// # Errors
/// Refuse a failed setup, self-check or producer exec.
pub fn run_helper(opts: &HelperOpts) -> Result<ProbeFinding, NsError> {
    let setup = |op: &'static str| move |source: Errno| NsError::Setup { op, source };
    let io_setup = |op: &'static str| {
        move |e: std::io::Error| NsError::Setup {
            op,
            source: Errno::from_io_error(&e).unwrap_or(Errno::IO),
        }
    };
    let view = &opts.view;
    // 0. the bind source is the admitted checkout object (U2 RU2-5): nothing is mounted for
    //    a path that leads elsewhere now
    if let Some((dev, ino)) = opts.expect.checkout {
        let st = rustix::fs::stat(&view.checkout).map_err(setup("stat_checkout"))?;
        if (st.st_dev, st.st_ino) != (dev, ino) {
            return Err(NsError::Setup {
                op: "checkout_identity",
                source: Errno::PERM,
            });
        }
    }
    // 1. no propagation back to the host's mount namespace
    rustix::mount::mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(setup("make_rprivate"))?;
    // 2. the private root: a tmpfs on a directory inside the scratch
    let newroot = view.scratch.join(NEWROOT);
    std::fs::create_dir_all(&newroot).map_err(io_setup("newroot_dir"))?;
    rustix::mount::mount(
        "tmpfs",
        &newroot,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        Some(c"mode=0755"),
    )
    .map_err(setup("tmpfs"))?;
    // 3. the directory skeleton, created while the tmpfs is still writable
    let inside = |p: &Path| newroot.join(p.strip_prefix("/").unwrap_or(p));
    for sub in ["proc", "dev", "etc", "scratch", "caches"] {
        std::fs::create_dir_all(newroot.join(sub)).map_err(io_setup("skeleton_dir"))?;
    }
    for p in &view.ro {
        let dst = inside(p);
        if p.is_dir() {
            std::fs::create_dir_all(&dst).map_err(io_setup("ro_dir"))?;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(io_setup("ro_parent"))?;
            }
            std::fs::write(&dst, b"").map_err(io_setup("ro_file"))?;
        }
    }
    for c in &view.caches {
        std::fs::create_dir_all(newroot.join("caches").join(&c.name))
            .map_err(io_setup("cache_dir"))?;
    }
    for node in DEV_NODES {
        std::fs::write(newroot.join("dev").join(node), b"").map_err(io_setup("dev_node"))?;
    }
    // a kaban-generated minimal passwd and group: uid 0 inside maps to the invoking user
    std::fs::write(
        newroot.join("etc/passwd"),
        format!("kaban:x:0:0:kaban-index:{SCRATCH_MOUNT}/home:/sbin/nologin\n"),
    )
    .map_err(io_setup("passwd"))?;
    std::fs::write(newroot.join("etc/group"), "kaban:x:0:\n").map_err(io_setup("group"))?;
    // 4. the read-only binds (the source mount's locked flags are kept on the remount)
    for p in &view.ro {
        bind_ro(p, &inside(p))?;
    }
    // 5. the scratch, writable
    rustix::mount::mount_bind(&view.scratch, newroot.join("scratch"))
        .map_err(setup("bind_scratch"))?;
    // 6. /dev nodes
    for node in DEV_NODES {
        rustix::mount::mount_bind(Path::new("/dev").join(node), newroot.join("dev").join(node))
            .map_err(setup("bind_dev"))?;
    }
    // 7. caches: overlay or the plain read-only bind
    let overlay = match &opts.mode {
        HelperMode::Probe => {
            let lower = view.scratch.join("probe-lower");
            std::fs::create_dir_all(&lower).map_err(io_setup("probe_lower"))?;
            let dst = newroot.join("caches").join("probe");
            std::fs::create_dir_all(&dst).map_err(io_setup("probe_cache_dir"))?;
            let found = if mount_overlay(&view.scratch, "probe", &lower, &dst).is_ok() {
                Overlay::Overlayfs
            } else {
                Overlay::None
            };
            // the probe's expected mount list has no caches: undo the trial mount
            if found == Overlay::Overlayfs {
                rustix::mount::unmount(&dst, UnmountFlags::DETACH)
                    .map_err(setup("probe_unmount"))?;
            }
            found
        }
        HelperMode::Run { overlay, .. } => {
            for c in &view.caches {
                let dst = newroot.join("caches").join(&c.name);
                match overlay {
                    Overlay::Overlayfs => mount_overlay(&view.scratch, &c.name, &c.lower, &dst)?,
                    Overlay::None => {
                        if c.lower.is_dir() {
                            bind_ro(&c.lower, &dst)?;
                        } else {
                            // an absent user cache: an empty read-only directory of the root
                            rustix::mount::mount_bind(&dst, &dst).map_err(setup("bind_cache"))?;
                            remount_ro(&dst)?;
                        }
                    }
                }
            }
            *overlay
        }
    };
    // 8. a fresh /proc (before the pivot: the kernel requires a visible procfs of this pid
    //    namespace in the mount namespace to mount another)
    rustix::mount::mount(
        "proc",
        newroot.join("proc"),
        "proc",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        None,
    )
    .map_err(setup("proc"))?;
    // 9. the root itself read-only, then the pivot
    rustix::mount::mount_remount(
        &newroot,
        MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
        "",
    )
    .map_err(setup("remount_root_ro"))?;
    rustix::process::chdir(&newroot).map_err(setup("chdir_newroot"))?;
    rustix::process::pivot_root(".", ".").map_err(setup("pivot_root"))?;
    rustix::mount::unmount(".", UnmountFlags::DETACH).map_err(setup("umount_old_root"))?;
    rustix::process::chdir("/").map_err(setup("chdir_root"))?;
    // 10. the self-check (S5.2), every item
    self_check(opts)?;
    match &opts.mode {
        HelperMode::Probe => Ok(ProbeFinding { overlay }),
        HelperMode::Run { indexer, args, .. } => {
            rustix::process::chdir(&view.checkout).map_err(setup("chdir_checkout"))?;
            // 11. the irreversible privilege drop (U2 RU2-1), the indexer identity (U2
            //     RU2-3), then the exec
            drop_privileges()?;
            if let Some((dev, ino)) = opts.expect.indexer {
                let st = rustix::fs::stat(indexer).map_err(setup("stat_indexer"))?;
                if (st.st_dev, st.st_ino) != (dev, ino) {
                    return Err(NsError::Setup {
                        op: "indexer_identity",
                        source: Errno::PERM,
                    });
                }
            }
            Err(NsError::Exec(crate::runner::exec_indexer(indexer, args)))
        }
    }
}

/// The securebits set and locked before the `exec`: uid 0 gains nothing on `exec` (`NO_ROOT`),
/// no capability fix-up on a uid change, `KEEP_CAPS` locked off, no ambient raise; every bit
/// locked so the indexer cannot clear it.
const SECURE_BITS: CapabilitiesSecureBits = CapabilitiesSecureBits::NO_ROOT
    .union(CapabilitiesSecureBits::NO_ROOT_LOCKED)
    .union(CapabilitiesSecureBits::NO_SETUID_FIXUP)
    .union(CapabilitiesSecureBits::NO_SETUID_FIXUP_LOCKED)
    .union(CapabilitiesSecureBits::KEEP_CAPS_LOCKED)
    .union(CapabilitiesSecureBits::NO_CAP_AMBIENT_RAISE)
    .union(CapabilitiesSecureBits::NO_CAP_AMBIENT_RAISE_LOCKED);

/// The highest capability number the kernel knows (`cap_last_cap`); 63 bounds the loop when
/// the file is unreadable (`PR_CAPBSET_DROP` of an unknown number is `EINVAL`, which is then
/// a setup failure: fail closed).
fn cap_last() -> u32 {
    std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(63)
        .min(63)
}

/// The irreversible privilege drop before the indexer `exec` (U2 RU2-1, G1-S5.1/S5.2). The
/// helper is uid 0 of the user namespace with every capability over it, which an indexer
/// could use to remount the read-only binds writable or mount new filesystems once the
/// self-check has passed. In order: nested user namespaces are closed (`max_user_namespaces
/// = 0` in this namespace's own sysctl: a new user namespace would hand its creator a full
/// capability set over a fresh mount namespace), the securebits are set and locked, the
/// ambient set is cleared, every capability the kernel knows is removed from the bounding set,
/// `no_new_privs` is set (no file capability or setuid bit raises privileges across `exec`),
/// and the effective, permitted and inheritable sets are emptied. Every state is read back
/// before the `exec`; any step or read-back that does not hold is a setup failure (exit 8).
fn drop_privileges() -> Result<(), NsError> {
    let setup = |op: &'static str| move |source: Errno| NsError::Setup { op, source };
    std::fs::write(USERNS_SYSCTLS[1], "0").map_err(|e| NsError::Setup {
        op: "max_user_namespaces",
        source: Errno::from_io_error(&e).unwrap_or(Errno::IO),
    })?;
    rustix::thread::set_capabilities_secure_bits(SECURE_BITS).map_err(setup("securebits"))?;
    rustix::thread::clear_ambient_capability_set().map_err(setup("ambient"))?;
    let last = cap_last();
    for n in 0..=last {
        rustix::thread::remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(
            1u64 << n,
        ))
        .map_err(setup("bounding_set"))?;
    }
    rustix::thread::set_no_new_privs(true).map_err(setup("no_new_privs"))?;
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )
    .map_err(setup("capset"))?;
    // read back: nothing below may hold a bit
    let sets = rustix::thread::capabilities(None).map_err(setup("capget"))?;
    let bounding_empty = (0..=last).all(|n| {
        rustix::thread::capability_is_in_bounding_set(CapabilitySet::from_bits_retain(1u64 << n))
            == Ok(false)
    });
    let no_new_privs = rustix::thread::no_new_privs().map_err(setup("no_new_privs_read"))?;
    let secure = rustix::thread::capabilities_secure_bits().map_err(setup("securebits_read"))?;
    let limit = std::fs::read_to_string(USERNS_SYSCTLS[1]).map_err(|e| NsError::Setup {
        op: "max_user_namespaces_read",
        source: Errno::from_io_error(&e).unwrap_or(Errno::IO),
    })?;
    let dropped = sets.effective.is_empty()
        && sets.permitted.is_empty()
        && sets.inheritable.is_empty()
        && bounding_empty
        && no_new_privs
        && secure.contains(SECURE_BITS)
        && limit.trim() == "0";
    if dropped {
        Ok(())
    } else {
        Err(NsError::Setup {
            op: "privileges_not_dropped",
            source: Errno::PERM,
        })
    }
}

/// Bind `src` at `dst` and remount it read-only, keeping the flags the source mount carries
/// (a user namespace may not clear `nosuid`, `nodev`, `noexec` or the atime flags it
/// inherited: the kernel refuses the remount otherwise).
fn bind_ro(src: &Path, dst: &Path) -> Result<(), NsError> {
    rustix::mount::mount_bind(src, dst).map_err(|source| NsError::Setup { op: "bind", source })?;
    remount_ro(dst)
}

fn remount_ro(dst: &Path) -> Result<(), NsError> {
    let vfs = rustix::fs::statvfs(dst).map_err(|source| NsError::Setup {
        op: "statvfs",
        source,
    })?;
    let mut flags = MountFlags::BIND | MountFlags::RDONLY;
    for (vfs_flag, mount_flag) in [
        (StatVfsMountFlags::NOSUID, MountFlags::NOSUID),
        (StatVfsMountFlags::NODEV, MountFlags::NODEV),
        (StatVfsMountFlags::NOEXEC, MountFlags::NOEXEC),
        (StatVfsMountFlags::NOATIME, MountFlags::NOATIME),
        (StatVfsMountFlags::NODIRATIME, MountFlags::NODIRATIME),
        (StatVfsMountFlags::RELATIME, MountFlags::RELATIME),
    ] {
        if vfs.f_flag.contains(vfs_flag) {
            flags |= mount_flag;
        }
    }
    rustix::mount::mount_remount(dst, flags, "").map_err(|source| NsError::Setup {
        op: "remount_ro",
        source,
    })
}

/// `overlay` at `dst` with `lowerdir=<lower>`, `upperdir` and `workdir` under
/// `<scratch>/cache/<name>/` (host paths: the helper runs before the pivot).
fn mount_overlay(scratch: &Path, name: &str, lower: &Path, dst: &Path) -> Result<(), NsError> {
    let base = scratch.join("cache").join(name);
    let upper = base.join("upper");
    let work = base.join("work");
    for d in [&upper, &work] {
        std::fs::create_dir_all(d).map_err(|e| NsError::Setup {
            op: "overlay_dirs",
            source: Errno::from_io_error(&e).unwrap_or(Errno::IO),
        })?;
    }
    let data = format!(
        "lowerdir={},upperdir={},workdir={}",
        overlay_escape(lower),
        overlay_escape(&upper),
        overlay_escape(&work)
    );
    let data = CString::new(data).map_err(|_| NsError::Setup {
        op: "overlay_data",
        source: Errno::INVAL,
    })?;
    rustix::mount::mount(
        "overlay",
        dst,
        "overlay",
        MountFlags::empty(),
        Some(data.as_c_str()),
    )
    .map_err(|source| NsError::Setup {
        op: "overlay",
        source,
    })
}

/// Overlayfs option escaping: `\`, `,` and `:` in a path.
fn overlay_escape(p: &Path) -> String {
    let mut out = String::new();
    for ch in p.to_string_lossy().chars() {
        if matches!(ch, '\\' | ',' | ':') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The S5.2 self-check from the inside: every item is evaluated in the spec's order and every
/// failing item is reported (a missing namespace flag shows up both as the equal inode and as
/// what that namespace lets through, so the refusal names the anomaly, not only its first
/// symptom). A `/proc` file that cannot be read is a setup failure, not a mismatch.
fn self_check(opts: &HelperOpts) -> Result<(), NsError> {
    let mut failed: Vec<&'static str> = Vec::new();
    // 1. the three namespace inodes differ from the parent's
    let mine = NsInodes::current()?;
    if mine.user == opts.parent.user || mine.net == opts.parent.net || mine.mnt == opts.parent.mnt {
        failed.push("namespace_inodes");
    }
    // 2. the network namespace holds the loopback interface only
    let netdev = std::fs::read_to_string("/proc/net/dev").map_err(|e| NsError::Setup {
        op: "read_net_dev",
        source: Errno::from_io_error(&e).unwrap_or(Errno::IO),
    })?;
    let interfaces: BTreeSet<&str> = netdev
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next())
        .map(str::trim)
        .collect();
    if interfaces != BTreeSet::from(["lo"]) {
        failed.push("network_interfaces");
    }
    // 3. a write into the checkout fails with EROFS
    let marker = opts.view.checkout.join(".kaban-ns-selfcheck");
    match rustix::fs::open(
        &marker,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ) {
        Err(Errno::ROFS) => {}
        Ok(_) => {
            let _ = std::fs::remove_file(&marker);
            failed.push("checkout_writable");
        }
        Err(_) => failed.push("checkout_not_erofs"),
    }
    // 4. the scratch is writable
    let probe = Path::new(SCRATCH_MOUNT).join(".kaban-ns-selfcheck");
    if std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .is_err()
    {
        failed.push("scratch_not_writable");
    }
    // 5. the mount list is exactly the view's
    let mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").map_err(|e| NsError::Setup {
            op: "read_mountinfo",
            source: Errno::from_io_error(&e).unwrap_or(Errno::IO),
        })?;
    if mount_points(&mountinfo) != opts.view.mount_points() {
        failed.push("mount_list");
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(NsError::SelfCheck(failed))
    }
}
