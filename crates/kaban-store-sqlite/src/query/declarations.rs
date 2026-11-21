//! Read only the names needed for SQL projections from the held format-14 catalog.
use crate::ontology::{format_stored, has_declared_columns, has_per_commit};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::query_budget::{BudgetLease, QueryBudget};
use rusqlite::{Row, Transaction};
use std::collections::BTreeMap;

pub(super) struct Declaration {
    pub(super) kind: String,
    pub(super) name: String,
    pub(super) ns: String,
    pub(super) per_commit: bool,
    pub(super) columns: Vec<String>,
    pub(super) props: Vec<String>,
    pub(super) rollups: Vec<Grouping>,
}

pub(super) struct Grouping {
    pub(super) name: String,
    pub(super) group_by: Vec<String>,
}

fn name(row: &Row<'_>, index: usize, storage: &mut BudgetLease) -> Result<String> {
    let value = row
        .get_ref(index)
        .sql()?
        .as_str()
        .map_err(|_| super::refused("model", "invalid declaration name"))?;
    // Includes the successive owned name, folded lookup key, escaped SQL expression,
    // generated alias and projection buffers, plus Vec/BTree entries for this name.
    // For arbitrary property names JSON escaping can expand a byte up to sixfold.
    super::charge(storage, 1024 + (value.len() as u64).saturating_mul(48))?;
    Ok(value.into())
}

pub(super) fn read(
    tx: &Transaction<'_>,
    budget: &QueryBudget,
    storage: &mut BudgetLease,
) -> Result<BTreeMap<i64, Declaration>> {
    let flag = if has_per_commit(tx)? {
        "per_commit"
    } else {
        "0"
    };
    let mut defs = BTreeMap::new();
    let mut stmt = tx
        .prepare(&format!(
            "SELECT rowid,kind,ns,name,{flag} FROM main.schema_type ORDER BY rowid"
        ))
        .sql()?;
    let mut rows = stmt.query([]).sql()?;
    while let Some(row) = rows.next().sql()? {
        budget.check()?;
        let kind = name(row, 1, storage)?;
        let ns = name(row, 2, storage)?;
        let bare = name(row, 3, storage)?;
        let stored = format_stored(&ns, &bare);
        // Covers the fixed-size declaration, projection and allowlist entries,
        // including initial tree nodes and array growth.
        super::charge(storage, 4096)?;
        defs.insert(
            row.get(0).sql()?,
            Declaration {
                kind,
                ns,
                name: stored,
                per_commit: row.get(4).sql()?,
                columns: vec![],
                props: vec![],
                rollups: vec![],
            },
        );
    }
    if has_declared_columns(tx)? {
        let mut stmt = tx
            .prepare(
                "SELECT s.rowid,json_extract(c.value,'$.name') FROM main.schema_type s,
            main.json_each(s.columns) c WHERE s.kind='label' ORDER BY s.rowid,c.key",
            )
            .sql()?;
        let mut rows = stmt.query([]).sql()?;
        while let Some(row) = rows.next().sql()? {
            budget.check()?;
            declaration(&mut defs, row)?
                .columns
                .push(name(row, 1, storage)?);
        }
        let sql = "SELECT s.rowid,r.key,json_extract(r.value,'$.name'),g.value
                   FROM main.schema_type s, main.json_each(s.rollups) r
                   LEFT JOIN main.json_each(r.value,'$.group_by') g ON true
                   WHERE s.kind='label' ORDER BY s.rowid,r.key,g.key";
        let mut stmt = tx.prepare(sql).sql()?;
        let mut rows = stmt.query([]).sql()?;
        let mut last = None;
        while let Some(row) = rows.next().sql()? {
            budget.check()?;
            let group = (row.get::<_, i64>(0).sql()?, row.get::<_, i64>(1).sql()?);
            let def = declaration(&mut defs, row)?;
            if last != Some(group) {
                def.rollups.push(Grouping {
                    name: name(row, 2, storage)?,
                    group_by: vec![],
                });
                last = Some(group);
            }
            if row.get_ref(3).sql()? != rusqlite::types::ValueRef::Null {
                let rollup = def
                    .rollups
                    .last_mut()
                    .ok_or_else(|| super::refused("model", "missing rollup"))?;
                rollup.group_by.push(name(row, 3, storage)?);
            }
        }
    }
    let sql = "SELECT s.rowid,p.key FROM main.schema_type s,
               main.json_each(s.props_schema,'$.properties') p
               WHERE s.kind='rel' AND json_type(s.props_schema,'$.properties')='object'
               ORDER BY s.rowid,p.key";
    let mut stmt = tx.prepare(sql).sql()?;
    let mut rows = stmt.query([]).sql()?;
    while let Some(row) = rows.next().sql()? {
        budget.check()?;
        declaration(&mut defs, row)?
            .props
            .push(name(row, 1, storage)?);
    }
    Ok(defs)
}

fn declaration<'a>(
    defs: &'a mut BTreeMap<i64, Declaration>,
    row: &Row<'_>,
) -> Result<&'a mut Declaration> {
    defs.get_mut(&row.get(0).sql()?)
        .ok_or_else(|| super::refused("model", "declaration missing in snapshot"))
}
