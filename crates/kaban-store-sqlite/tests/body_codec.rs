//! Spec 05 SS-17 through the store's own entry points: once a database is past the FTS5
//! retirement (migration 0012 follows it), bodies are written as tagged zstd BLOBs after
//! policy gating and every read surface (`get`, the index rows, the serialized records,
//! hashes, rescan, promote, import) decodes the exact original bytes; bodies an older binary
//! stored plain are re-encoded in place by migration 0013; a corrupt or
//! foreign body fails the read by name instead of yielding bytes; a database bound to a
//! dictionary this binary does not ship refuses to open with guidance; a purge erases an
//! encoded body like a plain one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
#![allow(clippy::unnecessary_wraps)] // the fake admission has the `IndexAdmission` shape

mod common;

use common::{Scratch, created, ctx, nid};
use kaban_core::ontology::{Ns, Qualified};
use kaban_core::text_index::{Canonical, Rows};
use kaban_core::{
    ChangeLogStore, ForgetMode, IngestStore, MemoryStore, NewNode, NodeId, Op, RedactedStrings,
    RememberOutcome, RescanAction, SchemaStore, SyncStore, WorkspaceId, WriteBatch,
};
use kaban_store_sqlite::index_rows::IndexRows;
use kaban_store_sqlite::{LATEST_SCHEMA_VERSION, SqliteStore};
use rusqlite::Connection;
use rusqlite::types::Value;
use std::fmt::Write;
use std::path::Path;

/// The tag an encoded body starts with (`body_codec.rs`).
const MAGIC: [u8; 3] = [0, b'K', b'Z'];

fn raw(s: &Scratch) -> Connection {
    Connection::open(s.db()).unwrap()
}

/// A raw connection that can rewrite a row: the write-stamp triggers (SS-26) call the
/// connection-local `kaban_writer()`, so the test binds a stand-in.
fn raw_writer(s: &Scratch) -> Connection {
    let conn = raw(s);
    conn.create_scalar_function(
        "kaban_writer",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |_| Ok("test".to_owned()),
    )
    .unwrap();
    conn
}

/// The `node.text` column as SQLite holds it.
fn stored(s: &Scratch, id: &str) -> Value {
    raw(s)
        .query_row("SELECT text FROM node WHERE id = ?1", [id], |r| {
            r.get::<_, Value>(0)
        })
        .unwrap()
}

fn is_encoded(value: &Value) -> bool {
    matches!(value, Value::Blob(b) if b.starts_with(&MAGIC))
}

/// Whether a tagged body carries a zstd frame (format 1) rather than the body itself (format 0).
fn is_compressed(value: &Value) -> bool {
    matches!(value, Value::Blob(b) if b.starts_with(&MAGIC) && b.get(3) == Some(&1))
}

fn schema_version(s: &Scratch) -> u32 {
    raw(s)
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap()
}

/// A store at the one schema: every body it writes is a tagged BLOB.
fn compressed(s: &Scratch) -> SqliteStore {
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    assert_eq!(schema_version(s), LATEST_SCHEMA_VERSION);
    store
}

/// A Markdown-like body of about `n` bytes carrying `word`.
fn body(word: &str, n: usize) -> String {
    let mut text = format!("## {word}\n\n");
    let line = format!(
        "The checkout keeps {word} in `pricing.rs`; caf\u{e9} retries are idempotent (ADR 0007).\n"
    );
    while text.len() < n {
        text.push_str(&line);
    }
    text
}

/// The error and every source below it, one line; a backend failure's driver error is
/// reached through its `BackendSource` holder (its Display is content-free by design).
fn chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        let e = e
            .downcast_ref::<kaban_core::BackendSource>()
            .map_or(e, |holder| holder.driver());
        out.push_str(": ");
        out.push_str(&e.to_string());
        cur = e.source();
    }
    out
}

fn occurrences(path: &Path, needle: &[u8]) -> usize {
    std::fs::read(path).map_or(0, |bytes| {
        bytes.windows(needle.len()).filter(|w| *w == needle).count()
    })
}

fn index_texts(s: &Scratch) -> (Vec<(String, String)>, Canonical) {
    let rows = IndexRows::open(&s.db()).unwrap();
    let mut seen = Vec::new();
    let at = rows
        .active(&mut |row| {
            seen.push((row.id.clone(), row.text.clone()));
            Ok(())
        })
        .unwrap();
    (seen, at)
}

/// Rejection first: a body whose payload no longer decodes, and one whose tag names a
/// dictionary this binary does not ship, fail every read by name. No read returns bytes.
#[test]
fn a_corrupt_or_foreign_encoded_body_fails_the_read_explicitly_never_as_garbage() {
    let s = Scratch::new("ss17-body", "corrupt");
    let store = compressed(&s);
    let victim = created(&store, "Fact", &body("quokka", 3000));
    let witness = created(&store, "Fact", &body("wombat", 3000));
    let Value::Blob(mut bytes) = stored(&s, &victim.id) else {
        panic!("encoded on write");
    };
    assert!(bytes.starts_with(&MAGIC));

    // a flipped run inside the frame
    for b in &mut bytes[24..40] {
        *b ^= 0x5A;
    }
    raw_writer(&s)
        .execute(
            "UPDATE node SET text = ?1 WHERE id = ?2",
            rusqlite::params![bytes, victim.id],
        )
        .unwrap();
    let err = store.get(&nid(&victim.id), false).unwrap_err();
    assert!(
        chain(&err).contains("corrupt body payload"),
        "{}",
        chain(&err)
    );
    let rows = IndexRows::open(&s.db()).unwrap();
    let err = rows.active(&mut |_| Ok(())).unwrap_err();
    assert!(
        chain(&err).contains("corrupt body payload"),
        "{}",
        chain(&err)
    );
    let err = rows.current(std::slice::from_ref(&victim.id)).unwrap_err();
    assert!(
        chain(&err).contains("corrupt body payload"),
        "{}",
        chain(&err)
    );
    // the witness beside it still reads
    assert_eq!(
        store
            .get(&nid(&witness.id), false)
            .unwrap_or_else(|e| panic!("{}", chain(&e)))
            .unwrap()
            .text,
        witness.text
    );

    // a tag naming dictionary 2
    let Value::Blob(mut bytes) = stored(&s, &witness.id) else {
        panic!("encoded on write");
    };
    bytes[MAGIC.len() + 1] = 2;
    raw_writer(&s)
        .execute(
            "UPDATE node SET text = ?1 WHERE id = ?2",
            rusqlite::params![bytes, witness.id],
        )
        .unwrap();
    let err = store.get(&nid(&witness.id), false).unwrap_err();
    assert!(
        chain(&err).contains("unknown body dictionary 2"),
        "{}",
        chain(&err)
    );
}

/// Rejection (T7 review t7-1): a `BLOB` is always an encoded body, so a BLOB too short to
/// carry the tag, or carrying none, fails every read by name; it is never returned as a
/// (possibly empty) body. A row beside it still reads.
#[test]
fn a_blob_without_a_whole_tag_fails_the_read_explicitly_never_as_an_empty_body() {
    let s = Scratch::new("ss17-body", "short-blob");
    let store = compressed(&s);
    let victim = created(&store, "Fact", &body("quokka", 3000));
    let neighbour = created(&store, "Fact", "short and plain");
    for blob in [
        Vec::new(),
        vec![0],
        vec![0, b'K'],
        b"plain text stored as a blob".to_vec(),
    ] {
        raw_writer(&s)
            .execute(
                "UPDATE node SET text = ?1 WHERE id = ?2",
                rusqlite::params![blob, victim.id],
            )
            .unwrap();
        assert!(matches!(stored(&s, &victim.id), Value::Blob(ref b) if *b == blob));
        let err = store.get(&nid(&victim.id), false).unwrap_err();
        assert!(
            chain(&err).contains("corrupt body payload"),
            "{} bytes through get: {}",
            blob.len(),
            chain(&err)
        );
        let rows = IndexRows::open(&s.db()).unwrap();
        let err = rows.current(std::slice::from_ref(&victim.id)).unwrap_err();
        assert!(
            chain(&err).contains("corrupt body payload"),
            "{} bytes through the index rows: {}",
            blob.len(),
            chain(&err)
        );
    }
    assert_eq!(
        store.get(&nid(&neighbour.id), false).unwrap().unwrap().text,
        "short and plain"
    );
}

/// T8c review t8c-3: a short body zstd does not pay for is stored plain behind the tag
/// (format 0) with its length and checksum, so a stored blob cut back to the tag (the
/// reviewer's `00 4b 5a 00 00 78` cut to five bytes), cut inside its payload, or altered to
/// another valid UTF-8 body fails every read by name instead of reading as an empty, shorter
/// or different body.
#[test]
fn a_truncated_or_altered_plain_body_fails_the_read_explicitly_never_as_another_body() {
    let s = Scratch::new("ss17-body", "plain-framed");
    let store = compressed(&s);
    let victim = created(&store, "Fact", "x");
    let value = stored(&s, &victim.id);
    assert!(is_encoded(&value) && !is_compressed(&value), "{value:?}");
    let Value::Blob(bytes) = value else {
        panic!("encoded on write");
    };
    let mut altered = bytes.clone();
    *altered.last_mut().unwrap() = b'y';
    for (case, blob) in [
        ("cut to the tag", bytes[..MAGIC.len() + 2].to_vec()),
        ("cut inside the payload", bytes[..bytes.len() - 1].to_vec()),
        ("altered to valid UTF-8", altered),
    ] {
        raw_writer(&s)
            .execute(
                "UPDATE node SET text = ?1 WHERE id = ?2",
                rusqlite::params![blob, victim.id],
            )
            .unwrap();
        let err = store.get(&nid(&victim.id), false).unwrap_err();
        assert!(
            chain(&err).contains("corrupt body payload"),
            "{case} through get: {}",
            chain(&err)
        );
        let rows = IndexRows::open(&s.db()).unwrap();
        let err = rows.current(std::slice::from_ref(&victim.id)).unwrap_err();
        assert!(
            chain(&err).contains("corrupt body payload"),
            "{case} through the index rows: {}",
            chain(&err)
        );
    }
}

/// T7 review t7-2: the encoder honours the decoder's bound. A body above the 64 MiB the
/// decoder accepts is committed behind the tag uncompressed (format 0) through `apply_batch`
/// (and so through every writer), never as a frame the next read refuses; the body a byte
/// below the bound still compresses and round-trips.
#[test]
fn a_body_above_the_decode_bound_is_stored_uncompressed_and_round_trips() {
    const BOUND: usize = 64 << 20;
    let s = Scratch::new("ss17-body", "bound");
    let store = compressed(&s);
    let mut text = String::with_capacity(BOUND + 8);
    while text.len() <= BOUND {
        text.push_str("quokka one by the shore, ");
    }
    text.truncate(BOUND + 1);
    let mut within = text.clone();
    within.truncate(BOUND);
    for (word, body, compressed) in [("above", &text, false), ("within", &within, true)] {
        let id = NodeId::new();
        store
            .apply_batch(
                WriteBatch::new(
                    WorkspaceId::default(),
                    vec![Op::UpsertNode {
                        id: id.clone(),
                        node: NewNode {
                            kind: "Fact".into(),
                            text: body.clone(),
                            summary: Some(word.into()),
                            ..Default::default()
                        },
                    }],
                ),
                &ctx(),
            )
            .unwrap();
        let value = stored(&s, id.as_str());
        assert!(is_encoded(&value), "{word}");
        assert_eq!(is_compressed(&value), compressed, "{word}");
        let read = store
            .get(&id, false)
            .unwrap_or_else(|e| panic!("{word}: {}", chain(&e)))
            .unwrap();
        assert!(read.text == *body, "{word}: the body read back differs");
    }
}

/// After the retirement a body is stored as a tagged BLOB and every read surface returns the
/// exact original bytes: `get`, the index rows (the Tantivy projection's input, at the same
/// canonical position as the change log), the serialized records (the export), and the
/// content hash (a second `remember` of the same text deduplicates against the stored row).
#[test]
fn bodies_are_encoded_on_write_and_read_back_exactly_on_every_surface() {
    let s = Scratch::new("ss17-body", "surfaces");
    let store = compressed(&s);
    let texts = [
        body("quokka", 20_000),
        "caf\u{e9} \u{1F418} \u{2014} na\u{ef}ve".to_owned(),
        "x".to_owned(),
    ];
    let notes: Vec<_> = texts
        .iter()
        .map(|t| created(&store, "Decision", t))
        .collect();
    for (note, text) in notes.iter().zip(&texts) {
        assert!(
            is_encoded(&stored(&s, &note.id)),
            "{} bytes: every body is a tagged BLOB",
            text.len()
        );
        assert_eq!(
            &store.get(&nid(&note.id), false).unwrap().unwrap().text,
            text
        );
        store.mark_serialized(&nid(&note.id), note.rev).unwrap();
    }
    let (rows, at) = index_texts(&s);
    assert_eq!(
        rows,
        notes
            .iter()
            .map(|n| (n.id.clone(), n.text.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(at.seq, store.changes_head().unwrap());
    let exported = store
        .serialized_records("default", &["Decision"], 10)
        .unwrap();
    let mut exported: Vec<String> = exported.into_iter().map(|n| n.text).collect();
    exported.sort();
    let mut expected = texts.to_vec();
    expected.sort();
    assert_eq!(exported, expected);
    for text in &texts {
        let again = store
            .remember(
                NewNode {
                    kind: "Decision".into(),
                    text: text.clone(),
                    ..Default::default()
                },
                &[],
                &ctx(),
            )
            .unwrap();
        assert!(
            matches!(again, RememberOutcome::Existing { .. }),
            "the hash is over the decoded body"
        );
    }
}

/// A database whose recorded dictionary is not the one this binary ships refuses to open,
/// naming the dictionary and what to do, rather than serving bodies it cannot decode.
#[test]
fn an_unknown_dictionary_hash_refuses_the_open_with_guidance() {
    let s = Scratch::new("ss17-body", "foreign-dictionary");
    let store = compressed(&s);
    created(&store, "Fact", &body("quokka", 2000));
    store
        .set_index_meta("body_dictionary_sha256", &"f".repeat(64))
        .unwrap();
    drop(store);

    let err = SqliteStore::open(&s.db(), &s.keys()).unwrap_err();

    let text = chain(&err);
    assert!(text.contains("dictionary"), "{text}");
    assert!(
        text.contains("kaban that wrote it"),
        "guidance names the way out: {text}"
    );

    // T7 review t7-4: every constructor that yields a connection able to read or write
    // bodies runs the same guard: the hook open (which writes), the diagnostic and
    // existing-key opens, and the index rows' own read-only connection
    let actor = kaban_core::ActorId::default();
    let refusals = [
        (
            "open_for_hook",
            SqliteStore::open_for_hook(&s.db(), &s.keys(), &actor).map(drop),
        ),
        (
            "open_diagnostic",
            SqliteStore::open_diagnostic(&s.db(), &s.keys(), &actor).map(drop),
        ),
        (
            "open_existing_key",
            SqliteStore::open_existing_key(&s.db(), &s.keys(), &actor).map(drop),
        ),
        ("IndexRows::open", IndexRows::open(&s.db()).map(drop)),
    ];
    for (constructor, outcome) in refusals {
        let err = outcome.expect_err(constructor);
        let found = chain(&err);
        assert!(
            found.contains("dictionary") && found.contains("kaban that wrote it"),
            "{constructor}: {found}"
        );
    }
    // T8c review t8c-6: the recorded binding is judged, never replaced by an open
    let recorded: String = raw(&s)
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'body_dictionary_sha256'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recorded, "f".repeat(64), "the refusals rewrote nothing");
}

/// The policy rescan rewrites a masked body in place: the rewrite is encoded and reads back
/// masked; the rows it keeps are untouched.
#[test]
fn a_rescan_redaction_rewrites_the_body_encoded_and_reads_back_masked() {
    let s = Scratch::new("ss17-body", "rescan");
    let store = compressed(&s);
    let leaky = created(&store, "Fact", &body("hunter2", 3000));
    let clean = created(&store, "Fact", &body("quokka", 3000));

    let report = store
        .rescan("v2", 100, &mut |n| {
            if n.text.contains("hunter2") {
                RescanAction::Redact {
                    strings: Box::new(RedactedStrings {
                        text: n.text.replace("hunter2", "[redacted]"),
                        summary: n.summary.clone(),
                        tags: n.tags.clone(),
                        props: n.props.clone(),
                        path_scope: n.path_scope.clone(),
                        session_id: n.session_id.clone(),
                        agent_id: n.agent_id.clone(),
                        repo: n.repo.clone(),
                        commit_sha: n.commit_sha.clone(),
                        accepted_commit: n.accepted_commit.clone(),
                        dup_key: n.dup_key.clone(),
                    }),
                    audit: vec![],
                }
            } else {
                RescanAction::Keep
            }
        })
        .unwrap();

    assert_eq!((report.redacted, report.kept), (1, 1));
    assert!(is_encoded(&stored(&s, &leaky.id)));
    let masked = store.get(&nid(&leaky.id), false).unwrap().unwrap();
    assert!(!masked.text.contains("hunter2"));
    assert_eq!(
        masked.text.matches("[redacted]").count(),
        body("hunter2", 3000).matches("hunter2").count()
    );
    assert_eq!(
        store.get(&nid(&clean.id), false).unwrap().unwrap().text,
        clean.text
    );
}

/// `promote_kind` recomputes every content hash of the group from its body: over encoded
/// bodies the new hash is the hash of the decoded text under the new kind.
#[test]
fn promote_recomputes_content_hashes_from_decoded_bodies() {
    let s = Scratch::new("ss17-body", "promote");
    let store = compressed(&s);
    let text = body("quokka supplies the widgets", 2000);
    let vendor = created(&store, "Vendor", &text);
    assert!(is_encoded(&stored(&s, &vendor.id)));
    store
        .schema_extend(kaban_core::TypeDef {
            kind: "label".into(),
            name: "infra:Vendor".into(),
            ns: None,
            description: "a supplier".into(),
            from_kinds: vec![],
            to_kinds: vec![],
            directed: true,
            props_schema: None,
            builtin: false,
            declaration: kaban_core::TypeDeclaration::default(),
        })
        .unwrap();

    let report = store
        .promote_kind(
            &WorkspaceId::default(),
            &vendor.kind,
            &Qualified {
                ns: Ns::parse("infra").unwrap(),
                name: "Vendor".into(),
            },
            &kaban_core::Actor::HumanCli,
        )
        .unwrap();

    assert_eq!(report.to, "infra:Vendor");
    let promoted = store.get(&nid(&vendor.id), false).unwrap().unwrap();
    assert_eq!(promoted.kind, "infra:Vendor");
    assert_eq!(promoted.text, text);
    // `nodes.rs`: sha256 over `workspace \u{1} kind \u{1} trimmed text`
    assert_eq!(
        promoted.content_hash,
        kaban_policy::sha256_hex(
            format!("default\u{1}infra:Vendor\u{1}{}", text.trim()).as_bytes()
        )
    );
}

/// The SS-12 purge canary over an encoded body: the ghost row keeps no body, and after the
/// VACUUM path not one byte of the stored frame (nor of the plaintext, which never touched
/// the file) remains in the database or its WAL.
#[test]
fn a_purge_erases_an_encoded_body_from_the_file() {
    const CANARY: &[u8] = b"PurgeCanaryQuokka7731";
    let s = Scratch::new("ss17-body", "purge");
    let wal = s.db().with_extension("db-wal");
    let store = compressed(&s);
    let word = std::str::from_utf8(CANARY).unwrap();
    // a body long enough for overflow pages whose frame is not a few repeated bytes
    let mut text = String::new();
    for i in 0..800_u64 {
        let _ = writeln!(
            text,
            "{word} line {i:04} at {:x}",
            (i * 2_654_435_761) % 0xFFFF_FFFF
        );
    }
    let victim = created(&store, "Fact", &text);
    let Value::Blob(frame) = stored(&s, &victim.id) else {
        panic!("encoded on write");
    };
    assert!(frame.len() > 1024, "{} bytes", frame.len());
    let needle = &frame[MAGIC.len() + 2 + 16..MAGIC.len() + 2 + 48];
    assert!(
        occurrences(&s.db(), needle) + occurrences(&wal, needle) > 0,
        "the frame is in the file before the purge"
    );
    assert_eq!(occurrences(&s.db(), CANARY) + occurrences(&wal, CANARY), 0);

    let outcome = store
        .forget(&nid(&victim.id), ForgetMode::Purge, &ctx())
        .unwrap();

    assert!(outcome.revoked);
    let (lifecycle, text): (String, Value) = raw(&s)
        .query_row(
            "SELECT lifecycle, text FROM node WHERE id = ?1",
            [&victim.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (lifecycle.as_str(), text),
        ("purged", Value::Text(String::new()))
    );
    assert_eq!(store.vacuum().unwrap(), kaban_core::MaintenanceOutcome::Ok);
    raw(&s)
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    assert_eq!(occurrences(&s.db(), needle), 0, "the frame left the file");
    assert_eq!(occurrences(&wal, needle), 0, "and the WAL");
    assert_eq!(occurrences(&s.db(), CANARY) + occurrences(&wal, CANARY), 0);
}
