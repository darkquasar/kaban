//! Immutable packs (spec 05 SS-18, SS-19): one coherent snapshot of a database, the admitted
//! code-graph snapshots the caller hands in, and a manifest binding them; the read-only open
//! of such a pack; and the mode-A fork of a pack into a writable copy with its own identity
//! (SS-20, [`fork`]). The text index is FTS5 inside the snapshot below the FTS5 retirement
//! (SS-16; a retired source has none, so its pack must carry the component); an active
//! Tantivy deployment adds the external index component ([`TextIndexComponent`], handed in
//! by the binary): the
//! builder holds the projector lease and the writer lock, drains projection to the sequence it
//! snapshots, and captures the index directory under `index/tantivy/` with its payload in the
//! manifest; a fork receives the copy beside its database and needs no rebind or reindex, the
//! payload binding to the lineage (design amendment 2).
//!
//! ```text
//!   build(req) => BEGIN IMMEDIATE on the source (other canonical writers excluded; busy when
//!                 the lock cannot be taken) => identities, versions, seq, audit head
//!              => VACUUM INTO <staging>/kaban.db (second connection; owner-only, O_EXCL)
//!              => ROLLBACK (the lock is only ever held for the capture)
//!              => leases cleared in the staged copy, rollback journal mode
//!              => snapshots copied, every file hashed, manifest.json written, files 0400
//!              => verify(<staging>) => rename(<staging>, <dest>)
//!   verify(dir, admit) => manifest (supported format and versions) => every declared file
//!                  present, a regular file, its size and sha256 as declared, no WAL header;
//!                  nothing undeclared
//!               => every declared code-graph snapshot admitted by the caller's `admit` and
//!                  its `meta.commit` the declared one
//!               => a declared index directory admitted by the adapter (opens as an index of
//!                  this build, every referenced segment present, versions implemented) and
//!                  carrying the declared payload
//!               => the snapshot opened `mode=ro&immutable=1`: PRAGMA integrity_check,
//!                  identities, schema, codec, sequence and audit head as declared, no lease
//!                  rows, the recorded dictionary the shipped one and every encoded body
//!                  decoded whole (spec 05 SS-17: a file hash cannot see a garbled frame)
//! ```
//!
//! A pack directory is `manifest.json`, `kaban.db` and `snapshots/<commit>.cg.db` files.
//! Staging happens beside the destination in an owner-only directory, so a failure at any
//! step (busy, disk, kill) leaves at most a `.<name>.staging-<pid>` directory and never a
//! directory under the destination name. Nothing here repairs, migrates or writes a pack:
//! verification opens the snapshot immutable, and so does [`SqliteStore::open_pack`]. The
//! code-graph snapshots are opened by the caller's [`SnapshotAdmission`] (this crate owns no
//! code-graph SQL, cg-join-v1 §2 SO-3) and the index directory by the adapter's
//! [`IndexAdmission`] (this crate owns no Tantivy code), both handed in as one [`Admission`];
//! the one admission path is [`verify`], which the staged build, `pack verify`,
//! [`SqliteStore::open_pack`] and [`fork`] all run.

use crate::schema::{LATEST_SCHEMA_VERSION, MigrationReport, Migrator, is_busy};
use crate::sql_error::SqlResultExt;
use crate::sqlite::SqliteStore;
use kaban_core::{ActorId, StoreError, new_id, now_iso};
use kaban_policy::{KeySigner, sha256_hex};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The manifest format this binary writes and reads.
pub const FORMAT: &str = "kaban-pack/1";
/// The snapshot's file name inside a pack.
pub const DB_FILE: &str = "kaban.db";
/// The manifest's file name inside a pack.
pub const MANIFEST_FILE: &str = "manifest.json";
/// The directory of the code-graph snapshots inside a pack.
pub const SNAPSHOT_DIR: &str = "snapshots";
/// The text index of a pack with the captured Tantivy directory.
pub const TEXT_INDEX_TANTIVY: &str = "tantivy";
/// The captured index directory inside a pack.
pub const INDEX_DIR: &str = "index/tantivy";
/// The FTS5 tokenizer of `node_fts` (migrations 0001 and 0005).
const ANALYZER: &str = "porter unicode61";
/// How bodies are stored in a database below the body codec migration (plain UTF-8); at or
/// past it, `index_meta.body_codec` names the codec (`body_codec::CODEC_ID`, spec 05 SS-17).
const CODEC: &str = "utf8";
/// The read cap of a manifest.
const MANIFEST_CAP: u64 = 1 << 20;

/// Why a pack could not be built, verified or opened. Messages name pack-relative files and
/// manifest fields only, never content.
#[derive(Debug)]
pub enum PackError {
    /// Another canonical writer holds the source: the maintenance window could not be
    /// obtained within the busy timeout.
    Busy,
    /// A base commit that is not 40 lowercase hex digits.
    InvalidCommit,
    /// The destination (or its staging sibling) already exists, or has no parent or name.
    Destination,
    /// Filesystem failure at a pack-relative file (or the staging directory).
    Io {
        /// The pack-relative file, or `staging`.
        file: String,
        /// The failure.
        source: std::io::Error,
    },
    /// The source or the snapshot failed in the driver.
    Store(StoreError),
    /// The manifest is absent, over the cap, not JSON or not of this shape.
    Manifest(&'static str),
    /// A manifest field names a format or version this binary does not support.
    Unsupported {
        /// `format`, `text_index`, `versions.schema`, `versions.analyzer` or `versions.codec`.
        field: &'static str,
        /// The declared value.
        found: String,
    },
    /// A declared component is not in the pack.
    Missing(String),
    /// A file in the pack that the manifest does not declare (a substituted component).
    Undeclared(String),
    /// A declared file's bytes are not the ones the manifest hashes.
    HashMismatch(String),
    /// A component whose SQLite header declares WAL mode: reading it through a non-immutable
    /// connection would create `-wal`/`-shm` sidecars inside the pack (SS-19), so it is
    /// refused at build and before any open.
    WalMode(String),
    /// A declared code-graph snapshot the caller's admission refused (the class, never
    /// content).
    Snapshot {
        /// The pack-relative file.
        file: String,
        /// The admission's failure class.
        class: String,
    },
    /// The declared index directory the adapter's admission refused: not an index this build
    /// reads, a referenced segment file missing, versions it does not implement (the class,
    /// never content).
    Index {
        /// The admission's failure class.
        class: String,
    },
    /// The source retired FTS5 (spec 05 SS-16), so its text index is the Tantivy component,
    /// and the build brought none: a binary without the adapter cannot pack it (T6 review
    /// t6-1).
    IndexRequired {
        /// The source's schema version.
        schema: u32,
    },
    /// `PRAGMA integrity_check` on the snapshot did not return `ok`.
    Integrity,
    /// A body in the snapshot this binary cannot decode (spec 05 SS-17): an unknown format
    /// or dictionary id, or a frame that does not decode whole. The codec's failure, behind
    /// the store's redacted renderers; never bytes.
    Body(StoreError),
    /// A manifest field disagrees with the snapshot or with another field.
    Inconsistent(&'static str),
    /// The declared base commit and an included base snapshot disagree.
    BaseMismatch,
    /// A pack below the current schema is opened read-only: a newer reader may migrate a
    /// writable copy (`pack fork`) only.
    SchemaBehind {
        /// The pack's schema version.
        found: u32,
        /// This binary's schema version.
        supported: u32,
    },
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("the database is busy: another writer holds it; retry"),
            Self::InvalidCommit => f.write_str("the base commit must be a full 40-hex commit id"),
            Self::Destination => {
                f.write_str("the destination exists or cannot be staged beside its parent")
            }
            Self::Io { file, source } => write!(f, "i/o on {file}: {source}"),
            Self::Store(e) => write!(f, "{e}"),
            Self::Manifest(why) => write!(f, "manifest.json is malformed: {why}"),
            Self::Unsupported { field, found } => write!(f, "unsupported {field}: {found}"),
            Self::Missing(file) => write!(f, "declared component missing: {file}"),
            Self::Undeclared(file) => write!(f, "undeclared file in the pack: {file}"),
            Self::HashMismatch(file) => write!(f, "hash mismatch: {file}"),
            Self::WalMode(file) => write!(
                f,
                "component in WAL mode (a read would create sidecars in the pack): {file}"
            ),
            Self::Snapshot { file, class } => write!(f, "snapshot not admitted: {file} ({class})"),
            Self::Index { class } => write!(f, "index component not admitted: {class}"),
            Self::IndexRequired { schema } => write!(
                f,
                "the database is at schema version {schema}, where the text index is the Tantivy component: build the pack with a binary that reads it"
            ),
            Self::Integrity => f.write_str("snapshot integrity check failed"),
            Self::Body(e) => write!(f, "a snapshot body does not decode: {e}"),
            Self::Inconsistent(field) => {
                write!(f, "manifest field inconsistent with the pack: {field}")
            }
            Self::BaseMismatch => {
                f.write_str("declared base commit disagrees with an included base snapshot")
            }
            Self::SchemaBehind { found, supported } => write!(
                f,
                "the pack is at schema version {found}; this binary reads {supported} (fork it to migrate)"
            ),
        }
    }
}

impl std::error::Error for PackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Store(e) | Self::Body(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for PackError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

type Result<T> = std::result::Result<T, PackError>;

/// What `pack build` records about one file: its size and SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDigest {
    /// Lowercase hex SHA-256 of the bytes.
    pub sha256: String,
    /// The byte length.
    pub bytes: u64,
}

/// The component versions a pack was written with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versions {
    /// `index_meta.schema_version`.
    pub schema: u32,
    /// The effective policy hash of the building process.
    pub policy: String,
    /// The ontology-bound identifier terms version the snapshot was projected under
    /// (`index_meta.ident_terms_version`; empty when never deployed).
    pub ontology: String,
    /// The ontology-dependent FTS projection the snapshot's text index was built by
    /// (`index_meta.fts_projection_signature`; empty when never deployed). Both are read
    /// from the captured database and checked against the pack's snapshot (review e-7).
    pub projection: String,
    /// The FTS5 tokenizer.
    pub analyzer: String,
    /// The body codec.
    pub codec: String,
}

/// An included code-graph snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// The snapshot's commit (its file name and `meta.commit`).
    pub commit: String,
    /// `snapshots/<commit>.cg.db`.
    pub file: String,
}

/// The audit chain's position at the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditHead {
    /// Rows in the chain.
    pub entries: u64,
    /// The last row's `entry_hmac`, `genesis` for an empty chain.
    pub head: String,
}

/// `manifest.json`: everything a reader needs to trust and place the pack. Record references
/// are UUIDs; `canonical_seq` is a local change-log position of `instance_id`, not an identity
/// (SS-27). Rendered and parsed by [`Manifest::to_value`] and [`Manifest::from_value`] (the
/// store crate carries no serde derive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// [`FORMAT`].
    pub format: String,
    /// This pack's UUID.
    pub pack_id: String,
    /// The pack the source database was forked from, when it was (`index_meta.parent_pack_id`).
    pub parent_pack_id: Option<String>,
    /// The source database's `db_id`.
    pub instance_id: String,
    /// The source database's `pack_lineage_id`.
    pub pack_lineage_id: String,
    /// The resolved full base commit.
    pub base_commit: String,
    /// The repository head when it was resolvable (informational).
    pub source_commit: Option<String>,
    /// Component versions.
    pub versions: Versions,
    /// `fts5` (the text index lives inside the snapshot) or `tantivy` (the captured directory,
    /// [`Self::index`]).
    pub text_index: String,
    /// The external text-index component, `tantivy` only.
    pub index: Option<IndexComponent>,
    /// `max(changes.seq)` at the snapshot (0 for an empty log).
    pub canonical_seq: u64,
    /// The audit chain at the snapshot.
    pub audit: AuditHead,
    /// RFC 3339 build time.
    pub created_at: String,
    /// Every file of the pack but the manifest, by pack-relative path.
    pub files: BTreeMap<String, FileDigest>,
    /// The included code-graph snapshots (each also in `files`).
    pub snapshots: Vec<SnapshotEntry>,
}

/// The external text-index component of a `tantivy` pack: the directory [`INDEX_DIR`] and the
/// payload of its last commit (`{seq, pack_lineage_id, versions, ..}` as the adapter writes
/// it), which must agree with the manifest's sequence, lineage and projection and with the
/// directory's own `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexComponent {
    /// The payload JSON.
    pub payload: Value,
}

/// A string field of a manifest object.
fn field_str(obj: &Map<String, Value>, key: &'static str) -> Result<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(PackError::Manifest(key))
}

/// An optional string field (absent or `null` is `None`).
fn field_opt(obj: &Map<String, Value>, key: &'static str) -> Result<Option<String>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(PackError::Manifest(key)),
    }
}

fn field_u64(obj: &Map<String, Value>, key: &'static str) -> Result<u64> {
    obj.get(key)
        .and_then(Value::as_u64)
        .ok_or(PackError::Manifest(key))
}

fn field_obj<'a>(obj: &'a Map<String, Value>, key: &'static str) -> Result<&'a Map<String, Value>> {
    obj.get(key)
        .and_then(Value::as_object)
        .ok_or(PackError::Manifest(key))
}

impl Manifest {
    /// The JSON document of `manifest.json`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let files: Map<String, Value> = self
            .files
            .iter()
            .map(|(file, d)| {
                (
                    file.clone(),
                    json!({ "sha256": d.sha256, "bytes": d.bytes }),
                )
            })
            .collect();
        let snapshots: Vec<Value> = self
            .snapshots
            .iter()
            .map(|s| json!({ "commit": s.commit, "file": s.file }))
            .collect();
        json!({
            "format": self.format,
            "pack_id": self.pack_id,
            "parent_pack_id": self.parent_pack_id,
            "instance_id": self.instance_id,
            "pack_lineage_id": self.pack_lineage_id,
            "base_commit": self.base_commit,
            "source_commit": self.source_commit,
            "versions": {
                "schema": self.versions.schema,
                "policy": self.versions.policy,
                "ontology": self.versions.ontology,
                "projection": self.versions.projection,
                "analyzer": self.versions.analyzer,
                "codec": self.versions.codec,
            },
            "text_index": self.text_index,
            "index": self.index.as_ref().map(|i| json!({ "dir": INDEX_DIR, "payload": i.payload })),
            "canonical_seq": self.canonical_seq,
            "audit": { "entries": self.audit.entries, "head": self.audit.head },
            "created_at": self.created_at,
            "files": files,
            "snapshots": snapshots,
        })
    }

    /// Parse a `manifest.json` document; a missing or mistyped field names itself.
    ///
    /// # Errors
    /// `PackError::Manifest` naming the field.
    pub fn from_value(v: &Value) -> Result<Self> {
        let obj = v.as_object().ok_or(PackError::Manifest("object"))?;
        let versions = field_obj(obj, "versions")?;
        let audit = field_obj(obj, "audit")?;
        let mut files = BTreeMap::new();
        for (file, digest) in field_obj(obj, "files")? {
            let digest = digest.as_object().ok_or(PackError::Manifest("files"))?;
            files.insert(
                file.clone(),
                FileDigest {
                    sha256: field_str(digest, "sha256")?,
                    bytes: field_u64(digest, "bytes")?,
                },
            );
        }
        let mut snapshots = Vec::new();
        for entry in obj
            .get("snapshots")
            .and_then(Value::as_array)
            .ok_or(PackError::Manifest("snapshots"))?
        {
            let entry = entry.as_object().ok_or(PackError::Manifest("snapshots"))?;
            snapshots.push(SnapshotEntry {
                commit: field_str(entry, "commit")?,
                file: field_str(entry, "file")?,
            });
        }
        Ok(Self {
            format: field_str(obj, "format")?,
            pack_id: field_str(obj, "pack_id")?,
            parent_pack_id: field_opt(obj, "parent_pack_id")?,
            instance_id: field_str(obj, "instance_id")?,
            pack_lineage_id: field_str(obj, "pack_lineage_id")?,
            base_commit: field_str(obj, "base_commit")?,
            source_commit: field_opt(obj, "source_commit")?,
            versions: Versions {
                schema: u32::try_from(field_u64(versions, "schema")?)
                    .map_err(|_| PackError::Manifest("versions.schema"))?,
                policy: field_str(versions, "policy")?,
                ontology: field_str(versions, "ontology")?,
                projection: field_str(versions, "projection")?,
                analyzer: field_str(versions, "analyzer")?,
                codec: field_str(versions, "codec")?,
            },
            text_index: field_str(obj, "text_index")?,
            index: match obj.get("index") {
                None | Some(Value::Null) => None,
                Some(Value::Object(index)) => Some(IndexComponent {
                    payload: index
                        .get("payload")
                        .cloned()
                        .ok_or(PackError::Manifest("index.payload"))?,
                }),
                Some(_) => return Err(PackError::Manifest("index")),
            },
            canonical_seq: field_u64(obj, "canonical_seq")?,
            audit: AuditHead {
                entries: field_u64(audit, "entries")?,
                head: field_str(audit, "head")?,
            },
            created_at: field_str(obj, "created_at")?,
            files,
            snapshots,
        })
    }
}

/// How a code-graph snapshot is admitted: the caller's admission (`kaban-codegraph`'s, through
/// the binary) opens `path` read-only and returns its `meta.commit`, or the failure class (never
/// content). The store owns no code-graph SQL (cg-join-v1 §2 SO-3), so [`verify`] runs the
/// admission it is handed on every declared snapshot and compares the commit itself.
pub type SnapshotAdmission = fn(&Path) -> std::result::Result<String, String>;

/// How the external text-index component is admitted (SS-19): the adapter's own read-only
/// verifier (through the binary) opens `path` (`<pack>/index/tantivy`) as an index of its
/// build, checks that every segment the commit references is present and that the payload's
/// layout and versions are ones it implements, and returns that payload, or the failure class
/// (never content). The store owns no Tantivy code, so [`verify`] runs the admission it is
/// handed and compares the payload with the manifest's itself (T4 review t4-5). A binary
/// built without the adapter admits no Tantivy component.
pub type IndexAdmission = fn(&Path) -> std::result::Result<Value, String>;

/// The caller's admissions a pack is verified with: one per component this crate cannot open
/// itself.
#[derive(Debug, Clone, Copy)]
pub struct Admission {
    /// Each declared code-graph snapshot.
    pub snapshot: SnapshotAdmission,
    /// The declared index directory.
    pub index: IndexAdmission,
}

/// One admitted code-graph snapshot to include: the caller admitted it and read its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSource {
    /// The snapshot's commit, equal to the base commit (the base snapshot).
    pub commit: String,
    /// The admitted file.
    pub path: PathBuf,
}

/// The external text-index component of an active Tantivy deployment (SS-18), as the binary
/// hands it in over the adapter's projector: the store crate owns the maintenance window, the
/// files and the manifest, the adapter owns lease, lock and projection.
pub trait TextIndexComponent {
    /// Run `body` while this component's ownership is held (the projector lease and the
    /// writer lock, both taken non-blocking: `Ok(None)` when either is held elsewhere, which
    /// the builder reports busy), releasing both afterwards.
    ///
    /// # Errors
    /// A store or index failure, or `body`'s own.
    fn with_held(
        &self,
        body: &mut dyn FnMut(&mut dyn HeldTextIndex) -> Result<Manifest>,
    ) -> Result<Option<Manifest>>;
}

/// The held component inside [`TextIndexComponent::with_held`].
pub trait HeldTextIndex {
    /// Publish every committed change to the head of the log (the builder's own
    /// acknowledgement) through `target`, the head the builder read under its lock, stopping
    /// at `deadline`, and return the published sequence; `None` when the run stopped short
    /// (the lease was lost, the deadline passed), which the builder reports busy. Both bounds
    /// are the builder's: a writer that keeps the log moving cannot keep the drain from
    /// returning (T4 review t4-8).
    ///
    /// # Errors
    /// A store or index failure.
    fn drain(
        &mut self,
        target: u64,
        deadline: Instant,
    ) -> std::result::Result<Option<u64>, StoreError>;

    /// The directory to capture and the payload of its last commit; `None` when the index
    /// has no directory or no commit (nothing to capture).
    ///
    /// # Errors
    /// The payload could not be read.
    fn captured(&self) -> std::result::Result<Option<(PathBuf, Value)>, StoreError>;
}

/// What [`build`] needs.
pub struct BuildRequest<'a> {
    /// The source database (opened as it is; never created or migrated).
    pub db: &'a Path,
    /// The pack directory to create; must not exist.
    pub dest: &'a Path,
    /// The resolved full base commit.
    pub base_commit: &'a str,
    /// The repository head when resolvable.
    pub source_commit: Option<String>,
    /// The building process's effective policy hash.
    pub policy_version: String,
    /// The admitted snapshots to include.
    pub snapshots: &'a [SnapshotSource],
    /// The admissions the staged pack is verified with.
    pub admit: Admission,
    /// The external text-index component of an active Tantivy deployment, `None` for FTS5.
    pub index: Option<&'a dyn TextIndexComponent>,
}

impl std::fmt::Debug for BuildRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildRequest")
            .field("db", &self.db)
            .field("dest", &self.dest)
            .field("base_commit", &self.base_commit)
            .field("source_commit", &self.source_commit)
            .field("snapshots", &self.snapshots)
            .field("index", &self.index.is_some())
            .finish_non_exhaustive()
    }
}

/// Whether `s` is a full lowercase 40-hex commit id.
#[must_use]
pub fn is_full_commit(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Build the pack at `req.dest`.
///
/// # Errors
/// [`PackError`]: `Busy` when another writer holds the source, `InvalidCommit`,
/// `Destination`, `BaseMismatch` when an included snapshot is not the base's, `SchemaBehind`
/// when the source is below the current schema, driver and i/o failures, and every
/// verification failure of the staged pack (then nothing is finalized).
pub fn build(req: &BuildRequest<'_>) -> Result<Manifest> {
    if !is_full_commit(req.base_commit) {
        return Err(PackError::InvalidCommit);
    }
    if req.snapshots.iter().any(|s| s.commit != req.base_commit) {
        return Err(PackError::BaseMismatch);
    }
    let (Some(parent), Some(name)) = (req.dest.parent(), req.dest.file_name()) else {
        return Err(PackError::Destination);
    };
    if req.dest.symlink_metadata().is_ok() {
        return Err(PackError::Destination);
    }
    let mut staging_name = std::ffi::OsString::from(".");
    staging_name.push(name);
    staging_name.push(format!(".staging-{}", std::process::id()));
    let staging = if parent.as_os_str().is_empty() {
        PathBuf::from(staging_name)
    } else {
        parent.join(staging_name)
    };
    create_private_dir(&staging).map_err(staging_io)?;
    let staged = stage(req, &staging).and_then(|manifest| {
        verify(&staging, req.admit)?;
        std::fs::rename(&staging, req.dest).map_err(staging_io)?;
        Ok(manifest)
    });
    if staged.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    staged
}

fn staging_io(source: std::io::Error) -> PackError {
    PackError::Io {
        file: "staging".into(),
        source,
    }
}

fn file_io(file: &str, source: std::io::Error) -> PackError {
    PackError::Io {
        file: file.to_owned(),
        source,
    }
}

/// Create `dir` with mode exactly 0700 (the builder's mode is filtered by the umask).
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// What the capture reads from the source under the write lock.
struct Captured {
    instance_id: String,
    pack_lineage_id: String,
    parent_pack_id: Option<String>,
    schema: u32,
    ontology: String,
    projection: String,
    codec: String,
    canonical_seq: u64,
    audit: AuditHead,
}

/// The `index_meta` key of [`Versions::ontology`].
const ONTOLOGY_KEY: &str = "ident_terms_version";
/// The `index_meta` key of [`Versions::projection`].
const PROJECTION_KEY: &str = "fts_projection_signature";

fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .sql()?)
}

fn capture(conn: &Connection) -> Result<Captured> {
    let identity = |key: &'static str| -> Result<String> {
        meta(conn, key)?.ok_or(PackError::Inconsistent(key))
    };
    let schema = Migrator::check(conn).map_err(PackError::Store)?;
    if !Migrator::is_current(schema) {
        return Err(PackError::SchemaBehind {
            found: schema,
            supported: LATEST_SCHEMA_VERSION,
        });
    }
    Ok(Captured {
        instance_id: identity("db_id")?,
        pack_lineage_id: identity("pack_lineage_id")?,
        parent_pack_id: meta(conn, "parent_pack_id")?,
        schema,
        ontology: meta(conn, ONTOLOGY_KEY)?.unwrap_or_default(),
        projection: meta(conn, PROJECTION_KEY)?.unwrap_or_default(),
        codec: meta(conn, crate::body_codec::CODEC_KEY)?.unwrap_or_else(|| CODEC.to_owned()),
        canonical_seq: canonical_seq(conn)?,
        audit: audit_head(conn)?,
    })
}

fn canonical_seq(conn: &Connection) -> Result<u64> {
    let seq: Option<i64> = conn
        .query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))
        .sql()?;
    Ok(seq.and_then(|s| u64::try_from(s).ok()).unwrap_or(0))
}

fn audit_head(conn: &Connection) -> Result<AuditHead> {
    let entries: i64 = conn
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .sql()?;
    let head: Option<String> = conn
        .query_row(
            "SELECT entry_hmac FROM audit ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    Ok(AuditHead {
        entries: u64::try_from(entries).unwrap_or(0),
        head: head.unwrap_or_else(|| crate::audit_table::GENESIS.to_owned()),
    })
}

/// Everything between the staging directory existing and its verification; with a text-index
/// component, under its held ownership (busy when that cannot be taken).
fn stage(req: &BuildRequest<'_>, staging: &Path) -> Result<Manifest> {
    match req.index {
        Some(component) => {
            match component.with_held(&mut |held| stage_held(req, staging, Some(held))) {
                Ok(Some(manifest)) => Ok(manifest),
                Ok(None) => Err(PackError::Busy),
                // the component's own lease is a write on the source: a canonical writer holding
                // the database past the busy timeout is the busy source, not a backend failure
                Err(PackError::Store(e)) if store_busy(&e) => Err(PackError::Busy),
                Err(e) => Err(e),
            }
        }
        None => stage_held(req, staging, None),
    }
}

/// Whether a store failure is SQLite's busy (another writer holds the database).
fn store_busy(e: &StoreError) -> bool {
    match e {
        StoreError::Backend { source } => source
            .driver()
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(is_busy),
        _ => false,
    }
}

/// How often the builder drains the index and finds the log moved on before it took the
/// lock, before it reports busy.
const QUIESCE_TRIES: usize = 3;
/// How long the builder's drains may take in all (a rebuild of a payload that cannot be
/// resumed from included) before the build is busy: the window no writer can stretch (T4
/// review t4-8).
const QUIESCE_WINDOW: Duration = Duration::from_secs(60);

/// The maintenance window: one write lock excludes every other canonical writer and reads
/// the head; the index component (when one is held) is drained through that head outside the
/// lock, since its acknowledgement is a write of its own, bounded by that target and by
/// [`QUIESCE_WINDOW`] (a drain that followed a moving head unbounded never reached the lock,
/// T4 review t4-8); the lock is taken again and the capture stands only when the sequence the
/// index published is the one under it; a writer that slipped in between is drained over
/// again, up to [`QUIESCE_TRIES`], then the build is busy. `VACUUM INTO` reads on a second
/// connection (WAL readers are not blocked).
fn quiesce(source: &Connection, held: Option<&mut (dyn HeldTextIndex + '_)>) -> Result<Captured> {
    let mut captured = lock_and_capture(source)?;
    let Some(held) = held else {
        return Ok(captured);
    };
    let deadline = Instant::now() + QUIESCE_WINDOW;
    for _ in 0..QUIESCE_TRIES {
        source.execute_batch("ROLLBACK").sql()?;
        let published = held
            .drain(captured.canonical_seq, deadline)?
            .ok_or(PackError::Busy)?;
        captured = lock_and_capture(source)?;
        if published == captured.canonical_seq {
            return Ok(captured);
        }
    }
    source.execute_batch("ROLLBACK").sql()?;
    Err(PackError::Busy)
}

/// `BEGIN IMMEDIATE` on `source` (busy when another writer holds it past the busy timeout)
/// and what the source says under it.
fn lock_and_capture(source: &Connection) -> Result<Captured> {
    match source.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => {}
        Err(e) if is_busy(&e) => return Err(PackError::Busy),
        Err(e) => return Err(crate::sql_error::backend_error(e).into()),
    }
    capture(source)
}

fn stage_held(
    req: &BuildRequest<'_>,
    staging: &Path,
    mut held: Option<&mut (dyn HeldTextIndex + '_)>,
) -> Result<Manifest> {
    let (source, _live) =
        crate::connection::open_existing_as_is(req.db, crate::SqliteStore::CAPABILITIES)?;
    source.busy_timeout(crate::schema::BUSY_TIMEOUT).sql()?;
    // a source whose bodies this binary cannot decode is not packed (SS-17, T7 review t7-4)
    crate::body_codec::verify(&source)?;
    let captured = quiesce(&source, held.as_deref_mut())?;
    // the index is the store's own (OWN-M): without the Tantivy component the pack would
    // certify an unsearchable snapshot (T6 review t6-1)
    if held.is_none() {
        return Err(PackError::IndexRequired {
            schema: captured.schema,
        });
    }
    let db_path = staging.join(DB_FILE);
    crate::connection::create_owner_only(&db_path).map_err(|e| file_io(DB_FILE, e))?;
    let target = db_path
        .to_str()
        .ok_or(PackError::Manifest("staging path is not UTF-8"))?;
    let reader = Connection::open_with_flags(
        req.db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .sql()?;
    reader.execute("VACUUM INTO ?1", [target]).sql()?;
    drop(reader);
    source.execute_batch("ROLLBACK").sql()?;
    drop(source);

    // the staged copy: no reusable live lease, a single file (rollback journal mode)
    let staged = Connection::open(&db_path).sql()?;
    staged.execute("DELETE FROM leases", []).sql()?;
    staged
        .query_row("PRAGMA journal_mode = DELETE", [], |r| {
            r.get::<_, String>(0)
        })
        .sql()?;
    drop(staged);

    let mut files = BTreeMap::new();
    files.insert(DB_FILE.to_owned(), digest_and_seal(&db_path, DB_FILE)?);
    let mut snapshots = Vec::with_capacity(req.snapshots.len());
    if !req.snapshots.is_empty() {
        create_private_dir(&staging.join(SNAPSHOT_DIR)).map_err(|e| file_io(SNAPSHOT_DIR, e))?;
    }
    for snapshot in req.snapshots {
        let file = snapshot_file(&snapshot.commit);
        let path = staging.join(&file);
        std::fs::copy(&snapshot.path, &path).map_err(|e| file_io(&file, e))?;
        files.insert(file.clone(), digest_and_seal(&path, &file)?);
        snapshots.push(SnapshotEntry {
            commit: snapshot.commit.clone(),
            file,
        });
    }
    // the index component: the directory as the held writer leaves it (nothing publishes
    // while it is held), lock files aside, under `index/tantivy/`
    let mut index = None;
    if let Some(held) = &held {
        let (dir, payload) = held.captured()?.ok_or(PackError::Inconsistent("index"))?;
        create_private_dir(&staging.join("index")).map_err(|e| file_io("index", e))?;
        create_private_dir(&staging.join(INDEX_DIR)).map_err(|e| file_io(INDEX_DIR, e))?;
        for name in index_files(&dir)? {
            let file = format!("{INDEX_DIR}/{name}");
            let path = staging.join(&file);
            std::fs::copy(dir.join(&name), &path).map_err(|e| file_io(&file, e))?;
            files.insert(file.clone(), digest_and_seal_raw(&path, &file)?);
        }
        index = Some(IndexComponent { payload });
    }
    let manifest = Manifest {
        format: FORMAT.into(),
        pack_id: new_id(),
        parent_pack_id: captured.parent_pack_id,
        instance_id: captured.instance_id,
        pack_lineage_id: captured.pack_lineage_id,
        base_commit: req.base_commit.to_owned(),
        source_commit: req.source_commit.clone(),
        versions: Versions {
            schema: captured.schema,
            policy: req.policy_version.clone(),
            ontology: captured.ontology,
            projection: captured.projection,
            analyzer: ANALYZER.into(),
            codec: captured.codec,
        },
        text_index: TEXT_INDEX_TANTIVY.into(),
        index,
        canonical_seq: captured.canonical_seq,
        audit: captured.audit,
        created_at: now_iso(),
        files,
        snapshots,
    };
    let manifest_path = staging.join(MANIFEST_FILE);
    let mut bytes = serde_json::to_vec_pretty(&manifest.to_value()).map_err(StoreError::from)?;
    bytes.push(b'\n');
    std::fs::write(&manifest_path, bytes).map_err(|e| file_io(MANIFEST_FILE, e))?;
    seal(&manifest_path, MANIFEST_FILE)?;
    Ok(manifest)
}

fn snapshot_file(commit: &str) -> String {
    format!("{SNAPSHOT_DIR}/{commit}.cg.db")
}

/// The regular files of an index directory a pack captures, by name: everything the adapter
/// wrote (`meta.json`, the managed list, the segment files) except lock files, which belong
/// to the processes holding them.
fn index_files(dir: &Path) -> Result<Vec<String>> {
    let entries = std::fs::read_dir(dir).map_err(|e| file_io(INDEX_DIR, e))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| file_io(INDEX_DIR, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry.file_type().map_err(|e| file_io(&name, e))?;
        let lock_file = Path::new(&name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"));
        if kind.is_file() && !lock_file {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Hash `path` and make it read-only (0400): the pack is immutable from here on. A WAL-mode
/// component is refused here, so no pack this binary builds holds one.
fn digest_and_seal(path: &Path, file: &str) -> Result<FileDigest> {
    let bytes = std::fs::read(path).map_err(|e| file_io(file, e))?;
    refuse_wal(file, &bytes)?;
    seal(path, file)?;
    Ok(FileDigest {
        sha256: sha256_hex(&bytes),
        bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    })
}

/// [`digest_and_seal`] for an index file, which is not a SQLite database (its bytes say
/// nothing about WAL mode).
fn digest_and_seal_raw(path: &Path, file: &str) -> Result<FileDigest> {
    let bytes = std::fs::read(path).map_err(|e| file_io(file, e))?;
    seal(path, file)?;
    Ok(FileDigest {
        sha256: sha256_hex(&bytes),
        bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    })
}

/// Every pack component is a SQLite file; one whose header's write or read version (bytes 18
/// and 19) is 2 is in WAL mode, and a read-only, non-immutable open of it in a writable
/// directory creates `-wal`/`-shm` beside it. Refused by the bytes alone, before any open.
fn refuse_wal(file: &str, bytes: &[u8]) -> Result<()> {
    if bytes
        .get(18..20)
        .is_some_and(|version| version.contains(&2))
    {
        return Err(PackError::WalMode(file.to_owned()));
    }
    Ok(())
}

fn seal(path: &Path, file: &str) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400))
        .map_err(|e| file_io(file, e))
}

/// Read and check the pack at `dir` without writing to it; `admit` opens each declared
/// code-graph snapshot and the declared index directory.
///
/// # Errors
/// [`PackError`]: `Manifest`, `Unsupported`, `Missing`, `Undeclared`, `HashMismatch`,
/// `Snapshot`, `Index`, `Integrity`, `Body`, `Inconsistent`, `BaseMismatch`, a `Store`
/// refusal of the recorded dictionary, driver and i/o failures.
pub fn verify(dir: &Path, admit: Admission) -> Result<Manifest> {
    let manifest = read_manifest(dir)?;
    check_supported(&manifest)?;
    check_files(dir, &manifest)?;
    check_snapshots(dir, &manifest, admit.snapshot)?;
    check_index(dir, &manifest, admit.index)?;
    check_database(dir, &manifest)?;
    Ok(manifest)
}

/// The Tantivy component is mandatory and its payload must match the manifest's lineage,
/// projection and canonical sequence (SS-19): the component is the index (T6 review t6-1), so
/// a manifest selecting anything else is unsupported.
fn check_text_index(m: &Manifest) -> Result<()> {
    match m.text_index.as_str() {
        TEXT_INDEX_TANTIVY => {
            let index = m
                .index
                .as_ref()
                .ok_or_else(|| PackError::Missing(INDEX_DIR.into()))?;
            let meta = format!("{INDEX_DIR}/meta.json");
            if !m.files.contains_key(&meta) {
                return Err(PackError::Missing(meta));
            }
            let payload = &index.payload;
            if payload.get("seq").and_then(Value::as_u64) != Some(m.canonical_seq) {
                return Err(PackError::Inconsistent("index.payload.seq"));
            }
            if payload.get("pack_lineage_id").and_then(Value::as_str) != Some(&m.pack_lineage_id) {
                return Err(PackError::Inconsistent("index.payload.pack_lineage_id"));
            }
            if payload
                .get("versions")
                .and_then(|v| v.get("projection"))
                .and_then(Value::as_str)
                != Some(&m.versions.projection)
            {
                return Err(PackError::Inconsistent("index.payload.versions"));
            }
        }
        other => {
            return Err(PackError::Unsupported {
                field: "text_index",
                found: other.to_owned(),
            });
        }
    }
    Ok(())
}

/// The captured directory admitted by the adapter (`admit`: it opens as an index of this
/// build, every segment its commit references is present, the payload's versions are
/// implemented) and the payload it carries the declared one: a directory substituted for
/// another, re-declared or not, carries another one.
fn check_index(dir: &Path, m: &Manifest, admit: IndexAdmission) -> Result<()> {
    let Some(index) = &m.index else {
        return Ok(());
    };
    let payload = admit(&dir.join(INDEX_DIR)).map_err(|class| PackError::Index { class })?;
    if payload != index.payload {
        return Err(PackError::Inconsistent("index.payload"));
    }
    Ok(())
}

/// Every declared code-graph snapshot admitted, its `meta.commit` the declared one (and so
/// the base: [`check_supported`] tied the two).
fn check_snapshots(dir: &Path, m: &Manifest, admit: SnapshotAdmission) -> Result<()> {
    for snapshot in &m.snapshots {
        match admit(&dir.join(&snapshot.file)) {
            Ok(commit) if commit == snapshot.commit => {}
            Ok(_) => return Err(PackError::BaseMismatch),
            Err(class) => {
                return Err(PackError::Snapshot {
                    file: snapshot.file.clone(),
                    class,
                });
            }
        }
    }
    Ok(())
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST_FILE);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(PackError::Missing(MANIFEST_FILE.into()));
        }
        Err(e) => return Err(file_io(MANIFEST_FILE, e)),
    };
    if !meta.is_file() {
        return Err(PackError::Manifest("not a regular file"));
    }
    if meta.len() > MANIFEST_CAP {
        return Err(PackError::Manifest("over the size cap"));
    }
    let bytes = std::fs::read(&path).map_err(|e| file_io(MANIFEST_FILE, e))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| PackError::Manifest("not a manifest document"))?;
    Manifest::from_value(&value)
}

fn check_supported(m: &Manifest) -> Result<()> {
    let unsupported = |field, found: &str| PackError::Unsupported {
        field,
        found: found.to_owned(),
    };
    if m.format != FORMAT {
        return Err(unsupported("format", &m.format));
    }
    if m.versions.schema != LATEST_SCHEMA_VERSION {
        return Err(unsupported(
            "versions.schema",
            &m.versions.schema.to_string(),
        ));
    }
    // after the schema: the text index a snapshot can hold depends on it
    check_text_index(m)?;
    if m.versions.analyzer != ANALYZER {
        return Err(unsupported("versions.analyzer", &m.versions.analyzer));
    }
    if m.versions.codec != CODEC && m.versions.codec != crate::body_codec::CODEC_ID {
        return Err(unsupported("versions.codec", &m.versions.codec));
    }
    if !is_full_commit(&m.base_commit) {
        return Err(PackError::InvalidCommit);
    }
    if !m.files.contains_key(DB_FILE) {
        return Err(PackError::Missing(DB_FILE.into()));
    }
    for s in &m.snapshots {
        if s.file != snapshot_file(&s.commit) || !m.files.contains_key(&s.file) {
            return Err(PackError::Manifest("snapshot entry does not name its file"));
        }
        // the included snapshots are base snapshots: they must agree with the declared base
        if s.commit != m.base_commit {
            return Err(PackError::BaseMismatch);
        }
    }
    Ok(())
}

/// Every declared file present, regular, of the declared size and hash, not in WAL mode;
/// nothing undeclared.
fn check_files(dir: &Path, m: &Manifest) -> Result<()> {
    for (file, digest) in &m.files {
        let path = dir.join(file);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(PackError::Missing(file.clone()));
            }
            Err(e) => return Err(file_io(file, e)),
        };
        if !meta.is_file() || meta.len() != digest.bytes {
            return Err(PackError::HashMismatch(file.clone()));
        }
        let bytes = std::fs::read(&path).map_err(|e| file_io(file, e))?;
        if sha256_hex(&bytes) != digest.sha256 {
            return Err(PackError::HashMismatch(file.clone()));
        }
        // every component but the index files is a SQLite file
        if !file.starts_with("index/") {
            refuse_wal(file, &bytes)?;
        }
    }
    let mut present = Vec::new();
    list_files(dir, "", &mut present)?;
    for file in present {
        if file != MANIFEST_FILE && !m.files.contains_key(&file) {
            return Err(PackError::Undeclared(file));
        }
    }
    Ok(())
}

/// The pack-relative paths of every entry under `dir`: `snapshots/` and `index/tantivy/` are
/// walked, any other directory is listed as an entry (and so undeclared).
fn list_files(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let here = if prefix.is_empty() { "." } else { prefix };
    let entries = std::fs::read_dir(dir).map_err(|e| file_io(here, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| file_io(here, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let relative = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(|e| file_io(&relative, e))?;
        if kind.is_dir() && [SNAPSHOT_DIR, "index", INDEX_DIR].contains(&relative.as_str()) {
            list_files(&entry.path(), &relative, out)?;
        } else {
            out.push(relative);
        }
    }
    Ok(())
}

/// `file:<path>?mode=ro&immutable=1`: percent-encoded like the code-graph attach URI.
pub(crate) fn immutable_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file:");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out.push_str("?mode=ro&immutable=1");
    out
}

/// Open the pack's snapshot read-only and immutable: no lock, no sidecar, no write, and so no
/// shared-open admission either (SS-23 covers writable opens; `connection::admit_open`).
fn open_immutable(dir: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    Ok(Connection::open_with_flags(immutable_uri(&dir.join(DB_FILE)), flags).sql()?)
}

/// The snapshot against its manifest, and its bodies against the codec (T7 review t7-3):
/// the recorded codec is the declared one, the recorded dictionary the shipped one
/// (`body_codec::verify`, the guard of every other open), and every encoded body of every
/// row, inactive rows included, decodes whole; `PRAGMA integrity_check` sees pages, not
/// frames, and the file hash only certifies the bytes the builder sealed.
fn check_database(dir: &Path, m: &Manifest) -> Result<()> {
    let conn = open_immutable(dir)?;
    let findings: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()
        })
        .sql()?;
    if findings.as_slice() != ["ok".to_owned()] {
        return Err(PackError::Integrity);
    }
    let schema = Migrator::check(&conn).map_err(PackError::Store)?;
    if schema != m.versions.schema {
        return Err(PackError::Inconsistent("versions.schema"));
    }
    if meta(&conn, ONTOLOGY_KEY)?.unwrap_or_default() != m.versions.ontology {
        return Err(PackError::Inconsistent("versions.ontology"));
    }
    if meta(&conn, PROJECTION_KEY)?.unwrap_or_default() != m.versions.projection {
        return Err(PackError::Inconsistent("versions.projection"));
    }
    if meta(&conn, crate::body_codec::CODEC_KEY)?.unwrap_or_else(|| CODEC.to_owned())
        != m.versions.codec
    {
        return Err(PackError::Inconsistent("versions.codec"));
    }
    crate::body_codec::verify(&conn).map_err(PackError::Store)?;
    check_bodies(&conn)?;
    if meta(&conn, "db_id")?.as_deref() != Some(m.instance_id.as_str()) {
        return Err(PackError::Inconsistent("instance_id"));
    }
    if meta(&conn, "pack_lineage_id")?.as_deref() != Some(m.pack_lineage_id.as_str()) {
        return Err(PackError::Inconsistent("pack_lineage_id"));
    }
    if meta(&conn, "parent_pack_id")? != m.parent_pack_id {
        return Err(PackError::Inconsistent("parent_pack_id"));
    }
    if canonical_seq(&conn)? != m.canonical_seq {
        return Err(PackError::Inconsistent("canonical_seq"));
    }
    if audit_head(&conn)? != m.audit {
        return Err(PackError::Inconsistent("audit"));
    }
    let leases: i64 = conn
        .query_row("SELECT count(*) FROM leases", [], |r| r.get(0))
        .sql()?;
    if leases != 0 {
        return Err(PackError::Inconsistent("leases"));
    }
    Ok(())
}

/// Every `BLOB` body decoded through the connection-local `kaban_body()`: the statement fails
/// at the first body the codec refuses (the one place a body is read inside SQL besides the
/// summary projection). A `TEXT` body is plaintext and is not decoded.
fn check_bodies(conn: &Connection) -> Result<()> {
    crate::body_codec::register(conn)?;
    conn.query_row(
        "SELECT count(*) FROM node WHERE typeof(text) = 'blob' AND kaban_body(text) IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )
    .sql()
    .map_err(PackError::Body)?;
    Ok(())
}

impl SqliteStore {
    /// Open the verified pack at `dir` for reads, with its manifest: the snapshot is opened
    /// immutable (no lock, no sidecar), nothing is migrated, seeded or tightened, and the
    /// audit signer is ephemeral because no row can be written. A pack whose schema is not
    /// current ([`Migrator::is_current`]) is refused: a newer reader may migrate a writable
    /// copy only.
    ///
    /// # Errors
    /// Every [`verify`] failure, `SchemaBehind`, driver failures.
    pub fn open_pack(dir: &Path, admit: Admission) -> Result<(Self, Manifest)> {
        let manifest = verify(dir, admit)?;
        if !Migrator::is_current(manifest.versions.schema) {
            return Err(PackError::SchemaBehind {
                found: manifest.versions.schema,
                supported: LATEST_SCHEMA_VERSION,
            });
        }
        let conn = open_immutable(dir)?;
        // every open binds its writer (SS-26); the immutable connection can never use it
        crate::stamp::bind(&conn, &ActorId::default())?;
        let signer = KeySigner::ephemeral().map_err(|e| StoreError::Audit(Box::new(e)))?;
        let mut store = Self::assemble(
            conn,
            manifest.instance_id.clone(),
            Arc::new(signer),
            MigrationReport::unchanged(manifest.versions.schema),
            None,
            Some(&dir.join(DB_FILE)),
        );
        // the captured text index (SS-19; T6 review t6-2): a pack whose manifest selects the
        // Tantivy component is searched from the directory it captured, opened as it stands,
        // read only (no reader mark, no lock file, no database hold, no projection: nothing
        // may be written into an immutable pack), with the pack's own snapshot as its record
        // rows; a pack below the retirement keeps the plain store, whose in-database index
        // serves
        if manifest.text_index == TEXT_INDEX_TANTIVY {
            let index = kaban_index_tantivy::TantivyIndex::open_captured(
                &dir.join(INDEX_DIR),
                store.ontology(),
                store.recall_config(),
                Box::new(crate::index_rows::IndexRows::open_immutable(
                    &dir.join(DB_FILE),
                )?),
            )
            .map_err(StoreError::from)?;
            store.text_index = Some(crate::tantivy::Attached::Captured(Arc::new(index)));
        }
        Ok((store, manifest))
    }
}

// ---- mode-A forks (SS-20) -------------------------------------------------------------------

/// What [`fork`] reports: the child's identities (no local row id, SS-27).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkReport {
    /// The child's new `db_id`.
    pub instance_id: String,
    /// The pack it was forked from.
    pub parent_pack_id: String,
    /// The lineage both share.
    pub pack_lineage_id: String,
    /// The pack's base commit, recorded in the child.
    pub base_commit: String,
    /// The `seq` of the child's audit genesis.
    pub genesis_seq: u64,
}

impl ForkReport {
    /// The JSON document `pack fork` prints.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "instance_id": self.instance_id,
            "parent_pack_id": self.parent_pack_id,
            "pack_lineage_id": self.pack_lineage_id,
            "base_commit": self.base_commit,
            "genesis_seq": self.genesis_seq,
        })
    }
}

/// Fork the verified pack at `pack_dir` into the writable database `dest_db` (must not
/// exist): an ordinary copy of the snapshot (a reflink when the filesystem offers one, never
/// a hard link) into this attempt's own staging file `.<name>.forking.<pid>.<new db_id>`
/// beside the destination, then one transaction that gives the copy a new
/// `instance_id`/`db_id`, keeps `pack_lineage_id` and every record UUID, records
/// `parent_pack_id` and `base_commit`, leaves no lease, and appends the audit genesis under a
/// fresh key `keys_dir/<new db_id>.key`; a pack's index component is copied into this
/// attempt's own staging directory `<staging>.index/tantivy` beside it (T4 review t4-3, t4-7).
/// The copy is published last, under the destination name only if nothing holds it by then
/// ([`publish`]), and the index staging is then renamed to `<stem>.index`, the adapter's own
/// layout. A failure anywhere before publication removes this attempt's staging file,
/// staging directory and key, and nothing else: concurrent attempts share no file, a
/// destination that appeared in the meantime is left as it is, an interrupted attempt leaves
/// only its own staging behind (a retry is not refused by it), and no database under the
/// destination name ever has a half identity (SS-20).
///
/// # Errors
/// Every [`verify`] failure, `Destination` (before the copy, or at publication when the name
/// was taken meanwhile), driver, key and i/o failures. An `Io` on `index/tantivy` after the
/// database was published (its name taken by another process meanwhile) leaves the published
/// database as it is, without its index copy: the adapter rebuilds the index on its next open.
pub fn fork(
    pack_dir: &Path,
    dest_db: &Path,
    keys_dir: &Path,
    admit: Admission,
) -> Result<ForkReport> {
    let manifest = verify(pack_dir, admit)?;
    let (Some(parent), Some(name)) = (dest_db.parent(), dest_db.file_name()) else {
        return Err(PackError::Destination);
    };
    if dest_db.symlink_metadata().is_ok() {
        return Err(PackError::Destination);
    }
    // the index component's destination beside the database (`<stem>.index/`, the adapter's
    // own layout), refused when anything is there already
    let index_root = manifest
        .index
        .as_ref()
        .map(|_| kaban_core::text_index::index_dir(dest_db))
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    if index_root
        .as_deref()
        .is_some_and(|root| root.symlink_metadata().is_ok())
    {
        return Err(PackError::Destination);
    }
    let instance_id = new_id();
    let mut staging_name = std::ffi::OsString::from(".");
    staging_name.push(name);
    staging_name.push(format!(".forking.{}.{instance_id}", std::process::id()));
    let staging = if parent.as_os_str().is_empty() {
        PathBuf::from(staging_name)
    } else {
        parent.join(staging_name)
    };
    let index_staging = index_root.as_ref().map(|_| {
        let mut staged = staging.as_os_str().to_owned();
        staged.push(".index");
        PathBuf::from(staged)
    });
    let manifest_bytes =
        std::fs::read(pack_dir.join(MANIFEST_FILE)).map_err(|e| file_io(MANIFEST_FILE, e))?;
    let published = copy_snapshot(&pack_dir.join(DB_FILE), &staging)
        .and_then(|()| {
            let signer = KeySigner::open_or_create(keys_dir, &instance_id)
                .map_err(|e| StoreError::Audit(Box::new(e)))?;
            initialize(
                &staging,
                &instance_id,
                &signer,
                &manifest,
                &sha256_hex(&manifest_bytes),
            )
        })
        .and_then(|genesis_seq| {
            if let Some(staged) = &index_staging {
                copy_index(&pack_dir.join(INDEX_DIR), &staged.join("tantivy"))?;
            }
            Ok(genesis_seq)
        })
        .and_then(|genesis_seq| publish(&staging, dest_db).map(|()| genesis_seq));
    let genesis_seq = match published {
        Ok(genesis_seq) => genesis_seq,
        Err(e) => {
            // this attempt's own artefacts only: never the destination or another attempt's
            let _ = std::fs::remove_file(&staging);
            if let Some(staged) = &index_staging {
                let _ = std::fs::remove_dir_all(staged);
            }
            let _ = std::fs::remove_file(kaban_policy::key_path(keys_dir, &instance_id));
            return Err(e);
        }
    };
    // the identity is published; its index copy takes the adapter's layout in one rename
    if let (Some(staged), Some(root)) = (&index_staging, &index_root)
        && let Err(e) = std::fs::rename(staged, root)
    {
        let _ = std::fs::remove_dir_all(staged);
        return Err(file_io(INDEX_DIR, e));
    }
    Ok(ForkReport {
        instance_id,
        parent_pack_id: manifest.pack_id,
        pack_lineage_id: manifest.pack_lineage_id,
        base_commit: manifest.base_commit,
        genesis_seq,
    })
}

/// Publish the initialized copy at `staging` under `dest` without replacing anything: a hard
/// link of the staging name (atomic; `EEXIST` when the name is taken) and then the staging
/// name is removed. The link is the publication step only: the file has one name again as
/// soon as it exists under the destination, and the pack's snapshot is never linked. An
/// attempt that finds the name taken is `Destination` and the caller removes its own copy.
fn publish(staging: &Path, dest: &Path) -> Result<()> {
    match std::fs::hard_link(staging, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(PackError::Destination);
        }
        Err(e) => return Err(staging_io(e)),
    }
    std::fs::remove_file(staging).map_err(staging_io)
}

/// The pack's index directory into the attempt's staging (`dst`, created owner-only with its
/// parent): ordinary copies, writable by the owner, since the fork's writer extends them;
/// the payload inside binds to the lineage the fork keeps, so nothing is rebound (SS-20).
fn copy_index(src: &Path, dst: &Path) -> Result<()> {
    let root = dst.parent().ok_or(PackError::Destination)?;
    create_private_dir(root).map_err(|e| file_io(INDEX_DIR, e))?;
    create_private_dir(dst).map_err(|e| file_io(INDEX_DIR, e))?;
    for name in index_files(src)? {
        let file = format!("{INDEX_DIR}/{name}");
        let to = dst.join(&name);
        std::fs::copy(src.join(&name), &to).map_err(|e| file_io(&file, e))?;
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| file_io(&file, e))?;
    }
    Ok(())
}

/// `src` into the new owner-only file `dst`: a reflink (`FICLONE`) when the filesystem
/// offers one, else a byte copy. Never a hard link: the copy is mutable.
fn copy_snapshot(src: &Path, dst: &Path) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut from = std::fs::File::open(src).map_err(|e| file_io(DB_FILE, e))?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)
        .map_err(staging_io)?;
    rustix::fs::fchmod(&to, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR)
        .map_err(|e| staging_io(e.into()))?;
    if !reflink(&to, &from) {
        std::io::copy(&mut from, &mut to).map_err(staging_io)?;
    }
    to.flush().map_err(staging_io)?;
    to.sync_all().map_err(staging_io)?;
    Ok(())
}

/// Share `from`'s blocks into `to` (`FICLONE`, copy-on-write): `true` when the filesystem did
/// it, `false` when it cannot (another filesystem, no support, not Linux) and bytes must be
/// copied.
#[cfg(target_os = "linux")]
fn reflink(to: &std::fs::File, from: &std::fs::File) -> bool {
    rustix::fs::ioctl_ficlone(to, from).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn reflink(_to: &std::fs::File, _from: &std::fs::File) -> bool {
    false
}

/// The atomic initialization of the staged copy: one `BEGIN IMMEDIATE` transaction.
fn initialize(
    staging: &Path,
    instance_id: &str,
    signer: &dyn kaban_policy::AuditSigner,
    manifest: &Manifest,
    pack_sha256: &str,
) -> Result<u64> {
    let mut conn = Connection::open(staging).sql()?;
    conn.busy_timeout(crate::schema::BUSY_TIMEOUT).sql()?;
    // the fork has no actor of its own: the staged copy is written as `local` (SS-26)
    crate::stamp::bind(&conn, &ActorId::default())?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .sql()?;
    // the parent's historical head is what the genesis chains onto; it was verified against
    // the manifest, and the manifest's bytes are the pack hash the genesis binds
    let head = audit_head(&tx)?;
    if head != manifest.audit {
        return Err(PackError::Inconsistent("audit"));
    }
    tx.execute(
        "UPDATE index_meta SET value = ?1 WHERE key = 'db_id'",
        [instance_id],
    )
    .sql()?;
    for (key, value) in [
        ("parent_pack_id", manifest.pack_id.as_str()),
        ("base_commit", manifest.base_commit.as_str()),
    ] {
        tx.execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )
        .sql()?;
    }
    tx.execute_batch("DELETE FROM leases;").sql()?;
    let genesis_seq = crate::audit_table::append_fork_genesis(
        &tx,
        signer,
        &crate::audit_table::ForkBinding {
            parent_pack_id: manifest.pack_id.clone(),
            parent_pack_sha256: pack_sha256.to_owned(),
            parent_instance_id: manifest.instance_id.clone(),
            parent_audit_head: head.head,
            parent_audit_entries: head.entries,
            base_commit: manifest.base_commit.clone(),
            policy_version: manifest.versions.policy.clone(),
        },
    )?;
    tx.commit().sql()?;
    Ok(genesis_seq)
}

/// The fork provenance of a database, as `verify-audit` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkProvenance {
    /// The genesis row's `seq`.
    pub genesis_seq: u64,
    /// The parent pack's id, hash, instance and historical audit head, when the binding
    /// parses (an altered binding renders as `null` and fails verification).
    pub binding: Option<Value>,
}

impl SqliteStore {
    /// This database's fork genesis, when it is a mode-A fork (SS-20).
    ///
    /// # Errors
    /// Driver failures.
    pub fn audit_fork(&self) -> kaban_core::Result<Option<ForkProvenance>> {
        let conn = self.lock();
        Ok(
            crate::audit_table::fork_genesis(&conn)?.map(|g| ForkProvenance {
                genesis_seq: g.seq,
                binding: g.binding.map(|b| {
                    json!({
                        "parent_pack_id": b.parent_pack_id,
                        "parent_pack_sha256": b.parent_pack_sha256,
                        "parent_instance_id": b.parent_instance_id,
                        "parent_audit_head": b.parent_audit_head,
                        "parent_audit_entries": b.parent_audit_entries,
                        "base_commit": b.base_commit,
                    })
                }),
            }),
        )
    }

    /// Recompute this database's whole chain under `signer` rather than the signer it was
    /// opened with: a pack is opened with an ephemeral signer (nothing can be written), and
    /// `--pack verify-audit` verifies its chain under the pack instance's key.
    ///
    /// # Errors
    /// Driver failures.
    pub fn audit_verify_with(
        &self,
        signer: &dyn kaban_policy::AuditSigner,
    ) -> kaban_core::Result<kaban_core::AuditVerification> {
        let conn = self.lock();
        crate::audit_table::verify(&conn, signer)
    }

    /// Recompute the parent's history (the rows before this database's fork genesis) under
    /// the parent's signer; `None` when this database is not a fork. Separate from
    /// [`kaban_core::AuditStore::audit_verify`], which is this database's own chain: there is
    /// no one global chain across a fork.
    ///
    /// # Errors
    /// Driver failures.
    pub fn audit_verify_parent_history(
        &self,
        parent: &dyn kaban_policy::AuditSigner,
    ) -> kaban_core::Result<Option<kaban_core::AuditVerification>> {
        let conn = self.lock();
        crate::audit_table::verify_parent_history(&conn, parent)
    }
}
