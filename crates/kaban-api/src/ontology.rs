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

use kaban_core::DiscoveredRules;
use kaban_core::config::{EngineConfig, PromoteWhen};
use kaban_core::ontology::meta::{
    DiscoveredPolicyName, Format, Governed, OnLink, Serialize as SerializeTarget, Stamp,
};
use kaban_core::ontology::resolve::{RelEnd, ResolvedKind, resolve_with};
use kaban_core::{
    GovernedSet, NamespaceSet, Ns, Ontology, OntologyError, OntologyFile, Qualified, TypeDef,
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

/// Rows per page when a promotion's group is checked against the target's schema (the
/// store clamps `list_nodes` to 500).
const PROMOTE_PAGE: usize = 500;

/// The discovered-labels knobs the service applies (EN-F5; engine-plan E1.8): the rules a
/// write carries and the prune/promotion thresholds of `ontology prune` and `doctor`.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredKnobs {
    /// Policy and cap (on every [`kaban_core::WriteCtx`]).
    pub rules: DiscoveredRules,
    /// Days before an unpromoted candidate kind is prunable.
    pub prune_after_days: u32,
    /// Promotion eligibility thresholds.
    pub promote_when: PromoteWhen,
}

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
    /// Governance mixin, when the kind is governed.
    pub governed: Option<Governed>,
    /// The merged `props_schema`.
    pub props_schema: Value,
}

/// Where a governed kind serializes (`governed.serialize` when it is not `none`): the file
/// lives at `.kaban/<ontology>/<dir>/<id>.md` and carries `ontology: <ontology>@<version>`
/// (engine-plan E1.6, OWN-46/47).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SerializeDir {
    /// The ontology (namespace) that declares the kind: the first path component under
    /// `.kaban/`.
    pub ontology: String,
    /// The loaded `version` of that ontology (the frontmatter key's version).
    pub version: u32,
    /// Directory under `.kaban/<ontology>/`.
    pub dir: String,
    /// File format.
    pub format: Format,
}

/// One export block (`governed.export`) of a governed kind.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlockSpec {
    /// The ontology (namespace) that declares the kind (`--ontology` selection, the digest's
    /// directory `.kaban/<ontology>/`).
    pub ontology: String,
    /// The stored kind name.
    pub kind: String,
    /// Heading, when the file gives one.
    pub heading: Option<String>,
    /// Listed in the digest.
    pub digest: bool,
    /// Rendered in the marker block.
    pub marker: bool,
    /// The record directory under `.kaban/<ontology>/` (the entry links), `None` for a kind
    /// with `serialize: none`.
    pub dir: Option<String>,
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
    /// The kind names in the order the file declares them (the export block and digest
    /// section order of EN-F4, "their position in the shipped file"; the parsed
    /// `OntologyFile.kinds` is a sorted map and has lost it).
    kind_order: Vec<String>,
}

/// The `kinds:` keys of an ontology file in declaration order (a YAML mapping keeps it;
/// `yaml_serde::Mapping` is insertion-ordered). Empty when the text has no `kinds` mapping.
fn declared_kind_order(text: &str) -> Vec<String> {
    yaml_serde::from_str::<yaml_serde::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("kinds")
                .and_then(yaml_serde::Value::as_mapping)
                .cloned()
        })
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
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
    /// The governed set of `ontology`, derived once per load (every write context reads it).
    governed: Arc<GovernedSet>,
}

impl State {
    /// The position of a kind in the loaded set: its file's load order, then its declaration
    /// position in that file (EN-F4 "position in the shipped file"), for the serialization
    /// and export orders. `None` when no loaded file declares the kind's namespace.
    fn position(&self, id: &Qualified) -> Option<(usize, usize)> {
        let (order, file) = self.files.iter().enumerate().find(|(_, f)| f.ns == id.ns)?;
        let declared = file
            .kind_order
            .iter()
            .position(|k| *k == id.name)
            .unwrap_or(usize::MAX);
        Some((order, declared))
    }

    fn new(files: Vec<Loaded>, ontology: Ontology) -> Self {
        let governed = Arc::new(GovernedSet::from_ontology(&ontology));
        Self {
            files,
            ontology,
            governed,
        }
    }
}

/// The registry: the shipped ontologies plus every loaded user file, resolved as one. The
/// service holds it ([`KabanApi::registry`], engine-plan E1.5) and reads every mixin
/// decision through it: the governed set of its write contexts, `why`'s governing kinds,
/// `remember`'s dedup rule and `props_schema`, `accept`'s tier, `link`'s `GOVERNS` endpoints.
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
                    kind_order: declared_kind_order(text),
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
        // the service's writes decide governance by the loaded set from here on (EN-F4, E1.4;
        // the service reads this registry, E1.5)
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = State::new(files, ontology);
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
            governed: k.governed.clone(),
            props_schema: k.props_schema.as_value().clone(),
        })
    }

    /// The governance mixin of the kind stored under `stored` (a clone: the registry is
    /// shared behind a lock).
    pub fn governed(&self, stored: &str) -> Option<Governed> {
        self.kind(stored).and_then(|k| k.governed)
    }

    /// The governed kinds and `on_link` rels of everything loaded, as the store decides
    /// governance by them (engine-plan E1.4; derived once per load, shared by every write
    /// context).
    pub fn governed_set(&self) -> Arc<GovernedSet> {
        Arc::clone(&self.read().governed)
    }

    /// The discovered-labels knobs of the loaded set (engine-spec EN-F5; engine-plan E1.8):
    /// the policy of the last loaded namespace declaring a `discovered` block (`review` when
    /// none does; the shipped `adr.yaml` declares `review`), and that block's `cap`,
    /// `prune_after_days` and `promote_when` (the `[discovered]` register of `engine` when no
    /// block is loaded: the register is the default, not the only source). Two loaded
    /// namespaces declaring different policies are reported once per call by a `warn` with
    /// the two namespace names (no user bytes); the later one wins.
    pub fn discovered(&self, engine: &EngineConfig) -> DiscoveredKnobs {
        let state = self.read();
        let mut declared: Option<(&Ns, &kaban_core::ontology::meta::DiscoveredPolicy)> = None;
        for ns in &state.ontology.namespaces {
            if let Some(block) = state.ontology.discovered.get(ns) {
                if let Some((earlier, before)) = declared
                    && before.policy != block.policy
                {
                    tracing::warn!(earlier = %earlier, later = %ns, "two loaded namespaces declare different discovered policies; the later one applies");
                }
                declared = Some((ns, block));
            }
        }
        match declared {
            Some((_, block)) => DiscoveredKnobs {
                rules: DiscoveredRules {
                    policy: block.policy,
                    cap: block.cap,
                },
                prune_after_days: block.prune_after_days,
                promote_when: PromoteWhen {
                    min_count: block.promote_when.min_count,
                    min_sources: block.promote_when.min_sources,
                    min_mean_score: block.promote_when.min_mean_score,
                },
            },
            None => DiscoveredKnobs {
                rules: DiscoveredRules {
                    policy: DiscoveredPolicyName::Review,
                    cap: engine.discovered.cap,
                },
                prune_after_days: engine.discovered.prune_after_days,
                promote_when: engine.discovered.promote_when.clone(),
            },
        }
    }

    /// `kaban ontology promote <from> <to>` (EN-F5, human-only): `to` is resolved against the
    /// loaded set (a bare name in the bare-namespace order, else qualified) and must be a
    /// declared kind (`unknown_kind` otherwise) the group can take (E1.9a RE3-4: not governed,
    /// since a governed record starts `proposed` and derives its `dedup` key at write time,
    /// and every active record of the group satisfies its `props_schema`); the decision is
    /// this orchestrator's, from the registry, before the store renames in one transaction.
    ///
    /// # Errors
    /// `Invalid("unknown_kind")` when `to` is not a loaded kind; `Store(PromoteTargetIncompatible)`
    /// when the target is governed or some record of the group fails its schema; store errors
    /// (`HumanOnly` for an agent actor, `unknown_kind` when `from` is not a counted `x:` kind,
    /// `PromoteConflict` on a dedup identity collision).
    pub fn promote(
        &self,
        api: &KabanApi,
        from: &str,
        to: &str,
        actor: &kaban_core::Actor,
    ) -> Result<kaban_core::PromoteReport, ApiError> {
        let (target, governed, schema) = {
            let state = self.read();
            state
                .ontology
                .kinds
                .values()
                .find(|k| stored_name(&k.id) == to || k.id.to_string() == to)
                .map(|k| (k.id.clone(), k.governed.is_some(), k.props_schema.clone()))
        }
        .ok_or_else(|| {
            invalid(&OntologyError::UnknownKind {
                path: "promote.to".to_owned(),
            })
        })?;
        if governed {
            return Err(kaban_core::StoreError::PromoteTargetIncompatible.into());
        }
        // every active record of the group against the target's schema, page by page, before
        // anything moves (the store lists active rows only: a retired record of the group is
        // renamed without this check and stays retired)
        let ws = api.workspace().as_str();
        let mut offset = 0;
        loop {
            let page = api.store.list_nodes(from, ws, offset, PROMOTE_PAGE)?;
            if page
                .iter()
                .any(|node| first_error_text(&validate(&schema, &node.props)).is_some())
            {
                return Err(kaban_core::StoreError::PromoteTargetIncompatible.into());
            }
            if page.len() < PROMOTE_PAGE {
                break;
            }
            offset += page.len();
        }
        Ok(api
            .store
            .promote_kind(api.workspace(), from, &target, actor)?)
    }

    /// `kaban ontology prune [--dry-run]` (EN-F5): the loaded knobs decide the thresholds.
    ///
    /// # Errors
    /// Store errors.
    pub fn prune(
        &self,
        api: &KabanApi,
        dry_run: bool,
    ) -> Result<kaban_core::PruneReport, ApiError> {
        let knobs = self.discovered(api.engine());
        Ok(api.store.prune_kinds(
            api.workspace(),
            &knobs.promote_when,
            std::time::Duration::from_secs(u64::from(knobs.prune_after_days) * 86_400),
            dry_run,
        )?)
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

    /// The stored names of every loaded kind whose effective `index.stamp` is `commit` (the
    /// default): the set `SyncStore::stamp_commit` updates (engine-spec §3.1 "Indexing
    /// meta-config", OWN-65; the shipped `Document`, `Section`, `Chunk` and `Tag` are `none`).
    /// In the resolver's order (by qualified identity).
    pub fn stamp_kinds(&self) -> Vec<String> {
        let state = self.read();
        state
            .ontology
            .kinds
            .values()
            .filter(|k| k.stamp() == Stamp::Commit)
            .map(|k| stored_name(&k.id))
            .collect()
    }

    /// The effective stamping policy of the kind stored under `stored`; `None` when the loaded
    /// files do not declare it (a discovered `x:` kind, for one).
    pub fn index_policy(&self, stored: &str) -> Option<Stamp> {
        self.with_kind(stored, ResolvedKind::stamp)
    }

    /// The stored names of the kinds whose records govern paths (`governs_paths: true`).
    pub fn governing_kinds(&self) -> Vec<String> {
        let state = self.read();
        state
            .ontology
            .kinds
            .values()
            .filter(|k| k.governed.as_ref().is_some_and(|g| g.governs_paths))
            .map(|k| stored_name(&k.id))
            .collect()
    }

    /// The kinds that serialize to files, with their ontology, its loaded version and their
    /// directory, in load order of their files then by stored name (the order `sync` walks).
    pub fn serialized_kinds(&self) -> Vec<(String, SerializeDir)> {
        let state = self.read();
        let mut kinds: Vec<((usize, usize), String, SerializeDir)> = state
            .ontology
            .kinds
            .values()
            .filter_map(|k| {
                let governed = k.governed.as_ref()?;
                let position = state.position(&k.id)?;
                let file = state.files.iter().find(|f| f.ns == k.id.ns)?;
                match &governed.serialize {
                    SerializeTarget::Dir(target) => Some((
                        position,
                        stored_name(&k.id),
                        SerializeDir {
                            ontology: file.ns.to_string(),
                            version: file.version,
                            dir: target.dir.to_string(),
                            format: target.format,
                        },
                    )),
                    SerializeTarget::None(_) => None,
                }
            })
            .collect();
        kinds.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        kinds
            .into_iter()
            .map(|(_, kind, dir)| (kind, dir))
            .collect()
    }

    /// Whether an ontology named `ns` is loaded at exactly `version` (the frontmatter check of
    /// `sync`: a file names the version that wrote it, OWN-47).
    pub fn has_version(&self, ns: &str, version: u32) -> bool {
        self.read()
            .files
            .iter()
            .any(|f| f.ns.as_str() == ns && f.version == version)
    }

    /// The export blocks of the governed kinds, in load order of their files then in the
    /// order the file declares them (EN-F4; the shipped `adr.yaml`: Decision, Convention,
    /// Failure), then by name.
    pub fn export_blocks(&self) -> Vec<BlockSpec> {
        let state = self.read();
        let mut blocks: Vec<((usize, usize), BlockSpec)> = state
            .ontology
            .kinds
            .values()
            .filter_map(|k| {
                let governed = k.governed.as_ref()?;
                let order = state.position(&k.id)?;
                let dir = match &governed.serialize {
                    SerializeTarget::Dir(target) => Some(target.dir.to_string()),
                    SerializeTarget::None(_) => None,
                };
                Some((
                    order,
                    BlockSpec {
                        ontology: k.id.ns.to_string(),
                        kind: stored_name(&k.id),
                        heading: governed.export.heading.clone(),
                        digest: governed.export.digest,
                        marker: governed.export.marker,
                        dir,
                    },
                ))
            })
            .collect();
        blocks.sort_by(|a, b| (a.0, &a.1.kind).cmp(&(b.0, &b.1.kind)));
        blocks.into_iter().map(|(_, b)| b).collect()
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
    let mut ctx = api.write_ctx(None);
    api.gate_joined(&mut ctx, TOOL, "file", &text)?;
    let hash = blake3_hex(text.as_bytes());
    let data: Value = yaml_serde::from_str(&text).map_err(|_| malformed_file())?;
    let mut redactions = Vec::new();
    api.gate_json(&mut ctx, TOOL, "file", &data, &mut redactions)?;
    if !redactions.is_empty() {
        api.flush_audit(&mut ctx)?;
        return Err(ApiError::Invalid(FILE_REDACTED.to_owned()));
    }
    let file = OntologyFile::from_yaml(&text).map_err(|e| invalid(&e))?;
    Ok(Loaded {
        path: Some(path.to_path_buf()),
        ns: file.ontology.clone(),
        version: file.version,
        hash,
        file,
        kind_order: declared_kind_order(&text),
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
    use kaban_core::{Governance, MemoryStore, NewNode, RememberOutcome};
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
            None,
            ApiConfig::default(),
        )
    }

    #[test]
    fn shipped_yaml_keeps_declaration_order_in_public_export_blocks() {
        let registry = OntologyRegistry::new(None).unwrap();
        let kinds: Vec<String> = registry
            .export_blocks()
            .into_iter()
            .map(|block| block.kind)
            .collect();
        assert_eq!(kinds, ["Decision", "Convention", "Failure"]);
    }

    /// E1.4: a load publishes the governed set into the service, so `write_ctx` carries the
    /// loaded kinds and a user kind with the mixin is `proposed` on write; before the load the
    /// service decides by the shipped set. (E1.5: the service reads the registry it holds, so
    /// the registry is handed to the service; the assertions are E1.4's.)
    #[test]
    fn a_load_publishes_the_governed_set_the_write_context_carries() {
        // Arrange
        let scratch = Scratch::new("publish");
        let store = Arc::new(SqliteStore::open_memory().unwrap());
        let registry = Arc::new(OntologyRegistry::new(Some(scratch.0.clone())).unwrap());
        let api = api_over(&store).with_registry(Arc::clone(&registry));
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kaban-core/fixtures/ontology/incidents.yaml");
        let path = scratch.0.join(ONTOLOGY_DIR).join("incidents.yaml");
        std::fs::copy(fixture, &path).unwrap();
        let before = api.governed();
        assert!(before.contains("Decision") && !before.contains("incidents:Incident"));
        assert_eq!(
            *before,
            *GovernedSet::shipped(),
            "the shipped set before any load"
        );

        // Act
        registry.load_file(&api, &path).unwrap();

        // Assert
        let after = api.write_ctx(None).governed;
        assert!(after.contains("incidents:Incident"));
        assert!(
            after.contains("Decision"),
            "the shipped kinds stay governed"
        );
        assert!(
            !after.contains("incidents:Decision"),
            "no mixin, not governed"
        );
        assert!(after.archives_on_link("incidents:REPLACES"));
        assert_eq!(*after, *registry.governed_set());
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
        assert_eq!(node.governance, Governance::Proposed);
    }

    /// E1.5: the registry is the one owner of the loaded ontology: a load through the
    /// service's registry is visible to every clone of the service (the governed set, the
    /// kind view), and a service built without a registry starts from the shipped files.
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
        assert!(clone.governed().contains("incidents:Incident"));
        assert!(Arc::ptr_eq(&api.governed(), &clone.governed()));
        assert_eq!(registry.list().len(), kaban_core::shipped::FILES.len() + 1);
    }

    /// E1.5 (core-spec §4, engine-spec EN-F4): the `GOVERNS` sources `link` enforces are the
    /// governing kinds (`governs_paths: true`), and for the shipped file that set is exactly
    /// the declared `rels.GOVERNS.from` (Decision, Convention): the rule and the declaration
    /// agree, and a loaded governing kind joins the set.
    #[test]
    fn the_shipped_governs_sources_are_exactly_the_governing_kinds() {
        // Arrange
        let scratch = Scratch::new("governs");
        let store = Arc::new(SqliteStore::open_memory().unwrap());
        let registry = Arc::new(OntologyRegistry::new(Some(scratch.0.clone())).unwrap());
        let api = api_over(&store).with_registry(Arc::clone(&registry));
        let declared: Vec<String> = {
            let state = registry.read();
            let governs = state
                .ontology
                .rels
                .values()
                .find(|r| stored_name(&r.id) == "GOVERNS")
                .expect("the shipped GOVERNS rel");
            let mut from: Vec<String> = governs
                .from
                .iter()
                .filter_map(|end| match end {
                    RelEnd::Kind(q) => Some(stored_name(q)),
                    RelEnd::Path => None,
                })
                .collect();
            from.sort();
            from
        };

        // Act
        let shipped = registry.governing_kinds();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kaban-core/fixtures/ontology/runbooks.yaml");
        let path = scratch.0.join(ONTOLOGY_DIR).join("runbooks.yaml");
        std::fs::copy(fixture, &path).unwrap();
        registry.load_file(&api, &path).unwrap();
        let loaded = registry.governing_kinds();

        // Assert
        assert_eq!(shipped, declared);
        assert_eq!(declared, ["Convention", "Decision"]);
        assert_eq!(loaded, ["Convention", "Decision", "runbooks:Runbook"]);
    }

    /// The hooks' adapter kinds are declared by the shipped `adr.yaml` under their bare names
    /// (EN-F4: "retained as the adapter kinds"); `Failure` is governed, the other two are not.
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
        let set = registry.governed_set();
        assert!(set.contains(kinds.failure));
        assert!(!set.contains(kinds.episode));
        assert!(!set.contains(kinds.improvement));
    }
}
