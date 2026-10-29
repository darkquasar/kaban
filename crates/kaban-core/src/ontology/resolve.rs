//! Name resolution, inheritance and conflicts (engine-spec §3.1 "Qualified and stored names",
//! EN-F1; engine-plan E1.2). [`resolve`] turns a set of [`OntologyFile`]s into one [`Ontology`]:
//! `extends` visibility, the bare-name lookup order (own namespace, `extends` in order, then
//! `core`, `adr`, `user`), single `is_a` inheritance, mixins in list order after `is_a`, the
//! inherited `props_schema` as a meta-validated [`Schema`], and the conflict rules.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

use super::OntologyError;
use super::meta::{
    Acceptance, ColumnWeights, DiscoveredPolicy, Extract, Governed, GroupBy, Index, KindBody, Ns,
    OnLink, OntologyFile, QuestionKindsDecl, QuestionTemplate, RankHints, RelBody, Stamp,
    check_ranges,
};
use crate::validator::{Schema, ValidatorLimits};

/// The namespaces whose kinds and rels keep their bare stored name (engine-spec §3.1: `core`,
/// `adr` and `user`; every other namespace stores `ns:Name`).
const BARE_NAMESPACES: [&str; 3] = ["core", "adr", "user"];

/// A kind or rel identity: `(ns, name)`; qualified form `ns:Name`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qualified {
    /// The declaring namespace.
    pub ns: Ns,
    /// The local name.
    pub name: String,
}

impl fmt::Display for Qualified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ns, self.name)
    }
}

/// The stored name of `q` (`node.kind`, `edge.rel`, every API field and golden): bare for
/// `core`, `adr` and `user`, else `ns:Name`.
#[must_use]
pub fn stored_name(q: &Qualified) -> String {
    if BARE_NAMESPACES.contains(&q.ns.as_str()) {
        q.name.clone()
    } else {
        format!("{}:{}", q.ns, q.name)
    }
}

/// The inverse of [`stored_name`]: `incidents:Decision` is `(Some(incidents), "Decision")`, a
/// bare name is `(None, name)`. A prefix that is not a namespace is part of the name.
#[must_use]
pub fn split_stored(stored: &str) -> (Option<Ns>, &str) {
    match stored.split_once(':') {
        Some((prefix, name)) => match Ns::parse(prefix) {
            Ok(ns) => (Some(ns), name),
            Err(_) => (None, stored),
        },
        None => (None, stored),
    }
}

/// The canonical stored spelling of a kind or rel name written by a caller (E1.7a RE2-1;
/// E1.9a RE3-2, the one normalisation contract the API and the store share): a qualified
/// spelling of a bare namespace (`user:Widget`, `adr:Decision`, `core:Fact`) is its bare
/// name, every other name is itself. The API applies it to the gated identifier before every
/// registry lookup (`props_schema`, `dedup`, the tier) and the store at its `schema_type` and
/// placement seams, so one record is judged and stored under one name.
#[must_use]
pub fn normalise_stored(stored: &str) -> String {
    match split_stored(stored) {
        (Some(ns), bare) => stored_name(&Qualified {
            ns,
            name: bare.to_owned(),
        }),
        (None, _) => stored.to_owned(),
    }
}

/// Namespaces known to be loaded besides the files being resolved (`extends` may name them;
/// a qualified reference into one is accepted unverified, the store checks the kind on write).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamespaceSet(BTreeSet<Ns>);

impl NamespaceSet {
    /// The empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a namespace.
    pub fn insert(&mut self, ns: Ns) {
        self.0.insert(ns);
    }

    /// Is `ns` in the set?
    #[must_use]
    pub fn contains(&self, ns: &Ns) -> bool {
        self.0.contains(ns)
    }
}

impl FromIterator<Ns> for NamespaceSet {
    fn from_iter<I: IntoIterator<Item = Ns>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// One end of a relationship: a kind or the `path` literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelEnd {
    /// A resolved kind.
    Kind(Qualified),
    /// The literal `path` (a `GOVERNS` target).
    Path,
}

/// A kind after inheritance and mixins.
#[derive(Debug, Clone)]
pub struct ResolvedKind {
    /// Identity.
    pub id: Qualified,
    /// Human description (required on a kind).
    pub description: String,
    /// Resolved parent.
    pub is_a: Option<Qualified>,
    /// Resolved mixins in application order.
    pub mixins: Vec<Qualified>,
    /// The merged, meta-validated `props_schema`.
    pub props_schema: Schema,
    /// Governance (per kind, never inherited: OD-E-2).
    pub governed: Option<Governed>,
    /// The acceptance tier of an ungoverned kind's candidates and `proposed` records
    /// (OWN-84 (1); per kind like `governed`, a mixin's counts once; refused beside
    /// `governed`).
    pub acceptance: Option<Acceptance>,
    /// Rank hints (inherited, overridden by the kind's own).
    pub rank: Option<RankHints>,
    /// FTS prop paths (inherited plus the kind's own).
    pub fts_fields: Vec<String>,
    /// Identifier prop paths (inherited plus the kind's own).
    pub ident_fields: Vec<String>,
    /// Column weights (inherited, overridden).
    pub column_weights: Option<ColumnWeights>,
    /// Extraction hints (inherited, overridden).
    pub extract: Option<Extract>,
    /// Winner gate (inherited, overridden).
    pub min_confidence: Option<f64>,
    /// Indexing meta-config (inherited, overridden; engine-spec §3.1 `index`, OWN-65).
    pub index: Option<Index>,
}

impl ResolvedKind {
    /// The effective stamping policy: the kind's `index.stamp`, `commit` when no block is set
    /// anywhere along `is_a` and the mixins (engine-spec §3.1 "Default when the block is
    /// absent: `commit`").
    pub fn stamp(&self) -> Stamp {
        self.index.map_or(Stamp::Commit, |index| index.stamp)
    }
}

/// A relationship type after resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRel {
    /// Identity.
    pub id: Qualified,
    /// Allowed sources (empty = any).
    pub from: Vec<RelEnd>,
    /// Allowed targets (empty = any).
    pub to: Vec<RelEnd>,
    /// Directed edge.
    pub directed: bool,
    /// Human description.
    pub description: Option<String>,
    /// Lifecycle side effect.
    pub on_link: Option<OnLink>,
    /// Minimum confidence for an auto edge.
    pub confidence_required: Option<f64>,
    /// Needs a human (EN-S10).
    pub governed_semantics: bool,
}

/// One resolved `source_types` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSourceType {
    /// Kinds an extractor may fill.
    pub kinds: Vec<Qualified>,
    /// Selecting globs.
    pub paths: Vec<String>,
}

/// The `kinds` of a question template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionKinds {
    /// Every kind with `governs_paths: true`.
    GovernsPaths,
    /// An explicit list.
    Kinds(Vec<Qualified>),
}

/// One resolved question template (every field a validated shape: the file form types
/// `group_by` and `where`, [`check_ranges`] bounds `depth`, the names are resolved here).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedQuestion {
    /// Target kinds.
    pub kinds: QuestionKinds,
    /// Predicate block (a mapping of plain data).
    pub where_: serde_json::Map<String, Value>,
    /// Excluding rel.
    pub exclude_via: Option<Qualified>,
    /// Grouping.
    pub group_by: Option<GroupBy>,
    /// Window in days.
    pub window: Option<u32>,
    /// Ordering keys.
    pub order: Vec<String>,
    /// Depth.
    pub depth: Option<u8>,
}

/// The resolved ontology of a set of files.
#[derive(Debug, Clone, Default)]
pub struct Ontology {
    /// Namespaces in first-seen file order.
    pub namespaces: Vec<Ns>,
    /// Kinds by identity.
    pub kinds: BTreeMap<Qualified, ResolvedKind>,
    /// Rels by identity.
    pub rels: BTreeMap<Qualified, ResolvedRel>,
    /// Mixin bodies by identity (as declared).
    pub mixins: BTreeMap<Qualified, KindBody>,
    /// `discovered` block per namespace.
    pub discovered: BTreeMap<Ns, DiscoveredPolicy>,
    /// `source_types` per namespace.
    pub source_types: BTreeMap<Ns, BTreeMap<String, ResolvedSourceType>>,
    /// `default_kind` per namespace.
    pub default_kind: BTreeMap<Ns, Qualified>,
    /// Priors merged across files (one value per key).
    pub priors: BTreeMap<String, f64>,
    /// Questions by `(ns, name)`.
    pub questions: BTreeMap<Qualified, ResolvedQuestion>,
}

impl Ontology {
    /// The `(ns, kind, name)` rows the store will hold: `kind` is `label` or `rel`, `name` the
    /// local name (the stored name is [`stored_name`]).
    #[must_use]
    pub fn type_rows(&self) -> BTreeSet<(Ns, &'static str, String)> {
        let kinds = self
            .kinds
            .keys()
            .map(|q| (q.ns.clone(), "label", q.name.clone()));
        let rels = self
            .rels
            .keys()
            .map(|q| (q.ns.clone(), "rel", q.name.clone()));
        kinds.chain(rels).collect()
    }
}

fn conflict(name: &Qualified, detail: &str) -> OntologyError {
    OntologyError::Conflict {
        name: name.clone(),
        detail: detail.to_owned(),
    }
}

fn file_error(path: &str) -> OntologyError {
    OntologyError::SchemaInvalid {
        rule: format!("file:{path}"),
    }
}

fn qualified(ns: &Ns, name: &str) -> Qualified {
    Qualified {
        ns: ns.clone(),
        name: name.to_owned(),
    }
}

/// The declarations gathered from every file before resolution.
struct Declarations<'a> {
    namespaces: Vec<Ns>,
    extends: BTreeMap<Ns, Vec<Ns>>,
    kinds: BTreeMap<Qualified, &'a KindBody>,
    mixins: BTreeMap<Qualified, &'a KindBody>,
    rels: BTreeMap<Qualified, &'a RelBody>,
    loaded: &'a NamespaceSet,
    limits: ValidatorLimits,
}

impl Declarations<'_> {
    fn knows(&self, ns: &Ns) -> bool {
        self.namespaces.contains(ns) || self.loaded.contains(ns)
    }

    /// Own namespace, `extends` in order, then `core`, `adr`, `user` (each once).
    fn order(&self, from: &Ns) -> Vec<Ns> {
        let mut order = vec![from.clone()];
        for ns in self.extends.get(from).into_iter().flatten() {
            if !order.contains(ns) {
                order.push(ns.clone());
            }
        }
        for bare in BARE_NAMESPACES {
            if let Ok(ns) = Ns::parse(bare)
                && !order.contains(&ns)
            {
                order.push(ns);
            }
        }
        order
    }

    /// Resolve a kind or mixin reference written in namespace `from` at the field `at` (the
    /// dotted path every error of the reference carries; never the name).
    fn lookup<T>(
        &self,
        from: &Ns,
        name: &str,
        at: &str,
        table: &BTreeMap<Qualified, T>,
    ) -> Result<Qualified, OntologyError> {
        if let Some((prefix, local)) = name.split_once(':') {
            let ns = Ns::parse(prefix).map_err(|e| e.at(at))?;
            if !self.knows(&ns) {
                return Err(OntologyError::UnknownNamespace {
                    path: at.to_owned(),
                });
            }
            let q = qualified(&ns, local);
            // a namespace known only as loaded is accepted unverified
            if self.namespaces.contains(&ns) && !table.contains_key(&q) {
                return Err(OntologyError::UnknownKind {
                    path: at.to_owned(),
                });
            }
            return Ok(q);
        }
        let candidates: Vec<Qualified> = self
            .order(from)
            .into_iter()
            .map(|ns| qualified(&ns, name))
            .filter(|q| table.contains_key(q))
            .collect();
        match candidates.len() {
            0 => Err(OntologyError::UnknownKind {
                path: at.to_owned(),
            }),
            1 => Ok(candidates
                .into_iter()
                .next()
                .unwrap_or_else(|| qualified(from, name))),
            _ => Err(OntologyError::AmbiguousKind {
                path: at.to_owned(),
            }),
        }
    }

    fn lookup_rel(&self, from: &Ns, name: &str, at: &str) -> Result<Qualified, OntologyError> {
        self.lookup(from, name, at, &self.rels)
            .map_err(|e| match e {
                OntologyError::UnknownKind { path } => OntologyError::UnknownRel { path },
                other => other,
            })
    }
}

/// The `props_schema` pieces merged along `is_a` and mixins.
#[derive(Default, Clone)]
struct MergedSchema {
    touched: bool,
    properties: BTreeMap<String, Value>,
    required: Vec<String>,
    additional: Option<bool>,
}

impl MergedSchema {
    /// Fold `schema` in: the contributing schema must be an object schema; a property already
    /// present with a different body is a retype; `required` is a union.
    fn fold(
        &mut self,
        schema: &Value,
        owner: &Qualified,
        retype_detail: &str,
    ) -> Result<(), OntologyError> {
        if schema.get("type") != Some(&Value::String("object".to_owned())) {
            return Err(conflict(owner, "props_schema_not_object"));
        }
        self.touched = true;
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (name, body) in props {
                match self.properties.get(name) {
                    Some(existing) if existing != body => {
                        return Err(conflict(owner, &format!("{retype_detail}:{name}")));
                    }
                    _ => {
                        self.properties.insert(name.clone(), body.clone());
                    }
                }
            }
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !self.required.iter().any(|r| r == name) {
                    self.required.push(name.to_owned());
                }
            }
        }
        if let Some(additional) = schema.get("additionalProperties").and_then(Value::as_bool) {
            self.additional = Some(additional);
        }
        Ok(())
    }

    fn into_value(self) -> Value {
        if !self.touched {
            return serde_json::json!({"type": "object", "additionalProperties": false});
        }
        let mut map = serde_json::Map::new();
        map.insert("type".to_owned(), Value::String("object".to_owned()));
        if let Some(additional) = self.additional {
            map.insert("additionalProperties".to_owned(), Value::Bool(additional));
        }
        if !self.required.is_empty() {
            map.insert(
                "required".to_owned(),
                Value::Array(self.required.into_iter().map(Value::String).collect()),
            );
        }
        if !self.properties.is_empty() {
            map.insert(
                "properties".to_owned(),
                Value::Object(self.properties.into_iter().collect()),
            );
        }
        Value::Object(map)
    }
}

/// Resolve `files` (every file whose kinds are available) with `loaded` naming the namespaces
/// that exist elsewhere, under the default [`ValidatorLimits`].
///
/// # Errors
/// Every [`OntologyError`] except `BuiltinImmutable` and `DiscoveredCapReached` (later seams).
pub fn resolve(files: &[OntologyFile], loaded: &NamespaceSet) -> Result<Ontology, OntologyError> {
    resolve_with(files, loaded, &ValidatorLimits::default())
}

/// [`resolve`] under explicit validator limits (`[ontology] pattern_size_limit_kib`).
///
/// # Errors
/// As [`resolve`].
pub fn resolve_with(
    files: &[OntologyFile],
    loaded: &NamespaceSet,
    limits: &ValidatorLimits,
) -> Result<Ontology, OntologyError> {
    let decl = gather(files, loaded, *limits)?;
    for ns in decl.extends.values().flatten() {
        if !decl.knows(ns) {
            return Err(OntologyError::UnknownNamespace {
                path: "extends".to_owned(),
            });
        }
    }
    let mut ontology = Ontology {
        namespaces: decl.namespaces.clone(),
        ..Ontology::default()
    };
    // mixins: never `is_a` or `mixins`; a `props_schema` must be in the subset
    for (q, body) in &decl.mixins {
        if body.is_a.is_some() || !body.mixins.is_empty() {
            return Err(conflict(q, "mixin_is_a"));
        }
        if let Some(schema) = &body.props_schema {
            Schema::parse(schema, limits)?;
        }
        ontology.mixins.insert(q.clone(), (*body).clone());
    }
    // kinds, parents first
    let mut visiting: BTreeSet<Qualified> = BTreeSet::new();
    let ids: Vec<Qualified> = decl.kinds.keys().cloned().collect();
    for q in &ids {
        resolve_kind(q, &decl, &mut ontology, &mut visiting)?;
    }
    // rels
    for (q, body) in &decl.rels {
        let path = |key: &str| format!("rels.{}.{key}", q.name);
        let from = body
            .from
            .as_ref()
            .ok_or_else(|| file_error(&path("from")))?;
        let to = body.to.as_ref().ok_or_else(|| file_error(&path("to")))?;
        let directed = body.directed.ok_or_else(|| file_error(&path("directed")))?;
        let ends = |names: &[String], key: &str| -> Result<Vec<RelEnd>, OntologyError> {
            let allow_path = key == "to";
            names
                .iter()
                .map(|name| {
                    if allow_path && name == "path" {
                        Ok(RelEnd::Path)
                    } else {
                        decl.lookup(&q.ns, name, &path(key), &decl.kinds)
                            .map(RelEnd::Kind)
                    }
                })
                .collect()
        };
        ontology.rels.insert(
            q.clone(),
            ResolvedRel {
                id: q.clone(),
                from: ends(from, "from")?,
                to: ends(to, "to")?,
                directed,
                description: body.description.clone(),
                on_link: body.on_link,
                confidence_required: body.confidence_required,
                governed_semantics: body.governed_semantics.unwrap_or(false),
            },
        );
    }
    // the per-namespace blocks
    for file in files {
        let ns = &file.ontology;
        if let Some(policy) = &file.discovered {
            match ontology.discovered.get(ns) {
                Some(existing) if existing != policy => {
                    return Err(conflict(&qualified(ns, "discovered"), "duplicate"));
                }
                _ => {
                    ontology.discovered.insert(ns.clone(), policy.clone());
                }
            }
        }
        for (name, source) in &file.source_types {
            let kinds = source
                .kinds
                .as_ref()
                .ok_or_else(|| file_error(&format!("source_types.{name}.kinds")))?;
            let resolved = ResolvedSourceType {
                kinds: kinds
                    .iter()
                    .map(|k| decl.lookup(ns, k, &format!("source_types.{name}.kinds"), &decl.kinds))
                    .collect::<Result<_, _>>()?,
                paths: source.paths.clone(),
            };
            let per_ns = ontology.source_types.entry(ns.clone()).or_default();
            match per_ns.get(name) {
                Some(existing) if *existing != resolved => {
                    return Err(conflict(&qualified(ns, name), "duplicate_source_type"));
                }
                _ => {
                    per_ns.insert(name.clone(), resolved);
                }
            }
        }
        if let Some(default_kind) = &file.default_kind {
            let q = decl.lookup(ns, default_kind, "default_kind", &decl.kinds)?;
            match ontology.default_kind.get(ns) {
                Some(existing) if *existing != q => {
                    return Err(conflict(&qualified(ns, "default_kind"), "duplicate"));
                }
                _ => {
                    ontology.default_kind.insert(ns.clone(), q);
                }
            }
        }
        for (key, value) in &file.priors {
            match ontology.priors.get(key) {
                Some(existing) if existing.to_bits() != value.to_bits() => {
                    return Err(conflict(&qualified(ns, key), "duplicate_prior"));
                }
                _ => {
                    ontology.priors.insert(key.clone(), *value);
                }
            }
        }
        for (name, template) in &file.questions {
            let resolved = resolve_question(ns, name, template, &decl)?;
            let q = qualified(ns, name);
            match ontology.questions.get(&q) {
                Some(existing) if *existing != resolved => {
                    return Err(conflict(&q, "duplicate_question"));
                }
                _ => {
                    ontology.questions.insert(q, resolved);
                }
            }
        }
    }
    Ok(ontology)
}

/// Collect every declaration; two files declaring one `(ns, name)` with identical bodies are
/// one declaration, with different bodies an `ontology_conflict`.
fn gather<'a>(
    files: &'a [OntologyFile],
    loaded: &'a NamespaceSet,
    limits: ValidatorLimits,
) -> Result<Declarations<'a>, OntologyError> {
    fn declare<'a, T: PartialEq>(
        table: &mut BTreeMap<Qualified, &'a T>,
        q: Qualified,
        body: &'a T,
    ) -> Result<(), OntologyError> {
        match table.get(&q) {
            Some(existing) if **existing != *body => Err(conflict(&q, "duplicate")),
            _ => {
                table.insert(q, body);
                Ok(())
            }
        }
    }
    let mut decl = Declarations {
        namespaces: Vec::new(),
        extends: BTreeMap::new(),
        kinds: BTreeMap::new(),
        mixins: BTreeMap::new(),
        rels: BTreeMap::new(),
        loaded,
        limits,
    };
    for file in files {
        check_ranges(file)?;
        let ns = &file.ontology;
        if !decl.namespaces.contains(ns) {
            decl.namespaces.push(ns.clone());
        }
        let extends = decl.extends.entry(ns.clone()).or_default();
        for e in &file.extends {
            if !extends.contains(e) {
                extends.push(e.clone());
            }
        }
        for (name, body) in &file.kinds {
            declare(&mut decl.kinds, qualified(ns, name.as_str()), body)?;
        }
        for (name, body) in &file.mixins {
            declare(&mut decl.mixins, qualified(ns, name.as_str()), body)?;
        }
        for (name, body) in &file.rels {
            declare(&mut decl.rels, qualified(ns, name.as_str()), body)?;
        }
    }
    Ok(decl)
}

/// Resolve one kind (parents first, memoised in `ontology.kinds`).
fn resolve_kind(
    q: &Qualified,
    decl: &Declarations<'_>,
    ontology: &mut Ontology,
    visiting: &mut BTreeSet<Qualified>,
) -> Result<(), OntologyError> {
    if ontology.kinds.contains_key(q) {
        return Ok(());
    }
    if !visiting.insert(q.clone()) {
        return Err(conflict(q, "is_a_cycle"));
    }
    let path = |key: &str| format!("kinds.{}.{key}", q.name);
    let body = decl
        .kinds
        .get(q)
        .ok_or_else(|| OntologyError::UnknownKind {
            path: format!("kinds.{}", q.name),
        })?;
    let description = body
        .description
        .clone()
        .ok_or_else(|| file_error(&path("description")))?;
    let mut merged = MergedSchema::default();
    let mut resolved = ResolvedKind {
        id: q.clone(),
        description,
        is_a: None,
        mixins: Vec::new(),
        props_schema: Schema::parse(
            &serde_json::json!({"type": "object"}),
            &ValidatorLimits::default(),
        )?,
        governed: None,
        acceptance: None,
        rank: None,
        fts_fields: Vec::new(),
        ident_fields: Vec::new(),
        column_weights: None,
        extract: None,
        min_confidence: None,
        index: None,
    };
    // is_a: the parent's merged schema and hints
    if let Some(parent_name) = &body.is_a {
        let parent_id = decl.lookup(&q.ns, parent_name, &path("is_a"), &decl.kinds)?;
        if parent_id == *q {
            return Err(conflict(q, "is_a_cycle"));
        }
        resolve_kind(&parent_id, decl, ontology, visiting)?;
        let parent = ontology
            .kinds
            .get(&parent_id)
            .ok_or_else(|| OntologyError::UnknownKind { path: path("is_a") })?;
        merged.fold(parent.props_schema.as_value(), q, "retype")?;
        resolved.is_a = Some(parent_id.clone());
        resolved.rank.clone_from(&parent.rank);
        resolved.fts_fields.clone_from(&parent.fts_fields);
        resolved.ident_fields.clone_from(&parent.ident_fields);
        resolved.column_weights.clone_from(&parent.column_weights);
        resolved.extract.clone_from(&parent.extract);
        resolved.min_confidence = parent.min_confidence;
        resolved.index = parent.index;
    }
    // mixins in list order; a key declared by two mixins is a conflict, whatever the values
    // (an explicit empty list and `description` are declarations too: E1.3a RE1-5)
    let mut set_by: BTreeSet<&'static str> = BTreeSet::new();
    for mixin_name in &body.mixins {
        let mixin_id = decl.lookup(&q.ns, mixin_name, &path("mixins"), &decl.mixins)?;
        if resolved.mixins.contains(&mixin_id) {
            continue; // the same mixin twice applies once
        }
        let mixin = decl
            .mixins
            .get(&mixin_id)
            .ok_or_else(|| OntologyError::UnknownKind {
                path: path("mixins"),
            })?;
        let mut claim = |key: &'static str| -> Result<(), OntologyError> {
            if !set_by.insert(key) {
                return Err(conflict(q, &format!("mixin_key:{key}")));
            }
            Ok(())
        };
        if mixin.description.is_some() {
            // tracked for the conflict rule only: a kind carries its own description
            claim("description")?;
        }
        if let Some(schema) = &mixin.props_schema {
            claim("props_schema")?;
            merged.fold(schema, q, "mixin_retype")?;
        }
        if let Some(governed) = &mixin.governed {
            claim("governed")?;
            resolved.governed = Some(governed.clone());
        }
        if let Some(acceptance) = mixin.acceptance {
            claim("acceptance")?;
            resolved.acceptance = Some(acceptance);
        }
        if let Some(rank) = &mixin.rank {
            claim("rank")?;
            resolved.rank = Some(rank.clone());
        }
        if let Some(fields) = &mixin.fts_fields {
            claim("fts_fields")?;
            extend_unique(&mut resolved.fts_fields, fields);
        }
        if let Some(fields) = &mixin.ident_fields {
            claim("ident_fields")?;
            extend_unique(&mut resolved.ident_fields, fields);
        }
        if let Some(weights) = &mixin.column_weights {
            claim("column_weights")?;
            resolved.column_weights = Some(weights.clone());
        }
        if let Some(extract) = &mixin.extract {
            claim("extract")?;
            resolved.extract = Some(extract.clone());
        }
        if let Some(min) = mixin.min_confidence {
            claim("min_confidence")?;
            resolved.min_confidence = Some(min);
        }
        if let Some(index) = mixin.index {
            claim("index")?;
            resolved.index = Some(index);
        }
        resolved.mixins.push(mixin_id);
    }
    // the kind's own keys win over mixins and inheritance; its properties may add, never retype
    if let Some(schema) = &body.props_schema {
        Schema::parse(schema, &decl.limits)?;
        merged.fold(schema, q, "retype")?;
    }
    if let Some(governed) = &body.governed {
        resolved.governed = Some(governed.clone());
    }
    if let Some(acceptance) = body.acceptance {
        resolved.acceptance = Some(acceptance);
    }
    // OWN-84 (1): a governed kind's tier is its mixin's `governed.acceptance`; a second
    // top-level `acceptance` beside it (the kind's own or a mixin's) is a conflict, like a
    // key two mixins declare
    if resolved.governed.is_some() && resolved.acceptance.is_some() {
        return Err(conflict(q, "acceptance_governed"));
    }
    if let Some(rank) = &body.rank {
        resolved.rank = Some(rank.clone());
    }
    extend_unique(
        &mut resolved.fts_fields,
        body.fts_fields.as_deref().unwrap_or_default(),
    );
    extend_unique(
        &mut resolved.ident_fields,
        body.ident_fields.as_deref().unwrap_or_default(),
    );
    if let Some(weights) = &body.column_weights {
        resolved.column_weights = Some(weights.clone());
    }
    if let Some(extract) = &body.extract {
        resolved.extract = Some(extract.clone());
    }
    if let Some(min) = body.min_confidence {
        resolved.min_confidence = Some(min);
    }
    if let Some(index) = body.index {
        resolved.index = Some(index);
    }
    resolved.props_schema = Schema::parse(&merged.into_value(), &decl.limits)?;
    visiting.remove(q);
    ontology.kinds.insert(q.clone(), resolved);
    Ok(())
}

fn extend_unique(target: &mut Vec<String>, more: &[String]) {
    for item in more {
        if !target.contains(item) {
            target.push(item.clone());
        }
    }
}

fn resolve_question(
    ns: &Ns,
    name: &str,
    template: &QuestionTemplate,
    decl: &Declarations<'_>,
) -> Result<ResolvedQuestion, OntologyError> {
    let path = |key: &str| format!("questions.{name}.{key}");
    let kinds = match template.kinds.as_ref() {
        Some(QuestionKindsDecl::GovernsPaths(_)) => QuestionKinds::GovernsPaths,
        Some(QuestionKindsDecl::Kinds(items)) => QuestionKinds::Kinds(
            items
                .iter()
                .map(|k| decl.lookup(ns, k, &path("kinds"), &decl.kinds))
                .collect::<Result<_, _>>()?,
        ),
        None => return Err(file_error(&path("kinds"))),
    };
    let where_ = template
        .where_
        .clone()
        .ok_or_else(|| file_error(&path("where")))?;
    let order = template
        .order
        .clone()
        .ok_or_else(|| file_error(&path("order")))?;
    let exclude_via = template
        .exclude_via
        .as_deref()
        .map(|rel| decl.lookup_rel(ns, rel, &path("exclude_via")))
        .transpose()?;
    Ok(ResolvedQuestion {
        kinds,
        where_,
        exclude_via,
        group_by: template.group_by,
        window: template.window,
        order,
        depth: template.depth,
    })
}
