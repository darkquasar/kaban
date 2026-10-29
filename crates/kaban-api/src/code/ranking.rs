//! Deterministic ordering before the cut (stage1-spec G1-F12, G1-F16; RV-7: no list is cut
//! before this call; stage1-plan CG7.1, CG8.1). Every candidate's distance from the caller's
//! `current_path` is computed first, then the complete set is sorted by `(distance, path, ref)`
//! (a card without a path first within its distance, as the reader sorts) and cut to the limit.
//! Without `current_path` every distance is 0 and the order is CG7.1's `(path, ref)`. The count
//! boost is gone (OWN-68 SC-10).

use kaban_codegraph::{Limits, check_repo_relative_path};

use super::{CodeError, CodeNode, TestLink};

/// What the ranking reads: the caller's `current_path`, gated by the handler and checked here
/// by the repository-relative path rule ([`RankInputs::checked`]) before any lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RankInputs {
    current_path: Option<String>,
}

impl RankInputs {
    /// The inputs from a gated `current_path`: `None` ranks by `(path, ref)` alone; a value
    /// must pass the repository-relative path rule (no `..` component, no leading `/`, not
    /// empty, not over-long, no control character), else `selector_invalid`.
    ///
    /// # Errors
    /// [`CodeError::SelectorInvalid`]: the fixed class, the value is never echoed.
    pub(crate) fn checked(current_path: Option<String>) -> Result<Self, CodeError> {
        if let Some(p) = current_path.as_deref() {
            check_repo_relative_path(p).map_err(|_| CodeError::SelectorInvalid)?;
        }
        Ok(Self { current_path })
    }

    /// The distance term of a card: [`path_distance`] from `current_path` to the card's path;
    /// 0 without `current_path`, and 0 for a card without a path (an external placeholder has
    /// no file to measure, so it keeps the reader's place at the head of its distance class).
    pub(crate) fn distance(&self, card: &CodeNode) -> u32 {
        match (self.current_path.as_deref(), card.path.as_deref()) {
            (Some(from), Some(to)) => path_distance(from, to),
            _ => 0,
        }
    }

    /// Fill `distance` on a card outside [`order_then_limit`]: the one card of
    /// `code_definition`, the enclosing cards of `code_references` (ranked by their handler),
    /// the nodes of `code_neighbours` (which keep their `(depth, ref)` order).
    pub(crate) fn fill(&self, card: &mut CodeNode) {
        card.distance = self.distance(card);
    }
}

/// G1-F16's distance between two repository-relative paths: 0 for the same file, 1 for the
/// same directory, otherwise 2 plus the number of directory components of either path outside
/// their longest common directory prefix. A root file has no directory components: two root
/// files are 1 apart, and a root file is `2 + depth` from a file `depth` directories down.
/// Empty components (`a//b.go`) are not counted.
pub(crate) fn path_distance(a: &str, b: &str) -> u32 {
    if a == b {
        return 0;
    }
    let dir_a: Vec<&str> = directory_components(a).collect();
    let dir_b: Vec<&str> = directory_components(b).collect();
    if dir_a == dir_b {
        return 1;
    }
    let common = dir_a
        .iter()
        .zip(dir_b.iter())
        .take_while(|(x, y)| x == y)
        .count();
    let outside = dir_a.len().saturating_sub(common) + dir_b.len().saturating_sub(common);
    u32::try_from(outside).map_or(u32::MAX, |n| n.saturating_add(2))
}

/// The non-empty directory components of a repository-relative path (none for a root file).
fn directory_components(path: &str) -> impl Iterator<Item = &str> {
    path.rsplit_once('/')
        .map_or("", |(dir, _)| dir)
        .split('/')
        .filter(|c| !c.is_empty())
}

/// An item a list tool ranks: the card itself, or a link that carries one.
pub(crate) trait Ranked {
    /// The card the order reads.
    fn card(&self) -> &CodeNode;
    /// The card whose `distance` is filled.
    fn card_mut(&mut self) -> &mut CodeNode;
}

impl Ranked for CodeNode {
    fn card(&self) -> &CodeNode {
        self
    }

    fn card_mut(&mut self) -> &mut CodeNode {
        self
    }
}

impl Ranked for TestLink {
    fn card(&self) -> &CodeNode {
        &self.test
    }

    fn card_mut(&mut self) -> &mut CodeNode {
        &mut self.test
    }
}

/// The effective list limit: `None` or `0` is the default, anything above the maximum is the
/// maximum (`Limits::DEFAULT`, G1-F12).
pub(crate) fn effective_limit(limit: Option<usize>) -> usize {
    let limits = Limits::DEFAULT;
    match limit {
        None | Some(0) => limits.default,
        Some(n) => n.min(limits.max),
    }
}

/// Fill every item's distance, order the complete set (stable, by `(distance, path, ref)`),
/// then keep the first `limit`; `truncated` says rows beyond the limit existed.
pub(crate) fn order_then_limit<T: Ranked>(
    mut items: Vec<T>,
    inputs: &RankInputs,
    limit: Option<usize>,
) -> (Vec<T>, bool) {
    for item in &mut items {
        inputs.fill(item.card_mut());
    }
    items.sort_by(|a, b| {
        let (a, b) = (a.card(), b.card());
        a.distance
            .cmp(&b.distance)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.r#ref.cmp(&b.r#ref))
    });
    let limit = effective_limit(limit);
    let truncated = items.len() > limit;
    items.truncate(limit);
    (items, truncated)
}

/// Fill every item's distance and keep the first `limit` in the order given (a list whose
/// order is the reader's, such as a file's members by range); `truncated` as
/// [`order_then_limit`].
pub(crate) fn fill_then_limit<T: Ranked>(
    mut items: Vec<T>,
    inputs: &RankInputs,
    limit: Option<usize>,
) -> (Vec<T>, bool) {
    for item in &mut items {
        inputs.fill(item.card_mut());
    }
    let limit = effective_limit(limit);
    let truncated = items.len() > limit;
    items.truncate(limit);
    (items, truncated)
}

#[cfg(test)]
mod tests {
    use super::path_distance;

    /// G1-F16's table, root files included; every pair is symmetric.
    #[test]
    fn path_distance_table() {
        let table: [(&str, &str, u32); 13] = [
            // the same file
            ("a.go", "a.go", 0),
            ("pkg/a/x.go", "pkg/a/x.go", 0),
            // the same directory (two root files are a directory too)
            ("a.go", "b.go", 1),
            ("pkg/a/x.go", "pkg/a/y.go", 1),
            // sibling directories: 2 + 1 + 1
            ("pkg/a/x.go", "pkg/b/y.go", 4),
            ("x/a.go", "y/a.go", 4),
            // parent and child directory: 2 + 1 + 0
            ("pkg/a/x.go", "pkg/y.go", 3),
            ("pkg/x.go", "pkg/a/y.go", 3),
            // a root file against a nested one: 2 + 0 + depth
            ("main.go", "pkg/x.go", 3),
            ("main.go", "pkg/a/x.go", 4),
            ("pkg/a/x.go", "main.go", 4),
            // a deeper fork: 2 + 1 + 2
            ("a/b/c/d.go", "a/b/e/f/g.go", 5),
            // empty components are not counted
            ("pkg//a/x.go", "pkg/a/y.go", 1),
        ];
        for (a, b, want) in table {
            assert_eq!(path_distance(a, b), want, "{a} -> {b}");
            assert_eq!(path_distance(b, a), want, "{b} -> {a}");
        }
    }
}
