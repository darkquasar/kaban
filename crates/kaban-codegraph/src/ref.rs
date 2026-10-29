//! The discriminated `ref` parse (G1-S4, stage1-spec §4.2): a `ref` begins with one of the closed
//! prefixes `scip:`, `local:`, `file:`; the SCIP grammar is applied only to the parts that are
//! SCIP symbols and the repository-relative path rule only to the parts that are paths, so a
//! `file:` ref never reaches `symbol::parse` (asserted by `ref_parse_discriminates`).

use crate::error::{PathError, RefError};
use crate::symbol;

/// The repository-relative path rule's size bound in bytes (G1-F15).
pub const MAX_PATH_BYTES: usize = 4096;

/// A parsed `ref`, borrowing its parts from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ref<'a> {
    /// `scip:<scip>`: a global symbol row.
    Scip(&'a str),
    /// `local:<path>:<scip>`: a document-local symbol row.
    Local {
        /// The repository-relative document path.
        path: &'a str,
        /// The `local <id>` symbol.
        scip: &'a str,
    },
    /// `file:<path>`: a file row.
    File(&'a str),
}

/// Parse a `ref`; the error is a class, never the input.
///
/// # Errors
/// [`RefError`]: unknown prefix, malformed `local:` form, a failed path rule, a failed grammar.
pub fn parse_ref(s: &str) -> Result<Ref<'_>, RefError> {
    if let Some(scip) = s.strip_prefix("scip:") {
        symbol::parse(scip)?;
        return Ok(Ref::Scip(scip));
    }
    if let Some(path) = s.strip_prefix("file:") {
        check_repo_relative_path(path)?;
        return Ok(Ref::File(path));
    }
    if let Some(rest) = s.strip_prefix("local:") {
        return parse_local(rest);
    }
    Err(RefError::UnknownPrefix)
}

/// `<path>:<scip>`: the path is everything up to the last `:` that is followed by a grammar-valid
/// SCIP symbol (`local N` symbols contain no `:`, a global symbol may, in a `Meta` descriptor or
/// inside backticks). When no split parses, the error is the grammar's verdict on the shortest
/// candidate (after the last `:`).
fn parse_local(rest: &str) -> Result<Ref<'_>, RefError> {
    let mut last_err = None;
    for (at, _) in rest.rmatch_indices(':') {
        let scip = &rest[at + 1..];
        match symbol::parse(scip) {
            Ok(_) => {
                let path = &rest[..at];
                check_repo_relative_path(path)?;
                return Ok(Ref::Local { path, scip });
            }
            Err(e) => {
                last_err.get_or_insert(e);
            }
        }
    }
    Err(last_err.map_or(RefError::Malformed, RefError::Symbol))
}

/// The repository-relative path rule of G1-F15: not empty, no leading `/`, no `..` component,
/// no NUL, no control character, at most [`MAX_PATH_BYTES`] bytes.
///
/// # Errors
/// [`PathError`] naming the rule that failed.
pub fn check_repo_relative_path(p: &str) -> Result<(), PathError> {
    if p.is_empty() {
        return Err(PathError::Empty);
    }
    if p.len() > MAX_PATH_BYTES {
        return Err(PathError::Oversize);
    }
    if p.starts_with('/') {
        return Err(PathError::LeadingSlash);
    }
    if p.contains('\0') {
        return Err(PathError::Nul);
    }
    if p.chars().any(char::is_control) {
        return Err(PathError::ControlCharacter);
    }
    if p.split('/').any(|component| component == "..") {
        return Err(PathError::ParentComponent);
    }
    Ok(())
}
