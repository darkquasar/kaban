//! The crate's closed error enums (stage1-plan CG2.1, CG2.2; cg-join-v1 §2).
//!
//! Every variant is a class, never a carrier of input bytes: `Display` prints the class name
//! and nothing else, so a value that failed the grammar, a refused path or a snapshot file name
//! can never leak through an error message (G1-S4 "no echo", G1-S8). A driver failure
//! (`ReadError::Sql`, `WriteError::Sql`) keeps the `rusqlite::Error` behind the core
//! [`BackendSource`] holder (M4 MT-8a, `GM2a` RG2-1): SQLite's own message may quote a function
//! or object name from the snapshot, so `Display`, `Debug` and the `source()` chain all stop at
//! the holder's fixed text; a test or an operator downcasts through [`BackendSource::driver`].

use kaban_core::BackendSource;
use thiserror::Error;

/// Why a string is not a SCIP symbol under the grammar of G1-S4 (`symbol::parse`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum SymbolError {
    /// Longer than [`crate::symbol::MAX_SYMBOL_BYTES`] bytes.
    #[error("symbol oversize")]
    Oversize,
    /// Contains a control character (Unicode category Cc, including NUL, tab and newline).
    #[error("symbol control character")]
    ControlCharacter,
    /// The shape is not `scheme manager package version descriptors+` or `local <id>`: a missing
    /// or empty segment, an unterminated escape, an unknown descriptor suffix, no descriptor.
    #[error("symbol malformed")]
    Malformed,
    /// The string parses but `format(parse(s)) != s`: an identifier escaped in backticks that
    /// needs no escaping, or a character outside the unescaped identifier set left unescaped.
    #[error("symbol not canonical")]
    NonCanonical,
}

/// Why a string is not a repository-relative path (G1-F15 rule, applied by `ref::parse_ref`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum PathError {
    /// The empty string names no file.
    #[error("path empty")]
    Empty,
    /// Longer than [`crate::r#ref::MAX_PATH_BYTES`] bytes.
    #[error("path oversize")]
    Oversize,
    /// Begins with `/`.
    #[error("path leading slash")]
    LeadingSlash,
    /// Has a `..` component.
    #[error("path parent component")]
    ParentComponent,
    /// Contains a NUL byte.
    #[error("path nul")]
    Nul,
    /// Contains a control character other than NUL.
    #[error("path control character")]
    ControlCharacter,
}

/// Why a string is not a `ref` of stage1-spec §4.2 (`ref::parse_ref`); the typed handler maps
/// every variant to the single class `ref_invalid` (G1-S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum RefError {
    /// Not one of the closed prefixes `scip:`, `local:`, `file:`.
    #[error("ref unknown prefix")]
    UnknownPrefix,
    /// A `local:` ref without the `:` that separates the path from the symbol.
    #[error("ref malformed")]
    Malformed,
    /// The path part failed the repository-relative path rule.
    #[error("ref path: {0}")]
    Path(#[from] PathError),
    /// The SCIP part failed the symbol grammar.
    #[error("ref symbol: {0}")]
    Symbol(#[from] SymbolError),
}

/// Why a snapshot file is not admitted by `SqliteSnapshot::open_ro` or `validate_for_attach`
/// (cg-join-v1 §2, closed; `Display` is the class, never a path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// `symlink_metadata` is not a regular file (directory, device, socket).
    #[error("snapshot not a regular file")]
    NotRegularFile,
    /// The final path component is a symbolic link (followed or dangling).
    #[error("snapshot symlink refused")]
    SymlinkRefused,
    /// Mode bits `group|other` are not zero.
    #[error("snapshot mode refused")]
    ModeRefused,
    /// The file name is not a final name `<commit40>.cg.db` (a `.tmp-<pid>` name, anything else).
    #[error("snapshot temp name refused")]
    TempNameRefused,
    /// `meta.schema_version` has a major above the reader's `SCHEMA_VERSION`: re-index or upgrade.
    #[error("snapshot too new: schema major {major} is above the supported {supported}")]
    SnapshotTooNew {
        /// The file's schema major.
        major: u32,
        /// This reader's `SCHEMA_VERSION`.
        supported: u32,
    },
    /// The file is not a readable snapshot: `PRAGMA quick_check` failed, `meta` is unreadable or
    /// malformed, or SQLite reported a corrupt image.
    #[error("snapshot corrupt")]
    Corrupt,
    /// An operating-system failure before SQLite saw the file, by `std::io::ErrorKind` class.
    #[error("snapshot io: {class}")]
    Io {
        /// The I/O error class (`not_found`, `permission_denied`, ...).
        class: &'static str,
    },
}

/// Why a write helper refused or failed (`write::*`; the writer side, `kaban-index`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WriteError {
    /// The rows are not in the order of stage1-spec §4.1 (the helper writes nothing).
    #[error("rows not sorted in the §4.1 order")]
    Unsorted,
    /// The ids are not the 1-based positions of the rows (the helper writes nothing).
    #[error("row ids are not 1-based positions")]
    Ids,
    /// `write::open_existing` was given a name that is not the publisher's temporary-file shape
    /// (`<commit40>.cg.db.tmp-<pid>`): a published `<commit40>.cg.db` is never reopened for
    /// writing (RU1-3). The name is not part of the text.
    #[error("not a temporary snapshot file name")]
    NotTempFile,
    /// SQLite refused (a read-only connection, a constraint, a full disk); the driver error is
    /// behind the redacting holder.
    #[error("sqlite failed")]
    Sql(#[source] BackendSource),
    /// Copying the finalized image to the created file failed. Backend diagnostics stay
    /// behind the same redacting boundary as SQLite diagnostics.
    #[error("snapshot output failed")]
    Output(#[source] BackendSource),
}

impl From<rusqlite::Error> for WriteError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(BackendSource::new(e))
    }
}

impl From<std::io::Error> for WriteError {
    fn from(error: std::io::Error) -> Self {
        Self::Output(BackendSource::new(error))
    }
}

/// Why a reader query failed (`CodeGraphReader`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReadError {
    /// More than one non-external `kind = 'symbol'` row shares the key (stage1-spec §4.2).
    #[error("ambiguous symbol")]
    Ambiguous,
    /// A column holds a value outside its closed set (the snapshot is not one this reader wrote).
    #[error("snapshot malformed: {column}")]
    Malformed {
        /// The column whose value was refused.
        column: &'static str,
    },
    /// A request argument is outside its bound (`depth` above `Limits::depth_max`).
    #[error("argument out of range: {argument}")]
    OutOfRange {
        /// The argument's name.
        argument: &'static str,
    },
    /// SQLite failed; the driver error is behind the redacting holder.
    #[error("sqlite failed")]
    Sql(#[source] BackendSource),
}

impl From<rusqlite::Error> for ReadError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(BackendSource::new(e))
    }
}
