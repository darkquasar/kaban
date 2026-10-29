//! SQLite implementation of [`kaban_core::GraphStore`] (rusqlite bundled).
//!
//! Nodes and edges live in plain tables; labels and relationship types are data in
//! `schema_type`; `audit` is the append-only HMAC chain written in the same transactions as
//! the gated writes (core-spec S3). Recall's text seeds come from the store's own text index
//! (the Tantivy adapter it opens, admits or rebuilds at open and projects every commit into,
//! spec 05 SS-07 and OWN-M; `tantivy`); an open that attaches none has no text index and
//! refuses what needs one. Every open runs the forward-only [`Migrator`] (core-spec §7):
//! pre-migration backup, one write lock for the whole run with a savepoint per migration,
//! refuse-newer; migration 0013 needs the database to itself.

mod anchors;
mod audit_table;
mod body_codec;
mod candidates;
mod changelog;
mod confirmations;
mod connection;
mod doctor;
mod events;
mod explain;
mod ident;
pub mod index_rows;
mod jobs;
mod layers;
mod leases;
mod ontology;
pub mod pack;
mod projector_thread;
mod pruning_protection;
mod recall;
mod rows;
mod schema;
mod session;
mod sql_error;
mod sqlite;
mod stamp;
mod sync;
mod tantivy;
mod traverse;

mod reconcile;

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
pub use layers::{LayerRunOutcome, LayerRunRecord};
pub use schema::{LATEST_SCHEMA_VERSION, MigrationReport, Migrator};
pub use sqlite::SqliteStore;
