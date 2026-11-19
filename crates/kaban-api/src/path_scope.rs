//! The canonical form of a record's `path_scope`: the paths and globs it is about, part of
//! the near-duplicate key of a kind with `dedup: title+scope`. Path scoping (governing paths,
//! currency against committed history) is gone (spec 11 PC-20); the canonical set stays.

/// The canonical set of a `path_scope` (M3 review R3-6): `\` becomes `/` and an entry named
/// twice is kept once (first occurrence order). Applied before persistence (`remember`, a
/// structured submission).
#[must_use]
pub fn canonical_path_scope(entries: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for e in entries {
        let c = e.replace('\\', "/");
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::canonical_path_scope;

    #[test]
    fn backslashes_become_slashes_and_a_repeated_entry_is_kept_once() {
        let entries = ["src\\api\\**", "docs/*.md", "src/api/**", "docs/*.md"].map(String::from);
        assert_eq!(canonical_path_scope(&entries), ["src/api/**", "docs/*.md"]);
    }
}
