//! The body codec (spec 05 SS-17): per-record zstd over `node.text` ("the body") with the one
//! shipped dictionary, inside the SQLite adapter. No codec trait, no user-trained dictionary.
//!
//! A stored body is a tagged `BLOB` (every row of a database at the one schema, spec 05
//! OWN-M/OWN-P); a `TEXT` value is plaintext (a fixture written raw) and passes through:
//!
//! ```text
//!   MAGIC (0x00 'K' 'Z') | format version (u8) | dictionary id (u8) | payload
//!   format 0: length (u32 LE) | crc32 (u32 LE) | the body itself   (the lossless fallback:
//!             zstd did not pay, or the body is above the decode bound)
//!   format 1: the payload is one zstd frame with a checksum, compressed at level 3 with the
//!             dictionary the id names; the frame carries the content size
//! ```
//!
//! Every format carries the decoded length and a checksum (T8c review t8c-3): a payload cut
//! short or altered in place, even to valid UTF-8, fails the read instead of reading as a
//! shorter or different body. [`decode`] returns the exact original bytes for a format 0 body
//! and a format 1 body under the shipped dictionary, and refuses everything else by name
//! ([`CodecError`]): a `BLOB` without a whole tag, an unknown format, an unknown dictionary
//! id, a payload that does not verify whole (truncation, a flipped byte, a checksum or
//! length mismatch) never yields bytes; a `TEXT` value is plaintext and passes through
//! undecoded. Every read surface
//! decodes through [`read_column`] (the `Node` rows, the
//! index rows, the hash recomputations of promote and rescan, the span materialisation) or
//! through the connection-local SQL function `kaban_body()` ([`register`]) where a projection
//! concatenates the body in SQL; every write goes through [`Writer`], which encodes every
//! body after policy gating (the gate sees the plaintext the caller hands in): format 1 when
//! the frame is the smaller one, format 0 otherwise, so a body column is always a tagged
//! `BLOB`. The schema creation records the codec and the dictionary in `index_meta`
//! ([`record`]); a database recorded under another dictionary refuses to open ([`verify`]:
//! the recorded binding is validated at every open, never replaced).
//!
//! The dictionary is a build-time constant ([`DICTIONARY`], `body_codec/kaban-bodies-v1.zdict`,
//! 65,536 bytes, sha256 [`DICTIONARY_SHA256`]), trained offline once by the ignored test
//! `tests::train_dictionary` over the repository's own Markdown and text corpus at commit
//! c747367 (`fixtures/` and `docs/`, every `*.md` and `*.txt`, sorted by path, split into
//! paragraphs of 64 bytes or more; 6,032 samples from 313 files). Before the first
//! import, embedded machine paths were normalized with equal-length replacements;
//! the checksum below identifies the shipped portable bytes. Training recipe:
//!
//! ```text
//! KABAN_BODY_DICTIONARY_CORPUS=fixtures:docs KABAN_BODY_DICTIONARY_SIZE=65536 \
//! KABAN_BODY_DICTIONARY_OUT=crates/kaban-store-sqlite/src/body_codec/kaban-bodies-v1.zdict \
//!   cargo nextest run -p kaban-store-sqlite --lib --offline --run-ignored only --no-capture \
//!     -E 'test(body_codec::tests::train_dictionary)'
//! ```
//!
//! A new dictionary is a new dictionary id and a new migration, never a replacement of this
//! file: the id in every stored body and `index_meta.body_dictionary_sha256` bind a database
//! to the dictionary that encoded it.
//!
//! Measured on the SS-13a storage sample (822 Markdown files, 11.1 MB; 802 documents, 8,330
//! chunks, 15,358 rows; `../specs/kaban/specs/00-project/evidence/s05-t7-record.md`): bodies 10,122,075 -> 4,673,450
//! bytes (-53.8%; 5,869,574 without the dictionary), the compact database 47,828,992 ->
//! 40,284,160 bytes (-15.8%) against the same records stored plain at the same schema; the
//! release binary grew 786,464 bytes (libzstd, the dictionary, this module), both size gates held.

use crate::schema::recorded_version;
use crate::sql_error::SqlResultExt;
use kaban_core::{Result, StoreError};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::{ToSqlOutput, Value, ValueRef};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::sync::LazyLock;
use zstd::bulk::{Compressor, Decompressor};
use zstd::dict::{DecoderDictionary, EncoderDictionary};
use zstd::zstd_safe::{CParameter, get_frame_content_size};

/// The tag's magic: a leading NUL no UTF-8 body starts with, then `KZ`.
pub(crate) const MAGIC: [u8; 3] = [0, b'K', b'Z'];
/// The format this binary writes (1: one zstd frame under the shipped dictionary).
pub(crate) const FORMAT_VERSION: u8 = 1;
/// The id of the shipped dictionary in every encoded body's tag.
pub(crate) const DICTIONARY_ID: u8 = 1;
/// Magic, format version and dictionary id.
pub(crate) const TAG_LEN: usize = MAGIC.len() + 2;
/// The shipped synthetic dictionary (see the module doc for its training).
pub(crate) const DICTIONARY: &[u8] = include_bytes!("body_codec/kaban-bodies-v1.zdict");
/// `sha256(DICTIONARY)`, the identity `index_meta` records beside the codec version.
pub(crate) const DICTIONARY_SHA256: &str =
    "924faebb77f72ca199aedaf272d6474943e8549aff6ed2b392bc02ab082f5213";
/// The codec's name in `index_meta.body_codec` and a pack manifest's `versions.codec`.
pub(crate) const CODEC_ID: &str = "zstd-dict-v1";
/// The `index_meta` key of [`CODEC_ID`], written with the schema ([`record`]).
pub(crate) const CODEC_KEY: &str = "body_codec";
/// The `index_meta` key of [`DICTIONARY_SHA256`], written with the schema ([`record`]).
pub(crate) const DICTIONARY_KEY: &str = "body_dictionary_sha256";
/// The length and checksum a format 0 payload starts with.
const PLAIN_HEADER: usize = 8;
/// The connection-local SQL function that decodes a body column (`kaban_body(n.text)`).
const SQL_FUNCTION: &str = "kaban_body";
/// The zstd level (the SS-13a measurement's).
const LEVEL: i32 = 3;
/// The body bound, on both sides of the codec: [`Writer`] stores a body above it plain and
/// [`decode`] refuses a frame claiming more unread, so no committed row is undecodable here
/// (the store API itself caps no body; the transports do, at 1 MiB, far below it).
const MAX_BODY: u64 = 64 << 20;

static ENCODER: LazyLock<EncoderDictionary<'static>> =
    LazyLock::new(|| EncoderDictionary::copy(DICTIONARY, LEVEL));
static DECODER: LazyLock<DecoderDictionary<'static>> =
    LazyLock::new(|| DecoderDictionary::copy(DICTIONARY));

/// Why a stored body could not be decoded. Messages name the tag's values only, never bytes.
#[derive(Debug)]
pub(crate) enum CodecError {
    /// The tag names a format this binary does not know.
    UnknownFormat(u8),
    /// The tag names a dictionary this binary does not ship.
    UnknownDictionary(u8),
    /// The payload is not one whole frame, or one whole plain body, of the length and
    /// checksum its header claims.
    Corrupt(std::io::Error),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFormat(v) => write!(f, "unknown body format {v}"),
            Self::UnknownDictionary(id) => write!(f, "unknown body dictionary {id}"),
            Self::Corrupt(_) => f.write_str("corrupt body payload"),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Corrupt(e) => Some(e),
            Self::UnknownFormat(_) | Self::UnknownDictionary(_) => None,
        }
    }
}

fn corrupt(what: &'static str) -> CodecError {
    CodecError::Corrupt(std::io::Error::new(std::io::ErrorKind::InvalidData, what))
}

/// Encode `plain` as a tagged body: format 1 under the shipped dictionary when the frame is
/// the smaller of the two, else (or should the compressor fail: an allocation failure is the
/// only way) format 0, the body behind its length and checksum ([`plain_framed`]), so the
/// result always decodes to `plain` and encoding never fails.
pub(crate) fn encode(plain: &[u8]) -> Vec<u8> {
    match compress(plain) {
        Ok(frame) if frame.len() < PLAIN_HEADER + plain.len() => {
            let mut stored = Vec::with_capacity(TAG_LEN + frame.len());
            stored.extend_from_slice(&MAGIC);
            stored.push(FORMAT_VERSION);
            stored.push(DICTIONARY_ID);
            stored.extend_from_slice(&frame);
            stored
        }
        _ => plain_framed(plain),
    }
}

/// `plain` as a format 0 body: the tag, the body's length and crc32, the body itself.
fn plain_framed(plain: &[u8]) -> Vec<u8> {
    let mut stored = Vec::with_capacity(TAG_LEN + PLAIN_HEADER + plain.len());
    stored.extend_from_slice(&MAGIC);
    stored.extend_from_slice(&[0, 0]);
    // a body longer than u32 cannot be a SQLite value (2 GiB at most); saturating keeps the
    // framing total and such a body unreadable rather than misread
    let length = u32::try_from(plain.len()).unwrap_or(u32::MAX);
    stored.extend_from_slice(&length.to_le_bytes());
    stored.extend_from_slice(&crc32fast::hash(plain).to_le_bytes());
    stored.extend_from_slice(plain);
    stored
}

/// The body of a format 0 payload, verified against the length and checksum it starts with.
fn unframe_plain(payload: &[u8]) -> std::result::Result<Vec<u8>, CodecError> {
    let Some((header, body)) = payload.split_at_checked(PLAIN_HEADER) else {
        return Err(corrupt("plain body shorter than its length and checksum"));
    };
    let (length, checksum) = header.split_at(4);
    let length = u32::from_le_bytes(length.try_into().unwrap_or([0; 4]));
    let checksum = u32::from_le_bytes(checksum.try_into().unwrap_or([0; 4]));
    if u64::from(length) != body.len() as u64 {
        return Err(corrupt("plain body is not the length its header claims"));
    }
    if crc32fast::hash(body) != checksum {
        return Err(corrupt("plain body checksum mismatch"));
    }
    Ok(body.to_vec())
}

fn compress(plain: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut compressor = Compressor::with_prepared_dictionary(&ENCODER)?;
    compressor.set_parameter(CParameter::ChecksumFlag(true))?;
    compressor.compress(plain)
}

/// The exact original bytes of a stored `BLOB` body, decoded as its tag says. A `BLOB` is
/// always a tagged body (plaintext is stored as `TEXT`, which the read surfaces pass through
/// without calling this), so one without a whole tag is corrupt, never returned as bytes: a
/// truncation to zero, one or two bytes would otherwise read as a valid, shorter body (T7
/// review t7-1).
///
/// # Errors
/// [`CodecError`]: the format or dictionary is unknown, or the payload is corrupt.
pub(crate) fn decode(stored: &[u8]) -> std::result::Result<Vec<u8>, CodecError> {
    let Some(tagged) = stored.strip_prefix(&MAGIC) else {
        return Err(corrupt("blob without the body tag"));
    };
    let Some((&[format, dictionary], payload)) = tagged.split_at_checked(2) else {
        return Err(corrupt("tag shorter than its two bytes"));
    };
    match format {
        0 => unframe_plain(payload),
        FORMAT_VERSION if dictionary == DICTIONARY_ID => decompress(payload),
        FORMAT_VERSION => Err(CodecError::UnknownDictionary(dictionary)),
        other => Err(CodecError::UnknownFormat(other)),
    }
}

fn decompress(frame: &[u8]) -> std::result::Result<Vec<u8>, CodecError> {
    let size = match get_frame_content_size(frame) {
        Ok(Some(size)) if size <= MAX_BODY => size,
        Ok(Some(_)) => return Err(corrupt("content size above the body bound")),
        Ok(None) => return Err(corrupt("frame without a content size")),
        Err(_) => return Err(corrupt("unreadable frame header")),
    };
    let capacity = usize::try_from(size).map_err(|_| corrupt("content size above usize"))?;
    let mut decompressor =
        Decompressor::with_prepared_dictionary(&DECODER).map_err(CodecError::Corrupt)?;
    let body = decompressor
        .decompress(frame, capacity)
        .map_err(CodecError::Corrupt)?;
    if body.len() != capacity {
        return Err(corrupt("frame shorter than its content size"));
    }
    Ok(body)
}

/// Record the codec and its dictionary in `index_meta` when the schema is created
/// (`schema::Migrator`): the binding [`verify`] judges at every open. A binding already
/// recorded stands (T8c review t8c-6): it is validated, never replaced.
pub(crate) fn record(conn: &Connection) -> Result<()> {
    for (key, value) in [(CODEC_KEY, CODEC_ID), (DICTIONARY_KEY, DICTIONARY_SHA256)] {
        conn.execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING",
            params![key, value],
        )
        .sql()?;
    }
    Ok(())
}

/// Refuse a database whose recorded codec or dictionary is not the shipped one: its bodies
/// cannot be decoded here, and no open must serve them as if they could, nor write bodies
/// encoded under another dictionary beside them. The recorded binding is judged on every
/// open and never replaced (T8c review t8c-6). Run by every constructor that yields a
/// connection able to read or write bodies (the ordinary open once the schema is in place,
/// the hook, diagnostic and existing-key opens, the index rows' connection, the pack's
/// capture and check; T7 review t7-4) and by [`Writer`] itself, so a writer encodes only on
/// a connection the guard admitted. Returns the recorded schema version it admitted.
///
/// # Errors
/// `StoreError::Migration` at the recorded version, its source naming both dictionaries and
/// the way out; SQLite errors.
pub(crate) fn verify(conn: &Connection) -> Result<u32> {
    let version = recorded_version(conn)?;
    let meta = |key: &str| -> Result<Option<String>> {
        conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .sql()
    };
    let codec = meta(CODEC_KEY)?.unwrap_or_default();
    let dictionary = meta(DICTIONARY_KEY)?.unwrap_or_default();
    if codec == CODEC_ID && dictionary == DICTIONARY_SHA256 {
        return Ok(version);
    }
    Err(StoreError::Migration {
        from: version,
        to: version,
        source: format!(
            "the bodies of this database are encoded with body codec '{codec}' and dictionary \
             '{dictionary}', not the shipped '{CODEC_ID}' with '{DICTIONARY_SHA256}': this kaban \
             cannot decode them; open the database with the kaban that wrote it, or export it \
             there and re-ingest here"
        )
        .into(),
    })
}

/// Register `kaban_body(x)` on `conn`: the decoded text of a body column (a `TEXT` value is
/// itself, a `BLOB` is decoded, `NULL` stays `NULL`; anything else, or a body [`decode`]
/// refuses, fails the statement). For the one place a body is read inside SQL, the ontology's
/// summary projection (`ident::summary_projection_over`); every other read is [`read_column`].
///
/// # Errors
/// SQLite errors.
pub(crate) fn register(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        SQL_FUNCTION,
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| match ctx.get_raw(0) {
            ValueRef::Null => Ok(Value::Null),
            ValueRef::Text(bytes) => Ok(Value::Text(
                std::str::from_utf8(bytes)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?
                    .to_owned(),
            )),
            ValueRef::Blob(bytes) => {
                let plain =
                    decode(bytes).map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                String::from_utf8(plain)
                    .map(Value::Text)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))
            }
            other => Err(rusqlite::Error::InvalidFunctionParameterType(
                0,
                other.data_type(),
            )),
        },
    )
    .sql()
}

/// The decoded body in column `idx` of `row`: a `BLOB` is decoded, anything else is read as
/// the driver reads a `String` (its strict UTF-8 decoding and column-type errors unchanged).
/// A body the codec refuses is a conversion failure with the [`CodecError`] as its source
/// (the row is not returned), like `rows.rs`'s JSON columns.
///
/// # Errors
/// `rusqlite::Error::FromSqlConversionFailure` with the codec's or UTF-8 error as source for
/// a blob; the driver's own errors for a text column.
pub(crate) fn read_column(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<String> {
    let failed = |e: Box<dyn std::error::Error + Send + Sync>| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Blob, e)
    };
    match row.get_ref(idx)? {
        ValueRef::Blob(bytes) => {
            let plain = decode(bytes).map_err(|e| failed(Box::new(e)))?;
            String::from_utf8(plain).map_err(|e| failed(Box::new(e)))
        }
        _ => row.get(idx),
    }
}

/// The writer of a body column: every body is bound as a tagged `BLOB`. Built inside the write
/// transaction through the dictionary guard ([`verify`]), so no writer ever encodes beside
/// bodies of another dictionary.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Writer;

impl Writer {
    /// # Errors
    /// [`verify`]'s refusal; SQLite errors.
    pub(crate) fn for_connection(conn: &Connection) -> Result<Self> {
        verify(conn)?;
        Ok(Self)
    }

    /// The value to bind for `text`: the tagged body, as one zstd frame when the body is within
    /// the bound [`decode`] accepts and the frame is the smaller of the two, else as format 0
    /// (the body behind its length and checksum, lossless). The bound is checked before
    /// encoding: a frame above it would commit as a row no read could decode (T7 review t7-2).
    #[allow(clippy::unused_self)] // the writer is the proof of [`verify`]: no body is encoded without one
    pub(crate) fn stored(self, text: &str) -> StoredBody {
        if u64::try_from(text.len()).is_ok_and(|len| len <= MAX_BODY) {
            StoredBody(encode(text.as_bytes()))
        } else {
            StoredBody(plain_framed(text.as_bytes()))
        }
    }
}

/// A body as [`Writer`] binds it: a tagged `BLOB`.
#[derive(Debug)]
pub(crate) struct StoredBody(Vec<u8>);

impl rusqlite::ToSql for StoredBody {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Blob(&self.0)))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)] // test code: failures are the assertion; the trainer reports its hash
mod tests {
    use super::{CodecError, DICTIONARY, DICTIONARY_ID, DICTIONARY_SHA256, decode, encode};

    /// The bodies the codec must carry unchanged: empty, short, Markdown-like, non-ASCII,
    /// and one of a mebibyte.
    fn bodies() -> Vec<Vec<u8>> {
        let mut long = String::new();
        while long.len() < 1 << 20 {
            long.push_str("## Decision 0007\n\nThe checkout keeps discounts in `pricing.rs`; retries are idempotent. ");
        }
        long.truncate(1 << 20);
        vec![
            Vec::new(),
            b"x".to_vec(),
            b"quokka one by the shore".to_vec(),
            "caf\u{e9} \u{1F418} \u{2014} na\u{ef}ve"
                .as_bytes()
                .to_vec(),
            long.into_bytes(),
        ]
    }

    #[test]
    fn every_body_round_trips_byte_for_byte() {
        for body in bodies() {
            let stored = encode(&body);
            assert_eq!(decode(&stored).unwrap(), body, "{} bytes", body.len());
        }
    }

    #[test]
    fn a_format_zero_body_decodes_as_its_framed_payload() {
        let tagged = super::plain_framed(b"still plain");
        assert_eq!(&tagged[..super::TAG_LEN], &[0, b'K', b'Z', 0, 0]);
        assert_eq!(decode(&tagged).unwrap(), b"still plain");
        // a body zstd does not pay for is stored this way by the writer
        let short = encode(b"x");
        assert_eq!(short[super::MAGIC.len()], 0, "{short:?}");
        assert_eq!(decode(&short).unwrap(), b"x");
    }

    /// T8c review t8c-3: a format 0 payload carries its length and checksum, so a truncation
    /// (the reviewer's `00 4b 5a 00 00 78` cut to the tag, the header cut short, the body cut
    /// short) and a corruption that stays valid UTF-8 are refused, never read as a shorter or
    /// different body.
    #[test]
    fn a_truncated_or_altered_format_zero_body_is_refused_not_returned() {
        let stored = encode(b"x");
        assert_eq!(stored[super::MAGIC.len()], 0, "format 0: {stored:?}");
        for cut in [super::TAG_LEN, super::TAG_LEN + 3, stored.len() - 1] {
            assert!(
                matches!(decode(&stored[..cut]), Err(CodecError::Corrupt(_))),
                "cut to {cut} bytes"
            );
        }
        let unframed = [0, b'K', b'Z', 0, 0, b'x'];
        assert!(matches!(decode(&unframed), Err(CodecError::Corrupt(_))));
        let mut altered = stored.clone();
        *altered.last_mut().unwrap() = b'y';
        assert!(matches!(decode(&altered), Err(CodecError::Corrupt(_))));
        let mut longer = stored;
        longer.push(b'y');
        assert!(matches!(decode(&longer), Err(CodecError::Corrupt(_))));
    }

    /// T7 review t7-1: a stored BLOB is always a tagged body, so one without a whole tag (no
    /// bytes, part of the magic, the magic alone, or untagged bytes that happen to be UTF-8)
    /// is corrupt, never returned as a body; plaintext passes through as `TEXT` only.
    #[test]
    fn a_blob_without_a_whole_tag_is_refused_not_returned() {
        let mut magic_only = super::MAGIC.to_vec();
        magic_only.push(super::FORMAT_VERSION);
        for stored in [
            &b""[..],
            &[0][..],
            &[0, b'K'][..],
            &super::MAGIC[..],
            &magic_only,
            b"plain text",
        ] {
            assert!(
                matches!(decode(stored), Err(CodecError::Corrupt(_))),
                "{stored:?}"
            );
        }
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        super::register(&conn).unwrap();
        let body = |value: rusqlite::types::Value| -> rusqlite::Result<Option<String>> {
            conn.query_row("SELECT kaban_body(?1)", [value], |r| r.get(0))
        };
        for blob in [Vec::new(), vec![0], vec![0, b'K'], b"plain text".to_vec()] {
            let err = body(rusqlite::types::Value::Blob(blob.clone())).unwrap_err();
            assert!(
                err.to_string().contains("corrupt body payload"),
                "{blob:?}: {err}"
            );
        }
        assert_eq!(
            body(rusqlite::types::Value::Text("plain text".into())).unwrap(),
            Some("plain text".into())
        );
        assert_eq!(body(rusqlite::types::Value::Null).unwrap(), None);
    }

    #[test]
    fn an_unknown_dictionary_is_refused() {
        // long enough for the frame to pay (a short body is stored plain, format 0)
        let mut stored = encode("quokka one by the shore ".repeat(40).as_bytes());
        assert_eq!(stored[super::MAGIC.len()], super::FORMAT_VERSION);
        stored[super::MAGIC.len() + 1] = DICTIONARY_ID + 6;
        assert!(matches!(
            decode(&stored),
            Err(CodecError::UnknownDictionary(id)) if id == DICTIONARY_ID + 6
        ));
    }

    #[test]
    fn an_unknown_format_is_refused() {
        let mut stored = encode(b"quokka one by the shore");
        stored[super::MAGIC.len()] = 9;
        assert!(matches!(decode(&stored), Err(CodecError::UnknownFormat(9))));
    }

    #[test]
    fn a_truncated_or_garbled_payload_is_refused_not_returned() {
        let stored = encode("quokka one by the shore ".repeat(40).as_bytes());
        let truncated = &stored[..stored.len() - 3];
        assert!(matches!(decode(truncated), Err(CodecError::Corrupt(_))));
        let mut garbled = stored.clone();
        for byte in &mut garbled[super::TAG_LEN + 8..super::TAG_LEN + 16] {
            *byte ^= 0x5A;
        }
        assert!(matches!(decode(&garbled), Err(CodecError::Corrupt(_))));
        let mut headless = super::MAGIC.to_vec();
        headless.extend_from_slice(&[super::FORMAT_VERSION, DICTIONARY_ID]);
        assert!(matches!(decode(&headless), Err(CodecError::Corrupt(_))));
    }

    #[test]
    fn the_shipped_dictionary_is_the_one_the_module_doc_names() {
        assert_eq!(kaban_policy::sha256_hex(DICTIONARY), DICTIONARY_SHA256);
        assert_eq!(DICTIONARY.len(), 65536);
    }

    /// A short Markdown body (the walkthrough ADR, 206 bytes) shrinks, and shrinks more than
    /// dictionary-less zstd makes it: the shipped dictionary is in use, not merely shipped.
    #[test]
    fn a_short_markdown_body_shrinks_more_under_the_dictionary_than_without() {
        let body = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/walkthrough/shop/docs/adr/0007-discounts.md"),
        )
        .unwrap();
        let stored = encode(&body);
        let bare = zstd::bulk::compress(&body, super::LEVEL).unwrap();
        assert!(
            stored.len() < body.len() && stored.len() < bare.len() + super::TAG_LEN,
            "{} bytes -> {} with the dictionary, {} without",
            body.len(),
            stored.len(),
            bare.len()
        );
    }

    /// The offline trainer of the shipped dictionary (run once; see the module doc for the
    /// invocation, the corpus and the resulting hash): every `*.md` and `*.txt` under the
    /// directories `KABAN_BODY_DICTIONARY_CORPUS` names (`:`-separated), sorted by path, split
    /// into paragraph samples, trained to `KABAN_BODY_DICTIONARY_SIZE` bytes (default 65536) and
    /// written to `KABAN_BODY_DICTIONARY_OUT`.
    #[test]
    #[ignore = "the one-off trainer of the shipped dictionary; needs the three KABAN_BODY_DICTIONARY_* variables"]
    fn train_dictionary() {
        let roots = std::env::var("KABAN_BODY_DICTIONARY_CORPUS").unwrap();
        let out = std::env::var("KABAN_BODY_DICTIONARY_OUT").unwrap();
        let size: usize =
            std::env::var("KABAN_BODY_DICTIONARY_SIZE").map_or(65536, |s| s.parse().unwrap());
        // relative paths are workspace-relative (nextest runs in the crate directory)
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let out = workspace.join(out);
        let mut files = Vec::new();
        for root in roots.split(':') {
            collect(&workspace.join(root), &mut files);
        }
        files.sort();
        let mut samples: Vec<Vec<u8>> = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            for paragraph in text.split("\n\n") {
                let paragraph = paragraph.trim();
                if paragraph.len() >= 64 {
                    samples.push(paragraph.as_bytes().to_vec());
                }
            }
        }
        let dictionary = zstd::dict::from_samples(&samples, size).unwrap();
        std::fs::write(&out, &dictionary).unwrap();
        eprintln!(
            "trained {} bytes from {} samples in {} files; sha256 {}",
            dictionary.len(),
            samples.len(),
            files.len(),
            kaban_policy::sha256_hex(&dictionary)
        );
    }

    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("md" | "txt")
            ) {
                out.push(path);
            }
        }
    }
}
