//! Darwin directory descriptors and native, descriptor-bound metadata.

use std::os::fd::AsFd;

use rustix::fs::{OFlags, Stat};

// Darwin has no O_PATH in rustix. A read-only directory descriptor still pins the
// admitted inode; NOFOLLOW refuses links at every openat. This conservatively
// requires directory read permission in addition to traversal permission.
pub(super) const WALK_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) fn device(st: &Stat) -> u64 {
    // Darwin dev_t is signed i32, but the identifier is its 32-bit pattern,
    // not a signed quantity to sign-extend into the persisted u64 identity.
    u64::from(u32::from_ne_bytes(st.st_dev.to_ne_bytes()))
}

pub(super) fn mode(st: &Stat) -> u32 {
    u32::from(st.st_mode)
}

pub(super) fn birthtime(fd: impl AsFd) -> Option<i128> {
    let st = rustix::fs::fstat(fd).ok()?;
    // A zero birth time denotes unavailable metadata. Reject malformed nanos
    // rather than manufacturing an identity discriminator.
    if (st.st_birthtime == 0 && st.st_birthtime_nsec == 0)
        || !(0..1_000_000_000).contains(&st.st_birthtime_nsec)
    {
        return None;
    }
    Some(i128::from(st.st_birthtime) * 1_000_000_000 + i128::from(st.st_birthtime_nsec))
}
