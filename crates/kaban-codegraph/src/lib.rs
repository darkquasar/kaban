//! kaban v4 code graph (stage1-spec §3, §4; stage1-plan GM2): the SCIP symbol grammar and the
//! discriminated `ref` parse (G1-S4), the per-commit snapshot schema and its deterministic write
//! helpers (§4.1, G1-N5), the read-only opener and the attach admission the engine joins through
//! (cg-join-v1 §2), the typed queries (G1-F12) and snapshot discovery (G1-F8).
//!
//! This crate spawns nothing (G1-S3) and depends on `kaban-core` and `rusqlite` only, never on
//! `kaban-policy`, `kaban-store-sqlite` or `kaban-api` (G1-E3, cg-join-v1 §2).
//!
//! ```text
//!   symbol.rs (grammar) <== ref.rs (discriminated parse)      error.rs (closed classes)
//!   identity.rs (handle walk, (dev, ino, btime), root commits, repo_id HMAC; CG4.1, G1-S6)
//!   schema.rs (DDL, vocabularies) <== write.rs (deterministic rows, kaban-index side)
//!   sqlite.rs (open_ro, validate_for_attach / AttachUri)  reader.rs (trait)  limits.rs
//!   queries/ (the trait's one implementation)  discover.rs (HeadInput, SnapshotRef, discover,
//!   NoSnapshot, SnapshotCache)  <== provider.rs (SnapshotProvider, Resolved, DiscoverProvider,
//!   HeadCache; CG5.2)  testing/ (FixedSnapshot, feature `test-support`)
//! ```

pub mod discover;
pub mod error;
pub mod identity;
pub mod limits;
pub mod provider;
pub mod queries;
pub mod reader;
pub mod r#ref;
pub mod schema;
pub mod sqlite;
pub mod symbol;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod write;

pub use discover::{HeadInput, IsolationTier, NoSnapshot, SnapshotCache, SnapshotRef, discover};
pub use error::{PathError, ReadError, RefError, SnapshotError, SymbolError, WriteError};
pub use identity::{
    CommonDirHandle, DirHandle, FsIdentity, IdentityError, RepoIdKey, RepoIdentity, fs_identity,
    repo_id, resolve_common_dir, root_commits, walk, worktree_reciprocal,
};
pub use limits::{Depth, Limits};
pub use provider::{DiscoverProvider, HeadCache, Resolved, SnapshotProvider};
pub use queries::{Policy, reader_policy_flag};
pub use reader::{CodeGraphReader, How, Meta, NodeRow};
pub use r#ref::{Ref, check_repo_relative_path, parse_ref};
pub use schema::{EdgeType, FileId, NodeId, NodeKind, Range, SCHEMA_VERSION, SCIP_KIND_NAMES};
pub use sqlite::{AttachUri, SqliteSnapshot, validate_for_attach};
pub use symbol::{Descriptor, Symbol, format, name, normalise_version, parent, parse};
