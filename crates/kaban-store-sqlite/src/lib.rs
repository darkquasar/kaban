//! SQLite implementation of [`kaban_core::GraphStore`] (rusqlite bundled).
//!
//! Nodes and edges live in plain tables; labels and relationship types are data in
//! `schema_type`. Recall's text seeds come from the store's own text index
//! (the Tantivy adapter it opens, admits or rebuilds at open and projects every commit into,
//! spec 05 SS-07 and OWN-M; `tantivy`); an open that attaches none has no text index and
//! refuses what needs one. Every file open first probes the main file's schema version
//! without writing (spec 11 CMP-01): a fresh project is created at the one supported version
//! ([`LATEST_SCHEMA_VERSION`]), a project at that version opens, and a file at any other
//! version is refused untouched.
//! There is no migration and no backup (OD-1).

mod anchors;
mod body_codec;
mod changelog;
mod connection;
mod doctor;
mod edge_write;
mod ident;
pub mod index_rows;
mod jobs;
mod leases;
mod ontology;
mod producers;
mod projector_thread;
mod recall;
mod rows;
mod schema;
mod session;
mod sql_error;
mod sqlite;
mod stamp;
mod tantivy;
mod traverse;

pub use connection::{ExclusiveHold, exclusive_hold};

/// Register `kaban_body(x)` on a connection of the caller's own: the decoded text of a body
/// column (spec 05 OWN-M: every body is a tagged `BLOB`; a `SELECT text FROM node` reads bytes,
/// `SELECT kaban_body(text)` the text). For inspection and tests; the store's own connections
/// have it.
///
/// # Errors
/// SQLite errors.
pub fn register_body_reader(conn: &rusqlite::Connection) -> kaban_core::Result<()> {
    body_codec::register(conn)
}
pub use producers::{ProducerRunOutcome, ProducerRunRecord};
pub use schema::{LATEST_SCHEMA_VERSION, MigrationReport, Migrator};
pub use sqlite::SqliteStore;
