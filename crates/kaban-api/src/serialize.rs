//! The `.kaban/<ontology>/<dir>/<id>.md` record file (core-spec F13, §7; core-plan C5.1 as
//! amended by `reviews/m5-prep.md` M5-1, M5-5, M5-7 and owner decisions OWN-18, OWN-25,
//! OWN-46, OWN-47; engine-plan E1.6): one Markdown file with strict YAML frontmatter per
//! project-scoped record of a kind with `governed.serialize != none` (the shipped `adr.yaml`:
//! Decision, Convention, Failure), the four-key tombstone a purge leaves in its place, and the
//! record hash that decides an equal-rev conflict.
//!
//! **Content boundary.** Every string that comes out of a file is host data (core-spec S1
//! "frontmatter values", "import files"). This module parses shapes only: it exposes typed
//! values and content-free errors (a field name of kaban's own vocabulary, a line number,
//! never a value from the file) and gates nothing. The importer (C5.1b) turns the file into one
//! JSON value with [`split`], runs it through `prepare_args` under its unregistered scope, and
//! only then finishes the typed parse with [`parse_frontmatter`]; the renderer's output passes
//! `gate_response_text` before a byte reaches the repository (M5-2, M5-3). `parse` is the
//! ungated convenience for callers that already hold judged text (tests, `resolve`).
//!
//! **File shape** (M5-1): `---\n<YAML frontmatter, keys in BTreeMap order>\n---\n<text>\n`, one
//! trailing newline, `text` the whole body with that newline stripped. Frontmatter keys, every
//! one optional unless marked R: `accepted_commit`, `confidence`, `content_hash` (R, the
//! database dedup hash, informational), `created_at` (R), `events` (oldest first), `governance`
//! (R), `id` (R), `importance`, `kind` (R, equal to the directory's kind), `lifecycle` (R),
//! `ontology` (`<name>@<version>`, the ontology that declares the kind and the version loaded
//! when the file was written, E1.6/OWN-47; every file the engine writes carries it; a file
//! without it is a pre-engine file and is read as `adr@1`), `path_scope` (canonical, sorted),
//! `props` (omitted when `{}`), `provenance`, `rev` (R), `summary`, `tags`, `updated_at` (R),
//! `valid_from`, `valid_to`, `version` (R, `1`). Omitted on purpose: `dup_key` (recomputed on
//! import), `policy_version` (re-gated on import), `store_scope` (always `project`: personal
//! records are never written) and `workspace_id` (OWN-25: the repository is the workspace;
//! import binds to the process workspace).
//!
//! **Tombstone** (M5-7, OWN-18): `---\nid: <id>\nlifecycle: purged\nrev: <rev>\nversion: 1\n---\n`
//! and nothing else; any other key or a body is [`SerializeError::TombstoneShape`].
//!
//! **Record hash** (M5-5): sha256 over the canonical rendering of `kind`, `lifecycle`,
//! `governance`, `path_scope`, `text`, `summary`, `tags`, `props`, `importance`, `confidence`,
//! `valid_from`, `valid_to`; `rev`, provenance, events, timestamps, the dedup hash and the
//! `ontology` key take no part, so a props-only edit at equal rev is still a conflict while a
//! pre-engine file without the key hashes like its stored record.

use crate::currency::canonical_path_scope;
use kaban_core::{Event, Governance, Lifecycle, Node, NodeId, SignerKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use thiserror::Error;

/// The file format version this binary writes and the highest it reads (core-spec §7: files
/// are re-importable by any binary of the same major).
pub const FORMAT_VERSION: u64 = 1;

/// The frontmatter keys this version knows, sorted (the order they are written in).
pub const KEYS: &[&str] = &[
    "accepted_commit",
    "confidence",
    "content_hash",
    "created_at",
    "events",
    "governance",
    "id",
    "importance",
    "kind",
    "lifecycle",
    "ontology",
    "path_scope",
    "props",
    "provenance",
    "rev",
    "summary",
    "tags",
    "updated_at",
    "valid_from",
    "valid_to",
    "version",
];

/// The keys a tombstone carries, exactly.
pub const TOMBSTONE_KEYS: &[&str] = &["id", "lifecycle", "rev", "version"];

/// Keys that must be present on a record.
const REQUIRED: &[&str] = &[
    "content_hash",
    "created_at",
    "governance",
    "id",
    "kind",
    "lifecycle",
    "rev",
    "updated_at",
    "version",
];

const OPEN: &str = "---\n";
const CLOSE: &str = "\n---\n";

/// Why a file could not be read as a record. Messages name kaban's own key vocabulary and
/// positions only, never a value from the file (core-spec S1, S3).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SerializeError {
    /// The file does not start with `---\n` or has no closing `\n---\n`.
    #[error("the file has no `---` frontmatter block")]
    NoFrontmatter,
    /// The frontmatter is not valid YAML; position only.
    #[error("the frontmatter is not valid YAML (line {line}, column {column})")]
    Yaml {
        /// 1-based line inside the frontmatter, 0 when unknown.
        line: usize,
        /// 1-based column, 0 when unknown.
        column: usize,
    },
    /// The frontmatter is not a mapping with string keys.
    #[error("the frontmatter is not a key/value mapping")]
    NotAMapping,
    /// A key this version does not know (strict parse, M5-1). The key is file content and is
    /// not echoed.
    #[error("the frontmatter has a key this kaban does not know")]
    UnknownKey,
    /// A required key is absent.
    #[error("the frontmatter is missing the required key `{0}`")]
    MissingKey(&'static str),
    /// A known key holds a value of the wrong shape (a non-integer `rev`, a non-UUIDv7 `id`,
    /// an unknown `lifecycle` word, ...). Only kaban's key name is named.
    #[error("the frontmatter value of `{0}` has the wrong shape")]
    BadValue(&'static str),
    /// `version` is above [`FORMAT_VERSION`]: written by a newer kaban.
    #[error("file format version {found} is newer than this kaban reads (up to {supported})")]
    UnsupportedVersion {
        /// The file's version.
        found: u64,
        /// [`FORMAT_VERSION`].
        supported: u64,
    },
    /// The file sits in a directory of one kind and declares another (M5-1).
    #[error("the record's kind does not match its directory")]
    KindMismatch,
    /// `lifecycle: purged` with anything but the four tombstone keys and an empty body.
    #[error("a tombstone carries exactly id, lifecycle, rev and version and no body")]
    TombstoneShape,
    /// The file's `ontology: <name>@<version>` names a version this process has not loaded
    /// (OWN-47: a file is validated against the version that wrote it, never against whichever
    /// version happens to be loaded). Fixed class, no value.
    #[error("ontology_version_unknown: the file names an ontology version that is not loaded")]
    OntologyVersionUnknown,
}

/// The `ontology: <name>@<version>` frontmatter value (E1.6, OWN-47): which ontology declares
/// the record's kind and which version of it was loaded when the file was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OntologyRef {
    /// The ontology (namespace) name.
    pub name: String,
    /// Its `version`.
    pub version: u32,
}

impl OntologyRef {
    /// The pre-engine files' implicit ontology: the shipped `adr` at version 1 (a file
    /// without the key was written before the engine and belongs to the shipped layout).
    #[must_use]
    pub fn pre_engine() -> Self {
        Self {
            name: "adr".to_owned(),
            version: 1,
        }
    }

    /// `<name>@<version>`: a namespace-shaped name and a decimal version; `None` otherwise.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (name, version) = s.split_once('@')?;
        let version: u32 = version.parse().ok()?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        Some(Self {
            name: name.to_owned(),
            version,
        })
    }
}

impl std::fmt::Display for OntologyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// Provenance block of a record file (M5-1): who and where, never a workspace (OWN-25).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordProvenance {
    /// Agent that produced the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Commit the record was stamped with (`post-commit`, M5-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// Repository identity (the root directory name, M3-23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Session that produced the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl RecordProvenance {
    fn is_empty(&self) -> bool {
        self.agent_id.is_none()
            && self.commit_sha.is_none()
            && self.repo.is_none()
            && self.session_id.is_none()
    }
}

/// One governance event as the file carries it (M5-1): the acceptance proof reads the last
/// `accepted` entry's `reason`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordEvent {
    /// Commit at the time of the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// `accepted`, `superseded`, `invalidated`, ...
    pub kind: String,
    /// Why.
    pub reason: String,
    /// Signer identity when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// Who signed.
    pub signer_kind: SignerKind,
    /// Unix seconds.
    pub ts: i64,
}

impl From<&Event> for RecordEvent {
    fn from(e: &Event) -> Self {
        Self {
            commit: e.commit_sha.clone(),
            kind: e.kind.clone(),
            reason: e.reason.clone(),
            signer: e.signer.clone(),
            signer_kind: e.signer_kind,
            ts: e.ts,
        }
    }
}

/// A record as the file carries it: the frontmatter fields plus the body (`text`). Built from
/// a stored node by [`SerializedRecord::from_node`], read from a file by [`parse`].
#[derive(Debug, Clone, PartialEq)]
pub struct SerializedRecord {
    /// Node id (the file name).
    pub id: NodeId,
    /// Node kind; equals the directory's kind.
    pub kind: String,
    /// Storage visibility (never `purged`: that is a tombstone).
    pub lifecycle: Lifecycle,
    /// Governance state.
    pub governance: Governance,
    /// Monotonic revision (F13).
    pub rev: u64,
    /// The memory text (the body).
    pub text: String,
    /// One-line summary.
    pub summary: Option<String>,
    /// Tags in stored order.
    pub tags: Vec<String>,
    /// Kind props (`{}` when none).
    pub props: Value,
    /// 0..1.
    pub importance: Option<f64>,
    /// 0..1.
    pub confidence: Option<f64>,
    /// Validity start.
    pub valid_from: Option<String>,
    /// Validity end.
    pub valid_to: Option<String>,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// Last update time (RFC 3339).
    pub updated_at: String,
    /// Commit at acceptance.
    pub accepted_commit: Option<String>,
    /// Canonical, sorted path scope.
    pub path_scope: Vec<String>,
    /// Who and where.
    pub provenance: RecordProvenance,
    /// The database dedup hash at render time (informational, F13 wording).
    pub content_hash: String,
    /// Governance events, oldest first.
    pub events: Vec<RecordEvent>,
    /// The ontology and version that wrote the file (`ontology: <name>@<version>`); `None`
    /// for a pre-engine file without the key (read as `adr@1` by the importer).
    pub ontology: Option<OntologyRef>,
}

impl SerializedRecord {
    /// The file view of a stored node and its governance events. A purged node has no record
    /// view: render it with [`render_tombstone`] (the caller checks `lifecycle`).
    ///
    /// # Errors
    /// [`SerializeError::BadValue`] (`id`) when the stored id is not a hyphenated `UUIDv7`; every
    /// id kaban writes is one, so this guards a corrupted row rather than a normal path.
    pub fn from_node(node: &Node, events: &[Event]) -> Result<Self, SerializeError> {
        let id = NodeId::parse(&node.id).map_err(|_| SerializeError::BadValue("id"))?;
        let mut path_scope = canonical_path_scope(&node.path_scope);
        path_scope.sort_unstable();
        Ok(Self {
            id,
            kind: node.kind.clone(),
            lifecycle: node.lifecycle,
            governance: node.governance,
            rev: u64::try_from(node.rev).unwrap_or(0),
            text: node.text.clone(),
            summary: node.summary.clone(),
            tags: node.tags.clone(),
            props: node.props.clone(),
            importance: node.importance,
            confidence: node.confidence,
            valid_from: node.valid_from.clone(),
            valid_to: node.valid_to.clone(),
            created_at: node.created_at.clone(),
            updated_at: node.updated_at.clone(),
            accepted_commit: node.accepted_commit.clone(),
            path_scope,
            provenance: RecordProvenance {
                agent_id: node.agent_id.clone(),
                commit_sha: node.commit_sha.clone(),
                repo: node.repo.clone(),
                session_id: node.session_id.clone(),
            },
            content_hash: node.content_hash.clone(),
            events: events.iter().map(RecordEvent::from).collect(),
            ontology: None,
        })
    }

    /// The same record stamped with the ontology and version that declare its kind (what
    /// every file the engine writes carries).
    #[must_use]
    pub fn with_ontology(mut self, ontology: OntologyRef) -> Self {
        self.ontology = Some(ontology);
        self
    }
}

/// What a file turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// A record.
    Record(Box<SerializedRecord>),
    /// A purge tombstone (M5-7).
    Tombstone {
        /// The purged id.
        id: NodeId,
        /// The rev the tombstone was written at.
        rev: u64,
    },
}

/// A file split into its two parts, before any typed parse: the frontmatter as one JSON value
/// (for the importer's gate) and the body with its one trailing newline stripped.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFile {
    /// The frontmatter mapping as a JSON object.
    pub frontmatter: Value,
    /// The body (`text`).
    pub body: String,
}

/// Render a stored node as its record file (or its tombstone when it is purged), stamped
/// with `ontology` (`<name>@<version>` of the ontology declaring its kind) when given.
///
/// # Errors
/// As [`SerializedRecord::from_node`].
pub fn render(
    node: &Node,
    events: &[Event],
    ontology: Option<&OntologyRef>,
) -> Result<String, SerializeError> {
    if node.lifecycle == Lifecycle::Purged {
        return Ok(render_tombstone(
            &node.id,
            u64::try_from(node.rev).unwrap_or(0),
        ));
    }
    let mut record = SerializedRecord::from_node(node, events)?;
    record.ontology = ontology.cloned();
    Ok(render_record(&record))
}

/// Render a record: frontmatter keys in sorted order, then the body and one trailing newline.
#[must_use]
pub fn render_record(record: &SerializedRecord) -> String {
    let map = frontmatter_map(record);
    // a BTreeMap of JSON values serializes in key order; nested `props` objects are
    // `serde_json::Map`s, which are BTreeMap-backed (no `preserve_order`), so the whole
    // document is deterministic (N5). The map holds only JSON scalars, lists and objects, so
    // the YAML serializer cannot fail on it; an empty document is the defensive fallback.
    let yaml = yaml_serde::to_string(&map).unwrap_or_default();
    let yaml = yaml.strip_prefix(OPEN).unwrap_or(&yaml);
    let yaml = yaml.strip_suffix('\n').unwrap_or(yaml);
    format!("{OPEN}{yaml}{CLOSE}{}\n", record.text)
}

/// The purge tombstone (M5-7): exactly four keys, no body.
#[must_use]
pub fn render_tombstone(id: &str, rev: u64) -> String {
    format!("{OPEN}id: {id}\nlifecycle: purged\nrev: {rev}\nversion: {FORMAT_VERSION}{CLOSE}")
}

/// Split a file into its frontmatter value and body without judging any value.
///
/// # Errors
/// [`SerializeError::NoFrontmatter`], [`SerializeError::Yaml`] or [`SerializeError::NotAMapping`].
pub fn split(text: &str) -> Result<RawFile, SerializeError> {
    let after_open = text
        .strip_prefix(OPEN)
        .ok_or(SerializeError::NoFrontmatter)?;
    // the frontmatter ends at the first `\n---\n` (or `\n---` at the very end of an empty-body
    // file whose final newline is the closing fence's)
    let (yaml, body) = match after_open.find(CLOSE) {
        Some(at) => (&after_open[..at], &after_open[at + CLOSE.len()..]),
        None => match after_open.strip_suffix("\n---") {
            Some(yaml) => (yaml, ""),
            None => return Err(SerializeError::NoFrontmatter),
        },
    };
    let frontmatter: Value = yaml_serde::from_str(yaml).map_err(|e| {
        let (line, column) = e
            .location()
            .map_or((0, 0), |loc| (loc.line(), loc.column()));
        SerializeError::Yaml { line, column }
    })?;
    if !frontmatter.is_object() {
        return Err(SerializeError::NotAMapping);
    }
    let body = body.strip_suffix('\n').unwrap_or(body).to_owned();
    Ok(RawFile { frontmatter, body })
}

/// Finish the typed parse of a split file (strict: unknown or missing keys, a newer version
/// and a malformed tombstone are errors).
///
/// # Errors
/// Every [`SerializeError`] but `KindMismatch`.
pub fn parse_frontmatter(raw: RawFile) -> Result<Parsed, SerializeError> {
    let RawFile { frontmatter, body } = raw;
    let map = as_object(frontmatter)?;
    if map.keys().any(|k| !KEYS.contains(&k.as_str())) {
        return Err(SerializeError::UnknownKey);
    }
    let version = map
        .get("version")
        .ok_or(SerializeError::MissingKey("version"))?
        .as_u64()
        .ok_or(SerializeError::BadValue("version"))?;
    if version > FORMAT_VERSION {
        return Err(SerializeError::UnsupportedVersion {
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    if map.get("lifecycle").and_then(Value::as_str) == Some(Lifecycle::Purged.as_str()) {
        return parse_tombstone(&map, &body);
    }
    if let Some(missing) = REQUIRED.iter().find(|k| !map.contains_key(**k)) {
        return Err(SerializeError::MissingKey(missing));
    }
    let mut fm = Fields(map);
    let lifecycle = fm.word("lifecycle", Lifecycle::parse)?;
    let governance = fm.word("governance", Governance::parse)?;
    let id = fm.word("id", |s| NodeId::parse(s).ok())?;
    let ontology = match fm.opt_string("ontology")? {
        None => None,
        Some(s) => Some(OntologyRef::parse(&s).ok_or(SerializeError::BadValue("ontology"))?),
    };
    Ok(Parsed::Record(Box::new(SerializedRecord {
        id,
        kind: fm.string("kind")?,
        lifecycle,
        governance,
        rev: fm.integer("rev")?,
        text: body,
        summary: fm.opt_string("summary")?,
        tags: fm.strings("tags")?,
        props: Value::Object(fm.object("props")?),
        importance: fm.opt_float("importance")?,
        confidence: fm.opt_float("confidence")?,
        valid_from: fm.opt_string("valid_from")?,
        valid_to: fm.opt_string("valid_to")?,
        created_at: fm.string("created_at")?,
        updated_at: fm.string("updated_at")?,
        accepted_commit: fm.opt_string("accepted_commit")?,
        path_scope: fm.strings("path_scope")?,
        provenance: fm.typed("provenance")?.unwrap_or_default(),
        content_hash: fm.string("content_hash")?,
        events: fm.typed("events")?.unwrap_or_default(),
        ontology,
    })))
}

/// Typed extraction of the known keys: every shape failure is [`SerializeError::BadValue`]
/// naming that key and nothing from the file (`api-parse-dont-validate`).
struct Fields(Map<String, Value>);

impl Fields {
    fn take(&mut self, key: &'static str) -> Option<Value> {
        self.0.remove(key).filter(|v| !v.is_null())
    }
    fn string(&mut self, key: &'static str) -> Result<String, SerializeError> {
        self.opt_string(key)?.ok_or(SerializeError::MissingKey(key))
    }
    fn opt_string(&mut self, key: &'static str) -> Result<Option<String>, SerializeError> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(SerializeError::BadValue(key)),
        }
    }
    /// A closed-vocabulary word (lifecycle, governance) or an exact-shape value (id).
    fn word<T>(
        &mut self,
        key: &'static str,
        parse: impl FnOnce(&str) -> Option<T>,
    ) -> Result<T, SerializeError> {
        let s = self.string(key)?;
        parse(&s).ok_or(SerializeError::BadValue(key))
    }
    fn integer(&mut self, key: &'static str) -> Result<u64, SerializeError> {
        self.take(key)
            .ok_or(SerializeError::MissingKey(key))?
            .as_u64()
            .ok_or(SerializeError::BadValue(key))
    }
    fn opt_float(&mut self, key: &'static str) -> Result<Option<f64>, SerializeError> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Number(n)) => n.as_f64().map(Some).ok_or(SerializeError::BadValue(key)),
            Some(_) => Err(SerializeError::BadValue(key)),
        }
    }
    fn strings(&mut self, key: &'static str) -> Result<Vec<String>, SerializeError> {
        match self.take(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s),
                    _ => Err(SerializeError::BadValue(key)),
                })
                .collect(),
            Some(_) => Err(SerializeError::BadValue(key)),
        }
    }
    fn object(&mut self, key: &'static str) -> Result<Map<String, Value>, SerializeError> {
        match self.take(key) {
            None => Ok(Map::new()),
            Some(Value::Object(m)) => Ok(m),
            Some(_) => Err(SerializeError::BadValue(key)),
        }
    }
    /// A nested shape with its own strict struct (`deny_unknown_fields`); the serde message
    /// is discarded, only the top-level key is named.
    fn typed<T: serde::de::DeserializeOwned>(
        &mut self,
        key: &'static str,
    ) -> Result<Option<T>, SerializeError> {
        match self.take(key) {
            None => Ok(None),
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|_| SerializeError::BadValue(key)),
        }
    }
}

/// [`split`] then [`parse_frontmatter`], for text the caller has already judged.
///
/// # Errors
/// Every [`SerializeError`] but `KindMismatch`.
pub fn parse(text: &str) -> Result<Parsed, SerializeError> {
    parse_frontmatter(split(text)?)
}

/// [`parse`] for a file found in the directory of `dir_kind`: a record of another kind is
/// [`SerializeError::KindMismatch`]; a tombstone carries no kind and always fits.
///
/// # Errors
/// Every [`SerializeError`].
pub fn parse_in_dir(text: &str, dir_kind: &str) -> Result<Parsed, SerializeError> {
    let parsed = parse(text)?;
    if let Parsed::Record(record) = &parsed
        && record.kind != dir_kind
    {
        return Err(SerializeError::KindMismatch);
    }
    Ok(parsed)
}

/// A tombstone: exactly [`TOMBSTONE_KEYS`], `version` already checked, no body.
fn parse_tombstone(map: &Map<String, Value>, body: &str) -> Result<Parsed, SerializeError> {
    let keys: Vec<&str> = map.keys().map(String::as_str).collect();
    if keys != TOMBSTONE_KEYS || !body.is_empty() {
        return Err(SerializeError::TombstoneShape);
    }
    let id = map
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| NodeId::parse(s).ok())
        .ok_or(SerializeError::BadValue("id"))?;
    let rev = map
        .get("rev")
        .and_then(Value::as_u64)
        .ok_or(SerializeError::BadValue("rev"))?;
    Ok(Parsed::Tombstone { id, rev })
}

/// The record hash (M5-5): sha256 over the domain-tagged canonical JSON of the content fields
/// (`kind`, `lifecycle`, `governance`, `path_scope`, `text`, `summary`, `tags`, `props`,
/// `importance`, `confidence`, `valid_from`, `valid_to`), the object keys sorted and absent
/// optionals written as `null` so an omitted field never aliases a present one.
#[must_use]
pub fn record_hash(record: &SerializedRecord) -> String {
    let mut m: BTreeMap<&'static str, Value> = BTreeMap::new();
    let opt_str = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
    m.insert(
        "confidence",
        record.confidence.map_or(Value::Null, json_f64),
    );
    m.insert(
        "governance",
        Value::String(record.governance.as_str().into()),
    );
    m.insert(
        "importance",
        record.importance.map_or(Value::Null, json_f64),
    );
    m.insert("kind", Value::String(record.kind.clone()));
    m.insert("lifecycle", Value::String(record.lifecycle.as_str().into()));
    m.insert("path_scope", string_list(&record.path_scope));
    m.insert("props", record.props.clone());
    m.insert("summary", opt_str(&record.summary));
    m.insert("tags", string_list(&record.tags));
    m.insert("text", Value::String(record.text.clone()));
    m.insert("valid_from", opt_str(&record.valid_from));
    m.insert("valid_to", opt_str(&record.valid_to));
    // a map of JSON values always serializes
    let canonical = serde_json::to_string(&m).unwrap_or_default();
    kaban_policy::sha256_hex(format!("kaban-record/v1\0{canonical}").as_bytes())
}

/// The frontmatter of a record as a sorted map of JSON values (absent optionals omitted).
fn frontmatter_map(record: &SerializedRecord) -> BTreeMap<&'static str, Value> {
    let mut m = BTreeMap::new();
    let mut opt_str = |key: &'static str, v: &Option<String>| {
        if let Some(s) = v {
            m.insert(key, Value::String(s.clone()));
        }
    };
    opt_str("accepted_commit", &record.accepted_commit);
    opt_str("summary", &record.summary);
    opt_str("valid_from", &record.valid_from);
    opt_str("valid_to", &record.valid_to);
    if let Some(c) = record.confidence {
        m.insert("confidence", json_f64(c));
    }
    if let Some(i) = record.importance {
        m.insert("importance", json_f64(i));
    }
    m.insert("content_hash", Value::String(record.content_hash.clone()));
    m.insert("created_at", Value::String(record.created_at.clone()));
    m.insert("updated_at", Value::String(record.updated_at.clone()));
    if !record.events.is_empty() {
        m.insert(
            "events",
            serde_json::to_value(&record.events).unwrap_or(Value::Array(Vec::new())),
        );
    }
    m.insert(
        "governance",
        Value::String(record.governance.as_str().into()),
    );
    m.insert("id", Value::String(record.id.as_str().into()));
    m.insert("kind", Value::String(record.kind.clone()));
    m.insert("lifecycle", Value::String(record.lifecycle.as_str().into()));
    if let Some(ontology) = &record.ontology {
        m.insert("ontology", Value::String(ontology.to_string()));
    }
    if !record.path_scope.is_empty() {
        m.insert("path_scope", string_list(&record.path_scope));
    }
    if record.props.as_object().is_some_and(|o| !o.is_empty()) {
        m.insert("props", record.props.clone());
    }
    if !record.provenance.is_empty() {
        m.insert(
            "provenance",
            serde_json::to_value(&record.provenance).unwrap_or(Value::Object(Map::new())),
        );
    }
    m.insert("rev", Value::from(record.rev));
    if !record.tags.is_empty() {
        m.insert("tags", string_list(&record.tags));
    }
    m.insert("version", Value::from(FORMAT_VERSION));
    m
}

/// A finite float as a JSON number (a non-finite one has no JSON form and is dropped to null,
/// which the strict parse then refuses: the store never holds one).
fn json_f64(f: f64) -> Value {
    serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
}

fn string_list(items: &[String]) -> Value {
    Value::Array(items.iter().cloned().map(Value::String).collect())
}

fn as_object(v: Value) -> Result<Map<String, Value>, SerializeError> {
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(SerializeError::NotAMapping),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;
    use kaban_core::StoreScope;
    use serde_json::json;

    /// An accepted Decision as the store would hand it over (invented data).
    fn decision() -> Node {
        Node {
            id: "01a17a03-5cbc-7000-8000-000000000001".into(),
            kind: "Decision".into(),
            text: "Use SQLite for the project store.\n\nIt is embedded and needs no daemon.".into(),
            summary: Some("SQLite for the project store".into()),
            tags: vec!["storage".into(), "arch".into()],
            confidence: Some(0.9),
            importance: Some(0.75),
            lifecycle: Lifecycle::Active,
            governance: Governance::Accepted,
            store_scope: StoreScope::Project,
            path_scope: vec![
                "crates/kaban-store-sqlite/**".into(),
                "crates/kaban-core".into(),
            ],
            rev: 4,
            serialized_rev: Some(3),
            dup_key: Some("sqlite for the project store\u{1}crates/kaban-core".into()),
            created_at: "2026-10-26T12:40:08Z".into(),
            updated_at: "2026-10-26T12:42:47Z".into(),
            valid_from: None,
            valid_to: None,
            session_id: Some("sess-7".into()),
            agent_id: Some("claude".into()),
            workspace_id: "nightjar-service".into(),
            repo: Some("nightjar-service".into()),
            commit_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
            accepted_commit: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
            props: json!({"rationale": "embedded, zero ops", "decision": "SQLite"}),
            content_hash: "ab".repeat(32),
            policy_version: "pv-3".into(),
        }
    }

    fn accepted_event() -> Event {
        Event {
            seq: 12,
            node_id: "01a17a03-5cbc-7000-8000-000000000001".into(),
            kind: "accepted".into(),
            reason: "reviewed in PR 42".into(),
            path_scope: vec!["crates/kaban-store-sqlite/**".into()],
            signer: None,
            signer_kind: SignerKind::HumanCli,
            host: None,
            session_id: None,
            ts: 1_793_000_727,
            commit_sha: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
            workspace_id: "nightjar-service".into(),
        }
    }

    #[test]
    fn a_decision_with_props_tags_scope_and_events_round_trips_through_the_file() {
        let node = decision();
        let events = [accepted_event()];
        let text = render(&node, &events, None).unwrap();

        // the file shape: frontmatter block, body, one trailing newline
        assert!(text.starts_with("---\n"), "{text}");
        assert!(text.ends_with("needs no daemon.\n"), "{text}");
        assert!(
            !text.ends_with("\n\n"),
            "exactly one trailing newline: {text:?}"
        );
        // keys in sorted order, no workspace, no dup_key, no policy_version, no store_scope
        let fm_end = text[4..].find("\n---\n").unwrap() + 4;
        let fm = &text[4..fm_end];
        let top_keys: Vec<&str> = fm
            .lines()
            .filter(|l| !l.starts_with(' ') && !l.starts_with('-'))
            .map(|l| l.split(':').next().unwrap())
            .collect();
        let mut sorted = top_keys.clone();
        sorted.sort_unstable();
        assert_eq!(top_keys, sorted, "BTreeMap order: {fm}");
        for absent in ["workspace_id", "dup_key", "policy_version", "store_scope"] {
            assert!(
                !top_keys.contains(&absent),
                "{absent} must not be written: {fm}"
            );
        }
        assert!(fm.contains("version: 1"), "{fm}");

        // the exact bytes are a persisted contract (core-spec §7: re-importable by any 3.x)
        let expected_file = "\
---
accepted_commit: 89abcdef0123456789abcdef0123456789abcdef
confidence: 0.9
content_hash: abababababababababababababababababababababababababababababababab
created_at: 2026-10-26T12:40:08Z
events:
- commit: 89abcdef0123456789abcdef0123456789abcdef
  kind: accepted
  reason: reviewed in PR 42
  signer_kind: human-cli
  ts: 1793000727
governance: accepted
id: 01a17a03-5cbc-7000-8000-000000000001
importance: 0.75
kind: Decision
lifecycle: active
path_scope:
- crates/kaban-core
- crates/kaban-store-sqlite/**
props:
  decision: SQLite
  rationale: embedded, zero ops
provenance:
  agent_id: claude
  commit_sha: 0123456789abcdef0123456789abcdef01234567
  repo: nightjar-service
  session_id: sess-7
rev: 4
summary: SQLite for the project store
tags:
- storage
- arch
updated_at: 2026-10-26T12:42:47Z
version: 1
---
Use SQLite for the project store.

It is embedded and needs no daemon.
";
        assert_eq!(text, expected_file);
        let parsed = parse(&text).unwrap();
        let Parsed::Record(record) = parsed else {
            panic!("a record, not a tombstone: {parsed:?}");
        };
        let expected = SerializedRecord::from_node(&node, &events).unwrap();
        assert_eq!(*record, expected);
        assert_eq!(
            record.path_scope,
            vec!["crates/kaban-core", "crates/kaban-store-sqlite/**"]
        );
        assert_eq!(record.events.len(), 1);
        assert_eq!(record.events[0].reason, "reviewed in PR 42");
        assert_eq!(record.events[0].signer_kind, SignerKind::HumanCli);
        // rendering the parsed record again gives the same bytes (N5)
        assert_eq!(render_record(&record), text);
    }

    #[test]
    fn record_yaml_preserves_ambiguous_strings_aliases_and_rejects_malformed_input() {
        let mut node = decision();
        node.props = json!({
            "boolean_text": "true", "number_text": "123", "null_text": "null",
            "yaml11_text": "on", "multiline": "first\nsecond\n", "empty": ""
        });
        let expected = SerializedRecord::from_node(&node, &[]).unwrap();
        let rendered = render_record(&expected);
        let Parsed::Record(actual) = parse(&rendered).unwrap() else {
            panic!("expected a record");
        };
        assert_eq!(*actual, expected);
        assert_eq!(render_record(&actual), rendered);
        let raw = split("---\nfirst: &text on\nsecond: *text\n---\nbody\n").unwrap();
        assert_eq!(raw.frontmatter, json!({"first": "on", "second": "on"}));
        assert_eq!(raw.body, "body");
        // JSON-value maps have always kept the last duplicate; typed policy structs reject it.
        let raw = split("---\ntitle: one\ntitle: two\n---\nbody\n").unwrap();
        assert_eq!(raw.frontmatter, json!({"title": "two"}));
        for yaml in ["title: [unfinished", "title: *undefined", "title: \"\\q\""] {
            let text = format!("---\n{yaml}\n---\nbody\n");
            assert!(matches!(split(&text), Err(SerializeError::Yaml { .. })));
        }
    }

    #[test]
    fn a_purged_node_renders_the_four_key_tombstone_and_parses_back() {
        let mut node = decision();
        node.lifecycle = Lifecycle::Purged;
        node.rev = 5;
        // what a purge leaves on the row (sqlite.rs purge path); path_scope, provenance,
        // timestamps and the kind survive the purge and must not reach the file (M5-7)
        node.text = String::new();
        node.summary = None;
        node.tags.clear();
        node.props = json!({});
        node.content_hash = String::new();

        let text = render(&node, &[accepted_event()], None).unwrap();
        assert_eq!(
            text,
            "---\nid: 01a17a03-5cbc-7000-8000-000000000001\nlifecycle: purged\nrev: 5\nversion: 1\n---\n"
        );
        for leak in [
            "kaban", "nightjar", "claude", "sess-7", "2026", "accepted", "Decision",
        ] {
            assert!(
                !text.contains(leak),
                "{leak} leaked into the tombstone: {text}"
            );
        }
        assert_eq!(
            parse(&text).unwrap(),
            Parsed::Tombstone {
                id: NodeId::parse("01a17a03-5cbc-7000-8000-000000000001").unwrap(),
                rev: 5
            }
        );
        // the tombstone fits any directory (it carries no kind)
        assert!(matches!(
            parse_in_dir(&text, "Convention").unwrap(),
            Parsed::Tombstone { .. }
        ));
        assert_eq!(render_tombstone(&node.id, 5), text);
    }

    /// E1.6 (OWN-47): a file written with an ontology carries `ontology: <name>@<version>`
    /// after `lifecycle` and parses back to it; a file without the key (pre-engine) parses
    /// with `None` and hashes exactly like the stamped one (the key takes no part in the
    /// record hash); a malformed value names the key and nothing else.
    #[test]
    fn the_ontology_key_round_trips_and_takes_no_part_in_the_record_hash() {
        // Arrange
        let node = decision();
        let stamp = OntologyRef::parse("adr@1").unwrap();

        // Act
        let stamped = render(&node, &[], Some(&stamp)).unwrap();
        let plain = render(&node, &[], None).unwrap();

        // Assert
        assert!(
            stamped.contains("\nlifecycle: active\nontology: adr@1\npath_scope:"),
            "{stamped}"
        );
        assert!(!plain.contains("ontology:"), "{plain}");
        let Parsed::Record(stamped_rec) = parse(&stamped).unwrap() else {
            panic!("record");
        };
        let Parsed::Record(plain_rec) = parse(&plain).unwrap() else {
            panic!("record");
        };
        assert_eq!(stamped_rec.ontology, Some(stamp));
        assert_eq!(plain_rec.ontology, None);
        assert_eq!(record_hash(&stamped_rec), record_hash(&plain_rec));
        for bad in ["adr", "adr@", "adr@one", "adr@1@2", "a dr@1", "adr@-1"] {
            let text = stamped.replacen("ontology: adr@1\n", &format!("ontology: {bad}\n"), 1);
            assert_eq!(
                parse(&text),
                Err(SerializeError::BadValue("ontology")),
                "{bad}"
            );
        }
        assert_eq!(
            OntologyRef::parse("incidents@2").unwrap().to_string(),
            "incidents@2"
        );
        assert!(
            !SerializeError::OntologyVersionUnknown
                .to_string()
                .contains('@')
        );
    }

    #[test]
    fn the_strict_parse_refuses_unknown_keys_missing_keys_newer_versions_and_fat_tombstones() {
        let node = decision();
        let good = render(&node, &[], None).unwrap();
        let with = |needle: &str, replacement: &str| good.replacen(needle, replacement, 1);

        // an unknown key (the plan's `workspace_id`, written by nothing) is refused, unnamed
        let unknown = with("rev: 4\n", "rev: 4\nworkspace_id: nightjar-service\n");
        assert_eq!(parse(&unknown), Err(SerializeError::UnknownKey));
        assert!(!SerializeError::UnknownKey.to_string().contains("nightjar"));

        // a missing required key names kaban's key, a wrong shape too
        let no_rev = with("rev: 4\n", "");
        assert_eq!(parse(&no_rev), Err(SerializeError::MissingKey("rev")));
        let bad_rev = with("rev: 4\n", "rev: four\n");
        assert_eq!(parse(&bad_rev), Err(SerializeError::BadValue("rev")));
        let bad_id = with(
            "id: 01a17a03-5cbc-7000-8000-000000000001\n",
            "id: 01a17a03-5cbc-4000-8000-000000000001\n",
        );
        assert_eq!(parse(&bad_id), Err(SerializeError::BadValue("id")));
        let bad_lifecycle = with("lifecycle: active\n", "lifecycle: sleeping\n");
        assert_eq!(
            parse(&bad_lifecycle),
            Err(SerializeError::BadValue("lifecycle"))
        );

        // a newer format version is refused before anything else is looked at
        let newer = with("version: 1\n", "version: 2\n");
        assert_eq!(
            parse(&newer),
            Err(SerializeError::UnsupportedVersion {
                found: 2,
                supported: 1
            })
        );

        // a tombstone with a body, or with any content key, is not a tombstone
        let tomb = render_tombstone(&node.id, 5);
        assert_eq!(
            parse(&format!("{tomb}Use SQLite for the project store.\n")),
            Err(SerializeError::TombstoneShape)
        );
        let fat = tomb.replacen("rev: 5\n", "rev: 5\nsummary: still here\n", 1);
        assert_eq!(parse(&fat), Err(SerializeError::TombstoneShape));
        // a record that says `purged` is a malformed tombstone, never a record
        let purged_record = with("lifecycle: active\n", "lifecycle: purged\n");
        assert_eq!(parse(&purged_record), Err(SerializeError::TombstoneShape));

        // the directory decides the kind
        assert_eq!(
            parse_in_dir(&good, "Convention"),
            Err(SerializeError::KindMismatch)
        );
        assert!(parse_in_dir(&good, "Decision").is_ok());

        // a stored row whose id is not a UUIDv7 has no file view (never a fresh id in its place)
        let mut odd = node.clone();
        odd.id = "not-an-id".into();
        assert_eq!(render(&odd, &[], None), Err(SerializeError::BadValue("id")));

        // not a record file at all
        assert_eq!(parse("# notes\n"), Err(SerializeError::NoFrontmatter));
        assert_eq!(parse("---\nid: x\n"), Err(SerializeError::NoFrontmatter));
        assert_eq!(
            parse("---\n- a\n- b\n---\n"),
            Err(SerializeError::NotAMapping)
        );
        assert!(matches!(
            parse("---\nid: [\n---\n"),
            Err(SerializeError::Yaml { .. })
        ));
    }

    #[test]
    fn the_record_hash_ignores_rev_provenance_events_and_timestamps_but_sees_a_props_edit() {
        let node = decision();
        let base = SerializedRecord::from_node(&node, &[accepted_event()]).unwrap();
        let hash = record_hash(&base);
        assert_eq!(hash.len(), 64, "sha256 hex: {hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{hash}");

        // the same content on another clone: higher rev, stamped, re-timestamped, no events
        let mut other = SerializedRecord::from_node(&node, &[]).unwrap();
        other.rev = 9;
        other.provenance = RecordProvenance::default();
        other.created_at = "2026-10-26T07:24:56Z".into();
        other.updated_at = "2026-10-26T08:07:26Z".into();
        other.accepted_commit = None;
        other.content_hash = "cd".repeat(32);
        assert_eq!(
            record_hash(&other),
            hash,
            "bookkeeping fields take no part (M5-5)"
        );

        // a props-only edit (the rationale) at equal rev is a different record
        let mut edited = base.clone();
        edited.props = json!({"rationale": "embedded, zero ops, and fast", "decision": "SQLite"});
        assert_ne!(record_hash(&edited), hash);
        // and so is every other content field
        for (name, mutate) in [
            (
                "text",
                Box::new(|r: &mut SerializedRecord| r.text.push('!'))
                    as Box<dyn Fn(&mut SerializedRecord)>,
            ),
            ("summary", Box::new(|r| r.summary = None)),
            ("tags", Box::new(|r| r.tags.push("x".into()))),
            (
                "path_scope",
                Box::new(|r| r.path_scope.pop().map_or((), drop)),
            ),
            ("importance", Box::new(|r| r.importance = Some(0.5))),
            ("confidence", Box::new(|r| r.confidence = None)),
            (
                "valid_to",
                Box::new(|r| r.valid_to = Some("2026-10-26T12:18:53Z".into())),
            ),
            (
                "governance",
                Box::new(|r| r.governance = Governance::Proposed),
            ),
            ("lifecycle", Box::new(|r| r.lifecycle = Lifecycle::Archived)),
            ("kind", Box::new(|r| r.kind = "Convention".into())),
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert_ne!(record_hash(&changed), hash, "{name} is part of the hash");
        }
        // two renders of the same record hash the same through parse as well
        let reparsed = match parse(&render_record(&base)).unwrap() {
            Parsed::Record(r) => r,
            Parsed::Tombstone { .. } => panic!("a record"),
        };
        assert_eq!(record_hash(&reparsed), hash);
    }
}
