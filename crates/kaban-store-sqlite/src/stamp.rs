//! Write stamps (spec 05 SS-26): who wrote a record or a change row, on which database copy,
//! in which pack lineage. One mechanism for every write path, part of the schema (migration
//! 0009) so no connection writes around it:
//!
//! ```text
//! open => bind: register kaban_writer() = the actor on the connection (before any migration)
//! any write => node/edge/event/assertion trigger or explicit purge row => INSERT changes
//!   ~> changes_stamp (persistent, AFTER INSERT, stamp still NULL)
//!        => RAISE writer_unbound when kaban_writer() IS NULL
//!        => stamp the change row from kaban_writer() + index_meta (db_id, pack_lineage_id)
//!        => copy that stamp onto the node, edge, event or assertion row the change names
//! ```
//!
//! The writer is fixed for the connection's life: a persistent trigger may not read a
//! temporary table or a connection's attached databases, so the connection-local identity is
//! an application-defined SQL function, registered by every open before the migrator runs (a
//! migration's canonical writes carry the configured actor). A connection kaban did not open
//! (a fixture, the `sqlite3` shell) has no `kaban_writer()`, so it cannot prepare a canonical
//! write at all (`no such function: kaban_writer`): nothing is written unstamped or under
//! another actor's stamp. The record copy is a stamp-only update: the `changes` triggers name
//! the canonical columns only (ident.rs for `node`, 0009 for `edge` and `events`), so it emits
//! no second change row. The audit chain is untouched.

use crate::sql_error::SqlResultExt;
use kaban_core::{ActorId, Result, new_id};
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;

/// The stamp columns of `node`, `edge`, `events`, `assertion` and `changes` (migration 0009);
/// never canonical.
pub(crate) const COLUMNS: [&str; 3] = ["actor_id", "instance_id", "pack_lineage_id"];

/// The SQL function the `changes_stamp` trigger reads the writer from.
const WRITER_FUNCTION: &str = "kaban_writer";

/// Whether the database carries the stamp columns (schema 0009 or later).
fn stamped(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('changes') WHERE name = 'actor_id')",
        [],
        |r| r.get(0),
    )
    .sql()
}

/// Make `actor` the writer every canonical write on `conn` is stamped with: register
/// `kaban_writer()` on the connection. Run by every open before the migrator, so the writer
/// is bound for the connection's whole life.
///
/// # Errors
/// SQLite errors.
pub(crate) fn bind(conn: &Connection, actor: &ActorId) -> Result<()> {
    let actor = actor.as_str().to_owned();
    conn.create_scalar_function(
        WRITER_FUNCTION,
        0,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        move |_| Ok(actor.clone()),
    )
    .sql()
}

/// Set the pack lineage once (fresh and upgraded databases alike) inside the caller's open
/// transaction; an existing value is kept, so copies of this database share it.
///
/// # Errors
/// SQLite errors.
pub(crate) fn ensure_pack_lineage(conn: &Connection) -> Result<()> {
    if stamped(conn)? {
        conn.execute(
            "INSERT OR IGNORE INTO index_meta(key, value) VALUES ('pack_lineage_id', ?1)",
            [new_id()],
        )
        .sql()?;
    }
    Ok(())
}
