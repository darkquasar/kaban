//! `references` (stage1-spec §6 `code_references`, §4.3 `REFERENCES`): the occurrence sites of
//! a symbol without the definition role, grouped by enclosing node (the document's file node
//! when the occurrence has none), groups in `(path, ref)` order, sites in range order.
//! Test exclusion checks the site role, file and referring scope, preserving production sites
//! in files that also contain inline tests.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};

use super::nodes;
use crate::error::ReadError;
use crate::reader::{ReferenceGroup, ReferenceSite};
use crate::schema::{NodeId, ROLE_DEFINITION, ROLE_TEST, Range};

pub(super) fn references(
    conn: &Connection,
    id: NodeId,
    include_tests: bool,
) -> Result<Vec<ReferenceGroup>, ReadError> {
    let sql = "SELECT o.sl, o.sc, o.el, o.ec, o.roles, f.path, \
                COALESCE(o.enclosing_id, (SELECT id FROM symbols WHERE kind = 'file' AND file_id = o.file_id)) \
         FROM occurrences o JOIN files f ON f.id = o.file_id \
         LEFT JOIN symbols enclosing ON enclosing.id = o.enclosing_id \
         WHERE o.symbol_id = ?1 AND (o.roles & ?2) = 0 \
         AND (?3 OR (f.is_test = 0 AND COALESCE(enclosing.is_test, 0) = 0 AND (o.roles & ?4) = 0)) \
         ORDER BY f.path, o.sl, o.sc, o.el, o.ec";
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(
        params![id.0, ROLE_DEFINITION, include_tests, ROLE_TEST],
        |r| {
            Ok((
                Range {
                    sl: r.get(0)?,
                    sc: r.get(1)?,
                    el: r.get(2)?,
                    ec: r.get(3)?,
                },
                r.get::<_, u32>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        },
    )?;
    let mut by_enclosing: BTreeMap<i64, Vec<ReferenceSite>> = BTreeMap::new();
    for row in rows {
        let (site, roles, path, enclosing) = row?;
        // a document without a file node cannot happen in a §4.1 snapshot; such a site is dropped
        let Some(enclosing) = enclosing else {
            continue;
        };
        by_enclosing
            .entry(enclosing)
            .or_default()
            .push(ReferenceSite { site, path, roles });
    }
    let mut groups = Vec::with_capacity(by_enclosing.len());
    for (enclosing, sites) in by_enclosing {
        let Some(enclosing) = nodes::node_by_id(conn, NodeId(enclosing))? else {
            return Err(ReadError::Malformed {
                column: "enclosing_id",
            });
        };
        groups.push(ReferenceGroup { enclosing, sites });
    }
    groups.sort_by(|a, b| {
        (a.enclosing.path.as_deref(), &a.enclosing.r#ref)
            .cmp(&(b.enclosing.path.as_deref(), &b.enclosing.r#ref))
    });
    Ok(groups)
}
