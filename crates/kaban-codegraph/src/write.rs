//! Deterministic snapshot writing (stage1-spec §4.1 row order, G1-N5; stage1-plan CG2.2;
//! cg-join-v1 §10 fixture construction). The helpers take already-sorted, already-gated rows
//! whose ids are their 1-based positions; a helper that finds its input out of order refuses with
//! [`WriteError::Unsorted`] or [`WriteError::Ids`] and writes nothing, so two builds from the same
//! rows are byte-identical. The caller owns the connection (a `<commit40>.cg.db.tmp-<pid>` file
//! opened 0600 by `kaban-index`, a scratch file in tests). [`CreatedSnapshot`] retains the
//! publisher's created descriptor and owns its connection until explicit finalization.
//!
//! ```text
//!   open_existing ==> begin ==> insert_files ==> insert_symbols ==> insert_edges
//!         ==> insert_occurrences ==> insert_snippets ==> insert_edge_types ==> insert_meta
//!         ==> rebuild_fts ==> finish
//! ```

mod persistence;

use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};

use crate::error::WriteError;
use crate::schema::{self, Confidence, EdgeType, FileId, NodeId, NodeKind, Range};

/// Open an **existing** temporary snapshot file for writing (`kaban-index` creates the
/// `<commit40>.cg.db.tmp-<pid>` file itself with `O_CREAT|O_EXCL|O_NOFOLLOW` 0600, stage1-spec
/// §4.1, then hands the path here). The basename must have exactly that shape (RU1-3): a
/// published `<commit40>.cg.db`, or any other name, is refused before SQLite sees the file, so
/// no helper of this module can ever mutate a published snapshot. No `CREATE` flag, so a
/// missing file is an error rather than a silently created one; `NOFOLLOW` so a link swapped
/// in at the final component is refused by the kernel. This is the one SQLite opener the writer
/// side uses: every statement after it goes through the helpers of this module (`sql_boundary`).
///
/// # Errors
/// [`WriteError::NotTempFile`] for a name outside the temp shape; [`WriteError::Sql`] (absent
/// file, a symlink, a directory, permissions).
pub fn open_existing(path: &Path) -> Result<Connection, WriteError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(WriteError::NotTempFile)?;
    if !is_temp_snapshot_name(name) {
        return Err(WriteError::NotTempFile);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    Ok(Connection::open_with_flags(path, flags)?)
}

/// Legacy Linux connection opener through `/proc/self/fd/<n>`, retained for callers that
/// own their connection. SQLite performs its own filename normalization. The portable
/// publisher uses [`CreatedSnapshot`] to avoid any SQLite pathname reopening and to bind
/// the finalized output to the created descriptor. No `CREATE`; the descriptor must refer
/// to a regular file. This legacy entry point is not available without procfs.
///
/// # Errors
/// [`WriteError::NotTempFile`] when the descriptor is not a regular file (the publisher's temp
/// inode is always one); [`WriteError::Sql`] when SQLite refuses the open.
pub fn open_created(fd: BorrowedFd<'_>) -> Result<Connection, WriteError> {
    let st = rustix::fs::fstat(fd).map_err(|_| WriteError::NotTempFile)?;
    if !rustix::fs::FileType::from_raw_mode(st.st_mode).is_file() {
        return Err(WriteError::NotTempFile);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Ok(Connection::open_with_flags(
        format!("/proc/self/fd/{}", fd.as_raw_fd()),
        flags,
    )?)
}

/// A snapshot whose finalized bytes are written only to the publisher's retained file.
/// SQL runs in memory; row helpers still borrow an ordinary connection. Consuming
/// [`Self::finish`] commits, backs up pages to an anonymous file and copies through a
/// fixed-size buffer before the publisher's identity check,
/// file fsync, no-overwrite rename and directory fsync. Drop never attempts a fallible copy.
/// The caller keeps ownership of its exclusively created temporary file for this lifetime.
pub struct CreatedSnapshot<'fd> {
    connection: Connection,
    output: BorrowedFd<'fd>,
}

impl std::fmt::Debug for CreatedSnapshot<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreatedSnapshot")
            .finish_non_exhaustive()
    }
}

impl<'fd> CreatedSnapshot<'fd> {
    /// Retain a regular output descriptor and open a private in-memory database.
    ///
    /// # Errors
    /// [`WriteError::NotTempFile`] for a non-regular descriptor, [`WriteError::Output`]
    /// for metadata failure, or [`WriteError::Sql`] when SQLite cannot initialize.
    pub fn open(output: BorrowedFd<'fd>) -> Result<Self, WriteError> {
        let stat = rustix::fs::fstat(output).map_err(std::io::Error::from)?;
        if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
            return Err(WriteError::NotTempFile);
        }
        Ok(Self {
            connection: Connection::open_in_memory()?,
            output,
        })
    }

    /// Borrow the connection for the existing sorted/gated row helpers.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Commit/analyze/vacuum, copy to the retained inode and close the connection.
    /// Persistence uses an anonymous disk-backed staging file and a 64 KiB copy buffer,
    /// never a contiguous whole-image allocation. This still needs database/row memory
    /// and space for the staging file; it does not promise constant total RAM.
    /// The publisher owns durability and publication after this returns successfully.
    ///
    /// # Errors
    /// [`WriteError::Sql`] for SQLite finalization/backup/close failures;
    /// [`WriteError::Output`] for staging-file or output I/O failures.
    pub fn finish(self) -> Result<(), WriteError> {
        finish(&self.connection)?;
        persistence::persist(&self.connection, self.output)?;
        self.connection.close().map_err(|(_, error)| error.into())
    }
}

/// `<commit40>.cg.db.tmp-<pid>`: 40 lowercase hex digits, the fixed infix, one or more decimal
/// digits (the publisher's temp-file shape, stage1-spec §4.1).
fn is_temp_snapshot_name(name: &str) -> bool {
    const INFIX: &str = ".cg.db.tmp-";
    let Some((commit, pid)) = name.split_once(INFIX) else {
        return false;
    };
    let hex40 = commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let digits = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    hex40 && digits
}

/// One `files` row; `id` is the 1-based position in `path` byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// The 1-based position.
    pub id: FileId,
    /// Repository-relative path (gated, byte order is the sort key).
    pub path: String,
    /// Language name (closed set, `kaban-index`'s detection).
    pub language: String,
    /// Test file by the per-language rule (stage1-spec §4.3).
    pub is_test: bool,
    /// Line count.
    pub lines: u32,
}

/// One `symbols` row; `id` is the 1-based position in `ref` byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    /// The 1-based position.
    pub id: NodeId,
    /// `symbol | local | file`.
    pub kind: NodeKind,
    /// Exact row identity (`scip:`, `local:`, `file:`), the sort key.
    pub r#ref: String,
    /// The raw SCIP symbol; `None` for a file node.
    pub scip: Option<String>,
    /// The normalised key; `kind = symbol` only.
    pub key: Option<String>,
    /// SCIP `SymbolInformation.Kind` name, or `File`.
    pub label: String,
    /// The last descriptor's identifier (G1-F1), or the file name.
    pub name: String,
    /// The defining file.
    pub file_id: Option<FileId>,
    /// Definition range.
    pub def: Option<Range>,
    /// Enclosing range.
    pub enc: Option<Range>,
    /// Signature text, gated.
    pub signature: Option<String>,
    /// Documentation text, gated.
    pub doc: Option<String>,
    /// Defined outside the repository (placeholder rows are external).
    pub external: bool,
    /// Defined in a test file.
    pub is_test: bool,
    /// The deterministic identifier split of cg-join-v1 §8 (built by CG3.3; empty allowed).
    pub ident_terms: String,
}

/// One `edges` row; sorted by `(src, type, dst)` which equals `(src ref, type text, dst ref)`
/// because ids are `ref`-order positions (`type` compares as its stored text, §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeRow {
    /// Source node.
    pub src: NodeId,
    /// Target node.
    pub dst: NodeId,
    /// The edge type.
    pub r#type: EdgeType,
    /// Site count for `REFERENCES`, 1 otherwise.
    pub weight: u32,
}

/// One `occurrences` row; sorted by `(file_id, sl, sc, el, ec, symbol_id, roles)` which equals
/// the `(file path, sl, sc, el, ec, symbol ref, roles)` order of §4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccurrenceRow {
    /// The occurring symbol.
    pub symbol_id: NodeId,
    /// The document.
    pub file_id: FileId,
    /// The site.
    pub range: Range,
    /// SCIP `SymbolRole` bits.
    pub roles: u32,
    /// The enclosing symbol by `enclosing_range`, `None` when attributed to the file.
    pub enclosing_id: Option<NodeId>,
}

/// One `snippets` row; sorted by `symbol_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetRow {
    /// The symbol.
    pub symbol_id: NodeId,
    /// Bounded, scanned text (G1-S1).
    pub text: String,
}

/// One `edge_types` row (the derivation table of stage1-spec §4.3); sorted by type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTypeRow {
    /// The edge type.
    pub r#type: EdgeType,
    /// The SCIP source of the derivation.
    pub source: String,
    /// The confidence class.
    pub confidence: Confidence,
}

/// Fix the page size, create the schema and open the write transaction.
///
/// # Errors
/// [`WriteError::Sql`] (a read-only connection, an existing schema).
pub fn begin(conn: &Connection) -> Result<(), WriteError> {
    conn.execute_batch(&format!("PRAGMA page_size = {};", schema::PAGE_SIZE))?;
    conn.execute_batch(schema::DDL)?;
    conn.execute_batch("BEGIN;")?;
    Ok(())
}

/// Insert the `files` rows (sorted by `path`, ids = positions).
///
/// # Errors
/// [`WriteError::Unsorted`], [`WriteError::Ids`], [`WriteError::Sql`].
pub fn insert_files(conn: &Connection, rows: &[FileRow]) -> Result<(), WriteError> {
    check_sorted(rows, |a, b| a.path.as_bytes() < b.path.as_bytes())?;
    check_ids(rows, |r| r.id.0)?;
    let mut stmt = conn.prepare(
        "INSERT INTO files(id, path, language, is_test, lines) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for r in rows {
        stmt.execute(params![r.id.0, r.path, r.language, r.is_test, r.lines])?;
    }
    Ok(())
}

/// Insert the `symbols` rows (sorted by `ref`, ids = positions).
///
/// # Errors
/// [`WriteError::Unsorted`], [`WriteError::Ids`], [`WriteError::Sql`].
pub fn insert_symbols(conn: &Connection, rows: &[SymbolRow]) -> Result<(), WriteError> {
    check_sorted(rows, |a, b| a.r#ref.as_bytes() < b.r#ref.as_bytes())?;
    check_ids(rows, |r| r.id.0)?;
    let mut stmt = conn.prepare(
        "INSERT INTO symbols(id, kind, ref, scip, key, label, name, file_id, \
         def_sl, def_sc, def_el, def_ec, enc_sl, enc_sc, enc_el, enc_ec, \
         signature, doc, external, is_test, ident_terms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
    )?;
    for r in rows {
        let def = split_range(r.def);
        let enc = split_range(r.enc);
        stmt.execute(params![
            r.id.0,
            r.kind.as_str(),
            r.r#ref,
            r.scip,
            r.key,
            r.label,
            r.name,
            r.file_id.map(|f| f.0),
            def[0],
            def[1],
            def[2],
            def[3],
            enc[0],
            enc[1],
            enc[2],
            enc[3],
            r.signature,
            r.doc,
            r.external,
            r.is_test,
            r.ident_terms,
        ])?;
    }
    Ok(())
}

/// Insert the `edges` rows (sorted by `(src, type, dst)`, `type` by its stored text: `TESTED_BY`
/// before `TYPE_OF`, RG2-2).
///
/// # Errors
/// [`WriteError::Unsorted`], [`WriteError::Sql`].
pub fn insert_edges(conn: &Connection, rows: &[EdgeRow]) -> Result<(), WriteError> {
    check_sorted(rows, |a, b| {
        (a.src, a.r#type.as_str(), a.dst) < (b.src, b.r#type.as_str(), b.dst)
    })?;
    let mut stmt =
        conn.prepare("INSERT INTO edges(src, dst, type, weight) VALUES (?1, ?2, ?3, ?4)")?;
    for r in rows {
        stmt.execute(params![r.src.0, r.dst.0, r.r#type.as_str(), r.weight])?;
    }
    Ok(())
}

/// Insert the `occurrences` rows (sorted by `(file_id, sl, sc, el, ec, symbol_id, roles)`).
///
/// # Errors
/// [`WriteError::Unsorted`], [`WriteError::Sql`].
pub fn insert_occurrences(conn: &Connection, rows: &[OccurrenceRow]) -> Result<(), WriteError> {
    check_sorted(rows, |a, b| {
        (a.file_id, a.range, a.symbol_id, a.roles) < (b.file_id, b.range, b.symbol_id, b.roles)
    })?;
    let mut stmt = conn.prepare(
        "INSERT INTO occurrences(symbol_id, file_id, sl, sc, el, ec, roles, enclosing_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for r in rows {
        stmt.execute(params![
            r.symbol_id.0,
            r.file_id.0,
            r.range.sl,
            r.range.sc,
            r.range.el,
            r.range.ec,
            r.roles,
            r.enclosing_id.map(|n| n.0),
        ])?;
    }
    Ok(())
}

/// Insert the `snippets` rows (sorted by `symbol_id`).
///
/// # Errors
/// [`WriteError::Unsorted`], [`WriteError::Sql`].
pub fn insert_snippets(conn: &Connection, rows: &[SnippetRow]) -> Result<(), WriteError> {
    check_sorted(rows, |a, b| a.symbol_id < b.symbol_id)?;
    let mut stmt = conn.prepare("INSERT INTO snippets(symbol_id, text) VALUES (?1, ?2)")?;
    for r in rows {
        stmt.execute(params![r.symbol_id.0, r.text])?;
    }
    Ok(())
}

/// Insert the `edge_types` rows (sorted by the stored `type` text).
///
/// # Errors
/// [`WriteError::Unsorted`], [`WriteError::Sql`].
pub fn insert_edge_types(conn: &Connection, rows: &[EdgeTypeRow]) -> Result<(), WriteError> {
    check_sorted(rows, |a, b| a.r#type.as_str() < b.r#type.as_str())?;
    let mut stmt =
        conn.prepare("INSERT INTO edge_types(type, source, confidence) VALUES (?1, ?2, ?3)")?;
    for r in rows {
        stmt.execute(params![r.r#type.as_str(), r.source, r.confidence.as_str()])?;
    }
    Ok(())
}

/// Insert the `meta` rows (sorted by key; `schema_version`, `commit`, `policy_version`, ... are
/// the caller's, see stage1-spec §4.1).
///
/// # Errors
/// [`WriteError::Unsorted`], [`WriteError::Sql`].
pub fn insert_meta(conn: &Connection, rows: &[(&str, &str)]) -> Result<(), WriteError> {
    check_sorted(rows, |a, b| a.0.as_bytes() < b.0.as_bytes())?;
    let mut stmt = conn.prepare("INSERT INTO meta(key, value) VALUES (?1, ?2)")?;
    for (k, v) in rows {
        stmt.execute(params![k, v])?;
    }
    Ok(())
}

/// Rebuild `symbols_fts` from the `symbols` content table (after `insert_symbols`).
///
/// # Errors
/// [`WriteError::Sql`].
pub fn rebuild_fts(conn: &Connection) -> Result<(), WriteError> {
    conn.execute_batch("INSERT INTO symbols_fts(symbols_fts) VALUES ('rebuild');")?;
    Ok(())
}

/// Commit, `ANALYZE`, then `VACUUM` last (G1-N5).
///
/// # Errors
/// [`WriteError::Sql`].
pub fn finish(conn: &Connection) -> Result<(), WriteError> {
    conn.execute_batch("COMMIT; ANALYZE; VACUUM;")?;
    Ok(())
}

/// `[sl, sc, el, ec]` as nullable columns.
fn split_range(r: Option<Range>) -> [Option<u32>; 4] {
    r.map_or([None; 4], |r| {
        [Some(r.sl), Some(r.sc), Some(r.el), Some(r.ec)]
    })
}

/// Strictly increasing under `less` (a duplicate is out of order too: the unique columns would
/// refuse it later with a half-written table).
fn check_sorted<T>(rows: &[T], less: impl Fn(&T, &T) -> bool) -> Result<(), WriteError> {
    if rows.windows(2).all(|w| less(&w[0], &w[1])) {
        Ok(())
    } else {
        Err(WriteError::Unsorted)
    }
}

/// Ids are the 1-based positions.
fn check_ids<T>(rows: &[T], id: impl Fn(&T) -> i64) -> Result<(), WriteError> {
    let positional = rows
        .iter()
        .enumerate()
        .all(|(i, r)| i64::try_from(i + 1).is_ok_and(|p| p == id(r)));
    if positional {
        Ok(())
    } else {
        Err(WriteError::Ids)
    }
}
