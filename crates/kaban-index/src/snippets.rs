//! Snippets (stage1-spec G1-F17, G1-F7, RS-3, RS-5; stage1-plan CG8.2): for every non-external
//! symbol row with a definition range and a file, the committed lines `def.sl ..= min(def.el,
//! def.sl + 7)` of its file, at most 1,024 bytes, read with `git show <sha>:<path>` through the
//! pinned git of `identity_cli` (never the working tree), gated as a free field (G1-S1: redact
//! keeps the masked text, a reject hit drops the row and counts `policy_dropped`).
//!
//! ```text
//!   Rows (symbols, files) ==> group by file_id ==> identity_cli::git_show(repo, sha, path)  one per file
//!       ==> slice lines in memory ==> bounded(1,024) ==> Policy::scan (the Free rule)
//!       ==> the ceiling again on the masked text (a mask can outgrow its match), cut outside every mask
//!       ==> Collected { rows sorted by symbol_id, skipped, policy_dropped }; the file bytes are dropped
//! ```
//!
//! `git` here is kaban's own child, outside the isolation tier (as `serve`'s git is): the
//! indexers have finished, the snapshot is not yet published.

use std::collections::BTreeMap;
use std::path::Path;

use kaban_codegraph::write::SnippetRow;
use kaban_codegraph::{NodeId, NodeKind, Range};
use kaban_core::process::InheritedFds;
use kaban_policy::{Hit, Policy};

use crate::derive::TRUNCATION_MARKER;
use crate::writer::Rows;
use crate::{derive, identity_cli};

/// Lines of one snippet: `def.sl ..= min(def.el, def.sl + 7)` (G1-F17).
pub const SNIPPET_LINES: u32 = 8;

/// Byte bound of one snippet, the fixed marker included (G1-F17).
pub const MAX_SNIPPET_BYTES: usize = 1024;

/// What one collection produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    /// The rows, sorted by `symbol_id` (`write::insert_snippets` refuses another order).
    pub rows: Vec<SnippetRow>,
    /// Symbols that got no snippet: the file's `git show` timed out, exited non-zero or
    /// exceeded the cap, the blob held a NUL byte, the slice was not UTF-8 or started past the
    /// end of the blob, or the masked text outgrew the ceiling and its masks could not be
    /// located (`snippet_skipped`).
    pub skipped: u64,
    /// Snippets dropped by a reject-action hit (`policy_dropped`).
    pub policy_dropped: u64,
}

/// Collect the snippets of `rows` at commit `sha` of the repository at `repo` under `policy`.
/// One `git show` per file, never per symbol; a file whose blob cannot be read skips its
/// symbols and the collection continues.
#[must_use]
pub fn collect(repo: &Path, sha: &str, rows: &Rows, policy: &Policy) -> Collected {
    collect_with(repo, sha, rows, policy, InheritedFds::Refuse)
}

/// [`collect`] with an explicit descriptor policy for the git child (`identity_cli::git_with`'s
/// convention: an in-process test passes [`InheritedFds::Tolerate`], production uses
/// [`collect`]).
#[must_use]
pub fn collect_with(
    repo: &Path,
    sha: &str,
    rows: &Rows,
    policy: &Policy,
    inherited_fds: InheritedFds,
) -> Collected {
    let paths: BTreeMap<_, &str> = rows
        .files()
        .iter()
        .map(|f| (f.id, f.path.as_str()))
        .collect();
    // no snippet (and no skip) for an external row, a file node or a row without a definition
    let mut by_file: BTreeMap<_, Vec<(NodeId, Range)>> = BTreeMap::new();
    for s in rows.symbols() {
        if s.external || s.kind == NodeKind::File {
            continue;
        }
        let (Some(def), Some(file_id)) = (s.def, s.file_id) else {
            continue;
        };
        by_file.entry(file_id).or_default().push((s.id, def));
    }
    let mut out = Collected::default();
    for (file_id, symbols) in by_file {
        let count = symbols.len() as u64;
        let Some(path) = paths.get(&file_id) else {
            out.skipped += count;
            continue;
        };
        // one bounded `git show` per file: a deadline, a non-zero exit (the path is not in the
        // commit) or the cap skips the file's symbols; the run continues
        let Ok(bytes) = identity_cli::git_show_with(repo, sha, path, inherited_fds) else {
            out.skipped += count;
            continue;
        };
        if bytes.contains(&0) {
            out.skipped += count;
            continue;
        }
        let lines = split_lines(&bytes);
        for (symbol_id, def) in symbols {
            let Some(text) = slice(&lines, def) else {
                out.skipped += 1;
                continue;
            };
            // the scanned string is the bounded string: bound first, then the Free rule of
            // G1-S1 (`gate::admit`): redact keeps the mask, a reject hit drops the row; the
            // ceiling holds on the masked text too (G1-F17 binds what is stored)
            let text = derive::bounded(&text, MAX_SNIPPET_BYTES);
            match admit_free(policy, text) {
                Admission::Keep(text) => out.rows.push(SnippetRow { symbol_id, text }),
                Admission::Drop => out.policy_dropped += 1,
                Admission::Skip => out.skipped += 1,
            }
        }
        // the file bytes drop here: nothing of the blob outlives its snippets
    }
    out.rows.sort_by_key(|r| r.symbol_id);
    out
}

/// What the free-field rule made of one bounded snippet.
enum Admission {
    /// The text to store: clean, or masked and within the ceiling.
    Keep(String),
    /// A reject hit: no row, `policy_dropped`.
    Drop,
    /// The masked text outgrew the ceiling and its masks could not be located in it (another
    /// engine layout than the one mirrored here): no row rather than a cut inside a mask,
    /// `snippet_skipped`.
    Skip,
}

/// The free-field rule of G1-S1 on a formed string (`gate::admit(FieldClass::Free, ..)`, whose
/// `Gated` input only the decoder constructs): a reject hit drops the field, a redact-only hit
/// keeps the masked text, a clean scan keeps the text. The ceiling then holds on what is kept
/// (G1-F17, RW2-7): the mask template is configurable and can be longer than the match, so
/// repeated short hits grow the masked text past `MAX_SNIPPET_BYTES`; it is cut back at a char
/// boundary outside every mask, after redaction, so no fragment of a match and no fragment of a
/// mask is stored.
fn admit_free(policy: &Policy, text: String) -> Admission {
    let verdict = policy.scan(&text);
    if verdict.rejected {
        return Admission::Drop;
    }
    if verdict.hits.is_empty() {
        return Admission::Keep(text);
    }
    if verdict.redacted.len() <= MAX_SNIPPET_BYTES {
        return Admission::Keep(verdict.redacted);
    }
    let template = policy.file.defaults.mask.as_str();
    match mask_ranges(&text, &verdict.hits, template, &verdict.redacted) {
        Some(masks) => Admission::Keep(cut_outside_masks(
            &verdict.redacted,
            &masks,
            MAX_SNIPPET_BYTES,
        )),
        None => Admission::Skip,
    }
}

/// The byte ranges of the masks inside `redacted`, rebuilt from the hits (offsets into `text`)
/// and the policy's mask template the way the engine lays them out: overlapping spans merge
/// under the first hit's rule, a span is clipped to what the previous one left, the text between
/// spans is copied. Every copied segment and every mask is checked against `redacted`; a
/// mismatch yields `None` (the caller then stores nothing rather than guess).
fn mask_ranges(
    text: &str,
    hits: &[Hit],
    template: &str,
    redacted: &str,
) -> Option<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    let (mut pos, mut out) = (0usize, 0usize);
    let mut i = 0;
    while i < hits.len() {
        let mut start = hits[i].start;
        let mut end = hits[i].end;
        let mut j = i + 1;
        while j < hits.len() && hits[j].start < end {
            end = end.max(hits[j].end);
            j += 1;
        }
        start = start.max(pos);
        if end > start {
            let copied = text.get(pos..start)?;
            if redacted.get(out..out + copied.len())? != copied {
                return None;
            }
            out += copied.len();
            let mask = template.replace("{rule}", &hits[i].rule);
            if redacted.get(out..out + mask.len())? != mask {
                return None;
            }
            ranges.push((out, out + mask.len()));
            out += mask.len();
            pos = end;
        }
        i = j;
    }
    (redacted.get(out..)? == text.get(pos..)?).then_some(ranges)
}

/// `masked` cut to `max` bytes with the fixed marker, at a char boundary and outside every one
/// of `masks` (byte ranges in `masked`): a cut that would land inside a mask moves to that
/// mask's start, so the stored text ends on whole masks and clean text only.
fn cut_outside_masks(masked: &str, masks: &[(usize, usize)], max: usize) -> String {
    let mut cut = max.saturating_sub(TRUNCATION_MARKER.len());
    if let Some(&(start, _)) = masks.iter().find(|(s, e)| *s < cut && cut < *e) {
        cut = start;
    }
    while cut > 0 && !masked.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + TRUNCATION_MARKER.len());
    out.push_str(&masked[..cut]);
    out.push_str(TRUNCATION_MARKER);
    out
}

/// The blob split on `\n` (`\r` kept); a final newline ends the last line and opens no empty
/// one.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    lines
}

/// Lines `def.sl ..= min(def.el, def.sl + 7)` joined with `\n`; `None` when `def.sl` lies past
/// the end or the selection is not UTF-8. `def.el` past the end stops at the last line.
fn slice(lines: &[&[u8]], def: Range) -> Option<String> {
    let start = usize::try_from(def.sl).ok()?;
    if start >= lines.len() {
        return None;
    }
    let last = def.el.min(def.sl.saturating_add(SNIPPET_LINES - 1));
    let end = usize::try_from(last).ok()?.min(lines.len() - 1);
    let mut joined: Vec<u8> = Vec::new();
    for (i, line) in lines[start..=end].iter().enumerate() {
        if i > 0 {
            joined.push(b'\n');
        }
        joined.extend_from_slice(line);
    }
    String::from_utf8(joined).ok()
}
