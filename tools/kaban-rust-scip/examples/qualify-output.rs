//! Read-only decoded-fact qualification of the independent original fixtures.
use anyhow::{Context, Result, ensure};
use protobuf::Message;
use scip::types::Index;
use std::{collections::BTreeSet, fs, path::Path};

fn name(symbol: &str) -> String {
    scip::symbol::parse_symbol(symbol)
        .ok()
        .and_then(|symbol| symbol.descriptors.last().map(|d| d.name.clone()))
        .unwrap_or_default()
}

fn offset(source: &str, line: i32, column: i32) -> usize {
    source
        .split_inclusive('\n')
        .take(line as usize)
        .map(str::len)
        .sum::<usize>()
        + column as usize
}

fn span(source: &str, range: &[i32]) -> (usize, usize) {
    match *range {
        [line, start, end] => (offset(source, line, start), offset(source, line, end)),
        [sl, sc, el, ec] => (offset(source, sl, sc), offset(source, el, ec)),
        _ => panic!("invalid SCIP range"),
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: qualify-output PROFILE INDEX SOURCE_ROOT"
    );
    let index = Index::parse_from_bytes(&fs::read(&args[2])?)?;
    let root = Path::new(&args[3]);
    let mut definitions = Vec::new();
    for doc in &index.documents {
        let source = fs::read_to_string(root.join(&doc.relative_path))?;
        for occurrence in &doc.occurrences {
            let (start, end) = span(&source, &occurrence.range);
            ensure!(source.get(start..end).is_some(), "invalid UTF-8 occurrence");
            ensure!(
                !source[start..end].trim().is_empty(),
                "occurrence must not reference whitespace: {} {:?}",
                doc.relative_path,
                occurrence.range
            );
            if occurrence.symbol_roles & 1 == 0 && name(&occurrence.symbol) == "add" {
                ensure!(
                    matches!(&source[start..end], "+" | "+="),
                    "Add reference must cover the exact operator"
                );
            }
            if occurrence.symbol_roles & 1 != 0 {
                let (body_start, body_end) = span(&source, &occurrence.enclosing_range);
                ensure!(
                    body_start <= start
                        && end <= body_end
                        && source.get(body_start..body_end).is_some(),
                    "invalid definition encloser"
                );
                definitions.push((doc, occurrence));
            }
        }
    }
    let named = |n: &str| {
        definitions
            .iter()
            .filter(|(_, o)| name(&o.symbol) == n)
            .map(|(_, o)| o.symbol.clone())
            .collect::<BTreeSet<_>>()
    };
    let unique = |n: &str| -> Result<String> {
        let set = named(n);
        ensure!(set.len() == 1, "expected one {n}, got {set:?}");
        set.into_iter().next().context("definition absent")
    };
    let reference = |from: &str, to: &str| -> Result<()> {
        let from = unique(from)?;
        let to = unique(to)?;
        let (doc, def) = definitions
            .iter()
            .find(|(_, o)| o.symbol == from)
            .context("caller absent")?;
        let source = fs::read_to_string(root.join(&doc.relative_path))?;
        let (bs, be) = span(&source, &def.enclosing_range);
        ensure!(
            doc.occurrences.iter().any(|o| {
                let (s, e) = span(&source, &o.range);
                o.symbol == to && o.symbol_roles & 1 == 0 && bs <= s && e <= be
            }),
            "missing reference {from} -> {to}"
        );
        Ok(())
    };
    match args[1].as_str() {
        "operator-ranges" => {
            unique("ordinary")?;
            unique("expanded")?;
            unique("ordinary_body")?;
            ensure!(
                named("inactive_item").is_empty(),
                "inactive block item was indexed"
            );
            let doc = index
                .documents
                .iter()
                .find(|doc| doc.relative_path == "src/lib.rs")
                .context("operator document absent")?;
            let source = fs::read_to_string(root.join("src/lib.rs"))?;
            let actual: BTreeSet<_> = doc
                .occurrences
                .iter()
                .filter(|occurrence| {
                    occurrence.symbol_roles & 1 == 0 && name(&occurrence.symbol) == "add"
                })
                .map(|occurrence| span(&source, &occurrence.range))
                .collect();
            let expected: BTreeSet<_> = source
                .match_indices('+')
                .map(|(offset, _)| (offset, offset + 1))
                .collect();
            ensure!(
                actual == expected && actual.len() == 2,
                "ordinary/expanded operator ranges differ: {actual:?} != {expected:?}"
            );
        }
        "minimal" => {
            for n in [
                "DatabaseDriver",
                "Postgres",
                "Sqlite",
                "NotADriver",
                "production",
                "middle",
                "leaf",
                "function_value",
                "café",
                "new",
                "production_works",
                "integration_works",
            ] {
                unique(n)?;
            }
            ensure!(
                named("execute").len() == 3,
                "trait and two member definitions must be distinct"
            );
            // R-2: an inherent member is a child of its type: its symbol is the ADT symbol plus
            // the member descriptor, and every reference site carries that same symbol
            let postgres = unique("Postgres")?;
            let new = unique("new")?;
            ensure!(
                new == format!("{postgres}new()."),
                "inherent member must be ADT-parented: {new} is not {postgres}new()."
            );
            reference("fresh", "new")?;
            let trait_ = unique("DatabaseDriver")?;
            // backlog 14: a declaration's `///` doc reaches the SCIP (first paragraph)
            let documentation = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .find(|s| s.symbol == trait_)
                .map(|s| s.documentation.clone())
                .context("DatabaseDriver row absent")?;
            ensure!(
                documentation
                    == vec![
                        "A driver with two implementations and a deliberately unrelated type."
                            .to_owned()
                    ],
                "the doc comment of DatabaseDriver: {documentation:?}"
            );
            let implementations: BTreeSet<_> = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .filter(|s| {
                    s.relationships
                        .iter()
                        .any(|r| r.is_implementation && r.symbol == trait_)
                })
                .map(|s| name(&s.symbol))
                .collect();
            ensure!(
                implementations == BTreeSet::from(["Postgres".into(), "Sqlite".into()]),
                "wrong concrete implementations: {implementations:?}"
            );
            let method_edges = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .filter(|s| name(&s.symbol) == "execute")
                .flat_map(|s| &s.relationships)
                .filter(|r| r.is_implementation)
                .count();
            ensure!(
                method_edges == 2,
                "missing typed method implementation edges"
            );
            let trait_method = index
                .documents
                .iter()
                .flat_map(|doc| &doc.symbols)
                .find(|symbol| {
                    name(&symbol.symbol) == "execute"
                        && symbol.kind.enum_value_or_default()
                            == scip::types::symbol_information::Kind::TraitMethod
                })
                .context("typed trait execute endpoint absent")?;
            let library_source = fs::read_to_string(root.join("src/lib.rs"))?;
            for concrete in ["Postgres", "Sqlite"] {
                let implementation = library_source
                    .find(&format!("impl DatabaseDriver for {concrete}"))
                    .context("oracle impl source")?;
                let method_offset = implementation
                    + library_source[implementation..]
                        .find("execute")
                        .context("oracle method source")?;
                let (_, method) = definitions
                    .iter()
                    .find(|(doc, occurrence)| {
                        doc.relative_path == "src/lib.rs"
                            && span(&library_source, &occurrence.range).0 == method_offset
                    })
                    .context("located concrete method absent")?;
                let information = index
                    .documents
                    .iter()
                    .flat_map(|doc| &doc.symbols)
                    .find(|symbol| symbol.symbol == method.symbol)
                    .context("method information absent")?;
                let endpoints: Vec<_> = information
                    .relationships
                    .iter()
                    .filter(|relationship| relationship.is_implementation)
                    .map(|relationship| relationship.symbol.as_str())
                    .collect();
                ensure!(
                    endpoints == vec![trait_method.symbol.as_str()],
                    "wrong resolved {concrete}::execute -> DatabaseDriver::execute endpoint"
                );
            }
            for (from, to) in [
                ("production", "middle"),
                ("middle", "leaf"),
                ("function_value", "leaf"),
                ("production_works", "production"),
                ("integration_works", "production"),
            ] {
                reference(from, to)?;
            }
            let cafe = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "café")
                .context("café absent")?;
            let source = fs::read_to_string(root.join(&cafe.0.relative_path))?;
            let (s, e) = span(&source, &cafe.1.range);
            ensure!(
                &source[s..e] == "café" && e - s == 5,
                "café must use UTF-8 byte columns"
            );
            let local = cafe
                .0
                .occurrences
                .iter()
                .find(|o| {
                    o.symbol.starts_with("local ") && o.symbol_roles & 1 != 0 && {
                        let (s, e) = span(&source, &o.range);
                        &source[s..e] == "local"
                    }
                })
                .context("local definition absent")?;
            ensure!(
                cafe.0
                    .occurrences
                    .iter()
                    .any(|o| o.symbol == local.symbol && o.symbol_roles & 1 == 0),
                "local use absent"
            );
            ensure!(
                index
                    .documents
                    .iter()
                    .flat_map(|d| &d.occurrences)
                    .any(|o| name(&o.symbol) == "size_of"
                        && o.symbol.starts_with("rust-analyzer ")
                        && o.symbol_roles & 1 == 0),
                "external size_of absent"
            );
            for n in ["production_works", "integration_works"] {
                ensure!(
                    definitions
                        .iter()
                        .find(|(_, o)| name(&o.symbol) == n)
                        .is_some_and(|(_, o)| o.symbol_roles & 32 != 0),
                    "test definition role absent"
                );
            }
            ensure!(
                definitions
                    .iter()
                    .find(|(_, o)| name(&o.symbol) == "production")
                    .is_some_and(|(_, o)| o.symbol_roles & 32 == 0),
                "production incorrectly test"
            );
        }
        "workspace" => {
            unique("answer")?;
            unique("base_value")?;
            unique("generated_value")?;
            reference("answer", "base_value")?;
            reference("answer", "generated_value")?;
            let (doc, generated) = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "generated_value")
                .context("generated absent")?;
            let source = fs::read_to_string(root.join(&doc.relative_path))?;
            let (s, e) = span(&source, &generated.range);
            ensure!(
                source[s..e].contains("make_value"),
                "generated definition must be at invocation"
            );
        }
        "target-contexts" => {
            ensure!(named("common").len() == 2, "same-name target collision");
            let library = definitions
                .iter()
                .find(|(doc, occurrence)| {
                    doc.relative_path == "src/lib.rs" && name(&occurrence.symbol) == "common"
                })
                .context("library common definition absent")?
                .1
                .symbol
                .clone();
            let binary = definitions
                .iter()
                .find(|(doc, occurrence)| {
                    doc.relative_path == "src/main.rs" && name(&occurrence.symbol) == "common"
                })
                .context("binary common definition absent")?
                .1
                .symbol
                .clone();
            ensure!(
                library != binary,
                "same-named lib/bin definitions collapsed"
            );
            let integration = index
                .documents
                .iter()
                .find(|doc| doc.relative_path == "tests/integration.rs")
                .context("integration document absent")?;
            ensure!(
                integration
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.symbol == library
                        && occurrence.symbol_roles & 1 == 0),
                "integration reference did not resolve to library target"
            );
            ensure!(
                !integration
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.symbol == binary),
                "integration incorrectly references binary target"
            );
            let main = index
                .documents
                .iter()
                .find(|doc| doc.relative_path == "src/main.rs")
                .context("binary document absent")?;
            ensure!(
                main.occurrences
                    .iter()
                    .any(|occurrence| occurrence.symbol == library
                        && occurrence.symbol_roles & 1 == 0)
                    && main
                        .occurrences
                        .iter()
                        .any(|occurrence| occurrence.symbol == binary
                            && occurrence.symbol_roles & 1 == 0),
                "binary must preserve distinct library and local reference endpoints"
            );
        }
        "semantic-extra" => {
            for n in ["unused_generated", "UnusedGenerated"] {
                let (doc, generated) = definitions
                    .iter()
                    .find(|(_, o)| name(&o.symbol) == n)
                    .context("unused generated definition absent")?;
                let source = fs::read_to_string(root.join(&doc.relative_path))?;
                let (s, e) = span(&source, &generated.range);
                ensure!(
                    source[s..e].contains("generated!"),
                    "unused generated definition not at invocation"
                );
            }
            for n in ["field", "Choice", "Alpha", "value", "Beta"] {
                unique(n)?;
            }
            // backlog 7: a nominal tuple field (a tuple struct's, a tuple variant's) is declared
            // under its owner by ordinal and `.0` on the struct resolves to it; a plain tuple's
            // `.0` has no identity (counted under `tokens_without_symbol`, astra §5: a separate
            // policy)
            let wrapper = unique("Wrapper")?;
            let pair_both = unique("Both")?;
            let wrapper_field = format!("{wrapper}0.");
            ensure!(
                definitions.iter().any(|(_, o)| o.symbol == wrapper_field)
                    && definitions
                        .iter()
                        .any(|(_, o)| o.symbol == format!("{pair_both}1.")),
                "tuple fields declared by ordinal (Wrapper#0., Pair#Both#1.): {:?}",
                definitions
                    .iter()
                    .filter(|(_, o)| o.symbol.ends_with("0.") || o.symbol.ends_with("1."))
                    .map(|(_, o)| o.symbol.clone())
                    .collect::<Vec<_>>()
            );
            let (doc, tuple_fields) = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "tuple_fields")
                .context("tuple_fields absent")?;
            let source = fs::read_to_string(root.join(&doc.relative_path))?;
            let (bs, be) = span(&source, &tuple_fields.enclosing_range);
            let body = &source[bs..be];
            let inside: Vec<_> = doc
                .occurrences
                .iter()
                .filter(|o| {
                    let (s, e) = span(&source, &o.range);
                    o.symbol_roles & 1 == 0 && bs <= s && e <= be
                })
                .map(|o| {
                    let (s, e) = span(&source, &o.range);
                    (&source[s..e], o.symbol.clone(), s - bs)
                })
                .collect();
            let wrapper_access = body.find("wrapper.0").context("wrapper.0 absent")? + 8;
            let plain_access = body.find("plain.0").context("plain.0 absent")? + 6;
            ensure!(
                inside.iter().any(|(text, symbol, at)| *text == "0"
                    && *symbol == wrapper_field
                    && *at == wrapper_access),
                "wrapper.0 does not resolve to {wrapper_field}: {inside:?}"
            );
            ensure!(
                !inside.iter().any(|(_, _, at)| *at == plain_access),
                "a plain tuple's .0 was given an identity: {inside:?}"
            );
            let edges: Vec<_> = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .flat_map(|s| {
                    s.relationships
                        .iter()
                        .filter(|r| r.is_implementation)
                        .map(move |r| (name(&s.symbol), name(&r.symbol)))
                })
                .collect();
            ensure!(
                edges
                    == vec![
                        ("Concrete".into(), "Marker".into()),
                        ("mark".into(), "mark".into())
                    ],
                "nonconcrete/conditional edges emitted: {edges:?}"
            );
        }
        "cargo-contexts" => {
            unique("production_only")?;
            unique("main")?;
            unique("disabled_library_test")?;
            unique("forbidden_test_cfg")?;
            reference("main", "production_only")?;
        }
        "local-contexts" => {
            ensure!(named("same").len() == 2, "both module functions required");
            let mut local_symbols = BTreeSet::new();
            for path in ["src/one.rs", "src/two.rs"] {
                let doc = index
                    .documents
                    .iter()
                    .find(|doc| doc.relative_path == path)
                    .context("module document absent")?;
                let local = doc
                    .occurrences
                    .iter()
                    .find(|occurrence| {
                        occurrence.symbol.starts_with("local ") && occurrence.symbol_roles & 1 != 0
                    })
                    .context("document-local definition absent")?;
                ensure!(
                    local_symbols.insert(local.symbol.clone()),
                    "cross-document local key collision"
                );
                ensure!(
                    doc.occurrences
                        .iter()
                        .any(|occurrence| occurrence.symbol == local.symbol
                            && occurrence.symbol_roles & 1 == 0),
                    "local use resolves outside its document"
                );
            }
        }
        "block-references" => {
            // df23-1: a block-local item has no module-level identity, nothing refers to it,
            // and the tokens inside its body are not references of the enclosing item
            for n in ["leaf", "first", "second", "third", "fourth"] {
                unique(n)?;
            }
            for n in ["Reentrant", "descend", "get", "generated"] {
                ensure!(
                    named(n).is_empty(),
                    "block-local item {n} published as a module-level identity: {:?}",
                    named(n)
                );
                ensure!(
                    !index
                        .documents
                        .iter()
                        .flat_map(|d| &d.occurrences)
                        .any(|o| name(&o.symbol) == n),
                    "a reference to block-local item {n} was published"
                );
            }
            reference("third", "leaf")?;
            let leaf = unique("leaf")?;
            let doc = index
                .documents
                .iter()
                .find(|doc| doc.relative_path == "src/lib.rs")
                .context("library document absent")?;
            let source = fs::read_to_string(root.join("src/lib.rs"))?;
            let (_, second) = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "second")
                .context("second absent")?;
            let (bs, be) = span(&source, &second.enclosing_range);
            let sites: Vec<_> = doc
                .occurrences
                .iter()
                .filter(|o| o.symbol == leaf && o.symbol_roles & 1 == 0)
                .map(|o| span(&source, &o.range))
                .collect();
            ensure!(
                !sites.iter().any(|(s, e)| bs <= *s && *e <= be),
                "the call inside block-local descend is attributed to second: {sites:?}"
            );
            ensure!(
                sites.len() == 1,
                "leaf must have exactly one reference site (inside third): {sites:?}"
            );
            // a non-ASCII local beside a macro call keeps UTF-8 byte columns (Task 0 guard)
            let cafe = doc
                .occurrences
                .iter()
                .find(|o| {
                    o.symbol.starts_with("local ") && o.symbol_roles & 1 != 0 && {
                        let (s, e) = span(&source, &o.range);
                        &source[s..e] == "café"
                    }
                })
                .context("non-ASCII local beside a macro call absent")?;
            ensure!(
                doc.occurrences
                    .iter()
                    .any(|o| o.symbol == cafe.symbol && o.symbol_roles & 1 == 0),
                "non-ASCII local use absent"
            );
        }
        "shared-contexts" => {
            // df23-2: one physical shared file, one owner context: its declarations and its
            // references are published for the owner only (the other context is counted)
            unique("library")?;
            let helper = unique("helper")?;
            ensure!(
                named("owner").len() == 2,
                "each integration test target declares its own owner: {:?}",
                named("owner")
            );
            let common = index
                .documents
                .iter()
                .find(|doc| doc.relative_path == "tests/common/mod.rs")
                .context("shared document absent")?;
            let owners: BTreeSet<_> = common
                .occurrences
                .iter()
                .filter(|o| name(&o.symbol) == "owner" && o.symbol_roles & 1 == 0)
                .map(|o| o.symbol.clone())
                .collect();
            ensure!(
                owners.len() == 1,
                "crate::owner() in the shared file must resolve in exactly one context: {owners:?}"
            );
            let target = |symbol: &str| {
                scip::symbol::parse_symbol(symbol)
                    .ok()
                    .and_then(|symbol| symbol.descriptors.first().map(|d| d.name.clone()))
            };
            let owner = owners.iter().next().context("owner reference absent")?;
            ensure!(
                target(&helper) == target(owner),
                "shared-file declarations must follow the owner context: {helper} vs {owner}"
            );
            let foreign: Vec<_> = definitions
                .iter()
                .filter(|(doc, o)| {
                    doc.relative_path == "tests/common/mod.rs" && target(&o.symbol) != target(owner)
                })
                .map(|(_, o)| (o.symbol.clone(), o.range.clone()))
                .collect();
            ensure!(
                foreign.is_empty(),
                "declarations of the shared file published for a non-owner context: {foreign:?}"
            );
        }
        "member-collisions" => {
            // r2-1: an associated const keeps its `impl#` identity (a field and a const both map
            // to a Term descriptor); methods and associated types are children of the type
            let s = unique("S")?;
            let xs = named("x");
            ensure!(
                xs.len() == 2,
                "field x and const x must stay distinct: {xs:?}"
            );
            ensure!(
                xs.contains(&format!("{s}x.")) && xs.iter().any(|x| x.contains("impl#[S]x.")),
                "field is `S#x.`, const is `impl#[S]x.`: {xs:?}"
            );
            ensure!(
                unique("Unit")? == format!("{s}Unit#"),
                "associated type is a child of the type"
            );
            // r2-2: cfg-exclusive inherent impls of one type share `S#m().`; the first
            // declaration location is kept (counted), never a refusal
            ensure!(
                unique("m")? == format!("{s}m()."),
                "method is a child of the type"
            );
            reference("use_members", "m")?;
            let (doc, m) = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "m")
                .context("m absent")?;
            let source = fs::read_to_string(root.join(&doc.relative_path))?;
            let (bs, be) = span(&source, &m.enclosing_range);
            ensure!(
                source[bs..be].contains("self.x"),
                "the production (first context) declaration of m is the one kept"
            );
        }
        "cfg-conflict" => {
            // two cfg alternatives of one fn: one identity, the first location kept, counted
            let (doc, selected) = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "selected")
                .context("selected absent")?;
            ensure!(
                named("selected").len() == 1,
                "one identity for the cfg alternatives"
            );
            let source = fs::read_to_string(root.join(&doc.relative_path))?;
            let (bs, be) = span(&source, &selected.enclosing_range);
            ensure!(
                source[bs..be].contains("{ 1 }"),
                "the production alternative is the location kept"
            );
        }
        "derive-impls" => {
            // R-3: an impl inside an anonymous const at module level (the shape serde's derives
            // expand to) is published with its trait and self type; the helper beside it in the
            // const stays a skipped block item
            let plain = |n: &str| {
                definitions
                    .iter()
                    .filter(|(_, o)| name(&o.symbol) == n && !o.symbol.contains("impl#"))
                    .map(|(_, o)| o.symbol.clone())
                    .collect::<BTreeSet<_>>()
            };
            let one = |n: &str| -> Result<String> {
                let set = plain(n);
                ensure!(set.len() == 1, "expected one {n}, got {set:?}");
                set.into_iter().next().context("definition absent")
            };
            let marker = one("Marker")?;
            for n in ["X", "Y", "Z", "V", "W", "Q", "use_all"] {
                one(n)?;
            }
            ensure!(
                named("helper").is_empty() && named("inner").is_empty(),
                "a helper beside the derived impl or inside a fn body was published: {:?} {:?}",
                named("helper"),
                named("inner")
            );
            let implementers: BTreeSet<_> = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .filter(|s| {
                    s.relationships
                        .iter()
                        .any(|r| r.is_implementation && r.symbol == marker)
                })
                .map(|s| name(&s.symbol))
                .collect();
            ensure!(
                implementers
                    == BTreeSet::from([
                        "X".into(),
                        "Y".into(),
                        "Z".into(),
                        "V".into(),
                        "Q".into(),
                        "P".into()
                    ]),
                "derived implementers of Marker (X, Y, Z, the cfg_attr-derived V, the qualified crate::Q, P under a pass-through attribute; never the generic W<T>): {implementers:?}"
            );
            // consumer request 3: `impl<'a> Marker2<'a> for X` carries no predicate and holds
            // for every lifetime, so it is unconditional and published; `impl<T>` stays skipped
            let marker2 = one("Marker2")?;
            let lifetime_only: BTreeSet<_> = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .filter(|s| {
                    s.relationships
                        .iter()
                        .any(|r| r.is_implementation && r.symbol == marker2)
                })
                .map(|s| name(&s.symbol))
                .collect();
            ensure!(
                lifetime_only == BTreeSet::from(["X".into()]),
                "the lifetime-only impl of Marker2 for X is unconditional; the inline bound (X2) and the lifetime where-clause (X3) are predicates: {lifetime_only:?}"
            );
            // fixg-1: a bound the trait declares on its lifetime (`Marker3<'a: 'static>`) is a
            // predicate the impl restates (X4, skipped); an impl with a concrete lifetime and no
            // parameters carries none (X5, published)
            let marker3 = one("Marker3")?;
            let trait_bound: BTreeSet<_> = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .filter(|s| {
                    s.relationships
                        .iter()
                        .any(|r| r.is_implementation && r.symbol == marker3)
                })
                .map(|s| name(&s.symbol))
                .collect();
            ensure!(
                trait_bound == BTreeSet::from(["X5".into()]),
                "implementers of the lifetime-bounded Marker3 (the concrete X5, never the bounded X4): {trait_bound:?}"
            );
            // r3-4: an accepted impl whose self type is declared elsewhere (std's Duration, the
            // alias target Instant) carries its edge on an external symbol, never on a
            // fabricated local declaration
            let external: BTreeSet<_> = index
                .external_symbols
                .iter()
                .filter(|s| {
                    s.relationships
                        .iter()
                        .any(|r| r.is_implementation && r.symbol == marker)
                })
                .map(|s| {
                    ensure!(
                        s.kind.enum_value_or_default()
                            == scip::types::symbol_information::Kind::Struct,
                        "external implementer without its kind: {}",
                        s.symbol
                    );
                    ensure!(
                        s.symbol.starts_with("rust-analyzer cargo "),
                        "external row with a local symbol: {}",
                        s.symbol
                    );
                    Ok(name(&s.symbol))
                })
                .collect::<Result<_>>()?;
            ensure!(
                external == BTreeSet::from(["Duration".into(), "Instant".into()]),
                "external implementers of Marker (Duration, Instant): {external:?}"
            );
            // fixg-2: `#[cfg_attr(all(), cfg_attr(test, derive(Marked)), derive(Marked2))] struct N`:
            // the second derive is numbered by its syntax slot, not by the expansions the
            // context answered for, so N's MarkerB impl has one identity in production (Marked
            // skipped) and in test (Marked expanded first), and its member edge survives
            let n_impls: BTreeSet<_> = definitions
                .iter()
                .filter(|(_, o)| o.symbol.ends_with(":impl#[N][MarkerB]"))
                .map(|(_, o)| o.symbol.clone())
                .collect();
            ensure!(
                n_impls.len() == 1,
                "one identity for N's derived MarkerB impl across the contexts: {n_impls:?}"
            );
            let trait_b = index
                .documents
                .iter()
                .flat_map(|doc| &doc.symbols)
                .find(|symbol| {
                    name(&symbol.symbol) == "b"
                        && symbol.kind.enum_value_or_default()
                            == scip::types::symbol_information::Kind::TraitMethod
                })
                .map(|symbol| symbol.symbol.clone())
                .context("trait b absent")?;
            let b_edges: Vec<_> = index
                .documents
                .iter()
                .flat_map(|doc| &doc.symbols)
                .filter(|s| name(&s.symbol) == "b" && s.symbol != trait_b)
                .flat_map(|s| &s.relationships)
                .filter(|r| r.is_implementation)
                .map(|r| r.symbol.clone())
                .collect();
            ensure!(
                b_edges == vec![trait_b.clone()],
                "N#b -> MarkerB#b survives the two contexts: {b_edges:?}"
            );
            // backlog 14: an undocumented alias and an impl publish no documentation (never
            // the aliased or implemented type's)
            let undocumented: Vec<_> = index
                .documents
                .iter()
                .flat_map(|d| &d.symbols)
                .filter(|s| name(&s.symbol) == "Instant" || s.symbol.contains(":impl#["))
                .filter(|s| !s.documentation.is_empty())
                .map(|s| (s.symbol.clone(), s.documentation.clone()))
                .collect();
            ensure!(
                undocumented.is_empty(),
                "borrowed documentation on an alias or an impl: {undocumented:?}"
            );
            // the impl declarations: one per anonymous const, under its module, qualified by the
            // const's lexical slot in the module's syntax (r3-1: the item index, then for a
            // derive the meta slot among the item's attributes, the position in the derive list
            // and the expansion-local ordinal (fixg-2); cfg-disabled items keep their slot, so
            // the test-only `T0` before `X` leaves `X` one identity in both contexts)
            let impls: BTreeSet<_> = definitions
                .iter()
                .filter(|(_, o)| o.symbol.contains(":impl#[") && o.symbol.ends_with("][Marker]"))
                .map(|(_, o)| o.symbol.clone())
                .collect();
            ensure!(
                impls.len() == 11,
                "eleven anonymous-const impls of Marker (T0, N test-only), one identity each: {impls:?}"
            );
            for (slot, self_type) in [
                ("1-1-0-0", "T0"),
                ("2-0-0-0", "X"),
                ("3-0-0-0", "Z"),
                ("4-0-0-0", "V"),
                ("28-0-0-0", "N"),
                ("6", "Y"),
                ("8", "`W<T>`"),
                ("11", "`std::time::Duration`"),
                ("13", "`std::time::Instant`"),
                ("15", "Q"),
                // fixg-3: the const under an attribute macro is the first in its expansion
                ("30-0", "P"),
            ] {
                let expected = format!("$const{slot}:impl#[{self_type}][Marker]");
                ensure!(
                    impls.iter().any(|s| s.contains(&expected)),
                    "impl declaration {expected} absent: {impls:?}"
                );
            }
            // member implementation: the derived X#m and Z#m implement Marker#m
            ensure!(
                named("m").len() == 7,
                "trait m, the five derived m (T0, N test-only) and P's must be distinct: {:?}",
                named("m")
            );
            let trait_m = index
                .documents
                .iter()
                .flat_map(|doc| &doc.symbols)
                .find(|symbol| {
                    name(&symbol.symbol) == "m"
                        && symbol.kind.enum_value_or_default()
                            == scip::types::symbol_information::Kind::TraitMethod
                })
                .map(|symbol| symbol.symbol.clone())
                .context("trait m absent")?;
            let member_edges: Vec<_> = index
                .documents
                .iter()
                .flat_map(|doc| &doc.symbols)
                .filter(|s| name(&s.symbol) == "m" && s.symbol != trait_m)
                .flat_map(|s| &s.relationships)
                .filter(|r| r.is_implementation)
                .map(|r| r.symbol.clone())
                .collect();
            ensure!(
                member_edges == vec![trait_m.clone(); 4],
                "the m -> Marker#m edges of X, Z, V and P: {member_edges:?}"
            );
            // a call to a derived member resolves to it: `X.m()` inside use_all
            let derived_m = named("m")
                .into_iter()
                .find(|s| s.contains("[X]"))
                .context("X's m absent")?;
            let (doc, use_all) = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "use_all")
                .context("use_all absent")?;
            let source = fs::read_to_string(root.join(&doc.relative_path))?;
            let (bs, be) = span(&source, &use_all.enclosing_range);
            ensure!(
                doc.occurrences.iter().any(|o| {
                    let (s, e) = span(&source, &o.range);
                    o.symbol == derived_m && o.symbol_roles & 1 == 0 && bs <= s && e <= be
                }),
                "X.m() inside use_all does not resolve to the derived member {derived_m}"
            );
            // r3-3: `X.m()` inside the skipped helper `inner` (a block item of `outer`) keeps no
            // occurrence, whatever its target: the helper has no identity, so the consumer would
            // attribute the call to `outer`
            let (doc, outer) = definitions
                .iter()
                .find(|(_, o)| name(&o.symbol) == "outer")
                .context("outer absent")?;
            let source = fs::read_to_string(root.join(&doc.relative_path))?;
            let (bs, be) = span(&source, &outer.enclosing_range);
            let escaped: Vec<_> = doc
                .occurrences
                .iter()
                .filter(|o| {
                    let (s, e) = span(&source, &o.range);
                    o.symbol_roles & 1 == 0 && bs <= s && e <= be && o.symbol == derived_m
                })
                .map(|o| o.range.clone())
                .collect();
            ensure!(
                escaped.is_empty(),
                "a reference inside the skipped helper escaped (attributed to outer): {escaped:?}"
            );
        }
        _ => anyhow::bail!("unknown oracle profile"),
    }
    println!("{}", protobuf::text_format::print_to_string_pretty(&index));
    eprintln!(
        "decoded independent {} oracle passed: {} documents, {} definitions",
        args[1],
        index.documents.len(),
        definitions.len()
    );
    Ok(())
}
