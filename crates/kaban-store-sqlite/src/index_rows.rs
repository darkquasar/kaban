//! The record store as an index with its own storage reads it (spec 05 SS-08): [`IndexRows`]
//! implements [`Rows`] over a read-only connection of its own to the database file, or
//! ([`IndexRows::shared`], spec 05 OWN-M) over the store's one connection for an in-memory
//! store, which has no file to open a second connection on; the store then never calls the
//! adapter while it holds that connection. The statements are the FTS5 refill's
//! twins: the `summary` column is the same `summary_projection` SQL, the identifier inputs are
//! the ones `ident::refresh` selects, both read from the committed `index_meta` maps on every
//! call, so an ontology redeployment is seen by the next read, never by a cached map. Every
//! operation reads inside one deferred read transaction, so the maps, the rows, the identity
//! keys and the sequence of one call come from one snapshot: another connection's deployment
//! or write lands wholly before or wholly after it, never in between.

use crate::catalog::Catalog;
use crate::catalog::phase::Reader;
use crate::ident::{FIELDS, FTS_FIELDS, Fields, committed_fields, summary_projection_over};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::text_index::{Canonical, IndexIdentity, IndexRow, Replay, Rows};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// A second connection waits for the writer's checkpoint like the main one does.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// See the module doc. The connection of [`IndexRows::open`] is read-only
/// (`SQLITE_OPEN_READ_ONLY`); either connection is serialized behind a mutex so the adapter,
/// shared across threads, stays `Sync`.
#[derive(Debug)]
pub struct IndexRows {
    conn: Arc<Mutex<Connection>>,
    /// The one read primitive every operation reads through ([`Catalog::read`]).
    catalog: Catalog,
}

impl IndexRows {
    /// Open the read-only connection on the database file at `db`. A file no store of this
    /// process holds is probed first, as every open is (spec 11 CMP-01): an older or foreign
    /// database is refused before SQLite reads it.
    ///
    /// # Errors
    /// `StoreError::SchemaUnsupported` or `StoreError::NotAKabanProject` for a file the probe
    /// refuses, `StoreError::FileMode` for a symlink or non-regular file, the file cannot be
    /// opened read-only (missing, unreadable, not a database), or the database is bound to a
    /// body dictionary this binary does not ship (`StoreError::Migration`, spec 05 SS-17).
    pub fn open(db: &Path) -> Result<Self> {
        crate::connection::probe_unregistered(db)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        // the file the probe read, by its URI (`connection::sqlite_uri`)
        let conn = Connection::open_with_flags(crate::connection::sqlite_uri(db), flags).sql()?;
        conn.busy_timeout(BUSY_TIMEOUT).sql()?;
        // the bodies this connection reads may be encoded (spec 05 SS-17), and are served
        // only under the dictionary the database records (T7 review t7-4)
        crate::body_codec::register(&conn)?;
        crate::body_codec::verify(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            catalog: Catalog::reader(Some(db)),
        })
    }

    /// The rows over the store's own connection (an in-memory store, spec 05 OWN-M): every
    /// read here takes the store's lock, so the store calls the adapter only with that lock
    /// released (constraint 1: the lock order is store, then adapter, never the reverse).
    #[must_use]
    pub(crate) fn shared(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            catalog: Catalog::reader(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `PRAGMA data_version` of this connection (spec 05 SS-21): a value that moves whenever
    /// another connection committed since this connection last read, so a mode-B holder
    /// polls it instead of the log. Only comparisons between two reads on this same handle
    /// mean anything; the handle is the stable observation connection.
    ///
    /// # Errors
    /// A backend failure.
    pub fn data_version(&self) -> Result<u64> {
        let version: i64 = self
            .lock()
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .sql()?;
        Ok(u64::try_from(version).unwrap_or_default())
    }
}

impl Rows for IndexRows {
    fn current(&self, ids: &[String]) -> Result<Vec<IndexRow>> {
        let conn = self.lock();
        let selected = serde_json::to_string(ids)?;
        // one deferred read transaction: the committed maps and the rows from one snapshot
        self.catalog.read(&conn, |tx| {
            let mut out = Vec::new();
            rows(tx, Some(&selected), &mut |row| {
                out.push(row);
                Ok(())
            })?;
            Ok(out)
        })
    }

    fn active(&self, visit: &mut dyn FnMut(IndexRow) -> Result<()>) -> Result<Canonical> {
        let conn = self.lock();
        // one deferred read transaction: the position and every row come from one snapshot
        self.catalog.read(&conn, |tx| {
            let at = canonical(tx)?;
            rows(tx, None, visit)?;
            Ok(at)
        })
    }

    fn canonical(&self) -> Result<Canonical> {
        let conn = self.lock();
        // one deferred read transaction: the identity keys and the sequence from one snapshot
        self.catalog.read(&conn, |tx| canonical(tx))
    }

    fn replay(&self, after: u64, limit: usize) -> Result<Replay> {
        let conn = self.lock();
        // one deferred read transaction: the oldest surviving row, the position and the page
        // from one snapshot (design amendment 6)
        self.catalog.read(&conn, |tx| {
            let oldest: i64 = tx
                .prepare_cached("SELECT coalesce(min(seq), 0) FROM changes")
                .sql()?
                .query_row([], |r| r.get(0))
                .sql()?;
            let at = canonical(tx)?;
            let changes = crate::changelog::read(tx, after, limit)?;
            Ok(Replay {
                oldest: u64::try_from(oldest).unwrap_or_default(),
                at,
                changes,
            })
        })
    }
}

/// The canonical position: the newest `changes.seq` and the identity keys of `index_meta`
/// (`db_id` and `pack_lineage_id`, both written at open on every database, the projection
/// signature and the identifier version).
pub(crate) fn canonical(conn: &Connection) -> Result<Canonical> {
    let meta = |key: &str| -> Result<Option<String>> {
        conn.prepare_cached("SELECT value FROM index_meta WHERE key = ?1")
            .sql()?
            .query_row([key], |r| r.get(0))
            .optional()
            .sql()
    };
    let instance_id = meta("db_id")?.unwrap_or_default();
    let pack_lineage_id = meta("pack_lineage_id")?.unwrap_or_default();
    let projection_signature = meta("fts_projection_signature")?.unwrap_or_default();
    let ident_terms_version = meta("ident_terms_version")?.unwrap_or_default();
    let seq: i64 = conn
        .prepare_cached("SELECT coalesce(max(seq), 0) FROM changes")
        .sql()?
        .query_row([], |r| r.get(0))
        .sql()?;
    Ok(Canonical {
        seq: u64::try_from(seq).unwrap_or_default(),
        identity: IndexIdentity {
            instance_id,
            pack_lineage_id,
            projection_signature,
            ident_terms_version,
        },
    })
}

/// The active rows, every one or those whose id is in the JSON array `ids`, in `rid` order,
/// through `visit`.
fn rows(
    conn: &Connection,
    ids: Option<&str>,
    visit: &mut dyn FnMut(IndexRow) -> Result<()>,
) -> Result<()> {
    let committed = committed_fields(conn)?;
    let selected = if ids.is_some() {
        " AND n.id IN (SELECT value FROM json_each(?1))"
    } else {
        ""
    };
    let sql = select(committed.as_ref(), selected);
    let ident_fields = committed.map(|(ident, _)| ident).unwrap_or_default();
    let mut st = conn.prepare_cached(&sql).sql()?;
    let binds: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let mut it = st.query(binds.as_slice()).sql()?;
    while let Some(r) = it.next().sql()? {
        visit(decode(r, &ident_fields).sql()?)?;
    }
    Ok(())
}

/// The active rows among `rids`, in `rid` order, read through a commit's staging reader (spec
/// 11 T7: the text class stages the rows a commit changed; every statement is prepared fresh
/// in the phase).
///
/// # Errors
/// SQLite's; a stored value that does not decode.
pub(crate) fn staged_rows(reader: &Reader<'_>, rids: &[i64]) -> Result<Vec<IndexRow>> {
    let mut meta = reader.prepare("SELECT value FROM index_meta WHERE key = ?1")?;
    let mut map = |key: &str| -> Result<Option<Fields>> {
        meta.query_row([key], |r| r.get::<_, String>(0))?
            .map(|v| serde_json::from_str(&v))
            .transpose()
            .map_err(Into::into)
    };
    let committed = map(FIELDS)?.zip(map(FTS_FIELDS)?);
    let sql = select(
        committed.as_ref(),
        " AND n.rid IN (SELECT value FROM json_each(?1))",
    );
    let ident_fields = committed.map(|(ident, _)| ident).unwrap_or_default();
    let selected = serde_json::to_string(rids)?;
    reader
        .prepare(&sql)?
        .query_map([selected], |r| decode(r, &ident_fields))
}

/// The statement of the active rows under the committed maps (`None`: a store without the
/// engine deployment, schema < 4, has neither a projection nor identifier fields nor anchors,
/// like its FTS5 index), filtered by `selected`.
fn select(committed: Option<&(Fields, Fields)>, selected: &str) -> String {
    let fts_fields = committed.map(|(_, fts)| fts.clone()).unwrap_or_default();
    let projection = summary_projection_over(&fts_fields, "n", "kaban_body(n.text)");
    let anchors = if committed.is_some() {
        "(SELECT json_group_array(key) FROM (SELECT key FROM node_anchor a WHERE a.rid = n.rid ORDER BY anchor_kind, key))"
    } else {
        "'[]'"
    };
    format!(
        "SELECT n.id, n.rid, n.rev, n.kind, n.workspace_id, n.tags, n.text, {projection}, n.summary, n.props, {anchors} \
         FROM node n WHERE n.lifecycle = 'active'{selected} ORDER BY n.rid"
    )
}

/// One row of [`select`] as the index sees it.
fn decode(r: &rusqlite::Row<'_>, ident_fields: &Fields) -> rusqlite::Result<IndexRow> {
    let json = |idx: usize, e: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    };
    let kind: String = r.get(3)?;
    let tags: Vec<String> =
        serde_json::from_str(&r.get::<_, String>(5)?).map_err(|e| json(5, e))?;
    let text = crate::body_codec::read_column(r, 6)?;
    let summary: Option<String> = r.get(7)?;
    let stored_summary: Option<String> = r.get(8)?;
    let props: Value = serde_json::from_str(&r.get::<_, String>(9)?).map_err(|e| json(9, e))?;
    let anchor_keys: Vec<String> =
        serde_json::from_str(&r.get::<_, String>(10)?).map_err(|e| json(10, e))?;
    let fields = ident_fields.get(&kind).map_or(&[][..], Vec::as_slice);
    Ok(IndexRow {
        id: r.get(0)?,
        rid: r.get(1)?,
        revision: r.get(2)?,
        ident_values: ident_values(fields, &text, stored_summary.as_deref(), &tags, &props),
        kind,
        workspace_id: r.get(4)?,
        tags,
        text,
        summary: summary.unwrap_or_default(),
        stored_summary,
        anchor_keys,
    })
}

/// The kind's `ident_fields` values of one row, exactly as `ident::refresh` selects them for
/// `ident_terms`: `text`, `summary` and `tags` by name, any other field as a `props` path whose
/// string and array-of-string values count.
fn ident_values(
    fields: &[String],
    text: &str,
    summary: Option<&str>,
    tags: &[String],
    props: &Value,
) -> Vec<String> {
    fn strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) => out.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
            _ => {}
        }
    }
    let mut selected = Vec::new();
    for field in fields {
        match field.as_str() {
            "text" => selected.push(text.to_owned()),
            "summary" => selected.extend(summary.map(str::to_owned)),
            "tags" => selected.extend(tags.iter().cloned()),
            path => {
                let mut current = props;
                for part in path.strip_prefix("props.").unwrap_or(path).split('.') {
                    current = current.get(part).unwrap_or(&Value::Null);
                }
                strings(current, &mut selected);
            }
        }
    }
    selected
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::IndexRows;
    use kaban_core::text_index::Rows;
    use kaban_core::{
        Actor, ForgetMode, IngestStore, MemoryStore, NewNode, Node, NodeId, RememberOutcome,
        WorkspaceId, WriteCtx,
    };
    use rusqlite::Connection;
    use std::path::PathBuf;

    /// A private scratch directory removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("ss08-rows-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ctx(ws: &str) -> WriteCtx {
        WriteCtx::new(WorkspaceId::new(ws), Actor::Agent)
    }

    fn created(outcome: RememberOutcome) -> Node {
        match outcome {
            RememberOutcome::Created(n) => n,
            RememberOutcome::Existing { node, .. } => panic!("fresh store: {node:?}"),
        }
    }

    /// The rows are the active rows in `rid` order with the projection's inputs: the projected
    /// summary under the committed field maps (`ident.rs`), a tombstoned row absent, `current`
    /// narrowing by id, and the canonical position the newest change with the committed
    /// identity keys: `db_id` as the instance, the `pack_lineage_id` key (set at open, a
    /// lineage of its own) as the lineage.
    #[test]
    fn rows_reproduce_the_projection_inputs_and_the_canonical_position() {
        let s = Scratch::new("twin");
        let db = s.0.join("kaban.db");
        let store = crate::SqliteStore::open(&db, &s.0.join("keys")).unwrap();
        let fact = created(
            store
                .remember(
                    NewNode {
                        kind: "Fact".into(),
                        text: "TransactionCoordinator retries the commit".into(),
                        summary: Some("retry policy".into()),
                        tags: vec!["db".into(), "retry_path".into()],
                        ..Default::default()
                    },
                    &[],
                    &ctx("default"),
                )
                .unwrap(),
        );
        let decision = created(
            store
                .remember(
                    NewNode {
                        kind: "Decision".into(),
                        text: "use idempotent commits".into(),
                        props: Some(serde_json::json!({
                            "decision": "idempotent commits",
                            "rationale": "the PaymentGateway replays"
                        })),
                        ..Default::default()
                    },
                    &[],
                    &ctx("default"),
                )
                .unwrap(),
        );
        let gone = created(
            store
                .remember(
                    NewNode {
                        kind: "Fact".into(),
                        text: "forgotten quokka".into(),
                        workspace_id: Some("ws2".into()),
                        ..Default::default()
                    },
                    &[],
                    &ctx("ws2"),
                )
                .unwrap(),
        );
        store
            .forget(
                &NodeId::parse(&gone.id).unwrap(),
                ForgetMode::Tombstone,
                &ctx("ws2"),
            )
            .unwrap();

        let rows = IndexRows::open(&db).unwrap();
        let mut seen = Vec::new();
        let at = rows
            .active(&mut |row| {
                seen.push(row);
                Ok(())
            })
            .unwrap();

        // Assert: two active rows in rid order, the tombstoned one absent
        let ids: Vec<&str> = seen.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, [fact.id.as_str(), decision.id.as_str()]);
        assert!(seen.windows(2).all(|w| w[0].rid < w[1].rid));
        let raw = Connection::open(&db).unwrap();
        // the Decision's projected summary carries its `props.rationale` (adr.yaml `fts_fields`);
        // the Fact's is its stored summary
        assert_eq!(seen[1].summary.trim(), "the PaymentGateway replays");
        assert_eq!(seen[1].stored_summary, None);
        assert_eq!(seen[0].summary, "retry policy");
        assert_eq!(seen[0].stored_summary.as_deref(), Some("retry policy"));
        assert_eq!(seen[0].tags, ["db", "retry_path"]);
        assert_eq!(seen[0].workspace_id, "default");
        // `current` narrows by id; an unknown or tombstoned id yields nothing
        let current = rows
            .current(&[decision.id.clone(), gone.id.clone(), "nope".into()])
            .unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0], seen[1]);
        // the canonical position is the newest change and the committed identity
        let max_seq: i64 = raw
            .query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(at.seq, u64::try_from(max_seq).unwrap());
        assert_eq!(at, rows.canonical().unwrap());
        assert_eq!(at.identity.instance_id, store.db_id());
        let committed = store.index_meta().unwrap();
        assert_eq!(
            committed.get("pack_lineage_id"),
            Some(at.identity.pack_lineage_id.as_str()),
            "the lineage is the committed key"
        );
        assert_ne!(
            at.identity.pack_lineage_id,
            store.db_id(),
            "a lineage of its own, never the instance"
        );
        assert_eq!(
            at.identity.ident_terms_version,
            kaban_core::ident::IDENT_TERMS_VERSION
        );
        assert_ne!(at.identity.projection_signature, "");
    }
}
