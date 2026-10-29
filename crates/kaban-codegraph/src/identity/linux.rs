//! Linux handle operations: preserve `O_PATH` traversal and optional statx birth time.

use std::os::fd::AsFd;

use rustix::fs::{AtFlags, OFlags, Stat, StatxFlags};

pub(super) const WALK_FLAGS: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) fn device(st: &Stat) -> u64 {
    st.st_dev
}

pub(super) fn mode(st: &Stat) -> u32 {
    st.st_mode
}

pub(super) fn birthtime(fd: impl AsFd) -> Option<i128> {
    rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::BTIME)
        .ok()
        .filter(|sx| StatxFlags::from_bits_retain(sx.stx_mask).contains(StatxFlags::BTIME))
        .map(|sx| {
            i128::from(sx.stx_btime.tv_sec) * 1_000_000_000 + i128::from(sx.stx_btime.tv_nsec)
        })
}
