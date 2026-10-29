//! SCIP ranges into snapshot ranges (stage1-spec §4.3 `[EN-amend]`; stage1-plan CG3.3).
//!
//! SCIP carries a site either as the legacy `repeated int32 range` (`[line, start, end]` on one
//! line or `[sl, sc, el, ec]`) or as the typed `single_line_range` / `multi_line_range` oneofs
//! the schema now prefers; the same for `enclosing_range` and `typed_enclosing_range`. The
//! writer prefers the typed form and falls back to the legacy one, and folds the forms it used
//! into `meta.scip_range_form = typed | legacy | mixed` so a fixture can assert it (§21 note A1).
//! All coordinates stay 0-based, end exclusive, as SCIP emits them; a negative value, a wrong
//! arity or an end before its start is a [`MalformedRange`] (RU1-7), counted by the derivation
//! and never persisted.

use std::fmt;

use kaban_codegraph::Range;
use scip::types::occurrence::{Typed_enclosing_range, Typed_range};
use scip::types::{MultiLineRange, SingleLineRange};

use crate::gate::AdmittedOccurrence;

/// Which SCIP form a range came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeForm {
    /// `typed_range` / `typed_enclosing_range`.
    Typed,
    /// `range` / `enclosing_range`.
    Legacy,
}

/// The forms an index used, folded over every range read (`meta.scip_range_form`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RangeForms {
    /// No range read yet.
    #[default]
    None,
    /// Only the typed form.
    Typed,
    /// Only the legacy form.
    Legacy,
    /// Both forms.
    Mixed,
}

impl RangeForms {
    /// Fold one more form in.
    #[must_use]
    pub const fn with(self, form: RangeForm) -> Self {
        match (self, form) {
            (Self::None | Self::Typed, RangeForm::Typed) => Self::Typed,
            (Self::None | Self::Legacy, RangeForm::Legacy) => Self::Legacy,
            (Self::Typed, RangeForm::Legacy)
            | (Self::Legacy, RangeForm::Typed)
            | (Self::Mixed, _) => Self::Mixed,
        }
    }

    /// The `meta.scip_range_form` value (`legacy` when no range was read: the schema's
    /// historical default).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None | Self::Legacy => "legacy",
            Self::Typed => "typed",
            Self::Mixed => "mixed",
        }
    }
}

impl fmt::Display for RangeForms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A producer range this module refuses (RU1-7): wrong arity, a negative coordinate, or an
/// end before its start. A fixed class: no coordinate is carried, the owner is a counted skip
/// (`counts:malformed_range_skipped`), never a panic or a persisted inverted span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MalformedRange;

impl fmt::Display for MalformedRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed_range")
    }
}

impl std::error::Error for MalformedRange {}

/// The legacy `repeated int32` range: `[line, start, end]` or `[sl, sc, el, ec]`; an empty
/// list is no range (`Ok(None)`: the producer emitted nothing).
///
/// # Errors
/// [`MalformedRange`]: another arity, a negative coordinate, or
/// `(start_line, start_column) > (end_line, end_column)`.
pub fn legacy(r: &[i32]) -> Result<Option<Range>, MalformedRange> {
    let (sl, sc, el, ec) = match *r {
        [] => return Ok(None),
        [l, s, e] => (l, s, l, e),
        [sl, sc, el, ec] => (sl, sc, el, ec),
        _ => return Err(MalformedRange),
    };
    ordered(sl, sc, el, ec).map(Some)
}

/// Four coordinates into an ordered, non-negative [`Range`].
fn ordered(sl: i32, sc: i32, el: i32, ec: i32) -> Result<Range, MalformedRange> {
    let coord = |v: i32| u32::try_from(v).map_err(|_| MalformedRange);
    let range = Range {
        sl: coord(sl)?,
        sc: coord(sc)?,
        el: coord(el)?,
        ec: coord(ec)?,
    };
    if (range.sl, range.sc) <= (range.el, range.ec) {
        Ok(range)
    } else {
        Err(MalformedRange)
    }
}

fn single(r: &SingleLineRange) -> Result<Range, MalformedRange> {
    ordered(r.line, r.start_character, r.line, r.end_character)
}

fn multi(r: &MultiLineRange) -> Result<Range, MalformedRange> {
    ordered(r.start_line, r.start_character, r.end_line, r.end_character)
}

/// The occurrence's site: `typed_range` when emitted, else `range`; `Ok(None)` when the
/// producer emitted neither.
///
/// # Errors
/// [`MalformedRange`] from the form that was read (a malformed typed range is not silently
/// replaced by the legacy one: the producer's preferred form is the one we trust or refuse).
pub fn occurrence_range(
    o: &AdmittedOccurrence<'_>,
) -> Result<Option<(Range, RangeForm)>, MalformedRange> {
    let typed = match o.typed_range() {
        Some(Typed_range::SingleLineRange(r)) => Some(single(r)),
        Some(Typed_range::MultiLineRange(r)) => Some(multi(r)),
        // the oneof is `#[non_exhaustive]`: a future variant is no range we understand
        Some(_) | None => None,
    };
    match typed {
        Some(r) => Ok(Some((r?, RangeForm::Typed))),
        None => Ok(legacy(o.range())?.map(|r| (r, RangeForm::Legacy))),
    }
}

/// The occurrence's enclosing range: `typed_enclosing_range` when emitted, else
/// `enclosing_range`; `Ok(None)` when the producer emitted neither.
///
/// # Errors
/// [`MalformedRange`] from the form that was read.
pub fn enclosing(o: &AdmittedOccurrence<'_>) -> Result<Option<(Range, RangeForm)>, MalformedRange> {
    let typed = match o.typed_enclosing_range() {
        Some(Typed_enclosing_range::SingleLineEnclosingRange(r)) => Some(single(r)),
        Some(Typed_enclosing_range::MultiLineEnclosingRange(r)) => Some(multi(r)),
        Some(_) | None => None,
    };
    match typed {
        Some(r) => Ok(Some((r?, RangeForm::Typed))),
        None => Ok(legacy(o.enclosing_range())?.map(|r| (r, RangeForm::Legacy))),
    }
}

/// `inner` lies inside `outer` (both end-exclusive; equal ranges are inside). Both ranges are
/// ordered (this module admits no other), so the predicate is a containment, not a wrap.
#[must_use]
pub fn contains(outer: Range, inner: Range) -> bool {
    (outer.sl, outer.sc) <= (inner.sl, inner.sc) && (inner.el, inner.ec) <= (outer.el, outer.ec)
}
