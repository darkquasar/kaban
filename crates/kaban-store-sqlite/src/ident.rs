//! The committed identifier and projection field maps and the change-log trigger over the
//! canonical columns (OWN-92/95; spec 05 OWN-M).
//!
//! The maps (`ident_fields_resolved`, `fts_fields_resolved`) and the projection signature are
//! the identity the Tantivy projection is bound to: `index_rows.rs` derives the identifier
//! inputs and the ontology-bound summary from them at projection time, so a changed ontology
//! replaces both maps atomically and every reader of the committed maps projects the same
//! text. Nothing is maintained in the database itself any more: the derived `ident_terms`
//! column and the FTS5 mirrors went with migration 0013.

use crate::sql_error::SqlResultExt;
use kaban_core::{Ontology, Result, ident::TEXT_ANALYZER_VERSION, stored_name};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;

pub(crate) const FIELDS: &str = "ident_fields_resolved";
pub(crate) const FTS_FIELDS: &str = "fts_fields_resolved";
/// The old metadata key is also part of the public doctor report.
const VERSION: &str = "ident_terms_version";
const PENDING: &str = "pending";

pub(crate) fn needs_rebuild(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM index_meta WHERE key=?1 AND value=?2)",
        params![VERSION, PENDING],
        |r| r.get(0),
    )
    .sql()
}

/// Runs in the same transaction as publishing a tier built from the committed projections.
pub(crate) fn mark_indexed(conn: &Connection) -> Result<()> {
    conn.execute("INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![VERSION, TEXT_ANALYZER_VERSION]).sql()?;
    Ok(())
}

const SIGNATURE: &str = "fts_projection_signature";
pub(crate) type Fields = BTreeMap<String, Vec<String>>;

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The ontology-bound summary projection of one row, with `body` as the SQL expression of the
/// row's text (the index rows pass `kaban_body({alias}.text)`, so the projection concatenates
/// the decoded body). SQL depends only on row values and the embedded resolved field paths:
/// this text is an input of `fts_projection_signature`, the identity an installed index is
/// bound to, so it is fixed.
pub(crate) fn summary_projection_over(fields: &Fields, alias: &str, body: &str) -> String {
    let mut cases = String::new();
    for (kind, paths) in fields {
        if paths.is_empty() {
            continue;
        }
        let mut projection = format!("coalesce({alias}.summary, '')");
        for field in paths {
            let expr = match field.as_str() {
                "text" => body.to_owned(),
                "summary" | "tags" => format!("{alias}.{field}"),
                _ => {
                    let path = field.strip_prefix("props.").unwrap_or(field);
                    // JSON-quote each member, then SQL-quote the resulting JSON path.
                    let mut quoted_path = String::new();
                    for part in path.split('.') {
                        let _ = write!(quoted_path, ".{}", Value::String(part.into()));
                    }
                    format!(
                        "json_extract({alias}.props, {})",
                        literal(&format!("${quoted_path}"))
                    )
                }
            };
            let _ = write!(projection, " || ' ' || coalesce(cast({expr} as text), '')");
        }
        let _ = write!(cases, " WHEN {} THEN ({projection})", literal(kind));
    }
    if cases.is_empty() {
        format!("{alias}.summary")
    } else {
        format!("CASE {alias}.kind{cases} ELSE {alias}.summary END")
    }
}

/// Commit the ontology's field maps in the caller's transaction. A changed ontology replaces
/// both maps atomically; other connections read the committed map on each projection, rather
/// than holding a stale connection-local field cache. `true` when the maps were replaced (the
/// text indexed under the old ones is stale).
pub(crate) fn deploy(conn: &Connection, ontology: &Ontology, force: bool) -> Result<bool> {
    let ident_fields = ontology
        .kinds
        .iter()
        .map(|(kind, resolved)| (stored_name(kind), resolved.ident_fields.clone()))
        .collect();
    let fts_fields = ontology
        .kinds
        .iter()
        .map(|(kind, resolved)| (stored_name(kind), resolved.fts_fields.clone()))
        .collect();
    deploy_fields(conn, &ident_fields, &fts_fields, force)
}

/// Change just this label's projection. The same commit restages its active rows.
pub(crate) fn deploy_label_text(conn: &Connection, label: &str, paths: &[String]) -> Result<bool> {
    let Some((ident, mut fts)) = committed_fields(conn)? else {
        return Ok(false);
    };
    if fts.get(label).map_or(&[][..], Vec::as_slice) == paths {
        return Ok(false);
    }
    fts.insert(label.to_owned(), paths.to_vec());
    let already_current: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM index_meta WHERE key=?1 AND value=?2)",
            params![VERSION, TEXT_ANALYZER_VERSION],
            |row| row.get(0),
        )
        .sql()?;
    deploy_fields(conn, &ident, &fts, false)?;
    // Only this label is restaged. An unrelated pending rebuild must survive.
    if already_current {
        mark_indexed(conn)?;
    }
    Ok(true)
}

pub(crate) fn committed_fields(conn: &Connection) -> Result<Option<(Fields, Fields)>> {
    let read = |key| -> Result<Option<Fields>> {
        let value: Option<String> = conn
            .query_row("SELECT value FROM index_meta WHERE key=?", [key], |row| {
                row.get(0)
            })
            .optional()
            .sql()?;
        Ok(value
            .map(|value| serde_json::from_str(&value))
            .transpose()?)
    };
    Ok(read(FIELDS)?.zip(read(FTS_FIELDS)?))
}

/// Bare open preserves the last explicit ontology deployment, including schema upgrades.
pub(crate) fn ensure(conn: &Connection, fallback: &Ontology) -> Result<bool> {
    if let Some((ident, fts)) = committed_fields(conn)? {
        deploy_fields(conn, &ident, &fts, false)
    } else {
        deploy(conn, fallback, false)
    }
}

fn deploy_fields(
    conn: &Connection,
    ident_fields: &Fields,
    fts_fields: &Fields,
    force: bool,
) -> Result<bool> {
    let version: u32 = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .sql()?
        .parse()
        .unwrap_or(1);
    if version < 4 {
        return Ok(false);
    }
    let fields = serde_json::to_string(ident_fields)?;
    let projection_fields = serde_json::to_string(fts_fields)?;
    // the projection text, an input of the signature (module doc)
    let projection = summary_projection_over(fts_fields, "n", "kaban_body(n.text)");
    // Preserve logging for every existing canonical column, including no-op canonical
    // updates. The writer reference is excluded because the stamp trigger copies it
    // onto the node after each canonical write.
    let canonical_columns: Vec<String> = conn
        .prepare(
            "SELECT name FROM pragma_table_info('node') WHERE name <> 'ident_terms' ORDER BY cid",
        )
        .sql()?
        .query_map([], |row| row.get::<_, String>(0))
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?
        .into_iter()
        .filter(|name| name != crate::stamp::COLUMN)
        .collect();
    let columns = canonical_columns
        .iter()
        .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(",");
    let changes_sql = format!(
        "CREATE TRIGGER node_changes_au AFTER UPDATE OF {columns} ON node WHEN new.lifecycle <> 'purged' BEGIN
          INSERT INTO changes(entity,op,id,workspace_id) VALUES('node','update',new.id,new.workspace_id);
         END;"
    );
    let signature = kaban_policy::sha256_hex(
        format!("{TEXT_ANALYZER_VERSION}\n{fields}\n{projection}\n{changes_sql}").as_bytes(),
    );
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key=?",
            [SIGNATURE],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    let terms_version: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key='ident_terms_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .sql()?;
    let current_projection: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key=?",
            [FTS_FIELDS],
            |row| row.get(0),
        )
        .optional()
        .sql()?;
    if !force
        && current.as_deref() == Some(signature.as_str())
        && terms_version.as_deref() == Some(TEXT_ANALYZER_VERSION)
        && current_projection.as_deref() == Some(projection_fields.as_str())
    {
        return Ok(false);
    }
    conn.execute_batch("DROP TRIGGER IF EXISTS node_changes_au;")
        .sql()?;
    conn.execute_batch(&changes_sql).sql()?;
    // A first deployment has no old packs. For an existing project retain a durable retry
    // signal even if the process exits after deploying the fields but before publishing the tier.
    let existing: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM index_meta WHERE key='db_id') AND EXISTS(SELECT 1 FROM pragma_database_list WHERE name='main' AND file <> '')",
            [],
            |r| r.get(0),
        )
        .sql()?;
    for (key, value) in [
        (FIELDS, fields.as_str()),
        (FTS_FIELDS, projection_fields.as_str()),
        (SIGNATURE, signature.as_str()),
        (
            VERSION,
            if existing {
                PENDING
            } else {
                TEXT_ANALYZER_VERSION
            },
        ),
    ] {
        conn.execute("INSERT INTO index_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value]).sql()?;
    }
    Ok(true)
}
