//! Discovery slices (stage1-plan CG2.3; stage1-spec G1-F8; cg-join-v1 §4): exact, nearest
//! ancestor, the four `NoSnapshot` reasons, the `SnapshotRef` fields and the LRU of two.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/builder.rs"]
mod builder;

use std::os::unix::fs::PermissionsExt;

use builder::{Snapshot, commit, final_path, scratch};
use kaban_codegraph::{
    HeadInput, IsolationTier, NoSnapshot, SCHEMA_VERSION, SnapshotCache, SnapshotError, discover,
};

fn head(commit: &str, ancestors: &[&str]) -> HeadInput {
    HeadInput {
        commit: commit.to_owned(),
        ancestors: ancestors.iter().map(|a| (*a).to_owned()).collect(),
    }
}

#[test]
fn discovery_exact() {
    let dir = scratch();
    let head_commit = commit(0x50);
    let ancestor = commit(0x51);
    let exact = Snapshot::new(&head_commit).write_final(dir.path(), &head_commit);
    Snapshot::new(&ancestor).write_final(dir.path(), &ancestor);
    let (path, r#ref) = discover(dir.path(), &head(&head_commit, &[&ancestor])).unwrap();
    assert_eq!(path, exact);
    assert!(r#ref.exact);
    assert_eq!(r#ref.commit, head_commit);
}

#[test]
fn discovery_nearest_ancestor_not_skipped() {
    let dir = scratch();
    let head_commit = commit(0x60);
    // six newer non-ancestor snapshots (other branches) and one older ancestor
    for n in 0x61..=0x66 {
        let other = commit(n);
        Snapshot::new(&other).write_final(dir.path(), &other);
    }
    let older = commit(0x6a);
    let oldest = commit(0x6b);
    let older_path = Snapshot::new(&older).write_final(dir.path(), &older);
    Snapshot::new(&oldest).write_final(dir.path(), &oldest);
    let nearest_without_file = commit(0x69);
    let input = head(&head_commit, &[&nearest_without_file, &older, &oldest]);
    let (path, r#ref) = discover(dir.path(), &input).unwrap();
    assert_eq!(
        path, older_path,
        "the first ancestor with a snapshot, not a newer non-ancestor"
    );
    assert!(!r#ref.exact);
    assert_eq!(r#ref.commit, older);
}

#[test]
fn discovery_none() {
    let dir = scratch();
    let head_commit = commit(0x70);
    let other = commit(0x71);
    Snapshot::new(&other).write_final(dir.path(), &other);
    assert_eq!(
        discover(dir.path(), &head(&head_commit, &[&commit(0x72)])).unwrap_err(),
        NoSnapshot::NoFile,
        "directory present, no candidate for head or any ancestor"
    );
    assert_eq!(
        discover(dir.path(), &head(&head_commit, &[])).unwrap_err(),
        NoSnapshot::NoFile
    );
}

#[test]
fn discovery_no_directory() {
    let dir = scratch();
    let missing = dir.path().join("no-such-repo-id");
    assert_eq!(
        discover(&missing, &head(&commit(0x73), &[])).unwrap_err(),
        NoSnapshot::NoDirectory
    );
    let as_file = dir.path().join("a-file");
    std::fs::write(&as_file, b"x").unwrap();
    assert_eq!(
        discover(&as_file, &head(&commit(0x73), &[])).unwrap_err(),
        NoSnapshot::NoDirectory
    );
}

/// A damaged copy of a valid snapshot under `commit`'s final name.
fn corrupt_file(dir: &std::path::Path, commit: &str) -> std::path::PathBuf {
    let path = final_path(dir, commit);
    std::fs::write(&path, b"SQLite format 3\0 damaged").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn too_new_file(dir: &std::path::Path, commit: &str) -> std::path::PathBuf {
    let mut s = Snapshot::new(commit);
    s.set_meta("schema_version", &(SCHEMA_VERSION + 1).to_string());
    s.write_final(dir, commit)
}

#[test]
fn discovery_skips_corrupt() {
    let dir = scratch();
    let head_commit = commit(0x80);
    let ancestor = commit(0x81);
    corrupt_file(dir.path(), &head_commit);
    let ancestor_path = Snapshot::new(&ancestor).write_final(dir.path(), &ancestor);
    let (path, r#ref) = discover(dir.path(), &head(&head_commit, &[&ancestor])).unwrap();
    assert_eq!(
        path, ancestor_path,
        "the ancestor is chosen past the corrupt exact file"
    );
    assert!(!r#ref.exact);

    // corrupt only: every existing candidate was skipped as corrupt
    let lonely = commit(0x82);
    corrupt_file(dir.path(), &lonely);
    assert_eq!(
        discover(dir.path(), &head(&lonely, &[&commit(0x83)])).unwrap_err(),
        NoSnapshot::CorruptOnly
    );
    // a refused mode counts as skipped, not as absent
    let loose = commit(0x84);
    let loose_path = Snapshot::new(&loose).write_final(dir.path(), &loose);
    std::fs::set_permissions(&loose_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        discover(dir.path(), &head(&loose, &[])).unwrap_err(),
        NoSnapshot::CorruptOnly
    );
}

#[test]
fn discovery_too_new_only() {
    let dir = scratch();
    let head_commit = commit(0x90);
    too_new_file(dir.path(), &head_commit);
    assert_eq!(
        discover(dir.path(), &head(&head_commit, &[])).unwrap_err(),
        NoSnapshot::TooNewOnly
    );
    // too new beside a corrupt ancestor: TooNewOnly wins (the user can act on it)
    let corrupt_ancestor = commit(0x91);
    corrupt_file(dir.path(), &corrupt_ancestor);
    assert_eq!(
        discover(dir.path(), &head(&head_commit, &[&corrupt_ancestor])).unwrap_err(),
        NoSnapshot::TooNewOnly
    );
    // and the other way round, corrupt exact with a too-new ancestor
    let corrupt_head = commit(0x92);
    corrupt_file(dir.path(), &corrupt_head);
    assert_eq!(
        discover(dir.path(), &head(&corrupt_head, &[&head_commit])).unwrap_err(),
        NoSnapshot::TooNewOnly
    );
    // a valid older ancestor past a too-new exact file is still found
    let valid = commit(0x93);
    let valid_path = Snapshot::new(&valid).write_final(dir.path(), &valid);
    let (path, _) = discover(dir.path(), &head(&head_commit, &[&valid])).unwrap();
    assert_eq!(path, valid_path);
}

#[test]
fn snapshot_ref_fields() {
    let dir = scratch();
    let head_commit = commit(0xa0);
    let mut s = Snapshot::new(&head_commit);
    s.set_meta("isolation", "container");
    s.set_meta("policy_version", "pv-7");
    s.write_final(dir.path(), &head_commit);
    let (_, r#ref) = discover(dir.path(), &head(&head_commit, &[])).unwrap();
    assert_eq!(r#ref.commit, head_commit);
    assert!(r#ref.exact);
    assert_eq!(r#ref.isolation_tier, IsolationTier::Container);
    assert_eq!(r#ref.schema_version, SCHEMA_VERSION);
    assert_eq!(r#ref.policy_version, "pv-7");

    // a meta that disagrees with the file name, or an unknown tier, is skipped like a corrupt file
    let liar = commit(0xa1);
    let mut s = Snapshot::new(&commit(0xa2));
    s.set_meta("isolation", "none");
    s.write_final(dir.path(), &liar);
    assert_eq!(
        discover(dir.path(), &head(&liar, &[])).unwrap_err(),
        NoSnapshot::CorruptOnly
    );
    let odd_tier = commit(0xa3);
    let mut s = Snapshot::new(&odd_tier);
    s.set_meta("isolation", "vm");
    s.write_final(dir.path(), &odd_tier);
    assert_eq!(
        discover(dir.path(), &head(&odd_tier, &[])).unwrap_err(),
        NoSnapshot::CorruptOnly
    );
}

/// How many descriptors of this process are open on `path` (SQLite holds one per read-only
/// connection): the connection-lifetime probe of RG2-6. nextest runs each test in its own
/// process, so no other test's connections are counted.
fn open_fds_to(path: &std::path::Path) -> usize {
    builder::open_fds_to(path)
}

/// `GM2a` RG2-6: the cache bounds **cached** connections at two; a caller's lease
/// (`Arc<SqliteSnapshot>`) is request-scoped and may outlive the eviction, and the connection
/// closes when the last lease drops. A third open, even one that fails, is admitted only after
/// the least recently used entry was released.
#[test]
fn snapshot_cache_keeps_two_and_evicts_the_least_recently_used() {
    let dir = scratch();
    let paths: Vec<_> = (0xb0..0xb3u8)
        .map(|n| {
            let c = commit(n);
            Snapshot::new(&c).write_final(dir.path(), &c)
        })
        .collect();
    let mut cache = SnapshotCache::new();
    assert!(cache.is_empty());
    let first = cache.get_or_open(&paths[0]).unwrap();
    let second = cache.get_or_open(&paths[1]).unwrap();
    assert_eq!(cache.len(), 2);
    assert_eq!(
        open_fds_to(&paths[0]),
        1,
        "one connection per cached snapshot"
    );
    assert_eq!(open_fds_to(&paths[1]), 1);
    // touching the first makes the second the least recently used
    let first_again = cache.get_or_open(&paths[0]).unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&first, &first_again),
        "the same open snapshot"
    );
    drop(first_again);
    let third = cache.get_or_open(&paths[2]).unwrap();
    assert_eq!(cache.len(), SnapshotCache::CAPACITY);
    assert!(cache.contains(&paths[0]) && cache.contains(&paths[2]));
    assert!(
        !cache.contains(&paths[1]),
        "the least recently used was evicted"
    );
    // the evicted entry's connection is held by the test's lease alone: the cache dropped its
    // reference, and the connection stays open until that lease drops
    assert_eq!(
        std::sync::Arc::strong_count(&second),
        1,
        "the lease is the last holder"
    );
    assert_eq!(
        open_fds_to(&paths[1]),
        1,
        "the lease keeps the connection alive"
    );
    // a reopen after eviction is a new connection, not the lease's
    let reopened = cache.get_or_open(&paths[1]).unwrap();
    assert!(
        !std::sync::Arc::ptr_eq(&second, &reopened),
        "no shared cache entry"
    );
    assert_eq!(open_fds_to(&paths[1]), 2, "the lease and the new entry");
    drop(reopened);
    drop(second);
    assert_eq!(
        open_fds_to(&paths[1]),
        1,
        "the lease's connection closed when it dropped; the cached one stays"
    );
    assert!(!cache.contains(&paths[0]), "the reopen evicted the first");
    drop(first);
    assert_eq!(open_fds_to(&paths[0]), 0, "closed with the last lease");
    drop(third);

    // a refused file does not enter the cache, but the slot it would take was released first:
    // the third connection is never open beside two cached ones
    let bad = final_path(dir.path(), &commit(0xb4));
    std::fs::write(&bad, b"nope").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(cache.len(), 2);
    assert_eq!(cache.get_or_open(&bad).unwrap_err(), SnapshotError::Corrupt);
    assert!(!cache.contains(&bad));
    assert_eq!(
        cache.len(),
        1,
        "the least recently used slot was released before the open"
    );
    assert!(cache.contains(&paths[1]), "the most recently used stays");
    assert_eq!(
        open_fds_to(&paths[2]),
        0,
        "the released connection closed (no lease held)"
    );
    assert_eq!(open_fds_to(&paths[1]), 1);
}
