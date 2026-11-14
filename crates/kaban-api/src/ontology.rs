//! The ontology registry (engine-spec §3.1, EN-F2, EN-S7, EN-S8; engine-plan E1.3): the shipped
//! `kaban-core.yaml` and `adr.yaml` plus the user files loaded at startup from
//! `.kaban/ontology/*.yaml` (sorted path order) or by `kaban ontology load <file>`, resolved
//! into one [`Ontology`] the service reads mixin, rank and extraction data from (OWN-38: the
//! files are the source of truth; nothing persists that data before E1.7).
//!
//! [`OntologyRegistry::load_file`] is the seam: the S7 containment walk
//! ([`kaban_policy::read_contained`]) and the byte budget, the EN-S8 content gate over the raw
//! bytes before the hash or the YAML parser sees them and again over every string of the
//! parsed data (a reject-class hit fails the load with rule ids only and nothing is stored;
//! E1.3a RE1-1), the E1.2 meta-validation and resolution against
//! the already loaded namespaces, `ontology_builtin_immutable` for the shipped namespaces, then
//! the non-builtin `schema_type` rows upserted under their stored names through the
//! `SchemaStore` role trait and `index_meta.ontology_hash:<ns>` / `ontology_version:<ns>`
//! recorded. A reload whose blake3 hash equals the recorded one writes nothing (N5).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use kaban_core::ontology::meta::{OnLink, Rules};
use kaban_core::ontology::resolve::{RelEnd, ResolvedKind, resolve_with};
use kaban_core::{
    NamespaceSet, Ns, Ontology, OntologyError, OntologyFile, Qualified, RuleSet, TypeDef,
    ValidatorLimits, stored_name,
};
use kaban_core::{first_error_text, validate};
use kaban_policy::ContainError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::KabanApi;
use crate::error::ApiError;

/// Largest ontology file the loader reads (the M5 R5-3 per-file import rule, the same bound
/// as `sync.rs` `MAX_FILE_BYTES`): the size is taken from the opened descriptor before a byte
/// is read, and a larger file is refused with [`FILE_TOO_LARGE`].
pub const MAX_ONTOLOGY_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// The directory of user ontology files below the repository root.
pub const ONTOLOGY_DIR: &str = ".kaban/ontology";

/// Fixed text of an oversized file (exit 4).
pub const FILE_TOO_LARGE: &str = "ontology file exceeds the import byte budget; nothing was loaded";
/// Fixed text of a containment failure (exit 4).
pub const FILE_ESCAPES: &str = "ontology file is a symlink, lies outside the repository, or changed during the open; nothing was loaded";
/// Fixed text when `.kaban` or `.kaban/ontology` is a symlink at startup (exit 4; the same
/// phrase `sync` uses for its `.kaban` containment, so the legacy CLI texts hold).
pub const DIR_ESCAPES: &str = "the .kaban directory or the ontology directory is a symlink or leaves the repository; nothing was loaded";
/// Fixed text of a redaction-class hit (exit 4): ontology text cannot be masked.
pub const FILE_REDACTED: &str =
    "denylist hit in ontology file; ontology text cannot be redacted, remove the value";

/// The tool name the content gate audits ontology loads under.
const TOOL: &str = "ontology_load";

/// What one load did (the `kaban ontology load` document).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LoadReport {
    /// The file's namespace.
    pub ns: String,
    /// The file's `version`.
    pub version: u32,
    /// blake3 of the file bytes, hex.
    pub hash: String,
    /// Kinds the file declares.
    pub kinds: usize,
    /// Rels the file declares.
    pub rels: usize,
    /// Whether anything was written (`false` when the recorded hash already matched).
    pub changed: bool,
}

/// One loaded ontology as `kaban ontology list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OntologySummary {
    /// Namespace.
    pub ns: String,
    /// `version`.
    pub version: u32,
    /// blake3 of the bytes, hex.
    pub hash: String,
    /// Kinds declared.
    pub kinds: usize,
    /// Rels declared.
    pub rels: usize,
    /// Shipped in the binary (`core`, `adr`) or loaded from a file.
    pub builtin: bool,
}

/// A kind as the service sees it: its stored name, identity and the resolved declaration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct KindView {
    /// The stored name (`node.kind`).
    pub stored: String,
    /// `(ns, name)`.
    pub id: Qualified,
    /// Description.
    pub description: String,
    /// The kind's `rules` block, when it declares one.
    pub rules: Option<Rules>,
    /// The merged `props_schema`.
    pub props_schema: Value,
}

/// One file the registry holds.
#[derive(Debug, Clone)]
struct Loaded {
    /// Shipped files have no path.
    path: Option<PathBuf>,
    ns: Ns,
    version: u32,
    hash: String,
    file: OntologyFile,
}

impl Loaded {
    fn summary(&self) -> OntologySummary {
        OntologySummary {
            ns: self.ns.to_string(),
            version: self.version,
            hash: self.hash.clone(),
            kinds: self.file.kinds.len(),
            rels: self.file.rels.len(),
            builtin: self.path.is_none(),
        }
    }
}

#[derive(Debug, Clone)]
struct State {
    files: Vec<Loaded>,
    ontology: Ontology,
    /// The rule set of `ontology`, derived once per load (every write reads it).
    rules: Arc<RuleSet>,
}

impl State {
    fn new(files: Vec<Loaded>, ontology: Ontology) -> Self {
        let rules = Arc::new(RuleSet::from_ontology(&ontology));
        Self {
            files,
            ontology,
            rules,
        }
    }
}

/// The registry: the shipped ontologies plus every loaded user file, resolved as one. The
/// service holds it ([`KabanApi::registry`], engine-plan E1.5) and reads every ontology
/// decision through it: the rule set of its writes, `remember`'s dedup rule and
/// `props_schema`.
/// Loads are serialised (one at a time, read-resolve-write-publish as one section); reads
/// never wait on a load except for the final swap of the published state.
#[derive(Debug, Clone)]
pub struct OntologyRegistry {
    /// The containment root of `load_file` and the startup directory (`None`: the current
    /// directory).
    root: Option<PathBuf>,
    /// The published state: many readers, swapped whole by a load.
    inner: Arc<RwLock<State>>,
    /// One load at a time (E1.3a RE1-7): the read of the current files, the resolution, the
    /// store writes and the publication are one exclusive section, so two concurrent loads
    /// cannot both start from one state and overwrite each other. Readers never take it.
    load_lock: Arc<Mutex<()>>,
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn invalid(e: &OntologyError) -> ApiError {
    ApiError::Invalid(e.to_string())
}

fn contain_error(e: ContainError) -> ApiError {
    match e {
        ContainError::TooLarge => ApiError::Invalid(FILE_TOO_LARGE.to_owned()),
        ContainError::Escapes | ContainError::NotRegularFile => {
            ApiError::Invalid(FILE_ESCAPES.to_owned())
        }
        other => ApiError::Invalid(format!("ontology file could not be read: {other}")),
    }
}

/// The shipped files parsed and resolved once (the state every registry starts from; the
/// builtin membership `schema_describe` reports).
fn shipped_state() -> Result<&'static State, ApiError> {
    static SHIPPED: std::sync::OnceLock<Result<State, OntologyError>> = std::sync::OnceLock::new();
    SHIPPED
        .get_or_init(|| {
            let mut files = Vec::with_capacity(kaban_core::shipped::FILES.len());
            for (_, text) in kaban_core::shipped::FILES {
                let file = OntologyFile::from_yaml(text)?;
                files.push(Loaded {
                    path: None,
                    ns: file.ontology.clone(),
                    version: file.version,
                    hash: blake3_hex(text.as_bytes()),
                    file,
                });
            }
            let parsed: Vec<OntologyFile> = files.iter().map(|f| f.file.clone()).collect();
            let ontology =
                resolve_with(&parsed, &NamespaceSet::new(), &ValidatorLimits::default())?;
            Ok(State::new(files, ontology))
        })
        .as_ref()
        .map_err(invalid)
}

/// The shipped ontology, resolved once.
fn shipped() -> Result<&'static Ontology, ApiError> {
    shipped_state().map(|state| &state.ontology)
}

/// The namespace of a `schema_type` row before migration 0004 persists it: a builtin row's
/// namespace comes from the shipped membership lists, a `ns:Name` stored name carries its own,
/// and a bare user row is `user`.
pub(crate) fn namespace_of(kind: &str, stored: &str, builtin: bool) -> Option<String> {
    if builtin {
        let ontology = shipped().ok()?;
        let table = if kind == "rel" {
            ontology.rels.keys().collect::<Vec<_>>()
        } else {
            ontology.kinds.keys().collect::<Vec<_>>()
        };
        return table
            .into_iter()
            .find(|q| q.name == stored)
            .map(|q| q.ns.to_string());
    }
    let (ns, _) = kaban_core::split_stored(stored);
    Some(ns.map_or_else(|| "user".to_owned(), |ns| ns.to_string()))
}

impl OntologyRegistry {
    /// Project the currently loaded ontology under one synchronous read-lock callback.
    /// The callback must not await, perform I/O, or reenter registry loading/mutation.
    /// It confers neither filesystem capability nor acceptance authority.
    pub fn with_ontology<R>(&self, f: impl FnOnce(&Ontology) -> R) -> R {
        f(&self.read().ontology)
    }
    /// The registry over the shipped files only, with `root` as the containment root of every
    /// later load (`None`: the current directory).
    ///
    /// # Errors
    /// Never for the shipped bytes (`Invalid` with the EN-F7 text otherwise).
    pub fn new(root: Option<PathBuf>) -> Result<Self, ApiError> {
        Ok(Self {
            root,
            inner: Arc::new(RwLock::new(shipped_state()?.clone())),
            load_lock: Arc::new(Mutex::new(())),
        })
    }

    /// The registry a service starts with when none is handed over ([`KabanApi::with_config`]):
    /// the shipped files, the current directory as the containment root.
    #[allow(clippy::expect_used)] // the embedded files are a build invariant pinned by the kaban-core tests (E1.4 D8)
    pub(crate) fn shipped_default() -> Self {
        Self::new(None).expect("the shipped ontology files resolve")
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The containment root: the configured one or the current directory.
    fn root(&self) -> Result<PathBuf, ApiError> {
        match &self.root {
            Some(root) => Ok(root.clone()),
            None => std::env::current_dir()
                .map_err(|_| ApiError::Invalid("the current directory is unavailable".into())),
        }
    }

    /// Load one user ontology file (see the module doc).
    ///
    /// # Errors
    /// `Rejected` on a reject-class denylist hit (exit 2; rule ids only); `Invalid` with the
    /// fixed containment, budget or redaction text or the EN-F7 error text (exit 4); store
    /// errors (exit 5).
    pub fn load_file(&self, api: &KabanApi, path: &Path) -> Result<LoadReport, ApiError> {
        let mut reports = self.load_files(api, std::slice::from_ref(&path.to_path_buf()))?;
        reports
            .pop()
            .ok_or_else(|| ApiError::Invalid("no file was loaded".into()))
    }

    /// The startup load: every regular `*.yaml` directly under `<root>/.kaban/ontology` in
    /// sorted path order, resolved together (two files declaring one `(ns, name)` differently
    /// fail `ontology_conflict`). An absent directory loads nothing; `.kaban` or `ontology`
    /// being a symlink is a containment failure; a symlink entry is handed to the walk and
    /// fails closed as it always has; a FIFO, socket, device or directory entry is skipped with
    /// a traced class (RE1-2: nothing may hold the startup open).
    ///
    /// # Errors
    /// As [`Self::load_file`].
    pub fn load_startup(&self, api: &KabanApi) -> Result<Vec<LoadReport>, ApiError> {
        let root = self.root()?;
        let dir = root.join(ONTOLOGY_DIR);
        for probe in [root.join(".kaban"), dir.clone()] {
            match std::fs::symlink_metadata(&probe) {
                Ok(m) if m.is_symlink() => return Err(ApiError::Invalid(DIR_ESCAPES.to_owned())),
                Ok(m) if !m.is_dir() => return Ok(Vec::new()),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => {
                    return Err(ApiError::Invalid(format!(
                        "ontology directory could not be read: {:?}",
                        e.kind()
                    )));
                }
            }
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| {
                ApiError::Invalid(format!(
                    "ontology directory could not be read: {:?}",
                    e.kind()
                ))
            })?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "yaml"))
            .filter(|entry| match entry.file_type() {
                Ok(kind) if kind.is_file() || kind.is_symlink() => true,
                Ok(_) => {
                    tracing::warn!(
                        class = "not_regular_file",
                        "an ontology directory entry is not a regular file; skipped"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(class = ?e.kind(), "an ontology directory entry could not be typed; skipped");
                    false
                }
            })
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        self.load_files(api, &paths)
    }

    /// Load `paths` as one set: read and gate each, resolve them together with what is loaded,
    /// write the changed namespaces, then publish the new state; the whole of it under the
    /// load lock (a poisoned lock is taken over: the state behind it is only ever replaced
    /// whole).
    fn load_files(&self, api: &KabanApi, paths: &[PathBuf]) -> Result<Vec<LoadReport>, ApiError> {
        let _loading = self
            .load_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = self.root()?;
        let limits = ValidatorLimits::from(&api.engine().ontology);
        let mut incoming = Vec::with_capacity(paths.len());
        for path in paths {
            incoming.push(read_and_gate(api, &root, path)?);
        }
        // the new set: shipped files, loaded files not replaced by path, the incoming files
        let current = self.read().files.clone();
        let mut files: Vec<Loaded> = current
            .into_iter()
            .filter(|f| f.path.as_ref().is_none_or(|p| !paths.contains(p)))
            .collect();
        for file in &incoming {
            check_builtin_immutable(file)?;
        }
        files.extend(incoming.iter().cloned());
        let ontology = resolve_all(&files, limits)?;
        // writes, per namespace, only when the recorded hash differs (N5)
        let meta = api.store.index_meta()?;
        let mut reports = Vec::with_capacity(incoming.len());
        for file in &incoming {
            let key = format!("ontology_hash:{}", file.ns);
            let changed = meta.get(&key) != Some(file.hash.as_str());
            if changed {
                write_namespace(api, &ontology, file)?;
            }
            reports.push(LoadReport {
                ns: file.ns.to_string(),
                version: file.version,
                hash: file.hash.clone(),
                kinds: file.file.kinds.len(),
                rels: file.file.rels.len(),
                changed,
            });
        }
        // the service and its store decide by the loaded set from here on (EN-F4, E1.4; the
        // service reads this registry, E1.5)
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = State::new(files, ontology);
        api.store.adopt_rules(self.rule_set());
        Ok(reports)
    }

    /// Every loaded ontology, shipped first then in load order.
    pub fn list(&self) -> Vec<OntologySummary> {
        self.read().files.iter().map(Loaded::summary).collect()
    }

    /// Read the resolved kind stored under `stored` under the registry lock, if the loaded
    /// files declare it (the request-path lookups: no clone of the schema per write).
    pub(crate) fn with_kind<R>(
        &self,
        stored: &str,
        f: impl FnOnce(&ResolvedKind) -> R,
    ) -> Option<R> {
        let state = self.read();
        state
            .ontology
            .kinds
            .values()
            .find(|k| stored_name(&k.id) == stored)
            .map(f)
    }

    /// The kind stored under `stored`, if the loaded files declare it.
    pub fn kind(&self, stored: &str) -> Option<KindView> {
        self.with_kind(stored, |k| KindView {
            stored: stored.to_owned(),
            id: k.id.clone(),
            description: k.description.clone(),
            rules: k.rules.clone(),
            props_schema: k.props_schema.as_value().clone(),
        })
    }

    /// The `rules` block of the kind stored under `stored` (a clone: the registry is shared
    /// behind a lock).
    pub fn rules(&self, stored: &str) -> Option<Rules> {
        self.kind(stored).and_then(|k| k.rules)
    }

    /// The kind rules and `on_link` rels of everything loaded, as the store decides writes by
    /// them (engine-plan E1.4; derived once per load, shared by every write).
    pub fn rule_set(&self) -> Arc<RuleSet> {
        Arc::clone(&self.read().rules)
    }

    /// Every rel with an `on_link` side effect, by stored name.
    pub fn rels_with_on_link(&self) -> Vec<(String, OnLink)> {
        let state = self.read();
        state
            .ontology
            .rels
            .values()
            .filter_map(|r| r.on_link.map(|on| (stored_name(&r.id), on)))
            .collect()
    }
}

/// Validate a write's `props` against the `props_schema` of `kind` in the registry (engine-spec
/// EN-F3; the seam `remember` and a batch write pass through): a kind the loaded files do not
/// declare keeps free-form props, as a free-form shipped kind does by its open schema.
///
/// # Errors
/// `Invalid("props.<dotted pointer>")` naming the first failing instance pointer (`props`
/// for the root; the texts the request path has always emitted), never a value.
pub fn validate_kind_props(
    registry: &OntologyRegistry,
    kind: &str,
    props: &Value,
) -> Result<(), ApiError> {
    let first = registry
        .with_kind(kind, |k| {
            first_error_text(&validate(&k.props_schema, props))
        })
        .flatten();
    match first {
        // the validator's text is `invalid_props props.<pointer>`; the request path reports
        // the pointer alone (core-spec F1 `props.<field>`, EN-F4 byte-identical)
        Some(text) => Err(ApiError::Invalid(
            text.strip_prefix("invalid_props ")
                .unwrap_or(&text)
                .to_owned(),
        )),
        None => Ok(()),
    }
}

/// Resolve every loaded file as one ontology (no namespace exists outside the files).
fn resolve_all(files: &[Loaded], limits: ValidatorLimits) -> Result<Ontology, ApiError> {
    let parsed: Vec<OntologyFile> = files.iter().map(|f| f.file.clone()).collect();
    resolve_with(&parsed, &NamespaceSet::new(), &limits).map_err(|e| invalid(&e))
}

/// The fixed text of a file that is not UTF-8 or not a YAML document (never a byte of it).
fn malformed_file() -> ApiError {
    ApiError::Invalid(
        OntologyError::SchemaInvalid {
            rule: "file:".into(),
        }
        .to_string(),
    )
}

/// The gate order of one file (EN-S8, core S1, the M5 R5-1 pattern; E1.3a RE1-1): the S7 open
/// and byte budget; the raw text, comments and all, judged in overlapping windows before the
/// YAML parser can drop anything (a reject-class hit is `Rejected` with rule ids, whatever the
/// rest of the file is; a file that is not UTF-8 is refused with the fixed class before any
/// scan); then the blake3 hash; then the plain YAML data and the field-aware gate over every
/// string of it (a redaction refuses the file: ontology text cannot be masked); then the E1.2
/// file form.
fn read_and_gate(api: &KabanApi, root: &Path, path: &Path) -> Result<Loaded, ApiError> {
    let bytes =
        kaban_policy::read_contained(root, path, MAX_ONTOLOGY_FILE_BYTES).map_err(contain_error)?;
    let text = String::from_utf8(bytes).map_err(|_| malformed_file())?;
    api.gate_joined(TOOL, "file", &text)?;
    let hash = blake3_hex(text.as_bytes());
    let data: Value = yaml_serde::from_str(&text).map_err(|_| malformed_file())?;
    let mut redactions = Vec::new();
    api.gate_json(TOOL, "file", &data, &mut redactions)?;
    if !redactions.is_empty() {
        return Err(ApiError::Invalid(FILE_REDACTED.to_owned()));
    }
    let file = OntologyFile::from_yaml(&text).map_err(|e| invalid(&e))?;
    Ok(Loaded {
        path: Some(path.to_path_buf()),
        ns: file.ontology.clone(),
        version: file.version,
        hash,
        file,
    })
}

/// A user file may not declare in the shipped namespaces (`core`, `adr`) nor redefine a
/// shipped stored name from the bare `user` namespace (`ontology_builtin_immutable`); the
/// store's builtin refusal is the backstop.
fn check_builtin_immutable(file: &Loaded) -> Result<(), ApiError> {
    let shipped = shipped()?;
    let qualify = |name: &str| Qualified {
        ns: file.ns.clone(),
        name: name.to_owned(),
    };
    if shipped.namespaces.contains(&file.ns) {
        let first = file
            .file
            .kinds
            .keys()
            .map(|k| k.as_str().to_owned())
            .chain(file.file.rels.keys().map(ToString::to_string))
            .next()
            .unwrap_or_default();
        return Err(invalid(&OntologyError::BuiltinImmutable(qualify(&first))));
    }
    let kinds = file.file.kinds.keys().map(|k| qualify(k.as_str()));
    let rels = file.file.rels.keys().map(|r| qualify(r.as_str()));
    let clash = kinds
        .filter(|q| {
            shipped
                .kinds
                .keys()
                .any(|s| stored_name(s) == stored_name(q))
        })
        .chain(rels.filter(|q| {
            shipped
                .rels
                .keys()
                .any(|s| stored_name(s) == stored_name(q))
        }))
        .next();
    match clash {
        Some(q) => Err(invalid(&OntologyError::BuiltinImmutable(q))),
        None => Ok(()),
    }
}

/// Upsert the kinds then the rels of `file`'s namespace under their stored names and record
/// the namespace's hash and version. The hash is written last, so a partial write never reads
/// as a completed load: the next load repeats the upserts.
fn write_namespace(api: &KabanApi, ontology: &Ontology, file: &Loaded) -> Result<(), ApiError> {
    let ends = |ends: &[RelEnd]| -> Vec<String> {
        ends.iter()
            .filter_map(|end| match end {
                RelEnd::Kind(q) => Some(stored_name(q)),
                // the `path` literal has no `schema_type` row before 0004 (as the seeds)
                RelEnd::Path => None,
            })
            .collect()
    };
    for kind in ontology.kinds.values().filter(|k| k.id.ns == file.ns) {
        api.store.schema_extend(TypeDef {
            kind: "label".into(),
            name: stored_name(&kind.id),
            ns: None,
            description: kind.description.clone(),
            from_kinds: vec![],
            to_kinds: vec![],
            directed: true,
            props_schema: Some(kind.props_schema.as_value().clone()),
            builtin: false,
            // the JSON declaration columns, canonical (E1.7 R2, E1.8)
            declaration: kaban_core::TypeDeclaration::of(kind),
        })?;
    }
    for rel in ontology.rels.values().filter(|r| r.id.ns == file.ns) {
        api.store.schema_extend(TypeDef {
            kind: "rel".into(),
            name: stored_name(&rel.id),
            ns: None,
            // the store requires a description; a rel without one is described by its name
            description: rel
                .description
                .clone()
                .unwrap_or_else(|| stored_name(&rel.id)),
            from_kinds: ends(&rel.from),
            to_kinds: ends(&rel.to),
            directed: rel.directed,
            props_schema: None,
            builtin: false,
            declaration: kaban_core::TypeDeclaration::default(),
        })?;
    }
    api.store.set_index_meta(
        &format!("ontology_version:{}", file.ns),
        &file.version.to_string(),
    )?;
    api.store
        .set_index_meta(&format!("ontology_hash:{}", file.ns), &file.hash)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;
    use crate::ApiConfig;
    use kaban_core::{MemoryStore, NewNode, RememberOutcome};
    use kaban_policy::Policy;
    use kaban_store_sqlite::SqliteStore;

    /// A scratch repository root with `.kaban/ontology/`, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("kaban-e14-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(ONTOLOGY_DIR)).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn api_over(store: &Arc<SqliteStore>) -> KabanApi {
        KabanApi::with_config(
            Arc::clone(store) as Arc<dyn kaban_core::GraphStore>,
            Policy::empty(),
            store.signer(),
            ApiConfig::default(),
        )
    }

    /// E1.4: a load publishes the rule set into the service's registry; before the load the
    /// service reads the shipped set. A fresh record of a loaded kind is active.
    #[test]
    fn a_load_publishes_the_rule_set_into_the_registry() {
        // Arrange
        let scratch = Scratch::new("publish");
        let store = Arc::new(SqliteStore::open_memory().unwrap());
        let registry = Arc::new(OntologyRegistry::new(Some(scratch.0.clone())).unwrap());
        let api = api_over(&store).with_registry(Arc::clone(&registry));
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kaban-core/fixtures/ontology/incidents.yaml");
        let path = scratch.0.join(ONTOLOGY_DIR).join("incidents.yaml");
        std::fs::copy(fixture, &path).unwrap();
        let before = api.rules();
        assert!(before.dedups("Decision") && before.rules("incidents:Incident").is_none());
        assert_eq!(
            *before,
            *RuleSet::shipped(),
            "the shipped set before any load"
        );

        // Act
        registry.load_file(&api, &path).unwrap();

        // Assert
        let after = api.rules();
        assert!(after.rules("incidents:Incident").is_some());
        assert!(after.dedups("Decision"), "the shipped rules stay");
        assert!(
            after.rules("incidents:Decision").is_none(),
            "no rules block, no rules"
        );
        assert!(after.archives_on_link("incidents:REPLACES"));
        assert_eq!(*after, *registry.rule_set());
        let outcome = store
            .remember(
                NewNode {
                    kind: "incidents:Incident".into(),
                    text: "the numbat queue stalled".into(),
                    ..Default::default()
                },
                &[],
                &api.write_ctx(None),
            )
            .unwrap();
        let RememberOutcome::Created(node) = outcome else {
            panic!("fresh store: {outcome:?}");
        };
        // a fresh record of a loaded kind is active at commit (REC-08, T3)
        assert_eq!(node.lifecycle, kaban_core::Lifecycle::Active);
    }

    /// E1.5: the registry is the one owner of the loaded ontology: a load through the
    /// service's registry is visible to every clone of the service (the rule set, the kind
    /// view), and a service built without a registry starts from the shipped files.
    #[test]
    fn the_service_reads_the_registry_it_holds_and_every_clone_sees_a_load() {
        // Arrange
        let scratch = Scratch::new("holds");
        let store = Arc::new(SqliteStore::open_memory().unwrap());
        let registry = Arc::new(OntologyRegistry::new(Some(scratch.0.clone())).unwrap());
        let api = api_over(&store).with_registry(Arc::clone(&registry));
        let clone = api.clone();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kaban-core/fixtures/ontology/incidents.yaml");
        let path = scratch.0.join(ONTOLOGY_DIR).join("incidents.yaml");
        std::fs::copy(fixture, &path).unwrap();
        assert!(api.registry().kind("incidents:Incident").is_none());
        assert_eq!(
            api_over(&store).registry().list().len(),
            kaban_core::shipped::FILES.len(),
            "a service without a registry starts from the shipped files"
        );

        // Act
        api.registry().load_file(&api, &path).unwrap();

        // Assert
        assert!(clone.registry().kind("incidents:Incident").is_some());
        assert!(clone.rules().rules("incidents:Incident").is_some());
        assert!(Arc::ptr_eq(&api.rules(), &clone.rules()));
        assert_eq!(registry.list().len(), kaban_core::shipped::FILES.len() + 1);
    }

    /// The hooks' adapter kinds are declared by the shipped `adr.yaml` under their bare names
    /// (EN-F4: "retained as the adapter kinds"); `Failure` has display rules, the other two
    /// do not.
    #[test]
    fn the_shipped_registry_declares_the_hook_adapter_kinds_under_adr() {
        // Arrange
        let registry = OntologyRegistry::new(None).unwrap();
        let kinds = crate::hooks::ADAPTER_KINDS;

        // Act
        let views: Vec<KindView> = [kinds.failure, kinds.episode, kinds.improvement]
            .iter()
            .map(|name| {
                registry
                    .kind(name)
                    .unwrap_or_else(|| panic!("{name} declared"))
            })
            .collect();

        // Assert
        for view in &views {
            assert_eq!(view.id.ns.as_str(), "adr", "{}", view.stored);
            assert_eq!(view.stored, view.id.name, "a bare stored name");
        }
        let set = registry.rule_set();
        assert!(set.display(kinds.failure).is_some());
        assert!(set.display(kinds.episode).is_none());
        assert!(set.display(kinds.improvement).is_none());
    }
}
