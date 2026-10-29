//! Bounded Rust test facts from committed source. A file whose source hits the policy is
//! classified by its path only and counted (`policy_skipped_source`, DF-3); so is a file over the
//! per-file bound (`MAX_RAW_DELIMITERS`, `MAX_TOKENS`: `source_classified_by_path`). This is not a semantic
//! indexer: it marks existing SCIP symbols/sites; it emits no identities or call edges.
//! Exact `#[test]` and `#[cfg(test)]` items/inline modules are supported. Compound cfg,
//! `cfg_attr`, macro expansion and out-of-line module inheritance are not inferred.

use std::{borrow::Cow, path::Path};

use kaban_codegraph::{NodeKind, Range};
use kaban_core::process::InheritedFds;
use kaban_policy::Policy;
use proc_macro2::{Span, TokenStream, TokenTree};
use syn::spanned::Spanned;

use crate::derive::Nodes;
use crate::writer::Commit;
use crate::{IndexError, identity_cli, ranges};

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 1024;
// DF-3: measured over kaban's own 632 committed files (max 26178 tokens, 4951 raw open
// delimiters): the bounds admit them with headroom; a file beyond either is classified by its
// path only (`Outcome::OverBound`), never refused.
const MAX_TOKENS: usize = 131_072;
const MAX_DEPTH: usize = 64;
// Before tokenization, conservatively count raw open delimiters even in strings/comments.
// proc_macro2 tokenization is iterative, but a rejected TokenStream's drop may recurse.
// This caps that tree before it exists; the token-aware depth limit below is tighter.
const MAX_RAW_DELIMITERS: usize = 16_384;

pub(crate) fn classify(
    nodes: &mut Nodes,
    repo: &Path,
    commit: &Commit,
    policy: &Policy,
    inherited_fds: InheritedFds,
) -> Result<(), IndexError> {
    let mut total = 0;
    let mut files = 0;
    for file in &mut nodes.files {
        if !file.language.eq_ignore_ascii_case("rust") {
            continue;
        }
        files += 1;
        if files > MAX_FILES {
            return Err(IndexError::SourceClassification);
        }
        // FileRow paths already passed the structural/formed gates and relative-path rule.
        // Commit is the validated 40-hex type: neither revision nor path is shell syntax.
        let bytes = identity_cli::git_show_with(repo, commit.as_str(), &file.path, inherited_fds)
            .map_err(|_| IndexError::SourceClassification)?;
        total += bytes.len();
        if bytes.len() > MAX_SOURCE_BYTES || total > MAX_TOTAL_BYTES || bytes.contains(&0) {
            return Err(IndexError::SourceClassification);
        }
        let source = std::str::from_utf8(&bytes).map_err(|_| IndexError::SourceClassification)?;
        // Scan the entire original before parsing. A hit (redact or reject) skips the file's
        // in-file classification: parsing masked text would silently shift UTF-8 offsets or
        // change syntax, and a hit value is never processed further. The file keeps its
        // path-based marking; counted and reported, never a refusal (DF-3). No source is
        // persisted.
        let verdict = policy.scan_windows(source, policy.file.defaults.max_field_chars);
        if verdict.rejected || !verdict.hits.is_empty() {
            nodes.counts.policy_skipped_source += 1;
            for rule in verdict
                .reject_rules
                .into_iter()
                .chain(verdict.hits.into_iter().map(|hit| hit.rule))
            {
                if !nodes.source_rule_ids.contains(&rule) {
                    nodes.source_rule_ids.push(rule);
                }
            }
            continue;
        }
        // syn is recursive; bound input/tokens/depth and give its parser/drop a fixed stack.
        // Exactly one worker exists at a time, returning only numeric spans, never an AST.
        let outcome = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(64 * 1024 * 1024)
                .spawn_scoped(scope, || parse(source))
                .map_err(|_| IndexError::SourceClassification)?
                .join()
                .map_err(|_| IndexError::SourceClassification)?
        })?;
        let facts = match outcome {
            Outcome::Facts(facts) => facts,
            Outcome::OverBound => {
                // the path rule's `is_test` and the producer's `Test` roles (transferred by
                // `derive::definition_ranges`, df23-5) stand; counted and reported, never a
                // refusal
                nodes.counts.source_classified_by_path += 1;
                continue;
            }
        };
        file.is_test |= facts.whole_file;
        for symbol in nodes
            .symbols
            .iter_mut()
            .filter(|s| s.file_id == Some(file.id))
        {
            if symbol.kind == NodeKind::File {
                symbol.is_test = file.is_test;
            } else if !symbol.external {
                symbol.is_test |= file.is_test
                    || symbol.def.is_some_and(|def| {
                        facts
                            .scopes
                            .iter()
                            .any(|scope| ranges::contains(*scope, def))
                    });
            }
        }
        nodes.test_scopes.insert(file.id, facts.scopes);
    }
    Ok(())
}

struct Facts {
    whole_file: bool,
    scopes: Vec<Range>,
}

/// What the bounded parse of one file yields.
enum Outcome {
    /// Parsed within the bounds: the file's test facts.
    Facts(Facts),
    /// Over `MAX_RAW_DELIMITERS` or `MAX_TOKENS`: no facts, the caller keeps the path rule.
    OverBound,
}

fn parse(source: &str) -> Result<Outcome, IndexError> {
    if source
        .bytes()
        .filter(|b| matches!(b, b'(' | b'[' | b'{'))
        .take(MAX_RAW_DELIMITERS + 1)
        .count()
        > MAX_RAW_DELIMITERS
    {
        return Ok(Outcome::OverBound);
    }
    let parser_source = parser_source(source);
    let tokens: TokenStream = parser_source
        .parse()
        .map_err(|_| IndexError::SourceClassification)?;
    let mut count = 0;
    let mut pending = vec![(tokens.clone().into_iter(), 0)];
    while let Some((mut iter, depth)) = pending.pop() {
        for token in iter.by_ref() {
            count += 1;
            if count > MAX_TOKENS {
                return Ok(Outcome::OverBound);
            }
            if depth > MAX_DEPTH {
                return Err(IndexError::SourceClassification);
            }
            if let TokenTree::Group(group) = token {
                pending.push((iter, depth));
                pending.push((group.stream().into_iter(), depth + 1));
                break;
            }
        }
    }
    let ast: syn::File = syn::parse2(tokens).map_err(|_| IndexError::SourceClassification)?;
    let mut facts = Facts {
        whole_file: test_attrs(&ast.attrs),
        scopes: Vec::new(),
    };
    collect(&ast.items, source, &mut facts.scopes)?;
    Ok(Outcome::Facts(facts))
}

/// Blank file-only prefixes, retaining every byte offset and newline in the original.
/// Bounds and policy always inspect `source`, not this parser-only view.
fn parser_source(source: &str) -> Cow<'_, str> {
    let content = source.strip_prefix('\u{feff}').unwrap_or(source);
    let bom = source.len() - content.len();
    let end = if let Some(rest) = content.strip_prefix("#!")
        && !skip_prefix_trivia(rest).starts_with('[')
    {
        bom + content.find('\n').unwrap_or(content.len())
    } else {
        bom
    };
    if end == 0 {
        Cow::Borrowed(source)
    } else {
        let mut normalized = source.to_owned();
        normalized.replace_range(..end, &" ".repeat(end));
        Cow::Owned(normalized)
    }
}

// As for Rust-file parsing, whitespace and ordinary comments may separate #! from
// an inner attribute's [. Doc comments are tokens, not trivia. Nested block comments
// are scanned iteratively; the original-source raw-delimiter cap already ran.
fn skip_prefix_trivia(mut rest: &str) -> &str {
    loop {
        rest = rest.trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}')
        });
        if rest.starts_with("//")
            && !rest.starts_with("//!")
            && (!rest.starts_with("///") || rest.starts_with("////"))
        {
            rest = rest.find('\n').map_or("", |end| &rest[end + 1..]);
        } else if rest.starts_with("/*")
            && !rest.starts_with("/*!")
            && (!rest.starts_with("/**") || rest.starts_with("/***") || rest.starts_with("/**/"))
        {
            let mut depth = 1;
            let mut end = 2;
            let bytes = rest.as_bytes();
            while depth > 0 && end + 1 < bytes.len() {
                match &bytes[end..end + 2] {
                    b"/*" => {
                        depth += 1;
                        end += 2;
                    }
                    b"*/" => {
                        depth -= 1;
                        end += 2;
                    }
                    _ => end += 1,
                }
            }
            if depth != 0 {
                return rest;
            }
            rest = &rest[end..];
        } else {
            return rest;
        }
    }
}

fn test_attrs(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("test")
            || (attr.path().is_ident("cfg")
                && attr
                    .parse_args::<syn::Path>()
                    .is_ok_and(|p| p.is_ident("test")))
    })
}

fn collect(items: &[syn::Item], source: &str, scopes: &mut Vec<Range>) -> Result<(), IndexError> {
    for item in items {
        let attrs = match item {
            syn::Item::Const(v) => &v.attrs,
            syn::Item::Enum(v) => &v.attrs,
            syn::Item::ExternCrate(v) => &v.attrs,
            syn::Item::Fn(v) => &v.attrs,
            syn::Item::ForeignMod(v) => &v.attrs,
            syn::Item::Impl(v) => &v.attrs,
            syn::Item::Macro(v) => &v.attrs,
            syn::Item::Mod(v) => &v.attrs,
            syn::Item::Static(v) => &v.attrs,
            syn::Item::Struct(v) => &v.attrs,
            syn::Item::Trait(v) => &v.attrs,
            syn::Item::TraitAlias(v) => &v.attrs,
            syn::Item::Type(v) => &v.attrs,
            syn::Item::Union(v) => &v.attrs,
            syn::Item::Use(v) => &v.attrs,
            _ => continue,
        };
        if test_attrs(attrs) {
            scopes.push(source_range(item.span(), source)?);
        }
        if let syn::Item::Mod(module) = item
            && let Some((_, items)) = &module.content
        {
            collect(items, source, scopes)?;
        }
    }
    Ok(())
}

fn source_range(span: Span, source: &str) -> Result<Range, IndexError> {
    // proc_macro2 columns count characters; byte_range is exact outside a proc macro.
    // Convert offsets ourselves so non-ASCII prefixes never move SCIP byte columns.
    let bytes = span.byte_range();
    let position = |offset| {
        let prefix = source
            .get(..offset)
            .ok_or(IndexError::SourceClassification)?;
        let line = prefix.bytes().filter(|b| *b == b'\n').count();
        let col = prefix.rsplit('\n').next().map_or(0, str::len);
        Ok::<_, IndexError>((
            u32::try_from(line).map_err(|_| IndexError::SourceClassification)?,
            u32::try_from(col).map_err(|_| IndexError::SourceClassification)?,
        ))
    };
    let (sl, sc) = position(bytes.start)?;
    let (el, ec) = position(bytes.end)?;
    Ok(Range { sl, sc, el, ec })
}
