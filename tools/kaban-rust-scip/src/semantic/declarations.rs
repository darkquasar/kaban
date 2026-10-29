//! Located semantic declaration inventory and conservative cross-cfg equivalence.
//!
//! Per-target/context semantic facts are reconciled before deterministic SCIP
//! serialization. Files and tokens are never globally deduplicated before HIR.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::Path,
};

use anyhow::{Context as _, Result, ensure};
use ra_ap_hir::{
    Adt, GenericDef, HasSource, HirFileId, Impl, InFile, Module, ModuleDef, ModuleSource, Semantics,
};
use ra_ap_ide::{MonikerDescriptorKind, MonikerResult, TryToNav, UpmappingResult};
use ra_ap_ide_db::{
    RootDatabase,
    defs::{Definition, IdentClass},
};
use ra_ap_syntax::{
    AstNode, SyntaxNode,
    ast::{self, HasAttrs, HasGenericParams, HasModuleItem, HasTypeBounds},
};
use ra_ap_vfs::Vfs;
use scip::types::{Descriptor, descriptor::Suffix};

use super::targets::Context;

#[derive(Debug, Eq, PartialEq)]
struct Location {
    path: String,
    focus: (u32, u32),
    body: (u32, u32),
    signature: String,
}

struct Declaration {
    location: Location,
    /// The first paragraph of the doc comment (backlog 14), the first context's; not part of
    /// the divergence check.
    documentation: Option<String>,
    cfgs: BTreeSet<Vec<String>>,
    kind: scip::types::symbol_information::Kind,
}

/// The most of a doc comment's first paragraph that is published (`SymbolInformation
/// .documentation`); the consumer bounds the joined block again.
const MAX_DOCUMENTATION_BYTES: usize = 1024;

#[derive(Default)]
pub(super) struct Inventory {
    /// Active block-local items left out of the inventory (DF-2; reported, not refused): every
    /// visit, across contexts.
    skipped_block_items: usize,
    /// The same items once each, by file and source range, with their packages (R-1, r1-2).
    block_items: super::BlockItems,
    /// Reference tokens left without an occurrence for want of a stable symbol (DF-2).
    skipped_tokens: usize,
    /// The same, by the kind of definition the token resolved to (backlog 7: what is actually
    /// missing, a plain tuple's `.0` today; nominal tuple fields have an identity).
    skipped_token_kinds: BTreeMap<&'static str, usize>,
    /// Visits of a file shared by several crates in a crate other than its resolving one (DF-2).
    skipped_shared_file_contexts: usize,
    /// Modules indexed with an unresolved import, module, extern crate or macro (DF-2).
    incomplete_modules: usize,
    /// Function bodies whose reference set differs between cfg contexts (DF-2).
    divergent_bodies: usize,
    /// Reference tokens left without an occurrence because they refer to a block-local item or
    /// lie inside one (df23-1): the item has no published identity, so the reference would be a
    /// wrong fact (two block-local `struct Reentrant` collapsing into one module-level symbol, a
    /// call inside a block-local fn attributed to the enclosing fn).
    block_item_references: usize,
    /// Identities declared at different locations (or with different signatures) in different
    /// cfg contexts (r2-2): the first location is kept and the divergence counted, not refused.
    divergent_declarations: usize,
    /// Occurrences left out because their position is not a character boundary of the
    /// document text that was analysed (FIX-B Task 0; counted, not refused).
    skipped_positions: usize,
    /// Impls inside anonymous consts (the derive shape, R-3) whose endpoints contract v1
    /// rejects (type or const parameters, predicates, negative; lifetime-only parameters are
    /// admitted): declared, no `IMPLEMENTS` edge, counted.
    derived_impls_generic_skipped: usize,
    /// Impls inside anonymous consts the lexical walk did not reach (R-3; not published, noted).
    derived_impls_unordered: usize,
    /// The text of every committed document as rust-analyzer loaded it, by relative path:
    /// positions are converted against this text, never against the file as it is on disk at
    /// encoding time (an in-place checkout may have been edited since loading).
    sources: BTreeMap<String, String>,
    /// Detailed diagnostics (at most eight per class), reported after the coverage summary.
    notes: Vec<String>,
    declarations: BTreeMap<String, Declaration>,
    references: BTreeMap<(String, u32, u32, String), BTreeSet<super::targets::Mode>>,
    relationships: BTreeMap<
        (String, String),
        (
            BTreeSet<super::targets::Mode>,
            BTreeSet<super::targets::Mode>,
        ),
    >,
    /// The kind of every relationship source type (r3-4: a self type declared in no indexed
    /// context, an external ADT or an alias to one, is serialized as an external symbol and
    /// has no declaration to take its kind from).
    source_kinds: BTreeMap<String, scip::types::symbol_information::Kind>,
    bodies: BTreeMap<String, BTreeSet<(u32, u32, String)>>,
}

impl Inventory {
    pub(super) fn collect(
        &mut self,
        db: &RootDatabase,
        vfs: &Vfs,
        context: &Context,
        checkout: &Path,
        catalog: &super::targets::Catalog,
    ) -> Result<()> {
        let sema = Semantics::new(db);
        let mut context_references = BTreeMap::<String, BTreeSet<(u32, u32, String)>>::new();
        let mut local_definitions = BTreeMap::new();
        let mut inactive = Vec::new();
        let mut visited = HashSet::new();
        for module in context.krate.modules(db) {
            // One physical file, one owner context (df23-2): a module whose file rust-analyzer
            // resolves in another crate first (`tests/common/mod.rs` of several integration
            // tests) publishes nothing in this context, its declarations included, so two
            // targets never publish one enclosing range the consumer must break ties between.
            // The visit is counted once, in the file pass below.
            let file_id = module
                .definition_source_file_id(db)
                .original_file(db)
                .file_id(db);
            let owner = sema.file_to_module_defs(file_id).next();
            if owner.is_some_and(|owner| owner.krate(db) != context.krate) {
                continue;
            }
            visited.insert(module);
            let mut diagnostics = Vec::new();
            module.diagnostics(db, &mut diagnostics, false);
            for diagnostic in &diagnostics {
                if let ra_ap_hir::AnyDiagnostic::InactiveCode(code) = diagnostic {
                    ensure!(
                        !code.node.file_id.is_macro(),
                        "unsupported cfg-dependent macro expansion body"
                    );
                    inactive.push((code.node.file_id, code.node.value.text_range()));
                }
            }
            // DF-2: an unresolved import, module, extern crate or macro leaves this module's
            // references incomplete; the module is still indexed (its declarations are real)
            // and the condition is reported (at most eight lines), not refused.
            let incomplete: Vec<&'static str> = diagnostics
                .iter()
                .filter_map(|diagnostic| {
                    use ra_ap_hir::AnyDiagnostic as D;
                    match diagnostic {
                        D::MacroError(error) if error.error => Some("macro error"),
                        D::MacroDefError(_) => Some("macro definition error"),
                        D::MacroExpansionParseError(_) => Some("macro expansion parse error"),
                        D::UnresolvedMacroCall(_) => Some("unresolved macro call"),
                        D::UnresolvedExternCrate(_) => Some("unresolved extern crate"),
                        D::UnresolvedImport(_) => Some("unresolved import"),
                        D::UnresolvedModule(_) => Some("unresolved module"),
                        _ => None,
                    }
                })
                .collect();
            if let Some(first) = incomplete.first() {
                if self.incomplete_modules < 8 {
                    let name = module
                        .name(db)
                        .map_or_else(|| "crate root".to_owned(), |n| n.as_str().to_owned());
                    self.notes.push(format!(
                        "semantic incomplete resolution in module {name}: {first} ({} diagnostics)",
                        incomplete.len()
                    ));
                }
                self.incomplete_modules += 1;
            }
            self.add(db, vfs, &sema, context, catalog, Definition::Module(module))?;
            for declaration in module.declarations(db) {
                // a child module is added when the loop reaches it, under its own file's
                // ownership check, not as a declaration of its parent
                if matches!(
                    declaration,
                    ModuleDef::BuiltinType(_) | ModuleDef::Module(_)
                ) {
                    continue;
                }
                self.add(db, vfs, &sema, context, catalog, declaration.into())?;
                match declaration {
                    ModuleDef::Adt(Adt::Struct(it)) => {
                        for field in it.fields(db) {
                            self.add(db, vfs, &sema, context, catalog, Definition::Field(field))?;
                        }
                    }
                    ModuleDef::Adt(Adt::Union(it)) => {
                        for field in it.fields(db) {
                            self.add(db, vfs, &sema, context, catalog, Definition::Field(field))?;
                        }
                    }
                    ModuleDef::Adt(Adt::Enum(it)) => {
                        for variant in it.variants(db) {
                            self.add(
                                db,
                                vfs,
                                &sema,
                                context,
                                catalog,
                                Definition::EnumVariant(variant),
                            )?;
                            for field in variant.fields(db) {
                                self.add(
                                    db,
                                    vfs,
                                    &sema,
                                    context,
                                    catalog,
                                    Definition::Field(field),
                                )?;
                            }
                        }
                    }
                    _ => {}
                }
                if let ModuleDef::Trait(trait_) = declaration {
                    for item in trait_.items(db) {
                        self.add(db, vfs, &sema, context, catalog, item.into())?;
                    }
                }
            }
            for macro_ in module.legacy_macros(db) {
                self.add(db, vfs, &sema, context, catalog, Definition::Macro(macro_))?;
            }
            for implementation in module.impl_defs(db) {
                self.implementation(db, vfs, &sema, context, catalog, implementation)?;
            }
        }
        // R-3: an impl inside an anonymous const at module level (`const _: () = { impl
        // Serialize for X { .. } };`, the shape serde's, thiserror's and clap's derives expand
        // to) is reached through rust-analyzer's unnamed-const traversal (`Impl::all_in_crate`,
        // astra §3) and published like a module-level impl: its own declaration under the module
        // with the lexical scope of its const (`symbol_descriptors`), its members, and its
        // relationships through the same contract v1 endpoints; a generic or predicate-bearing
        // one keeps no edge and is counted. Impls inside fn bodies stay unpublished (R-4).
        for implementation in Impl::all_in_crate(db, context.krate) {
            let Some((_, module)) = anonymous_const_owner(db, implementation.module(db)) else {
                continue;
            };
            if !visited.contains(&module) {
                continue;
            }
            let definition = Definition::SelfType(implementation);
            if derived_impl_scope(db, definition).is_none() {
                // the const is not among the module's lexical anonymous consts (an expansion
                // the syntactic walk does not reach): without a lexical identity nothing of the
                // impl is published, and the reference filter drops its tokens as block items
                if self.derived_impls_unordered < 8 {
                    self.notes.push(format!(
                        "semantic derived impl without a lexical ordinal: {}",
                        definition.label(db, context.krate.to_display_target(db))
                    ));
                }
                self.derived_impls_unordered += 1;
                continue;
            }
            self.add(db, vfs, &sema, context, catalog, definition)?;
            if !self.implementation(db, vfs, &sema, context, catalog, implementation)? {
                self.derived_impls_generic_skipped += 1;
            }
        }
        // Enumerate committed files separately for every selected semantic root. A file
        // shared by several crates (`tests/common/mod.rs` of many integration tests, a
        // `#[path]` include of a bin and its lib) is visited in the context of the crate
        // rust-analyzer resolves it in (the first owner) and skipped, counted, in the others
        // (DF-2; reported, not refused): its declarations belong to that one target.
        for (file_id, path) in vfs.iter() {
            let Some(path) = path.as_path() else { continue };
            let path: &Path = path.as_ref();
            if !path.starts_with(checkout) {
                continue;
            }
            let parsed = sema.parse_guess_edition(file_id);
            let owners: Vec<_> = sema.file_to_module_defs(file_id).collect();
            if !owners
                .iter()
                .any(|module| module.krate(db) == context.krate)
            {
                continue;
            }
            if owners
                .first()
                .is_some_and(|module| module.krate(db) != context.krate)
            {
                self.skipped_shared_file_contexts += 1;
                continue;
            }
            let relative = path
                .strip_prefix(checkout)?
                .to_str()
                .context("non-UTF-8 source path")?;
            self.sources
                .entry(relative.to_owned())
                .or_insert_with(|| parsed.syntax().text().to_string());
            let mut spans = Vec::new();
            self.skipped_block_items +=
                block_items(&sema, parsed.syntax(), false, &inactive, 0, &mut spans)?;
            if !spans.is_empty() {
                let index = self
                    .sources
                    .get(relative)
                    .map(|text| ra_ap_ide_db::line_index::LineIndex::new(text));
                for span in spans {
                    // an item whose span maps into another file (an `include!`d one) has no
                    // span in this document and is left to the visit count
                    if span.file_id.file_id(db) != file_id {
                        continue;
                    }
                    if let Some(index) = &index {
                        let line = |offset| index.line_col(offset).line;
                        self.block_items.record(
                            context.identity.package_name(),
                            relative,
                            (span.range.start().into(), span.range.end().into()),
                            (line(span.range.start()), line(span.range.end())),
                        );
                    }
                }
            }
            let mut block_cache = std::collections::HashMap::new();
            for token in parsed
                .syntax()
                .descendants_with_tokens()
                .filter_map(|it| it.into_token())
            {
                let original_file = sema.hir_file_for(parsed.syntax());
                if inactive.iter().any(|(file, range)| {
                    *file == original_file && range.contains_range(token.text_range())
                }) {
                    continue;
                }
                // Active token ranges also catch literal-only body alternatives:
                // equal reference sets alone do not prove cfg/body equivalence.
                let active = token.text_range();
                context_references
                    .entry(relative.into())
                    .or_default()
                    .insert((active.start().into(), active.end().into(), String::new()));
                if token.kind().is_trivia() {
                    continue;
                }
                for expanded in sema.descend_into_macros_exact(token.clone()) {
                    if let Some(parent) = expanded.parent() {
                        let expanded_file = sema.hir_file_for(&parent);
                        if inactive.iter().any(|(file, range)| {
                            *file == expanded_file && range.contains_range(expanded.text_range())
                        }) {
                            continue;
                        }
                    }
                    let Some(class) = IdentClass::classify_token(&sema, &expanded) else {
                        continue;
                    };
                    if matches!(class, IdentClass::Operator(_)) {
                        if !is_operator_token(&expanded) {
                            continue;
                        }
                        ensure!(
                            token.text() == expanded.text(),
                            "unsupported expanded operator source mapping"
                        );
                    }
                    let in_block_item = inside_block_item(&sema, &expanded, &mut block_cache);
                    for (definition, _) in class.definitions() {
                        // Builtin language entities have no committed or external
                        // declaration; they are not dropped semantic references.
                        if matches!(
                            definition,
                            Definition::BuiltinType(_)
                                | Definition::BuiltinAttr(_)
                                | Definition::BuiltinLifetime(_)
                                | Definition::ToolModule(_)
                                | Definition::DeriveHelper(_)
                        ) {
                            continue;
                        }
                        // A block-local item is not published (`block_items`), so neither is a
                        // reference to it nor a token inside its body: absence is honest, a
                        // module-level identity or a wrong encloser is not (df23-1; counted).
                        // An impl inside an anonymous const is published (R-3), so a reference
                        // to its members keeps its occurrence, unless the reference itself lies
                        // inside a skipped body (r3-3: the encloser would be wrong whatever the
                        // target).
                        if in_block_item
                            || (definition
                                .module(db)
                                .is_some_and(|module| is_block_module(db, module))
                                && derived_impl_scope(db, definition).is_none())
                        {
                            self.block_item_references += 1;
                            continue;
                        }
                        // A token whose definition rust-analyzer cannot give a stable moniker
                        // or a local identity (a tuple-struct field such as `.0`) is left
                        // without an occurrence and counted (DF-2; reported, not refused).
                        if !matches!(
                            definition,
                            Definition::ExternCrateDecl(_) | Definition::Crate(_)
                        ) && MonikerResult::from_def(db, definition, context.krate).is_none()
                        {
                            self.skip_token(definition_name(definition));
                            continue;
                        }
                        let symbol = semantic_symbol(db, vfs, context, catalog, definition)?;
                        if symbol.starts_with("local ")
                            && let Some(prior) =
                                local_definitions.insert(symbol.clone(), definition)
                            && prior != definition
                        {
                            // two locals a macro expansion maps to one call-site offset: the
                            // second keeps no occurrence (DF-2; counted, not refused)
                            local_definitions.insert(symbol, prior);
                            self.skip_token("Local (one call-site offset)");
                            continue;
                        }
                        if matches!(
                            definition,
                            Definition::Local(_)
                                | Definition::GenericParam(_)
                                | Definition::Label(_)
                        ) && let Some(nav) =
                            definition.try_to_nav(&sema).map(UpmappingResult::call_site)
                        {
                            let nav_path = vfs.file_path(nav.file_id);
                            let nav_path: &Path = nav_path
                                .as_path()
                                .context("local source unavailable")?
                                .as_ref();
                            let focus = nav.focus_or_full_range();
                            let nav_relative: String = nav_path
                                .strip_prefix(checkout)?
                                .to_str()
                                .context("local path is not UTF-8")?
                                .into();
                            self.retain(&sema, nav.file_id, &nav_relative);
                            let target = context.krate.to_display_target(db);
                            self.merge(
                                symbol.clone(),
                                Location {
                                    path: nav_relative,
                                    focus: (focus.start().into(), focus.end().into()),
                                    body: (
                                        nav.full_range.start().into(),
                                        nav.full_range.end().into(),
                                    ),
                                    signature: definition.label(db, target),
                                },
                                documentation(db, definition, target),
                                context.cfg.clone(),
                            )?;
                            if let Some(declaration) = self.declarations.get_mut(&symbol) {
                                declaration.kind = definition_kind(definition, db);
                            }
                        }
                        let range = token.text_range();
                        context_references
                            .entry(relative.into())
                            .or_default()
                            .insert((range.start().into(), range.end().into(), symbol.clone()));
                        self.references
                            .entry((
                                relative.into(),
                                range.start().into(),
                                range.end().into(),
                                symbol,
                            ))
                            .or_default()
                            .insert(context.mode);
                    }
                }
            }
        }
        for (symbol, declaration) in &self.declarations {
            if !symbol.starts_with(&context.identity.symbol([])) {
                continue;
            }
            if !declaration.cfgs.contains(&context.cfg) {
                continue;
            }
            use scip::types::symbol_information::Kind as K;
            if !matches!(declaration.kind, K::Function | K::Method | K::TraitMethod) {
                continue;
            }
            let location = &declaration.location;
            let facts: BTreeSet<_> = context_references
                .get(&location.path)
                .into_iter()
                .flatten()
                .filter(|(start, end, _)| location.body.0 <= *start && *end <= location.body.1)
                .cloned()
                .collect();
            // DF-2: a body whose reference set differs between cfg contexts (a `#[cfg(test)]`
            // branch) is published as the union, every reference tagged with the modes it
            // appears in (`self.references`); the divergence is counted and the first symbols
            // are reported, not refused.
            if let Some(prior) = self.bodies.get(symbol) {
                if *prior != facts {
                    if self.divergent_bodies < 8 {
                        self.notes
                            .push(format!("semantic cfg/body divergent body: {symbol}"));
                    }
                    self.divergent_bodies += 1;
                }
            } else {
                self.bodies.insert(symbol.clone(), facts);
            }
        }
        Ok(())
    }

    /// Publish an impl's members and, for a concrete, positive, unqualified nominal self type
    /// (the only unconditional implementation endpoint in contract v1), the `X -> Trait`
    /// relationship and the member implementations `X#m() -> Trait#m()`. Whether the endpoints
    /// were accepted.
    fn implementation(
        &mut self,
        db: &RootDatabase,
        vfs: &Vfs,
        sema: &Semantics<'_, RootDatabase>,
        context: &Context,
        catalog: &super::targets::Catalog,
        implementation: Impl,
    ) -> Result<bool> {
        for item in implementation.items(db) {
            self.add(db, vfs, sema, context, catalog, item.into())?;
        }
        let Some((adt, trait_)) = implementation_endpoints(db, implementation) else {
            return Ok(false);
        };
        let from = semantic_symbol(db, vfs, context, catalog, Definition::Adt(adt))?;
        let to = semantic_symbol(db, vfs, context, catalog, Definition::Trait(trait_))?;
        self.source_kinds
            .insert(from.clone(), definition_kind(Definition::Adt(adt), db));
        self.relationship((from, to), context);
        for item in implementation.items(db) {
            let matching = trait_.items(db).into_iter().find(|other| {
                std::mem::discriminant(&item) == std::mem::discriminant(other)
                    && item.name(db) == other.name(db)
            });
            if let Some(other) = matching {
                self.relationship(
                    (
                        semantic_symbol(db, vfs, context, catalog, item.into())?,
                        semantic_symbol(db, vfs, context, catalog, other.into())?,
                    ),
                    context,
                );
            }
        }
        Ok(true)
    }

    fn add(
        &mut self,
        db: &RootDatabase,
        vfs: &Vfs,
        sema: &Semantics<'_, RootDatabase>,
        context: &Context,
        catalog: &super::targets::Catalog,
        definition: Definition<'_>,
    ) -> Result<()> {
        // An anonymous item (`const _: () = { ... }`, the shape serde's derives expand to) has
        // no name of its own: its moniker collapses onto the enclosing module's symbol, so it is
        // not an addressable declaration and nothing of it is published (DF-2).
        if let Definition::Const(konst) = definition
            && konst.name(db).is_none()
        {
            return Ok(());
        }
        let symbol = semantic_symbol(db, vfs, context, catalog, definition)?;
        let nav = definition
            .try_to_nav(sema)
            .map(UpmappingResult::call_site)
            .context("unsupported semantic declaration without a committed call-site location")?;
        let relative = catalog.relative_source(vfs, nav.file_id)?;
        let focus = nav.focus_or_full_range();
        let body = nav.full_range;
        let source = self.retain(sema, nav.file_id, &relative);
        for range in [focus, body] {
            let start = u32::from(range.start()) as usize;
            let end = u32::from(range.end()) as usize;
            ensure!(
                start <= end
                    && end <= source.len()
                    && source.is_char_boundary(start)
                    && source.is_char_boundary(end),
                "semantic declaration in {relative} has an invalid UTF-8 source range"
            );
        }
        let kind = definition_kind(definition, db);
        let target = context.krate.to_display_target(db);
        self.merge(
            symbol.clone(),
            Location {
                path: relative,
                focus: (focus.start().into(), focus.end().into()),
                body: (body.start().into(), body.end().into()),
                signature: definition.label(db, target),
            },
            documentation(db, definition, target),
            context.cfg.clone(),
        )?;
        if let Some(declaration) = self.declarations.get_mut(&symbol) {
            declaration.kind = kind;
        }
        Ok(())
    }

    /// The analysed text of a committed document, retained on first sight: positions are
    /// converted against it, never against the file as it is on disk at encoding time.
    fn retain(
        &mut self,
        sema: &Semantics<'_, RootDatabase>,
        file_id: ra_ap_vfs::FileId,
        relative: &str,
    ) -> &str {
        self.sources.entry(relative.to_owned()).or_insert_with(|| {
            sema.parse_guess_edition(file_id)
                .syntax()
                .text()
                .to_string()
        })
    }

    /// Record a declaration of `symbol` seen under `cfg`. One identity declared at another
    /// location (or with another signature) in another cfg context (`#[cfg(test)]` alternatives,
    /// cfg-exclusive inherent impls of one type, r2-2) keeps the first location; the divergence is
    /// counted (`divergent_declarations`) and the first eight are reported, never a refusal.
    fn merge(
        &mut self,
        symbol: String,
        location: Location,
        documentation: Option<String>,
        cfg: Vec<String>,
    ) -> Result<()> {
        if let Some(prior) = self.declarations.get_mut(&symbol) {
            if prior.documentation.is_none() {
                prior.documentation = documentation;
            }
            if prior.location != location {
                if self.divergent_declarations < 8 {
                    self.notes.push(format!(
                        "semantic cfg-divergent declaration: {symbol} kept at {}:{}-{} (cfg {:?}), also declared at {}:{}-{} (cfg {:?})",
                        prior.location.path,
                        prior.location.body.0,
                        prior.location.body.1,
                        prior.cfgs,
                        location.path,
                        location.body.0,
                        location.body.1,
                        cfg
                    ));
                }
                self.divergent_declarations += 1;
            }
            prior.cfgs.insert(cfg);
        } else {
            self.declarations.insert(
                symbol,
                Declaration {
                    location,
                    documentation,
                    cfgs: BTreeSet::from([cfg]),
                    kind: scip::types::symbol_information::Kind::UnspecifiedKind,
                },
            );
        }
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.declarations.len()
    }

    /// Active block-local items seen and left out, over every selected context.
    pub(super) fn skipped_block_items(&self) -> usize {
        self.skipped_block_items
    }

    pub(super) fn block_items(&self) -> &super::BlockItems {
        &self.block_items
    }

    /// Reference tokens left without an occurrence, over every selected context.
    pub(super) fn skipped_tokens(&self) -> usize {
        self.skipped_tokens
    }

    fn skip_token(&mut self, kind: &'static str) {
        self.skipped_tokens += 1;
        *self.skipped_token_kinds.entry(kind).or_default() += 1;
    }

    /// The note breaking `tokens_without_symbol` down by definition kind, `None` at zero.
    pub(super) fn skipped_token_note(&self) -> Option<String> {
        (!self.skipped_token_kinds.is_empty()).then(|| {
            let kinds: Vec<_> = self
                .skipped_token_kinds
                .iter()
                .map(|(kind, count)| format!("{kind}={count}"))
                .collect();
            format!(
                "semantic tokens without symbol by definition: {}",
                kinds.join(", ")
            )
        })
    }

    /// Visits of a shared file skipped in a crate other than its resolving one.
    pub(super) fn skipped_shared_file_contexts(&self) -> usize {
        self.skipped_shared_file_contexts
    }

    /// Modules indexed although a diagnostic left their resolution incomplete.
    pub(super) fn incomplete_modules(&self) -> usize {
        self.incomplete_modules
    }

    /// Function bodies published as the union of their per-context reference sets.
    pub(super) fn divergent_bodies(&self) -> usize {
        self.divergent_bodies
    }

    /// Occurrences left out because their position is off the analysed document text.
    pub(super) fn skipped_positions(&self) -> usize {
        self.skipped_positions
    }

    /// Reference tokens left out because they refer to, or lie inside, a block-local item.
    pub(super) fn block_item_references(&self) -> usize {
        self.block_item_references
    }

    /// Identities whose declaration location differs across cfg contexts (first kept).
    pub(super) fn divergent_declarations(&self) -> usize {
        self.divergent_declarations
    }

    /// Impls inside anonymous consts declared without an `IMPLEMENTS` edge (contract v1).
    pub(super) fn derived_impls_generic_skipped(&self) -> usize {
        self.derived_impls_generic_skipped
    }

    /// The detailed diagnostics gathered while collecting, in order.
    pub(super) fn notes(&self) -> &[String] {
        &self.notes
    }

    fn relationship(&mut self, pair: (String, String), context: &Context) {
        let (seen, required) = self.relationships.entry(pair).or_default();
        seen.insert(context.mode);
        required.extend(context.modes());
    }

    /// Serialize the inventory. A position that is not a character boundary of the analysed
    /// text (`sources`) drops its occurrence and is counted (`skipped_positions`), never a
    /// refusal; a document whose text was not retained is a refusal naming that document.
    pub(super) fn encode(&mut self, checkout: &Path) -> Result<Vec<u8>> {
        use protobuf::Message;
        use scip::types::{
            Document, Index, Metadata, Occurrence, PositionEncoding, Relationship, Signature,
            SymbolInformation, TextEncoding, ToolInfo,
        };
        let text = |path: &String| {
            self.sources
                .get(path)
                .map(String::as_str)
                .with_context(|| format!("no analysed text retained for document {path}"))
        };
        let mut skipped_positions = 0usize;
        let mut documents = BTreeMap::<String, Document>::new();
        for (symbol, declaration) in &self.declarations {
            let location = &declaration.location;
            ensure!(
                location.body.0 <= location.focus.0 && location.focus.1 <= location.body.1,
                "semantic definition focus is outside its enclosing range in {}",
                location.path
            );
            let source = text(&location.path)?;
            let (Some(range), Some(enclosing_range)) = (
                source_range(source, location.focus),
                source_range(source, location.body),
            ) else {
                skipped_positions += 1;
                continue;
            };
            let document = documents
                .entry(location.path.clone())
                .or_insert_with(|| Document {
                    relative_path: location.path.clone(),
                    language: "rust".into(),
                    position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                    ..Default::default()
                });
            document.occurrences.push(Occurrence {
                range,
                symbol: symbol.clone(),
                symbol_roles: 1 | if declaration
                    .cfgs
                    .iter()
                    .all(|cfg| cfg.iter().any(|atom| atom == "test"))
                {
                    32
                } else {
                    0
                },
                enclosing_range,
                ..Default::default()
            });
            document.symbols.push(SymbolInformation {
                symbol: symbol.clone(),
                kind: declaration.kind.into(),
                signature_documentation: Some(Signature {
                    language: "rust".into(),
                    text: location.signature.clone(),
                    ..Default::default()
                })
                .into(),
                documentation: declaration.documentation.iter().cloned().collect(),
                relationships: self
                    .relationships
                    .iter()
                    .filter(|((from, _), (seen, required))| {
                        from == symbol
                            && seen == required
                            && seen.contains(&super::targets::Mode::Production)
                    })
                    .map(|((_, to), _)| Relationship {
                        symbol: to.clone(),
                        is_implementation: true,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            });
        }
        // r3-4: an accepted implementation whose self type has no declaration in any indexed
        // context (an external ADT such as `std::time::Duration`, or an alias to one) carries
        // its surviving relationships on an `external_symbols` row: the edge reaches the
        // consumer, no definition location is invented
        let mut external_symbols = BTreeMap::<&String, Vec<Relationship>>::new();
        for ((from, to), (seen, required)) in &self.relationships {
            if self.declarations.contains_key(from)
                || seen != required
                || !seen.contains(&super::targets::Mode::Production)
            {
                continue;
            }
            external_symbols
                .entry(from)
                .or_default()
                .push(Relationship {
                    symbol: to.clone(),
                    is_implementation: true,
                    ..Default::default()
                });
        }
        let external_symbols = external_symbols
            .into_iter()
            .map(|(symbol, relationships)| SymbolInformation {
                symbol: symbol.clone(),
                kind: self
                    .source_kinds
                    .get(symbol)
                    .copied()
                    .unwrap_or_default()
                    .into(),
                relationships,
                ..Default::default()
            })
            .collect();
        for ((path, start, end, symbol), modes) in &self.references {
            let Some(range) = source_range(text(path)?, (*start, *end)) else {
                skipped_positions += 1;
                continue;
            };
            let document = documents.entry(path.clone()).or_insert_with(|| Document {
                relative_path: path.clone(),
                language: "rust".into(),
                position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                ..Default::default()
            });
            if !document.occurrences.iter().any(|occurrence| {
                occurrence.symbol == *symbol
                    && occurrence.range == range
                    && occurrence.symbol_roles & 1 != 0
            }) {
                document.occurrences.push(Occurrence {
                    range,
                    symbol: symbol.clone(),
                    symbol_roles: if modes.contains(&super::targets::Mode::Production) {
                        0
                    } else {
                        32
                    },
                    ..Default::default()
                });
            }
        }
        for document in documents.values_mut() {
            document.occurrences.sort_by(|a, b| {
                (&a.range, &a.symbol, a.symbol_roles).cmp(&(&b.range, &b.symbol, b.symbol_roles))
            });
            document.occurrences.dedup();
        }
        self.skipped_positions += skipped_positions;
        Index {
            metadata: Some(Metadata {
                project_root: project_root(checkout)?,
                text_document_encoding: TextEncoding::UTF8.into(),
                tool_info: Some(ToolInfo {
                    name: "kaban-rust-scip".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            })
            .into(),
            documents: documents.into_values().collect(),
            external_symbols,
            ..Default::default()
        }
        .write_to_bytes()
        .context("cannot encode SCIP")
    }
}

// Module enumeration does not include block def-maps: active items inside blocks (impls and
// declarations inside function bodies, unused items produced by recursively expanded expression
// macros included) have no stable nested identity or encloser yet, so they are not published.
// Since DF-2 (owner decision: indexing must not refuse code) they are counted and reported
// instead of refusing the run; expansion failures still refuse.
fn block_items(
    sema: &Semantics<'_, RootDatabase>,
    root: &ra_ap_syntax::SyntaxNode,
    in_block: bool,
    inactive: &[(ra_ap_hir::HirFileId, ra_ap_syntax::TextRange)],
    depth: usize,
    spans: &mut Vec<ra_ap_hir::FileRange>,
) -> Result<usize> {
    use ra_ap_syntax::ast;
    // an expansion chain deeper than 64 (a self-referential macro) is not inspected further:
    // block items are only counted since DF-2, so nothing is lost but the count
    if depth >= 64 {
        return Ok(0);
    }
    let file = sema.hir_file_for(root);
    let mut skipped = 0;
    for node in root.descendants() {
        if inactive
            .iter()
            .any(|(other, range)| *other == file && range.contains_range(node.text_range()))
        {
            continue;
        }
        let block = in_block
            || node
                .ancestors()
                .any(|it| ast::StmtList::can_cast(it.kind()));
        // an impl in an anonymous const at module level, and its members, are published (R-3)
        if block
            && ast::Item::can_cast(node.kind())
            && !ast::MacroCall::can_cast(node.kind())
            && !(!in_block && is_derived_impl_item(&node))
        {
            skipped += 1;
            // the item's span in the committed file, the macro call's for a generated item
            spans.push(sema.original_range(&node));
        }
        if let Some(item) = ast::Item::cast(node.clone())
            && let Some(expansion) = sema.expand_attr_macro(&item)
        {
            ensure!(
                expansion.err.is_none(),
                "unsupported incomplete attribute expansion"
            );
            skipped += block_items(
                sema,
                &expansion.value.value,
                block,
                inactive,
                depth + 1,
                spans,
            )?;
        }
        if let Some(meta) = ast::Meta::cast(node.clone())
            && let Some(expansions) = sema.expand_derive_macro(&meta)
        {
            for expansion in expansions.into_iter().flatten() {
                ensure!(
                    expansion.err.is_none(),
                    "unsupported incomplete derive expansion"
                );
                skipped += block_items(sema, &expansion.value, block, inactive, depth + 1, spans)?;
            }
        }
        if let Some(call) = ast::MacroCall::cast(node)
            && let Some(expansion) = sema.expand_macro_call(&call)
        {
            skipped += block_items(sema, &expansion.value, block, inactive, depth + 1, spans)?;
        }
    }
    Ok(skipped)
}

/// Syntactic twin of `anonymous_const_owner` for the block-item count: `node` is an impl in the
/// body of an anonymous const that is not itself inside a body, or a direct member of one.
fn is_derived_impl_item(node: &SyntaxNode) -> bool {
    let implementation = if ast::Impl::can_cast(node.kind()) {
        Some(node.clone())
    } else {
        node.parent()
            .filter(|parent| ast::AssocItemList::can_cast(parent.kind()))
            .and_then(|list| list.parent())
            .filter(|parent| ast::Impl::can_cast(parent.kind()))
    };
    implementation
        .and_then(|implementation| implementation.parent())
        .filter(|parent| ast::StmtList::can_cast(parent.kind()))
        .and_then(|list| list.parent())
        .filter(|parent| ast::BlockExpr::can_cast(parent.kind()))
        .and_then(|block| block.parent())
        .and_then(ast::Const::cast)
        .is_some_and(|konst| {
            konst.underscore_token().is_some()
                && !konst
                    .syntax()
                    .ancestors()
                    .skip(1)
                    .any(|it| ast::StmtList::can_cast(it.kind()))
        })
}

/// A block def-map (the items inside a function body) has no stable nested identity yet
/// (s05-reviews/astra-block-items.md §2), so nothing of its items is published, except the
/// impls of anonymous consts at module level (R-3, `anonymous_const_owner`).
fn is_block_module(db: &RootDatabase, module: Module) -> bool {
    module.nearest_non_block_module(db) != module
}

/// The anonymous const (`const _: () = { .. };`) whose body is the block module `module`, with
/// the module declaring the const. Only a const declared directly in a module (the shape
/// derives expand to): the body of a function, and a const inside one, is not it (R-4).
fn anonymous_const_owner(
    db: &RootDatabase,
    module: Module,
) -> Option<(InFile<ast::Const>, Module)> {
    if !is_block_module(db, module) {
        return None;
    }
    let parent = module.parent(db)?;
    if is_block_module(db, parent) {
        return None;
    }
    let source = module.definition_source(db);
    let ModuleSource::BlockExpr(block) = &source.value else {
        return None;
    };
    let konst = ast::Const::cast(block.syntax().parent()?)?;
    konst
        .underscore_token()
        .is_some()
        .then(|| (InFile::new(source.file_id, konst), parent))
}

/// The lexical scope name of the anonymous const `target` in `module`, a numbering of the
/// SYNTAX in source order that every cfg context shares (astra §2; r3-1): the module's items
/// are the slots, cfg-disabled ones included (a disabled item publishes nothing but keeps its
/// slot, so an identity never depends on the context that formed it). `$const<i>` is the
/// literal anonymous const at item `i`; `$const<i>-<m>-<d>-<k>` the `k`-th anonymous const
/// (in preorder through nested expansions) of the `d`-th derive in the `m`-th meta of item
/// `i`'s attributes, `cfg_attr` looked through (r3-2), the metas and the derive list numbered
/// as written: a sibling derive the context pruned leaves the slots of the others where they
/// are (fixg-2); `$const<i>-<k>` the `k`-th one in the expansion of an attribute macro on item
/// `i` or of an item-position macro call. `None` when the walk does not reach the const.
fn anonymous_const_scope(
    sema: &Semantics<'_, RootDatabase>,
    module: Module,
    target: &InFile<ast::Const>,
) -> Option<String> {
    let source = module.definition_source(sema.db);
    // the walk expands through `sema`, which only answers for nodes of trees it parsed: the
    // module's item list is taken from its own parse of the file, not from the HIR source
    let root = sema.parse_or_expand(source.file_id);
    let items: Vec<ast::Item> = match &source.value {
        ModuleSource::SourceFile(_) => ast::SourceFile::cast(root)?.items().collect(),
        ModuleSource::Module(module) => {
            let range = module.syntax().text_range();
            let node = root.covering_element(range).into_node()?;
            ast::Module::cast(node)?.item_list()?.items().collect()
        }
        ModuleSource::BlockExpr(_) => return None,
    };
    let nested = |node: &SyntaxNode| node.children().filter_map(ast::Item::cast).collect();
    for (index, item) in items.into_iter().enumerate() {
        if let ast::Item::Const(konst) = &item
            && konst.underscore_token().is_some()
            && is_target(source.file_id, konst, target)
        {
            return Some(format!("$const{index}"));
        }
        // a const under an attribute macro is not the target as written: the const rust-analyzer
        // owns the block module of is the one in the expansion (fixg-3), reached below
        let mut ordinal = 0;
        if let Some(expansion) = sema.expand_attr_macro(&item) {
            // an attribute macro replaces the item: its derives are in the expansion
            let InFile { file_id, value } = expansion.value;
            if expansion_consts(sema, file_id, nested(&value), target, &mut ordinal, 1) {
                return Some(format!("$const{index}-{ordinal}"));
            }
            continue;
        }
        let metas = item.attrs().flat_map(|attr| attr.skip_cfg_attrs());
        for (meta_slot, meta) in metas.enumerate() {
            let Some(expansions) = sema.expand_derive_macro(&meta) else {
                continue;
            };
            // one entry per derive of the list, `None` where the context has no expansion
            for (derive, expansion) in expansions.into_iter().enumerate() {
                let Some(expansion) = expansion else {
                    continue;
                };
                let mut ordinal = 0;
                let file_id = sema.hir_file_for(&expansion.value);
                let items = nested(&expansion.value);
                if expansion_consts(sema, file_id, items, target, &mut ordinal, 1) {
                    return Some(format!("$const{index}-{meta_slot}-{derive}-{ordinal}"));
                }
            }
        }
        if let ast::Item::MacroCall(call) = &item
            && let Some(expansion) = sema.expand_macro_call(call)
            && expansion_consts(
                sema,
                expansion.file_id,
                nested(&expansion.value),
                target,
                &mut ordinal,
                1,
            )
        {
            return Some(format!("$const{index}-{ordinal}"));
        }
    }
    None
}

fn is_target(file: HirFileId, konst: &ast::Const, target: &InFile<ast::Const>) -> bool {
    file == target.file_id && konst.syntax().text_range() == target.value.syntax().text_range()
}

/// Preorder over the items of one expansion for `anonymous_const_scope`: true when `target` is
/// reached, its expansion-local ordinal left in `ordinal`; the anonymous consts of nested
/// expansions (an attribute macro, a derive under `cfg_attr` too, an item-position macro call)
/// are numbered in the same sequence. An expansion chain deeper than 64 is not inspected
/// further (`block_items`).
fn expansion_consts(
    sema: &Semantics<'_, RootDatabase>,
    file: HirFileId,
    items: Vec<ast::Item>,
    target: &InFile<ast::Const>,
    ordinal: &mut usize,
    depth: usize,
) -> bool {
    if depth >= 64 {
        return false;
    }
    let nested = |node: &SyntaxNode| node.children().filter_map(ast::Item::cast).collect();
    for item in items {
        if let ast::Item::Const(konst) = &item
            && konst.underscore_token().is_some()
        {
            if is_target(file, konst, target) {
                return true;
            }
            *ordinal += 1;
        }
        if let Some(expansion) = sema.expand_attr_macro(&item) {
            let InFile { file_id, value } = expansion.value;
            if expansion_consts(sema, file_id, nested(&value), target, ordinal, depth + 1) {
                return true;
            }
            continue;
        }
        for meta in item.attrs().flat_map(|attr| attr.skip_cfg_attrs()) {
            let Some(expansions) = sema.expand_derive_macro(&meta) else {
                continue;
            };
            for expansion in expansions.into_iter().flatten() {
                let file_id = sema.hir_file_for(&expansion.value);
                let items = nested(&expansion.value);
                if expansion_consts(sema, file_id, items, target, ordinal, depth + 1) {
                    return true;
                }
            }
        }
        if let ast::Item::MacroCall(call) = &item
            && let Some(expansion) = sema.expand_macro_call(call)
        {
            let items = nested(&expansion.value);
            if expansion_consts(sema, expansion.file_id, items, target, ordinal, depth + 1) {
                return true;
            }
        }
    }
    false
}

/// The scope descriptor of a definition that is an impl inside an anonymous const at module
/// level, or a member of one (R-3): `$const<slot>:` with the const's lexical scope name in its
/// module (`anonymous_const_scope`). `None` for everything else, and for an impl whose const
/// the lexical walk does not reach (nothing of it is published then).
fn derived_impl_scope(db: &RootDatabase, definition: Definition<'_>) -> Option<Descriptor> {
    use ra_ap_hir::{AsAssocItem, AssocItemContainer};
    let in_impl = |item: Option<ra_ap_hir::AssocItem>| match item?.container(db) {
        AssocItemContainer::Impl(implementation) => Some(implementation),
        AssocItemContainer::Trait(_) => None,
    };
    let implementation = match definition {
        Definition::SelfType(it) => it,
        Definition::Function(it) => in_impl(it.as_assoc_item(db))?,
        Definition::Const(it) => in_impl(it.as_assoc_item(db))?,
        Definition::TypeAlias(it) => in_impl(it.as_assoc_item(db))?,
        _ => return None,
    };
    let (konst, module) = anonymous_const_owner(db, implementation.module(db))?;
    let name = anonymous_const_scope(&Semantics::new(db), module, &konst)?;
    Some(Descriptor {
        name,
        suffix: Suffix::Meta.into(),
        ..Descriptor::default()
    })
}

/// Whether `token` lies inside a block-local item: its nearest enclosing item node, resolved in
/// its own scope, belongs to a block module. An item a macro generates inside a body resolves
/// where the expansion is, so it counts too; an impl inside an anonymous const at module level
/// and its members do not (R-3: published). `cache` is per document (items are shared by all
/// the tokens inside them).
fn inside_block_item(
    sema: &Semantics<'_, RootDatabase>,
    token: &ra_ap_syntax::SyntaxToken,
    cache: &mut std::collections::HashMap<SyntaxNode, bool>,
) -> bool {
    let Some(item) = token
        .parent_ancestors()
        .find(|node| ast::Item::can_cast(node.kind()) && !ast::MacroCall::can_cast(node.kind()))
    else {
        return false;
    };
    if let Some(&known) = cache.get(&item) {
        return known;
    }
    // the scope at a child of the item (its name, visibility or first attribute) is the
    // item's own scope; the scope at the item node is the surrounding one, which for an item
    // statement is the outer block, not the def-map the item lives in
    let probe = item.first_child().unwrap_or_else(|| item.clone());
    let block = sema
        .scope(&probe)
        .is_some_and(|scope| is_block_module(sema.db, scope.module()))
        && !is_derived_impl_member(sema, &item);
    cache.insert(item, block);
    block
}

/// Whether the item node is an impl inside an anonymous const at module level, or a direct
/// member of one, resolved through rust-analyzer (`anonymous_const_owner`).
fn is_derived_impl_member(sema: &Semantics<'_, RootDatabase>, item: &SyntaxNode) -> bool {
    let implementation = if let Some(implementation) = ast::Impl::cast(item.clone()) {
        Some(implementation)
    } else {
        item.parent()
            .filter(|parent| ast::AssocItemList::can_cast(parent.kind()))
            .and_then(|list| list.parent())
            .and_then(ast::Impl::cast)
    };
    implementation
        .and_then(|implementation| sema.to_impl_def(&implementation))
        .is_some_and(|implementation| {
            anonymous_const_owner(sema.db, implementation.module(sema.db)).is_some()
        })
}

fn is_operator_token(token: &ra_ap_syntax::SyntaxToken) -> bool {
    use ra_ap_syntax::ast::{self, RangeItem};
    let Some(parent) = token.parent() else {
        return false;
    };
    let operator = if let Some(expr) = ast::BinExpr::cast(parent.clone()) {
        expr.op_token()
    } else if let Some(expr) = ast::PrefixExpr::cast(parent.clone()) {
        expr.op_token()
    } else if let Some(expr) = ast::RangeExpr::cast(parent.clone()) {
        expr.op_token()
    } else if let Some(pattern) = ast::RangePat::cast(parent.clone()) {
        pattern.op_token()
    } else if let Some(expr) = ast::AwaitExpr::cast(parent.clone()) {
        expr.await_token()
    } else if let Some(expr) = ast::TryExpr::cast(parent.clone()) {
        expr.question_mark_token()
    } else if let Some(expr) = ast::IndexExpr::cast(parent) {
        expr.l_brack_token()
    } else {
        None
    };
    operator.as_ref() == Some(token)
}

fn project_root(checkout: &Path) -> Result<String> {
    let path = checkout
        .to_str()
        .context("SCIP project root is not UTF-8")?;
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}

pub(super) fn implementation_endpoints(
    db: &RootDatabase,
    implementation: ra_ap_hir::Impl,
) -> Option<(Adt, ra_ap_hir::Trait)> {
    // an impl whose only parameters are lifetimes (`impl<'de> Deserialize<'de> for X`) carries
    // no predicate and holds for every lifetime: unconditional (consumer request 3). A bound on
    // such a lifetime (`impl<'a: 'static>`, inline or in the where-clause) is a predicate
    // (fixg-1); a bound the trait declares on its lifetime is one the impl restates (rustc
    // E0478), so the impl's own syntax is the whole evidence
    if implementation.is_negative(db)
        || !GenericDef::Impl(implementation)
            .type_or_const_params(db)
            .is_empty()
        || !implementation
            .source(db)
            .is_some_and(|source| is_unconditional_header(&source.value))
    {
        return None;
    }
    let adt = implementation.self_ty(db).as_adt()?;
    if !GenericDef::Adt(adt).params(db).is_empty() {
        return None;
    }
    Some((adt, implementation.trait_(db)?))
}

/// Whether an impl header states no predicate: no where-clause and no bound on a lifetime
/// parameter (`impl<'a: 'b>`). Type and const parameters are the caller's check.
fn is_unconditional_header(implementation: &ast::Impl) -> bool {
    implementation.where_clause().is_none()
        && implementation.generic_param_list().is_none_or(|params| {
            params
                .lifetime_params()
                .all(|lifetime| lifetime.type_bound_list().is_none())
        })
}

/// The variant name of a definition, for the `tokens_without_symbol` breakdown.
fn definition_name(definition: Definition<'_>) -> &'static str {
    match definition {
        Definition::Macro(_) => "Macro",
        Definition::Field(_) => "Field",
        Definition::TupleField(_) => "TupleField",
        Definition::Module(_) => "Module",
        Definition::Crate(_) => "Crate",
        Definition::Function(_) => "Function",
        Definition::Adt(_) => "Adt",
        Definition::EnumVariant(_) => "EnumVariant",
        Definition::Const(_) => "Const",
        Definition::Static(_) => "Static",
        Definition::Trait(_) => "Trait",
        Definition::TypeAlias(_) => "TypeAlias",
        Definition::SelfType(_) => "SelfType",
        Definition::GenericParam(_) => "GenericParam",
        Definition::Local(_) => "Local",
        Definition::Label(_) => "Label",
        Definition::ExternCrateDecl(_) => "ExternCrateDecl",
        Definition::InlineAsmRegOrRegClass(()) => "InlineAsmRegOrRegClass",
        Definition::InlineAsmOperand(_) => "InlineAsmOperand",
        Definition::DeriveHelper(_)
        | Definition::BuiltinType(_)
        | Definition::BuiltinLifetime(_)
        | Definition::BuiltinAttr(_)
        | Definition::ToolModule(_) => "builtin",
    }
}

/// The published documentation of a definition: the first paragraph of its own doc comment
/// as rust-analyzer assembles it (`///`, `#[doc]`, `//!` for a module), or `None` without one.
fn documentation(
    db: &RootDatabase,
    definition: Definition<'_>,
    target: ra_ap_hir::DisplayTarget,
) -> Option<String> {
    use ra_ap_hir::HasAttrs as _;
    let own = match definition {
        // rust-analyzer's hover falls back to the aliased type's docs (under a "*This is the
        // documentation for*" header) and to the self type's docs for an impl: not the
        // declaration's own
        Definition::TypeAlias(alias) => alias.hir_docs(db).map(|docs| docs.docs().to_owned()),
        Definition::SelfType(implementation) => implementation
            .hir_docs(db)
            .map(|docs| docs.docs().to_owned()),
        _ => definition
            .docs(db, None, target)
            .map(|docs| docs.as_str().to_owned()),
    };
    first_paragraph(&own?)
}

/// The first paragraph of `docs` (up to the first blank line), at most
/// `MAX_DOCUMENTATION_BYTES`, cut back to a character boundary; `None` when nothing is left.
fn first_paragraph(docs: &str) -> Option<String> {
    let paragraph = docs
        .trim()
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut end = paragraph.len().min(MAX_DOCUMENTATION_BYTES);
    while !paragraph.is_char_boundary(end) {
        end -= 1;
    }
    let bounded = paragraph[..end].trim_end();
    (!bounded.is_empty()).then(|| bounded.to_owned())
}

/// The SCIP range (UTF-8 byte columns) of a byte range of `source`; `None` when an end is not
/// a character boundary of `source` or the range is reversed. Total: the caller counts a `None`.
fn source_range(source: &str, (start, end): (u32, u32)) -> Option<Vec<i32>> {
    let point = |offset: u32| -> Option<(i32, i32)> {
        let offset = offset as usize;
        if offset > source.len() || !source.is_char_boundary(offset) {
            return None;
        }
        let before = &source[..offset];
        let line = before.bytes().filter(|byte| *byte == b'\n').count();
        let column = before.rfind('\n').map_or(offset, |last| offset - last - 1);
        Some((line.try_into().ok()?, column.try_into().ok()?))
    };
    if start > end {
        return None;
    }
    let (sl, sc) = point(start)?;
    let (el, ec) = point(end)?;
    Some(if sl == el {
        vec![sl, sc, ec]
    } else {
        vec![sl, sc, el, ec]
    })
}

fn descriptors(moniker: ra_ap_ide::Moniker) -> impl Iterator<Item = Descriptor> {
    moniker
        .identifier
        .description
        .into_iter()
        .map(|descriptor| Descriptor {
            name: descriptor.name,
            suffix: match descriptor.desc {
                MonikerDescriptorKind::Namespace => Suffix::Namespace,
                MonikerDescriptorKind::Type => Suffix::Type,
                MonikerDescriptorKind::Term => Suffix::Term,
                MonikerDescriptorKind::Method => Suffix::Method,
                MonikerDescriptorKind::TypeParameter => Suffix::TypeParameter,
                MonikerDescriptorKind::Parameter => Suffix::Parameter,
                MonikerDescriptorKind::Macro => Suffix::Macro,
                MonikerDescriptorKind::Meta => Suffix::Meta,
            }
            .into(),
            ..Descriptor::default()
        })
}

/// The item descriptors of a definition's symbol, the one place an item identity is formed:
/// rust-analyzer's moniker, except that a member of an inherent impl covering its whole type is
/// a child of that type (R-2): `Postgres#new().`, not `impl#[Postgres]new().`, so the consumer's
/// parent rule (the symbol minus its last descriptor) yields the `CONTAINS` edge; and that an
/// impl inside an anonymous const at module level, with its members, carries the const's
/// lexical scope in front of the impl (R-3, r3-1): rust-analyzer names such an impl under the
/// module as if the const were not there (`impl#[Serialize][X]`), the scope keeps two
/// same-shaped impls in two consts distinct and the member a child of its impl,
/// `$const3-0-0-0:impl#[Serialize][X]#serialize().`. The owner's descriptors come through this same
/// function, so a later identity rule (a nested identity for block-local items,
/// s05-reviews/df23.md) applies to the type and to its members alike. One rule for declarations
/// and references.
fn symbol_descriptors(
    db: &RootDatabase,
    context: &Context,
    definition: Definition<'_>,
    moniker: ra_ap_ide::Moniker,
) -> Vec<Descriptor> {
    if let Some(scope) = derived_impl_scope(db, definition) {
        let mut all: Vec<_> = descriptors(moniker).collect();
        let position = all
            .iter()
            .position(|descriptor| {
                descriptor.name == "impl"
                    && descriptor.suffix.enum_value_or_default() == Suffix::Type
            })
            .unwrap_or(all.len());
        all.insert(position, scope);
        return all;
    }
    let owner =
        inherent_owner(db, context, definition).and_then(|adt| {
            match MonikerResult::from_def(db, Definition::Adt(adt), context.krate) {
                Some(MonikerResult::Moniker(owner)) => Some((adt, owner)),
                Some(MonikerResult::Local { .. }) | None => None,
            }
        });
    match owner {
        Some((adt, owner)) => {
            let member = descriptors(moniker).last();
            let mut owned = symbol_descriptors(db, context, Definition::Adt(adt), owner);
            owned.extend(member);
            owned
        }
        None => descriptors(moniker).collect(),
    }
}

/// The ADT whose inherent impl declares `definition`, when that impl covers the whole type: its
/// self type is the ADT applied to distinct bare parameters (`impl<T> Wrapper<T>`), never a
/// specialisation (`impl Wrapper<u32>`), so no two inherent impls can declare one member name
/// (rustc E0592). Methods (`S#m().`) and associated types (`S#T#`) only: an associated const and
/// a field both map to a `Term` descriptor (`S#x.`), so consts keep rust-analyzer's `impl#`
/// identity (r2-1), as do members of a specialised inherent impl and of trait impls.
fn inherent_owner(db: &RootDatabase, context: &Context, definition: Definition<'_>) -> Option<Adt> {
    use ra_ap_hir::{AsAssocItem, AssocItemContainer};
    let item = match definition {
        Definition::Function(it) => it.as_assoc_item(db),
        Definition::TypeAlias(it) => it.as_assoc_item(db),
        _ => None,
    }?;
    let AssocItemContainer::Impl(implementation) = item.container(db) else {
        return None;
    };
    if implementation.trait_(db).is_some() {
        return None;
    }
    let self_ty = implementation.self_ty(db);
    let adt = self_ty.as_adt()?;
    let bare = self_ty
        .type_arguments()
        .all(|argument| argument.as_type_param(db).is_some());
    let arguments = self_ty
        .type_and_const_arguments(db, context.krate.to_display_target(db))
        .count();
    (bare && self_ty.generic_params(db).len() == arguments).then_some(adt)
}

fn semantic_symbol(
    db: &RootDatabase,
    vfs: &Vfs,
    context: &Context,
    catalog: &super::targets::Catalog,
    definition: Definition<'_>,
) -> Result<String> {
    if let Definition::ExternCrateDecl(extern_) = definition {
        return semantic_symbol(
            db,
            vfs,
            context,
            catalog,
            Definition::Module(
                extern_
                    .resolved_crate(db)
                    .context("unresolved extern crate declaration")?
                    .root_module(db),
            ),
        );
    }
    if let Definition::Crate(krate) = definition {
        return semantic_symbol(
            db,
            vfs,
            context,
            catalog,
            Definition::Module(krate.root_module(db)),
        );
    }
    match MonikerResult::from_def(db, definition, context.krate) {
        Some(MonikerResult::Moniker(moniker)) => {
            let descriptors = symbol_descriptors(db, context, definition, moniker.clone());
            if let Some(identity) = definition
                .krate(db)
                .and_then(|krate| catalog.identity(db, vfs, krate))
            {
                Ok(identity.symbol(descriptors))
            } else {
                let package = scip::types::Package {
                    manager: "cargo".into(),
                    name: moniker.package_information.name.clone(),
                    version: moniker
                        .package_information
                        .version
                        .clone()
                        .unwrap_or_else(|| "rust-1.99.0".into()),
                    ..Default::default()
                };
                Ok(scip::symbol::format_symbol(scip::types::Symbol {
                    scheme: "rust-analyzer".into(),
                    package: Some(package).into(),
                    descriptors,
                    ..Default::default()
                }))
            }
        }
        Some(MonikerResult::Local { .. }) => {
            let sema = Semantics::new(db);
            let nav = definition
                .try_to_nav(&sema)
                .map(UpmappingResult::call_site)
                .context("unsupported unlocated local semantic symbol")?;
            // Document scoped, target-aware and independent of traversal/DB IDs.
            let identity = context.identity.symbol([]);
            let namespace: String = identity
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let relative = catalog.relative_source(vfs, nav.file_id)?;
            let document: String = relative
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            Ok(format!(
                "local {namespace}-{document}-{}",
                u32::from(nav.focus_or_full_range().start())
            ))
        }
        None => anyhow::bail!(
            "unsupported semantic token without a stable symbol: {}",
            definition.label(db, context.krate.to_display_target(db))
        ),
    }
}

fn definition_kind(
    definition: Definition<'_>,
    db: &RootDatabase,
) -> scip::types::symbol_information::Kind {
    use ra_ap_hir::AsAssocItem;
    use scip::types::symbol_information::Kind as K;
    match definition {
        Definition::Function(function) => {
            match function.as_assoc_item(db).map(|item| item.container(db)) {
                Some(ra_ap_hir::AssocItemContainer::Trait(_)) => K::TraitMethod,
                Some(ra_ap_hir::AssocItemContainer::Impl(_)) => K::Method,
                None => K::Function,
            }
        }
        Definition::Adt(Adt::Struct(_)) => K::Struct,
        Definition::Adt(Adt::Enum(_)) => K::Enum,
        Definition::Adt(Adt::Union(_)) => K::Union,
        Definition::Trait(_) => K::Trait,
        Definition::Module(_) | Definition::Crate(_) => K::Module,
        Definition::EnumVariant(_) => K::EnumMember,
        Definition::Field(_) | Definition::TupleField(_) => K::Field,
        Definition::Macro(_) => K::Macro,
        Definition::Const(_) => K::Constant,
        Definition::Static(_) => K::StaticVariable,
        Definition::TypeAlias(_) => K::TypeAlias,
        Definition::Local(_) => K::Variable,
        Definition::GenericParam(_) => K::TypeParameter,
        _ => K::UnspecifiedKind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> scip::types::Index {
        use protobuf::Message as _;
        scip::types::Index::parse_from_bytes(bytes).expect("SCIP")
    }

    #[test]
    fn a_position_off_the_analysed_text_is_skipped_and_counted_not_refused() {
        let mut inventory = Inventory::default();
        inventory
            .sources
            .insert("src/lib.rs".into(), "fn é() {}\n".into());
        // `é` occupies bytes 3..5: offset 4 is inside it
        for (start, end) in [(0, 2), (4, 5)] {
            inventory.references.insert(
                ("src/lib.rs".into(), start, end, "sym".into()),
                BTreeSet::from([super::super::targets::Mode::Production]),
            );
        }
        let bytes = inventory
            .encode(Path::new("/checkout"))
            .expect("a bad position is not a refusal");
        let index = decode(&bytes);
        let occurrences: Vec<_> = index
            .documents
            .iter()
            .flat_map(|document| &document.occurrences)
            .map(|occurrence| occurrence.range.clone())
            .collect();
        assert_eq!(occurrences, vec![vec![0, 0, 2]]);
        assert_eq!(inventory.skipped_positions(), 1);
    }

    #[test]
    fn a_document_whose_text_was_not_retained_is_named_in_the_error() {
        let mut inventory = Inventory::default();
        inventory.references.insert(
            ("src/missing.rs".into(), 0, 2, "sym".into()),
            BTreeSet::from([super::super::targets::Mode::Production]),
        );
        let error = inventory
            .encode(Path::new("/checkout"))
            .expect_err("no text to convert positions against");
        assert!(
            format!("{error:#}").contains("src/missing.rs"),
            "error must name the document: {error:#}"
        );
    }

    #[test]
    fn scip_positions_use_utf8_bytes_and_reject_mid_codepoint_offsets() {
        assert_eq!(
            source_range("é\ncafé", (3, 8)).expect("range"),
            vec![1, 0, 5]
        );
        assert!(source_range("é", (1, 2)).is_none());
        assert!(source_range("x", (1, 0)).is_none());
        assert!(source_range("x", (0, 2)).is_none());
        assert_eq!(
            project_root(Path::new("/checkout/é space")).expect("URI"),
            "file:///checkout/%C3%A9%20space"
        );
    }

    fn location(start: u32) -> Location {
        Location {
            path: "src/lib.rs".into(),
            focus: (start, start + 4),
            body: (start, start + 10),
            signature: "fn common()".into(),
        }
    }

    #[test]
    fn ordinary_declaration_merges_only_when_source_anchors_are_equal() {
        let mut inventory = Inventory::default();
        inventory
            .merge("same-symbol".into(), location(0), None, vec!["unix".into()])
            .expect("production");
        inventory
            .merge(
                "same-symbol".into(),
                location(0),
                None,
                vec!["test".into(), "unix".into()],
            )
            .expect("test");
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory.declarations["same-symbol"].cfgs.len(), 2);
    }

    /// r2-2: two cfg alternatives (or cfg-exclusive inherent impls) give one identity two
    /// declaration locations; the first is kept and the divergence is counted, never refused.
    #[test]
    fn cfg_alternative_with_the_same_symbol_and_different_source_keeps_the_first_and_counts() {
        let mut inventory = Inventory::default();
        inventory
            .merge("same-symbol".into(), location(0), None, vec![])
            .expect("production");
        inventory
            .merge(
                "same-symbol".into(),
                location(20),
                None,
                vec!["test".into()],
            )
            .expect("a cfg-divergent declaration is not a refusal");
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory.declarations["same-symbol"].location, location(0));
        assert_eq!(inventory.divergent_declarations(), 1);
        assert!(
            inventory.notes()[0].starts_with("semantic cfg-divergent declaration: same-symbol"),
            "{:?}",
            inventory.notes()
        );
    }

    /// backlog 7: the coverage note names what the tokens without a symbol resolved to.
    #[test]
    fn tokens_without_symbol_are_broken_down_by_definition_kind_in_one_note() {
        let mut inventory = Inventory::default();
        assert_eq!(inventory.skipped_token_note(), None);
        inventory.skip_token("TupleField");
        inventory.skip_token("TupleField");
        inventory.skip_token("Local (one call-site offset)");
        assert_eq!(inventory.skipped_tokens(), 3);
        assert_eq!(
            inventory.skipped_token_note().as_deref(),
            Some(
                "semantic tokens without symbol by definition: Local (one call-site offset)=1, TupleField=2"
            )
        );
    }

    /// backlog 14: the published documentation is the first paragraph, bounded on a character
    /// boundary; a declaration without one publishes nothing.
    #[test]
    fn documentation_is_the_first_paragraph_bounded_on_a_character_boundary() {
        assert_eq!(
            first_paragraph("Opens the store.\nSecond line.\n\nDetails follow.").as_deref(),
            Some("Opens the store.\nSecond line.")
        );
        assert_eq!(first_paragraph(" \n\t\n"), None);
        // the cut falls inside the last `é` (two bytes at an odd offset): one byte less
        let long = format!("a{}", "é".repeat(MAX_DOCUMENTATION_BYTES));
        let bounded = first_paragraph(&long).expect("non-empty");
        assert_eq!(bounded.len(), MAX_DOCUMENTATION_BYTES - 1);
        assert!(bounded[1..].chars().all(|c| c == 'é'));
    }

    /// The first context's doc is kept; a context without one does not clear it, and a doc
    /// difference between contexts is no divergence.
    #[test]
    fn documentation_of_the_first_context_is_kept_and_never_counted_divergent() {
        let mut inventory = Inventory::default();
        inventory
            .merge("same-symbol".into(), location(0), None, vec![])
            .expect("production");
        inventory
            .merge(
                "same-symbol".into(),
                location(0),
                Some("Test doc.".into()),
                vec!["test".into()],
            )
            .expect("test");
        inventory
            .merge("same-symbol".into(), location(0), None, vec!["unix".into()])
            .expect("unix");
        assert_eq!(
            inventory.declarations["same-symbol"]
                .documentation
                .as_deref(),
            Some("Test doc.")
        );
        assert_eq!(inventory.divergent_declarations(), 0);
    }

    #[test]
    fn same_anchor_with_a_changed_semantic_signature_is_divergent_too() {
        let mut inventory = Inventory::default();
        inventory
            .merge("same-symbol".into(), location(0), None, vec![])
            .expect("production");
        let mut changed = location(0);
        changed.signature = "fn common() -> u32".into();
        inventory
            .merge("same-symbol".into(), changed, None, vec!["test".into()])
            .expect("counted, not refused");
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory.divergent_declarations(), 1);
    }

    /// R-3: the block-item count leaves out exactly what the discovery publishes: an impl in
    /// the body of an anonymous const at module level and its direct members; not the helper
    /// beside it, not an anonymous const inside a function body, not a named const's body.
    #[test]
    fn derived_impl_items_are_the_impls_of_module_level_anonymous_consts_and_their_members() {
        let source = "const _: () = { fn helper() {} impl T for X { fn m() {} } };\n\
                      fn f() { const _: () = { impl T for Y { fn m() {} } }; }\n\
                      const N: () = { impl T for Z {} };\n";
        let parsed = ra_ap_syntax::SourceFile::parse(source, ra_ap_syntax::Edition::Edition2024);
        let published = |kind: fn(ra_ap_syntax::SyntaxKind) -> bool| -> Vec<bool> {
            parsed
                .syntax_node()
                .descendants()
                .filter(|node| kind(node.kind()))
                .map(|node| is_derived_impl_item(&node))
                .collect()
        };
        assert_eq!(published(ast::Impl::can_cast), vec![true, false, false]);
        // helper, X::m, f, Y::m
        assert_eq!(
            published(ast::Fn::can_cast),
            vec![false, true, false, false]
        );
    }
}
