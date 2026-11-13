//! The S5 purge's revocation and cleanup report (SS-03): a purge is revoked at once, the
//! cleanup residue is named until the WAL reader goes or a VACUUM runs. The claim-side tests
//! (purge re-deciding groups, tombstoned claims) went with the claims (T3).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_core::{
    Actor, ForgetMode, IngestStore, MaintenanceOutcome, MemoryStore, NodeId, RecallQuery,
    WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;
use rusqlite::{Connection, OpenFlags};
use std::fmt::Write;

fn ctx() -> WriteCtx {
    WriteCtx::new(WorkspaceId::default(), Actor::Agent)
}

fn raw(s: &Scratch) -> Connection {
    Connection::open_with_flags(s.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Every new read after the purge commits: no lexical or fuzzy hit, and the row is a ghost.
fn assert_revoked(store: &SqliteStore, s: &Scratch, id: &str, word: &str) {
    for fuzzy in [false, true] {
        let hits = store
            .recall(&RecallQuery {
                query: word.into(),
                hops: 0,
                fuzzy,
                ..Default::default()
            })
            .unwrap();
        assert!(hits.iter().all(|hit| hit.node.id != id), "{fuzzy}");
    }
    let fresh = raw(s);
    let (lifecycle, text): (String, String) = fresh
        .query_row("SELECT lifecycle, text FROM node WHERE id=?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((lifecycle.as_str(), text.as_str()), ("purged", ""));
}

/// SS-03: a reader still on the WAL blocks the TRUNCATE checkpoints. The purge is revoked at
/// once, the cleanup is reported incomplete (`busy`, not an error), and the explicit optimize
/// completes it once the reader is gone. Other modes neither revoke nor clean.
#[test]
fn a_wal_reader_leaves_cleanup_incomplete_while_the_purge_is_revoked() {
    let s = Scratch::new("ss03-purge", "wal-reader");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let victim = common::created(&store, "Fact", "PurgeCanaryQuokka private");
    let other = common::created(&store, "Fact", "kept note");
    let reader = raw(&s);
    reader.execute_batch("BEGIN").unwrap();
    assert_eq!(
        count(
            &reader,
            "SELECT count(*) FROM node WHERE lifecycle='active'"
        ),
        2
    );
    let outcome = store
        .forget(
            &NodeId::parse(&victim.id).unwrap(),
            ForgetMode::Purge,
            &ctx(),
        )
        .unwrap();
    assert!(outcome.revoked);
    assert!(!outcome.physically_cleaned);
    assert!(
        outcome.cleanup.as_deref().is_some_and(
            |c| c.starts_with("database/WAL: busy; free-page residue until VACUUM; index:")
        ),
        "{:?}",
        outcome.cleanup
    );
    assert_revoked(&store, &s, &victim.id, "PurgeCanaryQuokka");
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    assert_eq!(store.optimize_fts().unwrap(), MaintenanceOutcome::Ok);
    let wal = std::fs::metadata(format!("{}-wal", s.db().display())).map_or(0, |m| m.len());
    assert_eq!(wal, 0, "the retried cleanup truncates the WAL");
    let kept = store
        .forget(
            &NodeId::parse(&other.id).unwrap(),
            ForgetMode::Tombstone,
            &ctx(),
        )
        .unwrap();
    assert!(!kept.revoked && !kept.physically_cleaned && kept.cleanup.is_none());
}

/// Occurrences of `needle` in the file at `path` (0 for a missing file).
fn occurrences(path: &std::path::Path, needle: &[u8]) -> usize {
    std::fs::read(path).map_or(0, |bytes| {
        bytes.windows(needle.len()).filter(|w| *w == needle).count()
    })
}

/// SS-03 (review a-1): the post-commit hygiene merges the indexes and truncates the WAL but
/// reclaims no freed page, and under `secure_delete=FAST` a freed overflow page keeps its
/// bytes: a purged body whose stored frame spans overflow pages is still in the checkpointed
/// file, so the purge reports `physically_cleaned: false` with the free-page residue as its
/// detail rather than a success. The VACUUM path (`kaban doctor --optimize --vacuum`: the
/// vacuum, then the optimize's checkpoint) removes the bytes.
#[test]
fn a_purge_reports_the_free_page_residue_until_a_vacuum_removes_the_bytes() {
    // Arrange: a body whose encoded frame is long enough for overflow pages (hex of a
    // pseudo-random sequence: the codec halves it at best); the residue searched for is a
    // slice of the frame as stored, never the plain text
    const WORD: &str = "PurgeCanaryQuokka7731";
    let s = Scratch::new("ss03-purge", "canary-residue");
    let wal = format!("{}-wal", s.db().display());
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let word = WORD;
    let mut body = format!("{word} ");
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    while body.len() < 64 << 10 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        write!(body, "{state:016x}").unwrap();
    }
    let victim = common::created(&store, "Fact", &body);
    let stored: Vec<u8> = raw(&s)
        .query_row("SELECT text FROM node WHERE id = ?1", [&victim.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(stored.len() > 16 << 10, "{} stored bytes", stored.len());
    let canary = &stored[stored.len() / 2..stored.len() / 2 + 64];

    // Act: the purge, hygiene included
    let outcome = store
        .forget(
            &NodeId::parse(&victim.id).unwrap(),
            ForgetMode::Purge,
            &ctx(),
        )
        .unwrap();

    // Assert: revoked, not cleaned, the residue named; the bytes are still in the file
    assert!(outcome.revoked);
    assert!(!outcome.physically_cleaned);
    assert!(
        outcome
            .cleanup
            .as_deref()
            .is_some_and(|c| c.starts_with("database/WAL: free-page residue until VACUUM; index:")),
        "{:?}",
        outcome.cleanup
    );
    assert_revoked(&store, &s, &victim.id, word);
    assert_eq!(occurrences(std::path::Path::new(&wal), canary), 0);
    assert!(
        occurrences(&s.db(), canary) > 0,
        "the freed pages keep the body until a VACUUM"
    );

    // Act: the VACUUM path, then the checkpoint that folds it into the file
    assert_eq!(store.vacuum().unwrap(), MaintenanceOutcome::Ok);
    assert_eq!(store.optimize_fts().unwrap(), MaintenanceOutcome::Ok);

    // Assert: not one byte of the body in the file or its WAL
    assert_eq!(occurrences(&s.db(), canary), 0);
    assert_eq!(occurrences(std::path::Path::new(&wal), canary), 0);
}
