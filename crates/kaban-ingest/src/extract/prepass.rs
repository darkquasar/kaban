//! Obsidian pre-pass (docs-ingest-spec D1): `[[Note]]`, `[[Note#Heading]]`, `[[Note^block]]`,
//! `[[Note|alias]]`, `![[embed]]`, `#tag` and nested `#a/b` found by regex over the Markdown
//! source, skipping every byte range the event walk marked as code (spans and fences),
//! frontmatter, HTML or a standard link, so nothing inside a code span or fence is a link or
//! tag. Offsets are byte offsets into the source.

use super::{AnchorKind, LinkDraft, TagDraft};
use crate::cancel::{ExtractCtx, ExtractError};
use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// `![[target|alias]]`: group 1 the embed bang, 2 the target (with any `#heading` or `^block`),
/// 3 the alias.
static WIKILINK: LazyLock<Regex> = LazyLock::new(|| {
    // Invariant: a literal pattern compiled once; covered by the unit tests below.
    #[allow(clippy::expect_used)]
    Regex::new(r"(!?)\[\[([^\[\]|]+?)(?:\|([^\[\]]*?))?\]\]").expect("wikilink regex compiles")
});

/// `#tag` at the start, after whitespace or after `(`, `,`, `;`, `:`; letters, digits, `_`,
/// `-` and `/`. All-digit "tags" (`#1`) are issue numbers, not tags, and are filtered below.
static TAG: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"(?:^|[\s(,;:])#([\p{L}\p{N}_/-]+)").expect("tag regex compiles")
});

/// Byte ranges of the source the pre-pass must not read (code, frontmatter, HTML, links).
/// Pushed in event order, then [`Excluded::sorted`] merges them once so a lookup is a binary
/// search instead of a scan of every range (RD1-9: `n` candidates times `m` ranges was
/// quadratic in input the file-size cap does not bound tightly enough).
#[derive(Debug, Default)]
pub(super) struct Excluded(Vec<Range<usize>>);

impl Excluded {
    pub(super) fn push(&mut self, range: Range<usize>) {
        self.0.push(range);
    }

    /// Sort by start and merge overlapping or touching ranges; the result answers
    /// [`SortedExcluded::covers`] by binary search.
    pub(super) fn sorted(mut self) -> SortedExcluded {
        self.0.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(self.0.len());
        for r in self.0 {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        SortedExcluded(merged)
    }
}

/// [`Excluded`] after sorting and merging: disjoint ranges in ascending order.
#[derive(Debug, Default)]
pub(super) struct SortedExcluded(Vec<Range<usize>>);

impl SortedExcluded {
    fn covers(&self, at: usize) -> bool {
        // the last range starting at or before `at` is the only one that can contain it
        let i = self.0.partition_point(|r| r.start <= at);
        i > 0 && self.0[i - 1].contains(&at)
    }
}

/// Candidates examined between two `ctx.check()` calls (a bounded batch, DI0.3).
const CHECK_EVERY_CANDIDATES: usize = 1024;

/// Links and tags of `source` outside the excluded ranges, in source order. `ctx.check()` runs
/// every [`CHECK_EVERY_CANDIDATES`] regex matches, so cancellation and the deadline bound this
/// phase like the event walk (RD1-9).
///
/// # Errors
/// `Cancelled`/`DeadlineExceeded` from `ctx`.
pub(super) fn scan(
    source: &str,
    excluded: &SortedExcluded,
    ctx: &ExtractCtx,
) -> Result<(Vec<LinkDraft>, Vec<TagDraft>), ExtractError> {
    let mut seen = 0usize;
    let mut tick = |ctx: &ExtractCtx| -> Result<(), ExtractError> {
        seen += 1;
        if seen.is_multiple_of(CHECK_EVERY_CANDIDATES) {
            ctx.check()?;
        }
        Ok(())
    };
    let mut links = Vec::new();
    for caps in WIKILINK.captures_iter(source) {
        tick(ctx)?;
        let whole = caps.get(0).map_or(0..0, |m| m.range());
        if excluded.covers(whole.start) {
            continue;
        }
        let embed = caps.get(1).is_some_and(|m| !m.as_str().is_empty());
        let target = caps.get(2).map_or("", |m| m.as_str()).trim();
        if target.is_empty() {
            continue;
        }
        let anchor_kind = if target.contains('^') {
            AnchorKind::Block
        } else if target.contains('#') {
            AnchorKind::Heading
        } else {
            AnchorKind::Note
        };
        links.push(LinkDraft {
            raw_target: target.to_owned(),
            anchor_kind,
            alias: caps
                .get(3)
                .map(|m| m.as_str().trim())
                .filter(|a| !a.is_empty())
                .map(str::to_owned),
            embed,
            byte_offset: whole.start as u64,
            byte_end: whole.end as u64,
        });
    }
    let mut tags = Vec::new();
    for caps in TAG.captures_iter(source) {
        tick(ctx)?;
        let Some(name) = caps.get(1) else { continue };
        let hash_at = name.start() - 1;
        if excluded.covers(hash_at) {
            continue;
        }
        let tag = name.as_str().trim_end_matches('/');
        if tag.is_empty() || tag.chars().all(|c| c.is_ascii_digit() || c == '/') {
            continue;
        }
        tags.push(TagDraft {
            tag: tag.to_owned(),
            byte_offset: hash_at as u64,
            byte_end: (hash_at + 1 + tag.len()) as u64,
        });
    }
    Ok((links, tags))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::SourceId;
    use kaban_core::WorkspaceId;

    fn ctx() -> ExtractCtx {
        let gated = crate::hash::gate_path(&kaban_policy::Policy::empty(), "t.md").unwrap();
        ExtractCtx::new(
            tokio_util::sync::CancellationToken::new(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
            SourceId::new(&WorkspaceId::default(), &gated),
        )
    }

    #[test]
    fn sorted_exclusions_merge_overlaps_and_answer_coverage_by_binary_search() {
        let mut ex = Excluded::default();
        ex.push(10..20);
        ex.push(0..5);
        ex.push(15..25);
        ex.push(25..30);
        let sorted = ex.sorted();
        assert_eq!(sorted.0, vec![0..5, 10..30]);
        for at in [0, 4, 10, 19, 24, 29] {
            assert!(sorted.covers(at), "{at}");
        }
        for at in [5, 9, 30, 100] {
            assert!(!sorted.covers(at), "{at}");
        }
    }

    #[test]
    fn issue_numbers_heading_markers_and_url_fragments_are_not_tags() {
        let (_, tags) = scan(
            "# Title\n\nfix #1 and #42, see https://x.y/#frag and &#39; but #real and #a/b/\n",
            &SortedExcluded::default(),
            &ctx(),
        )
        .unwrap();
        let names: Vec<&str> = tags.iter().map(|t| t.tag.as_str()).collect();
        assert_eq!(names, vec!["real", "a/b"]);
    }

    #[test]
    fn an_empty_wikilink_is_ignored() {
        let (links, _) = scan(
            "[[]] and [[ ]] and [[ok]]",
            &SortedExcluded::default(),
            &ctx(),
        )
        .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].raw_target, "ok");
    }
}
