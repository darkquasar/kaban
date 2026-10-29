//! The builder's capture session (spec 05 SS-18, lane T4): what the pack builder copies is the
//! directory as the held writer leaves it, so the session must leave it alone once drained.
//! Tantivy's default merge policy merges small segments in the background after a commit and
//! rewrites `meta.json` and collects the merged-away files while doing so; a capture's drain
//! commits, so without a frozen writer a merge races the copy (T4 review t4-4).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "tantivy_common/mod.rs"]
mod common;

use common::{Scratch, note};
use kaban_index_tantivy::projector::Projector;
use std::path::Path;
use std::time::{Duration, Instant};

/// The directory as a pack would copy it: every regular file but the lock files, with its
/// bytes.
fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && p.extension().is_none_or(|ext| ext != "lock"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

/// Enough one-row commits for the default merge policy to merge them on the next commit
/// (its `min_num_segments` is 8), each made by a writer of its own and left unmerged. The
/// seeding writers are capture sessions, the one entry point whose merges are frozen: a
/// plain `catch_up` writer joins its background merges before it releases the lock (T2
/// review t2-1), so the eighth plain commit would hand the capture one merged segment.
const SMALL_SEGMENTS: usize = 8;

/// T4 review t4-4: after the capture's drain committed a page onto enough small segments for
/// a merge to be due, the held directory stays exactly as the drain left it: no merge
/// rewrites `meta.json` or removes a segment file while the session lives, so the files a
/// pack copies and the `meta.json` it copies belong together.
#[test]
fn the_held_directory_is_frozen_once_drained() {
    let s = Scratch::new("capture-frozen");
    let store = s.store();
    let index = s.adapter();
    for i in 0..SMALL_SEGMENTS {
        note(
            &store,
            "default",
            "Fact",
            &format!("quokka number {i}"),
            &[],
        );
        let seeded_head = kaban_core::ChangeLogStore::changes_head(&store).unwrap();
        let seeder = Projector::new(&index, &store, &store, "test:seed", 2);
        let mut seeding = seeder
            .capture()
            .unwrap()
            .unwrap_or_else(|halt| panic!("seed {i}: lease and lock taken: {halt:?}"));
        assert_eq!(
            seeding
                .drain(seeded_head, Instant::now() + Duration::from_secs(30))
                .unwrap(),
            Ok(seeded_head),
            "seed {i} drained to its head"
        );
    }
    let dir = s.index_dir();
    let segments_before = listing(&dir)
        .iter()
        .filter(|(name, _)| {
            Path::new(name)
                .extension()
                .is_some_and(|ext| ext == "store")
        })
        .count();
    assert!(
        segments_before >= SMALL_SEGMENTS,
        "one segment per commit: {segments_before}"
    );
    // the page the capture's drain commits
    note(
        &store,
        "default",
        "Fact",
        "quokka pending for the capture",
        &[],
    );
    let head = kaban_core::ChangeLogStore::changes_head(&store).unwrap();

    let projector = Projector::new(&index, &store, &store, "test:capture", 60);
    let mut capture = projector
        .capture()
        .unwrap()
        .unwrap_or_else(|halt| panic!("lease and lock taken: {halt:?}"));
    let published = capture
        .drain(head, Instant::now() + Duration::from_secs(30))
        .unwrap()
        .expect("drained to the head");
    assert_eq!(published, head);
    let drained = listing(&dir);
    // a background merge of nine one-row segments is a matter of milliseconds
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        listing(&dir),
        drained,
        "the directory changed while the capture held it"
    );
    let (captured_dir, payload) = capture.captured().unwrap().expect("a committed directory");
    assert_eq!(captured_dir, dir);
    assert_eq!(payload["seq"], head);
}

/// T4 review t4-8 (SS-18): the capture's drain is bounded by the builder's target and
/// deadline, so a writer that keeps the log moving cannot keep the builder from reaching its
/// lock or reporting busy. A deadline already passed stops the drain at once as
/// `Halt::Deadline` with the pending changes unpublished; a target below the head is covered
/// and the drain returns within the head, following nothing beyond it.
#[test]
fn the_capture_drain_stops_at_its_deadline_and_covers_its_target() {
    use kaban_core::ChangeLogStore;
    use kaban_core::text_index::{TextIndex, Visibility};
    use kaban_index_tantivy::projector::Halt;
    let s = Scratch::new("capture-bounded");
    let store = s.store();
    let index = s.adapter();
    note(&store, "default", "Fact", "quokka built first", &[]);
    let seeded = Projector::new(&index, &store, &store, "test:seed", 2);
    assert_eq!(seeded.catch_up().unwrap().halt, None);
    let built = store.changes_head().unwrap();
    assert_eq!(
        index.visible().unwrap(),
        Visibility::Through(i64::try_from(built).unwrap())
    );
    note(&store, "default", "Fact", "quokka pending one", &[]);
    let target = store.changes_head().unwrap();
    note(&store, "default", "Fact", "quokka pending two", &[]);
    let head = store.changes_head().unwrap();
    assert!(built < target && target < head);

    let projector = Projector::new(&index, &store, &store, "test:capture", 60);
    let mut capture = projector
        .capture()
        .unwrap()
        .unwrap_or_else(|halt| panic!("lease and lock taken: {halt:?}"));
    let passed = Instant::now();
    assert_eq!(
        capture.drain(head, passed).unwrap(),
        Err(Halt::Deadline),
        "a deadline already passed stops the drain"
    );
    assert_eq!(
        index.visible().unwrap(),
        Visibility::Through(i64::try_from(built).unwrap()),
        "nothing published past the deadline"
    );
    let published = capture
        .drain(target, Instant::now() + Duration::from_secs(30))
        .unwrap()
        .expect("drained through the target");
    assert!(
        (target..=head).contains(&published),
        "published {published}, target {target}, head {head}"
    );
}
